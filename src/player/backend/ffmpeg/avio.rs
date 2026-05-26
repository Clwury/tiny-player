use super::*;

mod cache;
mod callbacks;
mod download;
mod http;

pub(super) use cache::HttpRingCache;
#[cfg(test)]
pub(super) use cache::{
    CacheReadResult, CacheRestartRequest, HttpCacheRangeKind, HttpRingCacheState,
};
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

pub(super) struct CachedInputSource {
    cache: Option<HttpRingCache>,
    released: bool,
}

impl CachedInputSource {
    pub(super) fn new(
        url: &str,
        http_headers: &[(String, String)],
        content_len_hint: Option<u64>,
        cache_config: &PlaybackCacheConfig,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let cache = if should_cache_http_url(url)
            && !matches!(cache_config.mode, PlaybackCacheMode::Disabled)
        {
            Some(HttpRingCache::spawn(
                url.to_string(),
                http_headers,
                content_len_hint,
                cache_config,
                control,
                event_tx,
            )?)
        } else {
            None
        };
        Ok(Self {
            cache,
            released: false,
        })
    }

    pub(super) fn cached_avio(&self) -> std::result::Result<Option<CachedAvio>, String> {
        self.cache
            .as_ref()
            .map(|cache| CachedAvio::from_cache(cache.clone(), false))
            .transpose()
    }

    pub(super) fn release(&mut self) {
        self.released = true;
    }

    #[cfg(test)]
    pub(super) fn from_cache_for_test(cache: HttpRingCache) -> Self {
        Self {
            cache: Some(cache),
            released: false,
        }
    }
}

impl Drop for CachedInputSource {
    fn drop(&mut self) {
        if !self.released
            && let Some(cache) = &self.cache
        {
            cache.shutdown();
        }
    }
}

impl CachedAvio {
    pub(super) fn from_cache(
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

    pub(super) fn shutdown_cache_on_drop(&mut self) {
        self.shutdown_cache_on_drop = true;
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
