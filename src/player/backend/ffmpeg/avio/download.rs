use std::{io::Read, sync::Arc, time::Duration};

use super::{
    cache::{
        CacheAppendPermit, CacheAppendResult, CacheRestartRequest, CacheRetryPermit,
        HttpRingCacheShared,
    },
    http::{
        content_len_from_content_range, content_len_from_response, content_range_from_headers,
        http_cache_playback_range_request_bytes, http_cache_range_header,
        http_cache_range_request_len, http_cache_range_request_timeout, http_cache_read_timed_out,
    },
};

const HTTP_CACHE_MAX_RETRIES: u32 = 5;
const HTTP_CACHE_RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
const HTTP_CACHE_RETRY_MAX_DELAY: Duration = Duration::from_secs(2);

enum HttpDownloadOutcome {
    Eof,
    Restart(u64),
    Stopped,
}

#[derive(Debug)]
struct HttpDownloadError {
    offset: u64,
    message: String,
    retryable: bool,
}

impl HttpDownloadError {
    fn new(offset: u64, message: String, retryable: bool) -> Self {
        Self {
            offset,
            message,
            retryable,
        }
    }
}

#[derive(Default)]
struct HttpRetryState {
    offset: Option<u64>,
    retries: u32,
}

impl HttpRetryState {
    fn next_delay(&mut self, offset: u64) -> Option<(u32, Duration)> {
        if self.offset != Some(offset) {
            self.offset = Some(offset);
            self.retries = 0;
        }
        if self.retries >= HTTP_CACHE_MAX_RETRIES {
            return None;
        }
        let multiplier = 1_u32.checked_shl(self.retries).unwrap_or(u32::MAX);
        let delay = HTTP_CACHE_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(HTTP_CACHE_RETRY_MAX_DELAY);
        self.retries = self.retries.saturating_add(1);
        Some((self.retries, delay))
    }

    fn reset(&mut self) {
        self.offset = None;
        self.retries = 0;
    }
}

pub(super) fn http_ring_cache_download_loop(
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
            shared.set_error_at(0, format!("创建 HTTP 视频缓存客户端失败：{error}"));
            return;
        }
    };

    let mut offset = 0;
    let mut retry_state = HttpRetryState::default();
    loop {
        if shared.should_stop() {
            return;
        }
        if let Some(next_offset) = shared.take_restart_offset() {
            offset = next_offset;
            retry_state.reset();
        }
        match shared.wait_for_append_capacity(offset) {
            CacheAppendPermit::Ready(_) => {}
            CacheAppendPermit::Full => continue,
            CacheAppendPermit::Restart(next_offset) => {
                offset = next_offset;
                retry_state.reset();
                continue;
            }
            CacheAppendPermit::Stopped => return,
        }

        match download_http_cache_range(&client, &url, &headers, Arc::clone(&shared), offset) {
            Ok(HttpDownloadOutcome::Eof) => {
                retry_state.reset();
                shared.mark_eof();
                match shared.wait_for_restart_after_eof() {
                    Some(next_offset) => offset = next_offset,
                    None => return,
                }
            }
            Ok(HttpDownloadOutcome::Restart(next_offset)) => {
                offset = next_offset;
                retry_state.reset();
            }
            Ok(HttpDownloadOutcome::Stopped) => return,
            Err(error) => {
                if shared.should_stop() {
                    return;
                }
                if let Some(next_offset) = shared.take_restart_offset() {
                    offset = next_offset;
                    retry_state.reset();
                    continue;
                }
                offset = error.offset;
                if error.retryable
                    && let Some((retry, delay)) = retry_state.next_delay(error.offset)
                {
                    tracing::warn!(
                        offset = error.offset,
                        retry,
                        max_retries = HTTP_CACHE_MAX_RETRIES,
                        retry_delay_ms = delay.as_millis(),
                        error = %error.message,
                        "retrying HTTP stream cache range after transient failure"
                    );
                    if !shared.wait_for_retry_delay(delay) {
                        return;
                    }
                    continue;
                }

                let reader_offset = shared.reader_offset_now();
                if reader_offset < error.offset {
                    tracing::warn!(
                        offset = error.offset,
                        reader_offset,
                        retryable = error.retryable,
                        error = %error.message,
                        "HTTP prefetch failed; deferring playback error until the reader reaches the gap"
                    );
                    match shared.wait_for_reader_at_or_restart(error.offset) {
                        CacheRetryPermit::Ready if error.retryable => {
                            retry_state.reset();
                            continue;
                        }
                        CacheRetryPermit::Ready => {}
                        CacheRetryPermit::Restart(next_offset) => {
                            offset = next_offset;
                            retry_state.reset();
                            continue;
                        }
                        CacheRetryPermit::Stopped => return,
                    }
                }

                shared.set_error_at(error.offset, error.message);
                match shared.wait_for_restart_after_error(error.offset) {
                    CacheRetryPermit::Ready => {
                        retry_state.reset();
                    }
                    CacheRetryPermit::Restart(next_offset) => {
                        offset = next_offset;
                        retry_state.reset();
                    }
                    CacheRetryPermit::Stopped => return,
                }
            }
        }
    }
}

