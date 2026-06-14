use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::video_decode_worker::{
    VideoDecodeDrainResult, VideoDecodeEnqueueResult, VideoDecodePacketStatus, VideoDecodeWorker,
    VideoDecodeWorkerInfo, VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, VideoDecodedFrame,
};
use super::*;

const VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 8;

pub(super) struct PendingVideoDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
    pub(super) realign_after_decode_recovery: bool,
}

pub(super) struct VideoPacketAdmissionContext {
    pub(super) session_id: PlaybackSessionId,
    pub(super) video_stream: StreamInfo,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) skip_nonref_for_pressure: bool,
    pub(super) played_until_nsecs: Option<u64>,
}

#[derive(Clone, Copy)]
pub(super) struct VideoPacketAdmissionPressure {
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) skip_nonref_for_pressure: bool,
    pub(super) played_until_nsecs: Option<u64>,
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct VideoDecodeRecovery {
    waiting_for_keyframe: bool,
    realign_on_next_frame: bool,
    realign_after_recovery_point: bool,
    skipped_packets: u64,
}

impl VideoDecodeRecovery {
    pub(in crate::player::backend::ffmpeg) fn reset(&mut self) {
        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = false;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn reset_for_timeline_start(
        &mut self,
        codec_id: ffi::AVCodecID,
        current_start_position_nsecs: u64,
    ) {
        self.reset();
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC && current_start_position_nsecs > 0 {
            self.begin_with_realign(false);
        }
    }

    pub(in crate::player::backend::ffmpeg) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(in crate::player::backend::ffmpeg) fn skipped_packets(&self) -> u64 {
        self.skipped_packets
    }

    pub(in crate::player::backend::ffmpeg) fn should_skip_packet(
        &self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe || packet_is_video_decode_recovery_point(packet, codec_id) {
            return false;
        }
        codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            || self.skipped_packets < VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
    }

    pub(in crate::player::backend::ffmpeg) fn record_skipped_packet(&mut self) -> u64 {
        self.skipped_packets = self.skipped_packets.saturating_add(1);
        self.skipped_packets
    }

    pub(in crate::player::backend::ffmpeg) fn accept_recovery_point(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe || !packet_is_video_decode_recovery_point(packet, codec_id) {
            return false;
        }

        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = self.realign_after_recovery_point;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        true
    }

    pub(in crate::player::backend::ffmpeg) fn accept_after_wait_limit(
        &mut self,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return false;
        }
        if !self.waiting_for_keyframe
            || self.skipped_packets < VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
        {
            return false;
        }

        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = self.realign_after_recovery_point;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        true
    }

    pub(in crate::player::backend::ffmpeg) fn take_realign_on_next_frame(&mut self) -> bool {
        let realign = self.realign_on_next_frame;
        self.realign_on_next_frame = false;
        realign
    }

    pub(in crate::player::backend::ffmpeg) fn begin_with_realign(
        &mut self,
        realign_after_recovery_point: bool,
    ) {
        self.waiting_for_keyframe = true;
        self.realign_on_next_frame = false;
        self.realign_after_recovery_point = realign_after_recovery_point;
        self.skipped_packets = 0;
    }
}

fn packet_is_video_decode_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return packet_is_video_seek_point(packet, codec_id);
    }
    packet_is_video_recovery_point(packet, codec_id)
}

pub(super) struct VideoDecodePipeline {
    worker: VideoDecodeWorker,
    packets: VideoDecodePacketQueues,
}

impl VideoDecodePipeline {
    pub(super) fn spawn(decoder: Decoder) -> std::result::Result<Self, String> {
        Ok(Self {
            worker: VideoDecodeWorker::spawn(decoder)?,
            packets: VideoDecodePacketQueues::default(),
        })
    }

    pub(super) fn info(&self) -> &VideoDecodeWorkerInfo {
        self.worker.info()
    }

