use std::time::{Duration, Instant};

use super::{
    ByteRingBuffer, CacheRestartRequest, HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR,
    HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES,
    HttpCacheRangeKind, HttpRingCacheState, InputRateSample, RetainedCacheRange,
};

impl HttpRingCacheState {
    pub(in crate::player::backend::ffmpeg) fn append_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let input_len = data.len();
        let mut offset = offset;
        let mut data = data;
        self.record_input_bytes(input_len);
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = disk_cache.write_at(offset, data)
        {
            tracing::warn!(%error, "disabling HTTP disk cache after write failure");
            self.disk_cache_writable = false;
        }
        if offset != self.next_offset {
            self.restart_at_with_kind(offset, self.active_range_kind);
        }

        let max_capacity = self.buffer.max_capacity();
        if data.len() > max_capacity {
            let trim = data.len() - max_capacity;
            offset = offset.saturating_add(trim as u64);
            data = &data[trim..];
            self.restart_at_with_kind(offset, self.active_range_kind);
        }

        self.trim_to_capacity(max_capacity.saturating_sub(data.len()));
        if self.buffer.len().saturating_add(data.len()) > max_capacity {
            return false;
        }
        self.buffer.append(data);
        self.next_offset = offset.saturating_add(data.len() as u64);
        self.maybe_queue_playback_continuation();
        self.trim_to_capacity(self.active_memory_capacity());
        self.trim_retained_ranges_to_capacity(self.retained_capacity_with_side_reserve(false));
        true
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn maybe_queue_playback_continuation(
        &mut self,
    ) {
        if self.active_range_kind != HttpCacheRangeKind::Playback
            || self.config.range_request_bytes == 0
        {
            return;
        }
        let continuation_offset = self
            .active_request_start_offset
            .saturating_add(self.config.range_request_bytes);
        if self
            .content_len
            .is_some_and(|content_len| continuation_offset >= content_len)
        {
            return;
        }
        if self.next_offset < self.playback_continuation_prefetch_offset(continuation_offset) {
            return;
        }
        if self.request_playback_continuation_at(continuation_offset) {
            tracing::debug!(
                continuation_offset,
                active_request_start_offset = self.active_request_start_offset,
                active_next_offset = self.next_offset,
                range_request_bytes = self.config.range_request_bytes,
                "queued proactive HTTP playback continuation range"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn playback_continuation_prefetch_offset(
        &self,
        continuation_offset: u64,
    ) -> u64 {
        let trigger_bytes = self
            .config
            .range_request_bytes
            .saturating_mul(HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR)
            / HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR.max(1);
        self.active_request_start_offset
            .saturating_add(trigger_bytes.max(1))
            .min(continuation_offset)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn splice_retained_playback_at_active_end(
        &mut self,
        offset: u64,
    ) -> Option<u64> {
        if self.active_range_kind != HttpCacheRangeKind::Playback || offset != self.next_offset {
            return None;
        }
        let range_index = self.retained_ranges.iter().position(|range| {
            range.range_kind == HttpCacheRangeKind::Playback
                && offset >= range.base_offset
                && offset < range.next_offset
        })?;
        let range = self.retained_ranges.get(range_index)?;
        let copy_offset = usize::try_from(offset.saturating_sub(range.base_offset)).ok()?;
        let copy_len = usize::try_from(range.next_offset.saturating_sub(offset)).ok()?;
        if copy_len == 0 {
            return None;
        }

        let max_capacity = self.buffer.max_capacity();
        if copy_len > max_capacity {
            return None;
        }
        self.trim_to_capacity(max_capacity.saturating_sub(copy_len));
        if self.buffer.len().saturating_add(copy_len) > max_capacity {
            return None;
        }

        let mut data = vec![0; copy_len];
        let copied = self
            .retained_ranges
            .get(range_index)?
            .buffer
            .copy_at(copy_offset, &mut data);
        if copied == 0 {
            return None;
        }
        data.truncate(copied);
        let next_offset = offset.saturating_add(copied as u64);
        self.buffer.append(&data);
        self.next_offset = next_offset;
        self.active_request_start_offset = offset;
        if self
            .retained_ranges
            .get(range_index)
            .is_some_and(|range| offset <= range.base_offset && next_offset >= range.next_offset)
        {
            self.retained_ranges.remove(range_index);
        }
        self.maybe_queue_playback_continuation();
        self.trim_to_capacity(self.active_memory_capacity());
        self.trim_retained_ranges_to_capacity(self.retained_capacity_with_side_reserve(false));
        tracing::debug!(
            offset,
            next_offset,
            copied,
            "spliced proactive HTTP playback range into active stream cache"
        );
        Some(next_offset)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn append_retained_at(
        &mut self,
        offset: u64,
        data: &[u8],
        range_kind: HttpCacheRangeKind,
    ) -> bool {
        self.append_retained_at_preserving(offset, data, range_kind, &[])
    }

    pub(in crate::player::backend::ffmpeg) fn append_retained_at_protected(
        &mut self,
        offset: u64,
        data: &[u8],
        request: CacheRestartRequest,
    ) -> bool {
        self.append_retained_at_preserving(offset, data, request.range_kind, &[request])
    }

    fn append_retained_at_preserving(
        &mut self,
        offset: u64,
        data: &[u8],
        range_kind: HttpCacheRangeKind,
        protected: &[CacheRestartRequest],
    ) -> bool {
        if data.is_empty() {
            return true;
        }
        let input_len = data.len();
        let original_offset = offset;
        self.record_input_bytes(input_len);
        self.trim_to_capacity(self.active_memory_capacity());
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = disk_cache.write_at(offset, data)
        {
            tracing::warn!(%error, "disabling HTTP disk cache after side-range write failure");
            self.disk_cache_writable = false;
        }

        let max_capacity = self.buffer.max_capacity();
        let mut offset = offset;
        let mut data = data;
        if data.len() > max_capacity {
            let trim = data.len() - max_capacity;
            offset = offset.saturating_add(trim as u64);
            data = &data[trim..];
        }
        if data.is_empty() {
            return true;
        }

        let range_index = self.retained_range_index_for_append(offset, range_kind);
        let range_index = match range_index {
            Some(range_index) => range_index,
            None => {
                let mut buffer = ByteRingBuffer::new(max_capacity);
                buffer.append(data);
                let last_used_generation = self.next_retained_access_generation();
                self.retained_ranges.push_back(RetainedCacheRange {
                    buffer,
                    base_offset: offset,
                    next_offset: offset.saturating_add(data.len() as u64),
                    range_kind,
                    last_used_generation,
                });
                self.trim_retained_ranges_to_capacity_preserving(
                    self.retained_capacity_with_side_reserve(!protected.is_empty()),
                    protected,
                );
                tracing::debug!(
                    offset = original_offset,
                    retained_offset = offset,
                    len = input_len,
                    ?range_kind,
                    "stored HTTP side download in retained cache range"
                );
                return true;
            }
        };

        let generation = self.next_retained_access_generation();
        if let Some(range) = self.retained_ranges.get_mut(range_index) {
            if offset < range.next_offset {
                let skip = usize::try_from(range.next_offset - offset)
                    .unwrap_or(usize::MAX)
                    .min(data.len());
                offset = offset.saturating_add(skip as u64);
                data = &data[skip..];
            }
            if data.is_empty() {
                range.last_used_generation = generation;
                return true;
            }
            let overflow = range
                .buffer
                .len()
                .saturating_add(data.len())
                .saturating_sub(max_capacity);
            if overflow > 0 {
                range.buffer.discard_front(overflow);
                range.base_offset = range.base_offset.saturating_add(overflow as u64);
            }
            if range.buffer.len().saturating_add(data.len()) > max_capacity {
                return false;
            }
            range.buffer.append(data);
            range.next_offset = offset.saturating_add(data.len() as u64);
            range.last_used_generation = generation;
        }
        self.trim_retained_ranges_to_capacity_preserving(
            self.retained_capacity_with_side_reserve(!protected.is_empty()),
            protected,
        );
        true
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn retained_range_index_for_append(
        &self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> Option<usize> {
        self.retained_ranges.iter().position(|range| {
            range.range_kind == range_kind
                && offset >= range.base_offset
                && offset <= range.next_offset
        })
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn record_input_bytes(
        &mut self,
        bytes: usize,
    ) {
        if bytes == 0 {
            return;
        }
        self.input_rate_samples.push_back(InputRateSample {
            at: Instant::now(),
            bytes,
        });
        self.prune_input_rate_samples();
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn raw_input_rate(&self) -> Option<u64> {
        let now = Instant::now();
        let bytes: usize = self
            .input_rate_samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) <= Duration::from_secs(1))
            .map(|sample| sample.bytes)
            .sum();
        (bytes > 0).then(|| u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn prune_input_rate_samples(&mut self) {
        let now = Instant::now();
        while self
            .input_rate_samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) > Duration::from_secs(1))
        {
            self.input_rate_samples.pop_front();
        }
    }

    pub(in crate::player::backend::ffmpeg) fn trim_to_capacity(&mut self, capacity: usize) {
        let overflow = self.buffer.len().saturating_sub(capacity);
        if overflow == 0 {
            return;
        }
        let consumed = if self.offset_in_active_range(self.reader_offset) {
            self.reader_offset
                .saturating_sub(self.base_offset)
                .min(self.buffer.len() as u64) as usize
        } else {
            0
        };
        let trim = overflow.min(consumed);
        if trim == 0 {
            return;
        }
        self.buffer.discard_front(trim);
        self.base_offset = self.base_offset.saturating_add(trim as u64);
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn trim_retained_ranges_to_capacity(
        &mut self,
        capacity: usize,
    ) {
        self.trim_retained_ranges_to_capacity_preserving(capacity, &[]);
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn trim_retained_ranges_to_capacity_preserving(
        &mut self,
        capacity: usize,
        protected: &[CacheRestartRequest],
    ) {
        let mut retained_bytes = self.retained_memory_bytes();
        while retained_bytes > capacity {
            let Some(range_index) = self
                .retained_ranges
                .iter()
                .enumerate()
                .filter(|(_, range)| !retained_range_protected(range, protected))
                .min_by_key(|(_, range)| range.last_used_generation)
                .map(|(index, _)| index)
            else {
                break;
            };
            let Some(range) = self.retained_ranges.get_mut(range_index) else {
                break;
            };
            let overflow = retained_bytes - capacity;
            let trim = overflow.min(range.buffer.len());
            if trim == 0 {
                break;
            }
            range.buffer.discard_front(trim);
            range.base_offset = range.base_offset.saturating_add(trim as u64);
            retained_bytes -= trim;
            if range.buffer.len() == 0 || range.base_offset >= range.next_offset {
                self.retained_ranges.remove(range_index);
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn retained_memory_bytes(&self) -> usize {
        self.retained_ranges
            .iter()
            .map(|range| range.buffer.len())
            .sum()
    }

    pub(in crate::player::backend::ffmpeg) fn append_capacity_from(
        &mut self,
        offset: u64,
    ) -> usize {
        self.trim_to_capacity(self.active_memory_capacity());
        if self.active_range_kind == HttpCacheRangeKind::Playback
            && !self.offset_in_active_range(self.reader_offset)
            && (self.cached_range_contains(self.reader_offset)
                || self.side_download_may_produce(self.reader_offset))
        {
            self.prefetch_paused = true;
            return 0;
        }
        let active_reader_offset = self.reader_offset.max(self.base_offset);
        let buffered_ahead = offset.saturating_sub(active_reader_offset);
        let target = self.target_readahead_bytes();
        let resume = self.resume_readahead_bytes(target);
        if self.prefetch_paused {
            if buffered_ahead > resume {
                return 0;
            }
            self.prefetch_paused = false;
        }
        if buffered_ahead >= target {
            self.prefetch_paused = true;
            return 0;
        }

        let buffered_ahead = usize::try_from(buffered_ahead).unwrap_or(usize::MAX);
        let target = usize::try_from(target).unwrap_or(usize::MAX);
        self.active_memory_capacity()
            .min(target)
            .saturating_sub(buffered_ahead)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn target_readahead_bytes(&self) -> u64 {
        let memory_capacity = self.active_memory_capacity() as u64;
        let by_seconds = self
            .content_len
            .zip(self.duration_seconds)
            .map(|(content_len, duration)| {
                ((content_len as f64 / duration) * self.config.readahead_seconds).round() as u64
            })
            .filter(|bytes| *bytes > 0)
            .unwrap_or(memory_capacity);
        let by_config = self.config.max_readahead_bytes.unwrap_or(memory_capacity);
        by_seconds.min(by_config).min(memory_capacity).max(1)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn resume_readahead_bytes(
        &self,
        target: u64,
    ) -> u64 {
        let Some((content_len, duration)) = self.content_len.zip(self.duration_seconds) else {
            return target / 2;
        };
        let hysteresis_bytes =
            ((content_len as f64 / duration) * self.config.hysteresis_seconds).round() as u64;
        if hysteresis_bytes == 0 {
            target
        } else {
            target.saturating_sub(hysteresis_bytes).min(target)
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn side_retain_reserve_bytes(
        &self,
    ) -> usize {
        let target = usize::try_from(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES).unwrap_or(usize::MAX);
        target.min(self.config.memory_capacity / 8)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn active_memory_capacity(&self) -> usize {
        if self.active_range_kind == HttpCacheRangeKind::Playback {
            if self.config.memory_capacity == 0 {
                return 0;
            }
            self.config
                .memory_capacity
                .saturating_sub(self.side_retain_reserve_bytes())
                .max(1)
        } else {
            self.config.memory_capacity
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn retained_capacity_with_side_reserve(
        &self,
        protected: bool,
    ) -> usize {
        let memory_capacity = if protected {
            self.config
                .memory_capacity
                .saturating_add(self.side_retain_reserve_bytes())
        } else {
            self.config.memory_capacity
        };
        memory_capacity.saturating_sub(self.buffer.len())
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn side_range_request_bytes(
        &self,
        range_kind: HttpCacheRangeKind,
    ) -> u64 {
        let configured = self.config.range_request_bytes.max(1);
        match range_kind {
            HttpCacheRangeKind::Playback | HttpCacheRangeKind::TailMetadataProbe => configured.min(
                u64::try_from(self.side_retain_reserve_bytes())
                    .unwrap_or(u64::MAX)
                    .max(1),
            ),
        }
    }
}

fn retained_range_protected(range: &RetainedCacheRange, protected: &[CacheRestartRequest]) -> bool {
    protected.iter().any(|request| {
        request.range_kind == range.range_kind
            && request.offset >= range.base_offset
            && request.offset < range.next_offset
    })
}
