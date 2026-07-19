use super::audio_decode_worker::{
    AudioDecodeEnqueueResult, AudioDecodePacketResult, AudioDecodePacketStatus, AudioDecodeWorker,
    AudioDecodeWorkerInfo, AudioDecodeWorkerSnapshot, AudioDecodeWorkerState, AudioDecodedFrame,
};
use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::pending_audio_queue::matching_audio_timeline_gap;
use std::os::raw::c_int;

use crate::player::render_host::PlaybackSessionId;

use super::{
    AvPacket, Decoder, PENDING_AUDIO_CONTINUITY_TOLERANCE, PlaybackBlockReason, PlaybackGeneration,
    TimestampMapper,
};

const AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 16;

pub(super) struct PendingAudioDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
}

pub(super) struct AudioDecodePipeline {
    worker: AudioDecodeWorker,
    packets: AudioDecodePacketQueues,
}

impl AudioDecodePipeline {
    pub(super) fn spawn(
        decoder: Decoder,
        output_rate: c_int,
        output_channels: c_int,
    ) -> std::result::Result<Self, String> {
        Ok(Self {
            worker: AudioDecodeWorker::spawn(decoder, output_rate, output_channels)?,
            packets: AudioDecodePacketQueues::default(),
        })
    }

    pub(super) fn info(&self) -> &AudioDecodeWorkerInfo {
        self.worker.info()
    }