    pub(super) fn snapshot(&self) -> VideoDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        snapshot.pending_input_packets = self.packets.pending_input_count();
        snapshot.pending_input_capacity = self.packets.pending_input_capacity();
        snapshot
    }

    pub(super) fn block_reason_for(
        snapshot: VideoDecodeWorkerSnapshot,
        info: &VideoDecodeWorkerInfo,
    ) -> Option<PlaybackBlockReason> {
        match snapshot.state {
            VideoDecodeWorkerState::OutputFull if info.hardware_accelerated => {
                Some(PlaybackBlockReason::HwSurfacePool)
            }
            VideoDecodeWorkerState::OutputFull => Some(PlaybackBlockReason::DecodedQueueFull),
            _ if snapshot.pending_input_full()
                || snapshot.in_flight_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::PacketQueueFull)
            }
            VideoDecodeWorkerState::NeedPacket if snapshot.pending_input_packets == 0 => {
                Some(PlaybackBlockReason::DecoderInputEmpty)
            }
            _ => None,
        }
    }

    pub(super) fn set_skip_nonref_frames(
        &mut self,
        enabled: bool,
    ) -> std::result::Result<(), String> {
        self.worker.set_skip_nonref_frames(enabled)
    }

    pub(super) fn try_enqueue_packet(
        &mut self,
        packet: &AvPacket,
        generation: u64,
    ) -> std::result::Result<VideoDecodeEnqueueResult, String> {
        self.worker.try_enqueue_packet(packet, generation)
    }

    pub(super) fn try_enqueue_pending_packet(
        &mut self,
        pending_packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if self.packets.has_pending_input() {
            return Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id));
        }
        let enqueue_result =
            self.try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            VideoDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodePacketAdmissionStatus::Queued)
            }
            VideoDecodeEnqueueResult::InputFull | VideoDecodeEnqueueResult::OutputFull => {
                Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id))
            }
        }
    }

    pub(super) fn retry_pending_input(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let Some(pending_packet) = self.take_pending_input() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        let enqueue_result =
            self.try_enqueue_packet(&pending_packet.packet, pending_packet.generation)?;
        match enqueue_result {
            VideoDecodeEnqueueResult::Queued => {
                self.push_in_flight(pending_packet);
                Ok(DecodeInputRetryStatus::Queued)
            }
            VideoDecodeEnqueueResult::InputFull | VideoDecodeEnqueueResult::OutputFull => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(session_id, enqueue_result);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    fn buffer_pending_input_or_backpressure(
        &mut self,
        pending_packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) -> DecodePacketAdmissionStatus {
        match self.packets.push_pending_input(pending_packet) {
            Ok(()) => {
                let snapshot = self.snapshot();
                tracing::trace!(
                    session_id = ?session_id,
                    video_decode_pending_input_packets = snapshot.pending_input_packets,
                    video_decode_pending_input_capacity =
                        snapshot.pending_input_capacity,
                    video_decode_pending_input_full = snapshot.pending_input_full(),
                    video_decode_in_flight_packets = snapshot.in_flight_packets,
                    video_decode_state = ?snapshot.state,
                    "buffered FFmpeg video packet in decoder wrapper input queue"
                );
                DecodePacketAdmissionStatus::Queued
            }
            Err(pending_packet) => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(
                    session_id,
                    VideoDecodeEnqueueResult::InputFull,
                );
                DecodePacketAdmissionStatus::Backpressured
            }
        }
    }

    fn log_pending_input_backpressured(
        &self,
        session_id: PlaybackSessionId,
        enqueue_result: VideoDecodeEnqueueResult,
    ) {
        let snapshot = self.snapshot();
        let blocked_on =
            Self::block_reason_for(snapshot, self.info()).unwrap_or(match enqueue_result {
                VideoDecodeEnqueueResult::InputFull => PlaybackBlockReason::PacketQueueFull,
                VideoDecodeEnqueueResult::OutputFull if self.info().hardware_accelerated => {
                    PlaybackBlockReason::HwSurfacePool
                }
                VideoDecodeEnqueueResult::OutputFull => PlaybackBlockReason::DecodedQueueFull,
                VideoDecodeEnqueueResult::Queued => PlaybackBlockReason::OutputGate,
            });
        tracing::debug!(
            session_id = ?session_id,
            blocked_on = blocked_on.as_str(),
            video_decode_state = ?snapshot.state,
            video_decode_queued_frames = snapshot.queued_frames,
            video_decode_queue_capacity = snapshot.queue_capacity,
            video_decode_pending_input_packets = snapshot.pending_input_packets,
            video_decode_pending_input_capacity = snapshot.pending_input_capacity,
            video_decode_pending_input_full = snapshot.pending_input_full(),
            video_decode_in_flight_packets = snapshot.in_flight_packets,
            video_decode_completed_packets = snapshot.completed_packets,
            "FFmpeg video decoder wrapper input queue backpressured"
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn admit_demux_packet(
        &mut self,
        packet: &AvPacket,
        video_packet_count: &mut u64,
        playback_generation: &mut PlaybackGeneration,
        recovery: &mut VideoDecodeRecovery,
        dovi_pipeline: &mut DoviPipeline,
        skip_nonref_active: &mut bool,
        context: VideoPacketAdmissionContext,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        *video_packet_count = video_packet_count.saturating_add(1);
        let codec_id = context.video_stream.codec_id;
        if recovery.should_skip_packet(packet, codec_id) {
            let skipped_packets = recovery.record_skipped_packet();
            if skipped_packets == 1 || skipped_packets.is_multiple_of(60) {
                tracing::debug!(
                    pts = ?packet.best_timestamp(),
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    skipped_packets,
                    "skipping FFmpeg video packets while waiting for decode recovery point"
                );
            }
            return Ok(DecodePacketAdmissionStatus::Dropped);
        }

        if recovery.accept_recovery_point(packet, codec_id) {
            tracing::debug!(
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                codec = ?codec_id,
                packet_bytes = packet.byte_len(),
                recovery_point = packet_is_video_recovery_point(packet, codec_id),
                safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                "resuming FFmpeg video decode at recovery point"
            );
            let generation = playback_generation.advance();
            self.flush_buffers(generation)?;
        } else if recovery.accept_after_wait_limit(codec_id) {
            tracing::debug!(
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                codec = ?codec_id,
                packet_bytes = packet.byte_len(),
                recovery_point = packet_is_video_recovery_point(packet, codec_id),
                safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                max_skipped_packets = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
                "resuming FFmpeg video decode after recovery point wait limit"
            );
            let generation = playback_generation.advance();
            self.flush_buffers(generation)?;
        }

        log_video_decode_packet_if_needed(packet, codec_id, *video_packet_count, recovery);
        let stripped_video_packet = strip_hevc_dovi_rpu_decode_packet(
            packet,
            codec_id,
            HevcDecodePacketLogContext {
                video_packet_count: *video_packet_count,
                first_video_frame_pending: context.output_snapshot.first_video_frame_pending,
                recovery_waiting: recovery.waiting_for_keyframe(),
            },
        )?;
        if let Some(metadata) = stripped_video_packet
            .as_ref()
            .and_then(|packet| packet.metadata.clone())
        {
            tracing::trace!(
                pts = ?packet.best_timestamp(),
                profile = metadata.profile,
                profile5 = metadata.is_profile5(),
                rpu_bytes = metadata.rpu_payload.len(),
                "using stripped Dolby Vision RPU metadata for FFmpeg packet"
            );
            dovi_pipeline.observe_video_packet_metadata(packet, context.video_stream, metadata);
        } else {
            dovi_pipeline.observe_video_packet(packet, context.video_stream);
        }

        let skip_nonref_for_pressure = context.skip_nonref_for_pressure;
        if skip_nonref_for_pressure != *skip_nonref_active {
            self.set_skip_nonref_frames(skip_nonref_for_pressure)?;
            *skip_nonref_active = skip_nonref_for_pressure;
            tracing::debug!(
                session_id = ?context.session_id,
                skip_nonref_for_pressure,
                output_state = ?context.output_snapshot.state,
                played_until_nsecs = context.played_until_nsecs,
                queued_video_frames = context.output_snapshot.queued_video_frames,
                queued_video_ms = context.output_snapshot.queued_video_duration_nsecs as f64
                    / 1_000_000.0,
                decoded_video_range = ?context.output_snapshot.queued_video_range_nsecs,
                decoded_video_forward_ms = ?context
                    .output_snapshot
                    .queued_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "updated FFmpeg video decoder non-reference frame skipping for decode pressure"
            );
        }

        if stripped_video_packet
            .as_ref()
            .is_some_and(|stripped| stripped.packet.byte_len() == 0)
        {
            return Ok(DecodePacketAdmissionStatus::Dropped);
        }

        let generation = playback_generation.advance();
        let decode_packet = stripped_video_packet
            .as_ref()
            .map_or(packet, |stripped| &stripped.packet);
        let pending_packet = PendingVideoDecodePacket {
            generation,
            packet: AvPacket::ref_from(decode_packet)?,
            realign_after_decode_recovery: context.output_snapshot.first_video_frame_pending,
        };
        self.try_enqueue_pending_packet(pending_packet, context.session_id)
    }

    pub(super) fn recover_error_if_needed(
        &mut self,
        result: std::result::Result<(), String>,
        playback_generation: &mut PlaybackGeneration,
        codec_id: ffi::AVCodecID,
        packet: &AvPacket,
        recovery: &mut VideoDecodeRecovery,
        realign_after_recovery_point: bool,
    ) -> std::result::Result<(), String> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if video_decode_error_is_recoverable(&error) => {
                let resource_pressure = video_decode_error_is_resource_pressure(&error);
                tracing::debug!(
                    %error,
                    codec = ?codec_id,
                    packet_pts = ?packet.best_timestamp(),
                    packet_keyframe = packet.is_key(),
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    recovery_waiting_before = recovery.waiting_for_keyframe(),
                    recovery_skipped_packets = recovery.skipped_packets,
                    realign_after_recovery_point,
                    resource_pressure,
                    "recovering FFmpeg video decoder after recoverable decode error"
                );
                let generation = playback_generation.advance();
                self.flush_buffers(generation)?;
                recovery.begin_with_realign(realign_after_recovery_point);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodedFrame>, String> {
        self.worker.poll_frame(generation)
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodePacketStatus>, String> {
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
    ) -> std::result::Result<Option<VideoDecodeDrainResult>, String> {
        self.worker.poll_drain_result(generation)
    }

    pub(super) fn clear_packets(&mut self) {
        self.packets.clear();
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        self.packets.has_pending_or_in_flight()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingVideoDecodePacket> {
        self.packets.take_pending_input()
    }

    pub(super) fn push_in_flight(&mut self, packet: PendingVideoDecodePacket) {
        self.packets.push_in_flight(packet);
    }

    pub(super) fn front_generation(&self) -> Option<u64> {
        self.packets.front_generation()
    }

    pub(super) fn front_realign_after_decode_recovery(&self, fallback: bool) -> bool {
        self.packets.front_realign_after_decode_recovery(fallback)
    }

    pub(super) fn front_packet(&self) -> Option<&AvPacket> {
        self.packets.front_packet()
    }

    pub(super) fn pop_completed_packet(&mut self) -> Option<PendingVideoDecodePacket> {
        self.packets.pop_completed_packet()
    }
}

type VideoDecodePacketQueues =
    DecoderPacketQueues<PendingVideoDecodePacket, VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY>;

impl VideoDecodePacketQueues {
    pub(super) fn front_generation(&self) -> Option<u64> {
        self.front_in_flight().map(|packet| packet.generation)
    }

    pub(super) fn front_realign_after_decode_recovery(&self, fallback: bool) -> bool {
        self.front_in_flight()
            .map(|packet| packet.realign_after_decode_recovery)
            .unwrap_or(fallback)
    }

    pub(super) fn front_packet(&self) -> Option<&AvPacket> {
        self.front_in_flight().map(|packet| &packet.packet)
    }
}

fn strip_hevc_dovi_rpu_decode_packet(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
    log_context: HevcDecodePacketLogContext,
) -> std::result::Result<Option<StrippedDoviDecodePacket>, String> {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return Ok(None);
    }
    let Some(data) = packet.data() else {
        return Ok(None);
    };
    let Some(stripped) = strip_dovi_rpu_nalus(data) else {
        if should_debug_hevc_decode_packet_without_rpu(log_context) {
            tracing::debug!(
                packet_count = log_context.video_packet_count,
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                first_video_frame_pending = log_context.first_video_frame_pending,
                recovery_waiting = log_context.recovery_waiting,
                original_nals = %hevc_nal_summary(data, None),
                "HEVC decode packet has no stripped Dolby Vision RPU NALs"
            );
        } else if should_trace_hevc_decode_packet_nals(packet, log_context) {
            tracing::trace!(
                packet_count = log_context.video_packet_count,
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                first_video_frame_pending = log_context.first_video_frame_pending,
                recovery_waiting = log_context.recovery_waiting,
                original_nals = %hevc_nal_summary(data, None),
                "HEVC decode packet has no stripped Dolby Vision RPU NALs"
            );
        }
        return Ok(None);
    };

    if should_debug_stripped_hevc_dovi_packet(log_context, &stripped) {
        tracing::debug!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_packet_bytes = stripped.data.len(),
            stripped_bytes = stripped.stripped_bytes,
            nal_count = stripped.nal_count,
            stripped_nal_count = stripped.stripped_nal_count,
            stream_format = ?stripped.stream_format,
            rpu_metadata = stripped.metadata.is_some(),
            rpu_profile = ?stripped.metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?stripped.metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            first_video_frame_pending = log_context.first_video_frame_pending,
            recovery_waiting = log_context.recovery_waiting,
            original_nals = %hevc_nal_summary(data, Some(stripped.stream_format)),
            stripped_nals = %hevc_nal_summary(&stripped.data, Some(stripped.stream_format)),
            "stripped Dolby Vision RPU NALs before HEVC decode"
        );
    } else if should_trace_hevc_decode_packet_nals(packet, log_context) {
        tracing::trace!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_packet_bytes = stripped.data.len(),
            stripped_bytes = stripped.stripped_bytes,
            nal_count = stripped.nal_count,
            stripped_nal_count = stripped.stripped_nal_count,
            stream_format = ?stripped.stream_format,
            rpu_metadata = stripped.metadata.is_some(),
            rpu_profile = ?stripped.metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?stripped.metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            original_nals = %hevc_nal_summary(data, Some(stripped.stream_format)),
            stripped_nals = %hevc_nal_summary(&stripped.data, Some(stripped.stream_format)),
            "stripped Dolby Vision RPU NALs before HEVC decode"
        );
    }

    AvPacket::from_data_and_props(&stripped.data, packet).map(|packet| {
        Some(StrippedDoviDecodePacket {
            packet,
            metadata: stripped.metadata,
        })
    })
}

struct StrippedDoviDecodePacket {
    packet: AvPacket,
    metadata: Option<DoviFrameMetadata>,
}

#[derive(Clone, Copy)]
struct HevcDecodePacketLogContext {
    video_packet_count: u64,
    first_video_frame_pending: bool,
    recovery_waiting: bool,
}

fn should_debug_hevc_decode_packet_without_rpu(context: HevcDecodePacketLogContext) -> bool {
    context.recovery_waiting
}

fn should_debug_stripped_hevc_dovi_packet(
    context: HevcDecodePacketLogContext,
    stripped: &DoviRpuStripResult,
) -> bool {
    context.recovery_waiting || stripped.metadata.is_none()
}

fn should_trace_hevc_decode_packet_nals(
    packet: &AvPacket,
    context: HevcDecodePacketLogContext,
) -> bool {
    context.first_video_frame_pending
        || context.recovery_waiting
        || packet.is_key()
        || context.video_packet_count == 1
        || context.video_packet_count.is_multiple_of(120)
}

fn hevc_nal_summary(data: &[u8], format_hint: Option<HevcStreamFormat>) -> String {
    let format = format_hint.or_else(|| detect_hevc_stream_format(data));
    match format {
        Some(HevcStreamFormat::ByteStream) => hevc_annex_b_nal_summary(data),
        Some(HevcStreamFormat::LengthPrefixed { length_size }) => {
            hevc_length_prefixed_nal_summary(data, length_size)
        }
        None => format!("format=unknown;bytes={}", data.len()),
    }
}

fn detect_hevc_stream_format(data: &[u8]) -> Option<HevcStreamFormat> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Some(HevcStreamFormat::ByteStream);
    }
    for length_size in [4, 3, 2, 1] {
        if hevc_length_prefixed_nal_types(data, length_size).is_some() {
            return Some(HevcStreamFormat::LengthPrefixed { length_size });
        }
    }
    if data.windows(3).any(|window| window == [0, 0, 1])
        || data.windows(4).any(|window| window == [0, 0, 0, 1])
    {
        return Some(HevcStreamFormat::ByteStream);
    }
    None
}

