use super::*;

mod cache;
mod callbacks;
mod download;
mod http;

pub(super) use cache::HttpRingCache;
#[cfg(test)]
pub(super) use cache::{CacheReadResult, HttpCacheRangeKind, HttpRingCacheState};
use callbacks::{CachedAvioReader, cached_avio_read_packet, cached_avio_seek};
#[cfg(test)]
pub(super) use http::{
    HttpContentRange, content_len_from_content_range, content_range_from_headers,
    ffmpeg_http_headers, http_cache_range_header, http_cache_range_request_len,
    http_cache_range_request_timeout,
};
#[cfg(test)]
pub(super) use http::{http_cache_request_headers_for_log, http_cache_response_headers_for_log};
pub(super) use http::{input_format_options, reqwest_header_pairs, should_cache_http_url};

pub(super) struct CachedAvio {
    ptr: *mut ffi::AVIOContext,
    reader: *mut CachedAvioReader,
    cache: HttpRingCache,
    shutdown_cache_on_drop: bool,
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
        Self::from_cache(cache, true)
    }

    fn from_cache(
        cache: HttpRingCache,
        shutdown_cache_on_drop: bool,
    ) -> std::result::Result<Self, String> {
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

        Ok(Self {
            ptr,
            reader,
            cache,
            shutdown_cache_on_drop,
        })
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.ptr
    }

    pub(super) fn cache(&self) -> HttpRingCache {
        self.cache.clone()
    }
}

impl Drop for CachedAvio {
    fn drop(&mut self) {
        if self.shutdown_cache_on_drop {
            self.cache.shutdown();
        }
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
