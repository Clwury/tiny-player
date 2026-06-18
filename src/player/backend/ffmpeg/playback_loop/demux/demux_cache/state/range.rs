use std::collections::{BTreeMap, VecDeque};

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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn detached_append_range_mut(
        &mut self,
    ) -> Option<&mut DemuxCachedRange> {
        let range_id = self.detached_append_range_id()?;
        self.ranges.get_mut(&range_id)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn start_new_current_range(
        &mut self,
        is_bof: bool,
    ) {
        let range_id = self.next_range_id();
        self.ranges.insert(
            range_id,
            DemuxCachedRange {
                id: range_id,
                global_order: VecDeque::new(),
                stream_queues: BTreeMap::new(),
                sparse_stream_pruned_until_nsecs: BTreeMap::new(),
                is_bof,
                is_eof: false,
                last_used_generation: self.generation,
            },
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
            DemuxCachedRange {
                id: range_id,
                global_order: VecDeque::new(),
                stream_queues: BTreeMap::new(),
                sparse_stream_pruned_until_nsecs: BTreeMap::new(),
                is_bof: false,
                is_eof: false,
                last_used_generation: self.generation,
            },
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
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
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
