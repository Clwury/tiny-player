use ffmpeg_sys_next as ffi;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::player::{
    dovi::{
        DoviFrameMetadata, DoviRpuNalInspection, HevcStreamFormat, inspect_dovi_rpu_nalus,
        strip_dovi_rpu_nalus,
    },
    render_host::PlaybackSessionId,
};

use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::scheduled_video_queue::{
    VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS, video_timestamp_gap_within_threshold,
};
use super::video_decode_worker::{
    VideoDecodeDrainResult, VideoDecodeEnqueueResult, VideoDecodePacketStatus, VideoDecodeWorker,
    VideoDecodeWorkerInfo, VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, VideoDecodedFrame,
};
use super::video_frame_prepare_worker::DecodedVideoFrameDiagnostic;
use super::{
    AvPacket, AvPacketReadDiagnostic, CORRUPT_VIDEO_FRAME_RECOVERY_ERROR, Decoder,
    DemuxReaderWatermark, DoviPipeline, HardwareDecodeMode, PlaybackBlockReason,
    PlaybackGeneration, PlaybackOutputSnapshot, StreamInfo,
    VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, duration_nsecs,
    packet_is_video_recovery_point, packet_is_video_seek_point, timestamp_to_nsecs,
};

const VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 8;
const HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT: u64 = 24;
const HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT: u64 = 30;
const HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS: u64 = 500_000_000;
const HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS: u64 = 1_000_000_000;
const HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS: u64 = 1_000_000_000;
const HEVC_POST_FALLBACK_REBUFFER_UNDERFILL_NSECS: u64 = 250_000_000;
const HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER: Duration = Duration::from_millis(1_500);
const HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT: u64 = 32;
const HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER: Duration = Duration::from_millis(2_000);
const HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_BASE_AFTER: Duration = Duration::from_millis(8_000);
const HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_MAX_AFTER: Duration = Duration::from_millis(30_000);
const HEVC_STARTUP_ZERO_OUTPUT_HARD_MIN_FORWARD_NSECS: u64 = 1_000_000_000;
const HEVC_STARTUP_IN_FLIGHT_HARD_AFTER: Duration = Duration::from_millis(2_000);
const HEVC_STARTUP_WATCHDOG_RETRY_AFTER: Duration = Duration::from_millis(25);
const HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
const HEVC_STARTUP_PROBE_PACKET_LIMIT: usize = 32;
const HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS: u64 = 500_000_000;
const HEVC_HARDWARE_RECOVERY_PROGRESS_GRACE: Duration = Duration::from_millis(750);
const HEVC_SOFTWARE_RECOVERY_PROGRESS_GRACE: Duration = Duration::from_millis(2_000);
const HEVC_FALLBACK_SAME_TARGET_TOLERANCE_NSECS: u64 = 500_000_000;
const HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS: u64 = 500_000_000;
const HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY: usize = 32;

