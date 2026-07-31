#[cfg(test)]
use std::sync::mpsc;
use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use crate::player::backend::{BackendEvent, BackendEventKind, ByteCacheState, PlaybackCacheConfig};
#[cfg(test)]
use crate::player::render_host::PlaybackSessionId;

#[cfg(test)]
use super::HTTP_CACHE_PROBE_READ_WAIT;
use super::{
    CacheAppendPermit, CacheAppendResult, CacheReadResult, CacheRestartRequest, CacheRetryPermit,
    FfmpegControl, HTTP_CACHE_CONTENT_LEN_WAIT, HTTP_CACHE_PARTIAL_READ_MIN_BYTES,
    HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER, HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL,
    HTTP_CACHE_SIDE_DOWNLOAD_WORKERS, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES,
    HTTP_CACHE_STARTUP_FIRST_BYTE_TIMEOUT, HTTP_CACHE_WAIT_INTERVAL, HttpCacheConfig,
    HttpCacheRangeKind, HttpDiskCache, HttpReadWaitLogDecision, HttpRingCache, HttpRingCacheShared,
    HttpRingCacheState, PendingHttpDiskCacheWrite, PreparedByteAppend,
    RetainedPlaybackSpliceSource, http_ring_cache_download_loop,
    http_ring_cache_side_download_loop, playback_cache_state_from_http_status,
    reqwest_header_pairs,
};

fn startup_first_byte_wait_timed_out(
    offset: u64,
    total: usize,
    read_started_at: Instant,
    now: Instant,
) -> bool {
    offset == 0
        && total == 0
        && now.saturating_duration_since(read_started_at) >= HTTP_CACHE_STARTUP_FIRST_BYTE_TIMEOUT
}

impl HttpRingCache {
    pub(in crate::player::backend::ffmpeg::avio) fn has_cached_byte_at(&self, offset: u64) -> bool {
        self.shared.has_cached_byte_at(offset)
    }

    pub(in crate::player::backend::ffmpeg) fn input_progress_generation(&self) -> u64 {
        self.shared
            .input_progress_generation
            .load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg) fn wait_for_input_progress_change(
        &self,
        observed_generation: u64,
        timeout: Duration,
    ) -> bool {
        if self.input_progress_generation() != observed_generation {
            return true;
        }
        let guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        if self.input_progress_generation() != observed_generation {
            return true;
        }
        if guard.shutdown || self.shared.control.should_stop() {
            return false;
        }
        let (_guard, _) = self
            .shared
            .ready
            .wait_timeout(guard, timeout)
            .expect("HTTP stream cache poisoned");
        self.input_progress_generation() != observed_generation
    }

    pub(in crate::player::backend::ffmpeg) fn set_output_backpressure_prefetch_paused(
        &self,
        paused: bool,
    ) -> bool {
        let changed = self
            .shared
            .output_backpressure_paused
            .swap(paused, Ordering::AcqRel)
            != paused;
        if changed {
            self.shared.notify_ready();
            tracing::debug!(
                paused,
                reason = "output_gate_and_decoder_queue_full",
                "updated HTTP stream cache prefetch pause for output backpressure"
            );
        }
        changed
    }

