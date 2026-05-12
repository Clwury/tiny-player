use super::*;

pub(super) fn input_format_options(
    http_headers: &[(String, String)],
    probe_profile: InputProbeProfile,
) -> std::result::Result<*mut ffi::AVDictionary, String> {
    let mut options = ptr::null_mut();
    if let InputProbeProfile::Fast = probe_profile {
        if let Err(error) = set_input_format_option(
            &mut options,
            "probesize",
            &FFMPEG_FAST_PROBE_SIZE.to_string(),
        ) {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err(error);
        }
        if let Err(error) = set_input_format_option(
            &mut options,
            "analyzeduration",
            &FFMPEG_FAST_ANALYZE_DURATION_US.to_string(),
        ) {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err(error);
        }
    }

    if !http_headers.is_empty() {
        let headers = match ffmpeg_http_headers(http_headers) {
            Ok(headers) => headers,
            Err(error) => {
                unsafe { ffi::av_dict_free(&mut options) };
                return Err(error);
            }
        };
        if let Err(error) = set_input_format_option(&mut options, "headers", &headers) {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err(error);
        }
        if let Some((_, user_agent)) = http_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
            && let Err(error) = set_input_format_option(&mut options, "user_agent", user_agent)
        {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err(error);
        }
    }

    Ok(options)
}

fn set_input_format_option(
    options: &mut *mut ffi::AVDictionary,
    name: &str,
    value: &str,
) -> std::result::Result<(), String> {
    let name = CString::new(name).map_err(|_| "FFmpeg 输入选项名称包含无效字符".to_string())?;
    let value = CString::new(value).map_err(|_| "FFmpeg 输入选项值包含无效字符".to_string())?;
    let result = unsafe { ffi::av_dict_set(options, name.as_ptr(), value.as_ptr(), 0) };
    if result < 0 {
        return Err(format!("FFmpeg 设置输入选项失败：{}", ffmpeg_error(result)));
    }
    Ok(())
}

pub(super) fn ffmpeg_http_headers(
    headers: &[(String, String)],
) -> std::result::Result<String, String> {
    let mut output = String::new();
    for (name, value) in headers {
        let name = name.trim();
        let value = value.trim();
        if name.is_empty()
            || name
                .chars()
                .any(|character| matches!(character, ':' | '\r' | '\n' | '\0'))
        {
            return Err("HTTP 请求头名称无效".to_string());
        }
        if value
            .chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            return Err("HTTP 请求头值无效".to_string());
        }
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
        output.push_str("\r\n");
    }
    Ok(output)
}

pub(super) fn should_cache_http_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("http://") || url.starts_with("https://")
}

pub(super) struct CachedAvio {
    ptr: *mut ffi::AVIOContext,
    reader: *mut CachedAvioReader,
    cache: HttpRingCache,
}

impl CachedAvio {
    pub(super) fn new(
        url: &str,
        http_headers: &[(String, String)],
        content_len_hint: Option<u64>,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let cache = HttpRingCache::spawn(
            url.to_string(),
            http_headers,
            content_len_hint,
            control,
            event_tx,
        )?;
        let reader = Box::into_raw(Box::new(CachedAvioReader {
            cache: cache.clone(),
            read_pos: 0,
        }));
        let buffer = unsafe { ffi::av_malloc(FFMPEG_AVIO_BUFFER_SIZE as usize) }.cast::<u8>();
        if buffer.is_null() {
            unsafe { drop(Box::from_raw(reader)) };
            return Err("FFmpeg 分配缓存 AVIO 缓冲区失败".to_string());
        }

        let ptr = unsafe {
            ffi::avio_alloc_context(
                buffer,
                FFMPEG_AVIO_BUFFER_SIZE,
                0,
                reader.cast::<c_void>(),
                Some(cached_avio_read_packet),
                None,
                Some(cached_avio_seek),
            )
        };
        if ptr.is_null() {
            unsafe {
                ffi::av_free(buffer.cast::<c_void>());
                drop(Box::from_raw(reader));
            }
            return Err("FFmpeg 创建缓存 AVIO 上下文失败".to_string());
        }
        unsafe {
            (*ptr).seekable = ffi::AVIO_SEEKABLE_NORMAL;
        }

        Ok(Self { ptr, reader, cache })
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.ptr
    }
}

impl Drop for CachedAvio {
    fn drop(&mut self) {
        self.cache.shutdown();
        if !self.ptr.is_null() {
            let mut ptr = self.ptr;
            unsafe { ffi::avio_context_free(&mut ptr) };
            self.ptr = ptr;
        }
        if !self.reader.is_null() {
            unsafe { drop(Box::from_raw(self.reader)) };
            self.reader = ptr::null_mut();
        }
    }
}