fn hevc_length_prefixed_nal_types(
    data: &[u8],
    length_size: usize,
) -> Option<Vec<(Option<u8>, usize)>> {
    let mut offset = 0usize;
    let mut nals = Vec::new();
    while offset < data.len() {
        let length_end = offset.checked_add(length_size)?;
        if length_end > data.len() {
            return None;
        }
        let mut nal_len = 0usize;
        for byte in &data[offset..length_end] {
            nal_len = nal_len.checked_shl(8)?.checked_add(usize::from(*byte))?;
        }
        if nal_len == 0 {
            return None;
        }
        let nal_start = length_end;
        let nal_end = nal_start.checked_add(nal_len)?;
        if nal_end > data.len() {
            return None;
        }
        let nal = trim_hevc_nal_trailing_zeroes(&data[nal_start..nal_end]);
        nals.push((nal.first().map(|header| (header >> 1) & 0x3f), nal.len()));
        offset = nal_end;
    }
    Some(nals)
}

fn hevc_length_prefixed_nal_summary(data: &[u8], length_size: usize) -> String {
    match hevc_length_prefixed_nal_types(data, length_size) {
        Some(nals) => format_hevc_nal_summary(
            format!("length_prefixed({length_size})"),
            data.len(),
            &nals,
            None,
        ),
        None => format!(
            "format=length_prefixed({length_size});bytes={};parse_error=true",
            data.len()
        ),
    }
}

