use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    os::raw::c_int,
};

use super::{DemuxCachedRange, DemuxPacketCacheState, RangeId};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn next_range_id(
        &mut self,
    ) -> RangeId {
        let range_id = self.next_range_id;
        self.next_range_id = self.next_range_id.saturating_add(1);
        range_id
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_range(
        &self,
    ) -> &DemuxCachedRange {
        self.ranges
            .get(&self.read_range_id)
            .expect("FFmpeg demux packet cache read range missing")
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_range_mut(
        &mut self,
    ) -> &mut DemuxCachedRange {
        self.ranges
            .get_mut(&self.read_range_id)
            .expect("FFmpeg demux packet cache read range missing")
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_range(
        &self,
    ) -> &DemuxCachedRange {
        self.ranges
            .get(&self.append_range_id)
            .expect("FFmpeg demux packet cache append range missing")
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_range_mut(
        &mut self,
    ) -> &mut DemuxCachedRange {
        self.ranges
            .get_mut(&self.append_range_id)
            .expect("FFmpeg demux packet cache append range missing")
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_range_eof(
        &self,
    ) -> bool {
        self.read_range().is_eof
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn current_generation_range_view(
        &self,
        range: &DemuxCachedRange,
    ) -> (VecDeque<u64>, BTreeMap<c_int, VecDeque<u64>>) {
        self.range_view_excluding_blocked_generation(range, self.generation)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn next_generation_range_view(
        &self,
        range: &DemuxCachedRange,
    ) -> (VecDeque<u64>, BTreeMap<c_int, VecDeque<u64>>) {
        self.range_view_excluding_blocked_generation(range, self.generation.saturating_add(1))
    }

    fn range_view_excluding_blocked_generation(
        &self,
        range: &DemuxCachedRange,
        generation: u64,
    ) -> (VecDeque<u64>, BTreeMap<c_int, VecDeque<u64>>) {
        let global_order = range
            .global_order
            .iter()
            .copied()
            .filter(|packet_id| self.packets.contains_key(packet_id))
            .filter(|packet_id| {
                self.low_level_append_blocked_packet_generations
                    .get(packet_id)
                    .is_none_or(|blocked_generation| *blocked_generation != generation)
            })
            .collect::<VecDeque<_>>();
        let stream_queues = range
            .stream_queues
            .iter()
            .filter_map(|(stream_index, queue)| {
                let queue = queue
                    .iter()
                    .copied()
                    .filter(|packet_id| {
                        self.low_level_append_blocked_packet_generations
                            .get(packet_id)
                            .is_none_or(|blocked_generation| *blocked_generation != generation)
                    })
                    .collect::<VecDeque<_>>();
                (!queue.is_empty()).then_some((*stream_index, queue))
            })
            .collect::<BTreeMap<_, _>>();
        (global_order, stream_queues)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn detached_append_range_id(
        &self,
    ) -> Option<RangeId> {
        (self.append_range_id != self.read_range_id).then_some(self.append_range_id)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn detached_append_range(
        &self,
    ) -> Option<&DemuxCachedRange> {
        let range_id = self.detached_append_range_id()?;
        self.ranges.get(&range_id)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn start_new_current_range(
        &mut self,
        is_bof: bool,
    ) {
        let range_id = self.next_range_id();
        self.ranges.insert(
            range_id,
            DemuxCachedRange::new(range_id, is_bof, self.generation),
        );
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        self.clear_reader_tracking();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn start_detached_append_range(
        &mut self,
    ) {
        if self.detached_append_range().is_some() {
            return;
        }
        let range_id = self.next_range_id();
        self.append_range_id = range_id;
        self.ranges.insert(
            range_id,
            DemuxCachedRange::new(range_id, false, self.generation),
        );
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn preserve_current_range(
        &mut self,
    ) {
        if self.read_range().global_order.is_empty() {
            self.ranges.remove(&self.read_range_id);
            self.clear_reader_tracking();
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            if let Some(range) = self.ranges.remove(&self.read_range_id) {
                self.remove_range_packets(range);
            }
            self.clear_reader_tracking();
            return;
        }
        let generation = self.generation;
        self.read_range_mut().last_used_generation = generation;
        self.clear_reader_tracking();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn preserve_detached_append_range(
        &mut self,
    ) {
        let Some(range_id) = self.detached_append_range_id() else {
            return;
        };
        let Some(mut range) = self.ranges.remove(&range_id) else {
            self.append_range_id = self.read_range_id;
            return;
        };
        self.append_range_id = self.read_range_id;
        if range.global_order.is_empty() {
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            self.remove_range_packets(range);
        } else {
            range.last_used_generation = self.generation;
            self.ranges.insert(range.id, range);
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn activate_detached_append_range(
        &mut self,
    ) -> bool {
        let Some(range_id) = self.detached_append_range_id() else {
            return false;
        };
        let Some(range) = self.ranges.get(&range_id) else {
            self.append_range_id = self.read_range_id;
            return false;
        };
        if range.global_order.is_empty() && !range.is_eof {
            return false;
        }
        self.preserve_current_range();
        self.activate_range_for_read(range_id, 0);
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn remove_range_packets(
        &mut self,
        range: DemuxCachedRange,
    ) {
        for packet_id in range.global_order {
            self.consumed_packet_ids.remove(&packet_id);
            self.low_level_append_blocked_packet_generations
                .remove(&packet_id);
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_read_stream_bof(
        &mut self,
        stream_index: c_int,
        is_bof: bool,
    ) {
        let range = self.read_range_mut();
        range.ensure_stream_boundary(stream_index).is_bof = is_bof;
        if !is_bof {
            range.is_bof = false;
        }
        range.mark_seekable_dirty();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_range_eof(
        &mut self,
        range_id: RangeId,
        is_eof: bool,
    ) {
        let mut stream_indices = self.stream_kinds.keys().copied().collect::<BTreeSet<_>>();
        if let Some(range) = self.ranges.get(&range_id) {
            stream_indices.extend(range.stream_queues.keys().copied());
        }

        let Some(range) = self.ranges.get_mut(&range_id) else {
            return;
        };
        range.is_eof = is_eof;
        for stream_index in stream_indices {
            range.ensure_stream_boundary(stream_index).is_eof = is_eof;
        }
        range.mark_seekable_dirty();
        self.refresh_range_seek_boundaries(range_id);
        if is_eof {
            let eof_anchor = self.ranges.get(&range_id).and_then(|range| {
                range
                    .stream_queues
                    .get(&self.timeline_anchor_stream_index)
                    .into_iter()
                    .flat_map(|queue| queue.iter().rev().copied())
                    .find_map(|packet_id| {
                        let packet = self.packets.get(&packet_id)?;
                        (packet.timeline_anchor && packet.recovery_point)
                            .then_some((packet_id, packet))
                    })
            });
            if let Some((anchor_packet_id, anchor)) = eof_anchor {
                let boundary = self
                    .ranges
                    .get(&range_id)
                    .map(|range| range.stream_boundary(self.timeline_anchor_stream_index));
                tracing::debug!(
                    session_id = ?self.session_id,
                    range_id,
                    stream_index = self.timeline_anchor_stream_index,
                    closure_reason = "EOF",
                    anchor_packet_id,
                    anchor_kind = anchor.recovery_kind.as_str(),
                    anchor_nsecs = ?anchor.start_nsecs,
                    preroll_nsecs = self.cached_seek_preroll_nsecs,
                    seek_start_nsecs = ?boundary.and_then(|boundary| boundary.seek_start_nsecs),
                    seek_end_nsecs = ?boundary.and_then(|boundary| boundary.seek_end_nsecs),
                    "closed FFmpeg demux cached seek interval"
                );
            }
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn archived_bytes(
        &self,
    ) -> usize {
        let detached_append_range_id = self.detached_append_range_id();
        self.ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .map(|(_, range)| range)
            .flat_map(|range| range.global_order.iter())
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_read_index_for_test(
        &mut self,
        read_index: usize,
    ) {
        self.read_index = read_index;
        self.reset_reader_heads_for_read_index();
    }
}
