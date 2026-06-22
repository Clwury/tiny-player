use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    os::raw::c_int,
    time::Instant,
};

use super::{
    DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL, DemuxCacheState, DemuxCachedRange,
    DemuxPacketCacheState, PacketId, PlaybackCacheState, PlaybackCacheTimeRange,
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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn playback_cache_state(
        &self,
        paused_for_cache: bool,
    ) -> PlaybackCacheState {
        let forward_window = self.selected_forward_timeline_window();
        let cache_end = forward_window
            .map(|window| window.end_nsecs)
            .or_else(|| self.cached_until_nsecs())
            .map(nsecs_to_seconds);
        let reader_pts = Some(nsecs_to_seconds(
            forward_window
                .map(|window| window.reader_nsecs)
                .unwrap_or(self.reader_nsecs),
        ));
        let cache_duration = forward_window
            .map(|window| nsecs_to_seconds(window.duration_nsecs()))
            .or_else(|| {
                ordered_duration_seconds(Some(self.reader_nsecs), self.cached_until_nsecs())
            });
        let seekable_ranges = self.seekable_time_ranges();
        let active_seekable_summary = self.range_seekable_timeline_summary(self.read_range());
        let active_has_seekable_range = !active_seekable_summary.ranges.is_empty();
        let detached_append_range_id = self.detached_append_range_id();
        let detached_seekable_summary = self
            .detached_append_range()
            .map(|range| self.range_seekable_timeline_summary(range));
        let bof_cached = (active_has_seekable_range && active_seekable_summary.is_bof)
            || detached_seekable_summary
                .as_ref()
                .is_some_and(|summary| !summary.ranges.is_empty() && summary.is_bof)
            || self
                .ranges
                .iter()
                .filter(|(range_id, _)| **range_id != self.read_range_id)
                .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
                .any(|(_, range)| {
                    let summary = self.range_seekable_timeline_summary(range);
                    !summary.ranges.is_empty() && summary.is_bof
                });
        let eof_cached = (active_has_seekable_range && active_seekable_summary.is_eof)
            || detached_seekable_summary
                .as_ref()
                .is_some_and(|summary| !summary.ranges.is_empty() && summary.is_eof)
            || self
                .ranges
                .iter()
                .filter(|(range_id, _)| **range_id != self.read_range_id)
                .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
                .any(|(_, range)| {
                    let summary = self.range_seekable_timeline_summary(range);
                    !summary.ranges.is_empty() && summary.is_eof
                });

        PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end,
                reader_pts,
                cache_duration,
                eof: self.effective_eof(),
                underrun: self.has_demux_underrun(),
                idle: self.effective_eof() || self.should_pause_demux(),
                seeking: self.seeking || self.seek_request.is_some(),
                bof_cached,
                eof_cached,
                total_bytes: u64::try_from(self.cached_bytes).unwrap_or(u64::MAX),
                forward_bytes: u64::try_from(self.forward_bytes()).unwrap_or(u64::MAX),
                file_cache_bytes: self.disk_cache.as_ref().map(|cache| cache.next_offset),
                raw_input_rate: self.raw_input_rate(),
                ts_last: self.demux_ts_nsecs.map(nsecs_to_seconds),
                cached_seeks: self.cached_seeks,
                low_level_seeks: self.low_level_seeks,
                byte_level_seeks: 0,
                seekable_ranges,
                streams: self.stream_cache_states(),
            },
            byte: None,
            paused_for_cache,
            buffering_percent: self.cache_buffering_percent,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn take_buffered_changed_for_cache_state(
        &mut self,
        cache_state: &PlaybackCacheState,
    ) -> Option<Option<f64>> {
        let buffered_until = cache_state.demux.cache_end;
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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_cache_state_emit(
        &mut self,
        now: Instant,
    ) {
        self.last_cache_state_emit_at = Some(now);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_emitted_cache_state(
        &mut self,
        cache_state: &PlaybackCacheState,
    ) {
        self.last_emitted_cache_state = Some(cache_state.clone());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_ranges_changed_since_last_emit(
        &self,
    ) -> bool {
        self.last_emitted_cache_state
            .as_ref()
            .is_some_and(|state| state.demux.seekable_ranges != self.seekable_time_ranges())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn forward_bytes(
        &self,
    ) -> usize {
        self.reader_forward_bytes.saturating_add(
            self.detached_append_range()
                .map(|range| self.range_bytes(range))
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
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
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
        if self.effective_eof() || self.demux_position_detached {
            return true;
        }
        if self.stream_window_needs_reader_packet(window) {
            return false;
        }
        if self.memory_limit_bytes > 0 && self.forward_bytes() >= self.memory_limit_bytes {
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
        self.stream_window_needs_reader_packet(window) && !self.stream_window_idle(window)
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
        let mut ranges = Vec::new();
        self.collect_seekable_time_ranges(self.read_range(), &mut ranges);
        if let Some(range) = self.detached_append_range() {
            self.collect_seekable_time_ranges(range, &mut ranges);
        }
        let detached_append_range_id = self.detached_append_range_id();
        for (range_id, range) in &self.ranges {
            if *range_id == self.read_range_id {
                continue;
            }
            if Some(*range_id) == detached_append_range_id {
                continue;
            }
            self.collect_seekable_time_ranges(range, &mut ranges);
        }
        ranges.sort_by(|left, right| {
            left.start
                .partial_cmp(&right.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranges
            .into_iter()
            .filter(|range| range.end >= range.start)
            .collect()
    }

    fn collect_seekable_time_ranges(
        &self,
        range: &DemuxCachedRange,
        ranges: &mut Vec<PlaybackCacheTimeRange>,
    ) {
        for (start, end) in self.range_seekable_timeline_ranges(range) {
            ranges.push(PlaybackCacheTimeRange {
                start: nsecs_to_seconds(start),
                end: nsecs_to_seconds(end),
            });
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn range_seekable_timeline_ranges(
        &self,
        range: &DemuxCachedRange,
    ) -> Vec<(u64, u64)> {
        self.range_seekable_timeline_summary(range).ranges
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn range_seekable_timeline_summary(
        &self,
        range: &DemuxCachedRange,
    ) -> SeekableTimelineSummary {
        let timeline_boundary = range.stream_boundary(self.timeline_anchor_stream_index);
        let Some((mut start, mut end)) = Self::seekable_timeline_range_in_packet_range(
            &self.packets,
            self.timeline_anchor_stream_index,
            0,
            &range.stream_queues,
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
            match range
                .stream_queues
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

    fn stream_cache_states(&self) -> Vec<StreamCacheState> {
        let mut streams: BTreeMap<c_int, StreamCacheRangeState> = BTreeMap::new();
        let read_packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        self.collect_stream_cache_ranges(
            &self.read_range().stream_queues,
            &mut streams,
            |packet_id| {
                read_packet_positions.contains_key(&packet_id)
                    && self.active_packet_is_forward(packet_id)
            },
        );
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
                    underrun: self.stream_window_underrun(window),
                    idle: self.stream_window_idle(window),
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
