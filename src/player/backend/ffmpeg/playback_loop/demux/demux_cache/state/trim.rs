use std::{
    collections::VecDeque,
    os::raw::c_int,
    time::{Duration, Instant},
};

use super::{
    ArchivedStreamPruneCandidate, CachedDemuxPacket, DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL,
    DEMUX_PACKET_APPEND_TRIM_INTERVAL, DEMUX_PACKET_APPEND_TRIM_MAX_OVERRUN_BYTES,
    DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT, DEMUX_PACKET_APPEND_TRIM_TIME_BUDGET,
    DEMUX_PACKET_READ_TRIM_INTERVAL, DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_BYTES,
    DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL, DEMUX_PACKET_READ_TRIM_STEP_LIMIT,
    DEMUX_PACKET_READ_TRIM_TIME_BUDGET, DEMUX_PACKET_TRIM_INLINE_GLOBAL_SCAN_LIMIT,
    DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP, DEMUX_STREAM_PACKET_QUEUE_LIMIT,
    DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT, DemuxCachedRange, DemuxPacketCacheState,
    DemuxPacketTrimOutcome, PacketId, RangeId, StreamCacheKind,
};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_after_read_needed(
        &self,
    ) -> bool {
        self.backbuffer_pressure()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn memory_pressure(
        &self,
    ) -> bool {
        // Match mpv's demuxer-max-bytes semantics: this limit applies to the
        // forward packet window. Retained/donated backbuffer bytes are governed
        // separately by effective_backbuffer_limit().
        self.memory_limit_bytes > 0 && self.forward_bytes() >= self.memory_limit_bytes
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn backbuffer_pressure(
        &self,
    ) -> bool {
        self.backward_bytes() > self.effective_backbuffer_limit()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_trim_due(
        &mut self,
        force: bool,
    ) -> bool {
        let overrun = self.backbuffer_overrun_bytes();
        if overrun == 0 {
            self.append_trim_pressure_packets = 0;
            self.append_trim_active = false;
            self.append_trim_pending = false;
            return false;
        }
        if self.append_trim_pending {
            return true;
        }

        self.append_trim_pressure_packets = self.append_trim_pressure_packets.saturating_add(1);
        let interval = if self.append_trim_active {
            DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL
        } else {
            DEMUX_PACKET_APPEND_TRIM_INTERVAL
        };
        let enter_hysteresis =
            !self.append_trim_active && overrun >= self.append_trim_overrun_trigger_bytes();
        if force || enter_hysteresis || self.append_trim_pressure_packets >= interval {
            self.append_trim_pressure_packets = 0;
            self.append_trim_active = true;
            self.append_trim_pending = true;
            true
        } else {
            false
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn complete_append_trim(
        &mut self,
    ) {
        self.append_trim_pending = false;
        self.append_trim_pressure_packets = 0;
        self.append_trim_active =
            self.backbuffer_overrun_bytes() > self.append_trim_overrun_low_water_bytes();
    }

    fn backbuffer_overrun_bytes(&self) -> usize {
        self.backward_bytes()
            .saturating_sub(self.effective_backbuffer_limit())
    }

    fn append_trim_overrun_trigger_bytes(&self) -> usize {
        let total_budget = self
            .memory_limit_bytes
            .saturating_add(self.backbuffer_limit_bytes);
        (total_budget / 32).clamp(1, DEMUX_PACKET_APPEND_TRIM_MAX_OVERRUN_BYTES)
    }

    fn append_trim_overrun_low_water_bytes(&self) -> usize {
        self.append_trim_overrun_trigger_bytes() / 4
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_trim_due(
        &mut self,
    ) -> bool {
        if !self.trim_after_read_needed() {
            self.read_trim_pressure_packets = 0;
            return false;
        }

        let read_trim_interval = if self.read_trim_backbuffer_overrun() {
            DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL
        } else {
            DEMUX_PACKET_READ_TRIM_INTERVAL
        };
        self.read_trim_pressure_packets = self.read_trim_pressure_packets.saturating_add(1);
        if self.read_trim_pressure_packets >= read_trim_interval {
            self.read_trim_pressure_packets = 0;
            true
        } else {
            false
        }
    }

    fn read_trim_backbuffer_overrun(&self) -> bool {
        let overrun = self
            .backward_bytes()
            .saturating_sub(self.effective_backbuffer_limit());
        if overrun == 0 {
            return false;
        }
        let slack = (self.memory_limit_bytes / 16)
            .clamp(64 * 1024, DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_BYTES);
        overrun > slack
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_packet_queue_limit(
        &self,
        stream_index: c_int,
    ) -> usize {
        match self
            .stream_kinds
            .get(&stream_index)
            .copied()
            .unwrap_or(StreamCacheKind::Unknown)
        {
            StreamCacheKind::Subtitle => DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT,
            StreamCacheKind::Video | StreamCacheKind::Audio | StreamCacheKind::Unknown => {
                DEMUX_STREAM_PACKET_QUEUE_LIMIT
            }
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_packet_queue_full(
        &self,
    ) -> bool {
        let active_append_range = self.append_range_id == self.read_range_id;
        self.append_range()
            .stream_queues
            .iter()
            .any(|(stream_index, queue)| {
                let queued_packets = if active_append_range {
                    self.forward_streams
                        .get(stream_index)
                        .map(|state| state.packet_count)
                        .unwrap_or_default()
                } else {
                    queue
                        .iter()
                        .filter(|packet_id| self.packets.contains_key(packet_id))
                        .count()
                };
                queued_packets >= self.stream_packet_queue_limit(*stream_index)
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit(
        &mut self,
    ) -> bool {
        self.trim_to_limit_with_budget(None, None).performed
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit_for_append(
        &mut self,
    ) -> bool {
        self.trim_to_limit_for_append_with_outcome().performed
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit_for_append_with_outcome(
        &mut self,
    ) -> DemuxPacketTrimOutcome {
        self.trim_to_limit_with_budget(
            Some(DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT),
            Some(DEMUX_PACKET_APPEND_TRIM_TIME_BUDGET),
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit_for_read_with_outcome(
        &mut self,
    ) -> DemuxPacketTrimOutcome {
        self.trim_to_limit_with_budget(
            Some(DEMUX_PACKET_READ_TRIM_STEP_LIMIT),
            Some(DEMUX_PACKET_READ_TRIM_TIME_BUDGET),
        )
    }

    fn trim_to_limit_with_budget(
        &mut self,
        max_steps: Option<usize>,
        time_budget: Option<Duration>,
    ) -> DemuxPacketTrimOutcome {
        let started_at = Instant::now();
        let mut outcome = DemuxPacketTrimOutcome {
            global_order_len_before: self.global_order_entry_count(),
            ..DemuxPacketTrimOutcome::default()
        };
        while self.backward_bytes() > self.effective_backbuffer_limit() {
            if max_steps.is_some_and(|limit| outcome.steps >= limit) {
                break;
            }
            if time_budget.is_some_and(|budget| started_at.elapsed() >= budget) {
                outcome.budget_exhausted = true;
                break;
            }
            let packets_before = self.packets.len();
            let bytes_before = self.cached_bytes;
            let order_entries_before = self.global_order_entry_count();
            let pruned = self.prune_oldest_backbuffer_range() || self.prune_active_stream_prefix();
            if !pruned {
                break;
            }
            outcome.performed = true;
            outcome.steps = outcome.steps.saturating_add(1);
            outcome.removed_packets = outcome
                .removed_packets
                .saturating_add(packets_before.saturating_sub(self.packets.len()));
            outcome.removed_bytes = outcome
                .removed_bytes
                .saturating_add(bytes_before.saturating_sub(self.cached_bytes));
            outcome.compacted_global_entries = outcome.compacted_global_entries.saturating_add(
                order_entries_before.saturating_sub(self.global_order_entry_count()),
            );
        }
        outcome.global_order_len_after = self.global_order_entry_count();
        outcome.remaining_overrun_bytes = self.backbuffer_overrun_bytes();
        if outcome.remaining_overrun_bytes > 0
            && time_budget.is_some_and(|budget| started_at.elapsed() >= budget)
        {
            outcome.budget_exhausted = true;
        }
        outcome
    }

    fn global_order_entry_count(&self) -> usize {
        self.ranges
            .values()
            .map(|range| range.global_order.len())
            .fold(0usize, usize::saturating_add)
    }

    fn prune_active_stream_prefix(&mut self) -> bool {
        let Some(candidate) = self.active_stream_prune_candidate() else {
            return false;
        };
        let stream_index = candidate.stream_index;
        let range_id = self.read_range_id;
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return false;
        };
        self.remove_range_stream_prefix_packets(&mut range, stream_index, candidate.prune_count);
        self.ranges.insert(range_id, range);
        true
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn remove_read_range_stream_prefix_packets_for_test(
        &mut self,
        stream_index: c_int,
        count: usize,
    ) {
        let range_id = self.read_range_id;
        let mut range = self
            .ranges
            .remove(&range_id)
            .expect("read range exists for prefix prune test");
        self.remove_range_stream_prefix_packets(&mut range, stream_index, count);
        self.ranges.insert(range_id, range);
    }

    fn active_stream_prune_candidate(&self) -> Option<ArchivedStreamPruneCandidate> {
        let range = self.read_range();
        let candidates = range
            .stream_queues
            .iter()
            .filter(|(stream_index, queue)| {
                queue.front().is_some_and(|packet_id| {
                    Some(*packet_id) != self.next_packet_id_for_stream(**stream_index)
                })
            })
            .filter_map(|(stream_index, queue)| {
                let prune_count = self.active_stream_prefix_prune_count(*stream_index)?;
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(range, *stream_index);
                let prune_always = self.backbuffer_limit_bytes == 0
                    || seek_start_nsecs.is_none()
                    || head_packet.is_none_or(|packet| {
                        !self.packet_is_stream_seek_boundary(*stream_index, packet)
                    });
                Some(ArchivedStreamPruneCandidate {
                    stream_index: *stream_index,
                    prune_always,
                    seek_start_nsecs,
                    prune_count,
                })
            })
            .collect::<Vec<_>>();
        self.preferred_stream_prune_candidate(range, &candidates)
    }

    fn active_stream_prefix_prune_count(&self, stream_index: c_int) -> Option<usize> {
        let range = self.read_range();
        let queue = range.stream_queues.get(&stream_index)?;
        let reader_head = self.next_packet_id_for_stream(stream_index);
        let prefix_len = reader_head
            .map(|packet_id| packet_id_lower_bound(queue, packet_id))
            .unwrap_or(queue.len());
        let boundary_ids = self
            .stream_seek_boundary_ids(range, stream_index)
            .take_while(|packet_id| reader_head.is_none_or(|head| *packet_id < head))
            .take(2)
            .collect::<Vec<_>>();
        let prune_count = indexed_stream_prefix_prune_count(
            queue,
            prefix_len,
            &boundary_ids,
            self.backbuffer_limit_bytes > 0,
            true,
        )?;
        let prune_count = self.bounded_stream_prefix_prune_count(
            range,
            stream_index,
            prune_count,
            prefix_len,
            true,
        );
        self.limit_eager_side_stream_prune_count(range, stream_index, prune_count)
    }

    fn prune_oldest_backbuffer_range(&mut self) -> bool {
        let detached_append_range_id = self.detached_append_range_id();
        let Some(range_id) = self
            .ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .min_by_key(|(_, range)| range.last_used_generation)
            .map(|(range_id, _)| *range_id)
        else {
            return false;
        };
        if self.prune_archived_stream_prefix(range_id) {
            return true;
        }
        let Some(range) = self.ranges.remove(&range_id) else {
            return false;
        };
        self.remove_range_packets(range);
        true
    }

    fn prune_archived_stream_prefix(&mut self, range_id: RangeId) -> bool {
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return false;
        };
        let Some(candidate) = self.archived_stream_prune_candidate(&range) else {
            self.ranges.insert(range_id, range);
            return false;
        };
        let stream_index = candidate.stream_index;
        self.remove_range_stream_prefix_packets(&mut range, stream_index, candidate.prune_count);
        if range.global_order.is_empty() {
            return true;
        }
        self.ranges.insert(range_id, range);
        true
    }

    fn archived_stream_prune_candidate(
        &self,
        range: &DemuxCachedRange,
    ) -> Option<ArchivedStreamPruneCandidate> {
        let candidates = range
            .stream_queues
            .iter()
            .filter_map(|(stream_index, queue)| {
                let prune_count = self.archived_stream_prefix_prune_count(range, *stream_index)?;
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(range, *stream_index);
                let prune_always = self.backbuffer_limit_bytes == 0
                    || seek_start_nsecs.is_none()
                    || head_packet.is_none_or(|packet| {
                        !self.packet_is_stream_seek_boundary(*stream_index, packet)
                    });
                if head_packet.is_none() && queue.is_empty() {
                    return None;
                }
                Some(ArchivedStreamPruneCandidate {
                    stream_index: *stream_index,
                    prune_always,
                    seek_start_nsecs,
                    prune_count,
                })
            })
            .collect::<Vec<_>>();
        self.preferred_stream_prune_candidate(range, &candidates)
    }

    fn stream_queue_seek_start_nsecs(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
    ) -> Option<u64> {
        self.stream_seek_boundary_ids(range, stream_index)
            .find_map(|packet_id| {
                let packet = self.packets.get(&packet_id)?;
                let start_nsecs = packet.start_nsecs?;
                let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
                (end_nsecs >= start_nsecs).then(|| {
                    if stream_index == self.timeline_anchor_stream_index {
                        start_nsecs.saturating_add(self.cached_seek_preroll_nsecs)
                    } else {
                        start_nsecs
                    }
                })
            })
    }

    fn archived_stream_prefix_prune_count(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
    ) -> Option<usize> {
        let queue = range.stream_queues.get(&stream_index)?;
        let boundary_ids = self
            .stream_seek_boundary_ids(range, stream_index)
            .take(2)
            .collect::<Vec<_>>();
        let prune_count = indexed_stream_prefix_prune_count(
            queue,
            queue.len(),
            &boundary_ids,
            self.backbuffer_limit_bytes > 0,
            false,
        )?;
        let prune_count = self.bounded_stream_prefix_prune_count(
            range,
            stream_index,
            prune_count,
            queue.len(),
            false,
        );
        self.limit_eager_side_stream_prune_count(range, stream_index, prune_count)
    }

    fn stream_seek_boundary_ids<'a>(
        &'a self,
        range: &'a DemuxCachedRange,
        stream_index: c_int,
    ) -> impl Iterator<Item = PacketId> + 'a {
        let queue_start = range
            .stream_queues
            .get(&stream_index)
            .and_then(|queue| queue.front())
            .copied();
        let queue_end = range
            .stream_queues
            .get(&stream_index)
            .and_then(|queue| queue.back())
            .copied();
        range
            .stream_seek_boundaries
            .get(&stream_index)
            .into_iter()
            .flat_map(|boundaries| boundaries.iter().copied())
            .filter(move |packet_id| {
                queue_start.is_some_and(|start| *packet_id >= start)
                    && queue_end.is_some_and(|end| *packet_id <= end)
                    && self.packet_readable_in_current_generation(*packet_id)
            })
    }

    fn preferred_stream_prune_candidate(
        &self,
        range: &DemuxCachedRange,
        candidates: &[ArchivedStreamPruneCandidate],
    ) -> Option<ArchivedStreamPruneCandidate> {
        candidates
            .iter()
            .copied()
            .filter(|candidate| self.stream_prune_candidate_keeps_anchor_order(range, *candidate))
            .min_by(|left, right| self.compare_stream_prune_candidates(left, right))
            .or_else(|| {
                candidates
                    .iter()
                    .copied()
                    .min_by(|left, right| self.compare_stream_prune_candidates(left, right))
            })
    }

    fn compare_stream_prune_candidates(
        &self,
        left: &ArchivedStreamPruneCandidate,
        right: &ArchivedStreamPruneCandidate,
    ) -> std::cmp::Ordering {
        match (left.prune_always, right.prune_always) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (true, true) => left.stream_index.cmp(&right.stream_index),
            (false, false) => left
                .seek_start_nsecs
                .cmp(&right.seek_start_nsecs)
                .then_with(|| left.stream_index.cmp(&right.stream_index)),
        }
    }

    fn stream_prune_candidate_keeps_anchor_order(
        &self,
        range: &DemuxCachedRange,
        candidate: ArchivedStreamPruneCandidate,
    ) -> bool {
        if candidate.stream_index == self.timeline_anchor_stream_index {
            return true;
        }
        if !matches!(
            self.stream_kinds.get(&candidate.stream_index),
            Some(StreamCacheKind::Audio)
        ) {
            return true;
        }
        let Some(anchor_seek_start) = range
            .stream_boundary(self.timeline_anchor_stream_index)
            .seek_start_nsecs
        else {
            return true;
        };
        let Some(candidate_seek_start) = candidate.seek_start_nsecs else {
            return true;
        };
        if candidate_seek_start < anchor_seek_start {
            return true;
        }
        self.stream_has_prunable_prefix(range, self.timeline_anchor_stream_index)
            .is_none()
    }

    fn stream_has_prunable_prefix(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
    ) -> Option<()> {
        let queue = range.stream_queues.get(&stream_index)?;
        let front = queue.front()?;
        if range.id == self.read_range_id
            && Some(*front) == self.next_packet_id_for_stream(stream_index)
        {
            return None;
        }
        Some(())
    }

    fn limit_eager_side_stream_prune_count(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
        prune_count: usize,
    ) -> Option<usize> {
        if stream_index == self.timeline_anchor_stream_index
            || !matches!(
                self.stream_kinds.get(&stream_index),
                Some(StreamCacheKind::Audio)
            )
            || self.backbuffer_limit_bytes == 0
        {
            return Some(prune_count);
        }
        let anchor_seek_start = range
            .stream_boundary(self.timeline_anchor_stream_index)
            .seek_start_nsecs?;
        let queue = range.stream_queues.get(&stream_index)?;
        if self.stream_requires_recovery_point(stream_index) {
            // Never let side-stream alignment move a TrueHD/MLP trim point off
            // the major-sync index chosen by the mpv-style boundary scan.
            return self
                .stream_seek_boundary_ids(range, stream_index)
                .map(|packet_id| packet_id_lower_bound(queue, packet_id))
                .filter(|position| *position > 0 && *position <= prune_count)
                .filter(|position| {
                    queue
                        .get(*position)
                        .and_then(|packet_id| self.packets.get(packet_id))
                        .and_then(|packet| packet.start_nsecs)
                        .is_some_and(|next_start| next_start <= anchor_seek_start)
                })
                .last();
        }
        for count in (1..=prune_count).rev() {
            let Some(next_packet_id) = queue.get(count) else {
                continue;
            };
            let Some(next_start) = self
                .packets
                .get(next_packet_id)
                .and_then(|packet| packet.start_nsecs)
            else {
                continue;
            };
            if next_start <= anchor_seek_start {
                return Some(count);
            }
        }
        None
    }

    fn bounded_stream_prefix_prune_count(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
        prune_count: usize,
        maximum_prune_count: usize,
        preserve_last_boundary: bool,
    ) -> usize {
        if self.stream_requires_recovery_point(stream_index) {
            // A recovery block is atomic for seekability, like mpv's keyframe run.
            return prune_count;
        }
        if range.global_order.len() <= DEMUX_PACKET_TRIM_INLINE_GLOBAL_SCAN_LIMIT {
            return prune_count;
        }
        let Some(queue) = range.stream_queues.get(&stream_index) else {
            return prune_count;
        };
        let maximum_prune_count = maximum_prune_count.min(queue.len());
        let bounded_limit = maximum_prune_count.min(DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP);
        let bounded_boundary = self
            .stream_seek_boundary_ids(range, stream_index)
            .map(|packet_id| packet_id_lower_bound(queue, packet_id))
            .take_while(|position| *position <= bounded_limit)
            .filter(|position| *position > 0 && *position <= maximum_prune_count)
            .last();
        match bounded_boundary {
            Some(position) => position.max(prune_count.min(bounded_limit)),
            None if preserve_last_boundary => prune_count,
            None => prune_count.min(DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP),
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_is_stream_seek_boundary(
        &self,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
    ) -> bool {
        Self::packet_is_stream_seek_boundary_for(
            self.timeline_anchor_stream_index,
            stream_index,
            packet,
            self.cached_seek_requires_safe_point,
            self.stream_requires_recovery_point(stream_index),
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_is_stream_seek_boundary_for(
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
        require_safe_seek_point: bool,
        require_recovery_point: bool,
    ) -> bool {
        if stream_index == timeline_anchor_stream_index {
            let seek_boundary = if require_safe_seek_point {
                packet.safe_seek_point
            } else {
                packet.recovery_point
            };
            return packet.timeline_anchor && seek_boundary && packet.start_nsecs.is_some();
        }
        if require_recovery_point {
            return packet.recovery_point && packet.start_nsecs.is_some();
        }
        packet.start_nsecs.is_some()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn rebuild_range_stream_seek_boundaries(
        &mut self,
        range_id: super::RangeId,
        stream_index: c_int,
    ) {
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return;
        };
        let boundaries = range
            .stream_queues
            .get(&stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter().copied())
            .filter(|packet_id| {
                self.packets
                    .get(packet_id)
                    .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet))
            })
            .collect::<VecDeque<_>>();
        if boundaries.is_empty() {
            range.stream_seek_boundaries.remove(&stream_index);
        } else {
            range
                .stream_seek_boundaries
                .insert(stream_index, boundaries);
        }
        self.refresh_range_stream_seek_boundary_in_range(&mut range, stream_index);
        self.ranges.insert(range_id, range);
    }

    fn remove_range_stream_prefix_packets(
        &mut self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
        count: usize,
    ) {
        let old_seek_start_nsecs = range.stream_boundary(stream_index).seek_start_nsecs;
        let removed = {
            let Some(queue) = range.stream_queues.get_mut(&stream_index) else {
                return;
            };
            let count = count.min(queue.len());
            let removed = queue.drain(..count).collect::<Vec<_>>();
            if queue.is_empty() {
                range.stream_queues.remove(&stream_index);
            }
            removed
        };
        if removed.is_empty() {
            return;
        }
        let removed_packet_count = removed.len();
        if let Some(last_removed_packet_id) = removed.last().copied()
            && let Some(boundaries) = range.stream_seek_boundaries.get_mut(&stream_index)
        {
            while boundaries
                .front()
                .is_some_and(|packet_id| *packet_id <= last_removed_packet_id)
            {
                boundaries.pop_front();
            }
            if boundaries.is_empty() {
                range.stream_seek_boundaries.remove(&stream_index);
            }
        }
        range.mark_seekable_dirty();
        let pruned_until_nsecs = removed
            .iter()
            .filter_map(|packet_id| {
                self.packets
                    .get(packet_id)
                    .and_then(|packet| packet.end_nsecs.or(packet.start_nsecs))
            })
            .max();
        range.is_bof = false;
        {
            let boundary = range.ensure_stream_boundary(stream_index);
            boundary.is_bof = false;
            boundary.pruned_packet_count = boundary
                .pruned_packet_count
                .saturating_add(u64::try_from(removed.len()).unwrap_or(u64::MAX));
            let last_pruned_nsecs = old_seek_start_nsecs.or(pruned_until_nsecs);
            if let Some(last_pruned_nsecs) = last_pruned_nsecs {
                boundary.last_pruned_nsecs = Some(
                    boundary
                        .last_pruned_nsecs
                        .unwrap_or(last_pruned_nsecs)
                        .max(last_pruned_nsecs),
                );
            }
        }
        for packet_id in removed {
            self.consumed_packet_ids.remove(&packet_id);
            self.low_level_append_blocked_packet_generations
                .remove(&packet_id);
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
                range.subtract_report_bytes(packet.byte_len);
            }
        }
        if range.global_order.len() <= DEMUX_PACKET_TRIM_INLINE_GLOBAL_SCAN_LIMIT {
            if range.id == self.read_range_id {
                self.adjust_reader_head_positions_before_inline_compaction(range);
            }
            range
                .global_order
                .retain(|packet_id| self.packets.contains_key(packet_id));
        }
        self.compact_range_global_order_front(range);
        if range.id == self.read_range_id && range.global_order.is_empty() {
            self.clear_reader_tracking();
        }
        self.refresh_range_stream_seek_boundary_after_prefix_prune(
            range,
            stream_index,
            removed_packet_count,
        );
    }

    fn adjust_reader_head_positions_before_inline_compaction(&mut self, range: &DemuxCachedRange) {
        if self.reader_head_positions.is_empty() {
            return;
        }
        let reader_head_positions = self.reader_head_positions.clone();
        for (stream_index, head_position) in reader_head_positions {
            let removed_before = range
                .global_order
                .iter()
                .take(head_position)
                .filter(|packet_id| !self.packets.contains_key(packet_id))
                .count();
            if let Some(position) = self.reader_head_positions.get_mut(&stream_index) {
                *position = position.saturating_sub(removed_before);
            }
        }
        self.refresh_read_index_from_reader_head_positions();
    }

    fn compact_range_global_order_front(&mut self, range: &mut DemuxCachedRange) {
        let mut compacted = 0usize;
        while range
            .global_order
            .front()
            .is_some_and(|packet_id| !self.packets.contains_key(packet_id))
        {
            range.global_order.pop_front();
            compacted = compacted.saturating_add(1);
        }
        if compacted == 0 || range.id != self.read_range_id {
            return;
        }
        for position in self.reader_head_positions.values_mut() {
            *position = position.saturating_sub(compacted);
        }
        self.refresh_read_index_from_reader_head_positions();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn backward_bytes(
        &self,
    ) -> usize {
        self.cached_bytes.saturating_sub(self.forward_bytes())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn effective_backbuffer_limit(
        &self,
    ) -> usize {
        if self.backbuffer_limit_bytes == 0 {
            return 0;
        }
        if !self.donate_backbuffer {
            return self.backbuffer_limit_bytes;
        }
        let forward_bytes = self.forward_bytes();
        let Some(forward_with_guard) = forward_bytes.checked_add(1) else {
            return self.backbuffer_limit_bytes;
        };
        if self.memory_limit_bytes <= forward_with_guard {
            return self.backbuffer_limit_bytes;
        }
        self.backbuffer_limit_bytes
            .saturating_add(self.memory_limit_bytes - forward_with_guard)
    }
}

fn packet_id_lower_bound(queue: &VecDeque<PacketId>, packet_id: PacketId) -> usize {
    let mut left = 0usize;
    let mut right = queue.len();
    while left < right {
        let middle = left + (right - left) / 2;
        if queue
            .get(middle)
            .is_some_and(|candidate| *candidate < packet_id)
        {
            left = middle.saturating_add(1);
        } else {
            right = middle;
        }
    }
    left
}

fn indexed_stream_prefix_prune_count(
    queue: &VecDeque<PacketId>,
    prefix_len: usize,
    boundary_ids: &[PacketId],
    seekable_cache: bool,
    preserve_last_boundary: bool,
) -> Option<usize> {
    let prefix_len = prefix_len.min(queue.len());
    if prefix_len == 0 {
        return None;
    }
    if !seekable_cache {
        return Some(prefix_len);
    }

    let boundary_positions = boundary_ids
        .iter()
        .copied()
        .map(|packet_id| packet_id_lower_bound(queue, packet_id))
        .filter(|position| *position < prefix_len)
        .collect::<Vec<_>>();
    let starts_with_boundary = boundary_positions.first().copied() == Some(0);
    let retained_boundary_position = if starts_with_boundary {
        boundary_positions.get(1).copied()
    } else {
        boundary_positions.first().copied()
    };
    let mut prune_count = retained_boundary_position.unwrap_or(prefix_len);

    if preserve_last_boundary
        && let Some(last_boundary_position) = boundary_positions.last().copied()
        && prune_count > last_boundary_position
    {
        prune_count = last_boundary_position;
    }

    (prune_count > 0).then_some(prune_count)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::indexed_stream_prefix_prune_count;

    #[test]
    fn seekable_prefix_prune_count_keeps_next_seek_boundary() {
        assert_eq!(
            indexed_stream_prefix_prune_count(
                &VecDeque::from([10, 20, 30, 40]),
                4,
                &[10, 30],
                true,
                false,
            ),
            Some(2)
        );
    }

    #[test]
    fn seekable_prefix_prune_count_stops_before_first_boundary_after_delta_head() {
        assert_eq!(
            indexed_stream_prefix_prune_count(
                &VecDeque::from([10, 20, 30, 40]),
                4,
                &[30],
                true,
                false,
            ),
            Some(2)
        );
    }

    #[test]
    fn active_prefix_prune_count_preserves_only_covering_boundary() {
        assert_eq!(
            indexed_stream_prefix_prune_count(
                &VecDeque::from([10, 20, 30, 40]),
                4,
                &[10],
                true,
                true,
            ),
            None
        );
    }

    #[test]
    fn non_seekable_prefix_prune_count_prunes_all_available_packets() {
        assert_eq!(
            indexed_stream_prefix_prune_count(
                &VecDeque::from([10, 20, 30, 40]),
                4,
                &[20, 40],
                false,
                false,
            ),
            Some(4)
        );
        assert_eq!(
            indexed_stream_prefix_prune_count(&VecDeque::new(), 0, &[], false, false,),
            None
        );
    }
}
