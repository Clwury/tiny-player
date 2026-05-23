use super::{download::http_ring_cache_download_loop, http::reqwest_header_pairs, *};

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
    retained_range: Option<RetainedCacheRange>,
    disk_cache: Option<HttpDiskCache>,
    config: HttpCacheConfig,
    active_range_kind: HttpCacheRangeKind,
    pending_seek_range_kind: Option<(u64, HttpCacheRangeKind)>,
    reader_offset: u64,
    duration_seconds: Option<f64>,
    prefetch_paused: bool,
    content_len: Option<u64>,
    pub(in crate::player::backend::ffmpeg) eof: bool,
    shutdown: bool,
    restart_request: Option<CacheRestartRequest>,
    error: Option<String>,
    last_reported_stream_buffer: Option<HttpStreamBufferProgress>,
    last_reported_status: Option<HttpStreamCacheStatus>,
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

#[derive(Clone, Copy, Debug)]
struct CacheRestartRequest {
    offset: u64,
    range_kind: HttpCacheRangeKind,
}

struct RetainedCacheRange {
    buffer: ByteRingBuffer,
    base_offset: u64,
    next_offset: u64,
}

#[derive(Clone, Copy)]
struct HttpCacheConfig {
    memory_capacity: usize,
    chunk_size: usize,
    readahead_seconds: f64,
    hysteresis_seconds: f64,
    max_readahead_bytes: Option<u64>,
    disk_cache_bytes: Option<u64>,
}

struct HttpDiskCache {
    file: File,
    path: PathBuf,
    ranges: Vec<HttpCachedByteRange>,
    max_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HttpCachedByteRange {
    start: u64,
    end: u64,
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
    fn from_env() -> Self {
        Self {
            memory_capacity: env_usize("TINY_HTTP_CACHE_MEMORY_BYTES", HTTP_RING_CACHE_CAPACITY)
                .max(HTTP_CACHE_CHUNK_SIZE),
            chunk_size: env_usize("TINY_HTTP_CACHE_CHUNK_BYTES", HTTP_CACHE_CHUNK_SIZE)
                .clamp(64 * 1024, 16 * 1024 * 1024),
            readahead_seconds: env_f64(
                "TINY_HTTP_CACHE_READAHEAD_SECS",
                HTTP_CACHE_DEFAULT_READAHEAD_SECONDS,
            )
            .max(1.0),
            hysteresis_seconds: env_f64(
                "TINY_HTTP_CACHE_HYSTERESIS_SECS",
                HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS,
            )
            .max(0.0),
            max_readahead_bytes: env_u64("TINY_HTTP_CACHE_MAX_BYTES"),
            disk_cache_bytes: None,
        }
    }

    #[cfg(test)]
    fn for_test(memory_capacity: usize) -> Self {
        Self {
            memory_capacity,
            chunk_size: HTTP_CACHE_CHUNK_SIZE.min(memory_capacity.max(1)),
            readahead_seconds: HTTP_CACHE_DEFAULT_READAHEAD_SECONDS,
            hysteresis_seconds: HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS,
            max_readahead_bytes: None,
            disk_cache_bytes: None,
        }
    }
}

impl HttpDiskCache {
    fn new(max_bytes: u64) -> Option<Self> {
        let dir = env::var("TINY_HTTP_CACHE_DIR")
            .ok()
            .map(PathBuf::from)
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

        Some(Self {
            file,
            path,
            ranges: Vec::new(),
            max_bytes,
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

    fn read_at(&self, offset: u64, output: &mut [u8]) -> Option<usize> {
        let range = self.range_containing(offset)?;
        let len = output.len().min(usize::try_from(range.end - offset).ok()?);
        if len == 0 {
            return None;
        }
        self.file
            .read_at(&mut output[..len], offset)
            .ok()
            .filter(|read| *read > 0)
    }

    fn add_range(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        self.ranges.push(HttpCachedByteRange { start, end });
        self.ranges.sort_by_key(|range| range.start);

        let mut merged: Vec<HttpCachedByteRange> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
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
            let Some(first) = self.ranges.first_mut() else {
                break;
            };
            let overflow = cached_bytes.saturating_sub(self.max_bytes);
            let trim = overflow.min(first.end.saturating_sub(first.start));
            first.start = first.start.saturating_add(trim);
            if first.start >= first.end {
                self.ranges.remove(0);
            }
        }
    }

    fn cached_bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start))
            .sum()
    }

