use ffmpeg_sys_next as ffi;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::player::{
    dovi::{
        DoviFrameMetadata, DoviRpuNalInspection, HevcStreamFormat, inspect_dovi_rpu_nalus,
        strip_dovi_rpu_nalus,
    },
    render_host::{PlaybackSessionId, VulkanDecodeDevice, VulkanPrewarmTicket},
};

use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoder_packet_queue::DecoderPacketQueues;
use super::output_gate::{DECODE_RECOVERY_HOLD_GAP_MAX_NSECS, decode_recovery_gap_within_limit};
use super::scheduled_video_queue::{
    VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS, queued_video_continuity_gap_threshold_nsecs,
    should_drop_late_video_frame, video_timestamp_gap_within_threshold,
};
use super::video_decode_framedrop::{VideoDecodeDropPolicy, VideoDecodeFrameDrop};
use super::video_decode_worker::{
    VideoDecodeDrainResult, VideoDecodeEnqueueResult, VideoDecodePacketStatus, VideoDecodeWorker,
    VideoDecodeWorkerInfo, VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, VideoDecodedFrame,
};
use super::video_frame_prepare_worker::DecodedVideoFrameDiagnostic;
use super::{
    AvPacket, AvPacketReadDiagnostic, CORRUPT_VIDEO_FRAME_RECOVERY_ERROR, Decoder,
    DemuxReaderWatermark, DoviPipeline, HardwareDecodeMode, PlaybackBlockReason,
    PlaybackGeneration, PlaybackOutputSnapshot, PlaybackOutputState, StreamInfo,
    VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE,
    VideoRecoveryPointKind, duration_nsecs, packet_is_video_recovery_point,
    packet_is_video_seek_point, packet_video_recovery_point_kind, stream_has_dovi_config,
    timestamp_to_nsecs,
};

const VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 8;
pub(super) const HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT: u64 = 24;
const HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT: u64 = 30;
const HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS: u64 = 500_000_000;
const HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS: u64 = 1_000_000_000;
// mpv keeps driving the receive-first lavc loop while packets are consumed
// successfully and only treats actual decode errors as hardware failures. A
// damaged HEVC GOP can therefore produce no frames until the next IDR without
// causing hwdec fallback. Keep a five-second no-boundary guard, plus 500ms for
// HEVC reordering, before a stream without any usable recovery point requests
// destructive recovery. Low water, the post-IDR grace, and a confirmed decoded
// PTS gap remain faster paths for a decoder that is genuinely stuck.
const HEVC_DECODE_CHAIN_STABLE_OUTPUT_HARD_PACKET_LEAD_NSECS: u64 = 5_500_000_000;
const HEVC_DECODE_CHAIN_SAFE_ANCHOR_GRACE_PACKETS: u64 = 8;
const HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS: u64 = 1_000_000_000;
const HEVC_POST_FALLBACK_REBUFFER_UNDERFILL_NSECS: u64 = 250_000_000;
const HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER: Duration = Duration::from_millis(1_500);
const HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT: u64 = 32;
const HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER: Duration = Duration::from_millis(2_000);
const HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_BASE_AFTER: Duration = Duration::from_millis(8_000);
const HEVC_SOFTWARE_STARTUP_ZERO_OUTPUT_MAX_AFTER: Duration = Duration::from_millis(30_000);
const HEVC_STARTUP_ZERO_OUTPUT_HARD_MIN_FORWARD_NSECS: u64 = 1_000_000_000;
const HEVC_STARTUP_IN_FLIGHT_HARD_AFTER: Duration = Duration::from_millis(2_000);
const HEVC_STARTUP_STALL_TARGET_PROXIMITY_NSECS: u64 = 500_000_000;
const HEVC_STARTUP_WATCHDOG_RETRY_AFTER: Duration = Duration::from_millis(25);
const HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
// The failure trace needs the safe 677.866s IDR to remain available through
// the 690.633s reopen cutoff (12.767s). Keep a little time margin while the
// packet and byte limits below continue to make the journal strictly bounded.
const HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS: u64 = 15_000_000_000;
const HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS: usize = 1_024;
// Keep enough decode-order tail for four lavc frame threads plus HEVC B-frame
// reordering, but do not replay packets that arrived seconds after the frozen
// recovery cutoff. mpv likewise requeues only the packets saved for the
// failed decoding boundary instead of an ever-growing live packet history.
const HEVC_HW_REPLAY_REORDER_TAIL_PACKETS: usize = 16;
// The 20:00 high-bitrate trace retained 33.4 MiB through 1200.233s but needed
// another 833ms to cover the frozen recovery cutoff. Give the 15-second time
// bound enough byte headroom for this ~33Mbps HEVC stream; packet and duration
// limits remain independent hard bounds.
const HEVC_HW_REPLAY_JOURNAL_MAX_BYTES: usize = 64 * 1024 * 1024;
const HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME: Duration = Duration::from_secs(8);
const HEVC_SAME_HARDWARE_DRAIN_GRACE: Duration = Duration::from_millis(100);
const HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT: Duration = Duration::from_secs(1);
// Any replay source can briefly reproduce the decodable prefix of a damaged
// HEVC GOP. Keep the transaction armed after its first visible output commit
// until two seconds of continuous decoded progress proves that it crossed the
// bad interval instead of merely replaying the same short prefix.
const HEVC_SAME_HARDWARE_REPLAY_STABLE_PROGRESS_NSECS: u64 = 2_000_000_000;
// A clean cached-IDR rebuild can still reach a damaged open interval whose
// inter pictures are undecodable until the next IDR. Keep feeding that final
// Vulkan attempt for the same bounded gap that the output transaction can
// bridge, instead of applying the ordinary one-second/30-packet watchdog and
// terminating just before the next recovery point.
const HEVC_SAME_HARDWARE_CACHED_REBUILD_MAX_PACKET_LEAD_NSECS: u64 = 5_000_000_000;
const HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);
const HEVC_SAME_HARDWARE_LOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);
const HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT: Duration = Duration::from_millis(500);
const HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS: u8 = 1;
const HEVC_SAME_HARDWARE_MAX_REOPEN_ATTEMPTS: u8 = 1;
const HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS: u64 = 500_000_000;
const HEVC_HARDWARE_RECOVERY_PROGRESS_GRACE: Duration = Duration::from_millis(750);
const HEVC_SOFTWARE_RECOVERY_PROGRESS_GRACE: Duration = Duration::from_millis(2_000);
const HEVC_FALLBACK_SAME_TARGET_TOLERANCE_NSECS: u64 = 500_000_000;
const HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS: u64 = 500_000_000;
const HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY: usize = 32;
// Match mpv's hr-seek framedrop boundary: decoder-level non-reference skipping
// is only useful before the target and must be disabled before target frames
// are submitted, otherwise an exact landing frame can be discarded forever.
const EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS: u64 = 5_000_000;

pub(super) struct PendingVideoDecodePacket {
    pub(super) generation: u64,
    pub(super) packet: AvPacket,
    drop_policy: VideoDecodeDropPolicy,
    pub(super) realign_after_decode_recovery: bool,
    hevc_startup_in_flight_watchdog: bool,
    from_hevc_hw_replay: bool,
    hevc_decode_recovery_evidence_scoped: bool,
}