    pub(super) fn snapshot(&self) -> AudioDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        snapshot.pending_input_packets = self.packets.pending_input_count();
        snapshot.pending_input_capacity = self.packets.pending_input_capacity();
        snapshot
    }

    pub(super) fn block_reason_for(
        snapshot: AudioDecodeWorkerSnapshot,
    ) -> Option<PlaybackBlockReason> {
        match snapshot.state {
            AudioDecodeWorkerState::Recovering => Some(PlaybackBlockReason::DecoderRecovery),
            AudioDecodeWorkerState::OutputFull => Some(PlaybackBlockReason::DecodedQueueFull),
            _ if snapshot.pending_input_full()
                || snapshot.in_flight_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::PacketQueueFull)
            }
            AudioDecodeWorkerState::NeedPacket if snapshot.pending_input_packets == 0 => {
                Some(PlaybackBlockReason::DecoderInputEmpty)
            }
            _ => None,
        }
    }

    pub(super) fn try_enqueue_pending_packet(
        &mut self,
        pending_packet: PendingAudioDecodePacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if self.packets.has_pending_input() {
            return Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id));
        }
        let enqueue_result = self
            .worker
            .try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            AudioDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodePacketAdmissionStatus::Queued)
            }
            AudioDecodeEnqueueResult::InputFull | AudioDecodeEnqueueResult::OutputFull => {
                Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id))
            }
        }
    }

    pub(super) fn retry_pending_input(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        self.worker.service()?;
        let Some(pending_packet) = self.take_pending_input() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        let enqueue_result = self
            .worker
            .try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            AudioDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodeInputRetryStatus::Queued)
            }
            AudioDecodeEnqueueResult::InputFull | AudioDecodeEnqueueResult::OutputFull => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(session_id, enqueue_result);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    fn buffer_pending_input_or_backpressure(
        &mut self,
        pending_packet: PendingAudioDecodePacket,
        session_id: PlaybackSessionId,
    ) -> DecodePacketAdmissionStatus {
        match self.packets.push_pending_input(pending_packet) {
            Ok(()) => {
                let snapshot = self.snapshot();
                tracing::trace!(
                    session_id = ?session_id,
                    audio_decode_pending_input_packets = snapshot.pending_input_packets,
                    audio_decode_pending_input_capacity =
                        snapshot.pending_input_capacity,
                    audio_decode_pending_input_full = snapshot.pending_input_full(),
                    audio_decode_in_flight_packets = snapshot.in_flight_packets,
                    audio_decode_state = ?snapshot.state,
                    "buffered FFmpeg audio packet in decoder wrapper input queue"
                );
                DecodePacketAdmissionStatus::Queued
            }
            Err(pending_packet) => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(
                    session_id,
                    AudioDecodeEnqueueResult::InputFull,
                );
                DecodePacketAdmissionStatus::Backpressured
            }
        }
    }

    fn log_pending_input_backpressured(
        &self,
        session_id: PlaybackSessionId,
        enqueue_result: AudioDecodeEnqueueResult,
    ) {
        let snapshot = self.snapshot();
        let blocked_on = Self::block_reason_for(snapshot).unwrap_or(match enqueue_result {
            AudioDecodeEnqueueResult::InputFull => PlaybackBlockReason::PacketQueueFull,
            AudioDecodeEnqueueResult::OutputFull => PlaybackBlockReason::DecodedQueueFull,
            AudioDecodeEnqueueResult::Queued => PlaybackBlockReason::OutputGate,
        });
        tracing::debug!(
            session_id = ?session_id,
            blocked_on = blocked_on.as_str(),
            output_rate = self.info().output_rate,
            output_channels = self.info().output_channels,
            audio_decode_state = ?snapshot.state,
            audio_decode_queued_frames = snapshot.queued_frames,
            audio_decode_queued_ms = snapshot.queued_duration_nsecs as f64 / 1_000_000.0,
            audio_decode_limit_ms = snapshot.duration_limit_nsecs as f64 / 1_000_000.0,
            audio_decode_pending_input_packets = snapshot.pending_input_packets,
            audio_decode_pending_input_capacity = snapshot.pending_input_capacity,
            audio_decode_pending_input_full = snapshot.pending_input_full(),
            audio_decode_in_flight_packets = snapshot.in_flight_packets,
            audio_decode_completed_packets = snapshot.completed_packets,
            recovery_generation = ?snapshot.recovery_generation,
            recovery_elapsed_ms = ?snapshot
                .recovery_elapsed
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            flush_command_sent = snapshot.flush_command_sent,
            stale_results_discarded = snapshot.stale_results_discarded,
            last_result_progress_ms = ?snapshot
                .last_result_progress_elapsed
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            "FFmpeg audio decoder wrapper input queue backpressured"
        );
    }

    pub(super) fn admit_demux_packet(
        &mut self,
        packet: &AvPacket,
        playback_generation: &mut PlaybackGeneration,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        let generation = playback_generation.advance();
        let pending_packet = PendingAudioDecodePacket {
            generation,
            packet: AvPacket::ref_from(packet)?,
        };
        self.try_enqueue_pending_packet(pending_packet, session_id)
    }

    pub(super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<AudioDecodedFrame>, String> {
        self.worker.poll_frame(generation)
    }

    pub(super) fn decoded_timeline_gap_near(
        &mut self,
        audio_clock: &TimestampMapper,
        expected_previous_end_nsecs: u64,
        expected_next_start_nsecs: u64,
        min_gap_nsecs: u64,
        endpoint_tolerance_nsecs: u64,
    ) -> std::result::Result<Option<(u64, u64)>, String> {
        let mut preview_clock = audio_clock.clone();
        let initial_previous_end_nsecs = preview_clock.last_contiguous_end_nsecs();
        let audio_time_base = self.info().time_base;
        let timings = self.worker.decoded_frame_timings()?;
        let mapped_frames = timings.into_iter().map(|timing| {
            let timestamp = preview_clock.map_contiguous(
                timing.raw_timestamp,
                audio_time_base,
                timing.duration_nsecs,
                PENDING_AUDIO_CONTINUITY_TOLERANCE,
            );
            (
                timestamp.timeline_nsecs,
                timestamp
                    .timeline_nsecs
                    .saturating_add(timing.duration_nsecs),
            )
        });
        Ok(matching_audio_timeline_gap(
            initial_previous_end_nsecs,
            mapped_frames,
            expected_previous_end_nsecs,
            expected_next_start_nsecs,
            min_gap_nsecs,
            endpoint_tolerance_nsecs,
        ))
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<AudioDecodePacketStatus>, String> {
        self.worker.poll_packet_status(generation)
    }

    pub(super) fn flush_buffers(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.worker.flush_buffers(generation)?;
        self.clear_packets();
        Ok(())
    }

    pub(super) fn service_worker(&mut self) -> std::result::Result<(), String> {
        self.worker.service()
    }

    pub(super) fn request_drain(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.worker.request_drain(generation)
    }

    pub(super) fn poll_drain_result(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<AudioDecodePacketResult>, String> {
        self.worker.poll_drain_result(generation)
    }

    pub(super) fn clear_packets(&mut self) {
        self.packets.clear();
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        self.packets.has_pending_or_in_flight()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingAudioDecodePacket> {
        self.packets.take_pending_input()
    }

    pub(super) fn push_in_flight(&mut self, packet: PendingAudioDecodePacket) {
        self.packets.push_in_flight(packet);
    }

    pub(super) fn front_generation(&self) -> Option<u64> {
        self.packets.front_generation()
    }

    pub(super) fn pop_completed_packet(&mut self) -> Option<PendingAudioDecodePacket> {
        self.packets.pop_completed_packet()
    }
}

type AudioDecodePacketQueues =
    DecoderPacketQueues<PendingAudioDecodePacket, AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY>;

impl AudioDecodePacketQueues {
    fn front_generation(&self) -> Option<u64> {
        self.front_in_flight().map(|packet| packet.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, AudioDecodePipeline, AudioDecodeWorkerSnapshot,
        AudioDecodeWorkerState, PlaybackBlockReason,
    };

    fn snapshot(
        state: AudioDecodeWorkerState,
        pending_input_packets: usize,
    ) -> AudioDecodeWorkerSnapshot {
        AudioDecodeWorkerSnapshot {
            state,
            queued_frames: 0,
            queued_duration_nsecs: 0,
            duration_limit_nsecs: 1_000_000_000,
            pending_input_packets,
            pending_input_capacity: AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            in_flight_packets: 0,
            command_queue_capacity: 4,
            completed_packets: 0,
            recovery_generation: None,
            recovery_elapsed: None,
            flush_command_sent: false,
            stale_results_discarded: 0,
            last_result_progress_elapsed: None,
        }
    }

    #[test]
    fn full_pending_audio_decode_input_reports_packet_queue_full() {
        let reason = AudioDecodePipeline::block_reason_for(snapshot(
            AudioDecodeWorkerState::NeedPacket,
            AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
        ));

        assert_eq!(reason, Some(PlaybackBlockReason::PacketQueueFull));
    }

    #[test]
    fn non_full_pending_audio_decode_input_is_not_packet_queue_full() {
        let reason =
            AudioDecodePipeline::block_reason_for(snapshot(AudioDecodeWorkerState::NeedPacket, 1));

        assert_eq!(reason, None);
    }

    #[test]
    fn recovering_audio_decoder_reports_decoder_recovery() {
        let reason = AudioDecodePipeline::block_reason_for(snapshot(
            AudioDecodeWorkerState::Recovering,
            AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
        ));

        assert_eq!(reason, Some(PlaybackBlockReason::DecoderRecovery));
    }
}
