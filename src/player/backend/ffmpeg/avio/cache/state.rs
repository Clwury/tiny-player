pub(in crate::player::backend::ffmpeg::avio::cache) use std::collections::VecDeque;

pub(in crate::player::backend::ffmpeg::avio::cache) use crate::player::backend::PlaybackCacheConfig;

#[cfg(test)]
use super::HTTP_RING_CACHE_CAPACITY;
pub(in crate::player::backend::ffmpeg::avio::cache) use super::{
    ByteRingBuffer, CacheRestartRequest, HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR,
    HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES,
    HttpCacheConfig, HttpCacheRangeKind, HttpCacheReadError, HttpDiskCache,
    HttpPlaybackBufferRange, HttpRingCacheState, InputRateSample, RetainedCacheRange,
    RetainedPlaybackSpliceSource, http_stream_cache_status_changed,
};

pub(in crate::player::backend::ffmpeg::avio::cache) use progress::{
    merge_playback_buffer_ranges, merged_cached_byte_len, playback_buffer_range,
};

#[path = "state/download.rs"]
mod download;
#[path = "state/progress.rs"]
mod progress;
#[path = "state/range.rs"]
mod range;
#[path = "state/read.rs"]
mod read;
#[path = "state/report.rs"]
mod report;

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

    pub(in crate::player::backend::ffmpeg::avio::cache) fn new_with_config(
        start_offset: u64,
        config: HttpCacheConfig,
    ) -> Self {
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
        self.trim_to_capacity(self.active_memory_capacity());
        self.trim_retained_ranges_to_capacity(self.retained_capacity_with_side_reserve(false));
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
}