impl PendingVideoDecodePacket {
    pub(super) fn has_hevc_decode_recovery_evidence_scope(&self) -> bool {
        self.hevc_decode_recovery_evidence_scoped
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodeChainRecoveryAction {
    None,
    SoftRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcDecodeRecoveryAction {
    None,
    DrainPendingResults,
    FlushSameHardware,
    ReopenSameHardware,
    ReplaySameHardware,
    RebuildFromCachedSeek,
    RequestSoftwareFallback,
    FailExplicitly,
}

impl HevcDecodeRecoveryAction {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DrainPendingResults => "drain_pending_results",
            Self::FlushSameHardware => "flush_same_hardware",
            Self::ReopenSameHardware => "reopen_same_hardware",
            Self::ReplaySameHardware => "replay_same_hardware",
            Self::RebuildFromCachedSeek => "rebuild_from_cached_seek",
            Self::RequestSoftwareFallback => "request_software_fallback",
            Self::FailExplicitly => "fail_explicitly",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcSameHardwareRecoveryPhase {
    DrainingResults,
    Flushing,
    ReplayingAfterFlush,
    Reopening,
    PrewarmingAfterReopen,
    RebuildingFromCache,
    ReplayingAfterReopen,
    Recovered,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcSameHardwareRecoveryAttemptKind {
    FlushReplay,
    VulkanReopenReplay,
    CachedSafeIdrRebuild,
}

impl HevcSameHardwareRecoveryAttemptKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::FlushReplay => "flush_replay",
            Self::VulkanReopenReplay => "vulkan_reopen_replay",
            Self::CachedSafeIdrRebuild => "cached_safe_idr_rebuild",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HevcAdmittedVideoProgress {
    None,
    Partial,
    Stable,
}

#[derive(Clone, Debug)]
struct HevcSameHardwareRecoveryAttempt {
    attempt_id: u64,
    decoder_epoch: u64,
    kind: HevcSameHardwareRecoveryAttemptKind,
    generation_floor: u64,
    started_at: Instant,
    last_staged_progress_at: Option<Instant>,
    last_staged_end_nsecs: Option<u64>,
    last_admitted_progress_at: Option<Instant>,
    first_admitted_nsecs: Option<u64>,
    last_admitted_end_nsecs: Option<u64>,
    admitted_span_after_catch_up_nsecs: u64,
    catch_up_barrier_nsecs: Option<u64>,
    consecutive_zero_output_packets: u64,
    input_high_water_nsecs: Option<u64>,
    output_high_water_nsecs: Option<u64>,
    output_commit_observed: bool,
    hard_failure: Option<&'static str>,
    replay_packets: usize,
}

impl HevcSameHardwareRecoveryAttempt {
    fn new(
        attempt_id: u64,
        decoder_epoch: u64,
        kind: HevcSameHardwareRecoveryAttemptKind,
        generation_floor: u64,
        target_nsecs: u64,
        now: Instant,
    ) -> Self {
        Self {
            attempt_id,
            decoder_epoch,
            kind,
            generation_floor,
            started_at: now,
            last_staged_progress_at: None,
            last_staged_end_nsecs: None,
            last_admitted_progress_at: None,
            first_admitted_nsecs: None,
            last_admitted_end_nsecs: None,
            admitted_span_after_catch_up_nsecs: 0,
            catch_up_barrier_nsecs: None,
            consecutive_zero_output_packets: 0,
            input_high_water_nsecs: None,
            output_high_water_nsecs: Some(target_nsecs),
            output_commit_observed: false,
            hard_failure: None,
            replay_packets: 0,
        }
    }

    fn observes_generation(&self, generation: u64) -> bool {
        generation >= self.generation_floor
    }

    fn stable_progress_window_nsecs(&self) -> u64 {
        HEVC_SAME_HARDWARE_REPLAY_STABLE_PROGRESS_NSECS
    }

    fn progress_timeout(&self) -> Duration {
        if self.kind == HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild {
            HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT
        } else {
            HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT
        }
    }

    fn observe_packet(&mut self, generation: u64, packet_nsecs: Option<u64>, decoded_frames: u64) {
        if !self.observes_generation(generation) {
            return;
        }
        self.input_high_water_nsecs = max_optional_u64(self.input_high_water_nsecs, packet_nsecs);
        if decoded_frames > 0 {
            self.consecutive_zero_output_packets = 0;
            self.clear_packet_derived_hard_failure();
            return;
        }

        self.consecutive_zero_output_packets =
            self.consecutive_zero_output_packets.saturating_add(1);
        let packet_lead_nsecs = self
            .input_high_water_nsecs
            .zip(self.output_high_water_nsecs)
            .map(|(input, output)| input.saturating_sub(output));
        let cached_safe_idr_rebuild =
            self.kind == HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild;
        if !cached_safe_idr_rebuild
            && self.consecutive_zero_output_packets
                >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
        {
            self.hard_failure = Some("attempt reached 30 consecutive zero-output packets");
        } else if packet_lead_nsecs.is_some_and(|lead| {
            if cached_safe_idr_rebuild {
                !decode_recovery_gap_within_limit(
                    lead,
                    HEVC_SAME_HARDWARE_CACHED_REBUILD_MAX_PACKET_LEAD_NSECS,
                )
            } else {
                lead >= HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS
            }
        }) {
            self.hard_failure = Some(if cached_safe_idr_rebuild {
                "cached rebuild packet lead reached five seconds"
            } else {
                "attempt packet lead reached one second"
            });
        }
    }

    fn clear_packet_derived_hard_failure(&mut self) {
        if matches!(
            self.hard_failure,
            Some("attempt reached 30 consecutive zero-output packets")
                | Some("cached rebuild packet lead reached five seconds")
                | Some("attempt packet lead reached one second")
        ) {
            self.hard_failure = None;
        }
    }

    fn observe_admitted_video_progress(
        &mut self,
        input: HevcAdmittedVideoProgressObservation,
        now: Instant,
    ) -> HevcAdmittedVideoProgress {
        if !self.observes_generation(input.generation) {
            return HevcAdmittedVideoProgress::None;
        }
        let Some(after) = input.after_queue_end_nsecs else {
            return HevcAdmittedVideoProgress::None;
        };
        if input
            .before_queue_end_nsecs
            .is_some_and(|before| after <= before)
        {
            return HevcAdmittedVideoProgress::None;
        }

        let continuity_gap_threshold_nsecs =
            queued_video_continuity_gap_threshold_nsecs(input.frame_duration_nsecs);
        let contiguous_with_previous = input.before_queue_end_nsecs.is_some_and(|before| {
            input.frame_timeline_nsecs <= before.saturating_add(continuity_gap_threshold_nsecs)
        });
        if self.first_admitted_nsecs.is_some() && !contiguous_with_previous {
            self.admitted_span_after_catch_up_nsecs = 0;
            self.catch_up_barrier_nsecs = None;
        }

        self.first_admitted_nsecs
            .get_or_insert(input.frame_timeline_nsecs);
        self.last_admitted_end_nsecs = Some(after);
        self.last_admitted_progress_at = Some(now);
        self.output_high_water_nsecs = max_optional_u64(self.output_high_water_nsecs, Some(after));
        self.clear_packet_derived_hard_failure();
        let barrier = *self.catch_up_barrier_nsecs.get_or_insert_with(|| {
            self.input_high_water_nsecs
                .unwrap_or(input.frame_timeline_nsecs)
        });
        let before = if contiguous_with_previous {
            input
                .before_queue_end_nsecs
                .unwrap_or(input.frame_timeline_nsecs)
        } else {
            input.frame_timeline_nsecs
        };
        self.admitted_span_after_catch_up_nsecs = self
            .admitted_span_after_catch_up_nsecs
            .saturating_add(after.saturating_sub(before.max(barrier)));
        self.consecutive_zero_output_packets = 0;

        // Once the output gate has atomically committed this attempt, normal
        // VO resource pressure can stop decoder admission with the recovered
        // window exactly one frame short of its stability threshold.
        // mpv treats VO backpressure after accepted decoder output as healthy;
        // allow the equivalent single-frame boundary tolerance only after the
        // atomic commit, never while recovery output is still speculative.
        let stable_progress_window_nsecs = self.stable_progress_window_nsecs();
        let stable_progress_threshold_nsecs = if self.output_commit_observed {
            stable_progress_window_nsecs.saturating_sub(input.frame_duration_nsecs)
        } else {
            stable_progress_window_nsecs
        };
        if self.admitted_span_after_catch_up_nsecs >= stable_progress_threshold_nsecs {
            HevcAdmittedVideoProgress::Stable
        } else {
            HevcAdmittedVideoProgress::Partial
        }
    }

    fn observe_staged_video_progress(
        &mut self,
        generation: u64,
        staged_end_nsecs: u64,
        now: Instant,
    ) -> bool {
        if !self.observes_generation(generation)
            || self
                .last_staged_end_nsecs
                .is_some_and(|previous| staged_end_nsecs <= previous)
        {
            return false;
        }

        self.last_staged_progress_at = Some(now);
        self.last_staged_end_nsecs = Some(staged_end_nsecs);
        self.output_high_water_nsecs =
            max_optional_u64(self.output_high_water_nsecs, Some(staged_end_nsecs));
        self.consecutive_zero_output_packets = 0;
        // PacketDone can overtake main-thread frame admission. A few packets
        // after the returning IDR may therefore trip the frozen packet-lead
        // bound before that already-decoded frame reaches the output gate.
        // Match mpv's valid-frame reset, but preserve structural failures such
        // as an unbridged continuous timeline gap.
        self.clear_packet_derived_hard_failure();
        true
    }

    fn latest_output_progress_at(&self) -> Option<Instant> {
        match (self.last_staged_progress_at, self.last_admitted_progress_at) {
            (Some(staged), Some(admitted)) => Some(staged.max(admitted)),
            (Some(staged), None) => Some(staged),
            (None, Some(admitted)) => Some(admitted),
            (None, None) => None,
        }
    }

    fn idle_failure(&self, now: Instant) -> Option<&'static str> {
        if let Some(reason) = self.hard_failure {
            return Some(reason);
        }
        let last_output = self.latest_output_progress_at().unwrap_or(self.started_at);
        (now.saturating_duration_since(last_output) >= self.progress_timeout()).then_some(
            if self.kind == HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild {
                "cached rebuild had no staged or admitted video progress for five seconds"
            } else {
                "attempt had no staged or admitted video progress for one second"
            },
        )
    }

    fn has_recent_accepted_output_progress(&self, now: Instant) -> bool {
        self.latest_output_progress_at()
            .is_some_and(|last_progress_at| {
                now.saturating_duration_since(last_progress_at) < self.progress_timeout()
            })
    }

    fn packet_lead_nsecs(&self) -> Option<u64> {
        self.input_high_water_nsecs
            .zip(self.output_high_water_nsecs)
            .map(|(input, output)| input.saturating_sub(output))
    }
}

#[derive(Clone, Debug)]
struct HevcSameHardwareRecoveryAttemptRecord {
    attempt_id: u64,
    decoder_epoch: u64,
    kind: HevcSameHardwareRecoveryAttemptKind,
    outcome: &'static str,
    consecutive_zero_output_packets: u64,
    input_high_water_nsecs: Option<u64>,
    output_high_water_nsecs: Option<u64>,
    packet_lead_nsecs: Option<u64>,
    last_staged_end_nsecs: Option<u64>,
    first_admitted_nsecs: Option<u64>,
    last_admitted_end_nsecs: Option<u64>,
    admitted_span_after_catch_up_nsecs: u64,
    output_commit_observed: bool,
    replay_packets: usize,
    elapsed: Duration,
}

impl HevcSameHardwareRecoveryAttemptRecord {
    fn diagnostic(&self) -> String {
        format!(
            "attempt_id={} decoder_epoch={} kind={} outcome={} zero_output_packets={} input_high_water_nsecs={:?} output_high_water_nsecs={:?} packet_lead_nsecs={:?} last_staged_end_nsecs={:?} first_admitted_nsecs={:?} last_admitted_end_nsecs={:?} admitted_span_after_catch_up_nsecs={} output_commit_observed={} replay_packets={} elapsed_ms={:.3}",
            self.attempt_id,
            self.decoder_epoch,
            self.kind.as_str(),
            self.outcome,
            self.consecutive_zero_output_packets,
            self.input_high_water_nsecs,
            self.output_high_water_nsecs,
            self.packet_lead_nsecs,
            self.last_staged_end_nsecs,
            self.first_admitted_nsecs,
            self.last_admitted_end_nsecs,
            self.admitted_span_after_catch_up_nsecs,
            self.output_commit_observed,
            self.replay_packets,
            self.elapsed.as_secs_f64() * 1_000.0,
        )
    }
}

impl HevcSameHardwareRecoveryPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::DrainingResults => "draining_results",
            Self::Flushing => "flushing_same_hardware",
            Self::ReplayingAfterFlush => "replaying_after_flush",
            Self::Reopening => "reopening_same_hardware",
            Self::PrewarmingAfterReopen => "prewarming_after_reopen",
            Self::RebuildingFromCache => "rebuilding_from_cache",
            Self::ReplayingAfterReopen => "replaying_after_reopen",
            Self::Recovered => "recovered",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
struct HevcSameHardwareRecoveryTransaction {
    target_nsecs: u64,
    observed_target_nsecs: u64,
    reason: HevcDecodeChainFallbackReason,
    resource_pressure_triggered: bool,
    phase: HevcSameHardwareRecoveryPhase,
    started_at: Instant,
    last_progress_at: Instant,
    last_admitted_progress_at: Option<Instant>,
    drain_recorded: bool,
    flush_attempts: u8,
    reopen_attempts: u8,
    cached_rebuild_attempts: u8,
    replay_packets: usize,
    next_attempt_id: u64,
    active_attempt: Option<HevcSameHardwareRecoveryAttempt>,
    attempt_ledger: Vec<HevcSameHardwareRecoveryAttemptRecord>,
    root_zero_output_packets: u64,
    root_input_high_water_nsecs: Option<u64>,
    root_output_high_water_nsecs: Option<u64>,
    replay_required_high_water_nsecs: Option<u64>,
    last_result_produced_sequence: u64,
    prewarm_ticket: Option<VulkanPrewarmTicket>,
    original_error: String,
    last_error: Option<String>,
    last_logged_action: Option<HevcDecodeRecoveryAction>,
    last_action_log_at: Option<Instant>,
    suppressed_action_logs: u64,
    last_drain_log_at: Option<Instant>,
    suppressed_drain_logs: u64,
    resource_pressure_errors: u64,
    resource_pressure_release_epoch: Option<u64>,
    last_resource_pressure_log_at: Option<Instant>,
    suppressed_resource_pressure_errors: u64,
}

impl HevcSameHardwareRecoveryTransaction {
    fn new(
        fallback: HevcDecodeChainFallback,
        result_produced_sequence: u64,
        source_error: Option<String>,
        now: Instant,
    ) -> Self {
        let phase = if fallback.reason == HevcDecodeChainFallbackReason::ResourcePressure {
            HevcSameHardwareRecoveryPhase::Flushing
        } else {
            HevcSameHardwareRecoveryPhase::DrainingResults
        };
        Self {
            target_nsecs: fallback.target_nsecs,
            observed_target_nsecs: fallback.target_nsecs,
            reason: fallback.reason,
            resource_pressure_triggered: fallback.reason
                == HevcDecodeChainFallbackReason::ResourcePressure,
            phase,
            started_at: now,
            last_progress_at: now,
            last_admitted_progress_at: None,
            drain_recorded: false,
            flush_attempts: 0,
            reopen_attempts: 0,
            cached_rebuild_attempts: 0,
            replay_packets: 0,
            next_attempt_id: 1,
            active_attempt: None,
            attempt_ledger: Vec::new(),
            root_zero_output_packets: 0,
            root_input_high_water_nsecs: None,
            root_output_high_water_nsecs: None,
            replay_required_high_water_nsecs: Some(fallback.target_nsecs),
            last_result_produced_sequence: result_produced_sequence,
            prewarm_ticket: None,
            original_error: source_error.unwrap_or_else(|| {
                format!(
                    "{} requested bounded same-Vulkan recovery at {}ns",
                    fallback.reason.as_str(),
                    fallback.target_nsecs
                )
            }),
            last_error: None,
            last_logged_action: None,
            last_action_log_at: None,
            suppressed_action_logs: 0,
            last_drain_log_at: None,
            suppressed_drain_logs: 0,
            resource_pressure_errors: 0,
            resource_pressure_release_epoch: None,
            last_resource_pressure_log_at: None,
            suppressed_resource_pressure_errors: 0,
        }
    }

    fn resource_pressure(&self) -> bool {
        self.resource_pressure_triggered
    }

    fn resource_pressure_demux_admission_stopped(&self) -> bool {
        self.resource_pressure() && self.phase != HevcSameHardwareRecoveryPhase::Recovered
    }

    fn resource_pressure_decoder_input_stopped(&self) -> bool {
        self.resource_pressure()
            && matches!(
                self.phase,
                HevcSameHardwareRecoveryPhase::Flushing
                    | HevcSameHardwareRecoveryPhase::Reopening
                    | HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen
                    | HevcSameHardwareRecoveryPhase::RebuildingFromCache
                    | HevcSameHardwareRecoveryPhase::Failed
            )
    }

    fn promote_to_resource_pressure(
        &mut self,
        target_nsecs: u64,
        cutoff_nsecs: Option<u64>,
        error: &str,
        now: Instant,
    ) {
        if !self.resource_pressure_triggered {
            self.finish_active_attempt("preempted_by_resource_pressure", now);
            self.resource_pressure_triggered = true;
            self.reason = HevcDecodeChainFallbackReason::ResourcePressure;
            self.target_nsecs = target_nsecs;
            self.observed_target_nsecs = target_nsecs;
            self.root_zero_output_packets = 0;
            self.root_input_high_water_nsecs = cutoff_nsecs;
            self.root_output_high_water_nsecs = Some(target_nsecs);
            self.replay_required_high_water_nsecs = cutoff_nsecs.or(Some(target_nsecs));
            self.last_progress_at = now;
        }
        self.record_resource_pressure_error(error, cutoff_nsecs, now);
        self.last_error = Some(error.to_string());
    }

    fn record_resource_pressure_error(
        &mut self,
        error: &str,
        packet_nsecs: Option<u64>,
        now: Instant,
    ) {
        self.resource_pressure_errors = self.resource_pressure_errors.saturating_add(1);
        let first = self.resource_pressure_errors == 1;
        let summary_due = self.last_resource_pressure_log_at.is_some_and(|last| {
            now.saturating_duration_since(last) >= HEVC_SAME_HARDWARE_LOG_SUMMARY_INTERVAL
        });
        if first || summary_due {
            let suppressed = std::mem::take(&mut self.suppressed_resource_pressure_errors);
            self.last_resource_pressure_log_at = Some(now);
            tracing::warn!(
                %error,
                packet_nsecs,
                frozen_target_nsecs = self.target_nsecs,
                frozen_cutoff_nsecs = ?self.replay_required_high_water_nsecs,
                resource_pressure_errors = self.resource_pressure_errors,
                suppressed_resource_pressure_errors = suppressed,
                same_hw_recovery_phase = self.phase.as_str(),
                "Vulkan decode resource pressure routed into bounded recovery"
            );
        } else {
            self.suppressed_resource_pressure_errors =
                self.suppressed_resource_pressure_errors.saturating_add(1);
        }
    }

    fn claim_resource_pressure_external_release(&mut self, decoder_epoch: u64) -> bool {
        if !self.resource_pressure() || self.resource_pressure_release_epoch == Some(decoder_epoch)
        {
            return false;
        }
        self.resource_pressure_release_epoch = Some(decoder_epoch);
        true
    }

    fn expired(&self, now: Instant) -> bool {
        if now.saturating_duration_since(self.started_at)
            < HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME
        {
            return false;
        }

        // mpv clears its hardware failure streak whenever the decoder returns
        // a valid frame. Preserve tiny's absolute wall-time bound for switching,
        // draining, and packet-only replay, but let frames already accepted by
        // the recovery output gate reach its atomic commit on the next
        // coordinator pass. The attempt-specific idle, staging-budget, and
        // packet-lead bounds still terminate a decoder that actually stops.
        !self
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.has_recent_accepted_output_progress(now))
    }

    fn set_root_evidence(
        &mut self,
        zero_output_packets: u64,
        input_high_water_nsecs: Option<u64>,
        output_high_water_nsecs: Option<u64>,
    ) {
        self.root_zero_output_packets = zero_output_packets;
        self.root_input_high_water_nsecs = input_high_water_nsecs;
        self.root_output_high_water_nsecs = output_high_water_nsecs;
        self.replay_required_high_water_nsecs = input_high_water_nsecs.or(Some(self.target_nsecs));
    }

    fn observe_result_progress(&mut self, result_produced_sequence: u64, now: Instant) -> bool {
        if result_produced_sequence == self.last_result_produced_sequence {
            return false;
        }
        self.last_result_produced_sequence = result_produced_sequence;
        if !matches!(
            self.phase,
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
                | HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
        ) {
            self.last_progress_at = now;
        }
        true
    }

    fn fail(&mut self, error: impl Into<String>) {
        self.finish_active_attempt("failed", Instant::now());
        self.phase = HevcSameHardwareRecoveryPhase::Failed;
        self.last_error = Some(error.into());
    }

    fn begin_attempt(
        &mut self,
        decoder_epoch: u64,
        kind: HevcSameHardwareRecoveryAttemptKind,
        generation_floor: u64,
        now: Instant,
    ) -> u64 {
        self.finish_active_attempt("superseded", now);
        let attempt_id = self.next_attempt_id.max(1);
        self.next_attempt_id = attempt_id.saturating_add(1).max(1);
        self.active_attempt = Some(HevcSameHardwareRecoveryAttempt::new(
            attempt_id,
            decoder_epoch,
            kind,
            generation_floor,
            self.target_nsecs,
            now,
        ));
        self.last_progress_at = now;
        self.last_admitted_progress_at = None;
        attempt_id
    }

    fn finish_active_attempt(&mut self, outcome: &'static str, now: Instant) {
        let Some(attempt) = self.active_attempt.take() else {
            return;
        };
        self.replay_required_high_water_nsecs = max_optional_u64(
            self.replay_required_high_water_nsecs,
            attempt.input_high_water_nsecs,
        )
        .or(Some(self.target_nsecs));
        self.attempt_ledger
            .push(HevcSameHardwareRecoveryAttemptRecord {
                attempt_id: attempt.attempt_id,
                decoder_epoch: attempt.decoder_epoch,
                kind: attempt.kind,
                outcome,
                consecutive_zero_output_packets: attempt.consecutive_zero_output_packets,
                input_high_water_nsecs: attempt.input_high_water_nsecs,
                output_high_water_nsecs: attempt.output_high_water_nsecs,
                packet_lead_nsecs: attempt.packet_lead_nsecs(),
                last_staged_end_nsecs: attempt.last_staged_end_nsecs,
                first_admitted_nsecs: attempt.first_admitted_nsecs,
                last_admitted_end_nsecs: attempt.last_admitted_end_nsecs,
                admitted_span_after_catch_up_nsecs: attempt.admitted_span_after_catch_up_nsecs,
                output_commit_observed: attempt.output_commit_observed,
                replay_packets: attempt.replay_packets,
                elapsed: now.saturating_duration_since(attempt.started_at),
            });
    }

    fn active_attempt_id(&self) -> Option<u64> {
        self.active_attempt
            .as_ref()
            .map(|attempt| attempt.attempt_id)
    }

    fn active_decoder_epoch(&self) -> Option<u64> {
        self.active_attempt
            .as_ref()
            .map(|attempt| attempt.decoder_epoch)
    }

    fn observe_packet(&mut self, generation: u64, packet_nsecs: Option<u64>, decoded_frames: u64) {
        if let Some(attempt) = self.active_attempt.as_mut() {
            attempt.observe_packet(generation, packet_nsecs, decoded_frames);
        }
    }

    fn observe_admitted_video_progress(
        &mut self,
        observation: HevcAdmittedVideoProgressObservation,
        now: Instant,
    ) -> HevcAdmittedVideoProgress {
        // A decode-chain fallback is speculative until the pending decoder
        // results have been drained. Delayed HEVC output that extends the
        // scheduled queue continuously across the frozen recovery target is
        // authoritative progress: keep those frames and avoid a destructive
        // flush/reopen of a decoder that has already recovered.
        if self.phase == HevcSameHardwareRecoveryPhase::DrainingResults {
            let continuity_tolerance_nsecs =
                queued_video_continuity_gap_threshold_nsecs(observation.frame_duration_nsecs);
            let queue_extended = observation.after_queue_end_nsecs.is_some_and(|after| {
                observation
                    .before_queue_end_nsecs
                    .is_none_or(|before| after > before)
            });
            let continuously_extended = observation.before_queue_end_nsecs.is_none_or(|before| {
                observation.frame_timeline_nsecs
                    <= before.saturating_add(continuity_tolerance_nsecs)
            });
            let target_covered = observation
                .after_queue_end_nsecs
                .is_some_and(|after| after > self.target_nsecs)
                && observation.frame_timeline_nsecs
                    <= self.target_nsecs.saturating_add(continuity_tolerance_nsecs);
            if queue_extended && continuously_extended && target_covered {
                self.drain_recorded = true;
                self.last_progress_at = now;
                self.last_admitted_progress_at = Some(now);
                self.root_output_high_water_nsecs = max_optional_u64(
                    self.root_output_high_water_nsecs,
                    observation.after_queue_end_nsecs,
                );
                return HevcAdmittedVideoProgress::Stable;
            }
        }

        let progress = self
            .active_attempt
            .as_mut()
            .map(|attempt| attempt.observe_admitted_video_progress(observation, now))
            .unwrap_or(HevcAdmittedVideoProgress::None);
        if matches!(
            progress,
            HevcAdmittedVideoProgress::Partial | HevcAdmittedVideoProgress::Stable
        ) {
            self.last_progress_at = now;
            self.last_admitted_progress_at = Some(now);
        }
        progress
    }

    fn record_decoder_drain_pass(
        &mut self,
        made_progress: bool,
        decoder_work_pending: bool,
        now: Instant,
    ) -> bool {
        if self.phase != HevcSameHardwareRecoveryPhase::DrainingResults {
            return false;
        }
        self.drain_recorded = true;
        if made_progress {
            self.last_progress_at = now;
            return false;
        }
        // mpv's receive-first loop treats EAGAIN with no decoder-owned work as
        // an immediate request for more input. Once recovery has frozen input,
        // there is nothing that can arrive during the grace period, so flush
        // immediately. Preserve the grace only for genuinely in-flight decode
        // or frame-prepare work that can still extend the visible queue.
        if decoder_work_pending
            && now.saturating_duration_since(self.last_progress_at) < HEVC_SAME_HARDWARE_DRAIN_GRACE
        {
            return false;
        }
        self.phase = HevcSameHardwareRecoveryPhase::Flushing;
        true
    }

    fn mark_unbridged_continuous_gap(&mut self) {
        if let Some(attempt) = self.active_attempt.as_mut() {
            attempt.hard_failure = Some("unbridged continuous decode gap");
        }
    }

    fn terminal_action(&self, mode: HardwareDecodeMode) -> HevcDecodeRecoveryAction {
        if mode.allows_fallback() {
            HevcDecodeRecoveryAction::RequestSoftwareFallback
        } else {
            HevcDecodeRecoveryAction::FailExplicitly
        }
    }

    fn pending_action(&self, mode: HardwareDecodeMode) -> HevcDecodeRecoveryAction {
        match self.phase {
            HevcSameHardwareRecoveryPhase::DrainingResults => {
                HevcDecodeRecoveryAction::DrainPendingResults
            }
            HevcSameHardwareRecoveryPhase::Flushing => HevcDecodeRecoveryAction::FlushSameHardware,
            HevcSameHardwareRecoveryPhase::Reopening => {
                HevcDecodeRecoveryAction::ReopenSameHardware
            }
            HevcSameHardwareRecoveryPhase::RebuildingFromCache => {
                if mode.allows_fallback() {
                    HevcDecodeRecoveryAction::RequestSoftwareFallback
                } else {
                    HevcDecodeRecoveryAction::RebuildFromCachedSeek
                }
            }
            HevcSameHardwareRecoveryPhase::Failed => self.terminal_action(mode),
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
            | HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen
            | HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
            | HevcSameHardwareRecoveryPhase::Recovered => HevcDecodeRecoveryAction::None,
        }
    }

    fn advance_after_attempt_failure(
        &mut self,
        failure: &'static str,
        now: Instant,
        mode: HardwareDecodeMode,
    ) -> HevcDecodeRecoveryAction {
        match self.phase {
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush => {
                self.finish_active_attempt("escalated_to_reopen", now);
                self.last_error = Some(format!("same-decoder flush/replay failed: {failure}"));
                self.phase = HevcSameHardwareRecoveryPhase::Reopening;
                HevcDecodeRecoveryAction::ReopenSameHardware
            }
            HevcSameHardwareRecoveryPhase::ReplayingAfterReopen => {
                let cached_rebuild = self.active_attempt.as_ref().is_some_and(|attempt| {
                    attempt.kind == HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild
                });
                if cached_rebuild {
                    self.finish_active_attempt("exhausted", now);
                    self.fail(format!("cached safe-IDR rebuild failed: {failure}"));
                    self.terminal_action(mode)
                } else {
                    // Packet/byte coverage alone cannot prove that the replayed
                    // GOP is semantically decodable. A fresh Vulkan context can
                    // reproduce a short prefix and then hit the same missing
                    // references. Match mpv's bounded fallback progression:
                    // Auto moves to software, while ForceVulkan gets one final
                    // authoritative rebuild from the demux cache's safe IDR.
                    self.finish_active_attempt("escalated_to_cache_rebuild", now);
                    self.last_error = Some(format!("same-Vulkan reopen/replay failed: {failure}"));
                    self.phase = HevcSameHardwareRecoveryPhase::RebuildingFromCache;
                    self.pending_action(mode)
                }
            }
            _ => self.pending_action(mode),
        }
    }

    fn advance_after_repeated_failure_if_idle(
        &mut self,
        result_produced_sequence: u64,
        now: Instant,
        mode: HardwareDecodeMode,
    ) -> HevcDecodeRecoveryAction {
        self.observe_result_progress(result_produced_sequence, now);
        let Some(failure) = self
            .active_attempt
            .as_ref()
            .and_then(|attempt| attempt.idle_failure(now))
        else {
            return self.pending_action(mode);
        };
        self.advance_after_attempt_failure(failure, now, mode)
    }

    fn failed_attempt_needs_decoder_drain(
        &self,
        snapshot: VideoDecodeWorkerSnapshot,
        now: Instant,
    ) -> bool {
        let attempt_failed = self
            .active_attempt
            .as_ref()
            .and_then(|attempt| attempt.idle_failure(now))
            .is_some();
        if !attempt_failed {
            return false;
        }

        // `pending_input_packets` includes replay/demux packets still owned by
        // this wrapper. They have not entered AVCodecContext and are discarded
        // by the next flush/reopen, so waiting for them here deadlocks: failed
        // recovery stops decoder input precisely while this drain is active.
        // Match mpv's flush_all() ownership boundary and drain only work that
        // has actually been submitted to the decoder (or returned by it).
        hevc_decoder_drain_work_pending(snapshot)
    }

    fn record_replay(&mut self, replay_packets: usize, after_reopen: bool, now: Instant) {
        if replay_packets == 0 {
            let error = "safe HEVC replay journal does not cover the recovery target";
            if after_reopen {
                // Reopening the Vulkan context is not the final bounded option.
                // Keep the reopened worker and rebuild it from the demux cache's
                // preceding closed-GOP IDR/BLA before ForceVulkan can fail (or
                // Auto can fall through to software).
                self.finish_active_attempt("journal_incomplete", now);
                self.last_error = Some(error.to_string());
                self.phase = HevcSameHardwareRecoveryPhase::RebuildingFromCache;
            } else {
                self.finish_active_attempt("journal_incomplete", now);
                self.last_error = Some(error.to_string());
                self.phase = HevcSameHardwareRecoveryPhase::Reopening;
            }
            return;
        }
        self.replay_packets = self.replay_packets.saturating_add(replay_packets);
        if let Some(attempt) = self.active_attempt.as_mut() {
            attempt.replay_packets = attempt.replay_packets.saturating_add(replay_packets);
        }
        self.last_progress_at = now;
        self.phase = if after_reopen {
            HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
        } else {
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
        };
    }

    fn begin_cached_rebuild(
        &mut self,
        decoder_epoch: u64,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<(), String> {
        if self.phase != HevcSameHardwareRecoveryPhase::RebuildingFromCache {
            return Err(format!(
                "cached safe-IDR rebuild requested in phase {}",
                self.phase.as_str()
            ));
        }
        if self.cached_rebuild_attempts > 0 {
            self.fail("cached safe-IDR rebuild attempt limit reached");
            return Err("cached safe-IDR rebuild attempt limit reached".to_string());
        }
        self.cached_rebuild_attempts = self.cached_rebuild_attempts.saturating_add(1);
        self.begin_attempt(
            decoder_epoch,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            generation,
            now,
        );
        self.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        self.last_progress_at = now;
        Ok(())
    }

    fn should_log_action(&mut self, action: HevcDecodeRecoveryAction, now: Instant) -> Option<u64> {
        let changed = self.last_logged_action != Some(action);
        let summary_due = self.last_action_log_at.is_some_and(|last| {
            now.saturating_duration_since(last) >= HEVC_SAME_HARDWARE_LOG_SUMMARY_INTERVAL
        });
        if changed || self.last_action_log_at.is_none() || summary_due {
            let suppressed = std::mem::take(&mut self.suppressed_action_logs);
            self.last_logged_action = Some(action);
            self.last_action_log_at = Some(now);
            Some(suppressed)
        } else {
            self.suppressed_action_logs = self.suppressed_action_logs.saturating_add(1);
            None
        }
    }

    fn should_log_drain(&mut self, advanced: bool, now: Instant) -> Option<u64> {
        let summary_due = self.last_drain_log_at.is_none_or(|last| {
            now.saturating_duration_since(last) >= HEVC_SAME_HARDWARE_LOG_SUMMARY_INTERVAL
        });
        if advanced || summary_due {
            let suppressed = std::mem::take(&mut self.suppressed_drain_logs);
            self.last_drain_log_at = Some(now);
            Some(suppressed)
        } else {
            self.suppressed_drain_logs = self.suppressed_drain_logs.saturating_add(1);
            None
        }
    }

    fn terminal_error(&self, now: Instant, mode: HardwareDecodeMode) -> String {
        let elapsed = now.saturating_duration_since(self.started_at);
        let last_progress = now.saturating_duration_since(self.last_progress_at);
        let attempt_ledger = self
            .attempt_ledger
            .iter()
            .map(HevcSameHardwareRecoveryAttemptRecord::diagnostic)
            .collect::<Vec<_>>()
            .join(" | ");
        format!(
            "{:?} 同 Vulkan 硬解恢复失败：original_error={}; last_error={}; phase={}; flush_attempts={}; reopen_attempts={}; cached_rebuild_attempts={}; replay_packets={}; root_zero_output_packets={}; root_input_high_water_nsecs={:?}; root_output_high_water_nsecs={:?}; attempts=[{}]; elapsed_ms={:.3}; last_progress_ms={:.3}",
            mode,
            self.original_error,
            self.last_error
                .as_deref()
                .unwrap_or("no explicit low-level error"),
            self.phase.as_str(),
            self.flush_attempts,
            self.reopen_attempts,
            self.cached_rebuild_attempts,
            self.replay_packets,
            self.root_zero_output_packets,
            self.root_input_high_water_nsecs,
            self.root_output_high_water_nsecs,
            attempt_ledger,
            elapsed.as_secs_f64() * 1000.0,
            last_progress.as_secs_f64() * 1000.0,
        )
    }
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
    ResourcePressure,
    StartupInFlightStall,
    PtsGapAfterZeroOutput,
    RecoveryWaitRebuffer,
    PostFallbackRebufferUnderfill,
}

impl HevcDecodeChainFallbackReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ZeroOutputRebuffer => "hevc_decode_chain_zero_output_rebuffer",
            Self::ResourcePressure => "hevc_decode_chain_resource_pressure",
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
                | Self::ResourcePressure
                | Self::StartupInFlightStall
                | Self::RecoveryWaitRebuffer
                | Self::PostFallbackRebufferUnderfill
                | Self::PtsGapAfterZeroOutput
        )
    }

    pub(super) fn invalidated_by_video_progress(self) -> bool {
        matches!(
            self,
            Self::StartupInFlightStall
                | Self::RecoveryWaitRebuffer
                | Self::PostFallbackRebufferUnderfill
        )
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
pub(in crate::player::backend::ffmpeg) struct HevcLowLevelSeekLanding {
    pub(super) transaction_id: u64,
    pub(super) target_nsecs: u64,
    pub(super) seek_position_nsecs: u64,
    pub(super) anchor_nsecs: u64,
    pub(super) anchor_kind: VideoRecoveryPointKind,
    pub(super) range_id: Option<u64>,
    pub(super) anchor_packet_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HevcLowLevelSeekObservation {
    transaction_id: u64,
    target_nsecs: u64,
    seek_position_nsecs: u64,
    reason: &'static str,
    landing: Option<HevcLowLevelSeekLanding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcLowLevelRecoveryObservationAction {
    CraLanding {
        landing: HevcLowLevelSeekLanding,
        repeated: bool,
        reason: &'static str,
    },
    SafeLanding {
        landing: HevcLowLevelSeekLanding,
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcDecodeChainResetScope {
    Transient,
    RecoveryTransaction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HevcDecodeHealthState {
    #[default]
    Healthy,
    Suspected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HevcDecodePacketEvidenceScope {
    ExactSeek,
    DecodeRecovery,
    Playback,
}

fn hevc_decode_packet_evidence_scope(
    exact_seek_scoped: bool,
    output_decode_recovery_active: bool,
    same_hardware_recovery_active: bool,
    packet_decode_recovery_scoped: bool,
) -> HevcDecodePacketEvidenceScope {
    if exact_seek_scoped {
        HevcDecodePacketEvidenceScope::ExactSeek
    } else if output_decode_recovery_active
        || same_hardware_recovery_active
        || packet_decode_recovery_scoped
    {
        // A bounded-recovery PacketDone can arrive after the output transaction
        // has committed and the same-hardware transaction has reported recovery.
        // It still describes recovery input, not fresh playback evidence. This
        // includes journal replay and packets admitted by a cached rebuild.
        HevcDecodePacketEvidenceScope::DecodeRecovery
    } else {
        HevcDecodePacketEvidenceScope::Playback
    }
}

pub(super) struct HevcDecodePacketObservation<'a> {
    pub(super) generation: u64,
    pub(super) status: &'a VideoDecodePacketStatus,
    pub(super) packet: &'a AvPacket,
    pub(super) video_stream: StreamInfo,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) has_audio_output: bool,
    pub(super) synchronized_audio_timeline_gap_checked: bool,
    pub(super) synchronized_audio_timeline_gap: Option<AudioTimelineGapEvidence>,
    pub(super) fallback_target_nsecs: u64,
    pub(super) session_id: PlaybackSessionId,
    pub(super) recovery_scope: VideoDecodeRecoveryScope,
    pub(super) decode_recovery_active: bool,
    pub(super) packet_decode_recovery_scoped: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct HevcDecodedFrameGapObservation {
    pub(super) session_id: PlaybackSessionId,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) hardware_accelerated: bool,
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
    pub(super) decode_recovery_active: bool,
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
    pub(super) generation: u64,
    pub(super) frame_timeline_nsecs: u64,
    pub(super) frame_duration_nsecs: u64,
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
    pub(super) decode_recovery_active: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HevcDecodeChainStats {
    pub(super) recent_zero_output_packets: u64,
    pub(super) first_zero_output_packet_nsecs: Option<u64>,
    pub(super) last_decoded_video_end_nsecs: Option<u64>,
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
    recovery_kind: VideoRecoveryPointKind,
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
            recovery_kind: cache_read
                .map(|cache| cache.recovery_kind)
                .unwrap_or_else(|| packet_video_recovery_point_kind(packet, codec_id)),
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

pub(super) struct VideoPacketAdmissionContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) video_stream: StreamInfo,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) has_audio_output: bool,
    pub(super) skip_nonref_for_pressure: bool,
    pub(super) played_until_nsecs: Option<u64>,
    pub(super) video_clock: &'a super::TimestampMapper,
    pub(super) presentation: Option<crate::player::render_host::VideoPresentation>,
}

#[derive(Clone, Copy)]
pub(super) struct VideoPacketAdmissionPressure {
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) skip_nonref_for_pressure: bool,
    pub(super) played_until_nsecs: Option<u64>,
    pub(super) output_resource_pressure: bool,
    pub(super) presentation: Option<crate::player::render_host::VideoPresentation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum VideoDecodeRecoveryScope {
    #[default]
    SafeBoundary,
    ExactCachedSeek {
        transaction_id: u64,
        target_nsecs: u64,
    },
    ExactLowLevelSeek {
        transaction_id: u64,
        target_nsecs: u64,
        seek_position_nsecs: u64,
        actual_anchor_nsecs: u64,
        actual_anchor_kind: VideoRecoveryPointKind,
    },
}

impl VideoDecodeRecoveryScope {
    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::SafeBoundary => "safe_boundary",
            Self::ExactCachedSeek { .. } => "exact_cached_seek",
            Self::ExactLowLevelSeek { .. } => "exact_low_level_seek",
        }
    }

    pub(in crate::player::backend::ffmpeg) fn transaction_id(self) -> Option<u64> {
        match self {
            Self::SafeBoundary => None,
            Self::ExactCachedSeek { transaction_id, .. }
            | Self::ExactLowLevelSeek { transaction_id, .. } => Some(transaction_id),
        }
    }

    fn target_nsecs(self) -> Option<u64> {
        match self {
            Self::SafeBoundary => None,
            Self::ExactCachedSeek { target_nsecs, .. }
            | Self::ExactLowLevelSeek { target_nsecs, .. } => Some(target_nsecs),
        }
    }

    fn accepts_hevc_recovery_point(self) -> bool {
        !matches!(self, Self::SafeBoundary)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct ExactSeekPacketProgress {
    pub(in crate::player::backend::ffmpeg) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg) recovery_scope: VideoDecodeRecoveryScope,
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) packet_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) packet_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct ExactSeekCompletion {
    pub(in crate::player::backend::ffmpeg) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg) recovery_scope: VideoDecodeRecoveryScope,
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) first_eligible_frame_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) first_eligible_delta_nsecs: u64,
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct VideoDecodeRecovery {
    waiting_for_keyframe: bool,
    recovery_scope: VideoDecodeRecoveryScope,
    realign_on_next_frame: bool,
    realign_after_recovery_point: bool,
    skipped_packets: u64,
    first_skipped_packet_nsecs: Option<u64>,
    last_skipped_packet_nsecs: Option<u64>,
    seek_bootstrap_target_nsecs: Option<u64>,
    verified_replay_target_nsecs: Option<u64>,
    seek_bootstrap_preroll_frames: u64,
    seek_bootstrap_first_preroll_frame_nsecs: Option<u64>,
    seek_bootstrap_last_preroll_frame_nsecs: Option<u64>,
    exact_seek_packet_count: u64,
    exact_seek_last_packet_nsecs: Option<u64>,
    exact_seek_nonref_skip_completed: bool,
    completed_exact_seek: Option<ExactSeekCompletion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct SeekPrerollFrameProgress {
    pub(in crate::player::backend::ffmpeg) timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) preroll_frames: u64,
    pub(in crate::player::backend::ffmpeg) first_preroll_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) last_preroll_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) recovery_scope: VideoDecodeRecoveryScope,
}