    pub(in crate::player::backend::ffmpeg) fn update_demux_high_water_prefetch_paused(
        &self,
        total_bytes: usize,
        memory_limit_bytes: usize,
        prefetch_queue_full: bool,
        underrun: bool,
    ) -> bool {
        let mut current = self.shared.demux_high_water_paused.load(Ordering::Acquire);
        loop {
            let paused = demux_high_water_prefetch_should_pause(
                current,
                total_bytes,
                memory_limit_bytes,
                prefetch_queue_full,
                underrun,
            );
            if paused == current {
                return false;
            }
            match self.shared.demux_high_water_paused.compare_exchange_weak(
                current,
                paused,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.shared.notify_ready();
                    tracing::debug!(
                        paused,
                        total_bytes,
                        memory_limit_bytes,
                        prefetch_queue_full,
                        underrun,
                        high_water_percent = 90,
                        low_water_percent = 75,
                        "updated HTTP stream cache prefetch pause for demux cache waterline"
                    );
                    return true;
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn spawn(
        url: String,
        http_headers: &[(String, String)],
        content_len_hint: Option<u64>,
        cache_config: &PlaybackCacheConfig,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let headers = reqwest_header_pairs(http_headers)?;
        let config = HttpCacheConfig::from_playback_config(cache_config);
        let shared = Arc::new(HttpRingCacheShared {
            state: Mutex::new(
                HttpRingCacheState::new_with_config(0, config)
                    .with_content_len_hint(content_len_hint),
            ),
            ready: Condvar::new(),
            output_backpressure_paused: AtomicBool::new(false),
            demux_high_water_paused: AtomicBool::new(false),
            cache_config_generation: AtomicU64::new(0),
            input_progress_generation: AtomicU64::new(0),
            control,
            event_tx,
        });
        let worker_shared = Arc::clone(&shared);
        let side_url = url.clone();
        let side_headers = headers.clone();
        thread::Builder::new()
            .name("tiny-http-stream-cache".to_string())
            .spawn(move || http_ring_cache_download_loop(worker_shared, url, headers))
            .map_err(|error| format!("启动 HTTP 视频缓存线程失败：{error}"))?;
        for worker_index in 0..HTTP_CACHE_SIDE_DOWNLOAD_WORKERS {
            let side_worker_shared = Arc::clone(&shared);
            let side_url = side_url.clone();
            let side_headers = side_headers.clone();
            thread::Builder::new()
                .name(format!("tiny-http-stream-cache-side-{worker_index}"))
                .spawn(move || {
                    http_ring_cache_side_download_loop(side_worker_shared, side_url, side_headers)
                })
                .map_err(|error| format!("启动 HTTP 视频缓存辅助线程失败：{error}"))?;
        }

        Ok(Self { shared })
    }

    pub(in crate::player::backend::ffmpeg) fn apply_cache_config(
        &self,
        cache_config: &PlaybackCacheConfig,
    ) {
        let generation = self
            .shared
            .cache_config_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        match self.shared.state.try_lock() {
            Ok(mut guard) => {
                guard.apply_cache_config(cache_config);
                let status = guard.take_stream_cache_status_report();
                drop(guard);
                self.shared.send_stream_cache_status(status);
                self.shared.notify_ready();
            }
            Err(TryLockError::WouldBlock) => {
                let shared = Arc::clone(&self.shared);
                let cache_config = cache_config.clone();
                if let Err(error) = thread::Builder::new()
                    .name("tiny-http-cache-config".to_string())
                    .spawn(move || {
                        let status = {
                            let mut guard =
                                shared.state.lock().expect("HTTP stream cache poisoned");
                            if shared.cache_config_generation.load(Ordering::Acquire) != generation
                            {
                                return;
                            }
                            guard.apply_cache_config(&cache_config);
                            guard.take_stream_cache_status_report()
                        };
                        shared.send_stream_cache_status(status);
                        shared.notify_ready();
                    })
                {
                    tracing::warn!(%error, "failed to defer contended HTTP cache config update");
                }
            }
            Err(TryLockError::Poisoned(_)) => panic!("HTTP stream cache poisoned"),
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn read_at(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> CacheReadResult {
        if output.is_empty() {
            return CacheReadResult::Data(0);
        }

        let read_started_at = Instant::now();
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        let mut total = 0usize;
        loop {
            let current_offset = offset.saturating_add(total as u64);
            if guard.shutdown || self.shared.control.should_stop() {
                tracing::trace!(
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    seek_generation = self.shared.control.seek_generation(),
                    "HTTP stream cache read interrupted"
                );
                return CacheReadResult::Interrupted;
            }
            if guard
                .content_len
                .is_some_and(|content_len| current_offset >= content_len)
            {
                if total > 0 {
                    tracing::debug!(
                        offset,
                        current_offset,
                        total,
                        requested = output.len(),
                        content_len = ?guard.content_len,
                        "HTTP stream cache read returning partial data at content end"
                    );
                    let status = guard.take_stream_cache_status_report();
                    drop(guard);
                    self.shared.send_stream_cache_status(status);
                    return CacheReadResult::Data(total);
                }
                tracing::debug!(
                    offset,
                    current_offset,
                    requested = output.len(),
                    content_len = ?guard.content_len,
                    "HTTP stream cache read reached content end"
                );
                return CacheReadResult::Eof;
            }
            if let Some(read) = guard.copy_available(current_offset, &mut output[total..]) {
                total = total.saturating_add(read);
                guard.set_reader_offset(offset.saturating_add(total as u64));
                self.shared.notify_ready();
                if total == output.len() || total >= HTTP_CACHE_PARTIAL_READ_MIN_BYTES {
                    let status = guard.take_stream_cache_status_report();
                    drop(guard);
                    self.shared.send_stream_cache_status(status);
                    return CacheReadResult::Data(total);
                }
                continue;
            }
            if total > 0 {
                tracing::trace!(
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    "HTTP stream cache read returning currently available partial data"
                );
                let status = guard.take_stream_cache_status_report();
                drop(guard);
                self.shared.send_stream_cache_status(status);
                return CacheReadResult::Data(total);
            }
            if let Some(error) = guard.read_error_at(current_offset).cloned()
                && !guard.side_download_may_produce(current_offset)
            {
                tracing::debug!(
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    error_offset = error.offset,
                    error = %error.message,
                    "HTTP stream cache read reached failed byte range"
                );
                return CacheReadResult::Error(error.message);
            }
            if current_offset < guard.base_offset || current_offset > guard.next_offset {
                tracing::debug!(
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    active_range_kind = ?guard.active_range_kind,
                    "HTTP stream cache read requesting side range"
                );
                let status = guard
                    .queue_read_miss_at(current_offset)
                    .then(|| guard.take_stream_cache_status_report())
                    .flatten();
                self.shared.notify_ready();
                self.shared.send_stream_cache_status(status);
            }
            if guard.eof
                && current_offset >= guard.next_offset
                && !guard.side_download_may_produce(current_offset)
            {
                if total > 0 {
                    tracing::debug!(
                        offset,
                        current_offset,
                        total,
                        requested = output.len(),
                        base_offset = guard.base_offset,
                        next_offset = guard.next_offset,
                        "HTTP stream cache read returning partial data at range EOF"
                    );
                    let status = guard.take_stream_cache_status_report();
                    drop(guard);
                    self.shared.send_stream_cache_status(status);
                    return CacheReadResult::Data(total);
                }
                tracing::debug!(
                    offset,
                    current_offset,
                    requested = output.len(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    "HTTP stream cache read reached range EOF"
                );
                return CacheReadResult::Eof;
            }

            let now = Instant::now();
            if startup_first_byte_wait_timed_out(offset, total, read_started_at, now) {
                tracing::warn!(
                    offset,
                    current_offset,
                    requested = output.len(),
                    waited_ms = now
                        .saturating_duration_since(read_started_at)
                        .as_millis(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    content_len = ?guard.content_len,
                    "HTTP stream cache startup produced no first byte"
                );
                return CacheReadResult::Error("HTTP 视频缓存启播等待首字节超时".to_string());
            }
            let wait_observation = guard
                .read_wait_observation(current_offset, self.shared.prefetch_paused_by_downstream());
            let wait_log_decision = guard.observe_read_wait_log(wait_observation, now);
            let wait_log = match wait_log_decision {
                HttpReadWaitLogDecision::Changed { suppressed_repeats } => {
                    Some(("state_changed", suppressed_repeats, Duration::ZERO))
                }
                HttpReadWaitLogDecision::Summary {
                    repeated_observations,
                    blocked_for,
                } => Some(("summary", repeated_observations, blocked_for)),
                HttpReadWaitLogDecision::Suppressed => None,
            };
            if let Some((log_kind, suppressed_repeats, blocked_for)) = wait_log {
                tracing::debug!(
                    log_kind,
                    suppressed_repeats,
                    blocked_for_ms = blocked_for.as_secs_f64() * 1_000.0,
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    reader_offset = guard.reader_offset,
                    cached_bytes = guard.cached_bytes(),
                    content_len = ?guard.content_len,
                    active_range_kind = ?guard.active_range_kind,
                    active_forward_bytes = guard.active_forward_bytes(),
                    active_forward_est_seconds = ?guard.active_forward_est_seconds(guard.raw_input_rate()),
                    range_request_bytes_effective = guard.range_request_bytes_effective(),
                    prefetch_paused = guard.prefetch_paused,
                    restart_pending = guard.restart_request.is_some(),
                    eof = guard.eof,
                    wait_position = wait_observation.position.as_str(),
                    side_download_pending = wait_observation.side_download_pending,
                    output_backpressure_paused = wait_observation.output_backpressure_paused,
                    "HTTP stream cache read wait state"
                );
            }
            guard = self
                .shared
                .wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn read_at_for_test(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> CacheReadResult {
        self.read_at(offset, output)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn read_cached_at(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> CacheReadResult {
        if output.is_empty() {
            return CacheReadResult::Data(0);
        }

        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        let deadline = Instant::now()
            .checked_add(HTTP_CACHE_PROBE_READ_WAIT)
            .unwrap_or_else(Instant::now);
        loop {
            if guard.shutdown || self.shared.control.should_interrupt() {
                return CacheReadResult::Interrupted;
            }
            if guard
                .content_len
                .is_some_and(|content_len| offset >= content_len)
            {
                return CacheReadResult::Eof;
            }
            if let Some(read) = guard.copy_available(offset, output) {
                return CacheReadResult::Data(read);
            }
            if let Some(error) = guard.read_error_at(offset).cloned()
                && !guard.side_download_may_produce(offset)
            {
                return CacheReadResult::Error(error.message);
            }
            if offset < guard.base_offset || offset > guard.next_offset {
                let status = guard
                    .queue_read_miss_at(offset)
                    .then(|| guard.take_stream_cache_status_report())
                    .flatten();
                self.shared.notify_ready();
                self.shared.send_stream_cache_status(status);
            }
            if guard.eof && offset >= guard.next_offset && !guard.side_download_may_produce(offset)
            {
                return CacheReadResult::Eof;
            }
            if offset < guard.base_offset && !guard.side_download_may_produce(offset) {
                return CacheReadResult::Interrupted;
            }
            let now = Instant::now();
            if now >= deadline {
                return CacheReadResult::WouldBlock;
            }

            let wait_for = (deadline - now).min(HTTP_CACHE_WAIT_INTERVAL);
            guard = self.shared.wait_for_ready_change(guard, wait_for);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn note_reader_offset(
        &self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.note_seek_offset(offset, range_kind);
        self.shared.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn is_tail_metadata_probe_seek(
        &self,
        offset: u64,
    ) -> bool {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .is_tail_metadata_probe_seek(offset)
    }

    pub(in crate::player::backend::ffmpeg::avio) fn content_len(&self) -> Option<u64> {
        let deadline = Instant::now().checked_add(HTTP_CACHE_CONTENT_LEN_WAIT)?;
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        loop {
            if guard.content_len.is_some()
                || guard.shutdown
                || guard.error.is_some()
                || self.shared.control.should_stop()
            {
                return guard.content_len;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let wait_for = (deadline - now).min(HTTP_CACHE_WAIT_INTERVAL);
            guard = self.shared.wait_for_ready_change(guard, wait_for);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn shutdown(&self) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.shutdown = true;
        self.shared.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg) fn set_duration_seconds(
        &self,
        duration_seconds: Option<f64>,
    ) {
        let duration_seconds =
            duration_seconds.filter(|duration| duration.is_finite() && *duration > 0.0);
        match self.shared.state.try_lock() {
            Ok(mut guard) => guard.duration_seconds = duration_seconds,
            Err(TryLockError::WouldBlock) => {
                let shared = Arc::clone(&self.shared);
                if let Err(error) = thread::Builder::new()
                    .name("tiny-http-cache-duration".to_string())
                    .spawn(move || {
                        shared
                            .state
                            .lock()
                            .expect("HTTP stream cache poisoned")
                            .duration_seconds = duration_seconds;
                        shared.notify_ready();
                    })
                {
                    tracing::warn!(%error, "failed to defer contended HTTP cache duration update");
                }
                return;
            }
            Err(TryLockError::Poisoned(_)) => panic!("HTTP stream cache poisoned"),
        }
        self.shared.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg) fn try_playback_byte_cache_status(
        &self,
    ) -> Option<ByteCacheState> {
        let guard = match self.shared.state.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(_)) => panic!("HTTP stream cache poisoned"),
        };
        Some(guard.stream_cache_status())
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn from_state_for_test(
        state: HttpRingCacheState,
    ) -> Self {
        let (event_tx, _) = mpsc::channel();
        Self {
            shared: Arc::new(HttpRingCacheShared {
                state: Mutex::new(state),
                ready: Condvar::new(),
                output_backpressure_paused: AtomicBool::new(false),
                demux_high_water_paused: AtomicBool::new(false),
                cache_config_generation: AtomicU64::new(0),
                input_progress_generation: AtomicU64::new(0),
                control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
                event_tx,
            }),
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::avio) fn shared_for_download_test(
        &self,
    ) -> Arc<HttpRingCacheShared> {
        Arc::clone(&self.shared)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn control_for_test(&self) -> Arc<FfmpegControl> {
        Arc::clone(&self.shared.control)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn reader_offset_for_test(&self) -> u64 {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .reader_offset
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn next_offset_for_test(&self) -> u64 {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .next_offset
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn has_restart_request_for_test(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .restart_request
            .is_some()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn side_download_requests_for_test(
        &self,
    ) -> Vec<CacheRestartRequest> {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .side_download_requests
            .iter()
            .copied()
            .collect()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn memory_capacity_for_test(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .memory_capacity
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn range_request_bytes_for_test(&self) -> u64 {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .range_request_bytes
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn is_shutdown_for_test(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .shutdown
    }
}

fn demux_high_water_prefetch_should_pause(
    currently_paused: bool,
    total_bytes: usize,
    memory_limit_bytes: usize,
    prefetch_queue_full: bool,
    underrun: bool,
) -> bool {
    if underrun || memory_limit_bytes == 0 {
        return false;
    }
    if prefetch_queue_full {
        return true;
    }

    let (numerator, denominator) = if currently_paused { (3, 4) } else { (9, 10) };
    let threshold = memory_limit_bytes
        .saturating_mul(numerator)
        .div_ceil(denominator);
    total_bytes >= threshold
}

impl HttpRingCacheShared {
    fn write_disk_cache_outside_state_lock(
        &self,
        offset: u64,
        data: &[u8],
    ) -> Option<PendingHttpDiskCacheWrite> {
        let file = {
            let guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard
                .disk_cache_writable
                .then(|| {
                    guard
                        .disk_cache
                        .as_ref()
                        .map(|cache| Arc::clone(&cache.file))
                })
                .flatten()
        }?;
        let result = HttpDiskCache::write_file_at(&file, offset, data);
        Some(PendingHttpDiskCacheWrite { file, result })
    }

    fn prefetch_paused_by_downstream(&self) -> bool {
        self.output_backpressure_paused.load(Ordering::Acquire)
            || self.demux_high_water_paused.load(Ordering::Acquire)
    }

    fn notify_ready(&self) {
        self.control.wake();
        self.ready.notify_all();
    }

    fn wait_for_ready_change<'a>(
        &'a self,
        guard: MutexGuard<'a, HttpRingCacheState>,
        timeout: Duration,
    ) -> MutexGuard<'a, HttpRingCacheState> {
        let observed_generation = self.control.wake_generation();
        drop(guard);
        self.control
            .wait_for_wake_change(observed_generation, timeout);
        self.state.lock().expect("HTTP stream cache poisoned")
    }

    pub(in crate::player::backend::ffmpeg::avio) fn should_stop(&self) -> bool {
        self.control.should_stop()
            || self
                .state
                .lock()
                .expect("HTTP stream cache poisoned")
                .shutdown
    }

    pub(in crate::player::backend::ffmpeg::avio) fn take_restart_offset(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .restart_request
            .take()
            .map(|request| request.offset)
    }

    pub(in crate::player::backend::ffmpeg::avio) fn set_error_at(
        &self,
        offset: u64,
        error: String,
    ) {
        tracing::warn!(offset, %error, "HTTP video stream cache range failed");
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.set_read_error(offset, error);
        self.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn reader_offset_now(&self) -> u64 {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .reader_offset
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_retry_delay(
        &self,
        delay: Duration,
    ) -> bool {
        let deadline = Instant::now()
            .checked_add(delay)
            .unwrap_or_else(Instant::now);
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            guard = self.wait_for_ready_change(guard, deadline - now);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_reader_at_or_restart(
        &self,
        offset: u64,
    ) -> CacheRetryPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return CacheRetryPermit::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return CacheRetryPermit::Restart(request.offset);
            }
            if guard.reader_offset >= offset {
                return CacheRetryPermit::Ready;
            }
            guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_restart_after_error(
        &self,
        offset: u64,
    ) -> CacheRetryPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return CacheRetryPermit::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return CacheRetryPermit::Restart(request.offset);
            }
            if guard.read_error_at(offset).is_none() {
                return CacheRetryPermit::Ready;
            }
            guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn mark_eof(&self) {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.eof = true;
            guard.take_stream_cache_status_report()
        };
        self.send_stream_cache_status(status);
        self.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_restart_after_eof(
        &self,
    ) -> Option<u64> {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return None;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return Some(request.offset);
            }
            guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_side_download_request(
        &self,
    ) -> Option<CacheRestartRequest> {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return None;
            }
            if let Some(request) = guard.side_download_requests.pop_front() {
                guard.side_download_active.push(request);
                return Some(request);
            }
            guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn finish_side_download(
        &self,
        request: CacheRestartRequest,
        completed: bool,
    ) {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.finish_side_download_request(request, completed && !self.control.should_stop());
            guard.take_stream_cache_status_report()
        };
        self.send_stream_cache_status(status);
        self.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn finish_side_download_with_error(
        &self,
        request: CacheRestartRequest,
        error_offset: u64,
        error: String,
    ) {
        let (status, affects_reader) = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            let request_end = request
                .offset
                .saturating_add(guard.side_range_request_bytes(request.range_kind).max(1));
            let affects_reader = request.range_kind == HttpCacheRangeKind::Playback
                && guard.reader_offset >= request.offset
                && guard.reader_offset < request_end;
            guard.finish_side_download_request(request, false);
            if affects_reader {
                guard.set_read_error(error_offset, error.clone());
            }
            (guard.take_stream_cache_status_report(), affects_reader)
        };
        if affects_reader {
            tracing::warn!(
                request_offset = request.offset,
                error_offset,
                %error,
                "HTTP playback side range failed at the active reader"
            );
        } else {
            tracing::warn!(
                request_offset = request.offset,
                error_offset,
                range_kind = ?request.range_kind,
                %error,
                "HTTP background side range failed without poisoning playback"
            );
        }
        self.send_stream_cache_status(status);
        self.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn set_content_len(
        &self,
        content_len: Option<u64>,
    ) {
        if let Some(content_len) = content_len {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.content_len = Some(content_len);
            self.notify_ready();
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn content_len_now(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .content_len
    }

    pub(in crate::player::backend::ffmpeg::avio) fn chunk_size(&self) -> usize {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .chunk_size
    }

    pub(in crate::player::backend::ffmpeg::avio) fn continuous_playback_requests(&self) -> bool {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .continuous_playback_requests
    }

    pub(in crate::player::backend::ffmpeg::avio) fn has_cached_byte_at(&self, offset: u64) -> bool {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .cached_range_contains(offset)
    }

    pub(in crate::player::backend::ffmpeg::avio) fn playback_range_request_bytes(
        &self,
        offset: u64,
    ) -> u64 {
        let guard = self.state.lock().expect("HTTP stream cache poisoned");
        let configured = guard.config.range_request_bytes.max(1);
        if guard.active_range_kind == HttpCacheRangeKind::Playback
            && offset == guard.base_offset
            && guard.base_offset == guard.next_offset
            && guard.active_request_start_offset == guard.base_offset
        {
            return configured.min(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES.max(1));
        }
        configured
    }

    pub(in crate::player::backend::ffmpeg::avio) fn side_range_request_bytes(
        &self,
        request: CacheRestartRequest,
    ) -> u64 {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .side_range_request_bytes(request.range_kind)
    }

    fn finish_retained_playback_splice_without_state_lock<'a>(
        &'a self,
        guard: MutexGuard<'a, HttpRingCacheState>,
        offset: u64,
        mut source: RetainedPlaybackSpliceSource,
    ) -> (MutexGuard<'a, HttpRingCacheState>, Option<u64>) {
        drop(guard);
        let prepared = source.split_prepared_pages();
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        let next_offset = guard.finish_retained_playback_splice(offset, source, prepared);
        (guard, next_offset)
    }

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_append_capacity(
        &self,
        offset: u64,
    ) -> CacheAppendPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        let mut logged_prefetch_pause = false;
        let mut prefetch_pause_started_at = None;
        let mut next_prefetch_pause_log_at = None;
        let mut output_backpressure_pause_started_at = None;
        let mut next_output_backpressure_log_at = None;
        let mut output_backpressure_waits = 0u64;
        loop {
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendPermit::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return CacheAppendPermit::Restart(request.offset);
            }
            if let Some(source) = guard.take_retained_playback_splice_source(offset) {
                let (next_guard, next_offset) =
                    self.finish_retained_playback_splice_without_state_lock(guard, offset, source);
                guard = next_guard;
                if let Some(next_offset) = next_offset {
                    self.notify_ready();
                    return CacheAppendPermit::Restart(next_offset);
                }
            }
            if self.prefetch_paused_by_downstream() {
                let now = Instant::now();
                let started_at = *output_backpressure_pause_started_at.get_or_insert(now);
                output_backpressure_waits = output_backpressure_waits.saturating_add(1);
                if next_output_backpressure_log_at.is_none_or(|deadline| now >= deadline) {
                    tracing::debug!(
                        offset,
                        paused_ms =
                            now.saturating_duration_since(started_at).as_secs_f64() * 1000.0,
                        wait_count = output_backpressure_waits,
                        cached_bytes = guard.cached_bytes(),
                        reader_offset = guard.reader_offset,
                        reason = "output_gate_and_decoder_queue_full",
                        "HTTP stream cache prefetch paused by output backpressure"
                    );
                    next_output_backpressure_log_at = now.checked_add(Duration::from_secs(1));
                }
                guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
                continue;
            }
            output_backpressure_pause_started_at = None;
            next_output_backpressure_log_at = None;
            output_backpressure_waits = 0;
            let capacity = guard.append_capacity_from(offset);
            if capacity > 0 {
                return CacheAppendPermit::Ready(capacity);
            }
            let now = Instant::now();
            let pause_started = *prefetch_pause_started_at.get_or_insert(now);
            let active_reader_offset = guard.reader_offset.max(guard.base_offset);
            let buffered_ahead = offset.saturating_sub(active_reader_offset);
            let target_readahead_bytes = guard.target_readahead_bytes();
            let resume_readahead_bytes = guard.resume_readahead_bytes(target_readahead_bytes);
            if !logged_prefetch_pause {
                tracing::debug!(
                    offset,
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    reader_offset = guard.reader_offset,
                    active_reader_offset,
                    buffered_ahead,
                    target_readahead_bytes,
                    resume_readahead_bytes,
                    cached_bytes = guard.cached_bytes(),
                    content_len = ?guard.content_len,
                    active_range_kind = ?guard.active_range_kind,
                    prefetch_paused = guard.prefetch_paused,
                    eof = guard.eof,
                    "HTTP stream cache prefetch paused waiting for reader"
                );
                logged_prefetch_pause = true;
                next_prefetch_pause_log_at = now.checked_add(HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER);
            } else if next_prefetch_pause_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    offset,
                    paused_ms = now.saturating_duration_since(pause_started).as_millis(),
                    base_offset = guard.base_offset,
                    next_offset = guard.next_offset,
                    reader_offset = guard.reader_offset,
                    active_reader_offset,
                    buffered_ahead,
                    target_readahead_bytes,
                    resume_readahead_bytes,
                    cached_bytes = guard.cached_bytes(),
                    content_len = ?guard.content_len,
                    active_range_kind = ?guard.active_range_kind,
                    prefetch_paused = guard.prefetch_paused,
                    eof = guard.eof,
                    "HTTP stream cache prefetch still paused waiting for reader"
                );
                next_prefetch_pause_log_at =
                    now.checked_add(HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL);
            }
            guard = self.wait_for_ready_change(guard, HTTP_CACHE_WAIT_INTERVAL);
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::avio) fn append_capacity_now(
        &self,
        offset: u64,
    ) -> CacheAppendPermit {
        let (permit, status) = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendPermit::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return CacheAppendPermit::Restart(request.offset);
            }
            if let Some(source) = guard.take_retained_playback_splice_source(offset) {
                let (next_guard, next_offset) =
                    self.finish_retained_playback_splice_without_state_lock(guard, offset, source);
                guard = next_guard;
                if let Some(next_offset) = next_offset {
                    self.notify_ready();
                    return CacheAppendPermit::Restart(next_offset);
                }
            }
            let capacity = if self.prefetch_paused_by_downstream() {
                0
            } else {
                guard.append_capacity_from(offset)
            };
            let status = (capacity == 0)
                .then(|| guard.take_stream_cache_status_report())
                .flatten();
            let permit = if capacity > 0 {
                CacheAppendPermit::Ready(capacity)
            } else {
                CacheAppendPermit::Full
            };
            (permit, status)
        };
        if let Some(status) = status {
            let _ = self.event_tx.send(BackendEvent::new(
                self.control.session_id(),
                BackendEventKind::CacheStateChanged(playback_cache_state_from_http_status(status)),
            ));
        }
        permit
    }

    fn send_stream_cache_status(&self, status: Option<ByteCacheState>) {
        if let Some(status) = status {
            let _ = self.event_tx.send(BackendEvent::new(
                self.control.session_id(),
                BackendEventKind::CacheStateChanged(playback_cache_state_from_http_status(status)),
            ));
        }
    }

    fn send_cache_events(&self, status: Option<ByteCacheState>) {
        self.send_stream_cache_status(status);
    }

    pub(in crate::player::backend::ffmpeg::avio) fn append_or_restart(
        &self,
        offset: u64,
        data: &[u8],
    ) -> CacheAppendResult {
        let prepared = PreparedByteAppend::from_bytes(data);
        let disk_write = self.write_disk_cache_outside_state_lock(offset, data);
        let (result, status) = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendResult::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.notify_ready();
                return CacheAppendResult::Restart(request.offset);
            }
            if !guard.append_prepared_at_after_disk_write(offset, data, prepared, disk_write) {
                return CacheAppendResult::Restart(offset);
            }
            guard.clear_read_error_covered_by(offset, data.len());
            (
                CacheAppendResult::Appended,
                guard.take_stream_cache_status_report(),
            )
        };
        self.input_progress_generation
            .fetch_add(1, Ordering::AcqRel);
        self.notify_ready();
        self.send_cache_events(status);
        result
    }

    pub(in crate::player::backend::ffmpeg::avio) fn append_side_download_or_stop(
        &self,
        request: CacheRestartRequest,
        offset: u64,
        data: &[u8],
    ) -> CacheAppendResult {
        let prepared = PreparedByteAppend::from_bytes(data);
        let disk_write = self.write_disk_cache_outside_state_lock(offset, data);
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendResult::Stopped;
            }
            if !guard.side_download_active.contains(&request) {
                return CacheAppendResult::Stopped;
            }
            if !guard.append_retained_prepared_at_protected_after_disk_write(
                offset, data, prepared, request, disk_write,
            ) {
                return CacheAppendResult::Restart(offset);
            }
            guard.clear_read_error_covered_by(offset, data.len());
            guard.take_stream_cache_status_report()
        };
        self.input_progress_generation
            .fetch_add(1, Ordering::AcqRel);
        self.notify_ready();
        self.send_cache_events(status);
        CacheAppendResult::Appended
    }
}

#[cfg(test)]
mod startup_wait_tests {
    use std::time::Instant;

    use super::{HTTP_CACHE_STARTUP_FIRST_BYTE_TIMEOUT, startup_first_byte_wait_timed_out};

    #[test]
    fn startup_first_byte_wait_only_times_out_an_empty_read_at_stream_start() {
        let started_at = Instant::now();
        let deadline = started_at + HTTP_CACHE_STARTUP_FIRST_BYTE_TIMEOUT;

        assert!(startup_first_byte_wait_timed_out(
            0, 0, started_at, deadline
        ));
        assert!(!startup_first_byte_wait_timed_out(
            1, 0, started_at, deadline
        ));
        assert!(!startup_first_byte_wait_timed_out(
            0, 1, started_at, deadline
        ));
    }
}