pub(super) struct PendingVideoDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
    pub(super) realign_after_decode_recovery: bool,
    hevc_startup_in_flight_watchdog: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodeChainRecoveryAction {
    None,
    SoftRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodedFrameGapAction {
    Admit,
    AdmitSynchronizedTimelineGap,
    AdmitAndBridgeDecodeGap,
    DeferFallback,
    DropForFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioTimelineGapEvidence {
    pub(super) previous_end_nsecs: u64,
    pub(super) next_start_nsecs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodeChainFallbackReason {
    ZeroOutputRebuffer,
    StartupInFlightStall,
    PtsGapAfterZeroOutput,
    RecoveryWaitRebuffer,
    PostFallbackRebufferUnderfill,
}

impl HevcDecodeChainFallbackReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ZeroOutputRebuffer => "hevc_decode_chain_zero_output_rebuffer",
            Self::StartupInFlightStall => "hevc_decode_chain_startup_in_flight_stall",
            Self::PtsGapAfterZeroOutput => "hevc_decode_chain_pts_gap",
            Self::RecoveryWaitRebuffer => "hevc_decode_chain_recovery_wait_rebuffer",
            Self::PostFallbackRebufferUnderfill => {
                "hevc_decode_chain_post_fallback_rebuffer_underfill"
            }
        }
    }

    pub(super) fn requires_boundary_reset(self) -> bool {
        matches!(
            self,
            Self::ZeroOutputRebuffer
                | Self::StartupInFlightStall
                | Self::RecoveryWaitRebuffer
                | Self::PostFallbackRebufferUnderfill
                | Self::PtsGapAfterZeroOutput
        )
    }

    pub(super) fn invalidated_by_video_progress(self) -> bool {
        true
    }

    pub(super) fn requires_repeat_before_hardware_downgrade(self) -> bool {
        matches!(
            self,
            Self::RecoveryWaitRebuffer | Self::PostFallbackRebufferUnderfill
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HevcDecodeChainFallback {
    pub(super) target_nsecs: u64,
    pub(super) reason: HevcDecodeChainFallbackReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodeChainFallbackLoopAction {
    Proceed,
    ForceSoftware,
    SuppressLowLevelSeek,
    ForceLowLevelSeek,
    RecoveryExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HevcDecodeChainFallbackRecord {
    root_target_nsecs: u64,
    last_target_nsecs: u64,
    last_reason: HevcDecodeChainFallbackReason,
    hardware_accelerated: bool,
    recorded_at: Instant,
    software_suppressions: u8,
    post_low_level_suppressions: u8,
    low_level_seeks: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcDecodeChainResetScope {
    Transient,
    RecoveryTransaction,
}

pub(super) struct HevcDecodePacketObservation<'a> {
    pub(super) status: &'a VideoDecodePacketStatus,
    pub(super) packet: &'a AvPacket,
    pub(super) video_stream: StreamInfo,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) has_audio_output: bool,
    pub(super) fallback_target_nsecs: u64,
    pub(super) session_id: PlaybackSessionId,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcDecodedFrameGapObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
    pub(super) previous_expected_next_nsecs: Option<u64>,
    pub(super) previous_gap_nsecs: Option<i128>,
    pub(super) max_gap_nsecs: u64,
    pub(super) fallback_target_nsecs: u64,
    pub(super) audio_played_timeline_nsecs: Option<u64>,
    pub(super) audio_timeline_gap: Option<AudioTimelineGapEvidence>,
    pub(super) recovery_waiting: bool,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) source_frame_diagnostic: DecodedVideoFrameDiagnostic,
    pub(super) recent_cache_read_anomaly: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcSeekPrerollProgressObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) frame_timeline_nsecs: u64,
    pub(super) target_nsecs: u64,
    pub(super) preroll_frames: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcAdmittedVideoProgressObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) frame_timeline_nsecs: u64,
    pub(super) current_start_position_nsecs: u64,
    pub(super) before_queue_end_nsecs: Option<u64>,
    pub(super) after_queue_end_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcPostFallbackRebufferObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) now: Instant,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) audio_ready: bool,
    pub(super) fallback_target_nsecs: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HevcDecodeChainStats {
    pub(super) zero_output_packets: u64,
    pub(super) recent_zero_output_packets: u64,
    pub(super) first_zero_output_packet_nsecs: Option<u64>,
    pub(super) last_video_packet_nsecs: Option<u64>,
    pub(super) last_decoded_video_end_nsecs: Option<u64>,
    pub(super) soft_recovery_attempted: bool,
    pub(super) recent_soft_recovery_attempted: bool,
    pub(super) pending_fallback_reason: Option<HevcDecodeChainFallbackReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HevcPacketDiagnosticFields {
    stream_index: i32,
    pts: Option<i64>,
    dts: Option<i64>,
    pts_nsecs: Option<u64>,
    dts_nsecs: Option<u64>,
    duration: Option<i64>,
    duration_nsecs: Option<u64>,
    flags: i32,
    key_frame: bool,
    recovery_point: bool,
    safe_seek_point: bool,
    byte_len: usize,
    cache_read: Option<AvPacketReadDiagnostic>,
}

impl HevcPacketDiagnosticFields {
    fn from_packet(
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        time_base: ffi::AVRational,
    ) -> Self {
        let pts = packet.pts();
        let dts = packet.dts();
        let duration = packet.duration();
        let cache_read = packet.read_diagnostic();
        Self {
            stream_index: packet.stream_index(),
            pts,
            dts,
            pts_nsecs: pts.and_then(|pts| timestamp_to_nsecs(pts, time_base)),
            dts_nsecs: dts.and_then(|dts| timestamp_to_nsecs(dts, time_base)),
            duration,
            duration_nsecs: duration.and_then(|duration| timestamp_to_nsecs(duration, time_base)),
            flags: packet.flags(),
            key_frame: packet.is_key(),
            recovery_point: cache_read
                .map(|cache| cache.recovery_point)
                .unwrap_or_else(|| packet_is_video_recovery_point(packet, codec_id)),
            safe_seek_point: cache_read
                .map(|cache| cache.safe_seek_point)
                .unwrap_or_else(|| packet_is_video_seek_point(packet, codec_id)),
            byte_len: packet.byte_len(),
            cache_read,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HevcDecodePacketDiagnostic {
    ordinal: u64,
    generation: u64,
    hardware_accelerated: bool,
    packet: HevcPacketDiagnosticFields,
    pts_delta_nsecs: Option<i128>,
    dts_delta_nsecs: Option<i128>,
    decoded_frames: u64,
    zero_output_run_packets: u64,
    decode_ok: bool,
    decode_error: Option<String>,
    decode_elapsed_micros: u64,
    drained: bool,
}

#[derive(Default)]
struct HevcDecodePacketDiagnosticWindow {
    // Retained only for on-demand gap logging; watchdog decisions do not inspect this window.
    next_ordinal: u64,
    packets: VecDeque<HevcDecodePacketDiagnostic>,
}

impl HevcDecodePacketDiagnosticWindow {
    fn record(
        &mut self,
        status: &VideoDecodePacketStatus,
        packet: &AvPacket,
        video_stream: StreamInfo,
        zero_output_run_packets: u64,
        hardware_accelerated: bool,
    ) {
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        let packet = HevcPacketDiagnosticFields::from_packet(
            packet,
            video_stream.codec_id,
            video_stream.time_base,
        );
        let previous = self.packets.back().map(|previous| previous.packet);
        let pts_delta_nsecs = packet
            .pts_nsecs
            .zip(previous.and_then(|previous| previous.pts_nsecs))
            .map(|(current, previous)| i128::from(current) - i128::from(previous));
        let dts_delta_nsecs = packet
            .dts_nsecs
            .zip(previous.and_then(|previous| previous.dts_nsecs))
            .map(|(current, previous)| i128::from(current) - i128::from(previous));
        let decode_elapsed_micros = u64::try_from(status.elapsed.as_micros()).unwrap_or(u64::MAX);
        if self.packets.len() >= HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY {
            self.packets.pop_front();
        }
        self.packets.push_back(HevcDecodePacketDiagnostic {
            ordinal: self.next_ordinal,
            generation: status.generation,
            hardware_accelerated,
            packet,
            pts_delta_nsecs,
            dts_delta_nsecs,
            decoded_frames: status.decoded_frames,
            zero_output_run_packets,
            decode_ok: status.result.is_ok(),
            decode_error: status.result.as_ref().err().cloned(),
            decode_elapsed_micros,
            drained: status.drained,
        });
    }

    fn clear(&mut self) {
        self.next_ordinal = 0;
        self.packets.clear();
    }

    fn has_cache_read_anomaly(&self) -> bool {
        self.packets.iter().any(|packet| {
            let Some(cache) = packet.packet.cache_read else {
                return false;
            };
            cache.sequence_contiguous == Some(false)
                || (cache.previous_read_generation == Some(cache.cache_generation)
                    && cache.previous_read_packet_id == Some(cache.packet_id))
                || (packet.dts_delta_nsecs.is_some_and(|delta| delta < 0)
                    && cache.sequence_contiguous == Some(true))
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcStartupStallObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) hardware_accelerated: bool,
    pub(super) video_decode_snapshot: VideoDecodeWorkerSnapshot,
    pub(super) now: Instant,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) has_audio_output: bool,
    pub(super) fallback_target_nsecs: u64,
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
    pub(super) output_resource_pressure: bool,
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct VideoDecodeRecovery {
    waiting_for_keyframe: bool,
    realign_on_next_frame: bool,
    realign_after_recovery_point: bool,
    skipped_packets: u64,
    first_skipped_packet_nsecs: Option<u64>,
    last_skipped_packet_nsecs: Option<u64>,
    seek_bootstrap_target_nsecs: Option<u64>,
    seek_bootstrap_preroll_frames: u64,
    seek_bootstrap_first_preroll_frame_nsecs: Option<u64>,
    seek_bootstrap_last_preroll_frame_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct SeekPrerollFrameProgress {
    pub(in crate::player::backend::ffmpeg) timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) preroll_frames: u64,
    pub(in crate::player::backend::ffmpeg) first_preroll_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) last_preroll_frame_nsecs: Option<u64>,
}

impl VideoDecodeRecovery {
    pub(in crate::player::backend::ffmpeg) fn reset(&mut self) {
        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = false;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        self.first_skipped_packet_nsecs = None;
        self.last_skipped_packet_nsecs = None;
        self.clear_seek_bootstrap();
    }

    pub(in crate::player::backend::ffmpeg) fn reset_for_timeline_start(
        &mut self,
        codec_id: ffi::AVCodecID,
        current_start_position_nsecs: u64,
    ) {
        self.reset();
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC && current_start_position_nsecs > 0 {
            self.begin_with_realign(false);
            self.begin_seek_bootstrap(current_start_position_nsecs);
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
        if self.can_accept_hevc_recovery_point_after_wait_limit(packet, codec_id) {
            return false;
        }
        codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            || self.skipped_packets < VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
    }

    pub(in crate::player::backend::ffmpeg) fn record_skipped_packet(
        &mut self,
        packet_nsecs: Option<u64>,
    ) -> u64 {
        self.skipped_packets = self.skipped_packets.saturating_add(1);
        if let Some(packet_nsecs) = packet_nsecs {
            self.first_skipped_packet_nsecs.get_or_insert(packet_nsecs);
            self.last_skipped_packet_nsecs = Some(packet_nsecs);
        }
        self.skipped_packets
    }

    pub(in crate::player::backend::ffmpeg) fn skipped_packet_span_nsecs(&self) -> Option<u64> {
        self.first_skipped_packet_nsecs
            .zip(self.last_skipped_packet_nsecs)
            .map(|(first, last)| last.saturating_sub(first))
    }

    pub(in crate::player::backend::ffmpeg) fn seek_bootstrap_preroll_frames(&self) -> u64 {
        self.seek_bootstrap_preroll_frames
    }

    pub(in crate::player::backend::ffmpeg) fn observe_seek_preroll_frame(
        &mut self,
        frame_timeline_nsecs: u64,
    ) -> Option<SeekPrerollFrameProgress> {
        let target_nsecs = self.seek_bootstrap_target_nsecs?;
        self.seek_bootstrap_preroll_frames = self.seek_bootstrap_preroll_frames.saturating_add(1);
        self.seek_bootstrap_first_preroll_frame_nsecs
            .get_or_insert(frame_timeline_nsecs);
        self.seek_bootstrap_last_preroll_frame_nsecs = Some(frame_timeline_nsecs);
        Some(SeekPrerollFrameProgress {
            timeline_nsecs: frame_timeline_nsecs,
            target_nsecs,
            preroll_frames: self.seek_bootstrap_preroll_frames,
            first_preroll_frame_nsecs: self.seek_bootstrap_first_preroll_frame_nsecs,
            last_preroll_frame_nsecs: self.seek_bootstrap_last_preroll_frame_nsecs,
        })
    }

    pub(in crate::player::backend::ffmpeg) fn finish_seek_bootstrap_after_target_frame(
        &mut self,
        frame_timeline_nsecs: u64,
    ) -> Option<SeekPrerollFrameProgress> {
        let target_nsecs = self.seek_bootstrap_target_nsecs?;
        let progress = SeekPrerollFrameProgress {
            timeline_nsecs: frame_timeline_nsecs,
            target_nsecs,
            preroll_frames: self.seek_bootstrap_preroll_frames,
            first_preroll_frame_nsecs: self.seek_bootstrap_first_preroll_frame_nsecs,
            last_preroll_frame_nsecs: self.seek_bootstrap_last_preroll_frame_nsecs,
        };
        self.clear_seek_bootstrap();
        Some(progress)
    }

    pub(in crate::player::backend::ffmpeg) fn accept_recovery_point(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe || !packet_is_video_decode_recovery_point(packet, codec_id) {
            return false;
        }

        self.accept_waited_recovery_point();
        true
    }

    pub(in crate::player::backend::ffmpeg) fn accept_hevc_recovery_point_after_wait_limit(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.can_accept_hevc_recovery_point_after_wait_limit(packet, codec_id) {
            return false;
        }

        self.accept_waited_recovery_point();
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

        self.accept_waited_recovery_point();
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
        self.first_skipped_packet_nsecs = None;
        self.last_skipped_packet_nsecs = None;
    }

    fn begin_seek_bootstrap(&mut self, target_nsecs: u64) {
        self.seek_bootstrap_target_nsecs = Some(target_nsecs);
        self.seek_bootstrap_preroll_frames = 0;
        self.seek_bootstrap_first_preroll_frame_nsecs = None;
        self.seek_bootstrap_last_preroll_frame_nsecs = None;
    }

    fn clear_seek_bootstrap(&mut self) {
        self.seek_bootstrap_target_nsecs = None;
        self.seek_bootstrap_preroll_frames = 0;
        self.seek_bootstrap_first_preroll_frame_nsecs = None;
        self.seek_bootstrap_last_preroll_frame_nsecs = None;
    }

    fn can_accept_hevc_recovery_point_after_wait_limit(
        &self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && self.waiting_for_keyframe
            && self.hevc_recovery_wait_limit_exceeded()
            && packet_is_video_recovery_point(packet, codec_id)
    }

    fn hevc_recovery_wait_limit_exceeded(&self) -> bool {
        self.skipped_packets >= VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
            || self
                .skipped_packet_span_nsecs()
                .is_some_and(|span| span >= HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS)
    }

    fn accept_waited_recovery_point(&mut self) {
        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = self.realign_after_recovery_point;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        self.first_skipped_packet_nsecs = None;
        self.last_skipped_packet_nsecs = None;
    }
}

fn packet_is_video_decode_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return packet_is_video_seek_point(packet, codec_id);
    }
    packet_is_video_recovery_point(packet, codec_id)
}

#[derive(Clone, Copy, Debug, Default)]
struct HevcDecodeChainWatchdog {
    zero_output_packets: u64,
    first_zero_output_packet_nsecs: Option<u64>,
    last_video_packet_nsecs: Option<u64>,
    last_decoded_video_end_nsecs: Option<u64>,
    soft_recovery_attempted: bool,
    recent_zero_output_packets: u64,
    recent_soft_recovery_attempted: bool,
    recent_packet_lead_exceeded: bool,
    healthy_admitted_progress_nsecs: u64,
    pending_fallback: Option<HevcDecodeChainFallback>,
    post_fallback_rebuffer_underfill_started_at: Option<Instant>,
    first_zero_output_at: Option<Instant>,
    startup_in_flight_stall_started_at: Option<Instant>,
    startup_watchdog_retry_not_before: Option<Instant>,
    startup_watchdog_last_rejection_at: Option<Instant>,
    startup_watchdog_last_rejection_reason: Option<&'static str>,
    startup_watchdog_suppressed_rejections: u64,
    last_video_progress_at: Option<Instant>,
}

#[derive(Default)]
struct HevcStartupProbePackets {
    packets: VecDeque<AvPacket>,
}

impl HevcStartupProbePackets {
    fn remember(&mut self, packet: &AvPacket) -> std::result::Result<bool, String> {
        if self.packets.len() >= HEVC_STARTUP_PROBE_PACKET_LIMIT {
            return Ok(false);
        }
        self.packets.push_back(AvPacket::ref_from(packet)?);
        Ok(true)
    }

    fn take(&mut self) -> VecDeque<AvPacket> {
        std::mem::take(&mut self.packets)
    }

    fn clear(&mut self) {
        self.packets.clear();
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

fn hevc_startup_probe_replay_packets(
    packets: VecDeque<AvPacket>,
    playback_generation: &mut PlaybackGeneration,
) -> VecDeque<PendingVideoDecodePacket> {
    packets
        .into_iter()
        .map(|packet| PendingVideoDecodePacket {
            generation: playback_generation.advance(),
            packet,
            realign_after_decode_recovery: true,
            hevc_startup_in_flight_watchdog: false,
        })
        .collect()
}

fn video_decode_pending_input_snapshot(
    regular_pending: usize,
    recovery_replay_pending: usize,
) -> (usize, usize) {
    let pending = regular_pending.saturating_add(recovery_replay_pending);
    let capacity = if recovery_replay_pending == 0 {
        VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY
    } else {
        HEVC_STARTUP_PROBE_PACKET_LIMIT.max(VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY)
    };
    (pending, capacity)
}

#[derive(Clone, Copy, Debug)]
struct HevcDecodeChainWatchdogInput {
    session_id: PlaybackSessionId,
    packet_nsecs: Option<u64>,
    decoded_frames: u64,
    decode_ok: bool,
    hardware_accelerated: bool,
    output_snapshot: PlaybackOutputSnapshot,
    demux_watermark: DemuxReaderWatermark,
    has_audio_output: bool,
    fallback_target_nsecs: u64,
    now: Instant,
}

impl HevcDecodeChainWatchdog {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn reset_transient_after_progress(&mut self, admitted_progress_nsecs: u64, now: Instant) {
        let recent_zero_output_packets = self.recent_zero_output_packets;
        let recent_soft_recovery_attempted = self.recent_soft_recovery_attempted;
        let recent_packet_lead_exceeded = self.recent_packet_lead_exceeded;
        let has_recent_gap_evidence = recent_zero_output_packets > 0
            || recent_soft_recovery_attempted
            || recent_packet_lead_exceeded;
        let healthy_admitted_progress_nsecs = has_recent_gap_evidence.then(|| {
            self.healthy_admitted_progress_nsecs
                .saturating_add(admitted_progress_nsecs)
        });
        self.reset();
        self.recent_zero_output_packets = recent_zero_output_packets;
        self.recent_soft_recovery_attempted = recent_soft_recovery_attempted;
        self.recent_packet_lead_exceeded = recent_packet_lead_exceeded;
        self.healthy_admitted_progress_nsecs = healthy_admitted_progress_nsecs.unwrap_or_default();
        self.last_video_progress_at = Some(now);
        if self.healthy_admitted_progress_nsecs >= HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS {
            self.clear_recent_gap_evidence();
        }
    }

    fn take_fallback(&mut self) -> Option<HevcDecodeChainFallback> {
        self.pending_fallback.take()
    }

    fn has_pending_fallback(&self) -> bool {
        self.pending_fallback.is_some()
    }

    fn stats(&self) -> HevcDecodeChainStats {
        HevcDecodeChainStats {
            zero_output_packets: self.zero_output_packets,
            recent_zero_output_packets: self.recent_zero_output_packets,
            first_zero_output_packet_nsecs: self.first_zero_output_packet_nsecs,
            last_video_packet_nsecs: self.last_video_packet_nsecs,
            last_decoded_video_end_nsecs: self.last_decoded_video_end_nsecs,
            soft_recovery_attempted: self.soft_recovery_attempted,
            recent_soft_recovery_attempted: self.recent_soft_recovery_attempted,
            pending_fallback_reason: self.pending_fallback.map(|fallback| fallback.reason),
        }
    }

    fn has_strong_decoded_frame_gap_evidence(
        &self,
        input: &HevcDecodedFrameGapObservation,
    ) -> bool {
        input.recovery_waiting
            || input.source_frame_diagnostic.corrupt
            || input.source_frame_diagnostic.decode_error_flags != 0
            || self.zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
            || (self.soft_recovery_attempted
                && (self.zero_output_packets > 0 || self.recent_packet_lead_exceeded))
            || (self.recent_soft_recovery_attempted && self.recent_packet_lead_exceeded)
    }

    fn decoded_frame_gap_demux_is_healthy(
        input: &HevcDecodedFrameGapObservation,
        gap_nsecs: u64,
    ) -> bool {
        let demux_underrun = input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || input.demux_watermark.audio_underrun;
        let minimum_forward_nsecs =
            gap_nsecs.max(duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION));
        !demux_underrun
            && !input.recent_cache_read_anomaly
            && input
                .demux_watermark
                .selected_min_forward_nsecs
                .or(input.demux_watermark.video_forward_nsecs)
                .is_some_and(|forward| forward >= minimum_forward_nsecs)
    }

    fn decoded_frame_gap_output_is_stable(input: &HevcDecodedFrameGapObservation) -> bool {
        let queued_video_contiguous_forward_nsecs = input
            .output_snapshot
            .queued_video_contiguous_forward_nsecs
            .or(input.output_snapshot.queued_video_forward_nsecs)
            .unwrap_or(input.output_snapshot.queued_video_duration_nsecs);
        !input.output_snapshot.first_video_frame_pending
            && !input.output_snapshot.rebuffering
            && !input.output_snapshot.video_output_low_water
            && !input.output_snapshot.video_decode_underfill
            && queued_video_contiguous_forward_nsecs
                > duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
    }

    fn decoded_frame_gap_has_demux_underrun(input: &HevcDecodedFrameGapObservation) -> bool {
        input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || input.demux_watermark.audio_underrun
    }

    fn clear_recent_gap_evidence(&mut self) {
        self.recent_zero_output_packets = 0;
        self.recent_soft_recovery_attempted = false;
        self.recent_packet_lead_exceeded = false;
        self.healthy_admitted_progress_nsecs = 0;
    }

    fn recovery_progress_grace_active(&self, now: Instant, hardware_accelerated: bool) -> bool {
        let grace = if hardware_accelerated {
            HEVC_HARDWARE_RECOVERY_PROGRESS_GRACE
        } else {
            HEVC_SOFTWARE_RECOVERY_PROGRESS_GRACE
        };
        self.last_video_progress_at
            .is_some_and(|progress_at| now.saturating_duration_since(progress_at) < grace)
    }

    fn observe_startup_stall(
        &mut self,
        input: HevcStartupStallObservation,
    ) -> HevcDecodeChainRecoveryAction {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.reset();
            return HevcDecodeChainRecoveryAction::None;
        }

        self.observe_startup_in_flight_stall(input);
        if self.pending_fallback.is_some() {
            return HevcDecodeChainRecoveryAction::None;
        }

        if !hevc_startup_first_frame_zero_output_context(
            input.output_snapshot,
            input.demux_watermark,
            input.has_audio_output,
        ) {
            return HevcDecodeChainRecoveryAction::None;
        }

        if self.startup_hard_fallback_ready(
            input.now,
            input.demux_watermark,
            input.fallback_target_nsecs,
            input.hardware_accelerated,
        ) {
            self.pending_fallback = Some(HevcDecodeChainFallback {
                target_nsecs: input.fallback_target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            });
            tracing::debug!(
                session_id = ?input.session_id,
                target_nsecs = input.fallback_target_nsecs,
                hevc_zero_output_packets = self.zero_output_packets,
                recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                startup_zero_output_elapsed_ms = ?self.first_zero_output_at.map(|started_at| {
                    input.now.saturating_duration_since(started_at).as_secs_f64() * 1000.0
                }),
                demux_min_forward_ms = ?input
                    .demux_watermark
                    .selected_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "hevc_decode_chain_startup_first_frame_hard"
            );
        }

        HevcDecodeChainRecoveryAction::None
    }

    fn arm_startup_in_flight_stall(&mut self, session_id: PlaybackSessionId, now: Instant) {
        if self.pending_fallback.is_some() || self.startup_in_flight_stall_started_at.is_some() {
            return;
        }
        self.startup_in_flight_stall_started_at = Some(now);
        tracing::debug!(
            session_id = ?session_id,
            deadline_ms = HEVC_STARTUP_IN_FLIGHT_HARD_AFTER.as_secs_f64() * 1000.0,
            "armed HEVC startup in-flight decode watchdog"
        );
    }

    fn observe_startup_in_flight_stall(&mut self, input: HevcStartupStallObservation) {
        if !hevc_startup_in_flight_stall_context(input) {
            if hevc_startup_in_flight_stall_should_disarm(input) {
                self.startup_in_flight_stall_started_at = None;
            }
            return;
        }

        let started_at = match self.startup_in_flight_stall_started_at {
            Some(started_at) => started_at,
            None => {
                self.startup_in_flight_stall_started_at = Some(input.now);
                input.now
            }
        };
        self.trigger_startup_in_flight_fallback_if_elapsed(input, started_at);
    }

    fn trigger_startup_in_flight_fallback_if_elapsed(
        &mut self,
        input: HevcStartupStallObservation,
        started_at: Instant,
    ) {
        let elapsed = input.now.saturating_duration_since(started_at);
        tracing::trace!(
            session_id = ?input.session_id,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            video_decode_state = ?input.video_decode_snapshot.state,
            video_decode_in_flight_packets = input.video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = input.video_decode_snapshot.completed_packets,
            video_decode_queued_frames = input.video_decode_snapshot.queued_frames,
            demux_min_forward_ms = ?input
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "checked HEVC startup in-flight decode watchdog"
        );
        if elapsed < HEVC_STARTUP_IN_FLIGHT_HARD_AFTER {
            return;
        }

        let reason = HevcDecodeChainFallbackReason::StartupInFlightStall;
        self.pending_fallback = Some(HevcDecodeChainFallback {
            target_nsecs: input.fallback_target_nsecs,
            reason,
        });
        tracing::debug!(
            session_id = ?input.session_id,
            target_nsecs = input.fallback_target_nsecs,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            video_decode_state = ?input.video_decode_snapshot.state,
            video_decode_in_flight_packets = input.video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = input.video_decode_snapshot.completed_packets,
            video_decode_queued_frames = input.video_decode_snapshot.queued_frames,
            output_state = ?input.output_snapshot.state,
            first_video_frame_pending = input.output_snapshot.first_video_frame_pending,
            output_rebuffering = input.output_snapshot.rebuffering,
            demux_min_forward_ms = ?input
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            fallback_reason = reason.as_str(),
            "hevc_decode_chain_startup_in_flight_hard"
        );
    }

    fn observe_admitted_video_progress(
        &mut self,
        input: HevcAdmittedVideoProgressObservation,
    ) -> bool {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return false;
        }
        let queue_end_advanced = input.after_queue_end_nsecs.is_some_and(|after| {
            input
                .before_queue_end_nsecs
                .is_none_or(|before| after > before)
        });
        let after_start = input.frame_timeline_nsecs >= input.current_start_position_nsecs;
        if !queue_end_advanced || !after_start {
            tracing::trace!(
                session_id = ?input.session_id,
                pts = input.frame_timeline_nsecs,
                current_start_position_nsecs = input.current_start_position_nsecs,
                before_queue_end_nsecs = ?input.before_queue_end_nsecs,
                after_queue_end_nsecs = ?input.after_queue_end_nsecs,
                queue_end_advanced,
                after_start,
                "ignored HEVC decoded frame for watchdog reset because it was not admitted progress"
            );
            return false;
        }
        if self.zero_output_packets > 0
            || self.soft_recovery_attempted
            || self.post_fallback_rebuffer_underfill_started_at.is_some()
            || self.startup_in_flight_stall_started_at.is_some()
            || self.pending_fallback.is_some()
        {
            tracing::debug!(
                session_id = ?input.session_id,
                pts = input.frame_timeline_nsecs,
                current_start_position_nsecs = input.current_start_position_nsecs,
                before_queue_end_nsecs = ?input.before_queue_end_nsecs,
                after_queue_end_nsecs = ?input.after_queue_end_nsecs,
                watchdog_reset_reason = "admitted_video_queue_advanced",
                hevc_zero_output_packets = self.zero_output_packets,
                soft_recovery_attempted = self.soft_recovery_attempted,
                post_fallback_rebuffer_underfill_started =
                    self.post_fallback_rebuffer_underfill_started_at.is_some(),
                startup_in_flight_stall_started =
                    self.startup_in_flight_stall_started_at.is_some(),
                pending_fallback = self.pending_fallback.map(|fallback| fallback.reason.as_str()),
                "resetting HEVC decode chain watchdog after admitted video progress"
            );
        }
        let admitted_progress_nsecs = input
            .after_queue_end_nsecs
            .zip(input.before_queue_end_nsecs)
            .map(|(after, before)| after.saturating_sub(before))
            .unwrap_or_default();
        self.reset_transient_after_progress(admitted_progress_nsecs, Instant::now());
        true
    }

    fn observe_seek_preroll_progress(&mut self, input: HevcSeekPrerollProgressObservation) {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return;
        }
        if self.zero_output_packets > 0
            || self.soft_recovery_attempted
            || self.post_fallback_rebuffer_underfill_started_at.is_some()
            || self.startup_in_flight_stall_started_at.is_some()
            || self.pending_fallback.is_some()
        {
            tracing::debug!(
                session_id = ?input.session_id,
                pts = input.frame_timeline_nsecs,
                target_nsecs = input.target_nsecs,
                preroll_frames = input.preroll_frames,
                watchdog_reset_reason = "seek_preroll_decoded_frame",
                hevc_zero_output_packets = self.zero_output_packets,
                soft_recovery_attempted = self.soft_recovery_attempted,
                post_fallback_rebuffer_underfill_started =
                    self.post_fallback_rebuffer_underfill_started_at.is_some(),
                startup_in_flight_stall_started =
                    self.startup_in_flight_stall_started_at.is_some(),
                pending_fallback = self.pending_fallback.map(|fallback| fallback.reason.as_str()),
                "resetting HEVC decode chain watchdog after seek preroll decoded progress"
            );
        }
        self.reset_transient_after_progress(0, Instant::now());
    }

    fn observe_post_fallback_rebuffer_underfill(
        &mut self,
        input: HevcPostFallbackRebufferObservation,
    ) {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.post_fallback_rebuffer_underfill_started_at = None;
            return;
        }
        if self.pending_fallback.is_some() {
            return;
        }
        let decoded_video_forward_nsecs =
            input.output_snapshot.queued_video_bootstrap_forward_nsecs();
        let demux_forward_healthy = !input.demux_watermark.underrun
            && !input.demux_watermark.video_underrun
            && input
                .demux_watermark
                .selected_min_forward_nsecs
                .is_some_and(|forward| {
                    forward >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                });
        let underfilled = input.output_snapshot.rebuffering
            && !input.output_snapshot.video_decode_underfill
            && input.output_snapshot.video_bootstrap_after_seek
            && decoded_video_forward_nsecs < HEVC_POST_FALLBACK_REBUFFER_UNDERFILL_NSECS
            && demux_forward_healthy
            && input.audio_ready;
        if !underfilled {
            self.post_fallback_rebuffer_underfill_started_at = None;
            return;
        }
        let started_at = self
            .post_fallback_rebuffer_underfill_started_at
            .get_or_insert(input.now);
        let elapsed = input.now.saturating_duration_since(*started_at);
        tracing::trace!(
            session_id = ?input.session_id,
            decoded_video_ms = decoded_video_forward_nsecs as f64 / 1_000_000.0,
            audio_ready = input.audio_ready,
            demux_min_forward_ms = ?input
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            recovery_after_ms =
                HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER.as_secs_f64() * 1000.0,
            "checked HEVC post-fallback rebuffer underfill watchdog"
        );
        if elapsed < HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER {
            return;
        }
        let target_nsecs = input.fallback_target_nsecs;
        let reason = HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill;
        self.pending_fallback = Some(HevcDecodeChainFallback {
            target_nsecs,
            reason,
        });
        tracing::debug!(
            session_id = ?input.session_id,
            decoded_video_ms = decoded_video_forward_nsecs as f64 / 1_000_000.0,
            audio_ready = input.audio_ready,
            demux_min_forward_ms = ?input
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            fallback_target_nsecs = input.fallback_target_nsecs,
            playback_target_nsecs = target_nsecs,
            fallback_reason = reason.as_str(),
            "HEVC post-fallback rebuffer underfill requesting low-level fallback"
        );
    }

    fn observe_decoded_frame_gap(
        &mut self,
        input: HevcDecodedFrameGapObservation,
    ) -> HevcDecodedFrameGapAction {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.clear_recent_gap_evidence();
            return HevcDecodedFrameGapAction::Admit;
        }

        let positive_gap_nsecs = input
            .previous_gap_nsecs
            .and_then(|gap| u64::try_from(gap).ok());
        let Some(gap_nsecs) = positive_gap_nsecs else {
            return HevcDecodedFrameGapAction::Admit;
        };
        if video_timestamp_gap_within_threshold(gap_nsecs, input.max_gap_nsecs) {
            return HevcDecodedFrameGapAction::Admit;
        }

        if let Some(audio_gap) = input.audio_timeline_gap
            && let Some(previous_expected_next_nsecs) = input.previous_expected_next_nsecs
            && audio_gap
                .next_start_nsecs
                .checked_sub(audio_gap.previous_end_nsecs)
                .is_some_and(|audio_gap_nsecs| {
                    !video_timestamp_gap_within_threshold(audio_gap_nsecs, input.max_gap_nsecs)
                })
            && audio_gap
                .previous_end_nsecs
                .abs_diff(previous_expected_next_nsecs)
                <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
            && audio_gap.next_start_nsecs.abs_diff(input.timeline_nsecs)
                <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
        {
            self.reset();
            tracing::debug!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                video_previous_end_nsecs = previous_expected_next_nsecs,
                video_next_start_nsecs = input.timeline_nsecs,
                video_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                audio_previous_end_nsecs = audio_gap.previous_end_nsecs,
                audio_next_start_nsecs = audio_gap.next_start_nsecs,
                audio_gap_ms = audio_gap
                    .next_start_nsecs
                    .saturating_sub(audio_gap.previous_end_nsecs) as f64
                    / 1_000_000.0,
                av_previous_end_delta_ms = audio_gap
                    .previous_end_nsecs
                    .abs_diff(previous_expected_next_nsecs) as f64
                    / 1_000_000.0,
                av_next_start_delta_ms = audio_gap
                    .next_start_nsecs
                    .abs_diff(input.timeline_nsecs) as f64
                    / 1_000_000.0,
                "accepted synchronized HEVC media timeline gap without decode-chain fallback"
            );
            return HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap;
        }

        let clean_recovery_frame = input.source_frame_diagnostic.key_frame
            && !input.source_frame_diagnostic.corrupt
            && input.source_frame_diagnostic.decode_error_flags == 0;
        let recoverable_decode_gap =
            video_timestamp_gap_within_threshold(gap_nsecs, HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS)
                && clean_recovery_frame
                && Self::decoded_frame_gap_demux_is_healthy(&input, gap_nsecs);
        if recoverable_decode_gap {
            let cancelled_fallback_reason = self
                .pending_fallback
                .map(|fallback| fallback.reason.as_str());
            self.reset();
            tracing::debug!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                pts = input.timeline_nsecs,
                duration_nsecs = input.duration_nsecs,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                recoverable_gap_limit_ms =
                    HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS as f64 / 1_000_000.0,
                timestamp_rounding_tolerance_nsecs =
                    VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS,
                frame_key = input.source_frame_diagnostic.key_frame,
                frame_corrupt = input.source_frame_diagnostic.corrupt,
                frame_decode_error_flags = input.source_frame_diagnostic.decode_error_flags,
                demux_selected_min_forward_ms = ?input
                    .demux_watermark
                    .selected_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                recent_cache_read_anomaly = input.recent_cache_read_anomaly,
                cancelled_fallback_reason,
                "admitting clean HEVC recovery keyframe and bridging small decode gap"
            );
            return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
        }

        let has_evidence = self.has_strong_decoded_frame_gap_evidence(&input);
        if !has_evidence {
            let cleared_pending_fallback = self
                .pending_fallback
                .take()
                .map(|fallback| fallback.reason.as_str());
            tracing::debug!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                pts = input.timeline_nsecs,
                duration_nsecs = input.duration_nsecs,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                max_gap_ms = input.max_gap_nsecs as f64 / 1_000_000.0,
                recovery_waiting = input.recovery_waiting,
                frame_key = input.source_frame_diagnostic.key_frame,
                frame_corrupt = input.source_frame_diagnostic.corrupt,
                frame_decode_error_flags = input.source_frame_diagnostic.decode_error_flags,
                cleared_pending_fallback,
                queued_video_contiguous_forward_ms = ?input
                    .output_snapshot
                    .queued_video_contiguous_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                queued_video_largest_gap_ms = ?input
                    .output_snapshot
                    .queued_video_largest_gap_nsecs
                    .map(|gap| gap as f64 / 1_000_000.0),
                "observed HEVC decoded frame PTS gap without decode-chain evidence"
            );
            return HevcDecodedFrameGapAction::Admit;
        }

        let output_stable = Self::decoded_frame_gap_output_is_stable(&input);
        let demux_underrun = Self::decoded_frame_gap_has_demux_underrun(&input);
        if output_stable || demux_underrun {
            let deferred_pending_fallback = self
                .pending_fallback
                .take()
                .map(|fallback| fallback.reason.as_str());
            tracing::debug!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                pts = input.timeline_nsecs,
                duration_nsecs = input.duration_nsecs,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                output_stable,
                demux_underrun,
                deferred_pending_fallback,
                recovery_waiting = input.recovery_waiting,
                frame_key = input.source_frame_diagnostic.key_frame,
                frame_corrupt = input.source_frame_diagnostic.corrupt,
                frame_decode_error_flags = input.source_frame_diagnostic.decode_error_flags,
                queued_video_contiguous_forward_ms = ?input
                    .output_snapshot
                    .queued_video_contiguous_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                output_video_low_water = input.output_snapshot.video_output_low_water,
                video_decode_underfill = input.output_snapshot.video_decode_underfill,
                output_rebuffering = input.output_snapshot.rebuffering,
                "deferred HEVC decode-gap fallback until output reaches low water"
            );
            return HevcDecodedFrameGapAction::DeferFallback;
        }

        if self.pending_fallback.is_some() {
            return HevcDecodedFrameGapAction::DropForFallback;
        }

        let target_nsecs = input
            .previous_expected_next_nsecs
            .or(input.audio_played_timeline_nsecs)
            .unwrap_or(input.fallback_target_nsecs);
        let reason = HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput;
        self.pending_fallback = Some(HevcDecodeChainFallback {
            target_nsecs,
            reason,
        });
        tracing::debug!(
            session_id = ?input.session_id,
            codec = ?input.codec_id,
            pts = input.timeline_nsecs,
            duration_nsecs = input.duration_nsecs,
            previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
            previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
            max_gap_ms = input.max_gap_nsecs as f64 / 1_000_000.0,
            audio_played_timeline_nsecs = ?input.audio_played_timeline_nsecs,
            target_nsecs,
            fallback_reason = reason.as_str(),
            hevc_zero_output_packets = self.zero_output_packets,
            recent_hevc_zero_output_packets = self.recent_zero_output_packets,
            soft_recovery_attempted = self.soft_recovery_attempted,
            recent_soft_recovery_attempted = self.recent_soft_recovery_attempted,
            recent_packet_lead_exceeded = self.recent_packet_lead_exceeded,
            recovery_waiting = input.recovery_waiting,
            queued_video_contiguous_forward_ms = ?input
                .output_snapshot
                .queued_video_contiguous_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            queued_video_largest_gap_ms = ?input
                .output_snapshot
                .queued_video_largest_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            "hevc_decode_chain_pts_gap"
        );
        HevcDecodedFrameGapAction::DropForFallback
    }

    fn observe_packet(
        &mut self,
        input: HevcDecodeChainWatchdogInput,
    ) -> HevcDecodeChainRecoveryAction {
        if !input.decode_ok {
            self.startup_in_flight_stall_started_at = None;
            return HevcDecodeChainRecoveryAction::None;
        }
        if input.decoded_frames > 0 {
            self.startup_in_flight_stall_started_at = None;
            self.startup_watchdog_retry_not_before = None;
            self.last_video_progress_at = Some(input.now);
            tracing::trace!(
                session_id = ?input.session_id,
                decoded_frames = input.decoded_frames,
                hevc_zero_output_packets = self.zero_output_packets,
                soft_recovery_attempted = self.soft_recovery_attempted,
                "observed HEVC decoder output; waiting for admitted video progress before watchdog reset"
            );
            return HevcDecodeChainRecoveryAction::None;
        }

        if let Some((_, end_nsecs)) = input.output_snapshot.queued_video_range_nsecs {
            self.last_decoded_video_end_nsecs = Some(end_nsecs);
        }
        if self.zero_output_packets == 0 {
            self.first_zero_output_packet_nsecs = input.packet_nsecs;
            self.first_zero_output_at = Some(input.now);
            self.startup_watchdog_retry_not_before = None;
        }
        self.healthy_admitted_progress_nsecs = 0;
        self.zero_output_packets = self.zero_output_packets.saturating_add(1);
        self.last_video_packet_nsecs = input.packet_nsecs.or(self.last_video_packet_nsecs);

        let packet_lead_nsecs = self
            .last_video_packet_nsecs
            .zip(self.last_decoded_video_end_nsecs)
            .map(|(packet_nsecs, decoded_end_nsecs)| {
                packet_nsecs.saturating_sub(decoded_end_nsecs)
            });
        let packet_lead_exceeded = packet_lead_nsecs
            .is_some_and(|lead| lead >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS);
        let rebuffer_hard_packet_lead_exceeded = packet_lead_nsecs
            .is_some_and(|lead| lead >= HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS);
        let last_packet_to_fallback_target_ms = self.last_video_packet_nsecs.map(|packet_nsecs| {
            (i128::from(packet_nsecs) - i128::from(input.fallback_target_nsecs)) as f64
                / 1_000_000.0
        });
        self.recent_zero_output_packets = self
            .recent_zero_output_packets
            .max(self.zero_output_packets);
        self.recent_packet_lead_exceeded |= packet_lead_exceeded;
        let demux_underrun = input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || (input.has_audio_output && input.demux_watermark.audio_underrun);
        let queued_video_contiguous_forward_nsecs = input
            .output_snapshot
            .queued_video_contiguous_forward_nsecs
            .or(input.output_snapshot.queued_video_forward_nsecs)
            .unwrap_or(input.output_snapshot.queued_video_duration_nsecs);
        let rebuffer_video_starving = input.output_snapshot.rebuffering
            && queued_video_contiguous_forward_nsecs
                <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION);
        let output_unstable = input.output_snapshot.video_output_low_water
            || input.output_snapshot.video_decode_underfill
            || rebuffer_video_starving;
        let startup_zero_output_context = hevc_startup_first_frame_zero_output_context(
            input.output_snapshot,
            input.demux_watermark,
            input.has_audio_output,
        );

        tracing::debug!(
            session_id = ?input.session_id,
            hevc_zero_output_packets = self.zero_output_packets,
            fallback_target_nsecs = input.fallback_target_nsecs,
            first_zero_output_packet_nsecs = ?self.first_zero_output_packet_nsecs,
            last_video_packet_pts = ?self.last_video_packet_nsecs,
            last_packet_to_fallback_target_ms = ?last_packet_to_fallback_target_ms,
            last_decoded_video_end = ?self.last_decoded_video_end_nsecs,
            packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
            output_state = ?input.output_snapshot.state,
            output_video_low_water = input.output_snapshot.video_output_low_water,
            video_decode_underfill = input.output_snapshot.video_decode_underfill,
            queued_video_forward_ms = ?input
                .output_snapshot
                .queued_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            queued_video_contiguous_forward_ms = ?input
                .output_snapshot
                .queued_video_contiguous_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            queued_video_largest_gap_ms = ?input
                .output_snapshot
                .queued_video_largest_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            demux_underrun,
            demux_video_underrun = input.demux_watermark.video_underrun,
            demux_audio_underrun = input.demux_watermark.audio_underrun,
            demux_video_forward_ms = ?input
                .demux_watermark
                .video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_selected_min_forward_ms = ?input
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "observed HEVC decode packet with zero output frames"
        );

        if demux_underrun || (!output_unstable && !startup_zero_output_context) {
            return HevcDecodeChainRecoveryAction::None;
        }

        if self.recovery_progress_grace_active(input.now, input.hardware_accelerated) {
            return HevcDecodeChainRecoveryAction::None;
        }

        if startup_zero_output_context {
            if self.startup_hard_fallback_ready(
                input.now,
                input.demux_watermark,
                input.fallback_target_nsecs,
                input.hardware_accelerated,
            ) {
                self.pending_fallback = Some(HevcDecodeChainFallback {
                    target_nsecs: input.fallback_target_nsecs,
                    reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
                });
                tracing::debug!(
                    session_id = ?input.session_id,
                    target_nsecs = input.fallback_target_nsecs,
                    hevc_zero_output_packets = self.zero_output_packets,
                    recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                    last_video_packet_pts = ?self.last_video_packet_nsecs,
                    last_packet_to_fallback_target_ms = ?last_packet_to_fallback_target_ms,
                    startup_zero_output_elapsed_ms =
                        ?self.first_zero_output_at.map(|started_at| {
                            input.now.saturating_duration_since(started_at).as_secs_f64() * 1000.0
                        }),
                    "hevc_decode_chain_startup_first_frame_hard"
                );
                return HevcDecodeChainRecoveryAction::None;
            }
            return HevcDecodeChainRecoveryAction::None;
        }

        if self.soft_recovery_attempted
            && input.output_snapshot.rebuffering
            && !input.output_snapshot.video_decode_underfill
            && (self.zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
                || rebuffer_hard_packet_lead_exceeded)
        {
            self.pending_fallback = Some(HevcDecodeChainFallback {
                target_nsecs: input.fallback_target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            });
            tracing::debug!(
                session_id = ?input.session_id,
                target_nsecs = input.fallback_target_nsecs,
                hevc_zero_output_packets = self.zero_output_packets,
                last_video_packet_pts = ?self.last_video_packet_nsecs,
                last_packet_to_fallback_target_ms = ?last_packet_to_fallback_target_ms,
                last_decoded_video_end = ?self.last_decoded_video_end_nsecs,
                packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
                rebuffer_hard_packet_lead_exceeded,
                queued_video_contiguous_forward_ms = ?input
                    .output_snapshot
                    .queued_video_contiguous_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                queued_video_largest_gap_ms = ?input
                    .output_snapshot
                    .queued_video_largest_gap_nsecs
                    .map(|gap| gap as f64 / 1_000_000.0),
                "hevc_decode_chain_recovery_hard"
            );
            return HevcDecodeChainRecoveryAction::None;
        }

        if !self.soft_recovery_attempted
            && (self.zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
                || packet_lead_exceeded)
        {
            self.soft_recovery_attempted = true;
            self.recent_soft_recovery_attempted = true;
            self.zero_output_packets = 0;
            self.first_zero_output_packet_nsecs = None;
            tracing::debug!(
                session_id = ?input.session_id,
                last_video_packet_pts = ?self.last_video_packet_nsecs,
                last_decoded_video_end = ?self.last_decoded_video_end_nsecs,
                packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
                queued_video_contiguous_forward_ms = ?input
                    .output_snapshot
                    .queued_video_contiguous_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                queued_video_largest_gap_ms = ?input
                    .output_snapshot
                    .queued_video_largest_gap_nsecs
                    .map(|gap| gap as f64 / 1_000_000.0),
                "hevc_decode_chain_recovery_soft"
            );
            return HevcDecodeChainRecoveryAction::SoftRecovery;
        }

        HevcDecodeChainRecoveryAction::None
    }

    fn startup_hard_fallback_ready(
        &self,
        now: Instant,
        demux_watermark: DemuxReaderWatermark,
        fallback_target_nsecs: u64,
        hardware_accelerated: bool,
    ) -> bool {
        let demux_ready = demux_watermark
            .selected_min_forward_nsecs
            .is_some_and(|forward| forward >= HEVC_STARTUP_ZERO_OUTPUT_HARD_MIN_FORWARD_NSECS);
        if !demux_ready {
            return false;
        }
        if fallback_target_nsecs > 0
            && self
                .last_video_packet_nsecs
                .is_none_or(|packet_nsecs| packet_nsecs < fallback_target_nsecs)
        {
            return false;
        }
        let packet_budget_exhausted = hardware_accelerated
            && self.zero_output_packets >= HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT;
        let timeout = hevc_startup_zero_output_timeout(
            hardware_accelerated,
            fallback_target_nsecs,
            self.first_zero_output_packet_nsecs,
        );
        packet_budget_exhausted
            || self
                .first_zero_output_at
                .is_some_and(|started_at| now.saturating_duration_since(started_at) >= timeout)
    }

    fn startup_in_flight_deadline(&self) -> Option<Instant> {
        self.startup_in_flight_stall_started_at
            .map(|started_at| started_at + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
    }

    fn startup_watchdog_deadline(&self, hardware_accelerated: bool) -> Option<Instant> {
        if !hardware_accelerated {
            return None;
        }
        let deadline = min_instant(
            self.first_zero_output_at
                .map(|started_at| started_at + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
            self.startup_in_flight_deadline(),
        );
        match (deadline, self.startup_watchdog_retry_not_before) {
            (Some(deadline), Some(not_before)) => Some(deadline.max(not_before)),
            (Some(deadline), None) => Some(deadline),
            (None, _) => None,
        }
    }

    fn defer_startup_watchdog_after_no_action(&mut self, now: Instant) {
        let raw_deadline = min_instant(
            self.first_zero_output_at
                .map(|started_at| started_at + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
            self.startup_in_flight_deadline(),
        );
        if raw_deadline.is_some_and(|deadline| deadline <= now) {
            self.startup_watchdog_retry_not_before = Some(now + HEVC_STARTUP_WATCHDOG_RETRY_AFTER);
        }
    }

    fn record_startup_watchdog_rejection(
        &mut self,
        reason: &'static str,
        now: Instant,
    ) -> Option<u64> {
        let reason_changed = self.startup_watchdog_last_rejection_reason != Some(reason);
        let interval_elapsed = self
            .startup_watchdog_last_rejection_at
            .is_none_or(|logged_at| {
                now.saturating_duration_since(logged_at)
                    >= HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL
            });
        if reason_changed || interval_elapsed {
            let suppressed = std::mem::take(&mut self.startup_watchdog_suppressed_rejections);
            self.startup_watchdog_last_rejection_at = Some(now);
            self.startup_watchdog_last_rejection_reason = Some(reason);
            return Some(suppressed);
        }
        self.startup_watchdog_suppressed_rejections = self
            .startup_watchdog_suppressed_rejections
            .saturating_add(1);
        None
    }
}

pub(super) fn hevc_startup_zero_output_timeout(
    hardware_accelerated: bool,
    fallback_target_nsecs: u64,
    first_zero_output_packet_nsecs: Option<u64>,
) -> Duration {
    if hardware_accelerated {
        return HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER;
    }
    let preroll = first_zero_output_packet_nsecs
        .map(|packet_nsecs| fallback_target_nsecs.saturating_sub(packet_nsecs))
        .map(Duration::from_nanos)
        .unwrap_or_default();
    HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_BASE_AFTER
        .saturating_add(preroll.saturating_mul(2))
        .min(HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_MAX_AFTER)
}

fn min_instant(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn hevc_startup_first_frame_zero_output_context(
    output_snapshot: PlaybackOutputSnapshot,
    demux_watermark: DemuxReaderWatermark,
    has_audio_output: bool,
) -> bool {
    if !(output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
        || output_snapshot.queued_video_frames > 0
    {
        return false;
    }
    if demux_watermark.underrun
        || demux_watermark.video_underrun
        || (has_audio_output && demux_watermark.audio_underrun)
    {
        return false;
    }
    demux_watermark
        .selected_min_forward_nsecs
        .is_some_and(|forward| forward >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION))
}

fn hevc_startup_in_flight_stall_context(input: HevcStartupStallObservation) -> bool {
    if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return false;
    }
    if !input.hardware_accelerated {
        return false;
    }
    if !matches!(
        input.video_decode_snapshot.state,
        VideoDecodeWorkerState::Decoding
    ) {
        return false;
    }
    input.video_decode_snapshot.in_flight_packets > 0
        && input.video_decode_snapshot.completed_packets == 0
        && input.video_decode_snapshot.queued_frames == 0
        && input.output_snapshot.queued_video_frames == 0
}

fn hevc_startup_in_flight_stall_should_disarm(input: HevcStartupStallObservation) -> bool {
    if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC || !input.hardware_accelerated {
        return true;
    }
    input.video_decode_snapshot.queued_frames > 0
        || input.output_snapshot.queued_video_frames > 0
        || input.video_decode_snapshot.in_flight_packets == 0
}

pub(super) struct VideoDecodePipeline {
    worker: VideoDecodeWorker,
    packets: VideoDecodePacketQueues,
    hevc_startup_probe_replay: VecDeque<PendingVideoDecodePacket>,
    hevc_decode_chain_watchdog: HevcDecodeChainWatchdog,
    hevc_decode_packet_diagnostics: HevcDecodePacketDiagnosticWindow,
    hevc_startup_probe_packets: HevcStartupProbePackets,
    last_hevc_decode_chain_fallback: Option<HevcDecodeChainFallbackRecord>,
}

impl VideoDecodePipeline {
    pub(super) fn spawn(decoder: Decoder) -> std::result::Result<Self, String> {
        Ok(Self {
            worker: VideoDecodeWorker::spawn(decoder)?,
            packets: VideoDecodePacketQueues::default(),
            hevc_startup_probe_replay: VecDeque::new(),
            hevc_decode_chain_watchdog: HevcDecodeChainWatchdog::default(),
            hevc_decode_packet_diagnostics: HevcDecodePacketDiagnosticWindow::default(),
            hevc_startup_probe_packets: HevcStartupProbePackets::default(),
            last_hevc_decode_chain_fallback: None,
        })
    }

    pub(super) fn info(&self) -> &VideoDecodeWorkerInfo {
        self.worker.info()
    }

    pub(super) fn snapshot(&self) -> VideoDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        let (pending_input_packets, pending_input_capacity) = video_decode_pending_input_snapshot(
            self.packets.pending_input_count(),
            self.hevc_startup_probe_replay.len(),
        );
        snapshot.pending_input_packets = pending_input_packets;
        snapshot.pending_input_capacity = pending_input_capacity;
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
            _ if snapshot.pending_input_full() => Some(PlaybackBlockReason::PacketQueueFull),
            _ if snapshot.completed_packets > 0
                && snapshot.in_flight_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::DecoderOutputPending)
            }
            _ if snapshot.in_flight_packets >= snapshot.command_queue_capacity => {
                Some(PlaybackBlockReason::DecoderInFlight)
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
                self.push_in_flight(pending_packet, session_id);
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
                self.push_in_flight(pending_packet, session_id);
                Ok(DecodeInputRetryStatus::Queued)
            }
            VideoDecodeEnqueueResult::InputFull | VideoDecodeEnqueueResult::OutputFull => {
                self.packets.push_pending_input_front(pending_packet);
                self.log_pending_input_backpressured(session_id, enqueue_result);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    pub(super) fn requeue_hevc_startup_probe_packets(
        &mut self,
        playback_generation: &mut PlaybackGeneration,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        let packets = self.hevc_startup_probe_packets.take();
        let replay = hevc_startup_probe_replay_packets(packets, playback_generation);
        let requeued = replay.len();
        self.hevc_startup_probe_replay.extend(replay);
        if requeued > 0 {
            tracing::debug!(
                session_id = ?session_id,
                requeued,
                replay_pending = self.hevc_startup_probe_replay.len(),
                "requeued HEVC startup probe packets after hardware decode fallback"
            );
        }
        Ok(requeued)
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
        let packet_nsecs = packet
            .best_timestamp()
            .and_then(|timestamp| timestamp_to_nsecs(timestamp, context.video_stream.time_base));
        let hardware_accelerated = self.info().hardware_accelerated;
        let recovery_skipping_packet = recovery.should_skip_packet(packet, codec_id);
        tracing::trace!(
            session_id = ?context.session_id,
            packet_count = *video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            codec = ?codec_id,
            packet_bytes = packet.byte_len(),
            first_video_frame_pending = context.output_snapshot.first_video_frame_pending,
            recovery_waiting = recovery.waiting_for_keyframe(),
            recovery_skipped_packets = recovery.skipped_packets(),
            recovery_skipping_packet,
            "admitting FFmpeg video demux packet to decoder input"
        );
        if recovery_skipping_packet {
            let skipped_packets = recovery.record_skipped_packet(packet_nsecs);
            let skipped_span_nsecs = recovery.skipped_packet_span_nsecs();
            if skipped_packets == 1 || skipped_packets.is_multiple_of(60) {
                tracing::debug!(
                    pts = ?packet.best_timestamp(),
                    packet_nsecs = ?packet_nsecs,
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    "skipping FFmpeg video packets while waiting for decode recovery point"
                );
            }
            if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
                && context.output_snapshot.rebuffering
                && !context.output_snapshot.video_decode_underfill
                && self.hevc_decode_chain_watchdog.pending_fallback.is_none()
                && !self
                    .hevc_decode_chain_watchdog
                    .recovery_progress_grace_active(Instant::now(), hardware_accelerated)
                && (skipped_span_nsecs
                    .is_some_and(|span| span >= HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS)
                    || skipped_packets > VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS)
            {
                let fallback_target_nsecs = context
                    .output_snapshot
                    .video_output_rebuffer_anchor
                    .map(|anchor| anchor.timeline_nsecs)
                    .or(context.played_until_nsecs)
                    .or(packet_nsecs)
                    .unwrap_or_default();
                self.hevc_decode_chain_watchdog.pending_fallback = Some(HevcDecodeChainFallback {
                    target_nsecs: fallback_target_nsecs,
                    reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
                });
                tracing::debug!(
                    session_id = ?context.session_id,
                    fallback_target_nsecs,
                    packet_nsecs = ?packet_nsecs,
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    output_state = ?context.output_snapshot.state,
                    "hevc_decode_chain_recovery_wait_hard"
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
        } else {
            let skipped_packets = recovery.skipped_packets();
            let skipped_span_nsecs = recovery.skipped_packet_span_nsecs();
            if recovery.accept_hevc_recovery_point_after_wait_limit(packet, codec_id) {
                tracing::warn!(
                    pts = ?packet.best_timestamp(),
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    hard_skip_ms = HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS as f64 / 1_000_000.0,
                    max_skipped_packets = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
                    "resuming FFmpeg HEVC video decode at recovery point after bounded wait"
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
        }

        log_video_decode_packet_if_needed(packet, codec_id, *video_packet_count, recovery);
        let dovi_packet_rewrite = inspect_hevc_dovi_rpu_decode_packet(
            packet,
            codec_id,
            HevcDecodePacketLogContext {
                video_packet_count: *video_packet_count,
                first_video_frame_pending: context.output_snapshot.first_video_frame_pending,
                recovery_waiting: recovery.waiting_for_keyframe(),
            },
        )?;
        if let Some(metadata) = dovi_packet_rewrite.metadata().cloned() {
            tracing::trace!(
                pts = ?packet.best_timestamp(),
                profile = metadata.profile,
                profile5 = metadata.is_profile5(),
                rpu_bytes = metadata.rpu_payload.len(),
                "using Dolby Vision RPU metadata side channel for FFmpeg packet"
            );
            dovi_pipeline.observe_video_packet_metadata(packet, context.video_stream, metadata);
        } else {
            dovi_pipeline.observe_video_packet(packet, context.video_stream);
        }

        if dovi_packet_rewrite.drop_decode_packet() {
            return Ok(DecodePacketAdmissionStatus::Dropped);
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

        let generation = playback_generation.advance();
        let decode_packet = dovi_packet_rewrite.decode_packet(packet);
        let hardware_accelerated = self.info().hardware_accelerated;
        self.remember_hevc_startup_probe_packet(
            decode_packet,
            codec_id,
            context.output_snapshot,
            context.session_id,
        );
        let pending_packet = PendingVideoDecodePacket {
            generation,
            packet: AvPacket::ref_from(decode_packet)?,
            realign_after_decode_recovery: context.output_snapshot.first_video_frame_pending,
            hevc_startup_in_flight_watchdog: hevc_startup_in_flight_packet_should_arm(
                codec_id,
                hardware_accelerated,
            ),
        };
        let admission_status =
            self.try_enqueue_pending_packet(pending_packet, context.session_id)?;
        tracing::trace!(
            session_id = ?context.session_id,
            video_packet_admitted_count = *video_packet_count,
            admission_status = ?admission_status,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            codec = ?codec_id,
            packet_bytes = packet.byte_len(),
            "admitted FFmpeg video demux packet to decoder input"
        );
        Ok(admission_status)
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
        self.hevc_startup_probe_replay.clear();
    }

    pub(super) fn reset_hevc_decode_chain_transient_state(&mut self) {
        self.hevc_decode_chain_watchdog.reset();
        self.hevc_startup_probe_packets.clear();
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::Transient,
        );
    }

    pub(super) fn reset_hevc_decode_chain_recovery_transaction(&mut self) {
        self.reset_hevc_decode_chain_transient_state();
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::RecoveryTransaction,
        );
    }

    pub(super) fn observe_hevc_decode_packet_status(
        &mut self,
        observation: HevcDecodePacketObservation<'_>,
    ) -> HevcDecodeChainRecoveryAction {
        if observation.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.hevc_decode_chain_watchdog.reset();
            self.hevc_decode_packet_diagnostics.clear();
            self.hevc_startup_probe_packets.clear();
            return HevcDecodeChainRecoveryAction::None;
        }
        let packet_nsecs = observation.packet.best_timestamp().and_then(|timestamp| {
            timestamp_to_nsecs(timestamp, observation.video_stream.time_base)
        });
        let hardware_accelerated = self.info().hardware_accelerated;
        let action = self
            .hevc_decode_chain_watchdog
            .observe_packet(HevcDecodeChainWatchdogInput {
                session_id: observation.session_id,
                packet_nsecs,
                decoded_frames: observation.status.decoded_frames,
                decode_ok: observation.status.result.is_ok(),
                hardware_accelerated,
                output_snapshot: observation.output_snapshot,
                demux_watermark: observation.demux_watermark,
                has_audio_output: observation.has_audio_output,
                fallback_target_nsecs: observation.fallback_target_nsecs,
                now: Instant::now(),
            });
        let zero_output_run_packets = if observation.status.decoded_frames == 0 {
            self.hevc_decode_chain_watchdog.zero_output_packets
        } else {
            0
        };
        self.hevc_decode_packet_diagnostics.record(
            observation.status,
            observation.packet,
            observation.video_stream,
            zero_output_run_packets,
            hardware_accelerated,
        );
        action
    }

    pub(super) fn observe_hevc_decoded_frame_gap(
        &mut self,
        mut observation: HevcDecodedFrameGapObservation,
    ) -> HevcDecodedFrameGapAction {
        observation.recent_cache_read_anomaly =
            self.hevc_decode_packet_diagnostics.has_cache_read_anomaly();
        let action = self
            .hevc_decode_chain_watchdog
            .observe_decoded_frame_gap(observation);
        if observation.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && observation
                .previous_gap_nsecs
                .and_then(|gap| u64::try_from(gap).ok())
                .is_some_and(|gap| {
                    !video_timestamp_gap_within_threshold(gap, observation.max_gap_nsecs)
                })
        {
            self.log_hevc_decoded_frame_gap_diagnostics(observation, action);
        }
        action
    }

    fn log_hevc_decoded_frame_gap_diagnostics(
        &self,
        observation: HevcDecodedFrameGapObservation,
        action: HevcDecodedFrameGapAction,
    ) {
        let frame = observation.source_frame_diagnostic;
        let front_generation = self.front_generation();
        let front_packet = self.front_packet().map(|packet| {
            HevcPacketDiagnosticFields::from_packet(
                packet,
                observation.codec_id,
                self.info().time_base,
            )
        });
        let non_contiguous_cache_reads = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet
                    .packet
                    .cache_read
                    .is_some_and(|cache| cache.sequence_contiguous == Some(false))
            })
            .count();
        let packets_without_cache_diagnostic = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| packet.packet.cache_read.is_none())
            .count();
        let non_monotonic_dts_contiguous_cache_packets = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet.dts_delta_nsecs.is_some_and(|delta| delta < 0)
                    && packet
                        .packet
                        .cache_read
                        .is_some_and(|cache| cache.sequence_contiguous == Some(true))
            })
            .count();
        let repeated_cache_packet_reads = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet.packet.cache_read.is_some_and(|cache| {
                    cache.previous_read_generation == Some(cache.cache_generation)
                        && cache.previous_read_packet_id == Some(cache.packet_id)
                })
            })
            .count();
        tracing::debug!(
            session_id = ?observation.session_id,
            action = ?action,
            decoder_name = %self.info().decoder_name,
            hardware_accelerated = self.info().hardware_accelerated,
            video_time_base_num = self.info().time_base.num,
            video_time_base_den = self.info().time_base.den,
            frame_timeline_nsecs = observation.timeline_nsecs,
            frame_duration_nsecs = observation.duration_nsecs,
            previous_expected_next_nsecs = ?observation.previous_expected_next_nsecs,
            previous_gap_ms = ?observation
                .previous_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            max_gap_ms = observation.max_gap_nsecs as f64 / 1_000_000.0,
            frame_best_effort_timestamp = frame.best_effort_timestamp,
            frame_pts = frame.pts,
            frame_packet_dts = frame.packet_dts,
            frame_raw_duration = frame.duration,
            frame_flags = frame.flags,
            frame_key = frame.key_frame,
            frame_corrupt = frame.corrupt,
            frame_picture_type = frame.picture_type,
            frame_decode_error_flags = frame.decode_error_flags,
            frame_width = frame.width,
            frame_height = frame.height,
            frame_pixel_format = frame.pixel_format,
            recovery_waiting = observation.recovery_waiting,
            demux_underrun = observation.demux_watermark.underrun,
            demux_video_underrun = observation.demux_watermark.video_underrun,
            demux_audio_underrun = observation.demux_watermark.audio_underrun,
            demux_video_forward_ms = ?observation
                .demux_watermark
                .video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?observation
                .demux_watermark
                .audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_selected_min_forward_ms = ?observation
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            recent_completed_packet_diagnostics =
                self.hevc_decode_packet_diagnostics.packets.len(),
            non_contiguous_cache_reads,
            repeated_cache_packet_reads,
            packets_without_cache_diagnostic,
            non_monotonic_dts_contiguous_cache_packets,
            current_front_generation = ?front_generation,
            current_front_stream_index = ?front_packet.map(|packet| packet.stream_index),
            current_front_pts = ?front_packet.and_then(|packet| packet.pts),
            current_front_dts = ?front_packet.and_then(|packet| packet.dts),
            current_front_pts_nsecs = ?front_packet.and_then(|packet| packet.pts_nsecs),
            current_front_dts_nsecs = ?front_packet.and_then(|packet| packet.dts_nsecs),
            current_front_duration = ?front_packet.and_then(|packet| packet.duration),
            current_front_flags = ?front_packet.map(|packet| packet.flags),
            current_front_key = ?front_packet.map(|packet| packet.key_frame),
            current_front_recovery_point = ?front_packet.map(|packet| packet.recovery_point),
            current_front_safe_seek_point = ?front_packet.map(|packet| packet.safe_seek_point),
            current_front_packet_bytes = ?front_packet.map(|packet| packet.byte_len),
            current_front_cache_read = ?front_packet.and_then(|packet| packet.cache_read),
            "HEVC decoded frame PTS gap diagnostic snapshot"
        );

        for packet in &self.hevc_decode_packet_diagnostics.packets {
            let cache = packet.packet.cache_read;
            tracing::debug!(
                session_id = ?observation.session_id,
                diagnostic_ordinal = packet.ordinal,
                generation = packet.generation,
                hardware_accelerated = packet.hardware_accelerated,
                packet_stream_index = packet.packet.stream_index,
                packet_pts = ?packet.packet.pts,
                packet_dts = ?packet.packet.dts,
                packet_pts_nsecs = ?packet.packet.pts_nsecs,
                packet_dts_nsecs = ?packet.packet.dts_nsecs,
                packet_duration = ?packet.packet.duration,
                packet_duration_nsecs = ?packet.packet.duration_nsecs,
                packet_pts_delta_ms = ?packet
                    .pts_delta_nsecs
                    .map(|delta| delta as f64 / 1_000_000.0),
                packet_dts_delta_ms = ?packet
                    .dts_delta_nsecs
                    .map(|delta| delta as f64 / 1_000_000.0),
                packet_flags = packet.packet.flags,
                packet_key = packet.packet.key_frame,
                packet_recovery_point = packet.packet.recovery_point,
                packet_safe_seek_point = packet.packet.safe_seek_point,
                packet_bytes = packet.packet.byte_len,
                decoded_frames = packet.decoded_frames,
                zero_output_run_packets = packet.zero_output_run_packets,
                decode_ok = packet.decode_ok,
                decode_error = ?packet.decode_error,
                decode_elapsed_ms = packet.decode_elapsed_micros as f64 / 1_000.0,
                drained = packet.drained,
                cache_read_sequence = ?cache.map(|cache| cache.read_sequence),
                cache_generation = ?cache.map(|cache| cache.cache_generation),
                cache_read_range_id = ?cache.map(|cache| cache.read_range_id),
                cache_packet_id = ?cache.map(|cache| cache.packet_id),
                cache_stream_offset = ?cache.map(|cache| cache.stream_offset),
                cache_storage = ?cache.map(|cache| cache.storage),
                cache_read_index_before = ?cache.map(|cache| cache.read_index_before),
                cache_read_index_after = ?cache.map(|cache| cache.read_index_after),
                cache_reader_head_before = ?cache.and_then(|cache| cache.reader_head_before),
                cache_reader_head_after = ?cache.and_then(|cache| cache.reader_head_after),
                cache_previous_read_packet_id =
                    ?cache.and_then(|cache| cache.previous_read_packet_id),
                cache_previous_read_generation =
                    ?cache.and_then(|cache| cache.previous_read_generation),
                cache_previous_expected_next_packet_id =
                    ?cache.and_then(|cache| cache.previous_expected_next_packet_id),
                cache_sequence_contiguous = ?cache.and_then(|cache| cache.sequence_contiguous),
                cache_packet_start_nsecs = ?cache.and_then(|cache| cache.packet_start_nsecs),
                cache_packet_end_nsecs = ?cache.and_then(|cache| cache.packet_end_nsecs),
                cache_timeline_anchor = ?cache.map(|cache| cache.timeline_anchor),
                cache_recovery_point = ?cache.map(|cache| cache.recovery_point),
                cache_safe_seek_point = ?cache.map(|cache| cache.safe_seek_point),
                "HEVC decoded frame PTS gap recent decode packet diagnostic"
            );
        }
    }

    pub(super) fn observe_hevc_seek_preroll_progress(
        &mut self,
        observation: HevcSeekPrerollProgressObservation,
    ) {
        self.hevc_decode_chain_watchdog
            .observe_seek_preroll_progress(observation);
    }

    pub(super) fn observe_hevc_admitted_video_progress(
        &mut self,
        observation: HevcAdmittedVideoProgressObservation,
    ) {
        let admitted_progress = self
            .hevc_decode_chain_watchdog
            .observe_admitted_video_progress(observation);
        if admitted_progress && !self.hevc_decode_chain_watchdog.has_pending_fallback() {
            self.hevc_startup_probe_packets.clear();
            self.last_hevc_decode_chain_fallback = None;
        }
    }

    pub(super) fn observe_hevc_post_fallback_rebuffer_underfill(
        &mut self,
        observation: HevcPostFallbackRebufferObservation,
    ) {
        let hardware_accelerated = self.info().hardware_accelerated;
        if self
            .hevc_decode_chain_watchdog
            .recovery_progress_grace_active(observation.now, hardware_accelerated)
        {
            self.hevc_decode_chain_watchdog
                .post_fallback_rebuffer_underfill_started_at = None;
            return;
        }
        self.hevc_decode_chain_watchdog
            .observe_post_fallback_rebuffer_underfill(observation);
    }

    pub(super) fn observe_hevc_startup_stall(
        &mut self,
        observation: HevcStartupStallObservation,
    ) -> HevcDecodeChainRecoveryAction {
        self.hevc_decode_chain_watchdog
            .observe_startup_stall(observation)
    }

    pub(super) fn hevc_startup_stall_watchdog_deadline(&self) -> Option<Instant> {
        self.hevc_decode_chain_watchdog
            .startup_watchdog_deadline(self.info().hardware_accelerated)
    }

    pub(super) fn defer_hevc_startup_stall_watchdog_after_no_action(&mut self, now: Instant) {
        self.hevc_decode_chain_watchdog
            .defer_startup_watchdog_after_no_action(now);
    }

    pub(super) fn record_hevc_startup_stall_watchdog_rejection(
        &mut self,
        reason: &'static str,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_decode_chain_watchdog
            .record_startup_watchdog_rejection(reason, now)
    }

    pub(super) fn hevc_recent_video_progress_grace_active(&self, now: Instant) -> bool {
        self.hevc_decode_chain_watchdog
            .recovery_progress_grace_active(now, self.info().hardware_accelerated)
    }

    pub(super) fn hevc_decode_chain_stats(&self) -> HevcDecodeChainStats {
        self.hevc_decode_chain_watchdog.stats()
    }

    pub(super) fn take_hevc_decode_chain_fallback(&mut self) -> Option<HevcDecodeChainFallback> {
        self.hevc_decode_chain_watchdog.take_fallback()
    }

    pub(super) fn hevc_decode_chain_fallback_pending(&self) -> bool {
        self.hevc_decode_chain_watchdog.has_pending_fallback()
    }

    pub(super) fn hevc_decode_chain_fallback_loop_action(
        &self,
        fallback: HevcDecodeChainFallback,
    ) -> HevcDecodeChainFallbackLoopAction {
        hevc_decode_chain_fallback_loop_action(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
        )
    }

    pub(super) fn has_prior_matching_hevc_decode_chain_fallback(
        &self,
        fallback: HevcDecodeChainFallback,
    ) -> bool {
        self.last_hevc_decode_chain_fallback.is_some_and(|last| {
            hevc_fallback_targets_match(last.last_target_nsecs, fallback.target_nsecs)
                && last.last_reason == fallback.reason
        })
    }

    pub(super) fn remember_hevc_decode_chain_fallback(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        self.last_hevc_decode_chain_fallback = Some(hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        ));
    }

    pub(super) fn remember_hevc_decode_chain_software_suppression(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        let mut record = hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        );
        if record.low_level_seeks > 0 {
            record.post_low_level_suppressions =
                record.post_low_level_suppressions.saturating_add(1);
        } else {
            record.software_suppressions = record.software_suppressions.saturating_add(1);
        }
        self.last_hevc_decode_chain_fallback = Some(record);
    }

    pub(super) fn remember_hevc_decode_chain_low_level_seek(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        let mut record = hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        );
        record.low_level_seeks = record.low_level_seeks.saturating_add(1);
        self.last_hevc_decode_chain_fallback = Some(record);
    }

    pub(super) fn remember_hevc_recovery_low_level_seek_target(&mut self, target_nsecs: u64) {
        let Some(mut record) = self.last_hevc_decode_chain_fallback else {
            return;
        };
        record.last_target_nsecs = target_nsecs;
        record.hardware_accelerated = self.info().hardware_accelerated;
        record.recorded_at = Instant::now();
        record.low_level_seeks = record.low_level_seeks.saturating_add(1);
        self.last_hevc_decode_chain_fallback = Some(record);
    }

    pub(super) fn has_pending_or_in_flight(&self) -> bool {
        self.packets.has_pending_or_in_flight() || !self.hevc_startup_probe_replay.is_empty()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingVideoDecodePacket> {
        self.packets
            .take_pending_input()
            .or_else(|| self.hevc_startup_probe_replay.pop_front())
    }

    pub(super) fn push_in_flight(
        &mut self,
        packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) {
        let arm_hevc_startup_in_flight = packet.hevc_startup_in_flight_watchdog;
        self.packets.push_in_flight(packet);
        if arm_hevc_startup_in_flight {
            self.hevc_decode_chain_watchdog
                .arm_startup_in_flight_stall(session_id, Instant::now());
        }
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

    pub(super) fn reopen_software_decoder(
        &mut self,
        stream: StreamInfo,
    ) -> std::result::Result<bool, String> {
        if !self.info().hardware_accelerated {
            return Ok(false);
        }
        let decoder = Decoder::open_video(stream, HardwareDecodeMode::Off)
            .map_err(|error| format!("FFmpeg 重新打开软件视频解码器失败：{error}"))?;
        let worker = VideoDecodeWorker::spawn(decoder)?;
        self.worker.detach_without_join();
        self.worker = worker;
        self.clear_packets();
        self.hevc_decode_chain_watchdog.reset();
        Ok(true)
    }

    fn remember_hevc_startup_probe_packet(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        output_snapshot: PlaybackOutputSnapshot,
        session_id: PlaybackSessionId,
    ) {
        if !hevc_startup_probe_packet_should_record(
            codec_id,
            self.info().hardware_accelerated,
            output_snapshot,
        ) {
            return;
        }
        match self.hevc_startup_probe_packets.remember(packet) {
            Ok(true) => {
                tracing::trace!(
                    session_id = ?session_id,
                    packet_pts = ?packet.best_timestamp(),
                    hevc_startup_probe_packets = self.hevc_startup_probe_packets.len(),
                    "remembered HEVC startup probe packet for hardware decode fallback"
                );
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    session_id = ?session_id,
                    %error,
                    "failed to remember HEVC startup probe packet"
                );
            }
        }
    }
}

