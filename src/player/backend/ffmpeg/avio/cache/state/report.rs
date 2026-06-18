use crate::player::backend::ByteCacheState;

use super::{
    HttpPlaybackBufferRange, HttpRingCacheState, http_stream_cache_status_changed,
    merge_playback_buffer_ranges, merged_cached_byte_len, playback_buffer_range,
};

impl HttpRingCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_cache_status_for_test(
        &self,
    ) -> ByteCacheState {
        self.stream_cache_status()
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn stream_cache_status(
        &self,
    ) -> ByteCacheState {
        let content_len = self.content_len;
        let ranges = self.stream_buffer_ranges();
        let reader_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.reader_offset.min(content_len) as f64 / content_len as f64);
        let download_fraction = content_len
            .filter(|content_len| *content_len > 0)
            .map(|content_len| self.next_offset.min(content_len) as f64 / content_len as f64);
        ByteCacheState {
            ranges: ranges.into_iter().map(Into::into).collect(),
            reader_fraction,
            download_fraction,
            cached_bytes: self.cached_bytes(),
            content_length: content_len,
            disk_cache_enabled: self.disk_cache_writable,
            idle: self.cache_idle(),
            raw_input_rate: self.raw_input_rate(),
            byte_level_seeks: self.byte_level_seeks,
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn cache_idle(&self) -> bool {
        (self.prefetch_paused || self.eof)
            && self.restart_request.is_none()
            && self.side_download_requests.is_empty()
            && self.side_download_active.is_empty()
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn stream_buffer_ranges(
        &self,
    ) -> Vec<HttpPlaybackBufferRange> {
        let Some(content_len) = self.content_len.filter(|content_len| *content_len > 0) else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        for range in &self.retained_ranges {
            if let Some(range) =
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
            {
                ranges.push(range);
            }
        }
        if let Some(range) = playback_buffer_range(self.base_offset, self.next_offset, content_len)
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

    pub(in crate::player::backend::ffmpeg::avio::cache) fn cached_bytes(&self) -> u64 {
        let mut ranges = Vec::new();
        if self.next_offset > self.base_offset {
            ranges.push((self.base_offset, self.next_offset));
        }
        ranges.extend(
            self.retained_ranges
                .iter()
                .filter(|range| range.next_offset > range.base_offset)
                .map(|range| (range.base_offset, range.next_offset)),
        );
        if let Some(disk_cache) = &self.disk_cache {
            ranges.extend(
                disk_cache
                    .ranges
                    .iter()
                    .map(|range| (range.start, range.end)),
            );
        }
        merged_cached_byte_len(ranges)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn take_stream_cache_status_report(
        &mut self,
    ) -> Option<ByteCacheState> {
        let status = self.stream_cache_status();
        if !http_stream_cache_status_changed(
            self.last_reported_status.as_ref(),
            &status,
            self.config.range_request_bytes,
        ) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}