pub(super) fn http_ring_cache_side_download_loop(
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
            tracing::warn!(%error, "creating HTTP side-cache client failed");
            return;
        }
    };

    loop {
        if shared.should_stop() {
            return;
        }
        let Some(request) = shared.wait_for_side_download_request() else {
            return;
        };
        let mut offset = request.offset;
        let mut retry_state = HttpRetryState::default();
        loop {
            match download_http_side_cache_range(
                &client,
                &url,
                &headers,
                Arc::clone(&shared),
                request,
                offset,
            ) {
                Ok(HttpDownloadOutcome::Restart(next_offset)) => {
                    offset = next_offset;
                    retry_state.reset();
                }
                Ok(HttpDownloadOutcome::Eof) => {
                    shared.finish_side_download(request, true);
                    break;
                }
                Ok(HttpDownloadOutcome::Stopped) => {
                    shared.finish_side_download(request, false);
                    return;
                }
                Err(error) => {
                    if shared.should_stop() {
                        shared.finish_side_download(request, false);
                        return;
                    }
                    offset = error.offset;
                    if error.retryable
                        && let Some((retry, delay)) = retry_state.next_delay(error.offset)
                    {
                        tracing::warn!(
                            offset = error.offset,
                            request_offset = request.offset,
                            range_kind = ?request.range_kind,
                            retry,
                            max_retries = HTTP_CACHE_MAX_RETRIES,
                            retry_delay_ms = delay.as_millis(),
                            error = %error.message,
                            "retrying HTTP side-cache range after transient failure"
                        );
                        if !shared.wait_for_retry_delay(delay) {
                            shared.finish_side_download(request, false);
                            return;
                        }
                        continue;
                    }
                    shared.finish_side_download_with_error(request, error.offset, error.message);
                    break;
                }
            }
        }
    }
}