fn hevc_startup_probe_packet_should_record(
    codec_id: ffi::AVCodecID,
    hardware_accelerated: bool,
    output_snapshot: PlaybackOutputSnapshot,
) -> bool {
    codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && hardware_accelerated
        && (output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
        && output_snapshot.queued_video_frames == 0
}

fn hevc_startup_in_flight_packet_should_arm(
    codec_id: ffi::AVCodecID,
    hardware_accelerated: bool,
) -> bool {
    codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC && hardware_accelerated
}

fn hevc_decode_chain_fallback_loop_action(
    last: Option<HevcDecodeChainFallbackRecord>,
    fallback: HevcDecodeChainFallback,
    hardware_accelerated: bool,
) -> HevcDecodeChainFallbackLoopAction {
    let Some(last) = last else {
        return HevcDecodeChainFallbackLoopAction::Proceed;
    };
    if !hevc_fallback_targets_match(last.last_target_nsecs, fallback.target_nsecs) {
        return HevcDecodeChainFallbackLoopAction::Proceed;
    }
    if hardware_accelerated {
        return HevcDecodeChainFallbackLoopAction::ForceSoftware;
    }
    if fallback.target_nsecs == 0 {
        return HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek;
    }
    if last.low_level_seeks > 0 {
        return if last.post_low_level_suppressions == 0 {
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        } else {
            HevcDecodeChainFallbackLoopAction::RecoveryExhausted
        };
    }
    if last.software_suppressions == 0 {
        return HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek;
    }
    HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek
}

fn hevc_decode_chain_fallback_record_after(
    last: Option<HevcDecodeChainFallbackRecord>,
    fallback: HevcDecodeChainFallback,
    hardware_accelerated: bool,
    recorded_at: Instant,
) -> HevcDecodeChainFallbackRecord {
    let matching_last = last
        .filter(|last| hevc_fallback_targets_match(last.last_target_nsecs, fallback.target_nsecs));
    let mut record = matching_last.unwrap_or(HevcDecodeChainFallbackRecord {
        root_target_nsecs: fallback.target_nsecs,
        last_target_nsecs: fallback.target_nsecs,
        last_reason: fallback.reason,
        hardware_accelerated,
        recorded_at,
        software_suppressions: 0,
        post_low_level_suppressions: 0,
        low_level_seeks: 0,
    });
    record.last_target_nsecs = fallback.target_nsecs;
    record.last_reason = fallback.reason;
    record.hardware_accelerated = hardware_accelerated;
    record.recorded_at = recorded_at;
    record
}

fn hevc_fallback_targets_match(left: u64, right: u64) -> bool {
    left.abs_diff(right) <= HEVC_FALLBACK_SAME_TARGET_TOLERANCE_NSECS
}

fn hevc_decode_chain_recovery_record_after_reset(
    record: Option<HevcDecodeChainFallbackRecord>,
    scope: HevcDecodeChainResetScope,
) -> Option<HevcDecodeChainFallbackRecord> {
    match scope {
        HevcDecodeChainResetScope::Transient => record,
        HevcDecodeChainResetScope::RecoveryTransaction => None,
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

const HEVC_DOVI_STRIPPED_DECODE_REWRITE_ENABLED: bool = false;

fn inspect_hevc_dovi_rpu_decode_packet(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
    log_context: HevcDecodePacketLogContext,
) -> std::result::Result<DoviDecodePacketRewrite, String> {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return Ok(DoviDecodePacketRewrite::UseOriginal { metadata: None });
    }
    let Some(data) = packet.data() else {
        return Ok(DoviDecodePacketRewrite::UseOriginal { metadata: None });
    };
    let Some(inspection) = inspect_dovi_rpu_nalus(data) else {
        if should_debug_hevc_decode_packet_without_rpu(log_context) {
            tracing::debug!(
                packet_count = log_context.video_packet_count,
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                first_video_frame_pending = log_context.first_video_frame_pending,
                recovery_waiting = log_context.recovery_waiting,
                original_nals = %hevc_nal_summary(data, None),
                "HEVC decode packet has no Dolby Vision RPU NALs"
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
                "HEVC decode packet has no Dolby Vision RPU NALs"
            );
        }
        return Ok(DoviDecodePacketRewrite::UseOriginal { metadata: None });
    };

    let metadata = inspection.metadata.clone();
    let stripped_decode_action = hevc_dovi_decode_action_for_inspection(&inspection);
    let decode_packet_action = dovi_decode_packet_action_name(
        stripped_decode_action,
        HEVC_DOVI_STRIPPED_DECODE_REWRITE_ENABLED,
    );
    if should_debug_dovi_rpu_inspection(log_context, &inspection) {
        tracing::debug!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_bytes = inspection.stripped_bytes,
            nal_count = inspection.nal_count,
            kept_nal_count = inspection.kept_nal_count,
            stripped_nal_count = inspection.stripped_nal_count,
            stream_format = ?inspection.stream_format,
            rpu_metadata = metadata.is_some(),
            rpu_profile = ?metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            first_video_frame_pending = log_context.first_video_frame_pending,
            recovery_waiting = log_context.recovery_waiting,
            decode_packet_action,
            original_nals = %hevc_nal_summary(data, Some(inspection.stream_format)),
            "inspected Dolby Vision RPU NALs for HEVC decode"
        );
    } else if should_trace_hevc_decode_packet_nals(packet, log_context) {
        tracing::trace!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_bytes = inspection.stripped_bytes,
            nal_count = inspection.nal_count,
            kept_nal_count = inspection.kept_nal_count,
            stripped_nal_count = inspection.stripped_nal_count,
            stream_format = ?inspection.stream_format,
            rpu_metadata = metadata.is_some(),
            rpu_profile = ?metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            decode_packet_action,
            original_nals = %hevc_nal_summary(data, Some(inspection.stream_format)),
            "inspected Dolby Vision RPU NALs for HEVC decode"
        );
    }

    match stripped_decode_action {
        StrippedHevcDoviDecodeAction::DropMetadataOnly => {
            Ok(DoviDecodePacketRewrite::DropMetadataOnly { metadata })
        }
        StrippedHevcDoviDecodeAction::PassthroughUnparsedMetadataOnly => {
            Ok(DoviDecodePacketRewrite::UseOriginal { metadata })
        }
        StrippedHevcDoviDecodeAction::DecodeStripped
            if HEVC_DOVI_STRIPPED_DECODE_REWRITE_ENABLED =>
        {
            if let Some(stripped) = strip_dovi_rpu_nalus(data) {
                AvPacket::from_data_and_props(&stripped.data, packet).map(|packet| {
                    DoviDecodePacketRewrite::Decode {
                        packet,
                        metadata: stripped.metadata,
                    }
                })
            } else {
                Ok(DoviDecodePacketRewrite::UseOriginal { metadata })
            }
        }
        StrippedHevcDoviDecodeAction::DecodeStripped => {
            Ok(DoviDecodePacketRewrite::UseOriginal { metadata })
        }
    }
}

