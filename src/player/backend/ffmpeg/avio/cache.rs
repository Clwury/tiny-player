use super::{download::http_ring_cache_download_loop, http::reqwest_header_pairs, *};

#[derive(Clone)]
pub(super) struct HttpRingCache {
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
    active_range_kind: HttpCacheRangeKind,
    pending_seek_range_kind: Option<(u64, HttpCacheRangeKind)>,
    reader_offset: u64,
    content_len: Option<u64>,
    pub(in crate::player::backend::ffmpeg) eof: bool,
    shutdown: bool,
    restart_request: Option<CacheRestartRequest>,
    error: Option<String>,
    last_reported_stream_buffer: Option<HttpStreamBufferProgress>,
}

pub(super) enum CacheReadResult {
    Data(usize),
    Eof,
    Interrupted,
    Error(String),
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

impl HttpRingCache {
    pub(super) fn spawn(
        url: String,
        http_headers: &[(String, String)],
        content_len_hint: Option<u64>,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let headers = reqwest_header_pairs(http_headers)?;
        let shared = Arc::new(HttpRingCacheShared {
            state: Mutex::new(HttpRingCacheState::new(0).with_content_len_hint(content_len_hint)),
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
        loop {
            if guard.shutdown || self.shared.control.should_interrupt() {
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
                guard.set_reader_offset(offset.saturating_add(read as u64));
                self.shared.ready.notify_all();
                return CacheReadResult::Data(read);
            }
            if offset < guard.base_offset || offset > guard.next_offset {
                guard.request_restart_at(offset);
                self.shared.ready.notify_all();
            }
            if guard.eof && offset >= guard.next_offset {
                return CacheReadResult::Eof;
            }

            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
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
                || self.shared.control.should_interrupt()
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
}

impl HttpRingCacheState {
    pub(in crate::player::backend::ffmpeg) fn new(start_offset: u64) -> Self {
        Self::with_cache_capacity(start_offset, HTTP_RING_CACHE_CAPACITY)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new_with_cache_capacity(
        start_offset: u64,
        capacity: usize,
    ) -> Self {
        Self::with_cache_capacity(start_offset, capacity)
    }

    fn with_cache_capacity(start_offset: u64, capacity: usize) -> Self {
        Self {
            buffer: ByteRingBuffer::new(capacity),
            base_offset: start_offset,
            next_offset: start_offset,
            retained_range: None,
            active_range_kind: HttpCacheRangeKind::Playback,
            pending_seek_range_kind: None,
            reader_offset: start_offset,
            content_len: None,
            eof: false,
            shutdown: false,
            restart_request: None,
            error: None,
            last_reported_stream_buffer: None,
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
    pub(in crate::player::backend::ffmpeg) fn restart_at(&mut self, offset: u64) {
        self.restart_at_with_kind(offset, HttpCacheRangeKind::Playback);
    }

    pub(in crate::player::backend::ffmpeg) fn restart_at_with_kind(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
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
        self.trim_to_capacity(HTTP_RING_CACHE_CAPACITY);
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
        self.retained_range.as_ref().and_then(|range| {
            copy_available_from_range(
                &range.buffer,
                range.base_offset,
                range.next_offset,
                offset,
                output,
            )
        })
    }

    pub(in crate::player::backend::ffmpeg) fn set_reader_offset(&mut self, offset: u64) {
        self.reader_offset = offset;
        self.trim_to_capacity(HTTP_RING_CACHE_CAPACITY);
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

    pub(in crate::player::backend::ffmpeg) fn append_capacity_from(
        &mut self,
        offset: u64,
    ) -> usize {
        self.trim_to_capacity(HTTP_RING_CACHE_CAPACITY);
        let buffered_ahead = offset.saturating_sub(self.reader_offset);
        let buffered_ahead = usize::try_from(buffered_ahead).unwrap_or(usize::MAX);
        HTTP_RING_CACHE_CAPACITY.saturating_sub(buffered_ahead)
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

    fn take_stream_buffer_progress_report(&mut self) -> Option<HttpStreamBufferProgress> {
        let progress = self.stream_buffer_progress()?;
        if !http_stream_buffer_progress_changed(self.last_reported_stream_buffer, progress) {
            return None;
        }

        self.last_reported_stream_buffer = Some(progress);
        Some(progress)
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

    pub(super) fn wait_for_append_capacity(&self, offset: u64) -> CacheAppendPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
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
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
        }
    }

    pub(super) fn append_capacity_now(&self, offset: u64) -> CacheAppendPermit {
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
        if capacity > 0 {
            CacheAppendPermit::Ready(capacity)
        } else {
            CacheAppendPermit::Full
        }
    }

    pub(super) fn append_or_restart(&self, offset: u64, data: &[u8]) -> CacheAppendResult {
        let (result, progress) = {
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
            )
        };
        self.ready.notify_all();
        if let Some(progress) = progress {
            let _ = self.event_tx.send(BackendEvent::new(
                self.control.session_id(),
                BackendEventKind::HttpStreamBufferedChanged(Some(progress)),
            ));
        }
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
