use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    os::raw::c_int,
    time::Instant,
};

#[cfg(test)]
use super::PlaybackCacheState;
use super::seek_algorithm::{StreamSeekBlock, VideoSeekBlock};
use super::{
    DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL, DemuxCacheReportSnapshot, DemuxCacheState,
    DemuxCachedRange, DemuxPacketCacheState, PacketId, PlaybackCacheTimeRange,
    SeekableTimelineSummary, StreamCacheKind, StreamCacheRangeState, StreamCacheState,
    StreamForwardWindow, VideoRecoveryPointKind, nsecs_to_seconds, optional_buffered_value_changed,
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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_time_ranges(
        &self,
    ) -> Vec<PlaybackCacheTimeRange> {
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
        let ranges = merge_overlapping_seekable_time_ranges(ranges);
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
        if self.failed_cached_seek_ranges.contains_key(&range.id) {
            return;
        }
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
        let summary = self.compute_range_seekable_timeline_summary(range);
        range.store_seekable_summary(self.generation, summary.clone());
        summary
    }

    fn compute_range_seekable_timeline_summary(
        &self,
        range: &DemuxCachedRange,
    ) -> SeekableTimelineSummary {
        let mut seek_start = None;
        let mut seek_end = None;
        let mut min_bof_start = None;
        let mut max_eof_end = None;
        let mut is_bof = true;
        let mut is_eof = true;
        let mut saw_eager_stream = false;

        for (stream_index, kind) in &self.stream_kinds {
            if !matches!(kind, StreamCacheKind::Video | StreamCacheKind::Audio) {
                continue;
            }
            let boundary = range.stream_boundary(*stream_index);
            is_bof &= boundary.is_bof;
            is_eof &= boundary.is_eof;

            let Some(stream_start) = boundary.seek_start_nsecs else {
                if boundary.is_eof {
                    continue;
                }
                return SeekableTimelineSummary::default();
            };
            let Some(stream_end) = boundary.seek_end_nsecs else {
                if boundary.is_eof {
                    continue;
                }
                return SeekableTimelineSummary::default();
            };
            if stream_end <= stream_start {
                return SeekableTimelineSummary::default();
            }
            saw_eager_stream = true;

            if boundary.is_bof {
                min_bof_start = Some(min_bof_start.unwrap_or(stream_start).min(stream_start));
            } else {
                seek_start = Some(seek_start.unwrap_or(stream_start).max(stream_start));
            }
            if boundary.is_eof {
                max_eof_end = Some(max_eof_end.unwrap_or(stream_end).max(stream_end));
            } else {
                seek_end = Some(seek_end.unwrap_or(stream_end).min(stream_end));
            }
        }

        if !saw_eager_stream {
            return SeekableTimelineSummary::default();
        }
        if is_bof {
            seek_start = min_bof_start;
        }
        if is_eof {
            seek_end = max_eof_end;
        }
        let (Some(mut start), Some(end)) = (seek_start, seek_end) else {
            return SeekableTimelineSummary::default();
        };

        for (stream_index, kind) in &self.stream_kinds {
            if !matches!(kind, StreamCacheKind::Subtitle) {
                continue;
            }
            let boundary = range.stream_boundary(*stream_index);
            if let Some(last_pruned_nsecs) = boundary.last_pruned_nsecs {
                start = start.max(last_pruned_nsecs.saturating_add(100_000_000));
            }
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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_range_stream_seek_boundary(
        &mut self,
        range_id: super::RangeId,
        stream_index: c_int,
    ) {
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return;
        };
        self.refresh_range_stream_seek_boundary_in_range(&mut range, stream_index);
        self.ranges.insert(range_id, range);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_range_stream_seek_boundary_after_append(
        &mut self,
        range_id: super::RangeId,
        stream_index: c_int,
        packet_id: PacketId,
    ) {
        if !self.packet_readable_in_current_generation(packet_id) {
            return;
        }

        if stream_index != self.timeline_anchor_stream_index {
            if self.stream_requires_recovery_point(stream_index) {
                let appended_closes_seek_block = self
                    .packets
                    .get(&packet_id)
                    .is_some_and(|packet| packet.recovery_point && packet.start_nsecs.is_some());
                if !appended_closes_seek_block {
                    return;
                }
                let Some(block) = self.stream_seek_block_closed_by_appended_boundary(
                    range_id,
                    stream_index,
                    packet_id,
                ) else {
                    return;
                };
                let Some(range) = self.ranges.get_mut(&range_id) else {
                    return;
                };
                let boundary = range.ensure_stream_boundary(stream_index);
                Self::close_stream_seek_block(
                    block,
                    &mut boundary.seek_start_nsecs,
                    &mut boundary.seek_end_nsecs,
                );
                range.mark_seekable_dirty();
                return;
            }

            let Some((start_nsecs, end_nsecs)) = self.packets.get(&packet_id).and_then(|packet| {
                let start_nsecs = packet.start_nsecs?;
                let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
                Some((start_nsecs, end_nsecs))
            }) else {
                return;
            };
            let Some(range) = self.ranges.get_mut(&range_id) else {
                return;
            };
            let boundary = range.ensure_stream_boundary(stream_index);
            boundary.seek_start_nsecs = Some(
                boundary
                    .seek_start_nsecs
                    .unwrap_or(start_nsecs)
                    .min(start_nsecs),
            );
            boundary.seek_end_nsecs =
                Some(boundary.seek_end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
            range.mark_seekable_dirty();
            return;
        }

        let appended_closes_seek_block = self.packets.get(&packet_id).is_some_and(|packet| {
            packet.timeline_anchor
                && packet.start_nsecs.is_some()
                && Self::packet_is_cached_seek_anchor(packet)
        });
        if !appended_closes_seek_block {
            return;
        }
        let Some(block) =
            self.video_seek_block_closed_by_appended_anchor(range_id, stream_index, packet_id)
        else {
            return;
        };
        let closing_anchor = self.packets.get(&packet_id).map(|packet| {
            (
                packet.recovery_kind,
                packet.start_nsecs,
                packet.safe_seek_point,
            )
        });
        let cached_seek_preroll_nsecs = self.cached_seek_preroll_nsecs;
        let Some(range) = self.ranges.get_mut(&range_id) else {
            return;
        };
        let (seek_start_nsecs, seek_end_nsecs) = {
            let boundary = range.ensure_stream_boundary(stream_index);
            Self::close_video_seek_block(
                block,
                cached_seek_preroll_nsecs,
                &mut boundary.seek_start_nsecs,
                &mut boundary.seek_end_nsecs,
            );
            (boundary.seek_start_nsecs, boundary.seek_end_nsecs)
        };
        range.mark_seekable_dirty();
        tracing::debug!(
            session_id = ?self.session_id,
            range_id,
            stream_index,
            closure_reason = "next_recovery",
            anchor_packet_id = block.recovery_packet_id,
            anchor_kind = block.recovery_kind.as_str(),
            anchor_nsecs = block.recovery_start_nsecs,
            preroll_nsecs = cached_seek_preroll_nsecs,
            next_anchor_packet_id = packet_id,
            next_anchor_kind = ?closing_anchor.map(|anchor| anchor.0.as_str()),
            next_anchor_nsecs = ?closing_anchor.and_then(|anchor| anchor.1),
            next_anchor_is_safe_seek_point = ?closing_anchor.map(|anchor| anchor.2),
            seek_start_nsecs = ?seek_start_nsecs,
            seek_end_nsecs = ?seek_end_nsecs,
            "closed FFmpeg demux cached seek interval"
        );
    }

    fn stream_seek_block_closed_by_appended_boundary(
        &self,
        range_id: super::RangeId,
        stream_index: c_int,
        packet_id: PacketId,
    ) -> Option<StreamSeekBlock> {
        let queue = self
            .ranges
            .get(&range_id)?
            .stream_queues
            .get(&stream_index)?;
        let mut found_appended_packet = false;
        let mut block_min_nsecs = None;
        let mut block_max_nsecs = None;

        for candidate_id in queue.iter().rev().copied() {
            if !found_appended_packet {
                if candidate_id == packet_id {
                    found_appended_packet = true;
                }
                continue;
            }
            if !self.packet_readable_in_current_generation(candidate_id) {
                continue;
            }
            let Some(packet) = self.packets.get(&candidate_id) else {
                continue;
            };
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            block_min_nsecs = Some(block_min_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            block_max_nsecs = Some(block_max_nsecs.unwrap_or(start_nsecs).max(start_nsecs));
            if packet.recovery_point {
                return Some(StreamSeekBlock {
                    min_nsecs: block_min_nsecs?,
                    max_nsecs: block_max_nsecs?,
                });
            }
        }
        None
    }

    fn video_seek_block_closed_by_appended_anchor(
        &self,
        range_id: super::RangeId,
        stream_index: c_int,
        packet_id: PacketId,
    ) -> Option<VideoSeekBlock> {
        let queue = self
            .ranges
            .get(&range_id)?
            .stream_queues
            .get(&stream_index)?;
        let mut found_appended_packet = false;
        let mut block_min_nsecs = None;
        let mut block_max_nsecs = None;
        let mut recovery_start_nsecs = None;
        let mut recovery_packet_id = None;
        let mut recovery_kind = VideoRecoveryPointKind::None;
        let mut previous_recovery_start_nsecs = None;

        for candidate_id in queue.iter().rev().copied() {
            if !found_appended_packet {
                if candidate_id == packet_id {
                    found_appended_packet = true;
                }
                continue;
            }
            if !self.packet_readable_in_current_generation(candidate_id) {
                continue;
            }
            let Some(packet) = self.packets.get(&candidate_id) else {
                continue;
            };
            if !packet.timeline_anchor {
                continue;
            }
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            let is_recovery = Self::packet_is_cached_seek_anchor(packet);

            if recovery_start_nsecs.is_none() {
                let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
                block_min_nsecs = Some(block_min_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
                block_max_nsecs = Some(block_max_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
                if is_recovery {
                    recovery_start_nsecs = Some(start_nsecs);
                    recovery_packet_id = Some(candidate_id);
                    recovery_kind = packet.recovery_kind;
                }
            } else if is_recovery {
                previous_recovery_start_nsecs = Some(start_nsecs);
                break;
            }
        }

        Some(VideoSeekBlock {
            min_nsecs: block_min_nsecs?,
            max_nsecs: block_max_nsecs?,
            recovery_start_nsecs: recovery_start_nsecs?,
            previous_recovery_start_nsecs,
            recovery_packet_id: recovery_packet_id?,
            recovery_kind,
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_range_stream_seek_boundary_in_range(
        &self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
    ) {
        let queue = range
            .stream_queues
            .get(&stream_index)
            .map(|queue| {
                queue
                    .iter()
                    .copied()
                    .filter(|packet_id| self.packet_readable_in_current_generation(*packet_id))
                    .collect::<VecDeque<_>>()
            })
            .unwrap_or_default();

        let seek_range = if stream_index == self.timeline_anchor_stream_index {
            let mut stream_queues = BTreeMap::new();
            stream_queues.insert(stream_index, queue);
            Self::seekable_timeline_range_in_packet_range(
                &self.packets,
                self.timeline_anchor_stream_index,
                self.cached_seek_preroll_nsecs,
                &stream_queues,
                range.stream_boundary(stream_index).is_eof,
            )
        } else {
            Self::stream_seek_range_in_packet_queue(
                &self.packets,
                &queue,
                self.stream_requires_recovery_point(stream_index),
                range.stream_boundary(stream_index).is_eof,
            )
        };

        let boundary = range.ensure_stream_boundary(stream_index);
        boundary.seek_start_nsecs = seek_range.map(|(start, _)| start);
        boundary.seek_end_nsecs = seek_range.map(|(_, end)| end);
        range.mark_seekable_dirty();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_range_stream_seek_boundary_after_prefix_prune(
        &self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
        pruned_packet_count: usize,
    ) {
        let old_boundary = range.stream_boundary(stream_index);
        let old_seek_start_nsecs = old_boundary.seek_start_nsecs;
        let old_seek_end_nsecs = old_boundary.seek_end_nsecs;
        if !range.stream_queues.contains_key(&stream_index) {
            let boundary = range.ensure_stream_boundary(stream_index);
            boundary.seek_start_nsecs = None;
            boundary.seek_end_nsecs = None;
            range.mark_seekable_dirty();
            tracing::debug!(
                session_id = ?self.session_id,
                range_id = range.id,
                stream_index,
                stream_kind = ?self.stream_kinds.get(&stream_index),
                pruned_packet_count,
                old_seek_start_nsecs = ?old_seek_start_nsecs,
                old_seek_end_nsecs = ?old_seek_end_nsecs,
                new_seek_start_nsecs = ?Option::<u64>::None,
                new_seek_end_nsecs = ?Option::<u64>::None,
                boundary_index_rebuilt = false,
                retained_boundary_samples = ?Vec::<u64>::new(),
                outcome = "queue_empty",
                "updated FFmpeg demux seek boundary after prefix trim"
            );
            return;
        }

        // mpv keeps the already-confirmed seek tail when pruning a queue head:
        // only the first retained seek point changes. Requiring the first local
        // block to close again can transiently erase a valid HEVC range when its
        // first two IRAP points are closer than cached seek preroll.
        let boundary_index_rebuilt =
            self.repair_range_stream_seek_boundary_index(range, stream_index);
        let queue = range
            .stream_queues
            .get(&stream_index)
            .expect("stream queue checked before prefix boundary repair");
        let queue_start = queue.front().copied();
        let queue_end = queue.back().copied();
        let boundary_ids = range
            .stream_seek_boundaries
            .get(&stream_index)
            .into_iter()
            .flat_map(|boundaries| boundaries.iter().copied())
            .filter(|packet_id| {
                queue_start.is_some_and(|start| *packet_id >= start)
                    && queue_end.is_some_and(|end| *packet_id <= end)
                    && self.packet_readable_in_current_generation(*packet_id)
            })
            .take(3)
            .collect::<Vec<_>>();
        let retained_boundary_samples = boundary_ids
            .iter()
            .filter_map(|packet_id| {
                self.packets
                    .get(packet_id)
                    .and_then(|packet| packet.start_nsecs)
            })
            .collect::<Vec<_>>();

        let (seek_range, outcome) = if let Some(old_seek_end_nsecs) = old_seek_end_nsecs {
            let new_seek_start_nsecs = boundary_ids.first().and_then(|packet_id| {
                let start_nsecs = self.packets.get(packet_id)?.start_nsecs?;
                Some(if stream_index == self.timeline_anchor_stream_index {
                    start_nsecs.saturating_add(self.cached_seek_preroll_nsecs)
                } else {
                    start_nsecs
                })
            });
            match new_seek_start_nsecs {
                Some(start_nsecs) if start_nsecs < old_seek_end_nsecs => (
                    Some((start_nsecs, old_seek_end_nsecs)),
                    "preserved_confirmed_end",
                ),
                Some(_) => (None, "retained_boundary_after_confirmed_end"),
                None => (None, "no_retained_boundary"),
            }
        } else {
            self.refresh_range_stream_seek_boundary_in_range(range, stream_index);
            let boundary = range.stream_boundary(stream_index);
            let seek_range = boundary.seek_start_nsecs.zip(boundary.seek_end_nsecs);
            (seek_range, "recomputed_without_confirmed_end")
        };

        let range_id = range.id;
        let (new_seek_start_nsecs, new_seek_end_nsecs) = {
            let boundary = range.ensure_stream_boundary(stream_index);
            boundary.seek_start_nsecs = seek_range.map(|(start, _)| start);
            boundary.seek_end_nsecs = seek_range.map(|(_, end)| end);
            (boundary.seek_start_nsecs, boundary.seek_end_nsecs)
        };
        range.mark_seekable_dirty();
        tracing::debug!(
            session_id = ?self.session_id,
            range_id,
            stream_index,
            stream_kind = ?self.stream_kinds.get(&stream_index),
            pruned_packet_count,
            old_seek_start_nsecs = ?old_seek_start_nsecs,
            old_seek_end_nsecs = ?old_seek_end_nsecs,
            new_seek_start_nsecs = ?new_seek_start_nsecs,
            new_seek_end_nsecs = ?new_seek_end_nsecs,
            boundary_index_rebuilt,
            retained_boundary_samples = ?retained_boundary_samples,
            outcome,
            "updated FFmpeg demux seek boundary after prefix trim"
        );
    }

    fn repair_range_stream_seek_boundary_index(
        &self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
    ) -> bool {
        let Some(queue) = range.stream_queues.get(&stream_index) else {
            return false;
        };
        let queue_start = queue.front().copied();
        let queue_end = queue.back().copied();
        let indexed_first = range
            .stream_seek_boundaries
            .get(&stream_index)
            .into_iter()
            .flat_map(|boundaries| boundaries.iter().copied())
            .find(|packet_id| {
                queue_start.is_some_and(|start| *packet_id >= start)
                    && queue_end.is_some_and(|end| *packet_id <= end)
                    && self.packet_readable_in_current_generation(*packet_id)
            });
        let actual_first = queue.iter().copied().find(|packet_id| {
            self.packets
                .get(packet_id)
                .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet))
                && self.packet_readable_in_current_generation(*packet_id)
        });
        if indexed_first == actual_first {
            return false;
        }

        let rebuilt = queue
            .iter()
            .copied()
            .filter(|packet_id| {
                self.packets
                    .get(packet_id)
                    .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet))
                    && self.packet_readable_in_current_generation(*packet_id)
            })
            .collect::<VecDeque<_>>();
        if rebuilt.is_empty() {
            range.stream_seek_boundaries.remove(&stream_index);
        } else {
            range.stream_seek_boundaries.insert(stream_index, rebuilt);
        }
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_range_seek_boundaries(
        &mut self,
        range_id: super::RangeId,
    ) {
        let Some(range) = self.ranges.get(&range_id) else {
            return;
        };
        let mut stream_indices = self.stream_kinds.keys().copied().collect::<BTreeSet<_>>();
        stream_indices.extend(range.stream_queues.keys().copied());
        for stream_index in stream_indices {
            self.refresh_range_stream_seek_boundary(range_id, stream_index);
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn log_seekable_range_diagnostics(
        &self,
        seekable_ranges: &[PlaybackCacheTimeRange],
        forward_bytes: usize,
    ) {
        let mut per_stream = Vec::new();
        for (range_id, range) in &self.ranges {
            let mut stream_indices = self.stream_kinds.keys().copied().collect::<BTreeSet<_>>();
            stream_indices.extend(range.stream_queues.keys().copied());
            stream_indices.extend(range.stream_boundaries.keys().copied());
            for stream_index in stream_indices {
                let queue = range.stream_queues.get(&stream_index);
                let queue_len = queue.map(VecDeque::len).unwrap_or_default();
                let queue_start_nsecs = queue
                    .and_then(|queue| queue.front())
                    .and_then(|packet_id| self.packets.get(packet_id))
                    .and_then(|packet| packet.start_nsecs);
                let queue_end_nsecs = queue
                    .and_then(|queue| queue.back())
                    .and_then(|packet_id| self.packets.get(packet_id))
                    .and_then(|packet| packet.end_nsecs.or(packet.start_nsecs));
                let boundary = range.stream_boundary(stream_index);
                per_stream.push((
                    *range_id,
                    stream_index,
                    self.stream_kinds.get(&stream_index).copied(),
                    queue_len,
                    queue_start_nsecs,
                    queue_end_nsecs,
                    boundary.seek_start_nsecs,
                    boundary.seek_end_nsecs,
                    boundary.last_pruned_nsecs,
                    boundary.pruned_packet_count,
                    boundary.is_bof,
                    boundary.is_eof,
                ));
            }
        }

        tracing::debug!(
            session_id = ?self.session_id,
            reader_nsecs = self.reader_nsecs,
            seekable_ranges = ?seekable_ranges,
            cached_bytes = self.cached_bytes,
            forward_bytes,
            backward_bytes = self.backward_bytes(),
            effective_backbuffer_limit = self.effective_backbuffer_limit(),
            memory_limit_bytes = self.memory_limit_bytes,
            backbuffer_limit_bytes = self.backbuffer_limit_bytes,
            per_stream = ?per_stream,
            "FFmpeg demux seekable range report diagnostics"
        );
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

fn merge_overlapping_seekable_time_ranges(
    mut ranges: Vec<PlaybackCacheTimeRange>,
) -> Vec<PlaybackCacheTimeRange> {
    ranges.retain(|range| {
        range.start.is_finite() && range.end.is_finite() && range.end >= range.start
    });
    ranges.sort_by(|left, right| {
        left.start
            .total_cmp(&right.start)
            .then_with(|| left.end.total_cmp(&right.end))
    });

    // mpv attempts to join positively-overlapping demux cache ranges before
    // exposing seekable-ranges to the OSC. Tiny keeps the packet ranges
    // independent, so publish the equivalent non-overlapping union while
    // retaining merely adjacent ranges as separate entries.
    let mut merged: Vec<PlaybackCacheTimeRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && range.start < previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        merged.push(range);
    }
    merged
}
