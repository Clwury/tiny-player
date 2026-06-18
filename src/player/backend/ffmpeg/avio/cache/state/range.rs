use super::{
    ByteRingBuffer, CacheRestartRequest, HttpCacheRangeKind, HttpRingCacheState, RetainedCacheRange,
};

impl HttpRingCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn restart_at(&mut self, offset: u64) {
        self.restart_at_with_kind(offset, HttpCacheRangeKind::Playback);
    }

    pub(in crate::player::backend::ffmpeg) fn restart_at_with_kind(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        tracing::debug!(
            offset,
            ?range_kind,
            previous_base_offset = self.base_offset,
            previous_next_offset = self.next_offset,
            previous_reader_offset = self.reader_offset,
            previous_active_range_kind = ?self.active_range_kind,
            previous_buffer_len = self.buffer.len(),
            "restarting HTTP stream cache range"
        );
        self.retain_current_range_for_restart(offset);
        self.buffer.clear();
        self.base_offset = offset;
        self.next_offset = offset;
        self.active_request_start_offset = offset;
        self.active_range_kind = range_kind;
        self.pending_seek_range_kind = None;
        self.reader_offset = offset;
        self.eof = false;
        self.error = None;
        self.last_reported_status = None;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn retain_current_range_for_restart(
        &mut self,
        offset: u64,
    ) {
        if self.buffer.len() == 0 || (offset >= self.base_offset && offset <= self.next_offset) {
            return;
        }

        tracing::debug!(
            base_offset = self.base_offset,
            next_offset = self.next_offset,
            restart_offset = offset,
            active_range_kind = ?self.active_range_kind,
            "retaining HTTP stream cache range across restart"
        );
        let capacity = self.buffer.max_capacity();
        let buffer = std::mem::replace(&mut self.buffer, ByteRingBuffer::new(capacity));
        let last_used_generation = self.next_retained_access_generation();
        self.retained_ranges.push_back(RetainedCacheRange {
            buffer,
            base_offset: self.base_offset,
            next_offset: self.next_offset,
            range_kind: self.active_range_kind,
            last_used_generation,
        });
        self.trim_retained_ranges_to_capacity(self.config.memory_capacity);
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn next_retained_access_generation(
        &mut self,
    ) -> u64 {
        self.retained_access_generation = self.retained_access_generation.saturating_add(1);
        self.retained_access_generation
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn queue_read_miss_at(
        &mut self,
        offset: u64,
    ) -> bool {
        let range_kind = self.take_range_kind_for_miss(offset);
        self.request_side_download_at(offset, range_kind)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn range_kind_for_miss(
        &self,
        offset: u64,
    ) -> HttpCacheRangeKind {
        self.pending_seek_range_kind
            .filter(|(pending_offset, _)| *pending_offset == offset)
            .map(|(_, range_kind)| range_kind)
            .unwrap_or_else(|| {
                if self.is_tail_metadata_probe_seek(offset) {
                    HttpCacheRangeKind::TailMetadataProbe
                } else {
                    HttpCacheRangeKind::Playback
                }
            })
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn take_range_kind_for_miss(
        &mut self,
        offset: u64,
    ) -> HttpCacheRangeKind {
        if let Some((pending_offset, range_kind)) = self.pending_seek_range_kind.take()
            && pending_offset == offset
        {
            return range_kind;
        }

        self.range_kind_for_miss(offset)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn request_side_download_at(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> bool {
        if self.cached_range_contains(offset)
            || self.side_download_request_exists(offset, range_kind)
        {
            return false;
        }
        tracing::debug!(
            offset,
            ?range_kind,
            active_base_offset = self.base_offset,
            active_next_offset = self.next_offset,
            "queueing HTTP side download range"
        );
        self.side_download_requests
            .push_back(CacheRestartRequest { offset, range_kind });
        true
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn request_playback_continuation_at(
        &mut self,
        offset: u64,
    ) -> bool {
        if self.stale_playback_continuation_offset(offset) {
            tracing::trace!(
                offset,
                active_base_offset = self.base_offset,
                active_next_offset = self.next_offset,
                reader_offset = self.reader_offset,
                "skipping stale HTTP playback continuation range"
            );
            return false;
        }
        self.request_side_download_at(offset, HttpCacheRangeKind::Playback)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn stale_playback_continuation_offset(
        &self,
        offset: u64,
    ) -> bool {
        self.active_range_kind == HttpCacheRangeKind::Playback && offset < self.next_offset
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn side_download_request_exists(
        &self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> bool {
        self.side_download_requests
            .iter()
            .chain(self.side_download_active.iter())
            .any(|request| {
                request.range_kind == range_kind
                    && request.offset <= offset
                    && offset
                        < request
                            .offset
                            .saturating_add(self.config.range_request_bytes)
                    && self
                        .content_len
                        .is_none_or(|content_len| request.offset < content_len)
            })
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn cached_range_contains(
        &self,
        offset: u64,
    ) -> bool {
        (offset >= self.base_offset && offset < self.next_offset)
            || self
                .retained_ranges
                .iter()
                .any(|range| offset >= range.base_offset && offset < range.next_offset)
            || self
                .disk_cache
                .as_ref()
                .is_some_and(|disk_cache| disk_cache.range_index_containing(offset).is_some())
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn side_download_may_produce(
        &self,
        offset: u64,
    ) -> bool {
        self.side_download_requests
            .iter()
            .chain(self.side_download_active.iter())
            .any(|request| {
                offset >= request.offset
                    && offset
                        < request
                            .offset
                            .saturating_add(self.config.range_request_bytes)
                    && self
                        .content_len
                        .is_none_or(|content_len| offset < content_len)
            })
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn finish_side_download_request(
        &mut self,
        request: CacheRestartRequest,
        completed: bool,
    ) {
        if let Some(index) = self
            .side_download_active
            .iter()
            .position(|active| *active == request)
        {
            self.side_download_active.remove(index);
        }
        if completed {
            self.schedule_playback_continuation_after_side_download(request);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn schedule_playback_continuation_after_side_download(
        &mut self,
        request: CacheRestartRequest,
    ) {
        if request.range_kind != HttpCacheRangeKind::Playback {
            return;
        }
        let Some(continuation_offset) = self
            .retained_ranges
            .iter()
            .find(|range| {
                range.range_kind == HttpCacheRangeKind::Playback
                    && request.offset >= range.base_offset
                    && request.offset < range.next_offset
                    && self.reader_offset >= range.base_offset
                    && self.reader_offset < range.next_offset
            })
            .map(|range| range.next_offset)
        else {
            return;
        };
        if self
            .content_len
            .is_some_and(|content_len| continuation_offset >= content_len)
        {
            self.eof = true;
            return;
        }
        if self.stale_playback_continuation_offset(continuation_offset) {
            tracing::debug!(
                request_offset = request.offset,
                continuation_offset,
                active_base_offset = self.base_offset,
                active_next_offset = self.next_offset,
                reader_offset = self.reader_offset,
                "skipping stale HTTP active playback continuation after side range"
            );
            return;
        }
        if self.offset_in_active_range(continuation_offset)
            || self
                .restart_request
                .is_some_and(|pending| pending.offset == continuation_offset)
        {
            return;
        }
        tracing::debug!(
            request_offset = request.offset,
            continuation_offset,
            reader_offset = self.reader_offset,
            "scheduling HTTP active playback continuation after side range"
        );
        self.restart_request = Some(CacheRestartRequest {
            offset: continuation_offset,
            range_kind: HttpCacheRangeKind::Playback,
        });
        self.eof = false;
        self.prefetch_paused = false;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn offset_in_active_range(
        &self,
        offset: u64,
    ) -> bool {
        offset >= self.base_offset && offset <= self.next_offset
    }

    pub(in crate::player::backend::ffmpeg) fn is_tail_metadata_probe_seek(
        &self,
        offset: u64,
    ) -> bool {
        let Some(content_len) = self.content_len else {
            return false;
        };
        if offset >= content_len
            || offset < content_len.saturating_sub(self.config.range_request_bytes)
        {
            return false;
        }

        let active_range_near_offset = self.active_range_kind == HttpCacheRangeKind::Playback
            && self.buffer.len() > 0
            && offset
                <= self
                    .next_offset
                    .saturating_add(self.config.range_request_bytes)
            && self.base_offset <= offset.saturating_add(self.config.range_request_bytes);
        !active_range_near_offset
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn buffered_ahead_from(&self, offset: u64) -> u64 {
        if self.active_range_kind != HttpCacheRangeKind::Playback
            || self.buffer.len() == 0
            || offset < self.base_offset
            || offset > self.next_offset
        {
            return 0;
        }
        self.next_offset.saturating_sub(offset)
    }
}
