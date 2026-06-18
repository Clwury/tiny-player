use std::{collections::VecDeque, os::raw::c_int, sync::mpsc::Sender};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendSubtitleCue},
    render_host::{PlaybackSessionId, RenderSize},
};

use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::subtitle_decode_worker::{
    SubtitleCueUpdate, SubtitleDecodeEnqueueResult, SubtitleDecodePacketContext,
    SubtitleDecodePacketStatus, SubtitleDecodeWorker, SubtitleDecodeWorkerSnapshot,
    SubtitleDecodeWorkerState,
};
use super::{
    AudioOutput, AvPacket, DECODE_PACKET_SLOW_LOG_AFTER, Decoder, FfmpegControl,
    FfmpegPlaybackInput, PlaybackBlockReason, PlaybackGeneration, StreamCatalog, StreamInfo,
    TimestampMapper, load_external_subtitle_cue_list, open_subtitle_decoder, push_subtitle_cue,
    refresh_playback_timeline_origin, select_subtitle_stream_for_selection_from_catalog,
    subtitle_cue_queue_from_external, trim_overlapping_subtitle_cues_at, update_subtitle_overlay,
};

const SUBTITLE_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 16;

pub(super) struct SubtitlePipeline {
    stream: Option<StreamInfo>,
    worker: Option<SubtitleDecodeWorker>,
    packets: SubtitleDecodePacketQueues,
    external_cues: Vec<BackendSubtitleCue>,
    cues: VecDeque<BackendSubtitleCue>,
    active: Option<BackendSubtitleCue>,
    needs_prefetch: bool,
}

pub(super) struct PendingSubtitleDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
}

pub(super) struct SubtitleDecodeContext {
    pub(super) current_start_position_nsecs: u64,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
}

impl SubtitlePipeline {
    pub(super) fn new(
        stream: Option<StreamInfo>,
        decoder: Option<Decoder>,
        source: &FfmpegPlaybackInput,
        start_position_nsecs: u64,
    ) -> std::result::Result<Self, String> {
        let worker = match (stream, decoder) {
            (Some(stream), Some(decoder)) => Some(SubtitleDecodeWorker::spawn(decoder, stream)?),
            _ => None,
        };
        let external_cues = load_external_subtitle_cue_list(
            &source.selected_tracks,
            source.http_headers.as_slice(),
        )?;
        let cues = subtitle_cue_queue_from_external(&external_cues, start_position_nsecs);

        Ok(Self {
            stream,
            worker,
            packets: SubtitleDecodePacketQueues::default(),
            external_cues,
            cues,
            active: None,
            needs_prefetch: subtitle_needs_prefetch(stream),
        })
    }

    pub(super) fn switch_tracks(
        &mut self,
        source: &FfmpegPlaybackInput,
        stream_catalog: &StreamCatalog,
        video_size: Option<RenderSize>,
        start_position_nsecs: u64,
    ) -> std::result::Result<(), String> {
        let stream = select_subtitle_stream_for_selection_from_catalog(
            &source.selected_tracks,
            stream_catalog,
        )?;
        let decoder = open_subtitle_decoder(stream, video_size)?;
        let worker = match (stream, decoder) {
            (Some(stream), Some(decoder)) => Some(SubtitleDecodeWorker::spawn(decoder, stream)?),
            _ => None,
        };
        let external_cues = load_external_subtitle_cue_list(
            &source.selected_tracks,
            source.http_headers.as_slice(),
        )?;

        self.stream = stream;
        self.worker = worker;
        self.packets.clear();
        self.external_cues = external_cues;
        self.reset_cues_for_position(start_position_nsecs);
        self.needs_prefetch = subtitle_needs_prefetch(stream);
        Ok(())
    }

    pub(super) fn stream_index(&self) -> Option<c_int> {
        self.worker
            .as_ref()
            .map(|worker| worker.info().stream_index)
    }

    pub(super) fn needs_prefetch(&self) -> bool {
        self.needs_prefetch
    }

