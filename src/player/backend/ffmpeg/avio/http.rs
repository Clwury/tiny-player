use std::{ffi::CString, ptr, time::Duration};

use ffmpeg_sys_next as ffi;

use super::super::{
    FFMPEG_FAST_ANALYZE_DURATION_US, FFMPEG_FAST_PROBE_SIZE, FFMPEG_SUBTITLE_ANALYZE_DURATION_US,
    FFMPEG_SUBTITLE_PROBE_SIZE, HTTP_CACHE_RANGE_REQUEST_TIMEOUT,
    HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES, HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT,
    InputProbeProfile, ffmpeg_error,
};

pub(in crate::player::backend::ffmpeg) fn input_format_options(
    http_headers: &[(String, String)],
    probe_profile: InputProbeProfile,
) -> std::result::Result<*mut ffi::AVDictionary, String> {
    let mut options = ptr::null_mut();
    let probe_limits = match probe_profile {
        InputProbeProfile::Fast => Some((FFMPEG_FAST_PROBE_SIZE, FFMPEG_FAST_ANALYZE_DURATION_US)),
        InputProbeProfile::Subtitle => Some((
            FFMPEG_SUBTITLE_PROBE_SIZE,
            FFMPEG_SUBTITLE_ANALYZE_DURATION_US,
        )),
        InputProbeProfile::Full => None,
    };
    if let Some((probe_size, analyze_duration_us)) = probe_limits {
        if let Err(error) =
            set_input_format_option(&mut options, "probesize", &probe_size.to_string())
        {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err(error);
        }
        if let Err(error) = set_input_format_option(
            &mut options,
            "analyzeduration",
            &analyze_duration_us.to_string(),
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

pub(in crate::player::backend::ffmpeg) fn ffmpeg_http_headers(
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

pub(in crate::player::backend::ffmpeg) fn should_cache_http_url(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("http://") || url.starts_with("https://")
}

pub(in crate::player::backend::ffmpeg) fn reqwest_header_pairs(
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
pub(in crate::player::backend::ffmpeg) fn http_cache_request_headers_for_log(
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
pub(in crate::player::backend::ffmpeg) fn http_cache_response_headers_for_log(
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

pub(in crate::player::backend::ffmpeg) fn http_cache_range_header(
    offset: u64,
    content_len: Option<u64>,
    request_bytes: u64,
) -> String {
    let end = http_cache_range_end(offset, content_len, request_bytes);
    format!("bytes={offset}-{end}")
}

fn http_cache_range_end(offset: u64, content_len: Option<u64>, request_bytes: u64) -> u64 {
    let requested_end = offset.saturating_add(request_bytes.max(1).saturating_sub(1));
    content_len
        .and_then(|content_len| content_len.checked_sub(1))
        .map_or(requested_end, |content_end| requested_end.min(content_end))
}

pub(in crate::player::backend::ffmpeg) fn http_cache_range_request_len(
    offset: u64,
    content_len: Option<u64>,
    request_bytes: u64,
) -> u64 {
    http_cache_range_end(offset, content_len, request_bytes)
        .saturating_sub(offset)
        .saturating_add(1)
}

pub(in crate::player::backend::ffmpeg) fn http_cache_playback_range_request_bytes(
    configured_request_bytes: u64,
) -> u64 {
    configured_request_bytes.max(1)
}

pub(in crate::player::backend::ffmpeg) fn http_cache_range_request_timeout(
    request_len: u64,
) -> Duration {
    if request_len <= HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES {
        HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT
    } else {
        HTTP_CACHE_RANGE_REQUEST_TIMEOUT
    }
}

pub(super) fn http_cache_read_timed_out(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) || error
        .get_ref()
        .and_then(|source| source.downcast_ref::<reqwest::Error>())
        .is_some_and(reqwest::Error::is_timeout)
}

pub(super) fn content_len_from_response(
    response: &reqwest::blocking::Response,
    offset: u64,
) -> Option<u64> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct HttpContentRange {
    pub(in crate::player::backend::ffmpeg) start: u64,
    pub(in crate::player::backend::ffmpeg) end: u64,
    pub(in crate::player::backend::ffmpeg) total: Option<u64>,
}

pub(in crate::player::backend::ffmpeg) fn content_len_from_content_range(
    headers: &reqwest::header::HeaderMap,
) -> Option<u64> {
    content_range_from_headers(headers)?.total
}

pub(in crate::player::backend::ffmpeg) fn content_range_from_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<HttpContentRange> {
    let value = headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim();
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.rsplit_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    let total = if total == "*" {
        None
    } else {
        Some(total.parse().ok()?)
    };
    Some(HttpContentRange { start, end, total })
}
