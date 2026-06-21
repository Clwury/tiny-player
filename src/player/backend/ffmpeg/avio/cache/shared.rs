#[cfg(test)]
use std::sync::mpsc;
use std::{
    sync::{Arc, Condvar, Mutex, mpsc::Sender},
    thread,
    time::Instant,
};

use crate::player::backend::{BackendEvent, BackendEventKind, ByteCacheState, PlaybackCacheConfig};
#[cfg(test)]
use crate::player::render_host::PlaybackSessionId;

#[cfg(test)]
use super::HTTP_CACHE_PROBE_READ_WAIT;
use super::{
    CacheAppendPermit, CacheAppendResult, CacheReadResult, CacheRestartRequest, FfmpegControl,
    HTTP_CACHE_CONTENT_LEN_WAIT, HTTP_CACHE_PARTIAL_READ_MIN_BYTES,
    HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER, HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL,
    HTTP_CACHE_SIDE_DOWNLOAD_WORKERS, HTTP_CACHE_STALL_LOG_AFTER, HTTP_CACHE_STALL_LOG_INTERVAL,
    HTTP_CACHE_WAIT_INTERVAL, HttpCacheConfig, HttpCacheRangeKind, HttpRingCache,
    HttpRingCacheShared, HttpRingCacheState, http_ring_cache_download_loop,
    http_ring_cache_side_download_loop, playback_cache_state_from_http_status,
    reqwest_header_pairs,
};

impl HttpRingCache {
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
        let status = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("HTTP stream cache poisoned");
            guard.apply_cache_config(cache_config);
            guard.take_stream_cache_status_report()
        };
        self.shared.send_stream_cache_status(status);
        self.shared.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn read_at(
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
        let mut total = 0usize;
        let mut logged_wait = false;
        let mut wait_started_at = None;
        let mut next_stall_log_at = None;
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
            if let Some(error) = guard.error.clone() {
                tracing::debug!(
                    offset,
                    current_offset,
                    total,
                    requested = output.len(),
                    %error,
                    "HTTP stream cache read failed"
                );
                return CacheReadResult::Error(error);
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
                self.shared.ready.notify_all();
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
                self.shared.ready.notify_all();
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

            if !logged_wait {
                let now = Instant::now();
                wait_started_at = Some(now);
                next_stall_log_at = now.checked_add(HTTP_CACHE_STALL_LOG_AFTER);
                tracing::debug!(
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
                    "waiting for HTTP stream cache data"
                );
                logged_wait = true;
            } else {
                let now = Instant::now();
                if next_stall_log_at.is_some_and(|deadline| now >= deadline) {
                    tracing::debug!(
                        offset,
                        current_offset,
                        total,
                        requested = output.len(),
                        waited_ms = wait_started_at
                            .map(|started| now.saturating_duration_since(started).as_millis())
                            .unwrap_or(0),
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
                        "still waiting for HTTP stream cache data"
                    );
                    next_stall_log_at = now.checked_add(HTTP_CACHE_STALL_LOG_INTERVAL);
                }
            }
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
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
            if guard.shutdown || self.shared.control.should_stop() {
                return CacheReadResult::Interrupted;
            }
            if let Some(error) = guard.error.clone() {
                return CacheReadResult::Error(error);
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
            if offset < guard.base_offset || offset > guard.next_offset {
                let status = guard
                    .queue_read_miss_at(offset)
                    .then(|| guard.take_stream_cache_status_report())
                    .flatten();
                self.shared.ready.notify_all();
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
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, wait_for)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
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
        self.shared.ready.notify_all();
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
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, wait_for)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
        }
    }

    pub(in crate::player::backend::ffmpeg::avio) fn shutdown(&self) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.shutdown = true;
        self.shared.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg) fn set_duration_seconds(
        &self,
        duration_seconds: Option<f64>,
    ) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.duration_seconds =
            duration_seconds.filter(|duration| duration.is_finite() && *duration > 0.0);
        self.shared.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg) fn playback_byte_cache_status(&self) -> ByteCacheState {
        let guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.stream_cache_status()
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
                control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
                event_tx,
            }),
        }
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

impl HttpRingCacheShared {
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

    pub(in crate::player::backend::ffmpeg::avio) fn set_error(&self, error: String) {
        tracing::warn!(%error, "HTTP video stream cache failed");
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.error = Some(error);
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn mark_eof(&self) {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.eof = true;
            guard.take_stream_cache_status_report()
        };
        self.send_stream_cache_status(status);
        self.ready.notify_all();
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
                self.ready.notify_all();
                return Some(request.offset);
            }
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
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
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
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
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::avio) fn set_content_len(
        &self,
        content_len: Option<u64>,
    ) {
        if let Some(content_len) = content_len {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.content_len = Some(content_len);
            self.ready.notify_all();
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

    pub(in crate::player::backend::ffmpeg::avio) fn range_request_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .range_request_bytes
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

    pub(in crate::player::backend::ffmpeg::avio) fn wait_for_append_capacity(
        &self,
        offset: u64,
    ) -> CacheAppendPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        let mut logged_prefetch_pause = false;
        let mut prefetch_pause_started_at = None;
        let mut next_prefetch_pause_log_at = None;
        loop {
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendPermit::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.ready.notify_all();
                return CacheAppendPermit::Restart(request.offset);
            }
            if let Some(next_offset) = guard.splice_retained_playback_at_active_end(offset) {
                self.ready.notify_all();
                return CacheAppendPermit::Restart(next_offset);
            }
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
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
        }
    }

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
                self.ready.notify_all();
                return CacheAppendPermit::Restart(request.offset);
            }
            if let Some(next_offset) = guard.splice_retained_playback_at_active_end(offset) {
                self.ready.notify_all();
                return CacheAppendPermit::Restart(next_offset);
            }
            let capacity = guard.append_capacity_from(offset);
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
        let (result, status) = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendResult::Stopped;
            }
            if let Some(request) = guard.restart_request.take() {
                guard.restart_at_with_kind(request.offset, request.range_kind);
                self.ready.notify_all();
                return CacheAppendResult::Restart(request.offset);
            }
            if !guard.append_at(offset, data) {
                return CacheAppendResult::Restart(offset);
            }
            (
                CacheAppendResult::Appended,
                guard.take_stream_cache_status_report(),
            )
        };
        self.ready.notify_all();
        self.send_cache_events(status);
        result
    }

    pub(in crate::player::backend::ffmpeg::avio) fn append_side_download_or_stop(
        &self,
        request: CacheRestartRequest,
        offset: u64,
        data: &[u8],
    ) -> CacheAppendResult {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendResult::Stopped;
            }
            if !guard.side_download_active.contains(&request) {
                return CacheAppendResult::Stopped;
            }
            if !guard.append_retained_at_protected(offset, data, request) {
                return CacheAppendResult::Restart(offset);
            }
            guard.take_stream_cache_status_report()
        };
        self.ready.notify_all();
        self.send_cache_events(status);
        CacheAppendResult::Appended
    }
}
