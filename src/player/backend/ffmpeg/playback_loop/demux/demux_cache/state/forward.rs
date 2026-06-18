use std::{collections::BTreeMap, os::raw::c_int};

use super::{DemuxPacketCacheState, PacketId, StreamForwardState};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn active_packet_is_forward(
        &self,
        packet_id: PacketId,
    ) -> bool {
        let Some(packet) = self.packets.get(&packet_id) else {
            return false;
        };
        self.reader_heads.contains_key(&packet.stream_index)
            && !self.consumed_packet_ids.contains(&packet_id)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn clear_reader_tracking(
        &mut self,
    ) {
        self.read_index = 0;
        self.consumed_packet_ids.clear();
        self.reader_heads.clear();
        self.reader_head_positions.clear();
        self.clear_forward_cache();
    }

    fn clear_forward_cache(&mut self) {
        self.forward_streams.clear();
        self.reader_forward_bytes = 0;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn rebuild_forward_cache(
        &mut self,
    ) {
        let mut forward_streams = BTreeMap::new();
        let mut reader_forward_bytes = 0usize;
        for (stream_index, queue) in &self.read_range().stream_queues {
            for packet_id in queue {
                if !self.active_packet_is_forward(*packet_id) {
                    continue;
                }
                let Some(packet) = self.packets.get(packet_id) else {
                    continue;
                };
                reader_forward_bytes = reader_forward_bytes.saturating_add(packet.byte_len);
                forward_streams
                    .entry(*stream_index)
                    .or_insert_with(StreamForwardState::default)
                    .push_packet(packet);
            }
        }
        self.forward_streams = forward_streams;
        self.reader_forward_bytes = reader_forward_bytes;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn update_forward_cache_after_appended_packet(
        &mut self,
        packet_id: PacketId,
    ) {
        if self.append_range_id != self.read_range_id || !self.active_packet_is_forward(packet_id) {
            return;
        }
        let Some(packet) = self.packets.get(&packet_id) else {
            return;
        };
        let stream_index = packet.stream_index;
        let byte_len = packet.byte_len;
        let start_nsecs = packet.start_nsecs;
        let end_nsecs = packet.end_nsecs;
        self.reader_forward_bytes = self.reader_forward_bytes.saturating_add(byte_len);
        self.forward_streams
            .entry(stream_index)
            .or_default()
            .push_packet_parts(byte_len, start_nsecs, end_nsecs);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn update_forward_cache_after_consumed_packet(
        &mut self,
        packet_id: PacketId,
    ) {
        let Some(packet) = self.packets.get(&packet_id) else {
            return;
        };
        let stream_index = packet.stream_index;
        let byte_len = packet.byte_len;
        let packet_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
        let next_head = self.reader_heads.get(&stream_index).copied();
        let next_head_times = next_head.and_then(|head| {
            self.packets
                .get(&head)
                .map(|packet| (packet.start_nsecs, packet.end_nsecs.or(packet.start_nsecs)))
        });

        self.reader_forward_bytes = self.reader_forward_bytes.saturating_sub(byte_len);
        let mut rebuild_stream = false;
        let mut remove_stream = false;
        if let Some(state) = self.forward_streams.get_mut(&stream_index) {
            state.packet_count = state.packet_count.saturating_sub(1);
            state.bytes = state.bytes.saturating_sub(byte_len);
            if state.packet_count == 0 || next_head.is_none() {
                remove_stream = true;
            } else {
                state.reader_nsecs =
                    next_head_times.and_then(|(start_nsecs, end_nsecs)| start_nsecs.or(end_nsecs));
                if packet_end_nsecs.is_some_and(|end_nsecs| state.end_nsecs == Some(end_nsecs)) {
                    rebuild_stream = true;
                }
                if state.reader_nsecs.is_none() {
                    rebuild_stream = true;
                }
            }
        }
        if remove_stream {
            self.forward_streams.remove(&stream_index);
            return;
        }
        if rebuild_stream {
            self.rebuild_forward_cache_for_stream(stream_index);
        }
    }

    fn rebuild_forward_cache_for_stream(&mut self, stream_index: c_int) {
        let mut state = StreamForwardState::default();
        let mut found = false;
        if let Some(queue) = self.read_range().stream_queues.get(&stream_index) {
            for packet_id in queue {
                if !self.active_packet_is_forward(*packet_id) {
                    continue;
                }
                let Some(packet) = self.packets.get(packet_id) else {
                    continue;
                };
                state.push_packet(packet);
                found = true;
            }
        }
        if found {
            self.forward_streams.insert(stream_index, state);
        } else {
            self.forward_streams.remove(&stream_index);
        }
        self.reader_forward_bytes = self
            .forward_streams
            .values()
            .map(|state| state.bytes)
            .fold(0usize, usize::saturating_add);
    }
}
