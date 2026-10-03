use std::time::{Duration, Instant};

#[cfg(test)]
use super::CacheRestartRequest;
use super::{HttpCacheRangeKind, HttpRingCacheState, SideDownloadError, SideDownloadRequest};

const SIDE_DOWNLOAD_WAIT_LIMIT: Duration = Duration::from_secs(30);
const MAX_SIDE_DOWNLOAD_ERRORS: usize = 16;

impl HttpRingCacheState {
    pub(in crate::backend::ffmpeg::avio::cache) fn begin_metadata_probe(&mut self) {
        self.finish_metadata_probe();
        self.metadata_probe_active = true;
        self.side_download_errors.clear();
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn finish_metadata_probe(&mut self) {
        self.metadata_probe_active = false;
        self.reader_range_kind = HttpCacheRangeKind::Playback;
        self.pending_seek_range_kind = None;
        self.side_download_requests
            .retain(|request| request.range_kind == HttpCacheRangeKind::Playback);
        self.side_download_active
            .retain(|request| request.range_kind == HttpCacheRangeKind::Playback);
        self.side_download_errors
            .retain(|error| error.range_kind == HttpCacheRangeKind::Playback);
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn range_kind_for_seek(
        &self,
        offset: u64,
        from_end: bool,
    ) -> HttpCacheRangeKind {
        if self.metadata_probe_active && (from_end || self.is_tail_metadata_probe_seek(offset)) {
            HttpCacheRangeKind::TailMetadataProbe
        } else {
            HttpCacheRangeKind::Playback
        }
    }

    fn cached_byte_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        std::iter::once((self.base_offset, self.next_offset))
            .chain(
                self.retained_ranges
                    .iter()
                    .map(|range| (range.base_offset, range.next_offset)),
            )
            .chain(
                self.disk_cache
                    .iter()
                    .flat_map(|cache| cache.ranges.iter().map(|range| (range.start, range.end))),
            )
    }

    // Return only the first missing span. A subsequent read can request another
    // gap after consuming the cached or already requested bytes in between.
    pub(in crate::backend::ffmpeg::avio::cache) fn uncached_side_interval(
        &self,
        mut offset: u64,
        end: u64,
    ) -> Option<(u64, u64)> {
        loop {
            let covered_end = self
                .cached_byte_ranges()
                .filter(|&(start, stop)| start <= offset && offset < stop)
                .map(|(_, stop)| stop)
                .max();
            match covered_end {
                Some(stop) => offset = stop,
                None => break,
            }
        }
        let end = self
            .cached_byte_ranges()
            .filter(|&(start, stop)| start > offset && start < stop)
            .map(|(start, _)| start)
            .min()
            .map_or(end, |start| end.min(start));
        (offset < end).then_some((offset, end))
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn queue_side_download(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> bool {
        if self.side_download_error_at(offset).is_some() || self.side_download_may_produce(offset) {
            return false;
        }
        let end = offset.saturating_add(self.side_range_request_bytes(range_kind));
        let end = self.content_len.map_or(end, |len| end.min(len));
        let Some((offset, mut end)) = self.uncached_side_interval(offset, end) else {
            return false;
        };
        // Coverage is independent of request purpose, including an interval
        // that starts after this new request. Never download that overlap again.
        for request in self
            .side_download_requests
            .iter()
            .chain(&self.side_download_active)
        {
            if request.contains(offset) {
                return false;
            }
            if request.offset > offset {
                end = end.min(request.offset);
            }
        }
        for error in &self.side_download_errors {
            if error.start > offset {
                end = end.min(error.start);
            }
        }
        let request = self.new_side_download_request(offset, end, range_kind);
        let insertion = if range_kind == HttpCacheRangeKind::Playback {
            self.side_download_requests
                .iter()
                .position(|request| request.range_kind == HttpCacheRangeKind::TailMetadataProbe)
                .unwrap_or(self.side_download_requests.len())
        } else {
            self.side_download_requests.len()
        };
        tracing::debug!(
            offset,
            end_offset = end,
            ?range_kind,
            request_id = request.id,
            "queueing HTTP side download range"
        );
        self.side_download_requests.insert(insertion, request);
        true
    }

    fn new_side_download_request(
        &mut self,
        offset: u64,
        end_offset: u64,
        range_kind: HttpCacheRangeKind,
    ) -> SideDownloadRequest {
        self.next_side_request_id = self.next_side_request_id.wrapping_add(1);
        SideDownloadRequest {
            id: self.next_side_request_id,
            generation: self.request_generation,
            offset,
            end_offset,
            range_kind,
            deadline: Instant::now() + SIDE_DOWNLOAD_WAIT_LIMIT,
        }
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn side_download_error_at(
        &self,
        offset: u64,
    ) -> Option<&str> {
        self.side_download_errors
            .iter()
            .rev()
            .find(|error| error.start <= offset && offset < error.end)
            .map(|error| error.message.as_str())
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn record_side_download_error(
        &mut self,
        request: SideDownloadRequest,
        offset: u64,
        message: String,
    ) {
        let start = offset.max(request.offset);
        if start >= request.end_offset {
            return;
        }
        self.side_download_errors.push_back(SideDownloadError {
            start,
            end: request.end_offset,
            range_kind: request.range_kind,
            message,
        });
        while self.side_download_errors.len() > MAX_SIDE_DOWNLOAD_ERRORS {
            self.side_download_errors.pop_front();
        }
    }

    fn side_prefetch_allowed(&self) -> bool {
        let low_water = (self.target_readahead_bytes() / 4)
            .min(self.side_range_request_bytes(HttpCacheRangeKind::Playback))
            .max(1);
        self.restart_request.is_none()
            && self.error.is_none()
            && self
                .next_offset
                .saturating_sub(self.reader_offset.max(self.base_offset))
                >= low_water
    }

    // Called during network polling too, so low-priority requests yield while
    // waiting for headers/body, without waiting for a response or retry to end.
    pub(in crate::backend::ffmpeg::avio::cache) fn maintain_side_downloads(
        &mut self,
        now: Instant,
    ) {
        self.side_download_requests
            .retain(|request| request.generation == self.request_generation);
        self.side_download_active
            .retain(|request| request.generation == self.request_generation);
        let expired: Vec<_> = self
            .side_download_requests
            .iter()
            .chain(&self.side_download_active)
            .copied()
            .filter(|request| request.deadline <= now)
            .collect();
        for request in expired {
            self.side_download_requests
                .retain(|queued| *queued != request);
            self.side_download_active
                .retain(|active| *active != request);
            // Already received bytes remain readable before the failed suffix.
            if let Some((offset, _)) =
                self.uncached_side_interval(request.offset, request.end_offset)
            {
                self.record_side_download_error(
                    request,
                    offset,
                    "HTTP 视频缓存辅助读取等待超时".into(),
                );
            }
        }
        let demand = self.side_read_demand;
        let demand_queued = demand.is_some_and(|offset| {
            self.side_download_requests
                .iter()
                .any(|request| request.contains(offset))
        });
        let prefetch_allowed = self.side_prefetch_allowed();
        self.side_download_active.retain(|request| {
            demand.is_some_and(|offset| request.contains(offset))
                || (prefetch_allowed && !demand_queued)
        });
    }

    pub(in crate::backend::ffmpeg::avio::cache) fn take_side_download_request(
        &mut self,
    ) -> Option<SideDownloadRequest> {
        self.maintain_side_downloads(Instant::now());
        let demand = self.side_read_demand;
        let urgent = demand.and_then(|offset| {
            self.side_download_requests
                .iter()
                .position(|request| request.contains(offset))
        });
        let index = if let Some(index) = urgent {
            index
        } else {
            // At most one speculative auxiliary response, and only with enough
            // forward bytes. An AVIO read demand always bypasses this watermark.
            if demand.is_some()
                || !self.side_prefetch_allowed()
                || !self.side_download_active.is_empty()
            {
                return None;
            }
            0
        };
        let mut request = self.side_download_requests.remove(index)?;
        let (offset, end) = self.uncached_side_interval(request.offset, request.end_offset)?;
        request.offset = offset;
        request.end_offset = end;
        self.side_download_active.push(request);
        Some(request)
    }

    #[cfg(test)]
    pub(in crate::backend::ffmpeg::avio::cache) fn activate_side_download_for_test(
        &mut self,
        request: CacheRestartRequest,
    ) -> SideDownloadRequest {
        let end = request
            .offset
            .saturating_add(self.side_range_request_bytes(request.range_kind));
        let end = self.content_len.map_or(end, |len| end.min(len));
        let mut side = self.new_side_download_request(request.offset, end, request.range_kind);
        side.generation = request.generation;
        self.side_read_demand = Some(request.offset);
        self.side_download_active.push(side);
        side
    }
}