impl VideoDecodeRecovery {
    pub(in crate::player::backend::ffmpeg) fn reset(&mut self) {
        self.completed_exact_seek = None;
        self.waiting_for_keyframe = false;
        self.recovery_scope = VideoDecodeRecoveryScope::SafeBoundary;
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

    pub(in crate::player::backend::ffmpeg) fn begin_verified_replay_from_safe_anchor(
        &mut self,
        codec_id: ffi::AVCodecID,
        target_nsecs: u64,
    ) {
        self.reset();
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC && target_nsecs > 0 {
            // Replay packets are injected directly into the decoder queue and do
            // not pass through normal demux admission. The journal has already
            // proved that its first packet is a safe IDR/BLA and that coverage is
            // contiguous through the required cutoff, so do not scan live input
            // for a future keyframe after the replay drains.
            self.begin_seek_bootstrap(target_nsecs);
            self.verified_replay_target_nsecs = Some(target_nsecs);
        }
    }

    pub(in crate::player::backend::ffmpeg) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(in crate::player::backend::ffmpeg) fn enable_hevc_cached_recovery_point(
        &mut self,
        transaction_id: u64,
        target_nsecs: u64,
    ) {
        if self.waiting_for_keyframe {
            self.recovery_scope = VideoDecodeRecoveryScope::ExactCachedSeek {
                transaction_id,
                target_nsecs,
            };
        }
    }

    pub(in crate::player::backend::ffmpeg) fn enable_hevc_low_level_recovery_point(
        &mut self,
        landing: HevcLowLevelSeekLanding,
    ) {
        if self.waiting_for_keyframe {
            self.recovery_scope = VideoDecodeRecoveryScope::ExactLowLevelSeek {
                transaction_id: landing.transaction_id,
                target_nsecs: landing.target_nsecs,
                seek_position_nsecs: landing.seek_position_nsecs,
                actual_anchor_nsecs: landing.anchor_nsecs,
                actual_anchor_kind: landing.anchor_kind,
            };
        }
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_scope(&self) -> VideoDecodeRecoveryScope {
        self.recovery_scope
    }

    pub(in crate::player::backend::ffmpeg) fn requires_exact_seek_output(&self) -> bool {
        self.verified_replay_target_nsecs.is_some()
            || !matches!(self.recovery_scope, VideoDecodeRecoveryScope::SafeBoundary)
    }

    pub(in crate::player::backend::ffmpeg) fn verified_replay_target_nsecs(&self) -> Option<u64> {
        self.verified_replay_target_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn frame_start_position_nsecs(
        &self,
        current_start_position_nsecs: u64,
    ) -> u64 {
        self.verified_replay_target_nsecs
            .map(|target_nsecs| target_nsecs.max(current_start_position_nsecs))
            .unwrap_or(current_start_position_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn should_skip_nonref_for_seek_preroll(
        &mut self,
        packet_nsecs: Option<u64>,
        bounded_decode_recovery_active: bool,
        hardware_accelerated: bool,
    ) -> bool {
        if hardware_accelerated || bounded_decode_recovery_active {
            // Match mpv's "very exact" seek path for tiny's precise Vulkan
            // seeks. AVDISCARD_NONREF is only a seek-speed optimization, while
            // the output gate remains the authoritative exact trim boundary.
            // With asynchronous HEVC frame threads, changing skip_frame during
            // cached-IDR preroll can poison the later reference chain even
            // after input crosses the target. Decode hardware preroll in full;
            // the measured Vulkan path is already substantially faster than
            // presentation time. Bounded decoder repair needs the same rule.
            return false;
        }
        // Packet PTS is allowed to move backwards around HEVC B-frames. Once
        // input has crossed the exact-seek boundary, never re-enable
        // AVDISCARD_NONREF for a later packet whose PTS regresses. mpv clears
        // its start-PTS framedrop state as soon as the first eligible decoded
        // frame is observed; the asynchronous tiny worker can otherwise run
        // hundreds of packets ahead of that observation and destroy the
        // reference chain until the next IDR.
        if self.exact_seek_nonref_skip_completed {
            return false;
        }
        let Some(target_nsecs) = self.recovery_scope.target_nsecs() else {
            return false;
        };
        let Some(packet_nsecs) = packet_nsecs else {
            // Broken or absent packet timestamps cannot safely drive precise
            // framedrop. Preserve frames and never re-enable skipping for the
            // remainder of this seek; decoded-frame trimming is authoritative.
            self.exact_seek_nonref_skip_completed = true;
            return false;
        };
        let skip_nonref =
            packet_nsecs < target_nsecs.saturating_sub(EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS);
        if !skip_nonref {
            self.exact_seek_nonref_skip_completed = true;
        }
        skip_nonref
    }

    pub(in crate::player::backend::ffmpeg) fn skipped_packets(&self) -> u64 {
        self.skipped_packets
    }

    pub(in crate::player::backend::ffmpeg) fn should_skip_packet(
        &self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe
            || self.packet_is_video_decode_recovery_point(packet, codec_id)
        {
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

    pub(in crate::player::backend::ffmpeg) fn observe_exact_seek_packet_progress(
        &mut self,
        packet_nsecs: Option<u64>,
    ) -> Option<ExactSeekPacketProgress> {
        let packet_nsecs = packet_nsecs?;
        let target_nsecs = self.recovery_scope.target_nsecs()?;
        if packet_nsecs >= target_nsecs {
            return None;
        }
        let transaction_id = self.recovery_scope.transaction_id()?;
        if self
            .exact_seek_last_packet_nsecs
            .is_some_and(|previous| packet_nsecs <= previous)
        {
            return None;
        }
        self.exact_seek_last_packet_nsecs = Some(packet_nsecs);
        self.exact_seek_packet_count = self.exact_seek_packet_count.saturating_add(1);
        Some(ExactSeekPacketProgress {
            transaction_id,
            recovery_scope: self.recovery_scope,
            target_nsecs,
            packet_nsecs,
            packet_count: self.exact_seek_packet_count,
        })
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
            recovery_scope: self.recovery_scope,
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
            recovery_scope: self.recovery_scope,
        };
        if let Some(transaction_id) = self.recovery_scope.transaction_id() {
            self.completed_exact_seek = Some(ExactSeekCompletion {
                transaction_id,
                recovery_scope: self.recovery_scope,
                target_nsecs,
                first_eligible_frame_nsecs: frame_timeline_nsecs,
                first_eligible_delta_nsecs: frame_timeline_nsecs.saturating_sub(target_nsecs),
            });
        }
        self.clear_seek_bootstrap();
        Some(progress)
    }

    pub(in crate::player::backend::ffmpeg) fn take_exact_seek_completion(
        &mut self,
    ) -> Option<ExactSeekCompletion> {
        self.completed_exact_seek.take()
    }

    pub(in crate::player::backend::ffmpeg) fn accept_recovery_point(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe
            || !self.packet_is_video_decode_recovery_point(packet, codec_id)
        {
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
        self.exact_seek_packet_count = 0;
        self.exact_seek_last_packet_nsecs = None;
        self.exact_seek_nonref_skip_completed = false;
        self.completed_exact_seek = None;
    }

    fn clear_seek_bootstrap(&mut self) {
        self.seek_bootstrap_target_nsecs = None;
        self.verified_replay_target_nsecs = None;
        self.seek_bootstrap_preroll_frames = 0;
        self.seek_bootstrap_first_preroll_frame_nsecs = None;
        self.seek_bootstrap_last_preroll_frame_nsecs = None;
        self.exact_seek_packet_count = 0;
        self.exact_seek_last_packet_nsecs = None;
        self.exact_seek_nonref_skip_completed = false;
        self.recovery_scope = VideoDecodeRecoveryScope::SafeBoundary;
    }

    fn can_accept_hevc_recovery_point_after_wait_limit(
        &self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && self.waiting_for_keyframe
            && self.hevc_recovery_wait_limit_exceeded()
            // A parser-only CRA without AV_PKT_FLAG_KEY is not a demuxer-
            // guaranteed random-access packet. Promoting one after a decoder
            // flush can submit its RASL/inter pictures with an empty DPB and
            // immediately rebuild another broken RPS chain. Keep waiting for
            // a keyed CRA or the normal safe IDR/BLA path instead.
            && packet.is_key()
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

    fn packet_is_video_decode_recovery_point(
        &self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return packet_is_video_recovery_point(packet, codec_id);
        }
        packet_is_video_seek_point(packet, codec_id)
            || (self.recovery_scope.accepts_hevc_recovery_point()
                && packet_is_video_recovery_point(packet, codec_id))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct HevcDecodeChainWatchdog {
    health_state: HevcDecodeHealthState,
    zero_output_packets: u64,
    first_zero_output_packet_nsecs: Option<u64>,
    zero_output_safe_anchor_nsecs: Option<u64>,
    zero_output_packets_after_safe_anchor: u64,
    last_video_packet_nsecs: Option<u64>,
    last_decoded_video_end_nsecs: Option<u64>,
    soft_recovery_attempted: bool,
    recent_zero_output_packets: u64,
    post_soft_recovery_skipped_packets: u64,
    recent_soft_recovery_attempted: bool,
    recent_packet_lead_exceeded: bool,
    recent_input_packet_high_water_nsecs: Option<u64>,
    recent_output_high_water_nsecs: Option<u64>,
    recent_zero_output_safe_anchor_nsecs: Option<u64>,
    recent_cache_discontinuity: bool,
    recent_audio_timeline_gap_checked: bool,
    recent_synchronized_audio_timeline_gap: Option<AudioTimelineGapEvidence>,
    healthy_admitted_progress_nsecs: u64,
    healthy_catch_up_barrier_nsecs: Option<u64>,
    pending_fallback: Option<HevcDecodeChainFallback>,
    post_fallback_rebuffer_underfill_started_at: Option<Instant>,
    first_zero_output_at: Option<Instant>,
    startup_in_flight_stall_started_at: Option<Instant>,
    startup_watchdog_retry_not_before: Option<Instant>,
    startup_watchdog_last_rejection_at: Option<Instant>,
    startup_watchdog_last_rejection_reason: Option<&'static str>,
    startup_watchdog_suppressed_rejections: u64,
    startup_waiting_for_input: bool,
    startup_watchdog_completed: bool,
    zero_output_log_suppressed: u64,
    last_video_progress_at: Option<Instant>,
    last_result_produced_sequence: u64,
    exact_seek_transaction_id: Option<u64>,
    completed_exact_seek_transaction_id: Option<u64>,
    completed_exact_seek_landing_nsecs: Option<u64>,
    exact_seek_zero_output_packets: u64,
    exact_seek_input_high_water_nsecs: Option<u64>,
}

#[derive(Default)]
struct HevcHwReplayJournal {
    packets: VecDeque<AvPacket>,
    packet_end_nsecs: VecDeque<Option<u64>>,
    total_bytes: usize,
    anchor_nsecs: Option<u64>,
    high_water_nsecs: Option<u64>,
    anchor_kind: Option<VideoRecoveryPointKind>,
    coverage_contiguous: bool,
    coverage_exhausted: bool,
}

fn hevc_packet_is_safe_replay_anchor(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    packet
        .read_diagnostic()
        .is_some_and(|diagnostic| diagnostic.safe_seek_point)
        || packet_is_video_seek_point(packet, codec_id)
}

fn hevc_safe_replay_anchor_kind(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
) -> VideoRecoveryPointKind {
    packet
        .read_diagnostic()
        .filter(|diagnostic| diagnostic.safe_seek_point)
        .map(|diagnostic| diagnostic.recovery_kind)
        .filter(|kind| kind.is_recovery_point())
        .unwrap_or_else(|| packet_video_recovery_point_kind(packet, codec_id))
}

fn hevc_replay_packet_start_nsecs(packet: &AvPacket, time_base: ffi::AVRational) -> Option<u64> {
    packet
        .read_diagnostic()
        .and_then(|diagnostic| diagnostic.packet_start_nsecs)
        .or_else(|| {
            packet
                .best_timestamp()
                .and_then(|timestamp| timestamp_to_nsecs(timestamp, time_base))
        })
}

fn hevc_safe_anchor_can_roll_past_preserved_evidence(
    safe_anchor: bool,
    anchor_nsecs: Option<u64>,
    decoded_output_end_nsecs: Option<u64>,
    recovery_cutoff_locked: bool,
) -> bool {
    !recovery_cutoff_locked
        && safe_anchor
        && anchor_nsecs
            .zip(decoded_output_end_nsecs)
            .is_some_and(|(anchor, output_end)| anchor <= output_end)
}

impl HevcHwReplayJournal {
    fn remember(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        time_base: ffi::AVRational,
    ) -> std::result::Result<bool, String> {
        self.remember_with_anchor_retention(packet, codec_id, time_base, false)
    }

    fn remember_preserving_safe_anchor(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        time_base: ffi::AVRational,
    ) -> std::result::Result<bool, String> {
        self.remember_with_anchor_retention(packet, codec_id, time_base, true)
    }

    fn remember_with_anchor_retention(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        time_base: ffi::AVRational,
        preserve_safe_anchor: bool,
    ) -> std::result::Result<bool, String> {
        if preserve_safe_anchor && self.coverage_exhausted {
            return Ok(false);
        }
        let extending_locked_anchor = preserve_safe_anchor && !self.packets.is_empty();
        // Cached packets already carry the demuxer's recovery-point verdict.
        // Prefer that immutable verdict after packet payload rewrites (for
        // example Dolby Vision RPU stripping), and retain bitstream inspection
        // for uncached input.
        let safe_anchor = hevc_packet_is_safe_replay_anchor(packet, codec_id);
        let replace_safe_anchor = safe_anchor && (!preserve_safe_anchor || self.packets.is_empty());
        if replace_safe_anchor {
            self.clear();
            self.anchor_kind = Some(hevc_safe_replay_anchor_kind(packet, codec_id));
            self.coverage_contiguous = true;
        } else if self.packets.is_empty() {
            return Ok(false);
        }

        if packet
            .read_diagnostic()
            .is_some_and(|diagnostic| diagnostic.sequence_contiguous == Some(false))
        {
            if extending_locked_anchor {
                // Freeze the completed contiguous prefix. A later cache
                // discontinuity must not poison recovery of a cutoff already
                // covered by this safe-IDR segment.
                self.coverage_exhausted = true;
                return Ok(false);
            }
            if !safe_anchor {
                self.clear();
                return Ok(false);
            }
        }

        let packet_nsecs = hevc_replay_packet_start_nsecs(packet, time_base);
        let packet_end_nsecs = packet_nsecs.map(|start| {
            let duration_nsecs = packet
                .duration()
                .and_then(|duration| timestamp_to_nsecs(duration, time_base))
                .unwrap_or_default();
            start.saturating_add(duration_nsecs)
        });
        if replace_safe_anchor {
            self.anchor_nsecs = packet_nsecs;
        }
        let candidate_high_water = max_optional_u64(self.high_water_nsecs, packet_end_nsecs);
        let exceeds_duration =
            self.anchor_nsecs
                .zip(candidate_high_water)
                .is_some_and(|(anchor, current)| {
                    current.saturating_sub(anchor) > HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS
                });
        let exceeds_packets = self.packets.len() >= HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS;
        let exceeds_bytes =
            self.total_bytes.saturating_add(packet.byte_len()) > HEVC_HW_REPLAY_JOURNAL_MAX_BYTES;
        if exceeds_duration || exceeds_packets || exceeds_bytes {
            if preserve_safe_anchor {
                // Keep the bounded prefix replayable for any previously frozen
                // cutoff it already covers.
                self.coverage_exhausted = true;
            } else {
                self.clear();
            }
            return Ok(false);
        }

        let packet_bytes = packet.byte_len();
        let packet = AvPacket::ref_from(packet)?;
        self.packets.push_back(packet);
        self.packet_end_nsecs.push_back(packet_end_nsecs);
        self.total_bytes = self.total_bytes.saturating_add(packet_bytes);
        self.high_water_nsecs = candidate_high_water;
        Ok(true)
    }

    #[cfg(test)]
    fn clone_complete(
        &self,
        target_nsecs: u64,
    ) -> std::result::Result<Option<VecDeque<AvPacket>>, String> {
        let covers_target =
            self.anchor_nsecs
                .zip(self.high_water_nsecs)
                .is_some_and(|(anchor, high_water)| {
                    anchor <= target_nsecs && high_water >= target_nsecs
                });
        if self.packets.is_empty()
            || self.anchor_kind.is_none()
            || !self.coverage_contiguous
            || !covers_target
        {
            return Ok(None);
        }
        self.packets
            .iter()
            .map(AvPacket::ref_from)
            .collect::<std::result::Result<VecDeque<_>, _>>()
            .map(Some)
    }

    fn clone_replayable(
        &self,
        target_nsecs: u64,
        required_high_water_nsecs: u64,
    ) -> std::result::Result<Option<VecDeque<AvPacket>>, String> {
        let covers_required_interval =
            self.anchor_nsecs
                .zip(self.high_water_nsecs)
                .is_some_and(|(anchor, high_water)| {
                    let anchor_after_target_nsecs = anchor.saturating_sub(target_nsecs);
                    // Recovery may freeze on the end of the last presented
                    // frame while the journal has already rolled to the next
                    // safe IDR. Like mpv's decoder fallback replay, prefer that
                    // viable decoder boundary when the output scheduler can
                    // bridge the bounded timestamp gap. Exact-seek coverage in
                    // clone_complete intentionally remains strict.
                    let anchor_reaches_recovery_target = anchor <= target_nsecs
                        || video_timestamp_gap_within_threshold(
                            anchor_after_target_nsecs,
                            HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS,
                        );
                    anchor_reaches_recovery_target && high_water >= required_high_water_nsecs
                });
        if self.packets.is_empty()
            || self.anchor_kind.is_none()
            || !self.coverage_contiguous
            || !covers_required_interval
        {
            return Ok(None);
        }
        let replay_packet_count = self.replay_packet_count(required_high_water_nsecs);
        self.packets
            .iter()
            .take(replay_packet_count)
            .map(AvPacket::ref_from)
            .collect::<std::result::Result<VecDeque<_>, _>>()
            .map(Some)
    }

    fn replay_packet_count(&self, required_high_water_nsecs: u64) -> usize {
        if self.packet_end_nsecs.len() != self.packets.len() {
            return self.packets.len();
        }
        let mut prefix_high_water_nsecs = None;
        for (index, packet_end_nsecs) in self.packet_end_nsecs.iter().copied().enumerate() {
            prefix_high_water_nsecs = max_optional_u64(prefix_high_water_nsecs, packet_end_nsecs);
            if prefix_high_water_nsecs
                .is_some_and(|high_water| high_water >= required_high_water_nsecs)
            {
                return index
                    .saturating_add(1)
                    .saturating_add(HEVC_HW_REPLAY_REORDER_TAIL_PACKETS)
                    .min(self.packets.len());
            }
        }
        self.packets.len()
    }

    fn clear(&mut self) {
        self.packets.clear();
        self.packet_end_nsecs.clear();
        self.total_bytes = 0;
        self.anchor_nsecs = None;
        self.high_water_nsecs = None;
        self.anchor_kind = None;
        self.coverage_contiguous = false;
        self.coverage_exhausted = false;
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

fn hevc_hw_replay_packets(
    packets: VecDeque<AvPacket>,
    playback_generation: &mut PlaybackGeneration,
) -> VecDeque<PendingVideoDecodePacket> {
    packets
        .into_iter()
        .map(|packet| PendingVideoDecodePacket {
            generation: playback_generation.advance(),
            packet,
            drop_policy: VideoDecodeDropPolicy::None,
            realign_after_decode_recovery: true,
            hevc_startup_in_flight_watchdog: false,
            from_hevc_hw_replay: true,
            hevc_decode_recovery_evidence_scoped: true,
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
        HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS.max(VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY)
    };
    (pending, capacity)
}

pub(super) fn hevc_drain_video_result_progressed(
    before: VideoDecodeWorkerSnapshot,
    after: VideoDecodeWorkerSnapshot,
) -> bool {
    before.result_produced_sequence != after.result_produced_sequence
        || before.result_consumed_sequence != after.result_consumed_sequence
}

pub(super) fn hevc_decoder_drain_work_pending(snapshot: VideoDecodeWorkerSnapshot) -> bool {
    snapshot.submitted_not_consumed_packets > 0
        || snapshot.completed_packets > 0
        || snapshot.queued_frames > 0
        || !matches!(
            snapshot.state,
            VideoDecodeWorkerState::NeedPacket | VideoDecodeWorkerState::Eof
        )
}

fn take_next_video_decode_input(
    packets: &mut VideoDecodePacketQueues,
    hevc_hw_replay: &mut VecDeque<PendingVideoDecodePacket>,
) -> Option<PendingVideoDecodePacket> {
    hevc_hw_replay
        .pop_front()
        .or_else(|| packets.take_pending_input())
}

fn requeue_backpressured_video_decode_input(
    packets: &mut VideoDecodePacketQueues,
    hevc_hw_replay: &mut VecDeque<PendingVideoDecodePacket>,
    packet: PendingVideoDecodePacket,
) {
    if packet.from_hevc_hw_replay {
        hevc_hw_replay.push_front(packet);
    } else {
        packets.push_pending_input_front(packet);
    }
}

#[derive(Clone, Copy, Debug)]
struct HevcDecodeChainWatchdogInput {
    session_id: PlaybackSessionId,
    packet_nsecs: Option<u64>,
    safe_seek_point: bool,
    decoded_frames: u64,
    decode_ok: bool,
    hardware_accelerated: bool,
    output_snapshot: PlaybackOutputSnapshot,
    demux_watermark: DemuxReaderWatermark,
    has_audio_output: bool,
    synchronized_audio_timeline_gap_checked: bool,
    synchronized_audio_timeline_gap: Option<AudioTimelineGapEvidence>,
    cache_sequence_contiguous: bool,
    fallback_target_nsecs: u64,
    now: Instant,
}

#[derive(Clone, Copy, Debug)]
struct HevcPostSoftRecoverySkippedPacketObservation {
    session_id: PlaybackSessionId,
    packet_nsecs: Option<u64>,
    cache_sequence_contiguous: bool,
    hardware_accelerated: bool,
    output_snapshot: PlaybackOutputSnapshot,
    demux_watermark: DemuxReaderWatermark,
    has_audio_output: bool,
    fallback_target_nsecs: u64,
}

impl HevcDecodeChainWatchdog {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn clear_zero_output_boundary_grace(&mut self) {
        self.zero_output_safe_anchor_nsecs = None;
        self.zero_output_packets_after_safe_anchor = 0;
    }

    fn defer_pending_zero_output_fallback_at_safe_anchor(&mut self) -> bool {
        if !self.pending_fallback.is_some_and(|fallback| {
            fallback.reason == HevcDecodeChainFallbackReason::ZeroOutputRebuffer
        }) {
            return false;
        }
        self.pending_fallback = None;
        true
    }

    fn reset_transient_after_progress(
        &mut self,
        before_output_high_water_nsecs: Option<u64>,
        after_output_high_water_nsecs: Option<u64>,
        now: Instant,
    ) -> HevcAdmittedVideoProgress {
        let has_recent_gap_evidence = self.has_recent_gap_evidence();
        self.recent_output_high_water_nsecs = max_optional_u64(
            self.recent_output_high_water_nsecs,
            after_output_high_water_nsecs,
        );
        if after_output_high_water_nsecs.is_none() {
            self.healthy_admitted_progress_nsecs = 0;
            self.healthy_catch_up_barrier_nsecs = None;
        }
        let catch_up_barrier_nsecs = after_output_high_water_nsecs.map(|after| {
            *self
                .healthy_catch_up_barrier_nsecs
                .get_or_insert_with(|| self.recent_input_packet_high_water_nsecs.unwrap_or(after))
        });
        let caught_up_progress_nsecs = catch_up_barrier_nsecs
            .zip(after_output_high_water_nsecs)
            .filter(|(barrier, after)| after >= barrier)
            .map(|(barrier, after)| {
                let before = before_output_high_water_nsecs.unwrap_or(after);
                after.saturating_sub(before.max(barrier))
            })
            .unwrap_or_default();
        self.healthy_admitted_progress_nsecs = if has_recent_gap_evidence {
            self.healthy_admitted_progress_nsecs
                .saturating_add(caught_up_progress_nsecs)
        } else {
            0
        };

        // Decoder output only proves that the worker is alive. Keep the recent
        // packet/input/output high-water evidence until admitted video has
        // caught the input high-water and remained contiguous for 500ms.
        self.zero_output_packets = 0;
        self.first_zero_output_packet_nsecs = None;
        self.clear_zero_output_boundary_grace();
        self.last_video_packet_nsecs = None;
        self.soft_recovery_attempted = false;
        self.post_fallback_rebuffer_underfill_started_at = None;
        self.first_zero_output_at = None;
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        self.zero_output_log_suppressed = 0;
        self.last_decoded_video_end_nsecs = max_optional_u64(
            self.last_decoded_video_end_nsecs,
            after_output_high_water_nsecs,
        );
        self.last_video_progress_at = Some(now);

        if !has_recent_gap_evidence {
            if self
                .pending_fallback
                .is_some_and(|fallback| fallback.reason.invalidated_by_video_progress())
            {
                self.pending_fallback = None;
            }
            return after_output_high_water_nsecs.map_or(HevcAdmittedVideoProgress::None, |_| {
                HevcAdmittedVideoProgress::Stable
            });
        }
        if self.healthy_admitted_progress_nsecs >= HEVC_RECENT_GAP_EVIDENCE_CLEAR_AFTER_NSECS {
            return HevcAdmittedVideoProgress::Stable;
        }
        after_output_high_water_nsecs.map_or(HevcAdmittedVideoProgress::None, |_| {
            HevcAdmittedVideoProgress::Partial
        })
    }

    fn has_recent_gap_evidence(&self) -> bool {
        self.recent_zero_output_packets > 0
            || self.post_soft_recovery_skipped_packets > 0
            || self.recent_soft_recovery_attempted
            || self.recent_packet_lead_exceeded
    }

    fn take_fallback(&mut self) -> Option<HevcDecodeChainFallback> {
        self.pending_fallback.take()
    }

    fn has_pending_fallback(&self) -> bool {
        self.pending_fallback.is_some()
    }

    fn pending_fallback(&self) -> Option<HevcDecodeChainFallback> {
        self.pending_fallback
    }

    fn stats(&self) -> HevcDecodeChainStats {
        HevcDecodeChainStats {
            recent_zero_output_packets: self.recent_zero_output_packets,
            first_zero_output_packet_nsecs: self.first_zero_output_packet_nsecs,
            last_decoded_video_end_nsecs: self.last_decoded_video_end_nsecs,
            pending_fallback_reason: self.pending_fallback.map(|fallback| fallback.reason),
        }
    }

    fn exact_seek_evidence_scope_active(&self) -> bool {
        self.exact_seek_transaction_id.is_some()
    }

    fn has_strong_decoded_frame_gap_evidence(
        &self,
        input: &HevcDecodedFrameGapObservation,
    ) -> bool {
        input.recovery_waiting
            || input.source_frame_diagnostic.corrupt
            || input.source_frame_diagnostic.decode_error_flags != 0
            || self.zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
            || self.strong_recent_high_water_evidence()
            || (self.soft_recovery_attempted && self.zero_output_packets > 0)
    }

    fn strong_recent_high_water_evidence(&self) -> bool {
        self.recent_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
            && self.recent_packet_lead_exceeded
            && !self.recent_cache_discontinuity
    }

    fn packet_input_is_continuous(input: HevcDecodeChainWatchdogInput) -> bool {
        let demux_underrun = input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || (input.has_audio_output && input.demux_watermark.audio_underrun);
        let forward_healthy = input
            .demux_watermark
            .video_forward_nsecs
            .or(input.demux_watermark.selected_min_forward_nsecs)
            .is_some_and(|forward| forward >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS);
        !demux_underrun && input.cache_sequence_contiguous && forward_healthy
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

    fn rebuffer_has_video_headroom(output: PlaybackOutputSnapshot) -> bool {
        output.rebuffering
            && !output.video_output_low_water
            && output.queued_video_range_nsecs.is_some()
            && output
                .queued_video_contiguous_forward_nsecs
                .or(output.queued_video_forward_nsecs)
                .is_some_and(|forward| {
                    forward >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
                })
    }

    fn decoded_frame_gap_output_is_stable(input: &HevcDecodedFrameGapObservation) -> bool {
        (!input.output_snapshot.rebuffering && !input.output_snapshot.video_output_low_water)
            || Self::rebuffer_has_video_headroom(input.output_snapshot)
    }

    fn decoded_frame_gap_has_demux_underrun(input: &HevcDecodedFrameGapObservation) -> bool {
        input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || input.demux_watermark.audio_underrun
    }

    fn decoded_frame_gap_demux_cache_is_continuous(input: &HevcDecodedFrameGapObservation) -> bool {
        !Self::decoded_frame_gap_has_demux_underrun(input)
            && !input.recent_cache_read_anomaly
            && input
                .demux_watermark
                .video_forward_nsecs
                .or(input.demux_watermark.selected_min_forward_nsecs)
                .is_some_and(|forward| forward >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS)
    }

    fn decoded_frame_gap_matches_synchronized_timeline_gap(
        input: &HevcDecodedFrameGapObservation,
    ) -> bool {
        let Some(audio_gap) = input.audio_timeline_gap else {
            return false;
        };
        let Some(previous_expected_next_nsecs) = input.previous_expected_next_nsecs else {
            return false;
        };
        audio_gap
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
    }

    fn clear_recent_gap_evidence(&mut self) {
        self.health_state = HevcDecodeHealthState::Healthy;
        self.clear_zero_output_boundary_grace();
        self.recent_zero_output_packets = 0;
        self.post_soft_recovery_skipped_packets = 0;
        self.recent_soft_recovery_attempted = false;
        self.recent_packet_lead_exceeded = false;
        self.recent_input_packet_high_water_nsecs = None;
        self.recent_output_high_water_nsecs = None;
        self.recent_zero_output_safe_anchor_nsecs = None;
        self.recent_cache_discontinuity = false;
        self.recent_audio_timeline_gap_checked = false;
        self.recent_synchronized_audio_timeline_gap = None;
        self.healthy_admitted_progress_nsecs = 0;
        self.healthy_catch_up_barrier_nsecs = None;
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

    fn observe_post_soft_recovery_skipped_packet(
        &mut self,
        observation: HevcPostSoftRecoverySkippedPacketObservation,
    ) {
        if !observation.hardware_accelerated
            || (!observation.output_snapshot.video_output_low_water
                && !observation.output_snapshot.rebuffering)
            || !self.recent_soft_recovery_attempted
            || self.pending_fallback.is_some()
            || self.recent_synchronized_audio_timeline_gap.is_some()
            || (observation.has_audio_output && !self.recent_audio_timeline_gap_checked)
        {
            return;
        }
        let demux_underrun = observation.demux_watermark.underrun
            || observation.demux_watermark.video_underrun
            || (observation.has_audio_output && observation.demux_watermark.audio_underrun);
        let forward_healthy = observation
            .demux_watermark
            .video_forward_nsecs
            .or(observation.demux_watermark.selected_min_forward_nsecs)
            .is_some_and(|forward| forward >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS);
        if demux_underrun || !forward_healthy {
            return;
        }

        self.post_soft_recovery_skipped_packets =
            self.post_soft_recovery_skipped_packets.saturating_add(1);
        self.recent_cache_discontinuity |= !observation.cache_sequence_contiguous;
        if let Some(packet_nsecs) = observation.packet_nsecs {
            self.last_video_packet_nsecs = Some(
                self.last_video_packet_nsecs
                    .unwrap_or_default()
                    .max(packet_nsecs),
            );
            self.recent_input_packet_high_water_nsecs = Some(
                self.recent_input_packet_high_water_nsecs
                    .unwrap_or_default()
                    .max(packet_nsecs),
            );
        }
        let packet_lead_nsecs = self
            .recent_input_packet_high_water_nsecs
            .zip(self.last_decoded_video_end_nsecs)
            .map(|(input, output)| input.saturating_sub(output));
        let total_no_output_packets = self
            .recent_zero_output_packets
            .saturating_add(self.post_soft_recovery_skipped_packets);
        let hard_limit_reached = total_no_output_packets
            >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
            || packet_lead_nsecs
                .is_some_and(|lead| lead >= HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS);
        if !hard_limit_reached
            || !self.strong_recent_high_water_evidence()
            || self.recent_cache_discontinuity
        {
            return;
        }

        let target_nsecs = self
            .last_decoded_video_end_nsecs
            .unwrap_or(observation.fallback_target_nsecs);
        let reason = HevcDecodeChainFallbackReason::ZeroOutputRebuffer;
        self.health_state = HevcDecodeHealthState::Suspected;
        self.pending_fallback = Some(HevcDecodeChainFallback {
            target_nsecs,
            reason,
        });
        tracing::warn!(
            session_id = ?observation.session_id,
            target_nsecs,
            recent_hevc_zero_output_packets = self.recent_zero_output_packets,
            post_soft_recovery_skipped_packets = self.post_soft_recovery_skipped_packets,
            total_no_output_packets,
            packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
            fallback_reason = reason.as_str(),
            "HEVC post-soft-recovery high-water requested bounded decoder recovery before IDR"
        );
    }

    fn observe_startup_stall(
        &mut self,
        input: HevcStartupStallObservation,
    ) -> HevcDecodeChainRecoveryAction {
        if self.startup_watchdog_completed {
            return HevcDecodeChainRecoveryAction::None;
        }
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
        if self.startup_watchdog_completed
            || self.pending_fallback.is_some()
            || self.startup_in_flight_stall_started_at.is_some()
        {
            return;
        }
        self.startup_in_flight_stall_started_at = Some(now);
        tracing::debug!(
            session_id = ?session_id,
            deadline_ms = HEVC_STARTUP_IN_FLIGHT_HARD_AFTER.as_secs_f64() * 1000.0,
            "armed HEVC startup in-flight decode watchdog"
        );
    }

    fn suspend_startup_watchdog_for_input_wait(&mut self) -> bool {
        let changed = !self.startup_waiting_for_input
            || self.first_zero_output_at.is_some()
            || self.startup_in_flight_stall_started_at.is_some();
        self.startup_waiting_for_input = true;
        self.first_zero_output_at = None;
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        changed
    }

    fn resume_startup_watchdog_after_packet_submission(&mut self, now: Instant) {
        if !self.startup_waiting_for_input {
            return;
        }
        self.startup_waiting_for_input = false;
        self.startup_watchdog_retry_not_before = None;
        if self.zero_output_packets > 0 {
            self.first_zero_output_at = Some(now);
        }
        self.last_video_progress_at = Some(now);
    }

    fn observe_startup_in_flight_stall(&mut self, input: HevcStartupStallObservation) {
        if input.video_decode_snapshot.result_produced_sequence
            != self.last_result_produced_sequence
        {
            self.last_result_produced_sequence =
                input.video_decode_snapshot.result_produced_sequence;
            self.startup_in_flight_stall_started_at = None;
            self.startup_watchdog_retry_not_before = None;
            return;
        }
        if !hevc_startup_in_flight_stall_context(input) {
            self.startup_in_flight_stall_started_at = None;
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
            video_decode_submitted_not_consumed_packets = input.video_decode_snapshot.submitted_not_consumed_packets,
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
            video_decode_submitted_not_consumed_packets = input.video_decode_snapshot.submitted_not_consumed_packets,
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
    ) -> HevcAdmittedVideoProgress {
        if input.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return HevcAdmittedVideoProgress::None;
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
            return HevcAdmittedVideoProgress::None;
        }
        let continuity_gap_threshold_nsecs =
            queued_video_continuity_gap_threshold_nsecs(input.frame_duration_nsecs);
        let contiguous_with_previous = input.before_queue_end_nsecs.is_some_and(|before| {
            input.frame_timeline_nsecs <= before.saturating_add(continuity_gap_threshold_nsecs)
        });
        if self.has_recent_gap_evidence() && !contiguous_with_previous {
            self.healthy_admitted_progress_nsecs = 0;
            self.healthy_catch_up_barrier_nsecs = None;
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
                contiguous_with_previous,
                continuity_gap_threshold_nsecs,
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
        let progress = self.reset_transient_after_progress(
            if contiguous_with_previous {
                input.before_queue_end_nsecs
            } else {
                None
            },
            input.after_queue_end_nsecs,
            Instant::now(),
        );
        // Match mpv's receive-first lavc loop: every valid frame confirms
        // decoder progress and clears the hardware failure streak. With frame
        // threading, packets after a safe IDR can first release delayed frames
        // from the damaged GOP before the clean keyframe itself. Keep draining
        // those frames; the packet and decoded-gap watchdogs remain responsible
        // for bounded recovery if the decoder never reaches usable output.
        progress
    }

    fn observe_submitted_safe_recovery_point(
        &mut self,
        session_id: PlaybackSessionId,
        packet_nsecs: Option<u64>,
        safe_seek_point: bool,
        hardware_accelerated: bool,
    ) {
        if !hardware_accelerated || !safe_seek_point || self.zero_output_packets == 0 {
            return;
        }
        let Some(packet_nsecs) = packet_nsecs else {
            return;
        };
        if self
            .last_decoded_video_end_nsecs
            .is_some_and(|output_end_nsecs| packet_nsecs < output_end_nsecs)
        {
            return;
        }

        // The decode worker must publish frames while lavc is receiving them,
        // before it can publish PacketDone for the packet. A clean IDR can
        // therefore first release delayed frames from the damaged GOP; their
        // admission clears the transient zero-output counter before the IDR's
        // PacketDone is observed. Remember the safe boundary as soon as the
        // packet is owned by the normal playback decode queue. The decoded-gap
        // bridge still requires a clean frame plus strong, continuous demux and
        // audio evidence, so submission alone never declares recovery success.
        if self.zero_output_safe_anchor_nsecs.is_some() {
            return;
        }
        self.zero_output_safe_anchor_nsecs = Some(packet_nsecs);
        self.recent_zero_output_safe_anchor_nsecs = Some(
            self.recent_zero_output_safe_anchor_nsecs
                .unwrap_or_default()
                .max(packet_nsecs),
        );
        self.zero_output_packets_after_safe_anchor = 0;
        let deferred_pending_fallback = self.defer_pending_zero_output_fallback_at_safe_anchor();
        tracing::debug!(
            session_id = ?session_id,
            safe_anchor_nsecs = packet_nsecs,
            zero_output_packets = self.zero_output_packets,
            recent_zero_output_packets = self.recent_zero_output_packets,
            last_decoded_video_end_nsecs = ?self.last_decoded_video_end_nsecs,
            deferred_pending_fallback,
            "armed HEVC clean recovery bridge at submitted safe packet"
        );
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
        self.reset_transient_after_progress(None, None, Instant::now());
    }

    fn observe_exact_seek_packet_progress(
        &mut self,
        session_id: PlaybackSessionId,
        progress: ExactSeekPacketProgress,
    ) {
        if progress.packet_count == 1 || progress.packet_count.is_multiple_of(60) {
            tracing::debug!(
                session_id = ?session_id,
                transaction_id = progress.transaction_id,
                recovery_scope = progress.recovery_scope.as_str(),
                target_nsecs = progress.target_nsecs,
                packet_nsecs = progress.packet_nsecs,
                packet_count = progress.packet_count,
                packet_before_target = true,
                watchdog_progress = true,
                "observed HEVC exact-seek preroll packet progress"
            );
        }
        self.reset_transient_after_progress(None, None, Instant::now());
    }

    fn observe_exact_seek_decoder_result(
        &mut self,
        recovery_scope: VideoDecodeRecoveryScope,
        packet_nsecs: Option<u64>,
        decoded_frames: u64,
        decode_ok: bool,
        now: Instant,
    ) -> bool {
        let Some(transaction_id) = recovery_scope.transaction_id() else {
            return false;
        };
        if self.exact_seek_transaction_id != Some(transaction_id) {
            self.exact_seek_transaction_id = Some(transaction_id);
            self.completed_exact_seek_transaction_id = None;
            self.completed_exact_seek_landing_nsecs = None;
            self.exact_seek_zero_output_packets = 0;
            self.exact_seek_input_high_water_nsecs = None;
        }

        // Exact seek preroll is its own evidence scope. PacketDone still proves
        // that the worker is alive, but zero-output/reordering before the target
        // must never mutate the playback-period root evidence.
        if decode_ok {
            self.startup_in_flight_stall_started_at = None;
            self.startup_watchdog_retry_not_before = None;
            self.last_video_progress_at = Some(now);
            self.exact_seek_input_high_water_nsecs =
                max_optional_u64(self.exact_seek_input_high_water_nsecs, packet_nsecs);
            if decoded_frames == 0 {
                self.exact_seek_zero_output_packets =
                    self.exact_seek_zero_output_packets.saturating_add(1);
            }
        }
        true
    }

    fn complete_exact_seek_evidence_scope(
        &mut self,
        transaction_id: u64,
        first_eligible_frame_nsecs: u64,
        preserve_playback_evidence: bool,
        promote_failed_seek_evidence: bool,
        now: Instant,
    ) {
        let seek_zero_output_packets = if self.exact_seek_transaction_id == Some(transaction_id) {
            self.exact_seek_zero_output_packets
        } else {
            0
        };
        let seek_input_high_water_nsecs = (self.exact_seek_transaction_id == Some(transaction_id))
            .then_some(self.exact_seek_input_high_water_nsecs)
            .flatten();
        let seek_packet_lead_nsecs = seek_input_high_water_nsecs
            .map(|high_water| high_water.saturating_sub(first_eligible_frame_nsecs));
        let promoted_playback_evidence = !preserve_playback_evidence
            && promote_failed_seek_evidence
            && seek_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
            && seek_packet_lead_nsecs
                .is_some_and(|lead| lead >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_PACKET_LEAD_NSECS);

        if !preserve_playback_evidence {
            // A user seek generation starts a new playback evidence epoch.
            // A seek used inside a decoder/output recovery is different: its
            // first eligible frame is still uncommitted staging, so it must not
            // clear or advance the playback-period root evidence.
            self.clear_recent_gap_evidence();
            self.zero_output_packets = 0;
            self.first_zero_output_packet_nsecs = None;
            self.clear_zero_output_boundary_grace();
            self.last_video_packet_nsecs = None;
            self.first_zero_output_at = None;
            self.soft_recovery_attempted = false;
            self.post_fallback_rebuffer_underfill_started_at = None;
            self.startup_in_flight_stall_started_at = None;
            self.startup_watchdog_retry_not_before = None;
            self.last_decoded_video_end_nsecs = Some(first_eligible_frame_nsecs);
            self.recent_output_high_water_nsecs = Some(first_eligible_frame_nsecs);
            self.last_video_progress_at = Some(now);
            if promoted_playback_evidence {
                // Preroll zero-output packets normally stay isolated from the
                // new playback epoch. If the asynchronous hardware decoder has
                // already consumed at least 500 ms beyond its first eligible
                // output, however, the missing interval is post-target decode
                // failure evidence. Preserve it so the following PTS gap takes
                // the bounded same-Vulkan/software recovery path instead of
                // becoming a multi-second held frame.
                self.health_state = HevcDecodeHealthState::Suspected;
                self.recent_zero_output_packets = seek_zero_output_packets;
                self.recent_packet_lead_exceeded = true;
                self.recent_input_packet_high_water_nsecs = seek_input_high_water_nsecs;
            }
        }
        self.exact_seek_transaction_id = None;
        self.completed_exact_seek_transaction_id = Some(transaction_id);
        self.completed_exact_seek_landing_nsecs = Some(first_eligible_frame_nsecs);
        self.exact_seek_zero_output_packets = 0;
        self.exact_seek_input_high_water_nsecs = None;
        tracing::debug!(
            transaction_id,
            first_eligible_frame_nsecs,
            seek_zero_output_packets,
            seek_input_high_water_nsecs,
            seek_packet_lead_ms = ?seek_packet_lead_nsecs
                .map(|lead| lead as f64 / 1_000_000.0),
            preserve_playback_evidence,
            promoted_playback_evidence,
            "closed HEVC exact-seek evidence scope at first eligible frame"
        );
    }

    fn suspend_playback_watchdogs_for_decode_recovery(&mut self) {
        // The output barrier intentionally freezes scheduled/admitted progress.
        // Preserve all recent packet/high-water evidence, but cancel watchdogs
        // whose clocks would otherwise interpret that freeze as a second fault.
        self.pending_fallback = None;
        self.post_fallback_rebuffer_underfill_started_at = None;
        self.first_zero_output_at = None;
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        self.startup_waiting_for_input = false;
    }

    fn observe_packet_during_decode_recovery(
        &mut self,
        decode_ok: bool,
        decoded_frames: u64,
        now: Instant,
    ) {
        if !decode_ok {
            return;
        }
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        if decoded_frames > 0 {
            self.last_video_progress_at = Some(now);
        }
    }

    fn observe_post_fallback_rebuffer_underfill(
        &mut self,
        input: HevcPostFallbackRebufferObservation,
    ) {
        if input.decode_recovery_active {
            self.suspend_playback_watchdogs_for_decode_recovery();
            return;
        }
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

    fn observe_replay_packet_progress(&mut self, now: Instant) {
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        self.last_video_progress_at = Some(now);
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

        if input.decode_recovery_active {
            if Self::decoded_frame_gap_matches_synchronized_timeline_gap(&input) {
                tracing::debug!(
                    session_id = ?input.session_id,
                    video_previous_end_nsecs = ?input.previous_expected_next_nsecs,
                    video_next_start_nsecs = input.timeline_nsecs,
                    video_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                    audio_timeline_gap = ?input.audio_timeline_gap,
                    "confirmed synchronized media gap without mutating playback watchdog during decode recovery"
                );
                return HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap;
            }
            return HevcDecodedFrameGapAction::Admit;
        }

        let clean_decoded_frame = !input.source_frame_diagnostic.corrupt
            && input.source_frame_diagnostic.decode_error_flags == 0;
        if !input.hardware_accelerated {
            let bridge_gap =
                clean_decoded_frame && Self::decoded_frame_gap_demux_is_healthy(&input, gap_nsecs);
            self.reset();
            if bridge_gap {
                tracing::debug!(
                    session_id = ?input.session_id,
                    codec = ?input.codec_id,
                    previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                    next_frame_nsecs = input.timeline_nsecs,
                    gap_ms = gap_nsecs as f64 / 1_000_000.0,
                    frame_key = input.source_frame_diagnostic.key_frame,
                    frame_corrupt = input.source_frame_diagnostic.corrupt,
                    frame_decode_error_flags = input.source_frame_diagnostic.decode_error_flags,
                    demux_selected_min_forward_ms = ?input
                        .demux_watermark
                        .selected_min_forward_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    "bridging clean software-decoded media timeline gap"
                );
                return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
            }
            tracing::debug!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                next_frame_nsecs = input.timeline_nsecs,
                gap_ms = gap_nsecs as f64 / 1_000_000.0,
                clean_decoded_frame,
                recent_cache_read_anomaly = input.recent_cache_read_anomaly,
                "admitting software-decoded timeline gap without hardware fallback"
            );
            return HevcDecodedFrameGapAction::Admit;
        }

        if Self::decoded_frame_gap_matches_synchronized_timeline_gap(&input) {
            let audio_gap = input
                .audio_timeline_gap
                .expect("synchronized audio gap helper requires evidence");
            let previous_expected_next_nsecs = input
                .previous_expected_next_nsecs
                .expect("synchronized audio gap helper requires prior video end");
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
        // mpv accounts for the PTS delta to the next decoded frame when it
        // schedules that frame.  Do the equivalent while the first frame is
        // still unpresented: leaving a bounded gap in the scheduled queue can
        // fill every Vulkan surface while the initial-start waterline remains
        // pinned to the isolated prefix forever.
        let bounded_initial_decode_gap = input.output_snapshot.first_video_frame_pending
            && !input.output_snapshot.first_frame_presented
            && decode_recovery_gap_within_limit(gap_nsecs, DECODE_RECOVERY_HOLD_GAP_MAX_NSECS)
            && clean_recovery_frame
            && !self.strong_recent_high_water_evidence()
            && Self::decoded_frame_gap_demux_is_healthy(&input, gap_nsecs);
        if bounded_initial_decode_gap {
            let pending_fallback_reason_before_bridge = self
                .pending_fallback
                .map(|fallback| fallback.reason.as_str());
            if !self.strong_recent_high_water_evidence() {
                self.reset();
            }
            tracing::warn!(
                session_id = ?input.session_id,
                codec = ?input.codec_id,
                pts = input.timeline_nsecs,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                hold_gap_limit_ms =
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                frame_key = input.source_frame_diagnostic.key_frame,
                frame_corrupt = input.source_frame_diagnostic.corrupt,
                frame_decode_error_flags = input.source_frame_diagnostic.decode_error_flags,
                demux_selected_min_forward_ms = ?input
                    .demux_watermark
                    .selected_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                pending_fallback_reason_before_bridge,
                "bridging bounded HEVC decode gap before first video presentation"
            );
            return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
        }
        // mpv admits every successfully decoded frame during ordinary
        // playback and accounts for a short missing interval through the PTS
        // delta; it does not require the returning frame to be a keyframe.
        // Stay more conservative than mpv by relaxing this only after the
        // observed zero-output run crossed a safe anchor. Keep the keyframe
        // requirement for unrelated gaps and for exact-seek/startup rebuilds.
        let clean_non_key_after_safe_anchor = input.output_snapshot.state
            == PlaybackOutputState::Playing
            && self.strong_recent_high_water_evidence()
            && self
                .recent_zero_output_safe_anchor_nsecs
                .zip(input.previous_expected_next_nsecs)
                .is_some_and(|(anchor_nsecs, previous_end_nsecs)| {
                    anchor_nsecs >= previous_end_nsecs
                });
        let recoverable_frame = clean_decoded_frame
            && (input.source_frame_diagnostic.key_frame || clean_non_key_after_safe_anchor);
        let recoverable_decode_gap =
            video_timestamp_gap_within_threshold(gap_nsecs, HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS)
                && recoverable_frame
                && Self::decoded_frame_gap_demux_is_healthy(&input, gap_nsecs);
        if recoverable_decode_gap {
            let pending_fallback_reason_before_bridge = self
                .pending_fallback
                .map(|fallback| fallback.reason.as_str());
            if !self.strong_recent_high_water_evidence() {
                self.reset();
            }
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
                recent_zero_output_safe_anchor_nsecs = ?self
                    .recent_zero_output_safe_anchor_nsecs,
                pending_fallback_reason_before_bridge,
                "admitting clean HEVC frame and bridging small decode gap"
            );
            return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
        }

        // A precise seek can move the output gate from its initial transaction
        // into Rebuffering before presenting a frame. If a damaged GOP leaves
        // a useful queued prefix just below the resume waterline, withholding
        // the clean IDR deadlocks startup because the queue cannot reach that
        // waterline. Bridge that bounded, unpresented startup gap.
        //
        // Do not use the same repair after playback has started. mpv's
        // demand-driven decoder does not retain a multi-second frame queue;
        // extending tiny's previous scheduled frame to the returning IDR only
        // turns a Vulkan reference-chain failure into a visible still frame.
        // Once continuous audio and demux plus a long zero-output run prove a
        // video-only failure, the steady-state path below must replay from the
        // cached safe recovery point (and reopen the decoder if replay fails).
        let buffered_startup = input.output_snapshot.state == PlaybackOutputState::Rebuffering
            && !input.output_snapshot.first_frame_presented
            && input.output_snapshot.queued_video_range_nsecs.is_some()
            && Self::rebuffer_has_video_headroom(input.output_snapshot);
        let bounded_clean_recovery_point_gap = buffered_startup
            && decode_recovery_gap_within_limit(gap_nsecs, DECODE_RECOVERY_HOLD_GAP_MAX_NSECS)
            && clean_recovery_frame
            && self.strong_recent_high_water_evidence()
            && self
                .recent_zero_output_safe_anchor_nsecs
                .zip(input.previous_expected_next_nsecs)
                .is_some_and(|(anchor_nsecs, previous_end_nsecs)| {
                    anchor_nsecs >= previous_end_nsecs
                })
            && self.recent_audio_timeline_gap_checked
            && self.recent_synchronized_audio_timeline_gap.is_none()
            && Self::decoded_frame_gap_demux_is_healthy(&input, gap_nsecs)
            && Self::decoded_frame_gap_demux_cache_is_continuous(&input);
        if bounded_clean_recovery_point_gap {
            let zero_output_packets = self.recent_zero_output_packets;
            let input_high_water_nsecs = self.recent_input_packet_high_water_nsecs;
            let output_high_water_nsecs = self.recent_output_high_water_nsecs;
            let cleared_pending_fallback = self
                .pending_fallback
                .map(|fallback| fallback.reason.as_str());
            self.reset();
            tracing::warn!(
                session_id = ?input.session_id,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                recovery_frame_nsecs = input.timeline_nsecs,
                decode_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                hold_gap_limit_ms =
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                zero_output_packets,
                input_high_water_nsecs,
                output_high_water_nsecs,
                output_state = ?input.output_snapshot.state,
                first_frame_presented = input.output_snapshot.first_frame_presented,
                queued_video_contiguous_forward_ms = ?input
                    .output_snapshot
                    .queued_video_contiguous_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                cleared_pending_fallback,
                "bridging bounded clean HEVC startup recovery-point gap without decoder reset"
            );
            return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
        }

        // mpv derives display duration from adjacent PTS values, but that only
        // describes a genuine media timeline gap. When audio and demux remain
        // continuous and the decoder has already consumed dozens of packets
        // without output, stretching the previous frame hides a hardware
        // decode failure as a multi-second freeze. Preserve only the small-gap
        // path above; strong video-only gaps fall through to bounded replay of
        // the last safe hardware packet journal.
        let output_stable = Self::decoded_frame_gap_output_is_stable(&input);
        if self.health_state == HevcDecodeHealthState::Suspected
            && self.strong_recent_high_water_evidence()
            && self.recent_synchronized_audio_timeline_gap.is_none()
            && Self::decoded_frame_gap_demux_cache_is_continuous(&input)
        {
            self.health_state = HevcDecodeHealthState::Suspected;
            let target_nsecs = input
                .previous_expected_next_nsecs
                .or(input.audio_played_timeline_nsecs)
                .unwrap_or(input.fallback_target_nsecs);
            let reason = HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput;
            self.pending_fallback = Some(HevcDecodeChainFallback {
                target_nsecs,
                reason,
            });
            tracing::warn!(
                session_id = ?input.session_id,
                target_nsecs,
                frame_timeline_nsecs = input.timeline_nsecs,
                previous_expected_next_nsecs = ?input.previous_expected_next_nsecs,
                previous_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                recent_input_packet_high_water_nsecs = ?self
                    .recent_input_packet_high_water_nsecs,
                recent_output_high_water_nsecs = ?self.recent_output_high_water_nsecs,
                fallback_reason = reason.as_str(),
                "HEVC high-water decode failure confirmed at decoded PTS gap"
            );
            return HevcDecodedFrameGapAction::DropForFallback;
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
        // Any PacketDone, including a zero-output result caused by normal codec
        // reordering or seek preroll, proves that the worker and decoder made
        // forward progress. A separate consecutive-zero-output policy may still
        // inspect the packet, but it must not inherit an in-flight stall deadline.
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        if input.decoded_frames > 0 {
            let recovered_zero_output_packets = self.zero_output_packets;
            // With one lavc frame thread a clean IDR can both consume the
            // damaged GOP boundary and return frames in the same decode call.
            // The zero-output branch below used to be the only place that
            // remembered a crossed safe anchor, so this successful form of
            // recovery was misclassified as a PTS-gap failure and the already
            // recovered decoder was flushed/reopened. Preserve the anchor
            // before clearing transient zero-output state.
            let resumed_at_safe_anchor = input.hardware_accelerated
                && input.safe_seek_point
                && recovered_zero_output_packets > 0
                && input.packet_nsecs.is_some_and(|packet_nsecs| {
                    self.last_decoded_video_end_nsecs
                        .is_none_or(|output_end_nsecs| packet_nsecs >= output_end_nsecs)
                });
            if resumed_at_safe_anchor && let Some(packet_nsecs) = input.packet_nsecs {
                self.recent_zero_output_safe_anchor_nsecs = Some(
                    self.recent_zero_output_safe_anchor_nsecs
                        .unwrap_or_default()
                        .max(packet_nsecs),
                );
                tracing::debug!(
                    session_id = ?input.session_id,
                    safe_anchor_nsecs = packet_nsecs,
                    recovered_zero_output_packets,
                    decoded_frames = input.decoded_frames,
                    last_decoded_video_end_nsecs = ?self.last_decoded_video_end_nsecs,
                    "HEVC decoder resumed output directly at a safe recovery point"
                );
            }
            let deferred_pending_fallback =
                input.safe_seek_point && self.defer_pending_zero_output_fallback_at_safe_anchor();
            let suppressed_zero_output_packets =
                std::mem::take(&mut self.zero_output_log_suppressed);
            self.last_video_progress_at = Some(input.now);
            self.zero_output_packets = 0;
            self.first_zero_output_packet_nsecs = None;
            self.clear_zero_output_boundary_grace();
            self.first_zero_output_at = None;
            self.soft_recovery_attempted = false;
            if recovered_zero_output_packets > 0 || suppressed_zero_output_packets > 0 {
                tracing::debug!(
                    session_id = ?input.session_id,
                    decoded_frames = input.decoded_frames,
                    recovered_zero_output_packets,
                    suppressed_zero_output_packets,
                    recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                    recent_input_packet_high_water_nsecs = ?self.recent_input_packet_high_water_nsecs,
                    recent_output_high_water_nsecs = ?self.recent_output_high_water_nsecs,
                    safe_seek_point = input.safe_seek_point,
                    deferred_pending_fallback,
                    soft_recovery_attempted = self.soft_recovery_attempted,
                    "HEVC decoder resumed output; preserving high-water evidence until healthy admission"
                );
            }
            return HevcDecodeChainRecoveryAction::None;
        }

        if let Some((_, end_nsecs)) = input.output_snapshot.queued_video_range_nsecs {
            self.last_decoded_video_end_nsecs = Some(
                self.last_decoded_video_end_nsecs
                    .unwrap_or_default()
                    .max(end_nsecs),
            );
            if input.hardware_accelerated {
                self.recent_output_high_water_nsecs = Some(
                    self.recent_output_high_water_nsecs
                        .unwrap_or_default()
                        .max(end_nsecs),
                );
            }
        }
        if self.zero_output_packets == 0 {
            self.first_zero_output_packet_nsecs = input.packet_nsecs;
            self.first_zero_output_at = Some(input.now);
            self.startup_watchdog_retry_not_before = None;
            self.clear_zero_output_boundary_grace();
        }
        self.healthy_admitted_progress_nsecs = 0;
        self.zero_output_packets = self.zero_output_packets.saturating_add(1);
        if input.hardware_accelerated {
            self.recent_zero_output_packets = self.recent_zero_output_packets.saturating_add(1);
        } else {
            self.clear_recent_gap_evidence();
        }
        if let Some(packet_nsecs) = input.packet_nsecs {
            self.last_video_packet_nsecs = Some(
                self.last_video_packet_nsecs
                    .unwrap_or_default()
                    .max(packet_nsecs),
            );
            if input.hardware_accelerated {
                self.recent_input_packet_high_water_nsecs = Some(
                    self.recent_input_packet_high_water_nsecs
                        .unwrap_or_default()
                        .max(packet_nsecs),
                );
            }
            if input.hardware_accelerated && input.safe_seek_point {
                let anchor_is_after_output = self
                    .last_decoded_video_end_nsecs
                    .is_none_or(|output_end_nsecs| packet_nsecs >= output_end_nsecs);
                if anchor_is_after_output && self.zero_output_safe_anchor_nsecs.is_none() {
                    self.zero_output_safe_anchor_nsecs = Some(packet_nsecs);
                    self.recent_zero_output_safe_anchor_nsecs = Some(
                        self.recent_zero_output_safe_anchor_nsecs
                            .unwrap_or_default()
                            .max(packet_nsecs),
                    );
                    self.zero_output_packets_after_safe_anchor = 0;
                    let deferred_pending_fallback =
                        self.defer_pending_zero_output_fallback_at_safe_anchor();
                    tracing::debug!(
                        session_id = ?input.session_id,
                        safe_anchor_nsecs = packet_nsecs,
                        last_decoded_video_end_nsecs = ?self.last_decoded_video_end_nsecs,
                        zero_output_packets = self.zero_output_packets,
                        deferred_pending_fallback,
                        "HEVC zero-output run reached a safe recovery point"
                    );
                } else if self
                    .zero_output_safe_anchor_nsecs
                    .is_some_and(|anchor_nsecs| packet_nsecs > anchor_nsecs)
                {
                    self.zero_output_packets_after_safe_anchor =
                        self.zero_output_packets_after_safe_anchor.saturating_add(1);
                }
            } else if self
                .zero_output_safe_anchor_nsecs
                .is_some_and(|anchor_nsecs| packet_nsecs > anchor_nsecs)
            {
                self.zero_output_packets_after_safe_anchor =
                    self.zero_output_packets_after_safe_anchor.saturating_add(1);
            }
        }
        if input.hardware_accelerated {
            self.recent_cache_discontinuity |= !input.cache_sequence_contiguous;
            self.recent_audio_timeline_gap_checked |= input.synchronized_audio_timeline_gap_checked;
        }
        if input.hardware_accelerated
            && let Some(audio_gap) = input.synchronized_audio_timeline_gap
        {
            self.recent_synchronized_audio_timeline_gap = Some(audio_gap);
            if self.pending_fallback.is_some_and(|fallback| {
                fallback.reason == HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            }) {
                self.pending_fallback = None;
            }
            self.health_state = HevcDecodeHealthState::Healthy;
        }

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
        if input.hardware_accelerated {
            self.recent_packet_lead_exceeded |= packet_lead_exceeded;
        }
        let demux_underrun = input.demux_watermark.underrun
            || input.demux_watermark.video_underrun
            || (input.has_audio_output && input.demux_watermark.audio_underrun);
        // Rebuffering is a consumer-side pause, not decoder failure evidence.
        // A post-seek A/V start can be parked a few milliseconds below its
        // one-second resume target while still retaining far more than the
        // low-water budget. Match mpv's decoder ownership boundary and only
        // make zero-output recovery urgent when the retained video is actually
        // near drain; otherwise keep receiving until a clean recovery frame.
        let rebuffer_has_video_headroom = Self::rebuffer_has_video_headroom(input.output_snapshot);
        let output_unstable = input.output_snapshot.video_output_low_water
            || (input.output_snapshot.rebuffering && !rebuffer_has_video_headroom);
        let startup_zero_output_context = hevc_startup_first_frame_zero_output_context(
            input.output_snapshot,
            input.demux_watermark,
            input.has_audio_output,
        );

        let log_zero_output_milestone = hevc_zero_output_log_milestone(self.zero_output_packets);
        if !log_zero_output_milestone {
            self.zero_output_log_suppressed = self.zero_output_log_suppressed.saturating_add(1);
        }
        if log_zero_output_milestone {
            let suppressed_zero_output_packets =
                std::mem::take(&mut self.zero_output_log_suppressed);
            tracing::debug!(
            session_id = ?input.session_id,
            hevc_zero_output_packets = self.zero_output_packets,
            suppressed_zero_output_packets,
            fallback_target_nsecs = input.fallback_target_nsecs,
            first_zero_output_packet_nsecs = ?self.first_zero_output_packet_nsecs,
            last_video_packet_pts = ?self.last_video_packet_nsecs,
            last_packet_to_fallback_target_ms = ?last_packet_to_fallback_target_ms,
            last_decoded_video_end = ?self.last_decoded_video_end_nsecs,
            packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
            output_state = ?input.output_snapshot.state,
            output_rebuffering = input.output_snapshot.rebuffering,
            output_video_low_water = input.output_snapshot.video_output_low_water,
            rebuffer_has_video_headroom,
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
            cache_sequence_contiguous = input.cache_sequence_contiguous,
            synchronized_audio_timeline_gap = ?input.synchronized_audio_timeline_gap,
            synchronized_audio_timeline_gap_checked =
                input.synchronized_audio_timeline_gap_checked,
            recent_audio_timeline_gap_checked = self.recent_audio_timeline_gap_checked,
            recent_synchronized_audio_timeline_gap = ?self
                .recent_synchronized_audio_timeline_gap,
            recent_hevc_zero_output_packets = self.recent_zero_output_packets,
            recent_input_packet_high_water_nsecs = ?self.recent_input_packet_high_water_nsecs,
            recent_output_high_water_nsecs = ?self.recent_output_high_water_nsecs,
            zero_output_safe_anchor_nsecs = ?self.zero_output_safe_anchor_nsecs,
            zero_output_packets_after_safe_anchor = self.zero_output_packets_after_safe_anchor,
            "observed HEVC decode packet with zero output frames"
            );
        }

        let strong_high_water_failure = input.hardware_accelerated
            && self.strong_recent_high_water_evidence()
            && packet_lead_exceeded
            && (!input.has_audio_output || self.recent_audio_timeline_gap_checked)
            && self.recent_synchronized_audio_timeline_gap.is_none()
            && Self::packet_input_is_continuous(input);
        if strong_high_water_failure {
            let entered_suspected = self.health_state != HevcDecodeHealthState::Suspected;
            self.health_state = HevcDecodeHealthState::Suspected;
            if entered_suspected {
                tracing::warn!(
                    session_id = ?input.session_id,
                    recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                    packet_lead_ms = ?packet_lead_nsecs
                        .map(|lead| lead as f64 / 1_000_000.0),
                    recent_input_packet_high_water_nsecs = ?self
                        .recent_input_packet_high_water_nsecs,
                    recent_output_high_water_nsecs = ?self.recent_output_high_water_nsecs,
                    zero_output_safe_anchor_nsecs = ?self.zero_output_safe_anchor_nsecs,
                    zero_output_packets_after_safe_anchor = self
                        .zero_output_packets_after_safe_anchor,
                    decode_health_state = "suspected",
                    "HEVC hardware decode entered suspected high-water state"
                );
            }
        }
        let hard_high_water_failure = self.recent_zero_output_packets
            >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
            || rebuffer_hard_packet_lead_exceeded;
        let safe_anchor_grace_exhausted = self.zero_output_safe_anchor_nsecs.is_some()
            && self.zero_output_packets_after_safe_anchor
                >= HEVC_DECODE_CHAIN_SAFE_ANCHOR_GRACE_PACKETS;
        let stable_output_hard_packet_lead_exceeded = self.zero_output_safe_anchor_nsecs.is_none()
            && packet_lead_nsecs
                .is_some_and(|lead| lead >= HEVC_DECODE_CHAIN_STABLE_OUTPUT_HARD_PACKET_LEAD_NSECS);
        let destructive_recovery_ready = output_unstable
            || safe_anchor_grace_exhausted
            || stable_output_hard_packet_lead_exceeded;
        if !startup_zero_output_context
            && strong_high_water_failure
            && hard_high_water_failure
            && destructive_recovery_ready
        {
            let reason = HevcDecodeChainFallbackReason::ZeroOutputRebuffer;
            let target_nsecs = self
                .last_decoded_video_end_nsecs
                .unwrap_or(input.fallback_target_nsecs);
            let requested_fallback = HevcDecodeChainFallback {
                target_nsecs,
                reason,
            };
            let preserve_pts_gap_fallback = self.pending_fallback.is_some_and(|fallback| {
                fallback.reason == HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
            });
            let fallback_changed =
                !preserve_pts_gap_fallback && self.pending_fallback != Some(requested_fallback);
            if !preserve_pts_gap_fallback {
                self.pending_fallback = Some(requested_fallback);
            }
            if fallback_changed {
                tracing::warn!(
                    session_id = ?input.session_id,
                    target_nsecs,
                    recent_hevc_zero_output_packets = self.recent_zero_output_packets,
                    packet_lead_ms = ?packet_lead_nsecs.map(|lead| lead as f64 / 1_000_000.0),
                    recent_input_packet_high_water_nsecs = ?self
                        .recent_input_packet_high_water_nsecs,
                    recent_output_high_water_nsecs = ?self.recent_output_high_water_nsecs,
                    output_unstable,
                    output_rebuffering = input.output_snapshot.rebuffering,
                    rebuffer_has_video_headroom,
                    zero_output_safe_anchor_nsecs = ?self.zero_output_safe_anchor_nsecs,
                    zero_output_packets_after_safe_anchor = self
                        .zero_output_packets_after_safe_anchor,
                    safe_anchor_grace_exhausted,
                    stable_output_hard_packet_lead_exceeded,
                    fallback_reason = reason.as_str(),
                    "HEVC high-water decode failure requested bounded decoder recovery"
                );
            }
            return HevcDecodeChainRecoveryAction::None;
        }

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

        if !input.hardware_accelerated
            && !self.soft_recovery_attempted
            && (self.zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
                || packet_lead_exceeded)
        {
            self.soft_recovery_attempted = true;
            self.recent_soft_recovery_attempted = true;
            self.zero_output_packets = 0;
            self.first_zero_output_packet_nsecs = None;
            self.clear_zero_output_boundary_grace();
            self.zero_output_log_suppressed = 0;
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
        if self.startup_watchdog_completed
            || !hardware_accelerated
            || self.startup_waiting_for_input
        {
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

    fn complete_startup_watchdog_after_first_frame(&mut self) {
        self.startup_watchdog_completed = true;
        self.first_zero_output_at = None;
        self.startup_in_flight_stall_started_at = None;
        self.startup_watchdog_retry_not_before = None;
        self.startup_watchdog_last_rejection_at = None;
        self.startup_watchdog_last_rejection_reason = None;
        self.startup_watchdog_suppressed_rejections = 0;
        self.startup_waiting_for_input = false;
    }
}

fn hevc_zero_output_log_milestone(packet_count: u64) -> bool {
    packet_count == HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
        || packet_count == HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
        || packet_count.is_power_of_two()
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

fn max_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
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
    if input.demux_watermark.underrun
        || input.demux_watermark.video_underrun
        || (input.has_audio_output && input.demux_watermark.audio_underrun)
    {
        return false;
    }
    if input.video_decode_snapshot.result_produced_sequence
        != input.video_decode_snapshot.result_consumed_sequence
    {
        return false;
    }
    let target_neighborhood_reached = input.fallback_target_nsecs == 0
        || input
            .video_decode_snapshot
            .oldest_submitted_packet_nsecs
            .is_some_and(|packet_nsecs| {
                packet_nsecs.saturating_add(HEVC_STARTUP_STALL_TARGET_PROXIMITY_NSECS)
                    >= input.fallback_target_nsecs
            });
    target_neighborhood_reached
        && input.video_decode_snapshot.submitted_not_consumed_packets > 0
        && input.video_decode_snapshot.completed_packets == 0
        && input.video_decode_snapshot.queued_frames == 0
        && input.output_snapshot.queued_video_frames == 0
}

pub(super) struct VideoDecodePipeline {
    worker: VideoDecodeWorker,
    frame_drop: VideoDecodeFrameDrop,
    requested_hardware_mode: HardwareDecodeMode,
    decoder_epoch: u64,
    admitted_video_sequence: u64,
    last_admitted_decoder_epoch: Option<u64>,
    packets: VideoDecodePacketQueues,
    hevc_hw_replay: VecDeque<PendingVideoDecodePacket>,
    hevc_decode_chain_watchdog: HevcDecodeChainWatchdog,
    hevc_decode_packet_diagnostics: HevcDecodePacketDiagnosticWindow,
    hevc_hw_replay_journal: HevcHwReplayJournal,
    hevc_same_hardware_recovery: Option<HevcSameHardwareRecoveryTransaction>,
    last_hevc_decode_error: Option<String>,
    last_hevc_decode_chain_fallback: Option<HevcDecodeChainFallbackRecord>,
    hevc_low_level_seek_observation: Option<HevcLowLevelSeekObservation>,
    last_hevc_cra_low_level_landing: Option<HevcLowLevelSeekLanding>,
}

#[path = "video_decode_pipeline/admission.rs"]
mod admission;
#[path = "video_decode_pipeline/core.rs"]
mod core;
#[path = "video_decode_pipeline/queue.rs"]
mod queue;
#[path = "video_decode_pipeline/same_hardware.rs"]
mod same_hardware;
#[path = "video_decode_pipeline/seek_observation.rs"]
mod seek_observation;
#[path = "video_decode_pipeline/watchdog.rs"]
mod watchdog;
#[path = "video_decode_pipeline/worker.rs"]
mod worker;

fn runtime_hevc_software_fallback_allowed(mode: HardwareDecodeMode) -> bool {
    mode.allows_fallback()
}

fn hevc_same_hardware_reopen_mode() -> HardwareDecodeMode {
    HardwareDecodeMode::ForceVulkan
}

fn hevc_startup_in_flight_packet_should_arm(
    codec_id: ffi::AVCodecID,
    hardware_accelerated: bool,
    packet_nsecs: Option<u64>,
    target_nsecs: u64,
) -> bool {
    codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && hardware_accelerated
        && (target_nsecs == 0
            || packet_nsecs.is_some_and(|packet_nsecs| {
                packet_nsecs.saturating_add(HEVC_STARTUP_STALL_TARGET_PROXIMITY_NSECS)
                    >= target_nsecs
            }))
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

fn hevc_cra_low_level_landing_repeats(
    previous: HevcLowLevelSeekLanding,
    next: HevcLowLevelSeekLanding,
) -> bool {
    previous.anchor_kind == VideoRecoveryPointKind::Cra
        && next.anchor_kind == VideoRecoveryPointKind::Cra
        && previous.target_nsecs == next.target_nsecs
        && previous.seek_position_nsecs == next.seek_position_nsecs
        && previous.anchor_nsecs == next.anchor_nsecs
}

fn hevc_low_level_seek_would_repeat_cra(
    previous: Option<HevcLowLevelSeekLanding>,
    target_nsecs: u64,
    seek_position_nsecs: u64,
) -> bool {
    previous.is_some_and(|landing| {
        landing.anchor_kind == VideoRecoveryPointKind::Cra
            && landing.target_nsecs == target_nsecs
            && landing.seek_position_nsecs == seek_position_nsecs
    })
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
    stream: StreamInfo,
    log_context: HevcDecodePacketLogContext,
) -> std::result::Result<DoviDecodePacketRewrite, String> {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC || !stream_has_dovi_config(stream) {
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
        StrippedHevcDoviDecodeAction::PassthroughMetadataOnly => {
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
}

impl DoviDecodePacketRewrite {
    fn metadata(&self) -> Option<&DoviFrameMetadata> {
        match self {
            Self::UseOriginal { metadata } | Self::Decode { metadata, .. } => metadata.as_ref(),
        }
    }

    fn decode_packet<'a>(&'a self, original: &'a AvPacket) -> &'a AvPacket {
        match self {
            Self::Decode { packet, .. } => packet,
            Self::UseOriginal { .. } => original,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrippedHevcDoviDecodeAction {
    DecodeStripped,
    PassthroughMetadataOnly,
}

fn hevc_dovi_decode_action_for_inspection(
    inspection: &DoviRpuNalInspection,
) -> StrippedHevcDoviDecodeAction {
    if inspection.kept_nal_count > 0 {
        return StrippedHevcDoviDecodeAction::DecodeStripped;
    }

    // mpv feeds metadata-only packets through lavc as well. Keeping the
    // original packet is also essential when an unusual length-prefixed HEVC
    // packet was only *misidentified* as RPU-only by the metadata parser: a
    // discarded reference packet can suppress the rest of the GOP.
    StrippedHevcDoviDecodeAction::PassthroughMetadataOnly
}

fn dovi_decode_packet_action_name(
    stripped_action: StrippedHevcDoviDecodeAction,
    stripped_decode_rewrite_enabled: bool,
) -> &'static str {
    match (stripped_action, stripped_decode_rewrite_enabled) {
        (StrippedHevcDoviDecodeAction::PassthroughMetadataOnly, _) => "passthrough_metadata_only",
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
    let recovery_kind = packet_video_recovery_point_kind(packet, codec_id);
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
        recovery_kind = recovery_kind.as_str(),
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

fn video_decode_error_requires_hevc_resource_pressure_recovery(
    error: &str,
    codec_id: ffi::AVCodecID,
    hardware_accelerated: bool,
) -> bool {
    codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && hardware_accelerated
        && video_decode_error_is_resource_pressure(error)
}

#[cfg(test)]
#[path = "video_decode_pipeline/tests.rs"]
mod tests;