    fn range_containing(&self, offset: u64) -> Option<HttpCachedByteRange> {
        self.ranges
            .iter()
            .copied()
            .find(|range| offset >= range.start && offset < range.end)
    }
}

impl Drop for HttpDiskCache {
    fn drop(&mut self) {
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
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let headers = reqwest_header_pairs(http_headers)?;
        let config = HttpCacheConfig::from_env();
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
        thread::Builder::new()
            .name("tiny-http-stream-cache".to_string())
            .spawn(move || http_ring_cache_download_loop(worker_shared, url, headers))
            .map_err(|error| format!("启动 HTTP 视频缓存线程失败：{error}"))?;

        Ok(Self { shared })
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
                    "HTTP stream cache read requesting range restart"
                );
                guard.request_restart_at(current_offset);
                self.shared.ready.notify_all();
            }
            if guard.eof && current_offset >= guard.next_offset {
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
            if guard.eof && offset >= guard.next_offset {
                return CacheReadResult::Eof;
            }
            if offset < guard.base_offset {
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
        Self {
            buffer: ByteRingBuffer::new(config.memory_capacity),
            base_offset: start_offset,
            next_offset: start_offset,
            retained_range: None,
            disk_cache: config.disk_cache_bytes.and_then(HttpDiskCache::new),
            config,
            active_range_kind: HttpCacheRangeKind::Playback,
            pending_seek_range_kind: None,
            reader_offset: start_offset,
            duration_seconds: None,
            prefetch_paused: false,
            content_len: None,
            eof: false,
            shutdown: false,
            restart_request: None,
            error: None,
            last_reported_stream_buffer: None,
            last_reported_status: None,
        }
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
        self.retain_current_range_for_tail_restart(offset, range_kind);
        self.buffer.clear();
        self.base_offset = offset;
        self.next_offset = offset;
        self.active_range_kind = range_kind;
        self.pending_seek_range_kind = None;
        self.reader_offset = offset;
        self.eof = false;
        self.error = None;
        self.last_reported_stream_buffer = None;
        self.last_reported_status = None;
    }

    fn retain_current_range_for_tail_restart(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        if range_kind != HttpCacheRangeKind::TailMetadataProbe
            || self.active_range_kind != HttpCacheRangeKind::Playback
        {
            return;
        }
        if self.buffer.len() == 0 || (offset >= self.base_offset && offset <= self.next_offset) {
            return;
        }
        if !self.content_len.is_some_and(|content_len| {
            offset < content_len
                && offset >= content_len.saturating_sub(HTTP_CACHE_RANGE_REQUEST_BYTES)
        }) {
            return;
        }

        tracing::debug!(
            base_offset = self.base_offset,
            next_offset = self.next_offset,
            restart_offset = offset,
            "retaining HTTP stream cache range across tail metadata read"
        );
        let capacity = self.buffer.max_capacity();
        let buffer = std::mem::replace(&mut self.buffer, ByteRingBuffer::new(capacity));
        self.retained_range = Some(RetainedCacheRange {
            buffer,
            base_offset: self.base_offset,
            next_offset: self.next_offset,
        });
    }

    fn request_restart_at(&mut self, offset: u64) {
        let range_kind = self.take_pending_seek_range_kind(offset);
        self.restart_at_with_kind(offset, range_kind);
        self.restart_request = Some(CacheRestartRequest { offset, range_kind });
    }

    fn take_pending_seek_range_kind(&mut self, offset: u64) -> HttpCacheRangeKind {
        let Some((pending_offset, range_kind)) = self.pending_seek_range_kind else {
            return HttpCacheRangeKind::Playback;
        };
        self.pending_seek_range_kind = None;
        if pending_offset == offset {
            range_kind
        } else {
            HttpCacheRangeKind::Playback
        }
    }

    pub(in crate::player::backend::ffmpeg) fn append_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let mut offset = offset;
        let mut data = data;
        if let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = disk_cache.write_at(offset, data)
        {
            tracing::warn!(%error, "disabling HTTP disk cache after write failure");
            self.disk_cache = None;
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
        self.trim_to_capacity(self.config.memory_capacity);
        true
    }

    pub(in crate::player::backend::ffmpeg) fn trim_to_capacity(&mut self, capacity: usize) {
        let overflow = self.buffer.len().saturating_sub(capacity);
        if overflow == 0 {
            return;
        }
        let consumed = self
            .reader_offset
            .saturating_sub(self.base_offset)
            .min(self.buffer.len() as u64) as usize;
        let trim = overflow.min(consumed);
        if trim == 0 {
            return;
        }
        self.buffer.discard_front(trim);
        self.base_offset = self.base_offset.saturating_add(trim as u64);
    }

    pub(in crate::player::backend::ffmpeg) fn copy_available(
        &self,
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
        self.retained_range
            .as_ref()
            .and_then(|range| {
                copy_available_from_range(
                    &range.buffer,
                    range.base_offset,
                    range.next_offset,
                    offset,
                    output,
                )
            })
            .or_else(|| {
                self.disk_cache
                    .as_ref()
                    .and_then(|disk_cache| disk_cache.read_at(offset, output))
            })
    }

    pub(in crate::player::backend::ffmpeg) fn set_reader_offset(&mut self, offset: u64) {
        self.reader_offset = offset;
        self.trim_to_capacity(self.config.memory_capacity);
    }

    pub(in crate::player::backend::ffmpeg) fn note_seek_offset(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        self.pending_seek_range_kind = Some((offset, range_kind));
        if range_kind == HttpCacheRangeKind::Playback
            && offset >= self.base_offset
            && offset <= self.next_offset
        {
            self.set_reader_offset(offset);
        }
    }

    pub(in crate::player::backend::ffmpeg) fn is_tail_metadata_probe_seek(
        &self,
        offset: u64,
    ) -> bool {
        let Some(content_len) = self.content_len else {
            return false;
        };
        if offset >= content_len
            || offset < content_len.saturating_sub(HTTP_CACHE_RANGE_REQUEST_BYTES)
        {
            return false;
        }

        let active_range_near_offset = self.active_range_kind == HttpCacheRangeKind::Playback
            && self.buffer.len() > 0
            && offset
                <= self
                    .next_offset
                    .saturating_add(HTTP_CACHE_RANGE_REQUEST_BYTES)
            && self.base_offset <= offset.saturating_add(HTTP_CACHE_RANGE_REQUEST_BYTES);
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

    pub(in crate::player::backend::ffmpeg) fn stream_buffer_progress(
        &self,
    ) -> Option<HttpStreamBufferProgress> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        let mut start_offset = None;
        let mut end_offset = None;
        if let Some(range) = &self.retained_range {
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
        Some(HttpStreamBufferProgress {
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
                let range = self.retained_range.as_ref()?;
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
            }
        }
    }

    fn take_stream_buffer_progress_report(&mut self) -> Option<HttpStreamBufferProgress> {
        let progress = self.stream_buffer_progress()?;
        if !http_stream_buffer_progress_changed(self.last_reported_stream_buffer, progress) {
            return None;
        }

        self.last_reported_stream_buffer = Some(progress);
        Some(progress)
    }

    fn stream_cache_status(&self) -> HttpStreamCacheStatus {
        let content_len = self.content_len;
        let ranges = self.stream_buffer_ranges();
        let reader_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.reader_offset.min(content_len) as f64 / content_len as f64);
        let download_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.next_offset.min(content_len) as f64 / content_len as f64);
        HttpStreamCacheStatus {
            ranges: ranges.into_iter().map(Into::into).collect(),
            reader_fraction,
            download_fraction,
            cached_bytes: self.cached_bytes(),
            content_length: content_len,
            disk_cache_enabled: self.disk_cache.is_some(),
            idle: self.prefetch_paused,
        }
    }

    fn stream_buffer_ranges(&self) -> Vec<HttpPlaybackBufferRange> {
        let Some(content_len) = self.content_len.filter(|content_len| *content_len > 0) else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        if let Some(range) = &self.retained_range
            && let Some(range) =
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
        {
            ranges.push(range);
        }
        if self.active_range_kind == HttpCacheRangeKind::Playback
            && let Some(range) =
                playback_buffer_range(self.base_offset, self.next_offset, content_len)
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
        let memory_bytes = self.buffer.len() as u64
            + self
                .retained_range
                .as_ref()
                .map(|range| range.buffer.len() as u64)
                .unwrap_or(0);
        memory_bytes.saturating_add(
            self.disk_cache
                .as_ref()
                .map(HttpDiskCache::cached_bytes)
                .unwrap_or(0),
        )
    }

    fn take_stream_cache_status_report(&mut self) -> Option<HttpStreamCacheStatus> {
        let status = self.stream_cache_status();
        if !http_stream_cache_status_changed(self.last_reported_status.as_ref(), &status) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}

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