fn download_http_cache_range(
    client: &reqwest::blocking::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
    shared: Arc<HttpRingCacheShared>,
    mut offset: u64,
) -> std::result::Result<HttpDownloadOutcome, HttpDownloadError> {
    let known_content_len = shared.content_len_now();
    if known_content_len.is_some_and(|content_len| offset >= content_len) {
        return Ok(HttpDownloadOutcome::Eof);
    }

    let range_request_bytes =
        http_cache_playback_range_request_bytes(shared.playback_range_request_bytes(offset));
    let range = http_cache_range_header(offset, known_content_len, range_request_bytes);
    let range_len = http_cache_range_request_len(offset, known_content_len, range_request_bytes);
    let request_timeout = http_cache_range_request_timeout(range_len);
    tracing::debug!(
        offset,
        range = %range,
        range_len,
        range_request_bytes_effective = range_request_bytes,
        request_timeout_ms = request_timeout.as_millis(),
        "requesting HTTP stream cache range"
    );
    let mut request = client
        .get(url)
        .timeout(request_timeout)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::CONNECTION, "keep-alive")
        .header(reqwest::header::RANGE, range.as_str());
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let mut response = request.send().map_err(|error| {
        let retryable = http_cache_request_should_retry(&error);
        HttpDownloadError::new(offset, format!("HTTP 视频缓存请求失败：{error}"), retryable)
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        shared.set_content_len(content_len_from_content_range(response.headers()));
        return Ok(HttpDownloadOutcome::Eof);
    }
    if offset > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(HttpDownloadError::new(
            offset,
            format!("HTTP 视频缓存 Range 请求失败：服务器返回 {status}"),
            http_cache_status_should_retry(status),
        ));
    }
    if offset == 0
        && status != reqwest::StatusCode::OK
        && status != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(HttpDownloadError::new(
            offset,
            format!("HTTP 视频缓存请求失败：服务器返回 {status}"),
            http_cache_status_should_retry(status),
        ));
    }
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = content_range_from_headers(response.headers()).ok_or_else(|| {
            HttpDownloadError::new(
                offset,
                "HTTP 视频缓存 Range 响应缺少 Content-Range".to_string(),
                false,
            )
        })?;
        if content_range.start != offset {
            return Err(HttpDownloadError::new(
                offset,
                format!(
                    "HTTP 视频缓存 Range 响应偏移不匹配：请求 {offset}，返回 {}",
                    content_range.start
                ),
                false,
            ));
        }
    }
    let content_len = content_len_from_response(&response, offset);
    shared.set_content_len(content_len);

    let mut chunk = vec![0; shared.chunk_size()];
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
            Err(error) => {
                if let Some(next_offset) = shared.take_restart_offset() {
                    tracing::debug!(
                        offset,
                        next_offset,
                        range = %range,
                        %error,
                        "HTTP stream cache read stopped for a pending restart"
                    );
                    return Ok(HttpDownloadOutcome::Restart(next_offset));
                }
                let retryable = http_cache_read_should_restart(&error);
                return Err(HttpDownloadError::new(
                    offset,
                    format!("读取 HTTP 视频缓存失败：{error}"),
                    retryable,
                ));
            }
        };
        if read == 0 {
            if content_len.is_some_and(|content_len| offset < content_len) {
                return Err(HttpDownloadError::new(
                    offset,
                    "HTTP 视频缓存响应在预期内容结束前提前关闭".to_string(),
                    true,
                ));
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

fn download_http_side_cache_range(
    client: &reqwest::blocking::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
    shared: Arc<HttpRingCacheShared>,
    request: CacheRestartRequest,
    mut offset: u64,
) -> std::result::Result<HttpDownloadOutcome, HttpDownloadError> {
    let known_content_len = shared.content_len_now();
    if known_content_len.is_some_and(|content_len| offset >= content_len) {
        return Ok(HttpDownloadOutcome::Eof);
    }

    let range_request_bytes = shared.side_range_request_bytes(request);
    let Some(request_bytes) =
        side_request_remaining_bytes(request, offset, known_content_len, range_request_bytes)
    else {
        return Ok(HttpDownloadOutcome::Eof);
    };
    let range = http_cache_range_header(offset, known_content_len, request_bytes);
    let range_len = http_cache_range_request_len(offset, known_content_len, request_bytes);
    let request_timeout = http_cache_range_request_timeout(range_len);
    tracing::debug!(
        offset,
        request_offset = request.offset,
        range = %range,
        range_len,
        range_request_bytes_effective = range_request_bytes,
        request_timeout_ms = request_timeout.as_millis(),
        range_kind = ?request.range_kind,
        "requesting HTTP side cache range"
    );
    let mut http_request = client
        .get(url)
        .timeout(request_timeout)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::CONNECTION, "keep-alive")
        .header(reqwest::header::RANGE, range.as_str());
    for (name, value) in headers {
        http_request = http_request.header(name, value);
    }

    let mut response = http_request.send().map_err(|error| {
        let retryable = http_cache_request_should_retry(&error);
        HttpDownloadError::new(
            offset,
            format!("HTTP 视频缓存辅助请求失败：{error}"),
            retryable,
        )
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        shared.set_content_len(content_len_from_content_range(response.headers()));
        return Ok(HttpDownloadOutcome::Eof);
    }
    if offset > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(HttpDownloadError::new(
            offset,
            format!("HTTP 视频缓存辅助 Range 请求失败：服务器返回 {status}"),
            http_cache_status_should_retry(status),
        ));
    }
    if offset == 0
        && status != reqwest::StatusCode::OK
        && status != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(HttpDownloadError::new(
            offset,
            format!("HTTP 视频缓存辅助请求失败：服务器返回 {status}"),
            http_cache_status_should_retry(status),
        ));
    }
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = content_range_from_headers(response.headers()).ok_or_else(|| {
            HttpDownloadError::new(
                offset,
                "HTTP 视频缓存辅助 Range 响应缺少 Content-Range".to_string(),
                false,
            )
        })?;
        if content_range.start != offset {
            return Err(HttpDownloadError::new(
                offset,
                format!(
                    "HTTP 视频缓存辅助 Range 响应偏移不匹配：请求 {offset}，返回 {}",
                    content_range.start
                ),
                false,
            ));
        }
    }
    let content_len = content_len_from_response(&response, offset);
    shared.set_content_len(content_len);

    let mut chunk = vec![0; shared.chunk_size()];
    loop {
        if shared.should_stop() {
            return Ok(HttpDownloadOutcome::Stopped);
        }
        let Some(request_remaining) =
            side_request_remaining_bytes(request, offset, content_len, range_request_bytes)
        else {
            return Ok(HttpDownloadOutcome::Eof);
        };
        let read_capacity = chunk
            .len()
            .min(usize::try_from(request_remaining).unwrap_or(usize::MAX));
        let read = response
            .read(&mut chunk[..read_capacity])
            .map_err(|error| {
                let retryable = http_cache_read_should_restart(&error);
                HttpDownloadError::new(
                    offset,
                    format!("读取 HTTP 视频缓存辅助 range 失败：{error}"),
                    retryable,
                )
            })?;
        if read == 0 {
            if content_len.is_some_and(|content_len| offset < content_len)
                && side_request_remaining_bytes(request, offset, content_len, range_request_bytes)
                    .is_some()
            {
                return Err(HttpDownloadError::new(
                    offset,
                    "HTTP 视频缓存辅助响应在预期 range 结束前提前关闭".to_string(),
                    true,
                ));
            }
            return Ok(HttpDownloadOutcome::Eof);
        }
        match shared.append_side_download_or_stop(request, offset, &chunk[..read]) {
            CacheAppendResult::Appended => {
                offset = offset.saturating_add(read as u64);
                if side_request_remaining_bytes(request, offset, content_len, range_request_bytes)
                    .is_none()
                {
                    return Ok(HttpDownloadOutcome::Eof);
                }
            }
            CacheAppendResult::Restart(next_offset) => {
                return Ok(HttpDownloadOutcome::Restart(next_offset));
            }
            CacheAppendResult::Stopped => return Ok(HttpDownloadOutcome::Stopped),
        }
    }
}

