use super::audio_decode_worker::{
    AudioDecodeEnqueueResult, AudioDecodePacketResult, AudioDecodePacketStatus, AudioDecodeWorker,
    AudioDecodeWorkerInfo, AudioDecodeWorkerSnapshot, AudioDecodeWorkerState, AudioDecodedFrame,
};
use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::pending_audio_queue::matching_audio_timeline_gap;
use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use crate::player::render_host::PlaybackSessionId;

use super::{
    AvPacket, Decoder, PENDING_AUDIO_CONTINUITY_TOLERANCE, PlaybackBlockReason, PlaybackGeneration,
    TimestampMapper,
};

const AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 16;
const AUDIO_DECODE_BACKPRESSURE_LOG_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioDecodeBackpressureObservation {
    enqueue_result: AudioDecodeEnqueueResult,
    blocked_on: PlaybackBlockReason,
    worker_state: AudioDecodeWorkerState,
    pending_input_full: bool,
}

#[derive(Clone, Copy, Debug)]
struct AudioDecodeBackpressureLogState {
    observation: AudioDecodeBackpressureObservation,
    started_at: Instant,
    last_logged_at: Instant,
    total_observations: u64,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioDecodeBackpressureLogDecision {
    Changed {
        suppressed_repeats: u64,
        total_observations: u64,
    },
    Summary {
        suppressed_repeats: u64,
        total_observations: u64,
    },
    Suppressed,
}

fn observe_audio_decode_backpressure_log(
    state: &mut Option<AudioDecodeBackpressureLogState>,
    observation: AudioDecodeBackpressureObservation,
    now: Instant,
) -> AudioDecodeBackpressureLogDecision {
    let Some(current) = state.as_mut() else {
        *state = Some(AudioDecodeBackpressureLogState {
            observation,
            started_at: now,
            last_logged_at: now,
            total_observations: 1,
            suppressed_repeats: 0,
        });
        return AudioDecodeBackpressureLogDecision::Changed {
            suppressed_repeats: 0,
            total_observations: 1,
        };
    };

    if current.observation != observation {
        let suppressed_repeats = current.suppressed_repeats;
        *current = AudioDecodeBackpressureLogState {
            observation,
            started_at: now,
            last_logged_at: now,
            total_observations: 1,
            suppressed_repeats: 0,
        };
        return AudioDecodeBackpressureLogDecision::Changed {
            suppressed_repeats,
            total_observations: 1,
        };
    }

    current.total_observations = current.total_observations.saturating_add(1);
    current.suppressed_repeats = current.suppressed_repeats.saturating_add(1);
    if now.saturating_duration_since(current.last_logged_at)
        >= AUDIO_DECODE_BACKPRESSURE_LOG_INTERVAL
    {
        let suppressed_repeats = current.suppressed_repeats;
        current.suppressed_repeats = 0;
        current.last_logged_at = now;
        AudioDecodeBackpressureLogDecision::Summary {
            suppressed_repeats,
            total_observations: current.total_observations,
        }
    } else {
        AudioDecodeBackpressureLogDecision::Suppressed
    }
}

fn take_deferred_output_frame(
    slot: &mut Option<(u64, AudioDecodedFrame)>,
    generation: u64,
) -> Option<AudioDecodedFrame> {
    slot.as_ref()
        .is_some_and(|(deferred_generation, _)| *deferred_generation == generation)
        .then(|| slot.take().map(|(_, frame)| frame))
        .flatten()
}

fn retain_deferred_output_frame(
    slot: &mut Option<(u64, AudioDecodedFrame)>,
    generation: u64,
    frame: AudioDecodedFrame,
) -> std::result::Result<(), String> {
    if slot.is_some() {
        return Err(
            "FFmpeg audio decode pipeline already retains a deferred output frame".to_string(),
        );
    }
    *slot = Some((generation, frame));
    Ok(())
}

pub(super) struct PendingAudioDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
}

