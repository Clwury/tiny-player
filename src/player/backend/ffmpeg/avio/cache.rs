use std::collections::VecDeque;

use super::{
    download::{http_ring_cache_download_loop, http_ring_cache_side_download_loop},
    http::reqwest_header_pairs,
    *,
};

#[derive(Clone)]
pub(in crate::player::backend::ffmpeg) struct HttpRingCache {
    shared: Arc<HttpRingCacheShared>,
}

pub(super) struct HttpRingCacheShared {
    state: Mutex<HttpRingCacheState>,
    ready: Condvar,
    control: Arc<FfmpegControl>,
    event_tx: Sender<BackendEvent>,
}

pub(in crate::player::backend::ffmpeg) struct HttpRingCacheState {
    buffer: ByteRingBuffer,
    pub(in crate::player::backend::ffmpeg) base_offset: u64,
    pub(in crate::player::backend::ffmpeg) next_offset: u64,
    active_request_start_offset: u64,
    retained_ranges: VecDeque<RetainedCacheRange>,
    disk_cache: Option<HttpDiskCache>,
    disk_cache_writable: bool,
    config: HttpCacheConfig,
    active_range_kind: HttpCacheRangeKind,
    pending_seek_range_kind: Option<(u64, HttpCacheRangeKind)>,
    reader_offset: u64,
    byte_level_seeks: u64,
    input_rate_samples: VecDeque<InputRateSample>,
    retained_access_generation: u64,
    duration_seconds: Option<f64>,
    prefetch_paused: bool,
    content_len: Option<u64>,
    pub(in crate::player::backend::ffmpeg) eof: bool,
    shutdown: bool,
    restart_request: Option<CacheRestartRequest>,
    side_download_requests: VecDeque<CacheRestartRequest>,
    side_download_active: Vec<CacheRestartRequest>,
    error: Option<String>,
    last_reported_status: Option<ByteCacheState>,
}

pub(in crate::player::backend::ffmpeg) enum CacheReadResult {
    Data(usize),
    Eof,
    #[cfg(test)]
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::player::backend::ffmpeg) struct HttpPlaybackBufferRange {
    pub(in crate::player::backend::ffmpeg) start_offset: u64,
    pub(in crate::player::backend::ffmpeg) end_offset: u64,
    pub(in crate::player::backend::ffmpeg) content_len: u64,
}

pub(super) enum CacheAppendPermit {
    Ready(usize),
    Full,
    Restart(u64),
    Stopped,
}

pub(super) enum CacheAppendResult {
    Appended,
    Restart(u64),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::player::backend::ffmpeg) enum HttpCacheRangeKind {
    Playback,
    TailMetadataProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct CacheRestartRequest {
    pub(in crate::player::backend::ffmpeg) offset: u64,
    pub(in crate::player::backend::ffmpeg) range_kind: HttpCacheRangeKind,
}

struct RetainedCacheRange {
    buffer: ByteRingBuffer,
    base_offset: u64,
    next_offset: u64,
    #[allow(dead_code)]
    range_kind: HttpCacheRangeKind,
    last_used_generation: u64,
}

#[derive(Clone, Copy)]
struct InputRateSample {
    at: Instant,
    bytes: usize,
}

#[derive(Clone)]
struct HttpCacheConfig {
    memory_capacity: usize,
    chunk_size: usize,
    range_request_bytes: u64,
    readahead_seconds: f64,
    hysteresis_seconds: f64,
    max_readahead_bytes: Option<u64>,
    disk_cache_bytes: Option<u64>,
    cache_dir: Option<PathBuf>,
    unlink_files: CacheUnlinkPolicy,
}

struct HttpDiskCache {
    file: File,
    path: PathBuf,
    ranges: Vec<HttpCachedByteRange>,
    max_bytes: u64,
    access_generation: u64,
    unlink_on_drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpCachedByteRange {
    start: u64,
    end: u64,
    last_used_generation: u64,
}

struct ByteRingBuffer {
    storage: Vec<u8>,
    head: usize,
    len: usize,
    max_capacity: usize,
}

impl ByteRingBuffer {
    fn new(max_capacity: usize) -> Self {
        Self {
            storage: Vec::new(),
            head: 0,
            len: 0,
            max_capacity,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let required_len = self
            .len
            .checked_add(data.len())
            .expect("HTTP stream cache buffer length overflowed");
        debug_assert!(required_len <= self.max_capacity);
        self.ensure_storage_len(required_len);

        let write_offset = (self.head + self.len) % self.storage.len();
        copy_into_wrapped(&mut self.storage, write_offset, data);
        self.len = required_len;
    }

    fn discard_front(&mut self, len: usize) {
        let len = len.min(self.len);
        if len == 0 {
            return;
        }
        if len == self.len {
            self.clear();
            return;
        }

        self.head = (self.head + len) % self.storage.len();
        self.len -= len;
    }

    fn copy_at(&self, offset: usize, output: &mut [u8]) -> usize {
        if output.is_empty() || offset >= self.len || self.storage.is_empty() {
            return 0;
        }

        let read = (self.len - offset).min(output.len());
        let read_offset = (self.head + offset) % self.storage.len();
        copy_from_wrapped(&self.storage, read_offset, &mut output[..read]);
        read
    }

    fn ensure_storage_len(&mut self, required_len: usize) {
        if required_len <= self.storage.len() {
            return;
        }

        let new_len = self.grown_storage_len(required_len);
        if self.head == 0 {
            self.storage.resize(new_len, 0);
            return;
        }

        let mut storage = vec![0; new_len];
        self.copy_at(0, &mut storage[..self.len]);
        self.storage = storage;
        self.head = 0;
    }

    fn grown_storage_len(&self, required_len: usize) -> usize {
        let mut len = if self.storage.is_empty() {
            HTTP_CACHE_CHUNK_SIZE
                .min(self.max_capacity)
                .max(required_len)
        } else {
            self.storage.len()
        };
        while len < required_len {
            let next = len.saturating_mul(2).min(self.max_capacity);
            if next == len {
                break;
            }
            len = next;
        }
        len.max(required_len).min(self.max_capacity)
    }

    fn resize_capacity(&mut self, max_capacity: usize) {
        let max_capacity = max_capacity.max(1);
        if self.len > max_capacity {
            self.discard_front(self.len - max_capacity);
        }
        if self.storage.len() > max_capacity {
            let mut storage = vec![0; self.len];
            self.copy_at(0, &mut storage);
            self.storage = storage;
            self.head = 0;
        }
        self.max_capacity = max_capacity;
    }
}

fn copy_into_wrapped(storage: &mut [u8], offset: usize, data: &[u8]) {
    let front_len = data.len().min(storage.len() - offset);
    storage[offset..offset + front_len].copy_from_slice(&data[..front_len]);
    if front_len < data.len() {
        storage[..data.len() - front_len].copy_from_slice(&data[front_len..]);
    }
}

fn copy_from_wrapped(storage: &[u8], offset: usize, output: &mut [u8]) {
    let output_len = output.len();
    let front_len = output_len.min(storage.len() - offset);
    output[..front_len].copy_from_slice(&storage[offset..offset + front_len]);
    if front_len < output_len {
        output[front_len..].copy_from_slice(&storage[..output_len - front_len]);
    }
}

impl HttpCacheConfig {
    fn from_playback_config(config: &PlaybackCacheConfig) -> Self {
        let config = config.clone().normalized();
        let cache_active = !matches!(config.mode, PlaybackCacheMode::Disabled);
        let configured_chunk = usize::try_from(config.http_cache_chunk_bytes)
            .unwrap_or(usize::MAX)
            .clamp(64 * 1024, 16 * 1024 * 1024);
        let configured_memory = usize::try_from(config.http_cache_max_bytes)
            .unwrap_or(usize::MAX)
            .max(configured_chunk);
        let chunk_size = env_usize("TINY_HTTP_CACHE_CHUNK_BYTES", configured_chunk)
            .clamp(64 * 1024, 16 * 1024 * 1024);
        let memory_capacity =
            env_usize("TINY_HTTP_CACHE_MEMORY_BYTES", configured_memory).max(chunk_size);
        let range_request_bytes = env_u64("TINY_HTTP_CACHE_RANGE_REQUEST_BYTES")
            .unwrap_or(config.http_cache_range_request_bytes)
            .clamp(64 * 1024, 128 * 1024 * 1024)
            .max(u64::try_from(chunk_size).unwrap_or(u64::MAX));
        Self {
            memory_capacity,
            chunk_size,
            range_request_bytes,
            readahead_seconds: env_f64(
                "TINY_HTTP_CACHE_READAHEAD_SECS",
                config.effective_readahead_secs(cache_active),
            )
            .max(1.0),
            hysteresis_seconds: env_f64(
                "TINY_HTTP_CACHE_HYSTERESIS_SECS",
                config.demuxer_hysteresis_secs,
            )
            .max(0.0),
            max_readahead_bytes: Some(
                env_u64("TINY_HTTP_CACHE_MAX_BYTES").unwrap_or(config.http_cache_max_bytes),
            ),
            disk_cache_bytes: config.disk_cache.then(|| {
                env_u64("TINY_HTTP_CACHE_DISK_BYTES").unwrap_or(config.disk_cache_max_bytes)
            }),
            cache_dir: config.cache_dir,
            unlink_files: config.unlink_files,
        }
    }

    #[cfg(test)]
    fn for_test(memory_capacity: usize) -> Self {
        Self {
            memory_capacity,
            chunk_size: HTTP_CACHE_CHUNK_SIZE.min(memory_capacity.max(1)),
            range_request_bytes: HTTP_CACHE_RANGE_REQUEST_BYTES,
            readahead_seconds: HTTP_CACHE_DEFAULT_READAHEAD_SECONDS,
            hysteresis_seconds: HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS,
            max_readahead_bytes: None,
            disk_cache_bytes: None,
            cache_dir: None,
            unlink_files: CacheUnlinkPolicy::WhenDone,
        }
    }
}

impl HttpDiskCache {
    fn new(
        max_bytes: u64,
        configured_dir: Option<PathBuf>,
        unlink_files: CacheUnlinkPolicy,
    ) -> Option<Self> {
        let dir = configured_dir
            .or_else(|| env::var("TINY_HTTP_CACHE_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(env::temp_dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, path = %dir.display(), "failed to create HTTP disk cache directory");
            return None;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "tiny-http-cache-{}-{stamp}.tmp",
            std::process::id()
        ));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to create HTTP disk cache file");
                return None;
            }
        };
        let mut unlink_on_drop = matches!(unlink_files, CacheUnlinkPolicy::WhenDone);
        if matches!(unlink_files, CacheUnlinkPolicy::Immediate) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to immediately unlink HTTP disk cache file");
                    unlink_on_drop = true;
                }
            }
        }

        Some(Self {
            file,
            path,
            ranges: Vec::new(),
            max_bytes,
            access_generation: 0,
            unlink_on_drop,
        })
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let written_now = self
                .file
                .write_at(&data[written..], offset.saturating_add(written as u64))?;
            if written_now == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "disk cache write returned zero bytes",
                ));
            }
            written += written_now;
        }
        self.add_range(offset, offset.saturating_add(data.len() as u64));
        self.trim_to_limit();
        Ok(())
    }

    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Option<usize> {
        let range_index = self.range_index_containing(offset)?;
        let range = self.ranges[range_index];
        let len = output.len().min(usize::try_from(range.end - offset).ok()?);
        if len == 0 {
            return None;
        }
        let read = self
            .file
            .read_at(&mut output[..len], offset)
            .ok()
            .filter(|read| *read > 0)?;
        let generation = self.next_access_generation();
        if let Some(range) = self.ranges.get_mut(range_index) {
            range.last_used_generation = generation;
        }
        Some(read)
    }

    fn add_range(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        let generation = self.next_access_generation();
        self.ranges.push(HttpCachedByteRange {
            start,
            end,
            last_used_generation: generation,
        });
        self.ranges.sort_by_key(|range| range.start);

        let mut merged: Vec<HttpCachedByteRange> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
                last.last_used_generation =
                    last.last_used_generation.max(range.last_used_generation);
                continue;
            }
            merged.push(range);
        }
        self.ranges = merged;
    }

    fn trim_to_limit(&mut self) {
        loop {
            let cached_bytes = self.cached_bytes();
            if cached_bytes <= self.max_bytes {
                break;
            }
            let Some(range_index) = self
                .ranges
                .iter()
                .enumerate()
                .min_by_key(|(_, range)| range.last_used_generation)
                .map(|(index, _)| index)
            else {
                break;
            };
            let overflow = cached_bytes.saturating_sub(self.max_bytes);
            let Some(range) = self.ranges.get_mut(range_index) else {
                break;
            };
            let trim = overflow.min(range.end.saturating_sub(range.start));
            range.start = range.start.saturating_add(trim);
            if range.start >= range.end {
                self.ranges.remove(range_index);
            }
        }
    }

    fn cached_bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start))
            .sum()
    }

    fn next_access_generation(&mut self) -> u64 {
        self.access_generation = self.access_generation.saturating_add(1);
        self.access_generation
    }

    fn range_index_containing(&self, offset: u64) -> Option<usize> {
        self.ranges
            .iter()
            .position(|range| offset >= range.start && offset < range.end)
    }
}