fn hevc_annex_b_nal_summary(data: &[u8]) -> String {
    let mut cursor = 0usize;
    let mut nals = Vec::new();
    while let Some((start_code_pos, start_code_len)) = find_hevc_start_code(data, cursor) {
        let nal_start = start_code_pos.saturating_add(start_code_len);
        let nal_end = find_hevc_start_code(data, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(data.len());
        let nal = trim_hevc_nal_trailing_zeroes(&data[nal_start..nal_end]);
        if !nal.is_empty() {
            nals.push((nal.first().map(|header| (header >> 1) & 0x3f), nal.len()));
        }
        cursor = nal_end;
    }
    let parse_error = nals.is_empty().then_some("no_start_code_nals");
    format_hevc_nal_summary("annex_b".to_string(), data.len(), &nals, parse_error)
}

fn format_hevc_nal_summary(
    format: String,
    bytes: usize,
    nals: &[(Option<u8>, usize)],
    parse_error: Option<&'static str>,
) -> String {
    const NAL_SUMMARY_LIMIT: usize = 16;
    let rpu_nals = nals
        .iter()
        .filter(|(nal_type, _)| *nal_type == Some(62))
        .count();
    let nal_parts = nals
        .iter()
        .take(NAL_SUMMARY_LIMIT)
        .enumerate()
        .map(|(index, (nal_type, len))| format!("{index}:{nal_type:?}/{len}"))
        .collect::<Vec<_>>()
        .join(",");
    let truncated = if nals.len() > NAL_SUMMARY_LIMIT {
        ";truncated=true"
    } else {
        ""
    };
    let parse_error = parse_error
        .map(|error| format!(";parse_error={error}"))
        .unwrap_or_default();
    format!(
        "format={format};bytes={bytes};count={};rpu62={rpu_nals};nals=[{nal_parts}]{truncated}{parse_error}",
        nals.len()
    )
}

fn find_hevc_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index = index.saturating_add(1);
    }
    None
}