struct CachedAvioReader {
    cache: HttpRingCache,
    read_pos: u64,
}

#[derive(Clone)]
struct HttpRingCache {
    shared: Arc<HttpRingCacheShared>,
}

struct HttpRingCacheShared {
    state: Mutex<HttpRingCacheState>,
    ready: Condvar,
    control: Arc<FfmpegControl>,
    event_tx: Sender<BackendEvent>,
}

pub(super) struct HttpRingCacheState {
    buffer: ByteRingBuffer,
    pub(super) base_offset: u64,
    pub(super) next_offset: u64,
    reader_offset: u64,
    content_len: Option<u64>,
    pub(super) eof: bool,
    shutdown: bool,
    restart_offset: Option<u64>,
    error: Option<String>,
    last_reported_stream_buffer: Option<HttpStreamBufferProgress>,
}

enum CacheReadResult {
    Data(usize),
    Eof,
    Interrupted,
    Error(String),
}

enum CacheAppendPermit {
    Ready(usize),
    Full,
    Restart(u64),
    Stopped,
}

enum CacheAppendResult {
    Appended,
    Restart(u64),
    Stopped,
}

enum HttpDownloadOutcome {
    Eof,
    Restart(u64),
    Stopped,
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
    fn spawn(
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

    fn read_at(&self, offset: u64, output: &mut [u8]) -> CacheReadResult {
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

    fn note_reader_offset(&self, offset: u64) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("HTTP stream cache poisoned");
        guard.note_seek_offset(offset);
        self.shared.ready.notify_all();
    }

    fn content_len(&self) -> Option<u64> {
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

    fn shutdown(&self) {
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
    pub(super) fn new(start_offset: u64) -> Self {
        Self::with_cache_capacity(start_offset, HTTP_RING_CACHE_CAPACITY)
    }

    #[cfg(test)]
    pub(super) fn new_with_cache_capacity(start_offset: u64, capacity: usize) -> Self {
        Self::with_cache_capacity(start_offset, capacity)
    }

    fn with_cache_capacity(start_offset: u64, capacity: usize) -> Self {
        Self {
            buffer: ByteRingBuffer::new(capacity),
            base_offset: start_offset,
            next_offset: start_offset,
            reader_offset: start_offset,
            content_len: None,
            eof: false,
            shutdown: false,
            restart_offset: None,
            error: None,
            last_reported_stream_buffer: None,
        }
    }

    pub(super) fn with_content_len_hint(mut self, content_len_hint: Option<u64>) -> Self {
        self.content_len = content_len_hint.filter(|content_len| *content_len > 0);
        self
    }

    pub(super) fn restart_at(&mut self, offset: u64) {
        self.buffer.clear();
        self.base_offset = offset;
        self.next_offset = offset;
        self.reader_offset = offset;
        self.eof = false;
        self.error = None;
        self.last_reported_stream_buffer = None;
    }

    fn request_restart_at(&mut self, offset: u64) {
        self.restart_at(offset);
        self.restart_offset = Some(offset);
    }

    pub(super) fn append_at(&mut self, offset: u64, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        let mut offset = offset;
        let mut data = data;
        if offset != self.next_offset {
            self.restart_at(offset);
        }

        let max_capacity = self.buffer.max_capacity();
        if data.len() > max_capacity {
            let trim = data.len() - max_capacity;
            offset = offset.saturating_add(trim as u64);
            data = &data[trim..];
            self.restart_at(offset);
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

    pub(super) fn trim_to_capacity(&mut self, capacity: usize) {
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

    pub(super) fn copy_available(&self, offset: u64, output: &mut [u8]) -> Option<usize> {
        if offset < self.base_offset || offset >= self.next_offset {
            return None;
        }
        let start = usize::try_from(offset - self.base_offset).ok()?;
        if start >= self.buffer.len() {
            return None;
        }
        let read = self.buffer.copy_at(start, output);
        (read > 0).then_some(read)
    }

    pub(super) fn set_reader_offset(&mut self, offset: u64) {
        self.reader_offset = offset;
        self.trim_to_capacity(HTTP_RING_CACHE_CAPACITY);
    }

    pub(super) fn note_seek_offset(&mut self, offset: u64) {
        if offset >= self.base_offset && offset <= self.next_offset {
            self.set_reader_offset(offset);
        }
    }

    pub(super) fn append_capacity_from(&mut self, offset: u64) -> usize {
        self.trim_to_capacity(HTTP_RING_CACHE_CAPACITY);
        let buffered_ahead = offset.saturating_sub(self.reader_offset);
        let buffered_ahead = usize::try_from(buffered_ahead).unwrap_or(usize::MAX);
        HTTP_RING_CACHE_CAPACITY.saturating_sub(buffered_ahead)
    }

    pub(super) fn stream_buffer_progress(&self) -> Option<HttpStreamBufferProgress> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        let start_offset = self.base_offset.min(content_len);
        let end_offset = self.next_offset.min(content_len).max(start_offset);
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

fn http_ring_cache_download_loop(
    shared: Arc<HttpRingCacheShared>,
    url: String,
    headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
) {
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            shared.set_error(format!("创建 HTTP 视频缓存客户端失败：{error}"));
            return;
        }
    };

    let mut offset = 0;
    loop {
        if shared.should_stop() {
            return;
        }
        if let Some(next_offset) = shared.take_restart_offset() {
            offset = next_offset;
        }
        match shared.wait_for_append_capacity(offset) {
            CacheAppendPermit::Ready(_) => {}
            CacheAppendPermit::Full => continue,
            CacheAppendPermit::Restart(next_offset) => {
                offset = next_offset;
                continue;
            }
            CacheAppendPermit::Stopped => return,
        }

        match download_http_cache_range(&client, &url, &headers, Arc::clone(&shared), offset) {
            Ok(HttpDownloadOutcome::Eof) => {
                shared.mark_eof();
                match shared.wait_for_restart_after_eof() {
                    Some(next_offset) => offset = next_offset,
                    None => return,
                }
            }
            Ok(HttpDownloadOutcome::Restart(next_offset)) => {
                offset = next_offset;
            }
            Ok(HttpDownloadOutcome::Stopped) => return,
            Err(error) => {
                if shared.should_stop() {
                    return;
                }
                if let Some(next_offset) = shared.take_restart_offset() {
                    offset = next_offset;
                    continue;
                }
                shared.set_error(error);
                return;
            }
        }
    }
}

impl HttpRingCacheShared {
    fn should_stop(&self) -> bool {
        self.control.should_stop()
            || self
                .state
                .lock()
                .expect("HTTP stream cache poisoned")
                .shutdown
    }

    fn take_restart_offset(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .restart_offset
            .take()
    }

    fn set_error(&self, error: String) {
        tracing::warn!(%error, "HTTP video stream cache failed");
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.error = Some(error);
        self.ready.notify_all();
    }

    fn mark_eof(&self) {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        guard.eof = true;
        self.ready.notify_all();
    }

    fn wait_for_restart_after_eof(&self) -> Option<u64> {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return None;
            }
            if let Some(next_offset) = guard.restart_offset.take() {
                guard.restart_at(next_offset);
                self.ready.notify_all();
                return Some(next_offset);
            }
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, HTTP_CACHE_WAIT_INTERVAL)
                .expect("HTTP stream cache poisoned");
            guard = next_guard;
        }
    }

    fn set_content_len(&self, content_len: Option<u64>) {
        if let Some(content_len) = content_len {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            guard.content_len = Some(content_len);
            self.ready.notify_all();
        }
    }

    fn content_len_now(&self) -> Option<u64> {
        self.state
            .lock()
            .expect("HTTP stream cache poisoned")
            .content_len
    }

    fn wait_for_append_capacity(&self, offset: u64) -> CacheAppendPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        loop {
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendPermit::Stopped;
            }
            if let Some(next_offset) = guard.restart_offset.take() {
                guard.restart_at(next_offset);
                self.ready.notify_all();
                return CacheAppendPermit::Restart(next_offset);
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

    fn append_capacity_now(&self, offset: u64) -> CacheAppendPermit {
        let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
        if guard.shutdown || self.control.should_stop() {
            return CacheAppendPermit::Stopped;
        }
        if let Some(next_offset) = guard.restart_offset.take() {
            guard.restart_at(next_offset);
            self.ready.notify_all();
            return CacheAppendPermit::Restart(next_offset);
        }
        let capacity = guard.append_capacity_from(offset);
        if capacity > 0 {
            CacheAppendPermit::Ready(capacity)
        } else {
            CacheAppendPermit::Full
        }
    }

    fn append_or_restart(&self, offset: u64, data: &[u8]) -> CacheAppendResult {
        let (result, progress) = {
            let mut guard = self.state.lock().expect("HTTP stream cache poisoned");
            if guard.shutdown || self.control.should_stop() {
                return CacheAppendResult::Stopped;
            }
            if let Some(next_offset) = guard.restart_offset.take() {
                guard.restart_at(next_offset);
                self.ready.notify_all();
                return CacheAppendResult::Restart(next_offset);
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

fn download_http_cache_range(
    client: &reqwest::blocking::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
    shared: Arc<HttpRingCacheShared>,
    mut offset: u64,
) -> std::result::Result<HttpDownloadOutcome, String> {
    let known_content_len = shared.content_len_now();
    if known_content_len.is_some_and(|content_len| offset >= content_len) {
        return Ok(HttpDownloadOutcome::Eof);
    }

    let range = http_cache_range_header(offset, known_content_len);
    let range_len = http_cache_range_request_len(offset, known_content_len);
    let request_timeout = http_cache_range_request_timeout(range_len);
    let mut request = client
        .get(url)
        .timeout(request_timeout)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::CONNECTION, "keep-alive")
        .header(reqwest::header::RANGE, range.as_str());
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let mut response = match request.send() {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Ok(HttpDownloadOutcome::Restart(offset));
        }
        Err(error) => return Err(format!("HTTP 视频缓存请求失败：{error}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        shared.set_content_len(content_len_from_content_range(response.headers()));
        return Ok(HttpDownloadOutcome::Eof);
    }
    if offset > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!("HTTP 视频缓存 Range 请求失败：服务器返回 {status}"));
    }
    if offset == 0
        && status != reqwest::StatusCode::OK
        && status != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(format!("HTTP 视频缓存请求失败：服务器返回 {status}"));
    }
    let content_len = content_len_from_response(&response, offset);
    shared.set_content_len(content_len);

    let mut chunk = vec![0; HTTP_CACHE_CHUNK_SIZE];
    loop {
        if shared.should_stop() {
            return Ok(HttpDownloadOutcome::Stopped);
        }
        let capacity = match shared.append_capacity_now(offset) {
            CacheAppendPermit::Ready(capacity) => capacity,
            CacheAppendPermit::Full => return Ok(HttpDownloadOutcome::Restart(offset)),
            CacheAppendPermit::Restart(next_offset) => {
                return Ok(HttpDownloadOutcome::Restart(next_offset));
            }
            CacheAppendPermit::Stopped => return Ok(HttpDownloadOutcome::Stopped),
        };
        let read_capacity = chunk.len().min(capacity);
        let read = match response.read(&mut chunk[..read_capacity]) {
            Ok(read) => read,
            Err(error) if http_cache_read_timed_out(&error) => {
                if let Some(next_offset) = shared.take_restart_offset() {
                    return Ok(HttpDownloadOutcome::Restart(next_offset));
                }
                return Ok(HttpDownloadOutcome::Restart(offset));
            }
            Err(error) => {
                return Err(format!("读取 HTTP 视频缓存失败：{error}"));
            }
        };
        if read == 0 {
            if content_len.is_some_and(|content_len| offset < content_len) {
                return Ok(HttpDownloadOutcome::Restart(offset));
            }
            return Ok(HttpDownloadOutcome::Eof);
        }
        match shared.append_or_restart(offset, &chunk[..read]) {
            CacheAppendResult::Appended => {
                offset = offset.saturating_add(read as u64);
            }
            CacheAppendResult::Restart(next_offset) => {
                return Ok(HttpDownloadOutcome::Restart(next_offset));
            }
            CacheAppendResult::Stopped => return Ok(HttpDownloadOutcome::Stopped),
        }
    }
}

pub(super) fn reqwest_header_pairs(
    headers: &[(String, String)],
) -> std::result::Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = reqwest::header::HeaderName::from_bytes(name.trim().as_bytes())
                .map_err(|_| "HTTP 请求头名称无效".to_string())?;
            let value = reqwest::header::HeaderValue::from_str(value.trim())
                .map_err(|_| "HTTP 请求头值无效".to_string())?;
            Ok((name, value))
        })
        .collect()
}

#[cfg(test)]
pub(super) fn http_cache_request_headers_for_log(
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
    range: &str,
) -> Vec<String> {
    let mut output = vec![
        "accept-encoding: identity".to_string(),
        "connection: keep-alive".to_string(),
        format!("range: {range}"),
    ];
    output.extend(headers.iter().map(|(name, value)| {
        let value = value.to_str().unwrap_or("<non-utf8>");
        format!("{name}: {value}")
    }));
    output
}

#[cfg(test)]
pub(super) fn http_cache_response_headers_for_log(
    headers: &reqwest::header::HeaderMap,
) -> Vec<String> {
    let interesting = [
        reqwest::header::ACCEPT_RANGES,
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::CONTENT_RANGE,
        reqwest::header::CONTENT_TYPE,
        reqwest::header::SERVER,
        reqwest::header::HeaderName::from_static("cf-cache-status"),
        reqwest::header::HeaderName::from_static("via"),
    ];
    let mut output: Vec<_> = interesting
        .into_iter()
        .filter_map(|name| {
            let value = headers.get(&name)?.to_str().unwrap_or("<non-utf8>");
            Some(format!("{name}: {value}"))
        })
        .collect();
    output.sort();
    output
}

pub(super) fn http_cache_range_header(offset: u64, content_len: Option<u64>) -> String {
    let end = http_cache_range_end(offset, content_len);
    format!("bytes={offset}-{end}")
}

fn http_cache_range_end(offset: u64, content_len: Option<u64>) -> u64 {
    let requested_end = offset.saturating_add(HTTP_CACHE_RANGE_REQUEST_BYTES.saturating_sub(1));
    content_len
        .and_then(|content_len| content_len.checked_sub(1))
        .map_or(requested_end, |content_end| requested_end.min(content_end))
}

pub(super) fn http_cache_range_request_len(offset: u64, content_len: Option<u64>) -> u64 {
    http_cache_range_end(offset, content_len)
        .saturating_sub(offset)
        .saturating_add(1)
}

pub(super) fn http_cache_range_request_timeout(request_len: u64) -> Duration {
    if request_len <= HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES {
        HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT
    } else {
        HTTP_CACHE_RANGE_REQUEST_TIMEOUT
    }
}

fn http_cache_read_timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || error
        .get_ref()
        .and_then(|source| source.downcast_ref::<reqwest::Error>())
        .is_some_and(reqwest::Error::is_timeout)
}

