use std::{io::Read, sync::Arc, time::Duration};

use super::{
    cache::{CacheAppendPermit, CacheAppendResult, CacheRestartRequest, HttpRingCacheShared},
    http::{
        content_len_from_content_range, content_len_from_response, content_range_from_headers,
        http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
        http_cache_read_timed_out,
    },
};

enum HttpDownloadOutcome {
    Eof,
    Restart(u64),
    Stopped,
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
            shared.set_error(format!("创建 HTTP 视频缓存辅助客户端失败：{error}"));
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
                    shared.finish_side_download(request, false);
                    shared.set_error(error);
                    return;
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
) -> std::result::Result<HttpDownloadOutcome, String> {
    let known_content_len = shared.content_len_now();
    if known_content_len.is_some_and(|content_len| offset >= content_len) {
        return Ok(HttpDownloadOutcome::Eof);
    }

    let range_request_bytes = shared.range_request_bytes();
    let range = http_cache_range_header(offset, known_content_len, range_request_bytes);
    let range_len = http_cache_range_request_len(offset, known_content_len, range_request_bytes);
    let request_timeout = http_cache_range_request_timeout(range_len);
    tracing::debug!(
        offset,
        range = %range,
        range_len,
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

    let mut response = match request.send() {
        Ok(response) => response,
        Err(error) if http_cache_request_should_retry(&error) => {
            tracing::debug!(
                offset,
                range = %range,
                %error,
                "HTTP stream cache range request failed transiently; restarting"
            );
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
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = content_range_from_headers(response.headers())
            .ok_or_else(|| "HTTP 视频缓存 Range 响应缺少 Content-Range".to_string())?;
        if content_range.start != offset {
            return Err(format!(
                "HTTP 视频缓存 Range 响应偏移不匹配：请求 {offset}，返回 {}",
                content_range.start
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
            Err(error) if http_cache_read_should_restart(&error) => {
                if let Some(next_offset) = shared.take_restart_offset() {
                    tracing::debug!(
                        offset,
                        next_offset,
                        range = %range,
                        %error,
                        "HTTP stream cache read failed transiently with pending restart"
                    );
                    return Ok(HttpDownloadOutcome::Restart(next_offset));
                }
                tracing::debug!(
                    offset,
                    range = %range,
                    %error,
                    "HTTP stream cache read failed transiently; restarting current range"
                );
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

fn download_http_side_cache_range(
    client: &reqwest::blocking::Client,
    url: &str,
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
    shared: Arc<HttpRingCacheShared>,
    request: CacheRestartRequest,
    mut offset: u64,
) -> std::result::Result<HttpDownloadOutcome, String> {
    let known_content_len = shared.content_len_now();
    if known_content_len.is_some_and(|content_len| offset >= content_len) {
        return Ok(HttpDownloadOutcome::Eof);
    }

    let range_request_bytes = shared.range_request_bytes();
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

    let mut response = match http_request.send() {
        Ok(response) => response,
        Err(error) if http_cache_request_should_retry(&error) => {
            tracing::debug!(
                offset,
                range = %range,
                %error,
                "HTTP side cache range request failed transiently; restarting"
            );
            return Ok(HttpDownloadOutcome::Restart(offset));
        }
        Err(error) => return Err(format!("HTTP 视频缓存辅助请求失败：{error}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        shared.set_content_len(content_len_from_content_range(response.headers()));
        return Ok(HttpDownloadOutcome::Eof);
    }
    if offset > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "HTTP 视频缓存辅助 Range 请求失败：服务器返回 {status}"
        ));
    }
    if offset == 0
        && status != reqwest::StatusCode::OK
        && status != reqwest::StatusCode::PARTIAL_CONTENT
    {
        return Err(format!("HTTP 视频缓存辅助请求失败：服务器返回 {status}"));
    }
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let content_range = content_range_from_headers(response.headers())
            .ok_or_else(|| "HTTP 视频缓存辅助 Range 响应缺少 Content-Range".to_string())?;
        if content_range.start != offset {
            return Err(format!(
                "HTTP 视频缓存辅助 Range 响应偏移不匹配：请求 {offset}，返回 {}",
                content_range.start
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
        let read = match response.read(&mut chunk[..read_capacity]) {
            Ok(read) => read,
            Err(error) if http_cache_read_should_restart(&error) => {
                tracing::debug!(
                    offset,
                    range = %range,
                    %error,
                    "HTTP side cache read failed transiently; restarting current side range"
                );
                return Ok(HttpDownloadOutcome::Restart(offset));
            }
            Err(error) => {
                return Err(format!("读取 HTTP 视频缓存辅助 range 失败：{error}"));
            }
        };
        if read == 0 {
            if content_len.is_some_and(|content_len| offset < content_len)
                && side_request_remaining_bytes(request, offset, content_len, range_request_bytes)
                    .is_some()
            {
                return Ok(HttpDownloadOutcome::Restart(offset));
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
    use super::{side_request_remaining_bytes, transient_http_error_message};

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
