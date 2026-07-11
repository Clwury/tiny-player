use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use super::{
    CachedDemuxPacket, DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL, DemuxInputRateSample,
    DemuxPacketAppendOutcome, DemuxPacketAppendTiming, DemuxPacketCacheState, PacketId,
    StreamCacheKind,
};

const LOW_LEVEL_SEEK_APPEND_MAX_INITIAL_LEAD_NSECS: u64 = 2_000_000_000;

impl DemuxPacketCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_packet(
        &mut self,
        packet: CachedDemuxPacket,
    ) -> DemuxPacketAppendOutcome {
        let mut outcome = self.append_packet_fast(packet);
        self.complete_append_packet_trim(&mut outcome);
        outcome
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_packet_fast(
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
                cache_state_emit_deferred_for_consumer: false,
                trim_requested: false,
                trim_deferred_for_consumer: false,
                trim_deferred_for_recovery: false,
                trim_outcome: Default::default(),
                timing,
            };
        }
        timing.skip_resume_overlap += skip_resume_started_at.elapsed();
        let packet_index_started_at = Instant::now();
        let packet_id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.saturating_add(1);
        let stream_index = packet.stream_index;
        let packet_forward_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
        let blocked_for_current_read =
            self.mark_low_level_seek_noncurrent_packet_if_needed(packet_id, &packet);
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
        let packet_byte_len = packet.byte_len;
        self.cached_bytes = self.cached_bytes.saturating_add(packet_byte_len);
        self.packets.insert(packet_id, packet);
        let packet_is_seek_boundary = self.packets.get(&packet_id).is_some_and(|packet| {
            Self::packet_is_stream_seek_boundary_for(
                self.timeline_anchor_stream_index,
                stream_index,
                packet,
                self.cached_seek_requires_safe_point,
                self.stream_requires_recovery_point(stream_index),
            )
        });
        let packet_position = self.append_packet_id_to_append_range(
            packet_id,
            stream_index,
            packet_byte_len,
            packet_is_seek_boundary,
        );
        if !blocked_for_current_read {
            self.refresh_range_stream_seek_boundary_after_append(
                self.append_range_id,
                stream_index,
                packet_id,
            );
        }
        timing.queue_insert += queue_insert_started_at.elapsed();
        let maybe_start_reader_head_started_at = Instant::now();
        if !blocked_for_current_read {
            self.maybe_start_reader_head_for_appended_packet(
                packet_id,
                stream_index,
                packet_position,
            );
        }
        timing.maybe_start_reader_head += maybe_start_reader_head_started_at.elapsed();
        self.update_forward_cache_after_appended_packet(packet_id, packet_position);
        self.seeking = false;
        let readahead_reached =
            self.appended_packet_may_reach_readahead(stream_index, packet_forward_end_nsecs);
        let memory_pressure = self.memory_pressure();
        let backbuffer_pressure = self.backbuffer_pressure();
        let run_pause_maintenance =
            self.append_maintenance_due(cleared_seek || readahead_reached || memory_pressure);
        // mpv stops prefetching on forward-byte pressure, but only prunes the
        // backward cache when it exceeds the effective (possibly donated)
        // backbuffer budget.
        let trim_requested = backbuffer_pressure && self.append_trim_due(cleared_seek);
        let should_pause_demux = if cleared_seek || readahead_reached || run_pause_maintenance {
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
            force_cache_state_report: cleared_seek || should_pause_demux,
            cache_state_emit_deferred_for_consumer: false,
            trim_requested,
            trim_deferred_for_consumer: false,
            trim_deferred_for_recovery: false,
            trim_outcome: Default::default(),
            timing,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn complete_append_packet_trim(
        &mut self,
        outcome: &mut DemuxPacketAppendOutcome,
    ) -> bool {
        if !outcome.trim_requested {
            return false;
        }
        let trim_started_at = Instant::now();
        let trim_outcome = self.trim_to_limit_for_append_with_outcome();
        outcome.timing.trim += trim_started_at.elapsed();
        outcome.trim_outcome = outcome.trim_outcome.merged(trim_outcome);
        self.complete_append_trim();
        outcome.trim_requested = false;
        if !trim_outcome.performed {
            return false;
        }

        let hysteresis_started_at = Instant::now();
        self.refresh_readahead_hysteresis();
        outcome.timing.refresh_readahead_hysteresis += hysteresis_started_at.elapsed();
        let should_pause_started_at = Instant::now();
        outcome.force_cache_state_report |= self.should_pause_demux();
        outcome.timing.should_pause_demux += should_pause_started_at.elapsed();
        true
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
                    .is_some_and(|_| self.next_packet_id_for_stream(*stream_index).is_some())
            })
    }

    fn append_packet_id_to_append_range(
        &mut self,
        packet_id: PacketId,
        stream_index: c_int,
        byte_len: usize,
        packet_is_seek_boundary: bool,
    ) -> usize {
        let range = self.append_range_mut();
        range.ensure_stream_boundary(stream_index);
        let packet_position = range.global_order.len();
        range.global_order.push_back(packet_id);
        range
            .stream_queues
            .entry(stream_index)
            .or_default()
            .push_back(packet_id);
        if packet_is_seek_boundary {
            range
                .stream_seek_boundaries
                .entry(stream_index)
                .or_default()
                .push_back(packet_id);
        }
        range.add_report_bytes(byte_len);
        range.mark_seekable_dirty();
        packet_position
    }

    fn maybe_start_reader_head_for_appended_packet(
        &mut self,
        packet_id: PacketId,
        stream_index: c_int,
        packet_position: usize,
    ) {
        if self.append_range_id != self.read_range_id {
            return;
        }
        if self.next_packet_id_for_stream(stream_index).is_some() {
            return;
        }
        self.set_reader_head_for_current_generation(stream_index, packet_id);
        self.reader_head_positions
            .insert(stream_index, packet_position);
        self.refresh_read_index_from_reader_head_positions();
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
            return false;
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

    fn mark_low_level_seek_noncurrent_packet_if_needed(
        &mut self,
        packet_id: PacketId,
        packet: &CachedDemuxPacket,
    ) -> bool {
        if let Some(skip_until_nsecs) = self.resume_append_skip_until_nsecs {
            let packet_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
            if packet_end_nsecs.is_some_and(|end_nsecs| end_nsecs <= skip_until_nsecs) {
                self.low_level_append_blocked_packet_generations
                    .insert(packet_id, self.generation);
                self.consumed_packet_ids.insert(packet_id);
                tracing::debug!(
                    session_id = ?self.session_id,
                    skip_until_nsecs,
                    packet_start_nsecs = ?packet.start_nsecs,
                    packet_end_nsecs = ?packet.end_nsecs,
                    stream_index = packet.stream_index,
                    "appending repeated FFmpeg demux packet outside current reader head after cached-range low-level resume"
                );
                return true;
            }
            if packet
                .start_nsecs
                .is_some_and(|start_nsecs| start_nsecs >= skip_until_nsecs)
                || packet_end_nsecs.is_none()
            {
                self.resume_append_skip_until_nsecs = None;
            }
        }

        let Some(target_nsecs) = self.low_level_append_guard_target_nsecs else {
            return false;
        };
        let Some(packet_start_nsecs) = packet.start_nsecs.or(packet.end_nsecs) else {
            self.low_level_append_guard_target_nsecs = None;
            return false;
        };
        let max_initial_nsecs =
            target_nsecs.saturating_add(LOW_LEVEL_SEEK_APPEND_MAX_INITIAL_LEAD_NSECS);
        if packet_start_nsecs > max_initial_nsecs {
            self.low_level_append_blocked_packet_generations
                .insert(packet_id, self.generation);
            self.consumed_packet_ids.insert(packet_id);
            tracing::debug!(
                session_id = ?self.session_id,
                target_nsecs,
                packet_start_nsecs,
                packet_end_nsecs = ?packet.end_nsecs,
                stream_index = packet.stream_index,
                packet_lead_ms =
                    packet_start_nsecs.saturating_sub(target_nsecs) as f64 / 1_000_000.0,
                max_initial_lead_ms =
                    LOW_LEVEL_SEEK_APPEND_MAX_INITIAL_LEAD_NSECS as f64 / 1_000_000.0,
                "appending stale FFmpeg demux packet ahead of low-level seek target outside current reader head"
            );
            return true;
        }
        self.low_level_append_guard_target_nsecs = None;
        false
    }
}
