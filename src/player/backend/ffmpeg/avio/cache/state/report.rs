use crate::player::backend::ByteCacheState;

use super::{
    HttpCacheRangeKind, HttpPlaybackBufferRange, HttpRingCacheState,
    http_stream_cache_status_changed, merge_playback_buffer_ranges, merged_cached_byte_len,
    playback_buffer_range,
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
        let raw_input_rate = self.raw_input_rate();
        let active_forward_bytes = self.active_forward_bytes();
        ByteCacheState {
            ranges: ranges.into_iter().map(Into::into).collect(),
            reader_fraction,
            download_fraction,
            cached_bytes: self.cached_bytes(),
            content_length: content_len,
            disk_cache_enabled: self.disk_cache_writable,
            idle: self.cache_idle(),
            raw_input_rate,
            active_forward_bytes,
            active_forward_est_seconds: self.active_forward_est_seconds(raw_input_rate),
            range_request_bytes_effective: self.range_request_bytes_effective(),
            byte_level_seeks: self.byte_level_seeks,
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn active_forward_bytes(&self) -> u64 {
        if self.active_range_kind != HttpCacheRangeKind::Playback {
            return 0;
        }
        self.next_offset.saturating_sub(self.reader_offset)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn active_forward_est_seconds(
        &self,
        raw_input_rate: Option<u64>,
    ) -> Option<f64> {
        let bytes_per_second = raw_input_rate
            .filter(|rate| *rate > 0)
            .or_else(|| self.media_bitrate_bytes_per_second());
        bytes_per_second.map(|rate| self.active_forward_bytes() as f64 / rate as f64)
    }

    fn media_bitrate_bytes_per_second(&self) -> Option<u64> {
        let (content_len, duration) = self.content_len.zip(self.duration_seconds)?;
        let bytes_per_second = content_len as f64 / duration;
        bytes_per_second
            .is_finite()
            .then(|| bytes_per_second.round() as u64)
            .filter(|rate| *rate > 0)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn range_request_bytes_effective(
        &self,
    ) -> u64 {
        self.config.range_request_bytes.max(1)
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
