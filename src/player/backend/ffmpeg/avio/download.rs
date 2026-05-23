use super::{
    cache::{CacheAppendPermit, CacheAppendResult, HttpRingCacheShared},
    http::{
        content_len_from_content_range, content_len_from_response, content_range_from_headers,
        http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
        http_cache_read_timed_out,
    },
    *,
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
        Err(error) if error.is_timeout() => {
            tracing::debug!(
                offset,
                range = %range,
                "HTTP stream cache range request timed out; restarting"
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
            Err(error) if http_cache_read_timed_out(&error) => {
                if let Some(next_offset) = shared.take_restart_offset() {
                    tracing::debug!(
                        offset,
                        next_offset,
                        range = %range,
                        "HTTP stream cache read timed out with pending restart"
                    );
                    return Ok(HttpDownloadOutcome::Restart(next_offset));
                }
                tracing::debug!(
                    offset,
                    range = %range,
                    "HTTP stream cache read timed out; restarting current range"
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