pub(super) struct AudioDecodePipeline {
    worker: AudioDecodeWorker,
    packets: AudioDecodePacketQueues,
    deferred_output_frame: Option<(u64, AudioDecodedFrame)>,
    backpressure_log_state: Option<AudioDecodeBackpressureLogState>,
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
            deferred_output_frame: None,
            backpressure_log_state: None,
        })
    }

    pub(super) fn info(&self) -> &AudioDecodeWorkerInfo {
        self.worker.info()
    }

    pub(super) fn snapshot(&self) -> AudioDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        snapshot.pending_input_packets = self.packets.pending_input_count();
        snapshot.pending_input_capacity = self.packets.pending_input_capacity();
        if let Some((_, frame)) = self.deferred_output_frame.as_ref() {
            snapshot.state = AudioDecodeWorkerState::OutputFull;
            snapshot.queued_frames = snapshot.queued_frames.saturating_add(1);
            snapshot.queued_duration_nsecs = snapshot
                .queued_duration_nsecs
                .saturating_add(frame.audio.duration_nsecs);
        }
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
        if self.deferred_output_frame.is_some() || self.packets.has_pending_input() {
            return Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id));
        }
        let enqueue_result = self
            .worker
            .try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            AudioDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                self.clear_backpressure_log_if_recovered(session_id);
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
        if self.deferred_output_frame.is_some() {
            return Ok(DecodeInputRetryStatus::Backpressured);
        }
        let Some(pending_packet) = self.take_pending_input() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        let enqueue_result = self
            .worker
            .try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            AudioDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                self.clear_backpressure_log_if_recovered(session_id);
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
        &mut self,
        session_id: PlaybackSessionId,
        enqueue_result: AudioDecodeEnqueueResult,
    ) {
        let snapshot = self.snapshot();
        let blocked_on = Self::block_reason_for(snapshot).unwrap_or(match enqueue_result {
            AudioDecodeEnqueueResult::InputFull => PlaybackBlockReason::PacketQueueFull,
            AudioDecodeEnqueueResult::OutputFull => PlaybackBlockReason::DecodedQueueFull,
            AudioDecodeEnqueueResult::Queued => PlaybackBlockReason::OutputGate,
        });
        let decision = observe_audio_decode_backpressure_log(
            &mut self.backpressure_log_state,
            AudioDecodeBackpressureObservation {
                enqueue_result,
                blocked_on,
                worker_state: snapshot.state,
                pending_input_full: snapshot.pending_input_full(),
            },
            Instant::now(),
        );
        let (state_changed, suppressed_repeats, total_backpressure_observations) = match decision {
            AudioDecodeBackpressureLogDecision::Changed {
                suppressed_repeats,
                total_observations,
            } => (true, suppressed_repeats, total_observations),
            AudioDecodeBackpressureLogDecision::Summary {
                suppressed_repeats,
                total_observations,
            } => (false, suppressed_repeats, total_observations),
            AudioDecodeBackpressureLogDecision::Suppressed => return,
        };
        tracing::debug!(
            session_id = ?session_id,
            state_changed,
            suppressed_repeats,
            total_backpressure_observations,
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

    fn clear_backpressure_log_if_recovered(&mut self, session_id: PlaybackSessionId) {
        let snapshot = self.snapshot();
        let pressure_cleared = snapshot.pending_input_packets == 0
            && !snapshot.pending_input_full()
            && snapshot.state != AudioDecodeWorkerState::OutputFull
            && snapshot.in_flight_packets < snapshot.command_queue_capacity;
        if !pressure_cleared {
            return;
        }
        let Some(state) = self.backpressure_log_state.take() else {
            return;
        };
        tracing::debug!(
            session_id = ?session_id,
            state_changed = true,
            backpressure_elapsed_ms = state.started_at.elapsed().as_secs_f64() * 1000.0,
            total_backpressure_observations = state.total_observations,
            suppressed_repeats = state.suppressed_repeats,
            audio_decode_state = ?snapshot.state,
            audio_decode_pending_input_packets = snapshot.pending_input_packets,
            audio_decode_in_flight_packets = snapshot.in_flight_packets,
            "FFmpeg audio decoder wrapper input queue backpressure cleared"
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
        if let Some(frame) = take_deferred_output_frame(&mut self.deferred_output_frame, generation)
        {
            return Ok(Some(frame));
        }
        self.worker.poll_frame(generation)
    }

    pub(super) fn defer_output_frame(
        &mut self,
        generation: u64,
        frame: AudioDecodedFrame,
    ) -> std::result::Result<(), String> {
        retain_deferred_output_frame(&mut self.deferred_output_frame, generation, frame)
    }

    pub(super) fn has_deferred_output_frame(&self) -> bool {
        self.deferred_output_frame.is_some()
    }

    pub(super) fn take_output_frames_for_realign(&mut self) -> Vec<(u64, AudioDecodedFrame)> {
        let mut frames = Vec::new();
        if let Some(frame) = self.deferred_output_frame.take() {
            frames.push(frame);
        }
        frames.extend(self.worker.take_decoded_frames_for_realign());
        frames
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
        let mut timings = Vec::new();
        if let Some((_, deferred)) = self.deferred_output_frame.as_ref() {
            timings.push(super::audio_decode_worker::AudioDecodedFrameTiming {
                raw_timestamp: deferred.raw_timestamp,
                duration_nsecs: deferred.audio.duration_nsecs,
            });
        }
        timings.extend(self.worker.decoded_frame_timings()?);
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
        self.deferred_output_frame = None;
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
        self.backpressure_log_state = None;
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        self.deferred_output_frame.is_some() || self.packets.has_pending_or_in_flight()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingAudioDecodePacket> {
        self.packets.take_pending_input()
    }

    pub(super) fn push_in_flight(&mut self, packet: PendingAudioDecodePacket) {
        self.packets.push_in_flight(packet);
    }

    pub(super) fn front_generation(&self) -> Option<u64> {
        self.deferred_output_frame
            .as_ref()
            .map(|(generation, _)| *generation)
            .or_else(|| self.packets.front_generation())
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
    use std::time::{Duration, Instant};

    use super::super::DecodedAudio;
    use super::{
        AUDIO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, AudioDecodeBackpressureLogDecision,
        AudioDecodeBackpressureObservation, AudioDecodeEnqueueResult, AudioDecodePipeline,
        AudioDecodeWorkerSnapshot, AudioDecodeWorkerState, AudioDecodedFrame, PlaybackBlockReason,
        observe_audio_decode_backpressure_log, retain_deferred_output_frame,
        take_deferred_output_frame,
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

    #[test]
    fn far_ahead_audio_retains_exactly_one_frame_until_realign_or_retry() {
        let mut slot = None;
        retain_deferred_output_frame(
            &mut slot,
            7,
            AudioDecodedFrame {
                raw_timestamp: 42,
                audio: DecodedAudio {
                    samples: vec![0.0; 4],
                    duration_nsecs: 21_333_333,
                },
            },
        )
        .unwrap();

        assert!(
            retain_deferred_output_frame(
                &mut slot,
                8,
                AudioDecodedFrame {
                    raw_timestamp: 43,
                    audio: DecodedAudio {
                        samples: vec![0.0; 4],
                        duration_nsecs: 21_333_333,
                    },
                },
            )
            .is_err()
        );
        assert!(take_deferred_output_frame(&mut slot, 8).is_none());
        let retained = take_deferred_output_frame(&mut slot, 7).expect("first frame is retained");
        assert_eq!(retained.raw_timestamp, 42);
        assert!(slot.is_none());
    }

    #[test]
    fn repeated_audio_decode_backpressure_logs_only_changes_and_one_second_summaries() {
        let mut state = None;
        let now = Instant::now();
        let observation = AudioDecodeBackpressureObservation {
            enqueue_result: AudioDecodeEnqueueResult::InputFull,
            blocked_on: PlaybackBlockReason::PacketQueueFull,
            worker_state: AudioDecodeWorkerState::NeedPacket,
            pending_input_full: true,
        };
        assert_eq!(
            observe_audio_decode_backpressure_log(&mut state, observation, now),
            AudioDecodeBackpressureLogDecision::Changed {
                suppressed_repeats: 0,
                total_observations: 1,
            }
        );
        for millisecond in 1..1000_u64 {
            assert_eq!(
                observe_audio_decode_backpressure_log(
                    &mut state,
                    observation,
                    now + Duration::from_millis(millisecond),
                ),
                AudioDecodeBackpressureLogDecision::Suppressed
            );
        }
        assert_eq!(
            observe_audio_decode_backpressure_log(
                &mut state,
                observation,
                now + Duration::from_secs(1),
            ),
            AudioDecodeBackpressureLogDecision::Summary {
                suppressed_repeats: 1000,
                total_observations: 1001,
            }
        );

        let changed = AudioDecodeBackpressureObservation {
            enqueue_result: AudioDecodeEnqueueResult::OutputFull,
            blocked_on: PlaybackBlockReason::DecodedQueueFull,
            worker_state: AudioDecodeWorkerState::OutputFull,
            pending_input_full: true,
        };
        assert_eq!(
            observe_audio_decode_backpressure_log(
                &mut state,
                changed,
                now + Duration::from_secs(1) + Duration::from_millis(1),
            ),
            AudioDecodeBackpressureLogDecision::Changed {
                suppressed_repeats: 0,
                total_observations: 1,
            }
        );
    }
}
