use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    os::raw::c_int,
};

use super::{
    ArchivedStreamPruneCandidate, CachedDemuxPacket, DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT,
    DEMUX_PACKET_READ_TRIM_INTERVAL, DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_BYTES,
    DEMUX_PACKET_READ_TRIM_STEP_LIMIT, DEMUX_STREAM_PACKET_QUEUE_LIMIT,
    DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT, DemuxCachedRange, DemuxPacketCacheState, PacketId, RangeId,
    StreamCacheKind,
};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_after_read_needed(
        &self,
    ) -> bool {
        self.backbuffer_limit_bytes == 0
            || self.backward_bytes() > self.effective_backbuffer_limit()
            || (self.memory_limit_bytes > 0 && self.cached_bytes > self.memory_limit_bytes)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn memory_pressure(
        &self,
    ) -> bool {
        self.memory_limit_bytes > 0 && self.cached_bytes > self.memory_limit_bytes
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn backbuffer_pressure(
        &self,
    ) -> bool {
        self.backward_bytes() > self.effective_backbuffer_limit()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_trim_due(
        &mut self,
    ) -> bool {
        if !self.trim_after_read_needed() {
            self.read_trim_pressure_packets = 0;
            return false;
        }

        self.read_trim_pressure_packets = self.read_trim_pressure_packets.saturating_add(1);
        if self.read_trim_memory_overrun()
            || self.read_trim_pressure_packets >= DEMUX_PACKET_READ_TRIM_INTERVAL
        {
            self.read_trim_pressure_packets = 0;
            true
        } else {
            false
        }
    }

    fn read_trim_memory_overrun(&self) -> bool {
        if self.memory_limit_bytes == 0 || self.cached_bytes <= self.memory_limit_bytes {
            return false;
        }
        let slack = (self.memory_limit_bytes / 16)
            .max(64 * 1024)
            .min(DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_BYTES);
        self.cached_bytes > self.memory_limit_bytes.saturating_add(slack)
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
        self.trim_to_limit_with_step_limit(None)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit_for_append(
        &mut self,
    ) -> bool {
        self.trim_to_limit_with_step_limit(Some(DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn trim_to_limit_for_read(
        &mut self,
    ) -> bool {
        self.trim_to_limit_with_step_limit(Some(DEMUX_PACKET_READ_TRIM_STEP_LIMIT))
    }

    fn trim_to_limit_with_step_limit(&mut self, max_steps: Option<usize>) -> bool {
        let mut pruned = false;
        let mut steps = 0usize;
        while self.backward_bytes() > self.effective_backbuffer_limit() {
            if max_steps.is_some_and(|limit| steps >= limit) {
                break;
            }
            if self.prune_oldest_backbuffer_range() {
                pruned = true;
                steps = steps.saturating_add(1);
                continue;
            }
            if self.prune_active_stream_prefix() {
                pruned = true;
                steps = steps.saturating_add(1);
                continue;
            }
            break;
        }
        pruned
    }

    fn prune_active_stream_prefix(&mut self) -> bool {
        let Some(candidate) = self.active_stream_prune_candidate() else {
            return false;
        };
        let stream_index = candidate.stream_index;
        let Some(prune_count) = self.active_stream_prefix_prune_count(stream_index) else {
            return false;
        };
        if prune_count == 0 {
            return false;
        }
        let range_id = self.read_range_id;
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return false;
        };
        self.remove_range_stream_prefix_packets(&mut range, stream_index, prune_count);
        self.ranges.insert(range_id, range);
        true
    }

    fn active_stream_prune_candidate(&self) -> Option<ArchivedStreamPruneCandidate> {
        self.read_range()
            .stream_queues
            .iter()
            .filter(|(stream_index, queue)| {
                queue.front().is_some_and(|packet_id| {
                    Some(*packet_id) != self.reader_heads.get(stream_index).copied()
                })
            })
            .map(|(stream_index, queue)| {
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(*stream_index, queue);
                let prune_always = self.backbuffer_limit_bytes == 0
                    || seek_start_nsecs.is_none()
                    || head_packet.is_none_or(|packet| {
                        !self.packet_is_stream_seek_boundary(*stream_index, packet)
                    });
                ArchivedStreamPruneCandidate {
                    stream_index: *stream_index,
                    prune_always,
                    seek_start_nsecs,
                }
            })
            .min_by(
                |left, right| match (left.prune_always, right.prune_always) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    (true, true) => left.stream_index.cmp(&right.stream_index),
                    (false, false) => left
                        .seek_start_nsecs
                        .cmp(&right.seek_start_nsecs)
                        .then_with(|| left.stream_index.cmp(&right.stream_index)),
                },
            )
    }

    fn active_stream_prefix_prune_count(&self, stream_index: c_int) -> Option<usize> {
        let queue = self.read_range().stream_queues.get(&stream_index)?;
        let reader_head = self.reader_heads.get(&stream_index).copied();
        let boundaries = queue
            .iter()
            .take_while(|packet_id| Some(**packet_id) != reader_head)
            .map(|packet_id| {
                self.packets
                    .get(packet_id)
                    .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet))
            });
        stream_prefix_prune_count_from_boundaries(boundaries, self.backbuffer_limit_bytes > 0)
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
        let Some(prune_count) = self.archived_stream_prefix_prune_count(&range, stream_index)
        else {
            self.ranges.insert(range_id, range);
            return false;
        };
        if prune_count == 0 {
            self.ranges.insert(range_id, range);
            return false;
        }
        self.remove_range_stream_prefix_packets(&mut range, stream_index, prune_count);
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
        range
            .stream_queues
            .iter()
            .filter_map(|(stream_index, queue)| {
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(*stream_index, queue);
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
                })
            })
            .min_by(
                |left, right| match (left.prune_always, right.prune_always) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    (true, true) => left.stream_index.cmp(&right.stream_index),
                    (false, false) => left
                        .seek_start_nsecs
                        .cmp(&right.seek_start_nsecs)
                        .then_with(|| left.stream_index.cmp(&right.stream_index)),
                },
            )
    }

    fn stream_queue_seek_start_nsecs(
        &self,
        stream_index: c_int,
        queue: &VecDeque<PacketId>,
    ) -> Option<u64> {
        if stream_index == self.timeline_anchor_stream_index {
            let mut stream_queues = BTreeMap::new();
            stream_queues.insert(stream_index, queue.clone());
            return Self::seekable_timeline_ranges_in_packet_range(
                &self.packets,
                self.timeline_anchor_stream_index,
                0,
                &stream_queues,
                false,
            )
            .first()
            .map(|(start_nsecs, _)| *start_nsecs);
        }

        queue.iter().find_map(|packet_id| {
            let packet = self.packets.get(packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            (end_nsecs >= start_nsecs).then_some(start_nsecs)
        })
    }

    fn archived_stream_prefix_prune_count(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
    ) -> Option<usize> {
        let queue = range.stream_queues.get(&stream_index)?;
        let boundaries = queue.iter().map(|packet_id| {
            self.packets
                .get(packet_id)
                .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet))
        });
        stream_prefix_prune_count_from_boundaries(boundaries, self.backbuffer_limit_bytes > 0)
    }

    fn packet_is_stream_seek_boundary(
        &self,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
    ) -> bool {
        Self::packet_is_stream_seek_boundary_for(
            self.timeline_anchor_stream_index,
            stream_index,
            packet,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_is_stream_seek_boundary_for(
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
    ) -> bool {
        if stream_index == timeline_anchor_stream_index {
            return packet.timeline_anchor && packet.recovery_point && packet.start_nsecs.is_some();
        }
        packet.start_nsecs.is_some()
    }

    fn remove_range_stream_prefix_packets(
        &mut self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
        count: usize,
    ) {
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
        let sparse_pruned_until_nsecs = matches!(
            self.stream_kinds.get(&stream_index),
            Some(StreamCacheKind::Subtitle)
        )
        .then(|| {
            removed
                .iter()
                .filter_map(|packet_id| {
                    self.packets
                        .get(packet_id)
                        .and_then(|packet| packet.end_nsecs.or(packet.start_nsecs))
                })
                .max()
        })
        .flatten();
        range.is_bof = false;
        range.ensure_stream_boundary(stream_index).is_bof = false;
        if let Some(pruned_until_nsecs) = sparse_pruned_until_nsecs {
            range
                .sparse_stream_pruned_until_nsecs
                .entry(stream_index)
                .and_modify(|existing| *existing = (*existing).max(pruned_until_nsecs))
                .or_insert(pruned_until_nsecs);
        }
        let removed_packet_ids = removed.iter().copied().collect::<HashSet<_>>();
        if range.id == self.read_range_id {
            self.adjust_reader_head_positions_after_prune(range, &removed_packet_ids);
        }
        range
            .global_order
            .retain(|packet_id| !removed_packet_ids.contains(packet_id));
        if range.id == self.read_range_id && range.global_order.is_empty() {
            self.clear_reader_tracking();
        }
        for packet_id in removed {
            self.consumed_packet_ids.remove(&packet_id);
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            }
        }
    }

    fn adjust_reader_head_positions_after_prune(
        &mut self,
        range: &DemuxCachedRange,
        removed_packet_ids: &HashSet<PacketId>,
    ) {
        if removed_packet_ids.is_empty() || self.reader_head_positions.is_empty() {
            return;
        }

        let reader_head_positions = self.reader_head_positions.clone();
        let mut removed_before_head = BTreeMap::new();
        for (position, packet_id) in range.global_order.iter().copied().enumerate() {
            if !removed_packet_ids.contains(&packet_id) {
                continue;
            }
            for (stream_index, head_position) in &reader_head_positions {
                if position < *head_position {
                    *removed_before_head.entry(*stream_index).or_insert(0usize) += 1;
                }
            }
        }

        for (stream_index, removed_before) in removed_before_head {
            if let Some(position) = self.reader_head_positions.get_mut(&stream_index) {
                *position = position.saturating_sub(removed_before);
            }
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

fn stream_prefix_prune_count_from_boundaries<I>(
    boundaries: I,
    seekable_cache: bool,
) -> Option<usize>
where
    I: IntoIterator<Item = bool>,
{
    let mut boundaries = boundaries.into_iter();
    let first_is_boundary = boundaries.next()?;
    let starts_with_non_boundary = !first_is_boundary;
    let mut boundary_was_pruned = false;
    let mut prune_count = 0;

    for is_boundary in std::iter::once(first_is_boundary).chain(boundaries) {
        if is_boundary {
            if seekable_cache && (boundary_was_pruned || starts_with_non_boundary) {
                break;
            }
            boundary_was_pruned = true;
        }
        prune_count += 1;
    }

    (prune_count > 0).then_some(prune_count)
}

#[cfg(test)]
mod tests {
    use super::stream_prefix_prune_count_from_boundaries;

    #[test]
    fn seekable_prefix_prune_count_keeps_next_seek_boundary() {
        assert_eq!(
            stream_prefix_prune_count_from_boundaries([true, false, true, false], true),
            Some(2)
        );
    }

    #[test]
    fn seekable_prefix_prune_count_stops_before_first_boundary_after_delta_head() {
        assert_eq!(
            stream_prefix_prune_count_from_boundaries([false, false, true, false], true),
            Some(2)
        );
    }

    #[test]
    fn non_seekable_prefix_prune_count_prunes_all_available_packets() {
        assert_eq!(
            stream_prefix_prune_count_from_boundaries([false, true, false, true], false),
            Some(4)
        );
        assert_eq!(
            stream_prefix_prune_count_from_boundaries(std::iter::empty(), false),
            None
        );
    }
}
