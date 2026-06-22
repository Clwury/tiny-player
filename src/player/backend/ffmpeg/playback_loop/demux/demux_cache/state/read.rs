use std::{
    collections::{BTreeMap, HashMap, HashSet},
    os::raw::c_int,
    time::Instant,
};

use super::{
    DEMUX_CACHE_LOCK_TIMING_LOG_AFTER, DemuxPacketCacheReadTiming, DemuxPacketCacheState,
    DemuxPacketQueueSnapshot, DemuxPacketReadSource, DemuxStreamPacketQueueSnapshot, PacketId,
    StreamCacheKind,
};

impl DemuxPacketCacheState {
    fn packet_read_source(
        &self,
        packet_id: u64,
        stream_offset: usize,
    ) -> std::result::Result<DemuxPacketReadSource, String> {
        let Some(packet) = self.packets.get(&packet_id) else {
            return Err("FFmpeg demux packet cache entry missing".to_string());
        };
        packet.read_source(self.disk_cache.as_ref(), stream_offset)
    }

    fn packet_end_nsecs(&self, packet_id: u64) -> Option<u64> {
        self.packets
            .get(&packet_id)
            .and_then(|packet| packet.end_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn take_packet_round_robin(
        &mut self,
        stream_indices: &[c_int],
        timing: &mut DemuxPacketCacheReadTiming,
    ) -> std::result::Result<Option<DemuxPacketReadSource>, String> {
        let started_at = Instant::now();
        for (stream_offset, stream_index) in stream_indices.iter().copied().enumerate() {
            let Some(packet_id) = self.next_packet_id_for_stream(stream_index) else {
                continue;
            };
            let packet = self.packet_read_source(packet_id, stream_offset)?;
            if let Some(end_nsecs) = self.packet_end_nsecs(packet_id) {
                self.reader_nsecs = self.reader_nsecs.max(end_nsecs);
            }
            self.consume_packet_id(packet_id, timing);
            timing.take_packet += started_at.elapsed();
            return Ok(Some(packet));
        }
        timing.take_packet += started_at.elapsed();
        Ok(None)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn next_packet_id_for_stream(
        &self,
        stream_index: c_int,
    ) -> Option<PacketId> {
        let packet_id = self.reader_heads.get(&stream_index).copied()?;
        self.packets.contains_key(&packet_id).then_some(packet_id)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn consume_packet_id(
        &mut self,
        packet_id: PacketId,
        timing: &mut DemuxPacketCacheReadTiming,
    ) {
        let advance_started_at = Instant::now();
        self.advance_reader_head_over_packet(packet_id);
        timing.advance_reader_head += advance_started_at.elapsed();
        if let Some(stream_index) = self
            .packets
            .get(&packet_id)
            .map(|packet| packet.stream_index)
        {
            self.mark_read_stream_bof(stream_index, false);
        } else {
            self.read_range_mut().is_bof = false;
        }
        self.update_forward_cache_after_consumed_packet(packet_id);
        if self.read_trim_due() {
            let trim_started_at = Instant::now();
            self.trim_to_limit_for_read();
            timing.trim += trim_started_at.elapsed();
        }
    }

    fn advance_reader_head_over_packet(&mut self, packet_id: PacketId) {
        let Some(packet) = self.packets.get(&packet_id) else {
            return;
        };
        let stream_index = packet.stream_index;
        if self.reader_heads.get(&stream_index).copied() != Some(packet_id) {
            return;
        }
        let old_position = self
            .reader_head_positions
            .get(&stream_index)
            .copied()
            .or_else(|| self.packet_position_in_read_range(packet_id));
        self.consumed_packet_ids.insert(packet_id);
        let next_packet_id = self
            .read_range()
            .stream_queues
            .get(&stream_index)
            .and_then(|queue| {
                let position = queue.iter().position(|candidate| *candidate == packet_id)?;
                queue
                    .iter()
                    .skip(position.saturating_add(1))
                    .copied()
                    .find(|candidate| self.packets.contains_key(candidate))
            });
        match next_packet_id {
            Some(next_packet_id) => {
                self.reader_heads.insert(stream_index, next_packet_id);
                if let Some(next_position) = old_position
                    .and_then(|position| {
                        self.packet_position_in_read_range_after(next_packet_id, position)
                    })
                    .or_else(|| self.packet_position_in_read_range(next_packet_id))
                {
                    self.reader_head_positions
                        .insert(stream_index, next_position);
                } else {
                    self.reader_head_positions.remove(&stream_index);
                }
            }
            None => {
                self.reader_heads.remove(&stream_index);
                self.reader_head_positions.remove(&stream_index);
            }
        }
        self.refresh_read_index_from_reader_head_positions();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reset_reader_heads_for_read_index(
        &mut self,
    ) {
        let range_len = self.read_range().global_order.len();
        let read_index = self.read_index.min(range_len);
        let packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        let mut reader_heads = BTreeMap::new();
        for (stream_index, queue) in &self.read_range().stream_queues {
            let Some(packet_id) = queue.iter().copied().find(|packet_id| {
                self.packets.contains_key(packet_id)
                    && packet_positions
                        .get(packet_id)
                        .is_some_and(|position| *position >= read_index)
            }) else {
                continue;
            };
            reader_heads.insert(*stream_index, packet_id);
        }
        self.reader_heads = reader_heads;
        self.refresh_reader_tracking();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_reader_tracking(
        &mut self,
    ) {
        let started_at = Instant::now();
        self.refresh_reader_tracking_inner();
        let elapsed = started_at.elapsed();
        if elapsed >= DEMUX_CACHE_LOCK_TIMING_LOG_AFTER {
            tracing::debug!(
                session_id = ?self.session_id,
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                packet_count = self.read_range().global_order.len(),
                stream_count = self.read_range().stream_queues.len(),
                reader_head_count = self.reader_heads.len(),
                "FFmpeg demux packet cache refreshed reader tracking slowly"
            );
        }
    }

    fn refresh_reader_tracking_inner(&mut self) {
        let packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        let stream_queues = self.read_range().stream_queues.clone();
        self.reader_heads.retain(|stream_index, packet_id| {
            self.stream_kinds.contains_key(stream_index)
                && self.packets.contains_key(packet_id)
                && stream_queues
                    .get(stream_index)
                    .is_some_and(|queue| queue.iter().any(|candidate| *candidate == *packet_id))
        });
        self.reader_head_positions = self
            .reader_heads
            .iter()
            .filter_map(|(stream_index, packet_id)| {
                packet_positions
                    .get(packet_id)
                    .copied()
                    .map(|position| (*stream_index, position))
            })
            .collect();

        self.read_index = self
            .reader_head_positions
            .values()
            .copied()
            .min()
            .unwrap_or_else(|| self.read_range().global_order.len());

        let mut consumed = HashSet::new();
        for (stream_index, queue) in &self.read_range().stream_queues {
            let reader_head = self.reader_heads.get(stream_index).copied();
            for packet_id in queue {
                if Some(*packet_id) == reader_head {
                    break;
                }
                consumed.insert(*packet_id);
            }
        }
        self.consumed_packet_ids = consumed;
        self.rebuild_forward_cache();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_read_index_from_reader_head_positions(
        &mut self,
    ) {
        self.read_index = self
            .reader_head_positions
            .values()
            .copied()
            .min()
            .unwrap_or_else(|| self.read_range().global_order.len());
    }

    fn packet_position_in_read_range(&self, packet_id: PacketId) -> Option<usize> {
        self.read_range()
            .global_order
            .iter()
            .position(|candidate| *candidate == packet_id)
    }

    fn packet_position_in_read_range_after(
        &self,
        packet_id: PacketId,
        previous_position: usize,
    ) -> Option<usize> {
        self.read_range()
            .global_order
            .iter()
            .enumerate()
            .skip(previous_position.saturating_add(1))
            .find_map(|(position, candidate)| (*candidate == packet_id).then_some(position))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_queue_snapshot(
        &self,
    ) -> DemuxPacketQueueSnapshot {
        let mut stream_ids = self
            .read_range()
            .stream_queues
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if let Some(range) = self.detached_append_range() {
            for stream_index in range.stream_queues.keys().copied() {
                if !stream_ids.contains(&stream_index) {
                    stream_ids.push(stream_index);
                }
            }
        }
        stream_ids.sort_unstable();

        let forward_by_kind = self
            .reader_stream_forward_windows()
            .into_iter()
            .map(|window| (window.kind, window.duration_nsecs()))
            .collect::<Vec<_>>();
        let mut streams = Vec::new();
        for stream_index in stream_ids {
            let kind = self
                .stream_kinds
                .get(&stream_index)
                .copied()
                .unwrap_or(StreamCacheKind::Unknown);
            let queued_packets = self.queued_packet_count_for_stream(stream_index);
            if queued_packets == 0 {
                continue;
            }
            let packet_limit = self.stream_packet_queue_limit(stream_index);
            let queued_bytes = self.queued_bytes_for_stream(stream_index);
            let forward_nsecs = forward_by_kind
                .iter()
                .find_map(|(window_kind, duration)| (*window_kind == kind).then_some(*duration));
            streams.push(DemuxStreamPacketQueueSnapshot {
                stream_index,
                kind,
                queued_packets,
                packet_limit,
                packet_queue_full: queued_packets >= packet_limit,
                queued_bytes,
                forward_nsecs,
            });
        }
        DemuxPacketQueueSnapshot {
            total_packets: streams.iter().map(|stream| stream.queued_packets).sum(),
            total_bytes: streams.iter().map(|stream| stream.queued_bytes).sum(),
            memory_limit_bytes: self.memory_limit_bytes,
            streams,
        }
    }

    fn queued_packet_count_for_stream(&self, stream_index: c_int) -> usize {
        let active_count = self
            .forward_streams
            .get(&stream_index)
            .map(|state| state.packet_count)
            .unwrap_or_default();
        let detached_count = self
            .detached_append_range()
            .and_then(|range| range.stream_queues.get(&stream_index))
            .map(|queue| {
                queue
                    .iter()
                    .filter(|packet_id| self.packets.contains_key(packet_id))
                    .count()
            })
            .unwrap_or_default();
        active_count + detached_count
    }

    fn queued_bytes_for_stream(&self, stream_index: c_int) -> usize {
        let active_bytes = self
            .forward_streams
            .get(&stream_index)
            .map(|state| state.bytes)
            .unwrap_or_default();
        let detached_bytes: usize = self
            .detached_append_range()
            .and_then(|range| range.stream_queues.get(&stream_index))
            .into_iter()
            .flat_map(|queue| queue.iter())
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum();
        active_bytes + detached_bytes
    }
}
