use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use super::{
    CachedDemuxPacket, DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL, DemuxInputRateSample,
    DemuxPacketAppendOutcome, DemuxPacketAppendTiming, DemuxPacketCacheState, PacketId,
    StreamCacheKind,
};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_packet(
        &mut self,
        mut packet: CachedDemuxPacket,
    ) -> DemuxPacketAppendOutcome {
        let mut timing = DemuxPacketAppendTiming::default();
        let record_input_started_at = Instant::now();
        self.record_input_bytes(packet.byte_len);
        timing.record_input_bytes += record_input_started_at.elapsed();
        if let Some(demux_ts_nsecs) = packet.start_nsecs.or(packet.end_nsecs) {
            self.demux_ts_nsecs = Some(demux_ts_nsecs);
        }
        let skip_resume_started_at = Instant::now();
        if self.should_skip_resume_overlap_packet(&packet) {
            timing.skip_resume_overlap += skip_resume_started_at.elapsed();
            return DemuxPacketAppendOutcome {
                appended: false,
                force_cache_state_report: false,
                timing,
            };
        }
        timing.skip_resume_overlap += skip_resume_started_at.elapsed();
        let packet_index_started_at = Instant::now();
        let packet_id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.saturating_add(1);
        let stream_index = packet.stream_index;
        let packet_forward_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
        timing.packet_index += packet_index_started_at.elapsed();
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
        {
            let disk_write_started_at = Instant::now();
            if let Err(error) = packet.spill_to_disk(disk_cache) {
                tracing::warn!(%error, "pausing FFmpeg demux packet disk cache writes");
                self.disk_cache_writable = false;
            }
            timing.disk_write += disk_write_started_at.elapsed();
        }
        let cleared_seek = self.seeking;
        let queue_insert_started_at = Instant::now();
        self.cached_bytes = self.cached_bytes.saturating_add(packet.byte_len);
        self.append_packet_id_to_append_range(packet_id, stream_index);
        self.packets.insert(packet_id, packet);
        timing.queue_insert += queue_insert_started_at.elapsed();
        let maybe_start_reader_head_started_at = Instant::now();
        self.maybe_start_reader_head_for_appended_packet(packet_id, stream_index);
        timing.maybe_start_reader_head += maybe_start_reader_head_started_at.elapsed();
        self.update_forward_cache_after_appended_packet(packet_id);
        self.seeking = false;
        let readahead_reached =
            self.appended_packet_may_reach_readahead(stream_index, packet_forward_end_nsecs);
        let memory_pressure = self.memory_pressure();
        let backbuffer_pressure = self.backbuffer_pressure();
        let queue_pressure = self.stream_packet_queue_full();
        let run_pause_maintenance = self.append_maintenance_due(
            cleared_seek || readahead_reached || memory_pressure || backbuffer_pressure,
        );
        let trim_due = cleared_seek || memory_pressure || backbuffer_pressure;
        let pruned = if trim_due {
            let trim_started_at = Instant::now();
            let pruned = self.trim_to_limit_for_append();
            timing.trim += trim_started_at.elapsed();
            pruned
        } else {
            false
        };
        let should_pause_demux = if cleared_seek
            || readahead_reached
            || queue_pressure
            || run_pause_maintenance
            || pruned
        {
            let hysteresis_started_at = Instant::now();
            self.refresh_readahead_hysteresis();
            timing.refresh_readahead_hysteresis += hysteresis_started_at.elapsed();
            let should_pause_started_at = Instant::now();
            let should_pause_demux = self.should_pause_demux();
            timing.should_pause_demux += should_pause_started_at.elapsed();
            should_pause_demux
        } else {
            false
        };
        DemuxPacketAppendOutcome {
            appended: true,
            force_cache_state_report: cleared_seek || pruned || should_pause_demux,
            timing,
        }
    }

    fn append_maintenance_due(&mut self, force: bool) -> bool {
        self.append_maintenance_packets = self.append_maintenance_packets.saturating_add(1);
        let due = force
            || (!self.hysteresis_active
                && self.append_maintenance_packets >= DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL);
        if due {
            self.append_maintenance_packets = 0;
        }
        due
    }

    fn appended_packet_may_reach_readahead(
        &self,
        stream_index: c_int,
        packet_end_nsecs: Option<u64>,
    ) -> bool {
        if self.hysteresis_active {
            return false;
        }
        if !matches!(
            self.stream_kinds.get(&stream_index),
            Some(StreamCacheKind::Video | StreamCacheKind::Audio)
        ) {
            return false;
        }
        if !self.selected_eager_stream_heads_ready() {
            return false;
        }
        packet_end_nsecs.is_some_and(|end_nsecs| {
            end_nsecs.saturating_sub(self.reader_nsecs) >= self.readahead_nsecs
        })
    }

    fn selected_eager_stream_heads_ready(&self) -> bool {
        self.stream_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, StreamCacheKind::Video | StreamCacheKind::Audio))
            .all(|(stream_index, _)| {
                self.reader_heads
                    .get(stream_index)
                    .is_some_and(|packet_id| self.packets.contains_key(packet_id))
            })
    }

    fn append_packet_id_to_append_range(&mut self, packet_id: PacketId, stream_index: c_int) {
        let range = self.append_range_mut();
        range.ensure_stream_boundary(stream_index);
        range.global_order.push_back(packet_id);
        range
            .stream_queues
            .entry(stream_index)
            .or_default()
            .push_back(packet_id);
    }

    fn maybe_start_reader_head_for_appended_packet(
        &mut self,
        packet_id: PacketId,
        stream_index: c_int,
    ) {
        if self.append_range_id != self.read_range_id {
            return;
        }
        if self.reader_heads.contains_key(&stream_index) {
            return;
        }
        let packet_position = self.read_range().global_order.len().saturating_sub(1);
        self.reader_heads.insert(stream_index, packet_id);
        self.reader_head_positions
            .insert(stream_index, packet_position);
        self.read_index = self.read_index.min(packet_position);
        self.consumed_packet_ids.remove(&packet_id);
    }

    fn record_input_bytes(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.input_rate_samples.push_back(DemuxInputRateSample {
            at: Instant::now(),
            bytes,
        });
        self.prune_input_rate_samples();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn raw_input_rate(
        &self,
    ) -> Option<u64> {
        let now = Instant::now();
        let bytes: usize = self
            .input_rate_samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) <= Duration::from_secs(1))
            .map(|sample| sample.bytes)
            .sum();
        (bytes > 0).then(|| u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    fn prune_input_rate_samples(&mut self) {
        let now = Instant::now();
        while self
            .input_rate_samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) > Duration::from_secs(1))
        {
            self.input_rate_samples.pop_front();
        }
    }

    fn should_skip_resume_overlap_packet(&mut self, packet: &CachedDemuxPacket) -> bool {
        let Some(skip_until_nsecs) = self.resume_append_skip_until_nsecs else {
            return false;
        };
        let packet_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
        if packet_end_nsecs.is_some_and(|end_nsecs| end_nsecs <= skip_until_nsecs) {
            return true;
        }
        if packet
            .start_nsecs
            .is_some_and(|start_nsecs| start_nsecs >= skip_until_nsecs)
            || packet_end_nsecs.is_none()
        {
            self.resume_append_skip_until_nsecs = None;
        }
        false
    }
}