fn content_len_from_response(response: &reqwest::blocking::Response, offset: u64) -> Option<u64> {
    content_len_from_content_range(response.headers()).or_else(|| {
        (response.status() == reqwest::StatusCode::OK)
            .then(|| {
                response
                    .content_length()
                    .map(|len| offset.saturating_add(len))
            })
            .flatten()
    })
}

pub(super) fn content_len_from_content_range(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim();
    let total = value.rsplit_once('/')?.1;
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

unsafe extern "C" fn cached_avio_read_packet(
    opaque: *mut c_void,
    buf: *mut u8,
    buf_size: c_int,
) -> c_int {
    if opaque.is_null() || buf.is_null() || buf_size <= 0 {
        return ffi::AVERROR(ffi::EINVAL);
    }
    let reader = unsafe { &mut *(opaque as *mut CachedAvioReader) };
    let output = unsafe { slice::from_raw_parts_mut(buf, buf_size as usize) };
    match reader.cache.read_at(reader.read_pos, output) {
        CacheReadResult::Data(read) => {
            reader.read_pos = reader.read_pos.saturating_add(read as u64);
            c_int::try_from(read).unwrap_or(c_int::MAX)
        }
        CacheReadResult::Eof => ffi::AVERROR_EOF,
        CacheReadResult::Interrupted => ffi::AVERROR(ffi::EIO),
        CacheReadResult::Error(error) => {
            tracing::warn!(%error, "cached FFmpeg AVIO read failed");
            ffi::AVERROR(ffi::EIO)
        }
    }
}

unsafe extern "C" fn cached_avio_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    }
    let reader = unsafe { &mut *(opaque as *mut CachedAvioReader) };
    let seek_mode = whence & !ffi::AVSEEK_FORCE;
    if seek_mode == ffi::AVSEEK_SIZE {
        return reader
            .cache
            .content_len()
            .and_then(|len| i64::try_from(len).ok())
            .unwrap_or_else(|| i64::from(ffi::AVERROR(ffi::EIO)));
    }

    let next = match seek_mode {
        value if value == ffi::SEEK_SET => Some(offset),
        value if value == ffi::SEEK_CUR => i64::try_from(reader.read_pos)
            .ok()
            .and_then(|position| position.checked_add(offset)),
        value if value == ffi::SEEK_END => reader
            .cache
            .content_len()
            .and_then(|len| i64::try_from(len).ok())
            .and_then(|len| len.checked_add(offset)),
        _ => None,
    };
    let Some(next) = next else {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    };
    if next < 0 {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    }
    let next = next as u64;
    reader.read_pos = next;
    reader.cache.note_reader_offset(next);
    i64::try_from(next).unwrap_or(i64::MAX)
}