impl Drop for HttpDiskCache {
    fn drop(&mut self) {
        if !self.unlink_on_drop {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::debug!(%error, path = %self.path.display(), "failed to remove HTTP disk cache file");
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
}

impl HttpRingCache {
    pub(super) fn spawn(
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

    pub(super) fn read_at(&self, offset: u64, output: &mut [u8]) -> CacheReadResult {
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
                    return CacheReadResult::Data(total);
                }
                continue;
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

    pub(super) fn note_reader_offset(&self, offset: u64, range_kind: HttpCacheRangeKind) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.note_seek_offset(offset, range_kind);
        self.shared.ready.notify_all();
    }

    pub(super) fn is_tail_metadata_probe_seek(&self, offset: u64) -> bool {
        self.shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned")
            .is_tail_metadata_probe_seek(offset)
    }

    pub(super) fn content_len(&self) -> Option<u64> {
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

    pub(super) fn shutdown(&self) {
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

impl HttpRingCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new(start_offset: u64) -> Self {
        Self::new_with_config(
            start_offset,
            HttpCacheConfig::for_test(HTTP_RING_CACHE_CAPACITY),
        )
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new_with_cache_capacity(
        start_offset: u64,
        capacity: usize,
    ) -> Self {
        Self::new_with_config(start_offset, HttpCacheConfig::for_test(capacity))
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new_with_readahead_for_test(
        start_offset: u64,
        capacity: usize,
        readahead_seconds: f64,
        hysteresis_seconds: f64,
    ) -> Self {
        let config = HttpCacheConfig {
            memory_capacity: capacity,
            readahead_seconds,
            hysteresis_seconds,
            ..HttpCacheConfig::for_test(capacity)
        };
        Self::new_with_config(start_offset, config)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new_with_disk_cache_for_test(
        start_offset: u64,
        capacity: usize,
        disk_cache_bytes: u64,
    ) -> Self {
        let config = HttpCacheConfig {
            disk_cache_bytes: Some(disk_cache_bytes),
            ..HttpCacheConfig::for_test(capacity)
        };
        Self::new_with_config(start_offset, config)
    }

    fn new_with_config(start_offset: u64, config: HttpCacheConfig) -> Self {
        let disk_cache = config.disk_cache_bytes.and_then(|max_bytes| {
            HttpDiskCache::new(max_bytes, config.cache_dir.clone(), config.unlink_files)
        });
        let disk_cache_writable = disk_cache.is_some();
        Self {
            buffer: ByteRingBuffer::new(config.memory_capacity),
            base_offset: start_offset,
            next_offset: start_offset,
            active_request_start_offset: start_offset,
            retained_ranges: VecDeque::new(),
            disk_cache,
            disk_cache_writable,
            config,
            active_range_kind: HttpCacheRangeKind::Playback,
            pending_seek_range_kind: None,
            reader_offset: start_offset,
            byte_level_seeks: 0,
            input_rate_samples: VecDeque::new(),
            retained_access_generation: 0,
            duration_seconds: None,
            prefetch_paused: false,
            content_len: None,
            eof: false,
            shutdown: false,
            restart_request: None,
            side_download_requests: VecDeque::new(),
            side_download_active: Vec::new(),
            error: None,
            last_reported_status: None,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn apply_cache_config(
        &mut self,
        cache_config: &PlaybackCacheConfig,
    ) {
        let config = HttpCacheConfig::from_playback_config(cache_config);
        if config.memory_capacity != self.config.memory_capacity {
            self.buffer.resize_capacity(config.memory_capacity);
            for range in &mut self.retained_ranges {
                range.buffer.resize_capacity(config.memory_capacity);
            }
        }

        if let Some(max_bytes) = config.disk_cache_bytes {
            if self.disk_cache.is_none() {
                self.disk_cache =
                    HttpDiskCache::new(max_bytes, config.cache_dir.clone(), config.unlink_files);
            }
            if let Some(disk_cache) = self.disk_cache.as_mut() {
                disk_cache.max_bytes = max_bytes;
                disk_cache.trim_to_limit();
            }
            self.disk_cache_writable = self.disk_cache.is_some();
        } else {
            self.disk_cache_writable = false;
        }

        self.config = config;
        self.trim_to_capacity(self.config.memory_capacity);
        self.trim_retained_ranges_to_capacity(
            self.config
                .memory_capacity
                .saturating_sub(self.buffer.len()),
        );
        self.last_reported_status = None;
    }

    pub(in crate::player::backend::ffmpeg) fn with_content_len_hint(
        mut self,
        content_len_hint: Option<u64>,
    ) -> Self {
        self.content_len = content_len_hint.filter(|content_len| *content_len > 0);
        self
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_duration_seconds_for_test(
        &mut self,
        duration_seconds: f64,
    ) {
        self.duration_seconds =
            (duration_seconds.is_finite() && duration_seconds > 0.0).then_some(duration_seconds);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn restart_at(&mut self, offset: u64) {
        self.restart_at_with_kind(offset, HttpCacheRangeKind::Playback);
    }

    pub(in crate::player::backend::ffmpeg) fn restart_at_with_kind(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        tracing::debug!(
            offset,
            ?range_kind,
            previous_base_offset = self.base_offset,
            previous_next_offset = self.next_offset,
            previous_reader_offset = self.reader_offset,
            previous_active_range_kind = ?self.active_range_kind,
            previous_buffer_len = self.buffer.len(),
            "restarting HTTP stream cache range"
        );
        self.retain_current_range_for_restart(offset);
        self.buffer.clear();
        self.base_offset = offset;
        self.next_offset = offset;
        self.active_request_start_offset = offset;
        self.active_range_kind = range_kind;
        self.pending_seek_range_kind = None;
        self.reader_offset = offset;
        self.eof = false;
        self.error = None;
        self.last_reported_status = None;
    }

    fn retain_current_range_for_restart(&mut self, offset: u64) {
        if self.buffer.len() == 0 || (offset >= self.base_offset && offset <= self.next_offset) {
            return;
        }

        tracing::debug!(
            base_offset = self.base_offset,
            next_offset = self.next_offset,
            restart_offset = offset,
            active_range_kind = ?self.active_range_kind,
            "retaining HTTP stream cache range across restart"
        );
        let capacity = self.buffer.max_capacity();
        let buffer = std::mem::replace(&mut self.buffer, ByteRingBuffer::new(capacity));
        let last_used_generation = self.next_retained_access_generation();
        self.retained_ranges.push_back(RetainedCacheRange {
            buffer,
            base_offset: self.base_offset,
            next_offset: self.next_offset,
            range_kind: self.active_range_kind,
            last_used_generation,
        });
        self.trim_retained_ranges_to_capacity(self.config.memory_capacity);
    }

    fn next_retained_access_generation(&mut self) -> u64 {
        self.retained_access_generation = self.retained_access_generation.saturating_add(1);
        self.retained_access_generation
    }

    fn queue_read_miss_at(&mut self, offset: u64) -> bool {
        let range_kind = self.take_range_kind_for_miss(offset);
        self.request_side_download_at(offset, range_kind)
    }

    fn range_kind_for_miss(&self, offset: u64) -> HttpCacheRangeKind {
        self.pending_seek_range_kind
            .filter(|(pending_offset, _)| *pending_offset == offset)
            .map(|(_, range_kind)| range_kind)
            .unwrap_or_else(|| {
                if self.is_tail_metadata_probe_seek(offset) {
                    HttpCacheRangeKind::TailMetadataProbe
                } else {
                    HttpCacheRangeKind::Playback
                }
            })
    }

    fn take_range_kind_for_miss(&mut self, offset: u64) -> HttpCacheRangeKind {
        if let Some((pending_offset, range_kind)) = self.pending_seek_range_kind.take()
            && pending_offset == offset
        {
            return range_kind;
        }

        self.range_kind_for_miss(offset)
    }

    fn request_side_download_at(&mut self, offset: u64, range_kind: HttpCacheRangeKind) -> bool {
        if self.cached_range_contains(offset)
            || self.side_download_request_exists(offset, range_kind)
        {
            return false;
        }
        tracing::debug!(
            offset,
            ?range_kind,
            active_base_offset = self.base_offset,
            active_next_offset = self.next_offset,
            "queueing HTTP side download range"
        );
        self.side_download_requests
            .push_back(CacheRestartRequest { offset, range_kind });
        true
    }

    fn side_download_request_exists(&self, offset: u64, range_kind: HttpCacheRangeKind) -> bool {
        self.side_download_requests
            .iter()
            .chain(self.side_download_active.iter())
            .any(|request| {
                request.range_kind == range_kind
                    && request.offset <= offset
                    && offset
                        < request
                            .offset
                            .saturating_add(self.config.range_request_bytes)
                    && self
                        .content_len
                        .is_none_or(|content_len| request.offset < content_len)
            })
    }

    fn cached_range_contains(&self, offset: u64) -> bool {
        (offset >= self.base_offset && offset < self.next_offset)
            || self
                .retained_ranges
                .iter()
                .any(|range| offset >= range.base_offset && offset < range.next_offset)
            || self
                .disk_cache
                .as_ref()
                .is_some_and(|disk_cache| disk_cache.range_index_containing(offset).is_some())
    }

    fn side_download_may_produce(&self, offset: u64) -> bool {
        self.side_download_requests
            .iter()
            .chain(self.side_download_active.iter())
            .any(|request| {
                offset >= request.offset
                    && offset
                        < request
                            .offset
                            .saturating_add(self.config.range_request_bytes)
                    && self
                        .content_len
                        .is_none_or(|content_len| offset < content_len)
            })
    }

    fn finish_side_download_request(&mut self, request: CacheRestartRequest, completed: bool) {
        if let Some(index) = self
            .side_download_active
            .iter()
            .position(|active| *active == request)
        {
            self.side_download_active.remove(index);
        }
        if completed {
            self.schedule_playback_continuation_after_side_download(request);
        }
    }

    fn schedule_playback_continuation_after_side_download(&mut self, request: CacheRestartRequest) {
        if request.range_kind != HttpCacheRangeKind::Playback {
            return;
        }
        let Some(continuation_offset) = self
            .retained_ranges
            .iter()
            .find(|range| {
                range.range_kind == HttpCacheRangeKind::Playback
                    && request.offset >= range.base_offset
                    && request.offset < range.next_offset
                    && self.reader_offset >= range.base_offset
                    && self.reader_offset < range.next_offset
            })
            .map(|range| range.next_offset)
        else {
            return;
        };
        if self
            .content_len
            .is_some_and(|content_len| continuation_offset >= content_len)
        {
            self.eof = true;
            return;
        }
        if self.offset_in_active_range(continuation_offset)
            || self
                .restart_request
                .is_some_and(|pending| pending.offset == continuation_offset)
        {
            return;
        }
        tracing::debug!(
            request_offset = request.offset,
            continuation_offset,
            reader_offset = self.reader_offset,
            "scheduling HTTP active playback continuation after side range"
        );
        self.restart_request = Some(CacheRestartRequest {
            offset: continuation_offset,
            range_kind: HttpCacheRangeKind::Playback,
        });
        self.eof = false;
        self.prefetch_paused = false;
    }

    pub(in crate::player::backend::ffmpeg) fn append_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let input_len = data.len();
        let mut offset = offset;
        let mut data = data;
        self.record_input_bytes(input_len);
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = disk_cache.write_at(offset, data)
        {
            tracing::warn!(%error, "disabling HTTP disk cache after write failure");
            self.disk_cache_writable = false;
        }
        if offset != self.next_offset {
            self.restart_at_with_kind(offset, self.active_range_kind);
        }

        let max_capacity = self.buffer.max_capacity();
        if data.len() > max_capacity {
            let trim = data.len() - max_capacity;
            offset = offset.saturating_add(trim as u64);
            data = &data[trim..];
            self.restart_at_with_kind(offset, self.active_range_kind);
        }

        self.trim_to_capacity(max_capacity.saturating_sub(data.len()));
        if self.buffer.len().saturating_add(data.len()) > max_capacity {
            return false;
        }
        self.buffer.append(data);
        self.next_offset = offset.saturating_add(data.len() as u64);
        self.maybe_queue_playback_continuation();
        self.trim_to_capacity(self.config.memory_capacity);
        self.trim_retained_ranges_to_capacity(
            self.config
                .memory_capacity
                .saturating_sub(self.buffer.len()),
        );
        true
    }

    fn maybe_queue_playback_continuation(&mut self) {
        if self.active_range_kind != HttpCacheRangeKind::Playback
            || self.config.range_request_bytes == 0
        {
            return;
        }
        let continuation_offset = self
            .active_request_start_offset
            .saturating_add(self.config.range_request_bytes);
        if self
            .content_len
            .is_some_and(|content_len| continuation_offset >= content_len)
        {
            return;
        }
        if self.next_offset < self.playback_continuation_prefetch_offset(continuation_offset) {
            return;
        }
        if self.request_side_download_at(continuation_offset, HttpCacheRangeKind::Playback) {
            tracing::debug!(
                continuation_offset,
                active_request_start_offset = self.active_request_start_offset,
                active_next_offset = self.next_offset,
                range_request_bytes = self.config.range_request_bytes,
                "queued proactive HTTP playback continuation range"
            );
        }
    }

    fn playback_continuation_prefetch_offset(&self, continuation_offset: u64) -> u64 {
        let trigger_bytes = self
            .config
            .range_request_bytes
            .saturating_mul(HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR)
            / HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR.max(1);
        self.active_request_start_offset
            .saturating_add(trigger_bytes.max(1))
            .min(continuation_offset)
    }

    fn splice_retained_playback_at_active_end(&mut self, offset: u64) -> Option<u64> {
        if self.active_range_kind != HttpCacheRangeKind::Playback || offset != self.next_offset {
            return None;
        }
        let range_index = self.retained_ranges.iter().position(|range| {
            range.range_kind == HttpCacheRangeKind::Playback
                && offset >= range.base_offset
                && offset < range.next_offset
        })?;
        let range = self.retained_ranges.get(range_index)?;
        let copy_offset = usize::try_from(offset.saturating_sub(range.base_offset)).ok()?;
        let copy_len = usize::try_from(range.next_offset.saturating_sub(offset)).ok()?;
        if copy_len == 0 {
            return None;
        }

        let max_capacity = self.buffer.max_capacity();
        if copy_len > max_capacity {
            return None;
        }
        self.trim_to_capacity(max_capacity.saturating_sub(copy_len));
        if self.buffer.len().saturating_add(copy_len) > max_capacity {
            return None;
        }

        let mut data = vec![0; copy_len];
        let copied = self
            .retained_ranges
            .get(range_index)?
            .buffer
            .copy_at(copy_offset, &mut data);
        if copied == 0 {
            return None;
        }
        data.truncate(copied);
        let next_offset = offset.saturating_add(copied as u64);
        self.buffer.append(&data);
        self.next_offset = next_offset;
        self.active_request_start_offset = offset;
        if self
            .retained_ranges
            .get(range_index)
            .is_some_and(|range| offset <= range.base_offset && next_offset >= range.next_offset)
        {
            self.retained_ranges.remove(range_index);
        }
        self.maybe_queue_playback_continuation();
        self.trim_retained_ranges_to_capacity(
            self.config
                .memory_capacity
                .saturating_sub(self.buffer.len()),
        );
        tracing::debug!(
            offset,
            next_offset,
            copied,
            "spliced proactive HTTP playback range into active stream cache"
        );
        Some(next_offset)
    }

    pub(in crate::player::backend::ffmpeg) fn append_retained_at(
        &mut self,
        offset: u64,
        data: &[u8],
        range_kind: HttpCacheRangeKind,
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let input_len = data.len();
        let original_offset = offset;
        self.record_input_bytes(input_len);
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = disk_cache.write_at(offset, data)
        {
            tracing::warn!(%error, "disabling HTTP disk cache after side-range write failure");
            self.disk_cache_writable = false;
        }

        let max_capacity = self.buffer.max_capacity();
        let mut offset = offset;
        let mut data = data;
        if data.len() > max_capacity {
            let trim = data.len() - max_capacity;
            offset = offset.saturating_add(trim as u64);
            data = &data[trim..];
        }
        if data.is_empty() {
            return true;
        }

        let range_index = self.retained_range_index_for_append(offset, range_kind);
        let range_index = match range_index {
            Some(range_index) => range_index,
            None => {
                let mut buffer = ByteRingBuffer::new(max_capacity);
                buffer.append(data);
                let last_used_generation = self.next_retained_access_generation();
                self.retained_ranges.push_back(RetainedCacheRange {
                    buffer,
                    base_offset: offset,
                    next_offset: offset.saturating_add(data.len() as u64),
                    range_kind,
                    last_used_generation,
                });
                self.trim_retained_ranges_to_capacity(
                    self.config
                        .memory_capacity
                        .saturating_sub(self.buffer.len()),
                );
                tracing::debug!(
                    offset = original_offset,
                    retained_offset = offset,
                    len = input_len,
                    ?range_kind,
                    "stored HTTP side download in retained cache range"
                );
                return true;
            }
        };

        let generation = self.next_retained_access_generation();
        if let Some(range) = self.retained_ranges.get_mut(range_index) {
            if offset < range.next_offset {
                let skip = usize::try_from(range.next_offset - offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                offset = offset.saturating_add(skip as u64);
                data = &data[skip..];
            }
            if data.is_empty() {
                range.last_used_generation = generation;
                return true;
            }
            let overflow = range
                .buffer
                .len()
                .saturating_add(data.len())
                .saturating_sub(max_capacity);
            if overflow > 0 {
                range.buffer.discard_front(overflow);
                range.base_offset = range.base_offset.saturating_add(overflow as u64);
            }
            if range.buffer.len().saturating_add(data.len()) > max_capacity {
                return false;
            }
            range.buffer.append(data);
            range.next_offset = offset.saturating_add(data.len() as u64);
            range.last_used_generation = generation;
        }
        self.trim_retained_ranges_to_capacity(
            self.config
                .memory_capacity
                .saturating_sub(self.buffer.len()),
        );
        true
    }

    fn retained_range_index_for_append(
        &self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> Option<usize> {
        self.retained_ranges.iter().position(|range| {
            range.range_kind == range_kind
                && offset >= range.base_offset
                && offset <= range.next_offset
        })
    }

    fn record_input_bytes(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.input_rate_samples.push_back(InputRateSample {
            at: Instant::now(),
            bytes,
        });
        self.prune_input_rate_samples();
    }

    fn raw_input_rate(&self) -> Option<u64> {
        let now = Instant::now();
        let bytes: usize = self
            .input_rate_samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) <= Duration::from_secs(1))
            .map(|sample| sample.bytes)
            .sum();
        (bytes > 0).then(|| u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    fn prune_input_rate_samples(&mut self) {
        let now = Instant::now();
        while self
            .input_rate_samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) > Duration::from_secs(1))
        {
            self.input_rate_samples.pop_front();
        }
    }

    pub(in crate::player::backend::ffmpeg) fn trim_to_capacity(&mut self, capacity: usize) {
        let overflow = self.buffer.len().saturating_sub(capacity);
        if overflow == 0 {
            return;
        }
        let consumed = if self.offset_in_active_range(self.reader_offset) {
            self.reader_offset
                .saturating_sub(self.base_offset)
                .min(self.buffer.len() as u64) as usize
        } else {
            0
        };
        let trim = overflow.min(consumed);
        if trim == 0 {
            return;
        }
        self.buffer.discard_front(trim);
        self.base_offset = self.base_offset.saturating_add(trim as u64);
    }

    fn trim_retained_ranges_to_capacity(&mut self, capacity: usize) {
        let mut retained_bytes = self.retained_memory_bytes();
        while retained_bytes > capacity {
            let Some(range_index) = self
                .retained_ranges
                .iter()
                .enumerate()
                .min_by_key(|(_, range)| range.last_used_generation)
                .map(|(index, _)| index)
            else {
                break;
            };
            let Some(range) = self.retained_ranges.get_mut(range_index) else {
                break;
            };
            let overflow = retained_bytes - capacity;
            let trim = overflow.min(range.buffer.len());
            if trim == 0 {
                break;
            }
            range.buffer.discard_front(trim);
            range.base_offset = range.base_offset.saturating_add(trim as u64);
            retained_bytes -= trim;
            if range.buffer.len() == 0 || range.base_offset >= range.next_offset {
                self.retained_ranges.remove(range_index);
            }
        }
    }

    fn retained_memory_bytes(&self) -> usize {
        self.retained_ranges
            .iter()
            .map(|range| range.buffer.len())
            .sum()
    }

    pub(in crate::player::backend::ffmpeg) fn copy_available(
        &mut self,
        offset: u64,
        output: &mut [u8],
    ) -> Option<usize> {
        if let Some(read) = copy_available_from_range(
            &self.buffer,
            self.base_offset,
            self.next_offset,
            offset,
            output,
        ) {
            return Some(read);
        }
        for index in (0..self.retained_ranges.len()).rev() {
            let Some(read) = self.retained_ranges.get(index).and_then(|range| {
                copy_available_from_range(
                    &range.buffer,
                    range.base_offset,
                    range.next_offset,
                    offset,
                    output,
                )
            }) else {
                continue;
            };
            let generation = self.next_retained_access_generation();
            if let Some(range) = self.retained_ranges.get_mut(index) {
                range.last_used_generation = generation;
            }
            return Some(read);
        }
        self.disk_cache
            .as_mut()
            .and_then(|disk_cache| disk_cache.read_at(offset, output))
    }

    pub(in crate::player::backend::ffmpeg) fn set_reader_offset(&mut self, offset: u64) {
        self.reader_offset = offset;
        if self.offset_in_active_range(offset) {
            self.trim_to_capacity(self.config.memory_capacity);
        }
    }

    pub(in crate::player::backend::ffmpeg) fn note_seek_offset(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        if !self.offset_in_active_range(offset) {
            self.byte_level_seeks = self.byte_level_seeks.saturating_add(1);
        }
        self.pending_seek_range_kind = Some((offset, range_kind));
        if range_kind == HttpCacheRangeKind::Playback {
            if !self.offset_in_active_range(offset) {
                self.demote_active_range_to_retained();
            }
            self.set_reader_offset(offset);
        }
    }

    fn demote_active_range_to_retained(&mut self) {
        if self.buffer.len() == 0 || self.next_offset <= self.base_offset {
            self.base_offset = self.next_offset;
            return;
        }

        tracing::debug!(
            base_offset = self.base_offset,
            next_offset = self.next_offset,
            active_range_kind = ?self.active_range_kind,
            "demoting inactive HTTP active range to retained cache range"
        );
        let capacity = self.buffer.max_capacity();
        let buffer = std::mem::replace(&mut self.buffer, ByteRingBuffer::new(capacity));
        let last_used_generation = self.next_retained_access_generation();
        self.retained_ranges.push_back(RetainedCacheRange {
            buffer,
            base_offset: self.base_offset,
            next_offset: self.next_offset,
            range_kind: self.active_range_kind,
            last_used_generation,
        });
        self.base_offset = self.next_offset;
        self.trim_retained_ranges_to_capacity(self.config.memory_capacity);
    }

    fn offset_in_active_range(&self, offset: u64) -> bool {
        offset >= self.base_offset && offset <= self.next_offset
    }

    pub(in crate::player::backend::ffmpeg) fn is_tail_metadata_probe_seek(
        &self,
        offset: u64,
    ) -> bool {
        let Some(content_len) = self.content_len else {
            return false;
        };
        if offset >= content_len
            || offset < content_len.saturating_sub(self.config.range_request_bytes)
        {
            return false;
        }

        let active_range_near_offset = self.active_range_kind == HttpCacheRangeKind::Playback
            && self.buffer.len() > 0
            && offset
                <= self
                    .next_offset
                    .saturating_add(self.config.range_request_bytes)
            && self.base_offset <= offset.saturating_add(self.config.range_request_bytes);
        !active_range_near_offset
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn buffered_ahead_from(&self, offset: u64) -> u64 {
        if self.active_range_kind != HttpCacheRangeKind::Playback
            || self.buffer.len() == 0
            || offset < self.base_offset
            || offset > self.next_offset
        {
            return 0;
        }
        self.next_offset.saturating_sub(offset)
    }

    pub(in crate::player::backend::ffmpeg) fn append_capacity_from(
        &mut self,
        offset: u64,
    ) -> usize {
        self.trim_to_capacity(self.config.memory_capacity);
        if self.active_range_kind == HttpCacheRangeKind::Playback
            && !self.offset_in_active_range(self.reader_offset)
            && (self.cached_range_contains(self.reader_offset)
                || self.side_download_may_produce(self.reader_offset))
        {
            self.prefetch_paused = true;
            return 0;
        }
        let active_reader_offset = self.reader_offset.max(self.base_offset);
        let buffered_ahead = offset.saturating_sub(active_reader_offset);
        let target = self.target_readahead_bytes();
        let resume = self.resume_readahead_bytes(target);
        if self.prefetch_paused {
            if buffered_ahead > resume {
                return 0;
            }
            self.prefetch_paused = false;
        }
        if buffered_ahead >= target {
            self.prefetch_paused = true;
            return 0;
        }

        let buffered_ahead = usize::try_from(buffered_ahead).unwrap_or(usize::MAX);
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        self.config
            .memory_capacity
            .min(target)
            .saturating_sub(buffered_ahead)
    }

    fn target_readahead_bytes(&self) -> u64 {
        let memory_capacity = self.config.memory_capacity as u64;
        let by_seconds = self
            .content_len
            .zip(self.duration_seconds)
            .map(|(content_len, duration)| {
                ((content_len as f64 / duration) * self.config.readahead_seconds).round() as u64
            })
            .filter(|bytes| *bytes > 0)
            .unwrap_or(memory_capacity);
        let by_config = self.config.max_readahead_bytes.unwrap_or(memory_capacity);
        by_seconds.min(by_config).min(memory_capacity).max(1)
    }

    fn resume_readahead_bytes(&self, target: u64) -> u64 {
        let Some((content_len, duration)) = self.content_len.zip(self.duration_seconds) else {
            return target / 2;
        };
        let hysteresis_bytes =
            ((content_len as f64 / duration) * self.config.hysteresis_seconds).round() as u64;
        if hysteresis_bytes == 0 {
            target
        } else {
            target.saturating_sub(hysteresis_bytes).min(target)
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_buffer_progress(
        &self,
    ) -> Option<PlaybackCacheByteRange> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        let mut start_offset = None;
        let mut end_offset = None;
        for range in self
            .retained_ranges
            .iter()
            .filter(|range| range.range_kind == HttpCacheRangeKind::Playback)
        {
            extend_buffer_progress_range(
                &mut start_offset,
                &mut end_offset,
                range.base_offset,
                range.next_offset,
                content_len,
            );
        }
        if self.active_range_kind == HttpCacheRangeKind::Playback {
            extend_buffer_progress_range(
                &mut start_offset,
                &mut end_offset,
                self.base_offset,
                self.next_offset,
                content_len,
            );
        }
        let start_offset = start_offset?;
        let end_offset = end_offset?.max(start_offset);
        let content_len = content_len as f64;
        Some(PlaybackCacheByteRange {
            start_fraction: start_offset as f64 / content_len,
            end_fraction: end_offset as f64 / content_len,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn playback_buffer_range(
        &self,
    ) -> Option<HttpPlaybackBufferRange> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        match self.active_range_kind {
            HttpCacheRangeKind::Playback => {
                playback_buffer_range(self.base_offset, self.next_offset, content_len)
            }
            HttpCacheRangeKind::TailMetadataProbe => {
                let range = self
                    .retained_ranges
                    .iter()
                    .rev()
                    .find(|range| range.range_kind == HttpCacheRangeKind::Playback)?;
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
            }
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_cache_status_for_test(
        &self,
    ) -> ByteCacheState {
        self.stream_cache_status()
    }

    fn stream_cache_status(&self) -> ByteCacheState {
        let content_len = self.content_len;
        let ranges = self.stream_buffer_ranges();
        let reader_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.reader_offset.min(content_len) as f64 / content_len as f64);
        let download_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.next_offset.min(content_len) as f64 / content_len as f64);
        ByteCacheState {
            ranges: ranges.into_iter().map(Into::into).collect(),
            reader_fraction,
            download_fraction,
            cached_bytes: self.cached_bytes(),
            content_length: content_len,
            disk_cache_enabled: self.disk_cache_writable,
            idle: self.cache_idle(),
            raw_input_rate: self.raw_input_rate(),
            byte_level_seeks: self.byte_level_seeks,
        }
    }

    fn cache_idle(&self) -> bool {
        (self.prefetch_paused || self.eof)
            && self.restart_request.is_none()
            && self.side_download_requests.is_empty()
            && self.side_download_active.is_empty()
    }

    fn stream_buffer_ranges(&self) -> Vec<HttpPlaybackBufferRange> {
        let Some(content_len) = self.content_len.filter(|content_len| *content_len > 0) else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        for range in &self.retained_ranges {
            if let Some(range) =
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
            {
                ranges.push(range);
            }
        }
        if let Some(range) = playback_buffer_range(self.base_offset, self.next_offset, content_len)
        {
            ranges.push(range);
        }
        if let Some(disk_cache) = &self.disk_cache {
            ranges.extend(
                disk_cache
                    .ranges
                    .iter()
                    .filter_map(|range| playback_buffer_range(range.start, range.end, content_len)),
            );
        }
        merge_playback_buffer_ranges(ranges)
    }

    fn cached_bytes(&self) -> u64 {
        let mut ranges = Vec::new();
        if self.next_offset > self.base_offset {
            ranges.push((self.base_offset, self.next_offset));
        }
        ranges.extend(
            self.retained_ranges
                .iter()
                .filter(|range| range.next_offset > range.base_offset)
                .map(|range| (range.base_offset, range.next_offset)),
        );
        if let Some(disk_cache) = &self.disk_cache {
            ranges.extend(
                disk_cache
                    .ranges
                    .iter()
                    .map(|range| (range.start, range.end)),
            );
        }
        merged_cached_byte_len(ranges)
    }

    fn take_stream_cache_status_report(&mut self) -> Option<ByteCacheState> {
        let status = self.stream_cache_status();
        if !http_stream_cache_status_changed(
            self.last_reported_status.as_ref(),
            &status,
            self.config.range_request_bytes,
        ) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}

#[cfg(test)]
fn extend_buffer_progress_range(
    start_offset: &mut Option<u64>,
    end_offset: &mut Option<u64>,
    base_offset: u64,
    next_offset: u64,
    content_len: u64,
) {
    let range_start = base_offset.min(content_len);
    let range_end = next_offset.min(content_len).max(range_start);
    if range_end <= range_start {
        return;
    }
    *start_offset = Some(start_offset.map_or(range_start, |start| start.min(range_start)));
    *end_offset = Some(end_offset.map_or(range_end, |end| end.max(range_end)));
}

fn playback_buffer_range(
    base_offset: u64,
    next_offset: u64,
    content_len: u64,
) -> Option<HttpPlaybackBufferRange> {
    let range_start = base_offset.min(content_len);
    let range_end = next_offset.min(content_len).max(range_start);
    if range_end <= range_start {
        return None;
    }

    Some(HttpPlaybackBufferRange {
        start_offset: range_start,
        end_offset: range_end,
        content_len,
    })
}

fn merge_playback_buffer_ranges(
    mut ranges: Vec<HttpPlaybackBufferRange>,
) -> Vec<HttpPlaybackBufferRange> {
    ranges.sort_by_key(|range| range.start_offset);
    let mut merged: Vec<HttpPlaybackBufferRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start_offset <= last.end_offset
        {
            last.end_offset = last.end_offset.max(range.end_offset);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn merged_cached_byte_len(mut ranges: Vec<(u64, u64)>) -> u64 {
    ranges.retain(|(start, end)| end > start);
    ranges.sort_by_key(|(start, _)| *start);

    let mut total = 0u64;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in ranges {
        match current {
            Some((current_start, current_end)) if start <= current_end => {
                current = Some((current_start, current_end.max(end)));
            }
            Some((current_start, current_end)) => {
                total = total.saturating_add(current_end.saturating_sub(current_start));
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        total = total.saturating_add(end.saturating_sub(start));
    }
    total
}

impl From<HttpPlaybackBufferRange> for PlaybackCacheByteRange {
    fn from(range: HttpPlaybackBufferRange) -> Self {
        let content_len = range.content_len as f64;
        Self {
            start_fraction: range.start_offset as f64 / content_len,
            end_fraction: range.end_offset as f64 / content_len,
        }
    }
}

fn copy_available_from_range(
    buffer: &ByteRingBuffer,
    base_offset: u64,
    next_offset: u64,
    offset: u64,
    output: &mut [u8],
) -> Option<usize> {
    if offset < base_offset || offset >= next_offset {
        return None;
    }
    let start = usize::try_from(offset - base_offset).ok()?;
    if start >= buffer.len() {
        return None;
    }
    let read = buffer.copy_at(start, output);
    (read > 0).then_some(read)
}

impl HttpRingCacheShared {
    pub(super) fn should_stop(&self) -> bool {
        self.control.should_stop()
            || self
                .state
                .lock()
                .expect("HTTP stream cache poisoned")
                .shutdown
    }

    pub(super) fn take_restart_offset(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .restart_request
            .take()
            .map(|request| request.offset)
    }

    pub(super) fn set_error(&self, error: String) {
        tracing::warn!(%error, "HTTP video stream cache failed");
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.error = Some(error);
        self.ready.notify_all();
    }

    pub(super) fn mark_eof(&self) {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.eof = true;
            guard.take_stream_cache_status_report()
        };
        self.send_stream_cache_status(status);
        self.ready.notify_all();
    }

    pub(super) fn wait_for_restart_after_eof(&self) -> Option<u64> {
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

    pub(super) fn wait_for_side_download_request(&self) -> Option<CacheRestartRequest> {
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

    pub(super) fn finish_side_download(&self, request: CacheRestartRequest, completed: bool) {
        let status = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.finish_side_download_request(request, completed && !self.control.should_stop());
            guard.take_stream_cache_status_report()
        };
        self.send_stream_cache_status(status);
        self.ready.notify_all();
    }

    pub(super) fn set_content_len(&self, content_len: Option<u64>) {
        if let Some(content_len) = content_len {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.content_len = Some(content_len);
            self.ready.notify_all();
        }
    }

    pub(super) fn content_len_now(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .content_len
    }

    pub(super) fn chunk_size(&self) -> usize {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .chunk_size
    }

    pub(super) fn range_request_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .config
            .range_request_bytes
    }

    pub(super) fn wait_for_append_capacity(&self, offset: u64) -> CacheAppendPermit {
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

    pub(super) fn append_capacity_now(&self, offset: u64) -> CacheAppendPermit {
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

    pub(super) fn append_or_restart(&self, offset: u64, data: &[u8]) -> CacheAppendResult {
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

    pub(super) fn append_side_download_or_stop(
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
            if !guard.append_retained_at(offset, data, request.range_kind) {
                return CacheAppendResult::Restart(offset);
            }
            guard.take_stream_cache_status_report()
        };
        self.ready.notify_all();
        self.send_cache_events(status);
        CacheAppendResult::Appended
    }
}

fn http_stream_buffer_progress_changed(
    previous: Option<PlaybackCacheByteRange>,
    next: PlaybackCacheByteRange,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    (previous.start_fraction - next.start_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (previous.end_fraction - next.end_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (next.end_fraction >= 1.0 && previous.end_fraction < 1.0)
}

fn playback_cache_state_from_http_status(status: ByteCacheState) -> PlaybackCacheState {
    let raw_input_rate = status.raw_input_rate;
    let byte_level_seeks = status.byte_level_seeks;
    PlaybackCacheState {
        demux: DemuxCacheState {
            raw_input_rate,
            byte_level_seeks,
            ..DemuxCacheState::default()
        },
        byte: Some(status),
        ..PlaybackCacheState::default()
    }
}

fn http_stream_cache_status_changed(
    previous: Option<&ByteCacheState>,
    next: &ByteCacheState,
    cached_bytes_threshold: u64,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.disk_cache_enabled != next.disk_cache_enabled
        || previous.idle != next.idle
        || previous.content_length != next.content_length
        || previous.ranges.len() != next.ranges.len()
        || previous.raw_input_rate.is_some() != next.raw_input_rate.is_some()
        || previous.byte_level_seeks != next.byte_level_seeks
    {
        return true;
    }
    if previous
        .reader_fraction
        .zip(next.reader_fraction)
        .is_some_and(|(previous, next)| {
            (previous - next).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        })
    {
        return true;
    }
    if previous
        .download_fraction
        .zip(next.download_fraction)
        .is_some_and(|(previous, next)| {
            (previous - next).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        })
    {
        return true;
    }
    if previous
        .raw_input_rate
        .zip(next.raw_input_rate)
        .is_some_and(|(previous, next)| previous.abs_diff(next) >= 64 * 1024)
    {
        return true;
    }
    previous.cached_bytes.abs_diff(next.cached_bytes) >= cached_bytes_threshold
        || previous
            .ranges
            .iter()
            .zip(next.ranges.iter())
            .any(|(previous, next)| http_stream_buffer_progress_changed(Some(*previous), *next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_cache_state_queues_tail_side_download_without_active_restart() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));

        state.request_side_download_at(990, HttpCacheRangeKind::TailMetadataProbe);

        assert_eq!(state.base_offset, 100);
        assert_eq!(state.next_offset, 106);
        assert!(state.restart_request.is_none());
        assert_eq!(
            state
                .side_download_requests
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![CacheRestartRequest {
                offset: 990,
                range_kind: HttpCacheRangeKind::TailMetadataProbe,
            }]
        );
        assert!(state.side_download_may_produce(990));
    }

    #[test]
    fn http_cache_state_queues_playback_read_miss_without_active_restart() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));

        state.queue_read_miss_at(500);

        assert_eq!(state.base_offset, 100);
        assert_eq!(state.next_offset, 106);
        assert!(state.restart_request.is_none());
        assert_eq!(
            state
                .side_download_requests
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![CacheRestartRequest {
                offset: 500,
                range_kind: HttpCacheRangeKind::Playback,
            }]
        );
        assert!(state.side_download_may_produce(500));
    }

    #[test]
    fn http_cache_state_proactively_queues_next_playback_range() {
        let config = HttpCacheConfig {
            range_request_bytes: 100,
            ..HttpCacheConfig::for_test(1_000)
        };
        let mut state =
            HttpRingCacheState::new_with_config(0, config).with_content_len_hint(Some(1_000));

        assert!(state.append_at(0, &[0; 49]));
        assert!(state.side_download_requests.is_empty());

        assert!(state.append_at(49, &[0; 1]));

        assert_eq!(
            state
                .side_download_requests
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![CacheRestartRequest {
                offset: 100,
                range_kind: HttpCacheRangeKind::Playback,
            }]
        );
    }

    #[test]
    fn http_cache_state_demotes_active_range_when_playback_seek_leaves_it() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));

        state.note_seek_offset(500, HttpCacheRangeKind::Playback);

        assert_eq!(state.base_offset, 106);
        assert_eq!(state.next_offset, 106);
        assert_eq!(state.reader_offset, 500);
        assert_eq!(state.stream_cache_status().byte_level_seeks, 1);
        let mut output = [0; 3];
        assert_eq!(state.copy_available(102, &mut output), Some(3));
        assert_eq!(&output, b"cde");
    }

    #[test]
    fn http_cache_status_reports_byte_level_seek_count_changes() {
        let mut state = HttpRingCacheState::new(100);
        assert!(state.append_at(100, b"abcdef"));
        assert!(state.take_stream_cache_status_report().is_some());
        assert!(state.take_stream_cache_status_report().is_none());

        state.note_seek_offset(500, HttpCacheRangeKind::Playback);

        let status = state
            .take_stream_cache_status_report()
            .expect("byte-level seek count change is reportable");
        assert_eq!(status.byte_level_seeks, 1);
    }

    #[test]
    fn http_cache_state_pauses_inactive_active_prefetch_while_side_range_can_serve_reader() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.note_seek_offset(500, HttpCacheRangeKind::Playback);
        state.queue_read_miss_at(500);

        assert_eq!(state.append_capacity_from(106), 0);
        assert!(state.prefetch_paused);
    }

    #[test]
    fn http_cache_status_is_not_idle_while_side_download_is_pending() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.note_seek_offset(500, HttpCacheRangeKind::Playback);
        state.queue_read_miss_at(500);

        assert_eq!(state.append_capacity_from(106), 0);

        assert!(!state.stream_cache_status().idle);
    }

    #[test]
    fn http_cache_status_reports_side_download_queue_as_active_work() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.prefetch_paused = true;
        assert!(state.stream_cache_status().idle);
        assert!(state.take_stream_cache_status_report().is_some());

        assert!(state.queue_read_miss_at(500));

        let status = state
            .take_stream_cache_status_report()
            .expect("side download activity is reportable");
        assert!(!status.idle);
    }

    #[test]
    fn http_cache_probe_read_reports_queued_side_download_activity() {
        let (event_tx, event_rx) = mpsc::channel();
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.prefetch_paused = true;
        assert!(state.take_stream_cache_status_report().is_some());
        let cache = HttpRingCache {
            shared: Arc::new(HttpRingCacheShared {
                state: Mutex::new(state),
                ready: Condvar::new(),
                control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
                event_tx,
            }),
        };
        let mut output = [0; 4];

        assert!(matches!(
            cache.read_cached_at(500, &mut output),
            CacheReadResult::WouldBlock
        ));

        let event = event_rx
            .try_recv()
            .expect("queued side download status event is sent");
        assert!(matches!(
            event.kind,
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                byte: Some(ByteCacheState { idle: false, .. }),
                ..
            })
        ));
    }

    #[test]
    fn http_cache_status_is_idle_after_eof_without_prefetch_pause() {
        let mut state = HttpRingCacheState::new(0);
        assert!(state.append_at(0, b"abcdef"));
        state.eof = true;

        assert!(state.stream_cache_status().idle);
    }

    #[test]
    fn http_cache_shared_reports_idle_when_eof_reached() {
        let (event_tx, event_rx) = mpsc::channel();
        let shared = HttpRingCacheShared {
            state: Mutex::new(HttpRingCacheState::new(0)),
            ready: Condvar::new(),
            control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
            event_tx,
        };
        {
            let mut guard = shared.state.lock().expect("state locks");
            assert!(guard.append_at(0, b"abcdef"));
            assert!(!guard.stream_cache_status().idle);
            assert!(guard.take_stream_cache_status_report().is_some());
        }

        shared.mark_eof();

        let event = event_rx.try_recv().expect("EOF status event is sent");
        assert!(matches!(
            event.kind,
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                byte: Some(ByteCacheState { idle: true, .. }),
                ..
            })
        ));
    }

    #[test]
    fn http_cache_shared_reports_idle_after_last_side_download_finishes() {
        let (event_tx, event_rx) = mpsc::channel();
        let shared = HttpRingCacheShared {
            state: Mutex::new(HttpRingCacheState::new(100).with_content_len_hint(Some(1_000))),
            ready: Condvar::new(),
            control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
            event_tx,
        };
        let request = {
            let mut guard = shared.state.lock().expect("state locks");
            assert!(guard.append_at(100, b"abcdef"));
            guard.note_seek_offset(500, HttpCacheRangeKind::Playback);
            guard.queue_read_miss_at(500);
            assert_eq!(guard.append_capacity_from(106), 0);
            assert!(!guard.stream_cache_status().idle);
            assert!(guard.take_stream_cache_status_report().is_some());
            let request = guard
                .side_download_requests
                .pop_front()
                .expect("side download was queued");
            guard.side_download_active.push(request);
            request
        };

        shared.finish_side_download(request, false);

        let event = event_rx
            .try_recv()
            .expect("side completion status event is sent");
        assert!(matches!(
            event.kind,
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                byte: Some(ByteCacheState { idle: true, .. }),
                ..
            })
        ));
    }

    #[test]
    fn http_cache_state_schedules_active_continuation_after_playback_side_range() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.note_seek_offset(500, HttpCacheRangeKind::Playback);
        state.queue_read_miss_at(500);
        let request = state
            .side_download_requests
            .pop_front()
            .expect("side download was queued");
        state.side_download_active.push(request);
        assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

        state.finish_side_download_request(request, true);

        assert!(state.side_download_active.is_empty());
        assert_eq!(
            state.restart_request,
            Some(CacheRestartRequest {
                offset: 504,
                range_kind: HttpCacheRangeKind::Playback,
            })
        );
    }

    #[test]
    fn http_cache_state_marks_eof_instead_of_continuing_after_terminal_side_range() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(504));
        assert!(state.append_at(100, b"abcdef"));
        state.note_seek_offset(500, HttpCacheRangeKind::Playback);
        state.queue_read_miss_at(500);
        let request = state
            .side_download_requests
            .pop_front()
            .expect("side download was queued");
        state.side_download_active.push(request);
        assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

        state.finish_side_download_request(request, true);

        assert!(state.side_download_active.is_empty());
        assert!(state.restart_request.is_none());
        assert!(state.eof);
        assert!(state.stream_cache_status_for_test().idle);
    }

    #[test]
    fn http_cache_state_does_not_schedule_active_continuation_for_incomplete_side_range() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));
        state.note_seek_offset(500, HttpCacheRangeKind::Playback);
        state.queue_read_miss_at(500);
        let request = state
            .side_download_requests
            .pop_front()
            .expect("side download was queued");
        state.side_download_active.push(request);
        assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

        state.finish_side_download_request(request, false);

        assert!(state.side_download_active.is_empty());
        assert!(state.restart_request.is_none());
    }

