use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    os::raw::c_int,
};

use super::{DemuxCachedRange, DemuxPacketCacheState, RangeId, StreamCacheKind};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn range_can_resume(
        &self,
        range: &DemuxCachedRange,
    ) -> bool {
        self.stream_kinds.keys().all(|stream_index| {
            range
                .stream_resume_positions
                .get(stream_index)
                .is_none_or(|position| position.resumable())
        })
    }

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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_range_eager_streams_exhausted(
        &self,
    ) -> bool {
        // A read may request only a sparse subtitle stream or one A/V stream
        // while the other decoders are backpressured. Its empty queue says
        // nothing about the remaining A/V in this range. Cold disk packets
        // still count, even before the storage worker receives a read request.
        self.stream_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, StreamCacheKind::Video | StreamCacheKind::Audio))
            .all(|(stream_index, _)| self.next_packet_id_for_stream(*stream_index).is_none())
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
                    .is_none_or(|blocked_generation| *blocked_generation > generation)
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
                            .is_none_or(|blocked_generation| *blocked_generation > generation)
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
        self.enforce_cached_range_limit();
    }

    #[cfg(test)]
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
        self.enforce_cached_range_limit();
    }

    /// Keep archived seek ranges bounded. The active read range and a detached
    /// append range are always protected; among the remaining ranges, evict
    /// the least recently used one and prefer empty/short ranges on ties.
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn enforce_cached_range_limit(
        &mut self,
    ) -> usize {
        let limit = self.max_cached_ranges.max(1);
        let mut removed = 0usize;
        while self.ranges.len() > limit {
            let detached = self.detached_append_range_id();
            let candidate = self
                .ranges
                .iter()
                .filter(|(range_id, _)| {
                    **range_id != self.read_range_id && Some(**range_id) != detached
                })
                .min_by(|(_, left), (_, right)| {
                    left.last_used_generation
                        .cmp(&right.last_used_generation)
                        .then_with(|| left.global_order.len().cmp(&right.global_order.len()))
                })
                .map(|(range_id, _)| *range_id);
            let Some(range_id) = candidate else {
                // A limit of one can still temporarily require the active and
                // detached ranges; never drop either while a seek is in flight.
                break;
            };
            if let Some(range) = self.ranges.remove(&range_id) {
                self.remove_range_packets(range);
                removed = removed.saturating_add(1);
                self.bump_seekability_revision();
            }
        }
        removed
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn preserve_current_range(
        &mut self,
    ) {
        if self.read_range().global_order.is_empty() {
            if let Some(range) = self.ranges.remove(&self.read_range_id) {
                self.remove_range_packets(range);
            }
            self.clear_reader_tracking();
            self.enforce_cached_range_limit();
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            if let Some(range) = self.ranges.remove(&self.read_range_id) {
                self.remove_range_packets(range);
            }
            self.clear_reader_tracking();
            self.enforce_cached_range_limit();
            return;
        }
        let generation = self.generation;
        self.read_range_mut().last_used_generation = generation;
        self.clear_reader_tracking();
        self.enforce_cached_range_limit();
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
            self.failed_cached_seek_ranges.remove(&range_id);
            self.rejected_cached_seek_ranges.remove(&range_id);
            self.enforce_cached_range_limit();
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            self.remove_range_packets(range);
        } else {
            range.last_used_generation = self.generation;
            self.ranges.insert(range.id, range);
        }
        self.enforce_cached_range_limit();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn activate_detached_append_range(
        &mut self,
    ) -> bool {
        let Some(range_id) = self.detached_append_range_id() else {
            return false;
        };
        if !self.read_range_eager_streams_exhausted() {
            return false;
        }
        let Some(range) = self.ranges.get(&range_id) else {
            self.append_range_id = self.read_range_id;
            return false;
        };
        if range.global_order.is_empty() && !range.is_eof {
            return false;
        }
        let previous_read_range_id = self.read_range_id;
        self.preserve_current_range();
        self.activate_range_for_read(range_id, 0);
        self.enforce_cached_range_limit();
        tracing::debug!(
            session_id = ?self.session_id,
            previous_read_range_id,
            read_range_id = self.read_range_id,
            reader_nsecs = self.reader_nsecs,
            generation = self.generation,
            "FFmpeg demux packet cache continued into appended range after audio/video exhaustion"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn remove_range_packets(
        &mut self,
        range: DemuxCachedRange,
    ) {
        let range_id = range.id;
        for packet_id in range.global_order {
            self.consumed_packet_ids.remove(&packet_id);
            self.low_level_append_blocked_packet_generations
                .remove(&packet_id);
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.track_packet_storage_remove(packet_id, &packet);
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            }
        }
        self.failed_cached_seek_ranges.remove(&range_id);
        self.rejected_cached_seek_ranges.remove(&range_id);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn discard_ranges_before_track_refresh(
        &mut self,
    ) {
        // Called under the seek commit lock, after request_seek has fenced
        // in-flight demux reads and created an empty range. Like mpv's refresh
        // seek, discard retained packets that predate selecting this subtitle
        // track. Sparse subtitles cannot bound A/V ranges by their cue times.
        debug_assert!(self.read_range().global_order.is_empty());
        debug_assert_eq!(self.read_range_id, self.append_range_id);
        let stale_ranges = self
            .ranges
            .keys()
            .copied()
            .filter(|range_id| *range_id != self.read_range_id)
            .collect::<Vec<_>>();
        for range_id in stale_ranges {
            if let Some(range) = self.ranges.remove(&range_id) {
                self.remove_range_packets(range);
            }
        }
        self.bump_seekability_revision();
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
        self.bump_seekability_revision();
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
        self.bump_seekability_revision();
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
                        (packet.timeline_anchor && packet.is_cached_seek_anchor())
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
                    anchor_kind = anchor.cached_seek_anchor_kind().as_str(),
                    anchor_nsecs = ?anchor.seek_timestamp_nsecs,
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