impl From<HttpPlaybackBufferRange> for HttpStreamBufferProgress {
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
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.eof = true;
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
                BackendEventKind::HttpStreamCacheStatusChanged(status),
            ));
        }
        permit
    }

    fn send_stream_cache_status(&self, status: Option<HttpStreamCacheStatus>) {
        if let Some(status) = status {
            let _ = self.event_tx.send(BackendEvent::new(
                self.control.session_id(),
                BackendEventKind::HttpStreamCacheStatusChanged(status),
            ));
        }
    }

    fn send_stream_buffer_progress(&self, progress: Option<HttpStreamBufferProgress>) {
        if let Some(progress) = progress {
            let _ = self.event_tx.send(BackendEvent::new(
                self.control.session_id(),
                BackendEventKind::HttpStreamBufferedChanged(Some(progress)),
            ));
        }
    }

    fn send_cache_events(
        &self,
        progress: Option<HttpStreamBufferProgress>,
        status: Option<HttpStreamCacheStatus>,
    ) {
        self.send_stream_buffer_progress(progress);
        self.send_stream_cache_status(status);
    }

    pub(super) fn append_or_restart(&self, offset: u64, data: &[u8]) -> CacheAppendResult {
        let (result, progress, status) = {
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
                guard.take_stream_buffer_progress_report(),
                guard.take_stream_cache_status_report(),
            )
        };
        self.ready.notify_all();
        self.send_cache_events(progress, status);
        result
    }
}

fn http_stream_buffer_progress_changed(
    previous: Option<HttpStreamBufferProgress>,
    next: HttpStreamBufferProgress,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    (previous.start_fraction - next.start_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (previous.end_fraction - next.end_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (next.end_fraction >= 1.0 && previous.end_fraction < 1.0)
}

fn http_stream_cache_status_changed(
    previous: Option<&HttpStreamCacheStatus>,
    next: &HttpStreamCacheStatus,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.disk_cache_enabled != next.disk_cache_enabled
        || previous.idle != next.idle
        || previous.content_length != next.content_length
        || previous.ranges.len() != next.ranges.len()
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
    previous.cached_bytes.abs_diff(next.cached_bytes) >= HTTP_CACHE_RANGE_REQUEST_BYTES
        || previous
            .ranges
            .iter()
            .zip(next.ranges.iter())
            .any(|(previous, next)| http_stream_buffer_progress_changed(Some(*previous), *next))
}