enum DoviDecodePacketRewrite {
    UseOriginal {
        metadata: Option<DoviFrameMetadata>,
    },
    Decode {
        packet: AvPacket,
        metadata: Option<DoviFrameMetadata>,
    },
    DropMetadataOnly {
        metadata: Option<DoviFrameMetadata>,
    },
}

impl DoviDecodePacketRewrite {
    fn metadata(&self) -> Option<&DoviFrameMetadata> {
        match self {
            Self::UseOriginal { metadata }
            | Self::Decode { metadata, .. }
            | Self::DropMetadataOnly { metadata } => metadata.as_ref(),
        }
    }

    fn drop_decode_packet(&self) -> bool {
        matches!(self, Self::DropMetadataOnly { .. })
    }

    fn decode_packet<'a>(&'a self, original: &'a AvPacket) -> &'a AvPacket {
        match self {
            Self::Decode { packet, .. } => packet,
            Self::UseOriginal { .. } => original,
            Self::DropMetadataOnly { .. } => {
                unreachable!("metadata-only Dolby Vision packets are not decoded")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrippedHevcDoviDecodeAction {
    DecodeStripped,
    DropMetadataOnly,
    PassthroughUnparsedMetadataOnly,
}

fn hevc_dovi_decode_action_for_inspection(
    inspection: &DoviRpuNalInspection,
) -> StrippedHevcDoviDecodeAction {
    if inspection.kept_nal_count > 0 {
        return StrippedHevcDoviDecodeAction::DecodeStripped;
    }

    if inspection.metadata.is_some() {
        StrippedHevcDoviDecodeAction::DropMetadataOnly
    } else {
        StrippedHevcDoviDecodeAction::PassthroughUnparsedMetadataOnly
    }
}

fn dovi_decode_packet_action_name(
    stripped_action: StrippedHevcDoviDecodeAction,
    stripped_decode_rewrite_enabled: bool,
) -> &'static str {
    match (stripped_action, stripped_decode_rewrite_enabled) {
        (StrippedHevcDoviDecodeAction::DropMetadataOnly, _) => "drop_metadata_only",
        (StrippedHevcDoviDecodeAction::PassthroughUnparsedMetadataOnly, _) => {
            "passthrough_unparsed_metadata_only"
        }
        (StrippedHevcDoviDecodeAction::DecodeStripped, true) => "decode_stripped",
        (StrippedHevcDoviDecodeAction::DecodeStripped, false) => "use_original",
    }
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

fn should_debug_dovi_rpu_inspection(
    context: HevcDecodePacketLogContext,
    inspection: &DoviRpuNalInspection,
) -> bool {
    context.recovery_waiting || inspection.metadata.is_none()
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
    use ffmpeg_sys_next as ffi;
    use std::time::{Duration, Instant};

    use crate::player::render_host::{PlaybackSessionId, RenderSize};

    use super::super::{
        DemuxReaderWatermark, PlaybackOutputSnapshot, PlaybackOutputState, StreamInfo,
        VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES, VideoFrameConvertContext,
        packet_is_video_recovery_point, packet_is_video_seek_point,
    };
    use super::{
        AudioTimelineGapEvidence, DecodedVideoFrameDiagnostic, DoviFrameMetadata,
        DoviRpuNalInspection, HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY,
        HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER, HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS,
        HEVC_STARTUP_IN_FLIGHT_HARD_AFTER, HEVC_STARTUP_PROBE_PACKET_LIMIT,
        HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL, HEVC_STARTUP_WATCHDOG_RETRY_AFTER,
        HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER, HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT,
        HevcAdmittedVideoProgressObservation, HevcDecodeChainFallback,
        HevcDecodeChainFallbackLoopAction, HevcDecodeChainFallbackReason,
        HevcDecodeChainFallbackRecord, HevcDecodeChainRecoveryAction, HevcDecodeChainResetScope,
        HevcDecodeChainWatchdog, HevcDecodeChainWatchdogInput, HevcDecodePacketDiagnosticWindow,
        HevcDecodedFrameGapAction, HevcDecodedFrameGapObservation,
        HevcPostFallbackRebufferObservation, HevcSeekPrerollProgressObservation,
        HevcStartupProbePackets, HevcStartupStallObservation, HevcStreamFormat,
        PlaybackBlockReason, PlaybackGeneration, StrippedHevcDoviDecodeAction,
        VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
        VideoDecodePacketStatus, VideoDecodePipeline, VideoDecodeRecovery, VideoDecodeWorkerInfo,
        VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, hevc_decode_chain_fallback_loop_action,
        hevc_decode_chain_fallback_record_after, hevc_decode_chain_recovery_record_after_reset,
        hevc_dovi_decode_action_for_inspection, hevc_startup_in_flight_packet_should_arm,
        hevc_startup_probe_packet_should_record, hevc_startup_probe_replay_packets,
        hevc_startup_zero_output_timeout, video_decode_pending_input_snapshot,
    };

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

    fn packet_from_data(data: &[u8]) -> crate::player::backend::ffmpeg::AvPacket {
        let props = crate::player::backend::ffmpeg::AvPacket::new().expect("packet props allocate");
        crate::player::backend::ffmpeg::AvPacket::from_data_and_props(data, &props)
            .expect("packet data allocates")
    }

    #[test]
    fn hevc_decode_packet_diagnostic_window_keeps_recent_packet_deltas() {
        let video_stream = StreamInfo {
            index: 0,
            stream: std::ptr::null_mut(),
            decoder: std::ptr::null(),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            time_base: ffi::AVRational { num: 1, den: 1_000 },
            start_nsecs: None,
            frame_duration_nsecs: Some(40_000_000),
        };
        let mut diagnostics = HevcDecodePacketDiagnosticWindow::default();
        for index in 0..HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY + 2 {
            let mut packet = packet_from_data(&[0, 0, 1, 0x02, index as u8]);
            unsafe {
                (*packet.as_mut_ptr()).pts = 1_000 + i64::try_from(index).unwrap() * 40;
                (*packet.as_mut_ptr()).dts = 960 + i64::try_from(index).unwrap() * 40;
                (*packet.as_mut_ptr()).duration = 40;
            }
            diagnostics.record(
                &VideoDecodePacketStatus {
                    generation: u64::try_from(index).unwrap(),
                    result: Ok(()),
                    decoded_frames: 0,
                    elapsed: Duration::from_micros(250),
                    drained: false,
                },
                &packet,
                video_stream,
                u64::try_from(index + 1).unwrap(),
                true,
            );
        }

        assert_eq!(
            diagnostics.packets.len(),
            HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY
        );
        assert_eq!(diagnostics.packets.front().unwrap().ordinal, 3);
        let latest = diagnostics.packets.back().unwrap();
        assert_eq!(latest.pts_delta_nsecs, Some(40_000_000));
        assert_eq!(latest.dts_delta_nsecs, Some(40_000_000));
        assert_eq!(latest.packet.duration_nsecs, Some(40_000_000));
        assert!(latest.hardware_accelerated);
        assert_eq!(
            latest.zero_output_run_packets,
            u64::try_from(HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY + 2).unwrap()
        );
    }

    fn output_snapshot(
        state: PlaybackOutputState,
        rebuffering: bool,
        video_output_low_water: bool,
        queued_video_range_nsecs: Option<(u64, u64)>,
        queued_video_forward_nsecs: Option<u64>,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state,
            first_video_frame_pending: state.first_video_frame_pending(),
            rebuffering,
            queued_video_frames: usize::from(queued_video_range_nsecs.is_some()),
            queued_video_duration_nsecs: queued_video_range_nsecs
                .map(|(start, end)| end.saturating_sub(start))
                .unwrap_or_default(),
            queued_video_range_nsecs,
            queued_video_forward_nsecs,
            queued_video_contiguous_forward_nsecs: queued_video_forward_nsecs,
            queued_video_largest_gap_nsecs: None,
            video_output_low_water,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
        }
    }

    fn demux_watermark(video_underrun: bool) -> DemuxReaderWatermark {
        DemuxReaderWatermark {
            video_forward_nsecs: Some(2_000_000_000),
            audio_forward_nsecs: Some(2_000_000_000),
            selected_min_forward_nsecs: Some(2_000_000_000),
            video_underrun,
            underrun: video_underrun,
            ..Default::default()
        }
    }

    fn hevc_watchdog_input(
        packet_nsecs: u64,
        output_snapshot: PlaybackOutputSnapshot,
        demux_watermark: DemuxReaderWatermark,
        fallback_target_nsecs: u64,
    ) -> HevcDecodeChainWatchdogInput {
        HevcDecodeChainWatchdogInput {
            session_id: PlaybackSessionId(1),
            packet_nsecs: Some(packet_nsecs),
            decoded_frames: 0,
            decode_ok: true,
            hardware_accelerated: true,
            output_snapshot,
            demux_watermark,
            has_audio_output: true,
            fallback_target_nsecs,
            now: Instant::now(),
        }
    }

    fn decoded_frame_gap_observation(
        codec_id: ffi::AVCodecID,
        output_snapshot: PlaybackOutputSnapshot,
    ) -> HevcDecodedFrameGapObservation {
        HevcDecodedFrameGapObservation {
            session_id: PlaybackSessionId(1),
            codec_id,
            timeline_nsecs: 257_720_000_000,
            duration_nsecs: 40_000_000,
            previous_expected_next_nsecs: Some(252_920_000_000),
            previous_gap_nsecs: Some(4_800_000_000),
            max_gap_nsecs: 200_000_000,
            fallback_target_nsecs: 252_900_000_000,
            audio_played_timeline_nsecs: Some(252_900_000_000),
            audio_timeline_gap: None,
            recovery_waiting: false,
            output_snapshot,
            demux_watermark: DemuxReaderWatermark::default(),
            source_frame_diagnostic: DecodedVideoFrameDiagnostic::default(),
            recent_cache_read_anomaly: false,
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
    fn in_flight_video_decode_command_queue_reports_decoder_in_flight() {
        let info = worker_info(false);
        let reason = VideoDecodePipeline::block_reason_for(
            snapshot(VideoDecodeWorkerState::Decoding, 0, 4),
            &info,
        );

        assert_eq!(reason, Some(PlaybackBlockReason::DecoderInFlight));
    }

    #[test]
    fn completed_video_decode_status_reports_decoder_output_pending_when_command_queue_full() {
        let info = worker_info(false);
        let mut snapshot = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
        snapshot.completed_packets = 1;
        let reason = VideoDecodePipeline::block_reason_for(snapshot, &info);

        assert_eq!(reason, Some(PlaybackBlockReason::DecoderOutputPending));
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

    #[test]
    fn hevc_zero_output_watchdog_soft_recovers_when_output_low_water_and_demux_ready() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let action = watchdog.observe_packet(hevc_watchdog_input(
            1_600_000_000,
            output_snapshot(
                PlaybackOutputState::Playing,
                false,
                true,
                Some((900_000_000, 1_000_000_000)),
                Some(100_000_000),
            ),
            demux_watermark(false),
            1_250_000_000,
        ));

        assert_eq!(action, HevcDecodeChainRecoveryAction::SoftRecovery);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_zero_output_watchdog_keeps_hardware_decode_when_rebuffer_has_video_headroom() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let queued_video_end_nsecs = 583_322_222;
        let action = watchdog.observe_packet(hevc_watchdog_input(
            1_375_322_222,
            output_snapshot(
                PlaybackOutputState::Rebuffering,
                true,
                false,
                Some((0, queued_video_end_nsecs)),
                Some(queued_video_end_nsecs),
            ),
            demux_watermark(false),
            0,
        ));

        assert_eq!(action, HevcDecodeChainRecoveryAction::None);
        assert!(!watchdog.soft_recovery_attempted);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_startup_zero_output_does_not_soft_recover_after_two_packets() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);

        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                40_000_000,
                startup,
                demux_watermark(false),
                0,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                80_000_000,
                startup,
                demux_watermark(false),
                0,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_startup_zero_output_first_frame_timeout_waits_for_hard_budget() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let now = Instant::now();
        let mut input = hevc_watchdog_input(40_000_000, startup, demux_watermark(false), 0);
        input.now = now;

        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.observe_startup_stall(HevcStartupStallObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                hardware_accelerated: true,
                video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
                now: now + Duration::from_millis(750),
                output_snapshot: startup,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 0,
            }),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_startup_zero_output_waits_until_hard_packet_budget() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT - 1 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    40_000_000 * (index + 1),
                    startup,
                    demux_watermark(false),
                    0,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_startup_zero_output_hard_fallbacks_after_timeout() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let now = Instant::now();
        let mut input =
            hevc_watchdog_input(120_000_000, startup, demux_watermark(false), 120_000_000);
        input.now = now;
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
            now: now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 120_000_000,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 120_000_000,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn hevc_startup_in_flight_hard_fallbacks_after_timeout_without_packet_status() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
        let now = Instant::now();
        let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

        assert_eq!(
            watchdog.observe_startup_stall(HevcStartupStallObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                hardware_accelerated: true,
                video_decode_snapshot: in_flight,
                now,
                output_snapshot: startup,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 0,
            }),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.startup_in_flight_deadline(),
            Some(now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
        );

        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: in_flight,
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 0,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            })
        );
    }