fn trim_hevc_nal_trailing_zeroes(nal: &[u8]) -> &[u8] {
    let mut end = nal.len();
    while end > 0 && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[..end]
}

fn log_video_decode_packet_if_needed(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
    video_packet_count: u64,
    recovery: &VideoDecodeRecovery,
) {
    let recovery_point = packet_is_video_recovery_point(packet, codec_id);
    let safe_seek_point = packet_is_video_seek_point(packet, codec_id);
    if video_packet_count != 1
        && !video_packet_count.is_multiple_of(120)
        && !recovery.waiting_for_keyframe()
        && !packet.is_key()
        && !recovery_point
        && !safe_seek_point
    {
        return;
    }

    tracing::debug!(
        packet_count = video_packet_count,
        pts = ?packet.best_timestamp(),
        keyframe = packet.is_key(),
        codec = ?codec_id,
        packet_bytes = packet.byte_len(),
        recovery_point,
        safe_seek_point,
        recovery_waiting = recovery.waiting_for_keyframe(),
        recovery_skipped_packets = recovery.skipped_packets(),
        "decoding FFmpeg video packet"
    );
}

pub(in crate::player::backend::ffmpeg) fn video_decode_error_is_recoverable(error: &str) -> bool {
    error == CORRUPT_VIDEO_FRAME_RECOVERY_ERROR
        || error.starts_with("FFmpeg 发送解码包失败")
        || error.starts_with("FFmpeg 接收解码帧失败")
}