    pub(super) fn snapshot(&self) -> Option<SubtitleDecodeWorkerSnapshot> {
        self.worker.as_ref().map(|worker| {
            let mut snapshot = worker.snapshot();
            snapshot.pending_input_packets = self.packets.pending_input_count();
            snapshot.pending_input_capacity = self.packets.pending_input_capacity();
            snapshot
        })
    }

    pub(super) fn flush_decode_state(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        if let Some(worker) = self.worker.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.clear_packets();
        Ok(())
    }

    pub(super) fn reset_cues_for_position(&mut self, start_position_nsecs: u64) {
        self.cues = subtitle_cue_queue_from_external(&self.external_cues, start_position_nsecs);
        self.active = None;
    }

    pub(super) fn refresh_timeline_origin(
        &mut self,
        playback_timeline_origin_nsecs: &mut Option<u64>,
        video_clock: &TimestampMapper,
    ) {
        refresh_playback_timeline_origin(
            playback_timeline_origin_nsecs,
            video_clock,
            self.stream,
            &mut self.cues,
        );
    }

    pub(super) fn block_reason_for(
        snapshot: SubtitleDecodeWorkerSnapshot,
    ) -> Option<PlaybackBlockReason> {
        match snapshot.state {
            SubtitleDecodeWorkerState::InputFull => Some(PlaybackBlockReason::PacketQueueFull),
            _ if snapshot.pending_input_full()
                || snapshot.in_flight_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::PacketQueueFull)
            }
            SubtitleDecodeWorkerState::NeedPacket if snapshot.pending_input_packets == 0 => {
                Some(PlaybackBlockReason::DecoderInputEmpty)
            }
            _ => None,
        }
    }

    pub(super) fn try_enqueue_pending_packet(
        &mut self,
        pending_packet: PendingSubtitleDecodePacket,
        context: SubtitleDecodeContext,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        let Some(worker) = self.worker.as_mut() else {
            return Ok(DecodePacketAdmissionStatus::Dropped);
        };
        if self.packets.has_pending_input() {
            return Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id));
        }
        let enqueue_result = worker.try_enqueue_packet(
            &pending_packet.packet,
            pending_packet.generation,
            SubtitleDecodePacketContext {
                current_start_position_nsecs: context.current_start_position_nsecs,
                playback_timeline_origin_nsecs: context.playback_timeline_origin_nsecs,
            },
        )?;
        match enqueue_result {
            SubtitleDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodePacketAdmissionStatus::Queued)
            }
            SubtitleDecodeEnqueueResult::InputFull => {
                Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id))
            }
        }
    }

    pub(super) fn retry_pending_input(
        &mut self,
        context: SubtitleDecodeContext,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let Some(pending_packet) = self.take_pending_input() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        let Some(worker) = self.worker.as_mut() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        let enqueue_result = worker.try_enqueue_packet(
            &pending_packet.packet,
            pending_packet.generation,
            SubtitleDecodePacketContext {
                current_start_position_nsecs: context.current_start_position_nsecs,
                playback_timeline_origin_nsecs: context.playback_timeline_origin_nsecs,
            },
        )?;
        match enqueue_result {
            SubtitleDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodeInputRetryStatus::Queued)
            }
            SubtitleDecodeEnqueueResult::InputFull => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(session_id);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    fn buffer_pending_input_or_backpressure(
        &mut self,
        pending_packet: PendingSubtitleDecodePacket,
        session_id: PlaybackSessionId,
    ) -> DecodePacketAdmissionStatus {
        match self.packets.push_pending_input(pending_packet) {
            Ok(()) => {
                let snapshot = self.snapshot();
                tracing::trace!(
                    session_id = ?session_id,
                    subtitle_decode_pending_input_packets =
                        ?snapshot.map(|snapshot| snapshot.pending_input_packets),
                    subtitle_decode_pending_input_capacity =
                        ?snapshot.map(|snapshot| snapshot.pending_input_capacity),
                    subtitle_decode_pending_input_full =
                        ?snapshot.map(|snapshot| snapshot.pending_input_full()),
                    subtitle_decode_in_flight_packets =
                        ?snapshot.map(|snapshot| snapshot.in_flight_packets),
                    subtitle_decode_state = ?snapshot.map(|snapshot| snapshot.state),
                    "buffered FFmpeg subtitle packet in decoder wrapper input queue"
                );
                DecodePacketAdmissionStatus::Queued
            }
            Err(pending_packet) => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(session_id);
                DecodePacketAdmissionStatus::Backpressured
            }
        }
    }

    fn log_pending_input_backpressured(&self, session_id: PlaybackSessionId) {
        let snapshot = self.snapshot();
        let blocked_on = snapshot
            .and_then(Self::block_reason_for)
            .unwrap_or(PlaybackBlockReason::PacketQueueFull);
        tracing::debug!(
            session_id = ?session_id,
            blocked_on = blocked_on.as_str(),
            subtitle_decode_state = ?snapshot.map(|snapshot| snapshot.state),
            subtitle_decode_pending_input_packets =
                ?snapshot.map(|snapshot| snapshot.pending_input_packets),
            subtitle_decode_pending_input_capacity =
                ?snapshot.map(|snapshot| snapshot.pending_input_capacity),
            subtitle_decode_pending_input_full =
                ?snapshot.map(|snapshot| snapshot.pending_input_full()),
            subtitle_decode_in_flight_packets =
                ?snapshot.map(|snapshot| snapshot.in_flight_packets),
            subtitle_decode_completed_packets =
                ?snapshot.map(|snapshot| snapshot.completed_packets),
            "FFmpeg subtitle decoder wrapper input queue backpressured"
        );
    }

    pub(super) fn admit_demux_packet(
        &mut self,
        packet: &AvPacket,
        playback_generation: &mut PlaybackGeneration,
        context: SubtitleDecodeContext,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        let generation = playback_generation.advance();
        let pending_packet = PendingSubtitleDecodePacket {
            generation,
            packet: AvPacket::ref_from(packet)?,
        };
        self.try_enqueue_pending_packet(pending_packet, context, session_id)
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
        audio_output: Option<&AudioOutput>,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<Option<SubtitleDecodePacketStatus>, String> {
        let Some(worker) = self.worker.as_mut() else {
            return Ok(None);
        };
        let Some(mut status) = worker.poll_packet_status(generation)? else {
            return Ok(None);
        };
        for update in std::mem::take(&mut status.updates) {
            self.apply_decode_update(update);
        }
        if let Some(output) = audio_output {
            self.update_overlay_from_audio_clock(output, session_id, event_tx)?;
        }
        Ok(Some(status))
    }

    pub(super) fn clear_packets(&mut self) {
        self.packets.clear();
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        self.packets.has_pending_or_in_flight()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingSubtitleDecodePacket> {
        self.packets.take_pending_input()
    }

    pub(super) fn push_in_flight(&mut self, packet: PendingSubtitleDecodePacket) {
        self.packets.push_in_flight(packet);
    }

    pub(super) fn front_generation(&self) -> Option<u64> {
        self.packets.front_generation()
    }

    pub(super) fn pop_completed_packet(&mut self) -> Option<PendingSubtitleDecodePacket> {
        self.packets.pop_completed_packet()
    }

    #[allow(clippy::while_let_loop)]
    pub(super) fn drain_ready_decode_output(
        &mut self,
        audio_output: Option<&AudioOutput>,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<bool, String> {
        let mut made_progress = false;
        loop {
            let Some(front_generation) = self.front_generation() else {
                break;
            };

            let Some(status) =
                self.poll_packet_status(front_generation, audio_output, session_id, event_tx)?
            else {
                break;
            };
            let pending_packet = self
                .pop_completed_packet()
                .expect("front subtitle decode packet exists for status");
            made_progress = true;
            if status.elapsed >= DECODE_PACKET_SLOW_LOG_AFTER {
                let subtitle_decode_snapshot = self.snapshot();
                tracing::debug!(
                    session_id = ?session_id,
                    packet_pts = ?pending_packet.packet.best_timestamp(),
                    packet_bytes = pending_packet.packet.byte_len(),
                    decoded_subtitle_cues = status.decoded_cues,
                    elapsed_ms = status.elapsed.as_secs_f64() * 1000.0,
                    subtitle_decode_state =
                        ?subtitle_decode_snapshot.map(|snapshot| snapshot.state),
                    subtitle_decode_in_flight_packets =
                        ?subtitle_decode_snapshot.map(|snapshot| snapshot.in_flight_packets),
                    subtitle_decode_completed_packets =
                        ?subtitle_decode_snapshot.map(|snapshot| snapshot.completed_packets),
                    "FFmpeg subtitle decode packet completed slowly"
                );
            }
            status.result?;
            if control.has_pending_seek() {
                break;
            }
        }
        Ok(made_progress)
    }

    fn apply_decode_update(&mut self, update: SubtitleCueUpdate) {
        match update {
            SubtitleCueUpdate::Push(cue) => {
                if self.stream.is_some_and(|stream| {
                    stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE
                }) {
                    trim_overlapping_subtitle_cues_at(&mut self.cues, cue.start_nsecs);
                }
                push_subtitle_cue(&mut self.cues, cue);
            }
            SubtitleCueUpdate::TrimOverlapsAt(timeline_nsecs) => {
                trim_overlapping_subtitle_cues_at(&mut self.cues, timeline_nsecs);
            }
        }
    }

    pub(super) fn update_overlay_from_audio_clock(
        &mut self,
        output: &AudioOutput,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<(), String> {
        self.update_overlay(
            output.snapshot()?.played_timeline_nsecs,
            session_id,
            event_tx,
        );
        Ok(())
    }

    pub(super) fn update_overlay(
        &mut self,
        timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        update_subtitle_overlay(
            timeline_nsecs,
            &mut self.cues,
            &mut self.active,
            session_id,
            event_tx,
        );
    }
}

type SubtitleDecodePacketQueues =
    DecoderPacketQueues<PendingSubtitleDecodePacket, SUBTITLE_DECODE_PENDING_INPUT_QUEUE_CAPACITY>;

impl SubtitleDecodePacketQueues {
    fn front_generation(&self) -> Option<u64> {
        self.front_in_flight().map(|packet| packet.generation)
    }
}

fn subtitle_needs_prefetch(stream: Option<StreamInfo>) -> bool {
    stream.is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
}

#[cfg(test)]
mod tests {
    use super::{
        PlaybackBlockReason, SUBTITLE_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
        SubtitleDecodeWorkerSnapshot, SubtitleDecodeWorkerState, SubtitlePipeline,
    };

    fn snapshot(
        state: SubtitleDecodeWorkerState,
        pending_input_packets: usize,
    ) -> SubtitleDecodeWorkerSnapshot {
        SubtitleDecodeWorkerSnapshot {
            state,
            pending_input_packets,
            pending_input_capacity: SUBTITLE_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            in_flight_packets: 0,
            command_queue_capacity: 4,
            completed_packets: 0,
        }
    }

    #[test]
    fn full_pending_subtitle_decode_input_reports_packet_queue_full() {
        let reason = SubtitlePipeline::block_reason_for(snapshot(
            SubtitleDecodeWorkerState::NeedPacket,
            SUBTITLE_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
        ));

        assert_eq!(reason, Some(PlaybackBlockReason::PacketQueueFull));
    }

    #[test]
    fn non_full_pending_subtitle_decode_input_is_not_packet_queue_full() {
        let reason =
            SubtitlePipeline::block_reason_for(snapshot(SubtitleDecodeWorkerState::NeedPacket, 1));

        assert_eq!(reason, None);
    }
}