    #[test]
    fn hevc_startup_in_flight_deadline_can_be_armed_at_enqueue() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
        let now = Instant::now();
        let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
        watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

        assert_eq!(
            watchdog.startup_in_flight_deadline(),
            Some(now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
        );
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: in_flight,
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(true),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 0,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            })
        );
    }

    #[test]
    fn hevc_startup_in_flight_timeout_does_not_require_output_rebuffer_flag() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let playing_without_video =
            output_snapshot(PlaybackOutputState::Playing, false, false, None, None);
        let now = Instant::now();
        let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: in_flight,
            now,
            output_snapshot: playing_without_video,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: in_flight,
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: playing_without_video,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 0,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            })
        );
    }

    #[test]
    fn hevc_zero_output_packet_status_does_not_disarm_in_flight_deadline() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
        let now = Instant::now();
        watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

        let mut input = hevc_watchdog_input(40_000_000, startup, demux_watermark(false), 0);
        input.now = now + Duration::from_millis(500);
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.startup_in_flight_deadline(),
            Some(now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
        );

        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: snapshot(VideoDecodeWorkerState::Decoding, 0, 4),
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 0,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            })
        );
    }

    #[test]
    fn hevc_startup_in_flight_timeout_requires_hardware_decoder() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
        let now = Instant::now();
        let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: false,
            video_decode_snapshot: in_flight,
            now,
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: false,
            video_decode_snapshot: in_flight,
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_secs(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        });

        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.startup_in_flight_deadline(), None);
    }

    #[test]
    fn software_decoder_does_not_publish_hevc_startup_deadline() {
        let now = Instant::now();
        let watchdog = HevcDecodeChainWatchdog {
            first_zero_output_at: Some(now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
            ..Default::default()
        };

        assert_eq!(watchdog.startup_watchdog_deadline(false), None);
        assert!(watchdog.startup_watchdog_deadline(true).is_some());
    }

    #[test]
    fn rejected_hevc_startup_deadline_is_rearmed_in_the_future() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            first_zero_output_at: Some(
                now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER - Duration::from_millis(1),
            ),
            ..Default::default()
        };

        assert!(watchdog.startup_watchdog_deadline(true).unwrap() <= now);
        watchdog.defer_startup_watchdog_after_no_action(now);
        assert_eq!(
            watchdog.startup_watchdog_deadline(true),
            Some(now + HEVC_STARTUP_WATCHDOG_RETRY_AFTER)
        );
    }

    #[test]
    fn hevc_startup_rejection_logging_is_rate_limited() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog::default();

        assert_eq!(
            watchdog.record_startup_watchdog_rejection("decoder_not_decoding", now),
            Some(0)
        );
        assert_eq!(
            watchdog.record_startup_watchdog_rejection(
                "decoder_not_decoding",
                now + Duration::from_millis(1),
            ),
            None
        );
        assert_eq!(
            watchdog.record_startup_watchdog_rejection(
                "decoder_not_decoding",
                now + HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL,
            ),
            Some(1)
        );
    }

    #[test]
    fn software_long_gop_startup_timeout_scales_with_preroll_distance() {
        let target_nsecs = 669_625_000_000;
        let first_packet_nsecs = 663_833_000_000;
        let timeout =
            hevc_startup_zero_output_timeout(false, target_nsecs, Some(first_packet_nsecs));

        assert_eq!(timeout, Duration::from_millis(19_584));
        assert!(timeout > HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER);
    }

    #[test]
    fn software_long_gop_zero_output_does_not_fallback_at_hardware_timeout() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let target_nsecs = 669_625_000_000;
        let first_packet_nsecs = 663_833_000_000;
        let now = Instant::now();
        let mut first = hevc_watchdog_input(
            first_packet_nsecs,
            startup,
            demux_watermark(false),
            target_nsecs,
        );
        first.hardware_accelerated = false;
        first.now = now;
        assert_eq!(
            watchdog.observe_packet(first),
            HevcDecodeChainRecoveryAction::None
        );

        let mut target =
            hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
        target.hardware_accelerated = false;
        target.now = now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1);
        assert_eq!(
            watchdog.observe_packet(target),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.take_fallback(), None);

        let timeout =
            hevc_startup_zero_output_timeout(false, target_nsecs, Some(first_packet_nsecs));
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: false,
            video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
            now: now + timeout + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: target_nsecs,
        });
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn hevc_startup_zero_output_hard_fallbacks_after_packet_budget() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);

        for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    40_000_000 * (index + 1),
                    startup,
                    demux_watermark(false),
                    0,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 0,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn hevc_startup_zero_output_waits_for_seek_target_before_packet_budget_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let target_nsecs = 83_177_300_977;
        let first_preroll_packet_nsecs = 78_882_000_000;

        for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    first_preroll_packet_nsecs + 40_000_000 * index,
                    startup,
                    demux_watermark(false),
                    target_nsecs,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(watchdog.take_fallback(), None);

        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                target_nsecs,
                startup,
                demux_watermark(false),
                target_nsecs,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn hevc_startup_zero_output_timeout_waits_for_seek_target() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let target_nsecs = 83_177_300_977;
        let now = Instant::now();
        let mut preroll = hevc_watchdog_input(
            81_200_000_000,
            startup,
            demux_watermark(false),
            target_nsecs,
        );
        preroll.now = now;

        assert_eq!(
            watchdog.observe_packet(preroll),
            HevcDecodeChainRecoveryAction::None
        );
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: false,
            video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
            now: now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: target_nsecs,
        });
        assert_eq!(watchdog.take_fallback(), None);

        let mut target =
            hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
        target.now = now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(2);
        assert_eq!(
            watchdog.observe_packet(target),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn hevc_startup_probe_packets_keep_first_thirty_two_zero_output_packets() {
        let mut probe_packets = HevcStartupProbePackets::default();
        let packet = crate::player::backend::ffmpeg::AvPacket::new().expect("packet allocates");

        for _ in 0..HEVC_STARTUP_PROBE_PACKET_LIMIT {
            assert!(probe_packets.remember(&packet).expect("packet refs"));
        }
        assert!(!probe_packets.remember(&packet).expect("packet refs"));
        assert_eq!(probe_packets.len(), HEVC_STARTUP_PROBE_PACKET_LIMIT);
        assert_eq!(probe_packets.take().len(), HEVC_STARTUP_PROBE_PACKET_LIMIT);
        assert_eq!(probe_packets.len(), 0);
    }

    #[test]
    fn hevc_startup_probe_replay_preserves_all_packet_order() {
        let mut probe_packets = HevcStartupProbePackets::default();
        for id in 0..HEVC_STARTUP_PROBE_PACKET_LIMIT {
            let packet = packet_from_data(&[u8::try_from(id).expect("probe id fits")]);
            assert!(probe_packets.remember(&packet).expect("packet refs"));
        }

        let mut playback_generation = PlaybackGeneration::default();
        let replay =
            hevc_startup_probe_replay_packets(probe_packets.take(), &mut playback_generation);

        assert_eq!(replay.len(), HEVC_STARTUP_PROBE_PACKET_LIMIT);
        for (index, pending) in replay.iter().enumerate() {
            assert_eq!(pending.generation, index as u64 + 1);
            assert_eq!(pending.packet.data(), Some([index as u8].as_slice()));
            assert!(pending.realign_after_decode_recovery);
            assert!(!pending.hevc_startup_in_flight_watchdog);
        }
    }

    #[test]
    fn hevc_startup_probe_replay_reports_matching_pending_capacity() {
        assert_eq!(
            video_decode_pending_input_snapshot(0, HEVC_STARTUP_PROBE_PACKET_LIMIT),
            (
                HEVC_STARTUP_PROBE_PACKET_LIMIT,
                HEVC_STARTUP_PROBE_PACKET_LIMIT
            )
        );
        assert_eq!(
            video_decode_pending_input_snapshot(VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, 0),
            (
                VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
                VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY
            )
        );
    }

    #[test]
    fn hevc_startup_probe_packet_records_only_hardware_empty_startup_output() {
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let rebuffering =
            output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
        let playing = output_snapshot(PlaybackOutputState::Playing, false, false, None, None);
        let queued = output_snapshot(
            PlaybackOutputState::Rebuffering,
            true,
            false,
            Some((0, 40_000_000)),
            Some(40_000_000),
        );

        assert!(hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            startup,
        ));
        assert!(hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            rebuffering,
        ));
        assert!(!hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            false,
            startup,
        ));
        assert!(!hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            true,
            startup,
        ));
        assert!(!hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            playing,
        ));
        assert!(!hevc_startup_probe_packet_should_record(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            queued,
        ));
    }

    #[test]
    fn hevc_startup_in_flight_packet_arms_for_all_hevc_hardware_packets() {
        assert!(hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
        ));
        assert!(!hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            false,
        ));
        assert!(!hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            true,
        ));
    }

    #[test]
    fn hevc_decode_recovery_accepts_recovery_point_after_wait_limit() {
        let mut recovery = VideoDecodeRecovery::default();
        let non_recovery_packet =
            crate::player::backend::ffmpeg::AvPacket::new().expect("packet allocates");
        recovery.begin_with_realign(false);

        for index in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
            assert!(
                recovery.should_skip_packet(&non_recovery_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC)
            );
            recovery.record_skipped_packet(Some(index * 40_000_000));
        }

        let recovery_only_packet = packet_from_data(&[
            0, 0, 0, 3, 0x2a, 0x01, 0xaa, // CRA_NUT
        ]);
        assert!(packet_is_video_recovery_point(
            &recovery_only_packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!packet_is_video_seek_point(
            &recovery_only_packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(
            !recovery.should_skip_packet(&recovery_only_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC)
        );
        assert!(recovery.accept_hevc_recovery_point_after_wait_limit(
            &recovery_only_packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!recovery.waiting_for_keyframe());
    }

    #[test]
    fn hevc_seek_bootstrap_counts_preroll_and_clears_at_target_frame() {
        let mut recovery = VideoDecodeRecovery::default();
        let target_nsecs = 12_800_000_000;

        recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);

        let first_progress = recovery
            .observe_seek_preroll_frame(8_360_000_000)
            .expect("HEVC seek bootstrap tracks preroll");
        assert_eq!(first_progress.target_nsecs, target_nsecs);
        assert_eq!(first_progress.preroll_frames, 1);
        assert_eq!(recovery.seek_bootstrap_preroll_frames(), 1);

        let second_progress = recovery
            .observe_seek_preroll_frame(8_400_000_000)
            .expect("HEVC seek bootstrap keeps tracking preroll");
        assert_eq!(second_progress.preroll_frames, 2);
        assert_eq!(
            second_progress.first_preroll_frame_nsecs,
            Some(8_360_000_000)
        );
        assert_eq!(
            second_progress.last_preroll_frame_nsecs,
            Some(8_400_000_000)
        );

        let completed = recovery
            .finish_seek_bootstrap_after_target_frame(target_nsecs)
            .expect("first target frame completes bootstrap");
        assert_eq!(completed.preroll_frames, 2);
        assert_eq!(recovery.seek_bootstrap_preroll_frames(), 0);
        assert!(recovery.observe_seek_preroll_frame(8_440_000_000).is_none());
    }

    #[test]
    fn hevc_recovery_transaction_escalates_across_target_and_reason_drift() {
        let target_nsecs = 83_177_300_977;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
        };
        let now = Instant::now();
        let hardware_record = HevcDecodeChainFallbackRecord {
            root_target_nsecs: target_nsecs,
            last_target_nsecs: target_nsecs,
            last_reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            hardware_accelerated: true,
            recorded_at: now,
            software_suppressions: 0,
            post_low_level_suppressions: 0,
            low_level_seeks: 0,
        };
        let software_record = HevcDecodeChainFallbackRecord {
            hardware_accelerated: false,
            ..hardware_record
        };

        assert_eq!(
            hevc_decode_chain_fallback_loop_action(Some(hardware_record), fallback, true),
            HevcDecodeChainFallbackLoopAction::ForceSoftware
        );
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(Some(software_record), fallback, false),
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        );

        let mut suppressed_record = software_record;
        suppressed_record.software_suppressions = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(suppressed_record),
                HevcDecodeChainFallback {
                    target_nsecs: target_nsecs + 360_000_000,
                    reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
                },
                false,
            ),
            HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek
        );

        suppressed_record.low_level_seeks = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(Some(suppressed_record), fallback, false,),
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        );
        suppressed_record.post_low_level_suppressions = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(Some(suppressed_record), fallback, false,),
            HevcDecodeChainFallbackLoopAction::RecoveryExhausted
        );
    }

    #[test]
    fn hevc_recovery_record_keeps_root_target_across_fallback_drift() {
        let now = Instant::now();
        let first = HevcDecodeChainFallback {
            target_nsecs: 123_000_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        };
        let second = HevcDecodeChainFallback {
            target_nsecs: 123_360_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let first_record = hevc_decode_chain_fallback_record_after(None, first, false, now);
        let second_record = hevc_decode_chain_fallback_record_after(
            Some(first_record),
            second,
            false,
            now + Duration::from_millis(100),
        );

        assert_eq!(second_record.root_target_nsecs, first.target_nsecs);
        assert_eq!(second_record.last_target_nsecs, second.target_nsecs);
        assert_eq!(second_record.last_reason, second.reason);
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(Some(second_record), second, false,),
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        );
    }

    #[test]
    fn hevc_recovery_generation_transient_reset_preserves_fallback_record() {
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 123_360_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let record = hevc_decode_chain_fallback_record_after(None, fallback, false, Instant::now());

        assert_eq!(
            hevc_decode_chain_recovery_record_after_reset(
                Some(record),
                HevcDecodeChainResetScope::Transient,
            ),
            Some(record)
        );
        assert_eq!(
            hevc_decode_chain_recovery_record_after_reset(
                Some(record),
                HevcDecodeChainResetScope::RecoveryTransaction,
            ),
            None
        );
    }

    #[test]
    fn hevc_recovery_transaction_bounds_internal_seek_resets() {
        let cached_fallback = HevcDecodeChainFallback {
            target_nsecs: 123_000_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        };
        let zero_output_fallback = HevcDecodeChainFallback {
            target_nsecs: 123_360_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let now = Instant::now();
        let mut internal_seek_resets = 0;

        assert_eq!(
            hevc_decode_chain_fallback_loop_action(None, cached_fallback, true),
            HevcDecodeChainFallbackLoopAction::Proceed
        );
        internal_seek_resets += 1;
        let hardware_record =
            hevc_decode_chain_fallback_record_after(None, cached_fallback, true, now);
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(hardware_record),
                zero_output_fallback,
                true,
            ),
            HevcDecodeChainFallbackLoopAction::ForceSoftware
        );

        let software_record = hevc_decode_chain_fallback_record_after(
            Some(hardware_record),
            zero_output_fallback,
            false,
            now + Duration::from_millis(1),
        );
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(software_record),
                zero_output_fallback,
                false,
            ),
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        );

        let mut suppressed_record = software_record;
        suppressed_record.software_suppressions = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(suppressed_record),
                zero_output_fallback,
                false,
            ),
            HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek
        );
        internal_seek_resets += 1;

        suppressed_record.low_level_seeks = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(suppressed_record),
                zero_output_fallback,
                false,
            ),
            HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
        );
        suppressed_record.post_low_level_suppressions = 1;
        assert_eq!(
            hevc_decode_chain_fallback_loop_action(
                Some(suppressed_record),
                zero_output_fallback,
                false,
            ),
            HevcDecodeChainFallbackLoopAction::RecoveryExhausted
        );
        assert_eq!(internal_seek_resets, 2);
    }

    #[test]
    fn hevc_zero_output_watchdog_hard_fallback_after_soft_recovery_still_rebuffers() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let low_water = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_600_000_000,
                low_water,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::SoftRecovery
        );

        let rebuffering = output_snapshot(
            PlaybackOutputState::Rebuffering,
            true,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(50_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                2_100_000_000,
                rebuffering,
                demux_watermark(false),
                1_333_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 1_333_000_000,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn video_decode_recovery_tracks_skipped_packet_pts_span() {
        let mut recovery = VideoDecodeRecovery::default();
        recovery.begin_with_realign(false);

        assert_eq!(recovery.record_skipped_packet(Some(1_000_000_000)), 1);
        assert_eq!(recovery.skipped_packet_span_nsecs(), Some(0));
        assert_eq!(recovery.record_skipped_packet(Some(2_250_000_000)), 2);
        assert_eq!(recovery.skipped_packet_span_nsecs(), Some(1_250_000_000));

        recovery.reset();
        assert_eq!(recovery.skipped_packet_span_nsecs(), None);
    }

    #[test]
    fn hevc_post_fallback_rebuffer_underfill_uses_playback_target_for_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let mut rebuffering = output_snapshot(
            PlaybackOutputState::Rebuffering,
            true,
            true,
            Some((93_080_000_000, 93_200_000_000)),
            Some(120_000_000),
        );
        rebuffering.video_bootstrap_after_seek = true;
        let now = Instant::now();

        watchdog.observe_post_fallback_rebuffer_underfill(HevcPostFallbackRebufferObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            now,
            output_snapshot: rebuffering,
            demux_watermark: demux_watermark(false),
            audio_ready: true,
            fallback_target_nsecs: 93_080_000_000,
        });
        assert_eq!(watchdog.take_fallback(), None);

        watchdog.observe_post_fallback_rebuffer_underfill(HevcPostFallbackRebufferObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            now: now + HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER + Duration::from_millis(1),
            output_snapshot: rebuffering,
            demux_watermark: demux_watermark(false),
            audio_ready: true,
            fallback_target_nsecs: 93_080_000_000,
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 93_080_000_000,
                reason: HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill,
            })
        );
    }

    #[test]
    fn hevc_zero_output_watchdog_does_not_recover_when_demux_underruns() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let action = watchdog.observe_packet(hevc_watchdog_input(
            1_600_000_000,
            output_snapshot(
                PlaybackOutputState::Playing,
                false,
                true,
                Some((900_000_000, 1_000_000_000)),
                Some(100_000_000),
            ),
            demux_watermark(true),
            1_250_000_000,
        ));

        assert_eq!(action, HevcDecodeChainRecoveryAction::None);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_zero_output_watchdog_resets_only_after_admitted_video_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_100_000_000,
                snapshot,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 1);

        let mut progress = hevc_watchdog_input(
            1_133_000_000,
            snapshot,
            demux_watermark(false),
            1_250_000_000,
        );
        progress.decoded_frames = 1;
        assert_eq!(
            watchdog.observe_packet(progress),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 1);

        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 1_133_000_000,
            current_start_position_nsecs: 1_100_000_000,
            before_queue_end_nsecs: Some(1_100_000_000),
            after_queue_end_nsecs: Some(1_173_000_000),
        });
        assert_eq!(watchdog.zero_output_packets, 0);
        assert!(!watchdog.soft_recovery_attempted);
    }

    #[test]
    fn recent_software_video_progress_defers_packet_lead_recovery() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            last_video_progress_at: Some(now),
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Rebuffering,
            true,
            false,
            Some((1_000_000_000, 1_100_000_000)),
            Some(100_000_000),
        );
        let mut during_grace = hevc_watchdog_input(
            1_800_000_000,
            snapshot,
            demux_watermark(false),
            1_000_000_000,
        );
        during_grace.hardware_accelerated = false;
        during_grace.now = now + Duration::from_millis(1_999);

        assert_eq!(
            watchdog.observe_packet(during_grace),
            HevcDecodeChainRecoveryAction::None
        );
        assert!(!watchdog.soft_recovery_attempted);

        let mut after_grace = during_grace;
        after_grace.packet_nsecs = Some(1_833_000_000);
        after_grace.now = now + Duration::from_millis(2_001);
        assert_eq!(
            watchdog.observe_packet(after_grace),
            HevcDecodeChainRecoveryAction::SoftRecovery
        );
    }

    #[test]
    fn admitted_video_progress_cancels_transient_rebuffer_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog {
            pending_fallback: Some(HevcDecodeChainFallback {
                target_nsecs: 1_000_000_000,
                reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            }),
            ..Default::default()
        };

        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 1_000_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: None,
            after_queue_end_nsecs: Some(1_041_666_666),
        });

        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_recent_zero_output_evidence_alone_does_not_trigger_gap_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 178,
            recent_zero_output_packets: 178,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((144_067_000_000, 144_700_333_333)),
            Some(633_000_000),
        );

        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 144_700_000_000,
            current_start_position_nsecs: 69_267_000_000,
            before_queue_end_nsecs: Some(144_700_333_333),
            after_queue_end_nsecs: Some(144_733_333_333),
        });

        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.recent_zero_output_packets, 178);
        assert!(watchdog.recent_packet_lead_exceeded);

        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 144_967_000_000;
        gap.duration_nsecs = 33_333_333;
        gap.previous_expected_next_nsecs = Some(144_733_333_333);
        gap.previous_gap_nsecs = Some(233_666_667);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 144_733_333_333;
        gap.audio_played_timeline_nsecs = Some(144_040_652_981);

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::Admit
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn clean_hevc_keyframe_bridges_small_decode_gap_without_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 3,
            recent_zero_output_packets: 12,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((118_816_666_667, 119_649_999_999)),
            Some(847_832_951),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 119_866_666_667;
        gap.duration_nsecs = 16_666_666;
        gap.previous_expected_next_nsecs = Some(119_649_999_999);
        gap.previous_gap_nsecs = Some(216_666_668);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 119_649_999_999;
        gap.audio_played_timeline_nsecs = Some(118_802_167_048);
        gap.demux_watermark = demux_watermark(false);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
        );
        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
    }

    #[test]
    fn clean_hevc_keyframe_bridges_rounding_edge_decode_gap_from_seek_log() {
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 3,
            recent_zero_output_packets: 12,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Syncing,
            false,
            false,
            Some((174_066_666_667, 174_116_666_666)),
            Some(49_999_999),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 174_616_666_667;
        gap.duration_nsecs = 16_666_666;
        gap.previous_expected_next_nsecs = Some(174_116_666_666);
        gap.previous_gap_nsecs = Some(500_000_001);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 174_116_666_666;
        gap.audio_played_timeline_nsecs = Some(174_066_666_667);
        gap.demux_watermark = demux_watermark(false);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
        );
        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
    }

    #[test]
    fn clean_hevc_keyframe_does_not_bridge_decode_gap_beyond_rounding_tolerance() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Syncing,
            false,
            false,
            Some((174_066_666_667, 174_116_666_666)),
            Some(49_999_999),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 174_616_667_667;
        gap.duration_nsecs = 16_666_666;
        gap.previous_expected_next_nsecs = Some(174_116_666_666);
        gap.previous_gap_nsecs = Some(500_001_001);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 174_116_666_666;
        gap.audio_played_timeline_nsecs = Some(174_066_666_667);
        gap.demux_watermark = demux_watermark(false);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::Admit
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn strong_hevc_gap_evidence_defers_fallback_while_output_queue_is_healthy() {
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 24,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((1_000_000_000, 1_800_000_000)),
            Some(800_000_000),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            corrupt: true,
            decode_error_flags: 1,
            ..Default::default()
        };
        gap.demux_watermark = demux_watermark(false);

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::DeferFallback
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn strong_hevc_gap_evidence_requests_fallback_at_output_low_water() {
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 24,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((1_000_000_000, 1_120_000_000)),
            Some(120_000_000),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            corrupt: true,
            decode_error_flags: 1,
            ..Default::default()
        };
        gap.demux_watermark = demux_watermark(false);

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::DropForFallback
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 252_920_000_000,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
    }

    #[test]
    fn hevc_recent_gap_evidence_clears_after_contiguous_healthy_progress() {
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 24,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let frame_duration_nsecs = 40_000_000_u64;
        let healthy_frames_before_clear =
            usize::try_from(HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS / frame_duration_nsecs)
                .expect("healthy frame count fits");
        let mut before_queue_end_nsecs = 1_000_000_000_u64;

        for _ in 0..healthy_frames_before_clear {
            let after_queue_end_nsecs = before_queue_end_nsecs.saturating_add(frame_duration_nsecs);
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                frame_timeline_nsecs: before_queue_end_nsecs,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(before_queue_end_nsecs),
                after_queue_end_nsecs: Some(after_queue_end_nsecs),
            });
            before_queue_end_nsecs = after_queue_end_nsecs;
        }

        assert_eq!(watchdog.recent_zero_output_packets, 24);
        assert!(watchdog.recent_packet_lead_exceeded);

        let after_queue_end_nsecs = before_queue_end_nsecs.saturating_add(frame_duration_nsecs);
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: before_queue_end_nsecs,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(before_queue_end_nsecs),
            after_queue_end_nsecs: Some(after_queue_end_nsecs),
        });

        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert!(!watchdog.recent_packet_lead_exceeded);
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 0);
    }

    #[test]
    fn hevc_zero_output_watchdog_ignores_dropped_before_start_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_100_000_000,
                snapshot,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );

        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 1_050_000_000,
            current_start_position_nsecs: 1_100_000_000,
            before_queue_end_nsecs: Some(1_100_000_000),
            after_queue_end_nsecs: Some(1_100_000_000),
        });

        assert_eq!(watchdog.zero_output_packets, 1);
    }

    #[test]
    fn hevc_zero_output_watchdog_resets_after_seek_preroll_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_100_000_000,
                snapshot,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 1);

        watchdog.observe_seek_preroll_progress(HevcSeekPrerollProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 1_050_000_000,
            target_nsecs: 1_250_000_000,
            preroll_frames: 1,
        });

        assert_eq!(watchdog.zero_output_packets, 0);
        assert!(!watchdog.soft_recovery_attempted);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_zero_output_pts_gap_fallback_is_cancelled_by_admitted_video_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_500_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::SoftRecovery
        );

        assert_eq!(
            watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                snapshot,
            )),
            HevcDecodedFrameGapAction::DropForFallback
        );
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 257_720_000_000,
            current_start_position_nsecs: 252_900_000_000,
            before_queue_end_nsecs: Some(252_920_000_000),
            after_queue_end_nsecs: Some(257_760_000_000),
        });

        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_zero_output_pts_gap_fallback_is_cancelled_by_seek_preroll_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_500_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::SoftRecovery
        );
        assert_eq!(
            watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                snapshot,
            )),
            HevcDecodedFrameGapAction::DropForFallback
        );
        watchdog.observe_seek_preroll_progress(HevcSeekPrerollProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            frame_timeline_nsecs: 252_880_000_000,
            target_nsecs: 252_920_000_000,
            preroll_frames: 1,
        });

        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_large_pts_gap_without_decode_chain_evidence_does_not_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );

        assert_eq!(
            watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                snapshot,
            )),
            HevcDecodedFrameGapAction::Admit
        );

        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn synchronized_audio_video_timeline_gap_is_admitted_without_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 19,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((1_252_668_000_000, 1_254_127_708_333)),
            Some(1_459_708_333),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 1_254_962_000_000;
        gap.duration_nsecs = 41_708_333;
        gap.previous_expected_next_nsecs = Some(1_254_127_708_333);
        gap.previous_gap_nsecs = Some(834_291_667);
        gap.max_gap_nsecs = 200_000_000;
        gap.audio_timeline_gap = Some(AudioTimelineGapEvidence {
            previous_end_nsecs: 1_254_112_004_496,
            next_start_nsecs: 1_254_944_004_496,
        });

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap
        );
        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert!(!watchdog.recent_packet_lead_exceeded);
    }

    #[test]
    fn video_only_pts_gap_with_weak_recent_evidence_does_not_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 19,
            recent_packet_lead_exceeded: true,
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((1_252_668_000_000, 1_254_127_708_333)),
            Some(1_459_708_333),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 1_254_962_000_000;
        gap.previous_expected_next_nsecs = Some(1_254_127_708_333);
        gap.previous_gap_nsecs = Some(834_291_667);
        gap.max_gap_nsecs = 200_000_000;

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::Admit
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn non_hevc_large_pts_gap_does_not_trigger_hevc_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_500_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::SoftRecovery
        );

        assert_eq!(
            watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
                ffi::AVCodecID::AV_CODEC_ID_H264,
                snapshot,
            )),
            HevcDecodedFrameGapAction::Admit
        );

        assert_eq!(watchdog.take_fallback(), None);
    }

    fn dovi_inspection(
        kept_nal_count: usize,
        metadata: Option<DoviFrameMetadata>,
    ) -> DoviRpuNalInspection {
        DoviRpuNalInspection {
            metadata,
            stream_format: HevcStreamFormat::ByteStream,
            nal_count: kept_nal_count.saturating_add(1),
            kept_nal_count,
            stripped_nal_count: 1,
            stripped_bytes: 32,
        }
    }

    fn dovi_metadata() -> DoviFrameMetadata {
        DoviFrameMetadata {
            profile: 5,
            profile5: true,
            rpu_nalu: vec![0x7c, 0x01],
            rpu_payload: vec![0xaa],
        }
    }

    #[test]
    fn unparsed_rpu_only_packet_uses_original_decode_packet() {
        assert_eq!(
            hevc_dovi_decode_action_for_inspection(&dovi_inspection(0, None)),
            StrippedHevcDoviDecodeAction::PassthroughUnparsedMetadataOnly
        );
    }

    #[test]
    fn parsed_rpu_only_packet_still_drops() {
        assert_eq!(
            hevc_dovi_decode_action_for_inspection(&dovi_inspection(0, Some(dovi_metadata()))),
            StrippedHevcDoviDecodeAction::DropMetadataOnly
        );
    }

    #[test]
    fn mixed_dovi_packet_keeps_decode_action() {
        assert_eq!(
            hevc_dovi_decode_action_for_inspection(&dovi_inspection(1, None)),
            StrippedHevcDoviDecodeAction::DecodeStripped
        );
    }
}