fn video_decode_error_is_resource_pressure(error: &str) -> bool {
    error.contains("Cannot allocate memory") || error.contains("VK_ERROR_OUT_OF_DEVICE_MEMORY")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(
        state: VideoDecodeWorkerState,
        pending_input_packets: usize,
        in_flight_packets: usize,
    ) -> VideoDecodeWorkerSnapshot {
        VideoDecodeWorkerSnapshot {
            state,
            queued_frames: 0,
            queue_capacity: VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES,
            pending_input_packets,
            pending_input_capacity: VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            in_flight_packets,
            command_queue_capacity: 4,
            completed_packets: 0,
        }
    }

    fn worker_info(hardware_accelerated: bool) -> VideoDecodeWorkerInfo {
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        VideoDecodeWorkerInfo {
            stream_index: 0,
            time_base: ffi::AVRational { num: 1, den: 1 },
            size: Some(size),
            decoder_name: "test".to_string(),
            hardware_accelerated,
            vulkan_device: None,
            convert_context: VideoFrameConvertContext::new_for_test(size),
        }
    }

    #[test]
    fn full_pending_video_decode_input_reports_packet_queue_full() {
        let info = worker_info(false);
        let reason = VideoDecodePipeline::block_reason_for(
            snapshot(
                VideoDecodeWorkerState::NeedPacket,
                VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
                0,
            ),
            &info,
        );

        assert_eq!(reason, Some(PlaybackBlockReason::PacketQueueFull));
    }

    #[test]
    fn in_flight_video_decode_command_queue_reports_packet_queue_full() {
        let info = worker_info(false);
        let reason = VideoDecodePipeline::block_reason_for(
            snapshot(VideoDecodeWorkerState::Decoding, 0, 4),
            &info,
        );

        assert_eq!(reason, Some(PlaybackBlockReason::PacketQueueFull));
    }

    #[test]
    fn empty_video_decode_input_reports_decoder_input_empty() {
        let info = worker_info(false);
        let reason = VideoDecodePipeline::block_reason_for(
            snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
            &info,
        );

        assert_eq!(reason, Some(PlaybackBlockReason::DecoderInputEmpty));
    }

    #[test]
    fn output_full_video_decode_reports_surface_or_decoded_queue() {
        let software = worker_info(false);
        let hardware = worker_info(true);

        assert_eq!(
            VideoDecodePipeline::block_reason_for(
                snapshot(VideoDecodeWorkerState::OutputFull, 0, 0),
                &software,
            ),
            Some(PlaybackBlockReason::DecodedQueueFull)
        );
        assert_eq!(
            VideoDecodePipeline::block_reason_for(
                snapshot(VideoDecodeWorkerState::OutputFull, 0, 0),
                &hardware,
            ),
            Some(PlaybackBlockReason::HwSurfacePool)
        );
    }
}