fn side_request_remaining_bytes(
    request: CacheRestartRequest,
    offset: u64,
    content_len: Option<u64>,
    range_request_bytes: u64,
) -> Option<u64> {
    let request_end = request.offset.saturating_add(range_request_bytes.max(1));
    let request_end = content_len.map_or(request_end, |content_len| request_end.min(content_len));
    (offset < request_end).then_some(request_end - offset)
}

fn http_cache_request_should_retry(error: &reqwest::Error) -> bool {
    error.is_timeout()
        || error.is_connect()
        || error.is_body()
        || transient_http_error_message(&error.to_string())
}

fn http_cache_status_should_retry(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

fn http_cache_read_should_restart(error: &std::io::Error) -> bool {
    http_cache_read_timed_out(error)
        || matches!(
            error.kind(),
            std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        )
        || transient_http_error_message(&error.to_string())
}

fn transient_http_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("incomplete")
        || message.contains("connection reset")
        || message.contains("connection closed")
        || message.contains("connection aborted")
        || message.contains("broken pipe")
        || message.contains("unexpected eof")
        || message.contains("end of file")
}

#[cfg(test)]
mod tests {
    use super::super::cache::{CacheRestartRequest, HttpCacheRangeKind};
    use super::{
        HTTP_CACHE_MAX_RETRIES, HttpRetryState, http_cache_status_should_retry,
        side_request_remaining_bytes, transient_http_error_message,
    };

    #[test]
    fn http_cache_retries_transient_gateway_statuses() {
        for status in [
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(http_cache_status_should_retry(status), "status={status}");
        }
    }

    #[test]
    fn http_cache_does_not_retry_permanent_client_statuses() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::NOT_FOUND,
        ] {
            assert!(!http_cache_status_should_retry(status), "status={status}");
        }
    }

    #[test]
    fn http_cache_retry_backoff_is_bounded_and_resets_for_new_offset() {
        let mut state = HttpRetryState::default();
        let delays = (0..HTTP_CACHE_MAX_RETRIES)
            .map(|_| state.next_delay(100).expect("retry remains").1.as_millis())
            .collect::<Vec<_>>();

        assert_eq!(delays, vec![200, 400, 800, 1_600, 2_000]);
        assert!(state.next_delay(100).is_none());
        assert_eq!(
            state.next_delay(200).expect("new offset resets retries").1,
            std::time::Duration::from_millis(200)
        );
    }

    #[test]
    fn side_request_remaining_bytes_stops_at_side_range_boundary() {
        let request = CacheRestartRequest {
            offset: 500,
            range_kind: HttpCacheRangeKind::Playback,
        };

        assert_eq!(
            side_request_remaining_bytes(request, 500, None, 128),
            Some(128)
        );
        assert_eq!(
            side_request_remaining_bytes(request, 627, None, 128),
            Some(1)
        );
        assert_eq!(side_request_remaining_bytes(request, 628, None, 128), None);
        assert_eq!(
            side_request_remaining_bytes(request, 500, Some(550), 128),
            Some(50)
        );
        assert_eq!(
            side_request_remaining_bytes(request, 550, Some(550), 128),
            None
        );
    }

    #[test]
    fn transient_http_error_message_matches_incomplete_body() {
        assert!(transient_http_error_message(
            "error reading a body from connection: IncompleteMessage"
        ));
        assert!(transient_http_error_message("connection reset by peer"));
        assert!(!transient_http_error_message(
            "HTTP 视频缓存 Range 响应偏移不匹配"
        ));
    }
}
