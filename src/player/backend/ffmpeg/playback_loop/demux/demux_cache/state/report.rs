use std::{
    collections::{BTreeMap, VecDeque},
    os::raw::c_int,
    time::Instant,
};

#[cfg(test)]
use super::PlaybackCacheState;
use super::{
    DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL, DemuxCacheReportSnapshot, DemuxCacheState,
    DemuxCachedRange, DemuxPacketCacheState, PacketId, PlaybackCacheTimeRange,
    SeekableTimelineSummary, StreamCacheKind, StreamCacheRangeState, StreamCacheState,
    StreamForwardWindow, nsecs_to_seconds, optional_buffered_value_changed,
    ordered_duration_seconds,
};

impl DemuxPacketCacheState {
    fn timeline_anchor_packet_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.read_range()
            .stream_queues
            .get(&self.timeline_anchor_stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter().copied())
            .filter(|packet_id| !self.packet_blocked_for_current_generation(*packet_id))
            .filter(|packet_id| {
                self.packets
                    .get(packet_id)
                    .is_some_and(|packet| packet.timeline_anchor && packet.start_nsecs.is_some())
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_timeline_range(
        &self,
    ) -> Option<(u64, u64)> {
        let mut first_cached_nsecs = None;
        let mut buffered_until_nsecs = None;
        for packet_id in self.timeline_anchor_packet_ids() {
            let packet = self.packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            first_cached_nsecs = Some(first_cached_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            buffered_until_nsecs = Some(buffered_until_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        first_cached_nsecs.zip(buffered_until_nsecs)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn playback_cache_state(
        &self,
        paused_for_cache: bool,
    ) -> PlaybackCacheState {
        self.cache_report_snapshot(paused_for_cache)
            .into_cache_state()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_report_snapshot(
        &self,
        paused_for_cache: bool,
    ) -> DemuxCacheReportSnapshot {
        let forward_window = self.selected_forward_timeline_window();
        let cached_until_nsecs = if forward_window.is_none() {
            self.cached_until_nsecs()
        } else {
            None
        };
        let forward_bytes = self.forward_bytes();
        let cache_end = forward_window
            .map(|window| window.end_nsecs)
            .or(cached_until_nsecs)
            .map(nsecs_to_seconds);
        let reader_pts = Some(nsecs_to_seconds(
            forward_window
                .map(|window| window.reader_nsecs)
                .unwrap_or(self.reader_nsecs),
        ));
        let cache_duration = forward_window
            .map(|window| nsecs_to_seconds(window.duration_nsecs()))
            .or_else(|| ordered_duration_seconds(Some(self.reader_nsecs), cached_until_nsecs));
        let seekable_report = self.seekable_time_ranges_report();

        DemuxCacheReportSnapshot {
            session_id: self.session_id,
            demux: DemuxCacheState {
                cache_end,
                reader_pts,
                cache_duration,
                eof: self.effective_eof(),
                underrun: self.has_demux_underrun(),
                idle: self.effective_eof() || self.should_pause_demux(),
                seeking: self.seeking || self.seek_request.is_some(),
                bof_cached: seekable_report.bof_cached,
                eof_cached: seekable_report.eof_cached,
                total_bytes: u64::try_from(self.cached_bytes).unwrap_or(u64::MAX),
                forward_bytes: u64::try_from(forward_bytes).unwrap_or(u64::MAX),
                file_cache_bytes: self.disk_cache.as_ref().map(|cache| cache.next_offset),
                raw_input_rate: self.raw_input_rate(),
                ts_last: self.demux_ts_nsecs.map(nsecs_to_seconds),
                cached_seeks: self.cached_seeks,
                low_level_seeks: self.low_level_seeks,
                byte_level_seeks: 0,
                seekable_ranges: seekable_report.ranges,
                streams: self.stream_cache_states_with_forward_bytes(forward_bytes),
            },
            paused_for_cache,
            buffering_percent: self.cache_buffering_percent,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn take_buffered_changed_for_cache_end(
        &mut self,
        buffered_until: Option<f64>,
    ) -> Option<Option<f64>> {
        let changed = self
            .last_reported_buffered_until
            .map(|previous| optional_buffered_value_changed(previous, buffered_until))
            .unwrap_or(buffered_until.is_some());
        if !changed {
            return None;
        }
        self.last_reported_buffered_until = Some(buffered_until);
        Some(buffered_until)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_state_report_due(
        &self,
        now: Instant,
    ) -> bool {
        self.last_cache_state_emit_at
            .and_then(|last| now.checked_duration_since(last))
            .is_none_or(|elapsed| elapsed >= DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_cache_state_emit_dirty(
        &mut self,
    ) {
        self.cache_state_emit_dirty = true;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_state_emit_dirty(
        &self,
    ) -> bool {
        self.cache_state_emit_dirty
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_state_emit_ready(
        &self,
        now: Instant,
    ) -> bool {
        self.cache_state_emit_dirty
            && (self.last_cache_state_emit_at.is_none() || self.cache_state_report_due(now))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn clear_cache_state_emit_dirty(
        &mut self,
    ) {
        self.cache_state_emit_dirty = false;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_cache_state_emit(
        &mut self,
        now: Instant,
    ) {
        self.last_cache_state_emit_at = Some(now);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_emitted_cache_state(
        &mut self,
        cache_state: &PlaybackCacheState,
    ) {
        self.record_emitted_seekable_ranges(cache_state.demux.seekable_ranges.clone());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_emitted_seekable_ranges(
        &mut self,
        seekable_ranges: Vec<PlaybackCacheTimeRange>,
    ) {
        self.last_emitted_seekable_ranges = Some(seekable_ranges);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_ranges_changed_since_last_emit(
        &self,
    ) -> bool {
        self.last_emitted_seekable_ranges
            .as_ref()
            .is_some_and(|ranges| ranges != &self.seekable_time_ranges())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn forward_bytes(
        &self,
    ) -> usize {
        self.reader_forward_bytes.saturating_add(
            self.detached_append_range()
                .map(|range| {
                    if self.range_has_current_generation_blocked_packet(range) {
                        self.range_bytes(range)
                    } else {
                        range.report_bytes()
                    }
                })
                .unwrap_or_default(),
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reader_forward_bytes(
        &self,
    ) -> usize {
        self.reader_forward_bytes
    }

    fn range_bytes(&self, range: &DemuxCachedRange) -> usize {
        range
            .global_order
            .iter()
            .filter(|packet_id| !self.packet_blocked_for_current_generation(**packet_id))
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn range_has_current_generation_blocked_packet(
        &self,
        range: &DemuxCachedRange,
    ) -> bool {
        if self.low_level_append_blocked_packet_generations.is_empty() {
            return false;
        }
        range.global_order.iter().any(|packet_id| {
            self.low_level_append_blocked_packet_generations
                .get(packet_id)
                .is_some_and(|generation| *generation == self.generation)
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn has_demux_underrun(
        &self,
    ) -> bool {
        if self.effective_eof() || self.should_pause_demux() {
            return false;
        }
        let detached_has_packets = self
            .detached_append_range()
            .is_some_and(|range| !range.global_order.is_empty());
        if self.read_index >= self.read_range().global_order.len() && !detached_has_packets {
            return true;
        }
        self.active_stream_forward_windows()
            .into_iter()
            .any(|window| self.stream_window_underrun(window))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn selected_forward_timeline_window(
        &self,
    ) -> Option<StreamForwardWindow> {
        let mut windows = self.active_stream_forward_windows();
        if windows.is_empty() {
            return None;
        }
        let has_non_subtitle = windows
            .iter()
            .any(|window| !matches!(window.kind, StreamCacheKind::Subtitle));
        windows
            .drain(..)
            .filter(|window| {
                !(has_non_subtitle
                    && matches!(window.kind, StreamCacheKind::Subtitle)
                    && window.duration_nsecs() == 0)
            })
            .min_by_key(|window| window.duration_nsecs())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_window_idle(
        &self,
        window: StreamForwardWindow,
    ) -> bool {
        self.stream_window_idle_with_forward_bytes(window, self.forward_bytes())
    }

    fn stream_window_idle_with_forward_bytes(
        &self,
        window: StreamForwardWindow,
        forward_bytes: usize,
    ) -> bool {
        if self.effective_eof() || self.demux_position_detached {
            return true;
        }
        if self.stream_window_needs_reader_packet(window) {
            return false;
        }
        if self.memory_limit_bytes > 0 && forward_bytes >= self.memory_limit_bytes {
            return true;
        }
        let forward_duration = window.duration_nsecs();
        if forward_duration >= self.readahead_nsecs {
            return true;
        }
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        self.hysteresis_active && self.hysteresis_nsecs > 0 && forward_duration > resume_threshold
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_window_underrun(
        &self,
        window: StreamForwardWindow,
    ) -> bool {
        self.stream_window_underrun_with_forward_bytes(window, self.forward_bytes())
    }

    fn stream_window_underrun_with_forward_bytes(
        &self,
        window: StreamForwardWindow,
        forward_bytes: usize,
    ) -> bool {
        self.stream_window_needs_reader_packet(window)
            && !self.stream_window_idle_with_forward_bytes(window, forward_bytes)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_window_needs_reader_packet(
        &self,
        window: StreamForwardWindow,
    ) -> bool {
        !self.effective_eof()
            && !self.demux_position_detached
            && matches!(window.kind, StreamCacheKind::Video | StreamCacheKind::Audio)
            && !window.has_forward_packet
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn active_stream_forward_windows(
        &self,
    ) -> Vec<StreamForwardWindow> {
        self.stream_forward_windows(true)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reader_stream_forward_windows(
        &self,
    ) -> Vec<StreamForwardWindow> {
        self.stream_forward_windows(false)
    }

    fn stream_forward_windows(&self, include_detached: bool) -> Vec<StreamForwardWindow> {
        let detached_append_range = include_detached
            .then(|| self.detached_append_range())
            .flatten();

        let mut windows = Vec::new();
        for (stream_index, kind) in &self.stream_kinds {
            let mut state = self
                .forward_streams
                .get(stream_index)
                .copied()
                .unwrap_or_default();
            if let Some(queue) =
                detached_append_range.and_then(|range| range.stream_queues.get(stream_index))
            {
                for packet_id in queue {
                    if self.packet_blocked_for_current_generation(*packet_id) {
                        continue;
                    }
                    let Some(packet) = self.packets.get(packet_id) else {
                        continue;
                    };
                    state.push_packet(packet);
                }
            }

            match state.reader_nsecs.zip(state.end_nsecs) {
                Some((reader_nsecs, end_nsecs)) => windows.push(StreamForwardWindow {
                    kind: *kind,
                    reader_nsecs,
                    end_nsecs,
                    has_forward_packet: state.packet_count > 0,
                }),
                None if state.packet_count > 0 || !self.read_range_eof() => {
                    windows.push(StreamForwardWindow {
                        kind: *kind,
                        reader_nsecs: self.reader_nsecs,
                        end_nsecs: self.reader_nsecs,
                        has_forward_packet: state.packet_count > 0,
                    })
                }
                None => {}
            }
        }
        windows
    }

    fn seekable_time_ranges(&self) -> Vec<PlaybackCacheTimeRange> {
        self.seekable_time_ranges_report().ranges
    }

    fn seekable_time_ranges_report(&self) -> SeekableTimeRangesReport {
        let mut ranges = Vec::new();
        let mut bof_cached = false;
        let mut eof_cached = false;
        self.collect_seekable_time_ranges_report(
            self.read_range(),
            &mut ranges,
            &mut bof_cached,
            &mut eof_cached,
        );
        if let Some(range) = self.detached_append_range() {
            self.collect_seekable_time_ranges_report(
                range,
                &mut ranges,
                &mut bof_cached,
                &mut eof_cached,
            );
        }
        let detached_append_range_id = self.detached_append_range_id();
        for (range_id, range) in &self.ranges {
            if *range_id == self.read_range_id {
                continue;
            }
            if Some(*range_id) == detached_append_range_id {
                continue;
            }
            self.collect_seekable_time_ranges_report(
                range,
                &mut ranges,
                &mut bof_cached,
                &mut eof_cached,
            );
        }
        ranges.sort_by(|left, right| {
            left.start
                .partial_cmp(&right.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let ranges = ranges
            .into_iter()
            .filter(|range| range.end >= range.start)
            .collect::<Vec<_>>();
        SeekableTimeRangesReport {
            ranges,
            bof_cached,
            eof_cached,
        }
    }

    fn collect_seekable_time_ranges_report(
        &self,
        range: &DemuxCachedRange,
        ranges: &mut Vec<PlaybackCacheTimeRange>,
        bof_cached: &mut bool,
        eof_cached: &mut bool,
    ) {
        let summary = self.range_seekable_timeline_summary(range);
        if summary.ranges.is_empty() {
            return;
        }
        *bof_cached |= summary.is_bof;
        *eof_cached |= summary.is_eof;
        for (start, end) in summary.ranges {
            ranges.push(PlaybackCacheTimeRange {
                start: nsecs_to_seconds(start),
                end: nsecs_to_seconds(end),
            });
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn range_seekable_timeline_summary(
        &self,
        range: &DemuxCachedRange,
    ) -> SeekableTimelineSummary {
        if let Some(summary) = range.cached_seekable_summary(self.generation) {
            return summary;
        }
        let summary = if self.range_has_current_generation_blocked_packet(range) {
            let (_, stream_queues) = self.current_generation_range_view(range);
            self.compute_range_seekable_timeline_summary(range, &stream_queues)
        } else {
            self.compute_range_seekable_timeline_summary(range, &range.stream_queues)
        };
        range.store_seekable_summary(self.generation, summary.clone());
        summary
    }

    fn compute_range_seekable_timeline_summary(
        &self,
        range: &DemuxCachedRange,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
    ) -> SeekableTimelineSummary {
        let timeline_boundary = range.stream_boundary(self.timeline_anchor_stream_index);
        let Some((mut start, mut end)) = Self::seekable_timeline_range_in_packet_range(
            &self.packets,
            self.timeline_anchor_stream_index,
            self.cached_seek_preroll_nsecs,
            self.cached_seek_requires_safe_point,
            stream_queues,
            timeline_boundary.is_eof,
        ) else {
            return SeekableTimelineSummary::default();
        };

        let mut audio_ranges = Vec::new();
        let mut is_bof = timeline_boundary.is_bof;
        let mut is_eof = timeline_boundary.is_eof;
        for (stream_index, kind) in &self.stream_kinds {
            if *stream_index == self.timeline_anchor_stream_index {
                continue;
            }
            if !matches!(kind, StreamCacheKind::Audio) {
                continue;
            }
            let boundary = range.stream_boundary(*stream_index);
            is_bof &= boundary.is_bof;
            is_eof &= boundary.is_eof;
            match stream_queues
                .get(stream_index)
                .and_then(|queue| Self::stream_seek_range_in_packet_queue(&self.packets, queue))
            {
                Some(stream_range) => audio_ranges.push((boundary, stream_range)),
                None if boundary.is_eof => {}
                None => return SeekableTimelineSummary::default(),
            }
        }
        if !audio_ranges.is_empty() {
            for (boundary, (audio_start, audio_end)) in &audio_ranges {
                start = if is_bof && boundary.is_bof {
                    start.min(*audio_start)
                } else if !boundary.is_bof {
                    start.max(*audio_start)
                } else {
                    start
                };
                end = if is_eof && boundary.is_eof {
                    end.max(*audio_end)
                } else if !boundary.is_eof {
                    end.min(*audio_end)
                } else {
                    end
                };
            }
            if end <= start {
                return SeekableTimelineSummary::default();
            }
        }

        if let Some(pruned_until_nsecs) = range
            .sparse_stream_pruned_until_nsecs
            .values()
            .copied()
            .max()
        {
            start = start.max(pruned_until_nsecs);
        }

        if end <= start {
            return SeekableTimelineSummary::default();
        }

        SeekableTimelineSummary {
            ranges: vec![(start, end)],
            is_bof,
            is_eof,
        }
    }

    fn stream_cache_states_with_forward_bytes(
        &self,
        forward_bytes: usize,
    ) -> Vec<StreamCacheState> {
        let mut streams: BTreeMap<c_int, StreamCacheRangeState> = BTreeMap::new();
        for (stream_index, state) in &self.forward_streams {
            streams.insert(
                *stream_index,
                StreamCacheRangeState {
                    reader_nsecs: state.reader_nsecs,
                    cache_end_nsecs: state.end_nsecs,
                    has_forward_packet: state.packet_count > 0,
                },
            );
        }
        if let Some(range) = self.detached_append_range() {
            self.collect_stream_cache_ranges(&range.stream_queues, &mut streams, |_| true);
        }
        if !self.read_range_eof() {
            for stream_index in self.stream_kinds.keys() {
                streams
                    .entry(*stream_index)
                    .or_insert(StreamCacheRangeState {
                        reader_nsecs: Some(self.reader_nsecs),
                        cache_end_nsecs: Some(self.reader_nsecs),
                        has_forward_packet: false,
                    });
            }
        }
        streams
            .into_iter()
            .map(|(stream_index, state)| {
                let reader_pts = state.reader_nsecs.map(nsecs_to_seconds);
                let cache_end = state.cache_end_nsecs.map(nsecs_to_seconds);
                let kind = self
                    .stream_kinds
                    .get(&stream_index)
                    .copied()
                    .unwrap_or(StreamCacheKind::Unknown);
                let reader_nsecs = state.reader_nsecs.unwrap_or(self.reader_nsecs);
                let end_nsecs = state.cache_end_nsecs.unwrap_or(reader_nsecs);
                let window = StreamForwardWindow {
                    kind,
                    reader_nsecs,
                    end_nsecs,
                    has_forward_packet: state.has_forward_packet,
                };
                StreamCacheState {
                    kind,
                    cache_end,
                    reader_pts,
                    cache_duration: ordered_duration_seconds(
                        state.reader_nsecs,
                        state.cache_end_nsecs,
                    ),
                    underrun: self.stream_window_underrun_with_forward_bytes(window, forward_bytes),
                    idle: self.stream_window_idle_with_forward_bytes(window, forward_bytes),
                }
            })
            .collect()
    }

    fn collect_stream_cache_ranges(
        &self,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
        streams: &mut BTreeMap<c_int, StreamCacheRangeState>,
        mut include_packet: impl FnMut(PacketId) -> bool,
    ) {
        for (stream_index, queue) in stream_queues {
            for packet_id in queue {
                if !include_packet(*packet_id) {
                    continue;
                }
                let Some(packet) = self.packets.get(packet_id) else {
                    continue;
                };
                let entry = streams.entry(*stream_index).or_default();
                entry.has_forward_packet = true;
                if let Some(start_nsecs) = packet.start_nsecs {
                    entry.reader_nsecs =
                        Some(entry.reader_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
                }
                if let Some(end_nsecs) = packet.end_nsecs.or(packet.start_nsecs) {
                    entry.cache_end_nsecs =
                        Some(entry.cache_end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
                }
            }
        }
    }
}

struct SeekableTimeRangesReport {
    ranges: Vec<PlaybackCacheTimeRange>,
    bof_cached: bool,
    eof_cached: bool,
}