    #[test]
    fn http_cache_state_splices_proactive_playback_range_at_active_end() {
        let config = HttpCacheConfig {
            range_request_bytes: 6,
            ..HttpCacheConfig::for_test(64)
        };
        let mut state =
            HttpRingCacheState::new_with_config(0, config).with_content_len_hint(Some(64));
        assert!(state.append_at(0, b"abcdef"));
        assert!(state.append_retained_at(6, b"ghijkl", HttpCacheRangeKind::Playback));

        assert_eq!(state.splice_retained_playback_at_active_end(6), Some(12));

        let mut output = [0; 12];
        assert_eq!(state.copy_available(0, &mut output), Some(12));
        assert_eq!(&output, b"abcdefghijkl");
        assert_eq!(state.next_offset, 12);
        assert_eq!(state.active_request_start_offset, 6);
        assert!(state.retained_ranges.is_empty());
    }

    #[test]
    fn http_cache_state_does_not_queue_side_download_for_cached_offset() {
        let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
        assert!(state.append_at(100, b"abcdef"));

        state.request_side_download_at(102, HttpCacheRangeKind::TailMetadataProbe);

        assert!(state.side_download_requests.is_empty());
    }

    #[test]
    fn http_cache_state_queues_multiple_side_downloads_and_suppresses_duplicates() {
        let mut state = HttpRingCacheState::new(100)
            .with_content_len_hint(Some(HTTP_CACHE_RANGE_REQUEST_BYTES * 4));

        state.request_side_download_at(1_000, HttpCacheRangeKind::TailMetadataProbe);
        state.request_side_download_at(
            1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES / 2,
            HttpCacheRangeKind::TailMetadataProbe,
        );
        state.request_side_download_at(
            1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
            HttpCacheRangeKind::TailMetadataProbe,
        );

        assert_eq!(
            state
                .side_download_requests
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                CacheRestartRequest {
                    offset: 1_000,
                    range_kind: HttpCacheRangeKind::TailMetadataProbe,
                },
                CacheRestartRequest {
                    offset: 1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
                    range_kind: HttpCacheRangeKind::TailMetadataProbe,
                },
            ]
        );
    }

    #[test]
    fn http_cache_state_uses_configured_side_download_range_request_budget() {
        let config = HttpCacheConfig {
            range_request_bytes: 1024,
            ..HttpCacheConfig::for_test(1024)
        };
        let mut state =
            HttpRingCacheState::new_with_config(100, config).with_content_len_hint(Some(10_000));

        state.request_side_download_at(1_000, HttpCacheRangeKind::Playback);
        state.request_side_download_at(1_500, HttpCacheRangeKind::Playback);
        state.request_side_download_at(2_025, HttpCacheRangeKind::Playback);

        assert_eq!(
            state
                .side_download_requests
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                CacheRestartRequest {
                    offset: 1_000,
                    range_kind: HttpCacheRangeKind::Playback,
                },
                CacheRestartRequest {
                    offset: 2_025,
                    range_kind: HttpCacheRangeKind::Playback,
                },
            ]
        );
    }

    #[test]
    fn http_cache_shared_dispatches_multiple_side_downloads_to_active_set() {
        let (event_tx, _) = mpsc::channel();
        let shared = HttpRingCacheShared {
            state: Mutex::new(
                HttpRingCacheState::new(100)
                    .with_content_len_hint(Some(HTTP_CACHE_RANGE_REQUEST_BYTES * 4)),
            ),
            ready: Condvar::new(),
            control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
            event_tx,
        };
        {
            let mut guard = shared.state.lock().expect("state locks");
            guard.request_side_download_at(1_000, HttpCacheRangeKind::TailMetadataProbe);
            guard.request_side_download_at(
                1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
                HttpCacheRangeKind::TailMetadataProbe,
            );
        }

        let first = shared
            .wait_for_side_download_request()
            .expect("first request dequeues");
        let second = shared
            .wait_for_side_download_request()
            .expect("second request dequeues");

        {
            let guard = shared.state.lock().expect("state locks");
            assert!(guard.side_download_requests.is_empty());
            assert_eq!(guard.side_download_active, vec![first, second]);
        }
        shared.finish_side_download(first, true);
        let guard = shared.state.lock().expect("state locks");
        assert_eq!(guard.side_download_active, vec![second]);
    }

    #[test]
    fn http_disk_cache_unlinks_immediately_but_keeps_open_file_usable() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let mut disk_cache = HttpDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::Immediate,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();

        assert!(!path.exists());
        disk_cache.write_at(0, b"payload").expect("payload writes");
        let mut restored = [0; 7];

        assert_eq!(disk_cache.read_at(0, &mut restored), Some(7));
        assert_eq!(&restored, b"payload");
    }

    #[test]
    fn http_disk_cache_prunes_least_recently_used_range() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let mut disk_cache = HttpDiskCache::new(
            8,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::WhenDone,
        )
        .expect("disk cache creates");
        disk_cache.write_at(0, b"aaaa").expect("first range writes");
        disk_cache
            .write_at(10, b"bbbb")
            .expect("second range writes");
        let mut restored = [0; 1];
        assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));

        disk_cache
            .write_at(20, b"cccc")
            .expect("third range writes");

        assert!(disk_cache.read_at(10, &mut restored).is_none());
        assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));
        assert_eq!(restored[0], b'a');
        assert_eq!(disk_cache.read_at(20, &mut restored), Some(1));
        assert_eq!(restored[0], b'c');
    }

    #[test]
    fn http_disk_cache_removes_file_when_done() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let path = {
            let disk_cache = HttpDiskCache::new(
                1024,
                Some(dir.path().to_path_buf()),
                CacheUnlinkPolicy::WhenDone,
            )
            .expect("disk cache creates");
            let path = disk_cache.path.clone();
            assert!(path.exists());
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn http_disk_cache_can_leave_file_for_inspection() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let path = {
            let disk_cache = HttpDiskCache::new(
                1024,
                Some(dir.path().to_path_buf()),
                CacheUnlinkPolicy::Never,
            )
            .expect("disk cache creates");
            let path = disk_cache.path.clone();
            assert!(path.exists());
            path
        };

        assert!(path.exists());
        std::fs::remove_file(path).expect("leftover cache file removes");
    }
}
