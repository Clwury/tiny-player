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
use super::output_gate::DECODE_RECOVERY_HOLD_GAP_MAX_NSECS;
use super::scheduled_video_queue::{
    VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS, queued_video_continuity_gap_threshold_nsecs,
    video_timestamp_gap_within_threshold,
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
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE,
    VideoRecoveryPointKind, duration_nsecs, packet_is_video_recovery_point,
    packet_is_video_seek_point, packet_video_recovery_point_kind, timestamp_to_nsecs,
};

const VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY: usize = 8;
pub(super) const HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT: u64 = 24;
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
const HEVC_STARTUP_STALL_TARGET_PROXIMITY_NSECS: u64 = 500_000_000;
const HEVC_STARTUP_WATCHDOG_RETRY_AFTER: Duration = Duration::from_millis(25);
const HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
// The failure trace needs the safe 677.866s IDR to remain available through
// the 690.633s reopen cutoff (12.767s). Keep a little time margin while the
// packet and byte limits below continue to make the journal strictly bounded.
const HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS: u64 = 15_000_000_000;
const HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS: usize = 1_024;
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
            lead >= if cached_safe_idr_rebuild {
                HEVC_SAME_HARDWARE_CACHED_REBUILD_MAX_PACKET_LEAD_NSECS
            } else {
                HEVC_DECODE_CHAIN_REBUFFER_HARD_PACKET_LEAD_NSECS
            }
        }) {
            self.hard_failure = Some(if cached_safe_idr_rebuild {
                "cached rebuild packet lead reached five seconds"
            } else {
                "attempt packet lead reached one second"
            });
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

    fn idle_failure(&self, now: Instant) -> Option<&'static str> {
        if let Some(reason) = self.hard_failure {
            return Some(reason);
        }
        let last_admitted = self.last_admitted_progress_at.unwrap_or(self.started_at);
        (now.saturating_duration_since(last_admitted) >= self.progress_timeout()).then_some(
            if self.kind == HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild {
                "cached rebuild had no admitted video progress for five seconds"
            } else {
                "attempt had no admitted video progress for one second"
            },
        )
    }

    fn has_recent_committed_output_progress(&self, now: Instant) -> bool {
        self.output_commit_observed
            && self
                .last_admitted_progress_at
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
            "attempt_id={} decoder_epoch={} kind={} outcome={} zero_output_packets={} input_high_water_nsecs={:?} output_high_water_nsecs={:?} packet_lead_nsecs={:?} first_admitted_nsecs={:?} last_admitted_end_nsecs={:?} admitted_span_after_catch_up_nsecs={} output_commit_observed={} replay_packets={} elapsed_ms={:.3}",
            self.attempt_id,
            self.decoder_epoch,
            self.kind.as_str(),
            self.outcome,
            self.consecutive_zero_output_packets,
            self.input_high_water_nsecs,
            self.output_high_water_nsecs,
            self.packet_lead_nsecs,
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
        // draining, and speculative replay, but do not turn healthy, atomically
        // committed output into a fatal error merely because presentation is
        // paced in real time. The attempt-specific idle and packet-lead bounds
        // still terminate a decoder that actually stops progressing.
        !self
            .active_attempt
            .as_ref()
            .is_some_and(|attempt| attempt.has_recent_committed_output_progress(now))
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
        snapshot.submitted_not_consumed_packets > 0
            || snapshot.completed_packets > 0
            || snapshot.queued_frames > 0
            || !matches!(
                snapshot.state,
                VideoDecodeWorkerState::NeedPacket | VideoDecodeWorkerState::Eof
            )
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

pub(super) struct VideoPacketAdmissionContext {
    pub(super) session_id: PlaybackSessionId,
    pub(super) video_stream: StreamInfo,
    pub(super) output_snapshot: PlaybackOutputSnapshot,
    pub(super) demux_watermark: DemuxReaderWatermark,
    pub(super) has_audio_output: bool,
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
        !matches!(self.recovery_scope, VideoDecodeRecoveryScope::SafeBoundary)
    }

    pub(in crate::player::backend::ffmpeg) fn should_skip_nonref_for_seek_preroll(
        &mut self,
        packet_nsecs: Option<u64>,
        bounded_decode_recovery_active: bool,
    ) -> bool {
        if bounded_decode_recovery_active {
            // Match mpv's "very exact" seek behavior for internal decoder
            // repair. HEVC packet PTS can move backwards around B-frames;
            // toggling AVDISCARD_NONREF on that reordered timeline can leave a
            // threaded hardware decoder without the reference chain needed to
            // land the repaired seek. Decode the bounded cached-IDR preroll in
            // full and trim only decoded frames at the output boundary.
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
    last_video_packet_nsecs: Option<u64>,
    last_decoded_video_end_nsecs: Option<u64>,
    soft_recovery_attempted: bool,
    recent_zero_output_packets: u64,
    post_soft_recovery_skipped_packets: u64,
    recent_soft_recovery_attempted: bool,
    recent_packet_lead_exceeded: bool,
    recent_input_packet_high_water_nsecs: Option<u64>,
    recent_output_high_water_nsecs: Option<u64>,
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

        self.packets.push_back(AvPacket::ref_from(packet)?);
        self.total_bytes = self.total_bytes.saturating_add(packet.byte_len());
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
        self.packets
            .iter()
            .map(AvPacket::ref_from)
            .collect::<std::result::Result<VecDeque<_>, _>>()
            .map(Some)
    }

    fn clear(&mut self) {
        self.packets.clear();
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

    fn decoded_frame_gap_output_is_stable(input: &HevcDecodedFrameGapObservation) -> bool {
        !input.output_snapshot.rebuffering && !input.output_snapshot.video_output_low_water
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
        self.recent_zero_output_packets = 0;
        self.post_soft_recovery_skipped_packets = 0;
        self.recent_soft_recovery_attempted = false;
        self.recent_packet_lead_exceeded = false;
        self.recent_input_packet_high_water_nsecs = None;
        self.recent_output_high_water_nsecs = None;
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
        self.reset_transient_after_progress(
            if contiguous_with_previous {
                input.before_queue_end_nsecs
            } else {
                None
            },
            input.after_queue_end_nsecs,
            Instant::now(),
        )
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
            && gap_nsecs <= DECODE_RECOVERY_HOLD_GAP_MAX_NSECS
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
        let recoverable_decode_gap =
            video_timestamp_gap_within_threshold(gap_nsecs, HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS)
                && clean_recovery_frame
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
                pending_fallback_reason_before_bridge,
                "admitting clean HEVC recovery keyframe and bridging small decode gap"
            );
            return HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap;
        }

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
            let suppressed_zero_output_packets =
                std::mem::take(&mut self.zero_output_log_suppressed);
            self.last_video_progress_at = Some(input.now);
            self.zero_output_packets = 0;
            self.first_zero_output_packet_nsecs = None;
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
        let output_unstable =
            input.output_snapshot.video_output_low_water || input.output_snapshot.rebuffering;
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
                    decode_health_state = "suspected",
                    "HEVC hardware decode entered suspected high-water state"
                );
            }
        }
        let hard_high_water_failure = self.recent_zero_output_packets
            >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT
            || rebuffer_hard_packet_lead_exceeded;
        if !startup_zero_output_context && strong_high_water_failure && hard_high_water_failure {
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

impl VideoDecodePipeline {
    pub(super) fn spawn(decoder: Decoder) -> std::result::Result<Self, String> {
        let requested_hardware_mode = decoder.hardware_decode_mode();
        Ok(Self {
            worker: VideoDecodeWorker::spawn(decoder)?,
            requested_hardware_mode,
            decoder_epoch: 1,
            admitted_video_sequence: 0,
            last_admitted_decoder_epoch: None,
            packets: VideoDecodePacketQueues::default(),
            hevc_hw_replay: VecDeque::new(),
            hevc_decode_chain_watchdog: HevcDecodeChainWatchdog::default(),
            hevc_decode_packet_diagnostics: HevcDecodePacketDiagnosticWindow::default(),
            hevc_hw_replay_journal: HevcHwReplayJournal::default(),
            hevc_same_hardware_recovery: None,
            last_hevc_decode_error: None,
            last_hevc_decode_chain_fallback: None,
            hevc_low_level_seek_observation: None,
            last_hevc_cra_low_level_landing: None,
        })
    }

    pub(super) fn info(&self) -> &VideoDecodeWorkerInfo {
        self.worker.info()
    }

    pub(super) fn decoder_epoch(&self) -> u64 {
        self.decoder_epoch
    }

    pub(super) fn admitted_video_sequence(&self) -> u64 {
        self.admitted_video_sequence
    }

    pub(super) fn last_admitted_decoder_epoch(&self) -> Option<u64> {
        self.last_admitted_decoder_epoch
    }

    pub(super) fn snapshot(&self) -> VideoDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        let (pending_input_packets, pending_input_capacity) = video_decode_pending_input_snapshot(
            self.packets.pending_input_count(),
            self.hevc_hw_replay.len(),
        );
        snapshot.pending_input_packets = pending_input_packets;
        snapshot.pending_input_capacity = pending_input_capacity;
        snapshot.oldest_submitted_packet_nsecs = self.packets.front_packet().and_then(|packet| {
            packet
                .read_diagnostic()
                .and_then(|diagnostic| diagnostic.packet_start_nsecs)
                .or_else(|| {
                    packet
                        .best_timestamp()
                        .and_then(|timestamp| timestamp_to_nsecs(timestamp, self.info().time_base))
                })
        });
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
                && snapshot.submitted_not_consumed_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::DecoderOutputPending)
            }
            _ if snapshot.submitted_not_consumed_packets >= snapshot.command_queue_capacity => {
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
        if self.packets.has_pending_input() || !self.hevc_hw_replay.is_empty() {
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
                requeue_backpressured_video_decode_input(
                    &mut self.packets,
                    &mut self.hevc_hw_replay,
                    pending_packet,
                );
                self.log_pending_input_backpressured(session_id, enqueue_result);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    pub(super) fn requeue_hevc_hw_replay_journal(
        &mut self,
        playback_generation: &mut PlaybackGeneration,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        let required_high_water_nsecs = self
            .hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.replay_required_high_water_nsecs)
            .unwrap_or(target_nsecs)
            .max(target_nsecs);
        let journal_anchor_nsecs = self.hevc_hw_replay_journal.anchor_nsecs;
        let journal_high_water_nsecs = self.hevc_hw_replay_journal.high_water_nsecs;
        let journal_anchor_after_target_nsecs = journal_anchor_nsecs
            .filter(|anchor_nsecs| *anchor_nsecs > target_nsecs)
            .map(|anchor_nsecs| anchor_nsecs.saturating_sub(target_nsecs));
        let journal_packets = self.hevc_hw_replay_journal.len();
        let journal_bytes = self.hevc_hw_replay_journal.total_bytes;
        let Some(packets) = self
            .hevc_hw_replay_journal
            .clone_replayable(target_nsecs, required_high_water_nsecs)?
        else {
            tracing::warn!(
                session_id = ?session_id,
                target_nsecs,
                required_high_water_nsecs,
                journal_anchor_nsecs = ?journal_anchor_nsecs,
                journal_anchor_after_target_ms = ?journal_anchor_after_target_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                recoverable_forward_anchor_limit_ms =
                    HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS as f64 / 1_000_000.0,
                journal_high_water_nsecs = ?journal_high_water_nsecs,
                journal_packets,
                journal_bytes,
                journal_packet_limit = HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
                journal_byte_limit = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
                journal_duration_limit_ms =
                    HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS as f64 / 1_000_000.0,
                journal_contiguous = self.hevc_hw_replay_journal.coverage_contiguous,
                journal_exhausted = self.hevc_hw_replay_journal.coverage_exhausted,
                "safe HEVC replay journal did not cover the complete recovery cutoff"
            );
            return Ok(0);
        };
        let replay = hevc_hw_replay_packets(packets, playback_generation);
        let requeued = replay.len();
        self.hevc_hw_replay.extend(replay);
        if requeued > 0 {
            tracing::debug!(
                session_id = ?session_id,
                target_nsecs,
                required_high_water_nsecs,
                journal_anchor_nsecs = ?journal_anchor_nsecs,
                journal_anchor_after_target_ms = ?journal_anchor_after_target_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                journal_high_water_nsecs = ?journal_high_water_nsecs,
                requeued,
                replay_pending = self.hevc_hw_replay.len(),
                "requeued safe HEVC hardware replay journal after hardware decode fallback"
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
                    video_decode_submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
                    video_decode_state = ?snapshot.state,
                    "buffered FFmpeg video packet in decoder wrapper input queue"
                );
                DecodePacketAdmissionStatus::Queued
            }
            Err(pending_packet) => {
                self.packets.push_pending_input_back(pending_packet);
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
            video_decode_submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
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
        if let Some(observation) =
            self.observe_hevc_low_level_recovery_packet(packet, packet_nsecs, codec_id)
        {
            match observation {
                HevcLowLevelRecoveryObservationAction::CraLanding {
                    landing,
                    repeated,
                    reason,
                } => {
                    recovery.enable_hevc_low_level_recovery_point(landing);
                    tracing::warn!(
                        session_id = ?context.session_id,
                        transaction_id = landing.transaction_id,
                        recovery_scope = recovery.recovery_scope().as_str(),
                        reason,
                        target_nsecs = landing.target_nsecs,
                        seek_position_nsecs = landing.seek_position_nsecs,
                        actual_anchor_nsecs = landing.anchor_nsecs,
                        preroll_debt_nsecs =
                            landing.target_nsecs.saturating_sub(landing.anchor_nsecs),
                        actual_recovery_kind = landing.anchor_kind.as_str(),
                        range_id = ?landing.range_id,
                        anchor_packet_id = ?landing.anchor_packet_id,
                        repeated_low_level_landing = repeated,
                        repeat_low_level_seek_suppressed = repeated,
                        awaiting_closed_cached_interval = false,
                        arbitration_outcome = "decode_from_actual_landing",
                        "accepted CRA as the exact low-level seek decode anchor"
                    );
                }
                HevcLowLevelRecoveryObservationAction::SafeLanding { landing, reason } => {
                    recovery.enable_hevc_low_level_recovery_point(landing);
                    tracing::debug!(
                        session_id = ?context.session_id,
                        transaction_id = landing.transaction_id,
                        recovery_scope = recovery.recovery_scope().as_str(),
                        reason,
                        target_nsecs = landing.target_nsecs,
                        seek_position_nsecs = landing.seek_position_nsecs,
                        actual_anchor_nsecs = landing.anchor_nsecs,
                        preroll_debt_nsecs =
                            landing.target_nsecs.saturating_sub(landing.anchor_nsecs),
                        actual_recovery_kind = landing.anchor_kind.as_str(),
                        range_id = ?landing.range_id,
                        anchor_packet_id = ?landing.anchor_packet_id,
                        awaiting_closed_cached_interval = false,
                        arbitration_outcome = "decode_from_actual_landing",
                        "observed safe recovery point after low-level seek"
                    );
                }
            }
        }
        if let Some(progress) = recovery.observe_exact_seek_packet_progress(packet_nsecs) {
            self.hevc_decode_chain_watchdog
                .observe_exact_seek_packet_progress(context.session_id, progress);
        }
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
            let fallback_target_nsecs = context
                .output_snapshot
                .video_output_rebuffer_anchor
                .map(|anchor| anchor.timeline_nsecs)
                .or(context.played_until_nsecs)
                .or(packet_nsecs)
                .unwrap_or_default();
            if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
                && self.hevc_same_hardware_recovery.is_none()
            {
                self.hevc_decode_chain_watchdog
                    .observe_post_soft_recovery_skipped_packet(
                        HevcPostSoftRecoverySkippedPacketObservation {
                            session_id: context.session_id,
                            packet_nsecs,
                            cache_sequence_contiguous: packet
                                .read_diagnostic()
                                .and_then(|diagnostic| diagnostic.sequence_contiguous)
                                .unwrap_or(true),
                            hardware_accelerated,
                            output_snapshot: context.output_snapshot,
                            demux_watermark: context.demux_watermark,
                            has_audio_output: context.has_audio_output,
                            fallback_target_nsecs,
                        },
                    );
            }
            if skipped_packets == 1 || skipped_packets.is_multiple_of(60) {
                tracing::debug!(
                    pts = ?packet.best_timestamp(),
                    packet_nsecs = ?packet_nsecs,
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
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
                recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
                safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                recovery_scope = recovery.recovery_scope().as_str(),
                exact_seek_output = recovery.requires_exact_seek_output(),
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
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
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
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
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

        let bounded_decode_recovery_active = self.hevc_same_hardware_recovery.is_some();
        let skip_nonref_for_exact_seek = recovery
            .should_skip_nonref_for_seek_preroll(packet_nsecs, bounded_decode_recovery_active);
        let skip_nonref = context.skip_nonref_for_pressure || skip_nonref_for_exact_seek;
        if skip_nonref != *skip_nonref_active {
            self.set_skip_nonref_frames(skip_nonref)?;
            *skip_nonref_active = skip_nonref;
            tracing::debug!(
                session_id = ?context.session_id,
                transaction_id = ?recovery.recovery_scope().transaction_id(),
                recovery_scope = recovery.recovery_scope().as_str(),
                skip_nonref,
                skip_nonref_for_pressure = context.skip_nonref_for_pressure,
                skip_nonref_for_exact_seek,
                bounded_decode_recovery_active,
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
                "updated FFmpeg video decoder non-reference frame skipping"
            );
        }

        let generation = playback_generation.advance();
        let decode_packet = dovi_packet_rewrite.decode_packet(packet);
        let hardware_accelerated = self.info().hardware_accelerated;
        let startup_target_nsecs = context
            .output_snapshot
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .or(context.played_until_nsecs)
            .unwrap_or_default();
        self.remember_hevc_hw_replay_packet(decode_packet, codec_id, context.session_id);
        let pending_packet = PendingVideoDecodePacket {
            generation,
            packet: AvPacket::ref_from(decode_packet)?,
            realign_after_decode_recovery: context.output_snapshot.first_video_frame_pending,
            hevc_startup_in_flight_watchdog: hevc_startup_in_flight_packet_should_arm(
                codec_id,
                hardware_accelerated,
                packet_nsecs,
                startup_target_nsecs,
            ),
            from_hevc_hw_replay: false,
            hevc_decode_recovery_evidence_scoped: bounded_decode_recovery_active,
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn recover_error_if_needed(
        &mut self,
        result: std::result::Result<(), String>,
        playback_generation: &mut PlaybackGeneration,
        codec_id: ffi::AVCodecID,
        packet: &AvPacket,
        recovery: &mut VideoDecodeRecovery,
        realign_after_recovery_point: bool,
        committed_output_high_water_nsecs: Option<u64>,
    ) -> std::result::Result<bool, String> {
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && let Err(error) = &result
        {
            self.last_hevc_decode_error = Some(error.clone());
        }
        match result {
            Ok(()) => Ok(false),
            Err(error)
                if video_decode_error_requires_hevc_resource_pressure_recovery(
                    &error,
                    codec_id,
                    self.info().hardware_accelerated,
                ) =>
            {
                let packet_nsecs = packet
                    .read_diagnostic()
                    .and_then(|diagnostic| diagnostic.packet_start_nsecs)
                    .or_else(|| {
                        packet.best_timestamp().and_then(|timestamp| {
                            timestamp_to_nsecs(timestamp, self.info().time_base)
                        })
                    });
                let target_nsecs = committed_output_high_water_nsecs
                    .or(self.hevc_decode_chain_watchdog.last_decoded_video_end_nsecs)
                    .or(packet_nsecs)
                    .unwrap_or_default();
                self.request_hevc_resource_pressure_recovery(
                    target_nsecs,
                    packet_nsecs.map(|packet| packet.max(target_nsecs)),
                    &error,
                    Instant::now(),
                );
                let decoder_epoch = self.decoder_epoch;
                let release_external_references = self
                    .hevc_same_hardware_recovery
                    .as_mut()
                    .is_some_and(|transaction| {
                        transaction.claim_resource_pressure_external_release(decoder_epoch)
                    });
                recovery.reset();
                Ok(release_external_references)
            }
            Err(error) if video_decode_error_is_recoverable(&error) => {
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
                    resource_pressure = video_decode_error_is_resource_pressure(&error),
                    "recovering FFmpeg video decoder after recoverable decode error"
                );
                let generation = playback_generation.advance();
                self.flush_buffers(generation)?;
                recovery.begin_with_realign(realign_after_recovery_point);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodedFrame>, String> {
        let result = self.worker.poll_frame(generation);
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodePacketStatus>, String> {
        let result = self.worker.poll_packet_status(generation);
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
    }

    pub(super) fn flush_buffers(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.worker.flush_buffers(generation)?;
        self.clear_packets();
        Ok(())
    }

    pub(super) fn service_worker(&mut self) -> std::result::Result<(), String> {
        let result = self.worker.service();
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
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
        self.hevc_hw_replay.clear();
    }

    pub(super) fn reset_hevc_decode_chain_transient_state(&mut self) {
        self.hevc_decode_chain_watchdog.reset();
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::Transient,
        );
    }

    pub(super) fn reset_hevc_decoder_transient_preserving_gap_evidence(&mut self, now: Instant) {
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
    }

    pub(super) fn reset_hevc_decode_chain_recovery_transaction(&mut self) {
        self.reset_hevc_decode_chain_transient_state();
        self.hevc_hw_replay_journal.clear();
        self.hevc_same_hardware_recovery = None;
        self.last_hevc_decode_error = None;
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::RecoveryTransaction,
        );
        self.hevc_low_level_seek_observation = None;
        self.last_hevc_cra_low_level_landing = None;
    }

    pub(super) fn begin_hevc_low_level_seek_observation(
        &mut self,
        transaction_id: u64,
        target_nsecs: u64,
        seek_position_nsecs: u64,
        reason: &'static str,
    ) -> bool {
        if self.hevc_low_level_seek_would_repeat_cra(target_nsecs, seek_position_nsecs) {
            return false;
        }
        self.hevc_low_level_seek_observation = Some(HevcLowLevelSeekObservation {
            transaction_id,
            target_nsecs,
            seek_position_nsecs,
            reason,
            landing: None,
        });
        true
    }

    pub(super) fn hevc_low_level_seek_would_repeat_cra(
        &self,
        target_nsecs: u64,
        seek_position_nsecs: u64,
    ) -> bool {
        hevc_low_level_seek_would_repeat_cra(
            self.last_hevc_cra_low_level_landing,
            target_nsecs,
            seek_position_nsecs,
        )
    }

    pub(super) fn finish_hevc_low_level_exact_recovery(
        &mut self,
        transaction_id: u64,
    ) -> Option<HevcLowLevelSeekLanding> {
        let observation = self.hevc_low_level_seek_observation?;
        if observation.transaction_id != transaction_id {
            return None;
        }
        self.hevc_low_level_seek_observation = None;
        observation.landing
    }

    pub(super) fn clear_hevc_low_level_seek_recovery(&mut self) -> Option<HevcLowLevelSeekLanding> {
        self.hevc_low_level_seek_observation
            .take()
            .and_then(|observation| observation.landing)
    }

    fn observe_hevc_low_level_recovery_packet(
        &mut self,
        packet: &AvPacket,
        packet_nsecs: Option<u64>,
        codec_id: ffi::AVCodecID,
    ) -> Option<HevcLowLevelRecoveryObservationAction> {
        let observation = self.hevc_low_level_seek_observation?;
        if observation.landing.is_some() || codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return None;
        }
        let cache_read = packet.read_diagnostic();
        let recovery_kind = cache_read
            .map(|diagnostic| diagnostic.recovery_kind)
            .unwrap_or_else(|| packet_video_recovery_point_kind(packet, codec_id));
        if !recovery_kind.is_recovery_point() {
            return None;
        }
        let anchor_nsecs = cache_read
            .and_then(|diagnostic| diagnostic.packet_start_nsecs)
            .or(packet_nsecs)?;
        let landing = HevcLowLevelSeekLanding {
            transaction_id: observation.transaction_id,
            target_nsecs: observation.target_nsecs,
            seek_position_nsecs: observation.seek_position_nsecs,
            anchor_nsecs,
            anchor_kind: recovery_kind,
            range_id: cache_read.map(|diagnostic| diagnostic.read_range_id),
            anchor_packet_id: cache_read.map(|diagnostic| diagnostic.packet_id),
        };
        if recovery_kind == VideoRecoveryPointKind::Cra {
            let repeated = self
                .last_hevc_cra_low_level_landing
                .is_some_and(|previous| hevc_cra_low_level_landing_repeats(previous, landing));
            self.last_hevc_cra_low_level_landing = Some(landing);
            if let Some(current) = self.hevc_low_level_seek_observation.as_mut() {
                current.landing = Some(landing);
            }
            return Some(HevcLowLevelRecoveryObservationAction::CraLanding {
                landing,
                repeated,
                reason: observation.reason,
            });
        }
        if let Some(current) = self.hevc_low_level_seek_observation.as_mut() {
            current.landing = Some(landing);
        }
        Some(HevcLowLevelRecoveryObservationAction::SafeLanding {
            landing,
            reason: observation.reason,
        })
    }

    pub(super) fn observe_hevc_decode_packet_status(
        &mut self,
        observation: HevcDecodePacketObservation<'_>,
    ) -> HevcDecodeChainRecoveryAction {
        if observation.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.hevc_decode_chain_watchdog.reset();
            self.hevc_decode_packet_diagnostics.clear();
            self.hevc_hw_replay_journal.clear();
            return HevcDecodeChainRecoveryAction::None;
        }
        let packet_nsecs = observation
            .packet
            .read_diagnostic()
            .and_then(|diagnostic| diagnostic.packet_start_nsecs)
            .or_else(|| {
                observation.packet.best_timestamp().and_then(|timestamp| {
                    timestamp_to_nsecs(timestamp, observation.video_stream.time_base)
                })
            });
        let hardware_accelerated = self.info().hardware_accelerated;
        let now = Instant::now();
        let exact_seek_scoped = self
            .hevc_decode_chain_watchdog
            .observe_exact_seek_decoder_result(
                observation.recovery_scope,
                packet_nsecs,
                observation.status.decoded_frames,
                observation.status.result.is_ok(),
                now,
            );
        let evidence_scope = hevc_decode_packet_evidence_scope(
            exact_seek_scoped,
            observation.decode_recovery_active,
            self.hevc_same_hardware_recovery.is_some(),
            observation.packet_decode_recovery_scoped,
        );
        let action = match evidence_scope {
            HevcDecodePacketEvidenceScope::ExactSeek => HevcDecodeChainRecoveryAction::None,
            HevcDecodePacketEvidenceScope::DecodeRecovery => {
                self.hevc_decode_chain_watchdog
                    .observe_packet_during_decode_recovery(
                        observation.status.result.is_ok(),
                        observation.status.decoded_frames,
                        now,
                    );
                HevcDecodeChainRecoveryAction::None
            }
            HevcDecodePacketEvidenceScope::Playback => self
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
                    synchronized_audio_timeline_gap_checked: observation
                        .synchronized_audio_timeline_gap_checked,
                    synchronized_audio_timeline_gap: observation.synchronized_audio_timeline_gap,
                    cache_sequence_contiguous: observation
                        .packet
                        .read_diagnostic()
                        .and_then(|diagnostic| diagnostic.sequence_contiguous)
                        .unwrap_or(true),
                    fallback_target_nsecs: observation.fallback_target_nsecs,
                    now,
                }),
        };
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.observe_packet(
                observation.generation,
                packet_nsecs,
                observation.status.decoded_frames,
            );
        }
        let zero_output_run_packets = match evidence_scope {
            HevcDecodePacketEvidenceScope::ExactSeek => {
                self.hevc_decode_chain_watchdog
                    .exact_seek_zero_output_packets
            }
            HevcDecodePacketEvidenceScope::DecodeRecovery => 0,
            HevcDecodePacketEvidenceScope::Playback if observation.status.decoded_frames == 0 => {
                self.hevc_decode_chain_watchdog.zero_output_packets
            }
            HevcDecodePacketEvidenceScope::Playback => 0,
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
        observation.decode_recovery_active |= self.hevc_same_hardware_recovery.is_some();
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

    pub(super) fn complete_hevc_exact_seek_evidence(
        &mut self,
        completion: ExactSeekCompletion,
        decode_recovery_active: bool,
    ) {
        let preserve_playback_evidence =
            decode_recovery_active || self.hevc_same_hardware_recovery.is_some();
        let promote_failed_seek_evidence = self.info().hardware_accelerated;
        self.hevc_decode_chain_watchdog
            .complete_exact_seek_evidence_scope(
                completion.transaction_id,
                completion.first_eligible_frame_nsecs,
                preserve_playback_evidence,
                promote_failed_seek_evidence,
                Instant::now(),
            );
    }

    pub(super) fn request_hevc_same_hardware_recovery(
        &mut self,
        fallback: HevcDecodeChainFallback,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        if !self.info().hardware_accelerated {
            return HevcDecodeRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();

        let snapshot = self.snapshot();
        if self.hevc_same_hardware_recovery.is_none() {
            let mut transaction = HevcSameHardwareRecoveryTransaction::new(
                fallback,
                snapshot.result_produced_sequence,
                self.last_hevc_decode_error.clone(),
                now,
            );
            transaction.set_root_evidence(
                self.hevc_decode_chain_watchdog.recent_zero_output_packets,
                self.hevc_decode_chain_watchdog
                    .recent_input_packet_high_water_nsecs,
                self.hevc_decode_chain_watchdog
                    .recent_output_high_water_nsecs,
            );
            tracing::warn!(
                target_nsecs = transaction.target_nsecs,
                reason = transaction.reason.as_str(),
                root_zero_output_packets = transaction.root_zero_output_packets,
                root_input_high_water_nsecs = ?transaction.root_input_high_water_nsecs,
                root_output_high_water_nsecs = ?transaction.root_output_high_water_nsecs,
                decoder_epoch = self.decoder_epoch,
                same_hw_recovery_phase = transaction.phase.as_str(),
                submitted_sequence = snapshot.submitted_sequence,
                result_produced_sequence = snapshot.result_produced_sequence,
                result_consumed_sequence = snapshot.result_consumed_sequence,
                oldest_submitted_packet_nsecs = ?snapshot.oldest_submitted_packet_nsecs,
                "started bounded HEVC same-Vulkan recovery transaction"
            );
            self.hevc_same_hardware_recovery = Some(transaction);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }

        if self
            .hevc_same_hardware_recovery
            .as_ref()
            .is_some_and(|transaction| {
                !hevc_fallback_targets_match(transaction.target_nsecs, fallback.target_nsecs)
            })
        {
            let transaction = self
                .hevc_same_hardware_recovery
                .as_ref()
                .expect("same-hardware recovery transaction exists");
            tracing::warn!(
                root_target_nsecs = transaction.target_nsecs,
                observed_target_nsecs = fallback.target_nsecs,
                root_reason = transaction.reason.as_str(),
                observed_reason = fallback.reason.as_str(),
                same_hw_recovery_phase = transaction.phase.as_str(),
                "kept bounded same-Vulkan transaction across fallback target drift"
            );
        }

        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware recovery transaction exists");
        transaction.observed_target_nsecs = fallback.target_nsecs;
        if transaction.expired(now) {
            transaction.fail("same-Vulkan recovery wall-time limit exceeded");
            return transaction.terminal_action(self.requested_hardware_mode);
        }
        if transaction.failed_attempt_needs_decoder_drain(snapshot, now) {
            transaction.observe_result_progress(snapshot.result_produced_sequence, now);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }
        transaction.advance_after_repeated_failure_if_idle(
            snapshot.result_produced_sequence,
            now,
            self.requested_hardware_mode,
        )
    }

    pub(super) fn request_hevc_resource_pressure_recovery(
        &mut self,
        target_nsecs: u64,
        cutoff_nsecs: Option<u64>,
        error: &str,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        if !self.info().hardware_accelerated {
            return HevcDecodeRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();

        let snapshot = self.snapshot();
        if self.hevc_same_hardware_recovery.is_none() {
            let fallback = HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ResourcePressure,
            };
            let mut transaction = HevcSameHardwareRecoveryTransaction::new(
                fallback,
                snapshot.result_produced_sequence,
                Some(error.to_string()),
                now,
            );
            transaction.set_root_evidence(0, cutoff_nsecs, Some(target_nsecs));
            transaction.record_resource_pressure_error(error, cutoff_nsecs, now);
            tracing::warn!(
                target_nsecs = transaction.target_nsecs,
                frozen_cutoff_nsecs = ?transaction.replay_required_high_water_nsecs,
                decoder_epoch = self.decoder_epoch,
                same_hw_recovery_phase = transaction.phase.as_str(),
                submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
                "started release-first HEVC Vulkan resource-pressure recovery"
            );
            self.hevc_same_hardware_recovery = Some(transaction);
            return HevcDecodeRecoveryAction::FlushSameHardware;
        }

        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware recovery transaction exists");
        transaction.promote_to_resource_pressure(target_nsecs, cutoff_nsecs, error, now);
        if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
            transaction.finish_active_attempt("resource_pressure_escalated_to_reopen", now);
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            HevcDecodeRecoveryAction::ReopenSameHardware
        } else {
            transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
            HevcDecodeRecoveryAction::FlushSameHardware
        }
    }

    pub(super) fn hevc_same_hardware_recovery_is_resource_pressure(&self) -> bool {
        self.hevc_same_hardware_recovery
            .as_ref()
            .is_some_and(HevcSameHardwareRecoveryTransaction::resource_pressure)
    }

    pub(super) fn hevc_resource_pressure_demux_admission_stopped(&self) -> bool {
        self.hevc_same_hardware_recovery.as_ref().is_some_and(
            HevcSameHardwareRecoveryTransaction::resource_pressure_demux_admission_stopped,
        )
    }

    pub(super) fn hevc_resource_pressure_decoder_input_stopped(&self) -> bool {
        self.hevc_same_hardware_recovery.as_ref().is_some_and(
            HevcSameHardwareRecoveryTransaction::resource_pressure_decoder_input_stopped,
        )
    }

    pub(super) fn pending_hevc_same_hardware_recovery_action(
        &mut self,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        let snapshot = self.snapshot();
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return HevcDecodeRecoveryAction::None;
        };
        if transaction.expired(now)
            && !matches!(
                transaction.phase,
                HevcSameHardwareRecoveryPhase::Recovered | HevcSameHardwareRecoveryPhase::Failed
            )
        {
            transaction.fail("same-Vulkan recovery wall-time limit exceeded");
        }
        if transaction.failed_attempt_needs_decoder_drain(snapshot, now) {
            transaction.observe_result_progress(snapshot.result_produced_sequence, now);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }
        transaction.advance_after_repeated_failure_if_idle(
            snapshot.result_produced_sequence,
            now,
            self.requested_hardware_mode,
        )
    }

    pub(super) fn record_hevc_same_hardware_drain_pass(
        &mut self,
        made_progress: bool,
        now: Instant,
    ) -> bool {
        self.observe_hevc_same_hardware_worker_progress(now);
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::DrainingResults {
            return false;
        }
        transaction.drain_recorded = true;
        if made_progress {
            transaction.last_progress_at = now;
            return false;
        }
        if now.saturating_duration_since(transaction.last_progress_at)
            < HEVC_SAME_HARDWARE_DRAIN_GRACE
        {
            return false;
        }
        transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
        true
    }

    pub(super) fn begin_hevc_same_hardware_flush(
        &mut self,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("HEVC same-Vulkan flush requested without a transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::Flushing {
            return Err(format!(
                "HEVC same-Vulkan flush requested in phase {}",
                transaction.phase.as_str()
            ));
        }
        if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            transaction.last_error = Some("same-decoder flush attempt limit reached".to_string());
            return Err("HEVC same-decoder flush attempt limit reached".to_string());
        }
        transaction.flush_attempts = transaction.flush_attempts.saturating_add(1);
        transaction.last_progress_at = now;
        if let Err(error) = self.flush_buffers(generation) {
            let transaction = self
                .hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives flush failure");
            transaction.last_error = Some(format!("same-decoder flush failed: {error}"));
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            return Err(error);
        }
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware transaction survives flush");
        transaction.begin_attempt(
            self.decoder_epoch,
            HevcSameHardwareRecoveryAttemptKind::FlushReplay,
            generation,
            now,
        );
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        Ok(())
    }

    pub(super) fn begin_hevc_same_hardware_reopen(
        &mut self,
        stream: StreamInfo,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<Arc<VulkanDecodeDevice>, String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("HEVC same-Vulkan reopen requested without a transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::Reopening {
            return Err(format!(
                "HEVC same-Vulkan reopen requested in phase {}",
                transaction.phase.as_str()
            ));
        }
        if transaction.reopen_attempts >= HEVC_SAME_HARDWARE_MAX_REOPEN_ATTEMPTS {
            transaction.fail("same-Vulkan reopen attempt limit reached");
            return Err("HEVC same-Vulkan reopen attempt limit reached".to_string());
        }
        transaction.reopen_attempts = transaction.reopen_attempts.saturating_add(1);
        let release_first = transaction.resource_pressure();

        // mpv's force_fallback() tears down the failed AVCodecContext before
        // opening its replacement. Preserve the atomic open-first swap for
        // ordinary corruption recovery, but never keep two Vulkan pools alive
        // while recovering from device-memory pressure.
        if release_first
            && let Err(error) = self
                .worker
                .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
        {
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives worker retirement failure")
                .fail(format!(
                    "same-Vulkan old worker release-first retirement failed: {error}"
                ));
            return Err(error);
        }

        // Force the candidate open to remain hardware-only even when the original
        // policy was Auto. Software fallback is a separate terminal action.
        let decoder = match Decoder::open_video(stream, hevc_same_hardware_reopen_mode()) {
            Ok(decoder) => decoder,
            Err(error) => {
                let transaction = self
                    .hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives reopen failure");
                transaction.fail(format!("same-Vulkan decoder open failed: {error}"));
                return Err(error);
            }
        };
        if !decoder.is_hardware_accelerated() {
            let error = "same-Vulkan candidate unexpectedly opened without hardware acceleration"
                .to_string();
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives invalid candidate")
                .fail(error.clone());
            return Err(error);
        }
        let Some(device) = decoder.vulkan_device() else {
            let error = "same-Vulkan candidate did not expose a Vulkan decode device".to_string();
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives missing device")
                .fail(error.clone());
            return Err(error);
        };
        let worker = match VideoDecodeWorker::spawn(decoder) {
            Ok(worker) => worker,
            Err(error) => {
                self.hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives worker spawn failure")
                    .fail(format!("same-Vulkan worker spawn failed: {error}"));
                return Err(error);
            }
        };

        if !release_first {
            // Ordinary recovery retains the open-first atomic swap.
            if let Err(error) = self
                .worker
                .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
            {
                self.hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives worker retirement failure")
                    .fail(format!("same-Vulkan old worker retirement failed: {error}"));
                return Err(error);
            }
        }
        self.worker = worker;
        self.clear_packets();
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
        self.last_hevc_decode_error = None;
        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware transaction survives atomic worker swap");
        transaction.begin_attempt(
            self.decoder_epoch,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            generation,
            now,
        );
        transaction.phase = HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen;
        transaction.last_progress_at = now;
        transaction.last_result_produced_sequence = self.worker.snapshot().result_produced_sequence;
        Ok(device)
    }

    pub(super) fn record_hevc_same_hardware_replay(
        &mut self,
        replay_packets: usize,
        after_reopen: bool,
        now: Instant,
    ) {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return;
        };
        transaction.record_replay(replay_packets, after_reopen, now);
    }

    pub(super) fn begin_hevc_same_hardware_cached_rebuild(
        &mut self,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("cached safe-IDR rebuild requested without a transaction".to_string());
        };
        transaction.begin_cached_rebuild(self.decoder_epoch, generation, now)
    }

    pub(super) fn fail_hevc_same_hardware_cached_rebuild(&mut self, error: impl Into<String>) {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return;
        };
        if transaction.phase == HevcSameHardwareRecoveryPhase::RebuildingFromCache {
            transaction.cached_rebuild_attempts =
                transaction.cached_rebuild_attempts.saturating_add(1);
        }
        transaction.fail(error);
    }

    pub(super) fn mark_hevc_same_hardware_prewarm_ready(&mut self, now: Instant) -> bool {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen {
            return false;
        }
        transaction.prewarm_ticket = None;
        transaction.last_progress_at = now;
        true
    }

    pub(super) fn record_hevc_same_hardware_prewarm_request(
        &mut self,
        ticket: VulkanPrewarmTicket,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("Vulkan prewarm requested without a same-hardware transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen {
            return Err(format!(
                "Vulkan prewarm requested in same-hardware phase {}",
                transaction.phase.as_str()
            ));
        }
        transaction.prewarm_ticket = Some(ticket);
        Ok(())
    }

    pub(super) fn hevc_same_hardware_prewarm_ticket(&self) -> Option<VulkanPrewarmTicket> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.prewarm_ticket)
    }

    pub(super) fn fail_hevc_same_hardware_recovery(&mut self, error: impl Into<String>) {
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.fail(error);
        }
    }

    pub(super) fn hevc_same_hardware_recovery_target(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .map(|transaction| transaction.target_nsecs)
    }

    pub(super) fn hevc_same_hardware_recovery_attempt_id(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(HevcSameHardwareRecoveryTransaction::active_attempt_id)
    }

    pub(super) fn hevc_same_hardware_recovery_decoder_epoch(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(HevcSameHardwareRecoveryTransaction::active_decoder_epoch)
    }

    pub(super) fn mark_hevc_same_hardware_unbridged_continuous_gap(&mut self) {
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.mark_unbridged_continuous_gap();
        }
    }

    pub(super) fn hevc_same_hardware_action_log_summary(
        &mut self,
        action: HevcDecodeRecoveryAction,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_mut()
            .and_then(|transaction| transaction.should_log_action(action, now))
    }

    pub(super) fn hevc_same_hardware_drain_log_summary(
        &mut self,
        advanced: bool,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_mut()
            .and_then(|transaction| transaction.should_log_drain(advanced, now))
    }

    pub(super) fn hevc_same_hardware_recovery_terminal_error(
        &self,
        now: Instant,
    ) -> Option<String> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .filter(|transaction| transaction.phase == HevcSameHardwareRecoveryPhase::Failed)
            .map(|transaction| transaction.terminal_error(now, self.requested_hardware_mode))
    }

    pub(super) fn finish_hevc_same_hardware_recovery_terminal(&mut self) {
        self.hevc_same_hardware_recovery = None;
    }

    pub(super) fn requested_hardware_mode(&self) -> HardwareDecodeMode {
        self.requested_hardware_mode
    }

    fn observe_hevc_same_hardware_worker_progress(&mut self, now: Instant) -> bool {
        let result_produced_sequence = self.worker.snapshot().result_produced_sequence;
        self.hevc_same_hardware_recovery
            .as_mut()
            .is_some_and(|transaction| {
                transaction.observe_result_progress(result_produced_sequence, now)
            })
    }

    pub(super) fn mark_hevc_same_hardware_output_committed(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> bool {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        if !matches!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
                | HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
        ) {
            return false;
        }
        let Some(attempt) = transaction.active_attempt.as_mut() else {
            return false;
        };
        if attempt.output_commit_observed {
            return false;
        }
        attempt.output_commit_observed = true;
        tracing::debug!(
            session_id = ?session_id,
            target_nsecs = transaction.target_nsecs,
            attempt_id = attempt.attempt_id,
            decoder_epoch = attempt.decoder_epoch,
            attempt_kind = attempt.kind.as_str(),
            admitted_span_after_catch_up_ms =
                attempt.admitted_span_after_catch_up_nsecs as f64 / 1_000_000.0,
            "recorded atomic output commit for bounded HEVC recovery"
        );
        true
    }

    pub(super) fn observe_hevc_admitted_video_progress(
        &mut self,
        observation: HevcAdmittedVideoProgressObservation,
    ) {
        let now = Instant::now();
        let root_progress = if self.hevc_same_hardware_recovery.is_none() {
            self.hevc_decode_chain_watchdog
                .observe_admitted_video_progress(observation)
        } else {
            HevcAdmittedVideoProgress::None
        };
        let admitted_decoder_epoch = self
            .hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.active_attempt.as_ref())
            .filter(|attempt| attempt.observes_generation(observation.generation))
            .map(|attempt| attempt.decoder_epoch)
            .or_else(|| {
                self.hevc_same_hardware_recovery
                    .as_ref()
                    .is_none_or(|transaction| {
                        transaction.phase == HevcSameHardwareRecoveryPhase::DrainingResults
                    })
                    .then_some(self.decoder_epoch)
            });
        let transaction_progress = self
            .hevc_same_hardware_recovery
            .as_mut()
            .map(|transaction| transaction.observe_admitted_video_progress(observation, now));
        let admitted_progress = transaction_progress.unwrap_or(root_progress);
        if matches!(
            admitted_progress,
            HevcAdmittedVideoProgress::Partial | HevcAdmittedVideoProgress::Stable
        ) {
            self.admitted_video_sequence = self.admitted_video_sequence.saturating_add(1);
            self.last_admitted_decoder_epoch = admitted_decoder_epoch;
            self.last_hevc_decode_error = None;
        }
        if admitted_progress != HevcAdmittedVideoProgress::Stable {
            return;
        }

        self.hevc_decode_chain_watchdog.clear_recent_gap_evidence();
        self.hevc_decode_chain_watchdog.pending_fallback = None;
        self.last_hevc_decode_chain_fallback = None;
        if let Some(mut transaction) = self.hevc_same_hardware_recovery.take() {
            transaction.finish_active_attempt("recovered", now);
            transaction.phase = HevcSameHardwareRecoveryPhase::Recovered;
            transaction.last_progress_at = now;
            tracing::info!(
                session_id = ?observation.session_id,
                target_nsecs = transaction.target_nsecs,
                reason = transaction.reason.as_str(),
                same_hw_recovery_phase = transaction.phase.as_str(),
                decoder_epoch = self.decoder_epoch,
                admitted_video_sequence = self.admitted_video_sequence,
                flush_attempts = transaction.flush_attempts,
                reopen_attempts = transaction.reopen_attempts,
                replay_packets = transaction.replay_packets,
                attempt_ledger = ?transaction.attempt_ledger,
                elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                "completed bounded HEVC same-Vulkan recovery transaction"
            );
        }
    }

    pub(super) fn observe_hevc_post_fallback_rebuffer_underfill(
        &mut self,
        observation: HevcPostFallbackRebufferObservation,
    ) {
        if observation.decode_recovery_active || self.hevc_same_hardware_recovery.is_some() {
            self.hevc_decode_chain_watchdog
                .suspend_playback_watchdogs_for_decode_recovery();
            return;
        }
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
        if self.hevc_same_hardware_recovery.is_some() {
            self.hevc_decode_chain_watchdog
                .suspend_playback_watchdogs_for_decode_recovery();
            return HevcDecodeChainRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .observe_startup_stall(observation)
    }

    pub(super) fn hevc_startup_stall_watchdog_deadline(&self) -> Option<Instant> {
        if self.hevc_same_hardware_recovery.is_some() {
            return None;
        }
        self.hevc_decode_chain_watchdog
            .startup_watchdog_deadline(self.info().hardware_accelerated)
    }

    pub(super) fn suspend_hevc_playback_watchdogs_for_decode_recovery(&mut self) {
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();
    }

    pub(super) fn complete_hevc_startup_watchdog_after_first_frame(&mut self) {
        self.hevc_decode_chain_watchdog
            .complete_startup_watchdog_after_first_frame();
    }

    pub(super) fn defer_hevc_startup_stall_watchdog_after_no_action(&mut self, now: Instant) {
        self.hevc_decode_chain_watchdog
            .defer_startup_watchdog_after_no_action(now);
    }

    pub(super) fn suspend_hevc_startup_watchdog_for_input_wait(&mut self) -> bool {
        self.hevc_decode_chain_watchdog
            .suspend_startup_watchdog_for_input_wait()
    }

    pub(super) fn observe_hevc_decode_pipeline_progress(&mut self, now: Instant) {
        self.hevc_decode_chain_watchdog
            .observe_replay_packet_progress(now);
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.last_progress_at = now;
        }
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

    pub(super) fn hevc_exact_seek_evidence_scope_active(&self) -> bool {
        self.hevc_decode_chain_watchdog
            .exact_seek_evidence_scope_active()
    }

    pub(super) fn hevc_exact_seek_landing_nsecs(&self) -> Option<u64> {
        self.hevc_decode_chain_watchdog
            .completed_exact_seek_landing_nsecs
    }

    pub(super) fn take_hevc_decode_chain_fallback(&mut self) -> Option<HevcDecodeChainFallback> {
        self.hevc_decode_chain_watchdog.take_fallback()
    }

    pub(super) fn hevc_decode_chain_fallback_pending(&self) -> bool {
        self.hevc_decode_chain_watchdog.has_pending_fallback()
    }

    pub(super) fn pending_hevc_decode_chain_fallback(&self) -> Option<HevcDecodeChainFallback> {
        self.hevc_decode_chain_watchdog.pending_fallback()
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
        self.packets.has_pending_or_in_flight() || !self.hevc_hw_replay.is_empty()
    }

    pub(super) fn take_pending_input(&mut self) -> Option<PendingVideoDecodePacket> {
        take_next_video_decode_input(&mut self.packets, &mut self.hevc_hw_replay)
    }

    pub(super) fn push_in_flight(
        &mut self,
        packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) {
        let arm_hevc_startup_in_flight = packet.hevc_startup_in_flight_watchdog;
        let from_hevc_hw_replay = packet.from_hevc_hw_replay;
        self.packets.push_in_flight(packet);
        let now = Instant::now();
        self.hevc_decode_chain_watchdog
            .resume_startup_watchdog_after_packet_submission(now);
        if from_hevc_hw_replay {
            self.hevc_decode_chain_watchdog
                .observe_replay_packet_progress(now);
            if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
                transaction.last_progress_at = now;
            }
        }
        if arm_hevc_startup_in_flight {
            self.hevc_decode_chain_watchdog
                .arm_startup_in_flight_stall(session_id, now);
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
        if !runtime_hevc_software_fallback_allowed(self.requested_hardware_mode) {
            return Err(format!(
                "software decoder reopen blocked by hardware-mode invariant: requested_hw_mode={:?}",
                self.requested_hardware_mode
            ));
        }
        // Match mpv's force_fallback(): completely retire the failed hardware
        // AVCodecContext before selecting and opening the final software path.
        // At this point bounded Vulkan recovery is exhausted, so preserving an
        // already-failed worker for an atomic swap provides no useful rollback.
        self.worker
            .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
            .map_err(|error| format!("FFmpeg 硬解 worker 退役失败：{error}"))?;
        let decoder = Decoder::open_video(stream, HardwareDecodeMode::Off)
            .map_err(|error| format!("FFmpeg 重新打开软件视频解码器失败：{error}"))?;
        let worker = VideoDecodeWorker::spawn(decoder)?;
        self.worker = worker;
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.clear_packets();
        self.hevc_decode_chain_watchdog.reset();
        Ok(true)
    }

    fn remember_hevc_hw_replay_packet(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        session_id: PlaybackSessionId,
    ) {
        if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC || !self.info().hardware_accelerated {
            return;
        }
        let time_base = self.info().time_base;
        let safe_anchor = hevc_packet_is_safe_replay_anchor(packet, codec_id);
        let packet_nsecs = hevc_replay_packet_start_nsecs(packet, time_base);
        let decoded_output_end_nsecs = max_optional_u64(
            self.hevc_decode_chain_watchdog.last_decoded_video_end_nsecs,
            self.hevc_decode_chain_watchdog
                .recent_output_high_water_nsecs,
        );
        let recovery_cutoff_locked = self.hevc_same_hardware_recovery.is_some()
            || self.hevc_decode_chain_watchdog.has_pending_fallback();
        let recent_evidence_would_preserve =
            self.hevc_decode_chain_watchdog.has_recent_gap_evidence()
                || self.hevc_decode_chain_watchdog.health_state == HevcDecodeHealthState::Suspected;
        let roll_safe_anchor = recent_evidence_would_preserve
            && hevc_safe_anchor_can_roll_past_preserved_evidence(
                safe_anchor,
                packet_nsecs,
                decoded_output_end_nsecs,
                recovery_cutoff_locked,
            );
        let preserve_safe_anchor =
            !roll_safe_anchor && (recovery_cutoff_locked || recent_evidence_would_preserve);
        let coverage_was_exhausted = self.hevc_hw_replay_journal.coverage_exhausted;
        let previous_anchor_nsecs = self.hevc_hw_replay_journal.anchor_nsecs;
        let previous_high_water_nsecs = self.hevc_hw_replay_journal.high_water_nsecs;
        let remember_result = if preserve_safe_anchor {
            self.hevc_hw_replay_journal
                .remember_preserving_safe_anchor(packet, codec_id, time_base)
        } else {
            self.hevc_hw_replay_journal
                .remember(packet, codec_id, time_base)
        };
        match remember_result {
            Ok(true) => {
                if roll_safe_anchor {
                    tracing::debug!(
                        session_id = ?session_id,
                        packet_nsecs,
                        decoded_output_end_nsecs,
                        previous_anchor_nsecs,
                        previous_high_water_nsecs,
                        previous_coverage_exhausted = coverage_was_exhausted,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_anchor_kind = ?self.hevc_hw_replay_journal.anchor_kind
                            .map(|kind| kind.as_str()),
                        "rolled HEVC recovery journal to safe anchor already covered by output"
                    );
                } else {
                    tracing::trace!(
                        session_id = ?session_id,
                        packet_pts = ?packet.best_timestamp(),
                        hevc_hw_replay_packets = self.hevc_hw_replay_journal.len(),
                        hevc_hw_replay_bytes = self.hevc_hw_replay_journal.total_bytes,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_anchor_kind = ?self.hevc_hw_replay_journal.anchor_kind
                            .map(|kind| kind.as_str()),
                        preserve_safe_anchor,
                        "remembered HEVC packet in safe hardware replay journal"
                    );
                }
            }
            Ok(false) => {
                if preserve_safe_anchor
                    && !coverage_was_exhausted
                    && self.hevc_hw_replay_journal.coverage_exhausted
                {
                    tracing::warn!(
                        session_id = ?session_id,
                        hevc_hw_replay_packets = self.hevc_hw_replay_journal.len(),
                        hevc_hw_replay_bytes = self.hevc_hw_replay_journal.total_bytes,
                        rejected_packet_bytes = packet.byte_len(),
                        hevc_hw_replay_packet_limit = HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
                        hevc_hw_replay_byte_limit = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
                        hevc_hw_replay_duration_limit_ms =
                            HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS as f64 / 1_000_000.0,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_high_water_nsecs = ?self.hevc_hw_replay_journal.high_water_nsecs,
                        "HEVC recovery journal exhausted its bounded packet, byte, or duration coverage"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    session_id = ?session_id,
                    %error,
                    "failed to remember HEVC hardware replay packet"
                );
            }
        }
    }
}

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
mod tests {
    use ffmpeg_sys_next as ffi;
    use std::time::{Duration, Instant};

    use crate::player::render_host::{PlaybackSessionId, RenderSize};

    use super::super::{
        AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, DemuxReaderWatermark,
        HardwareDecodeMode, PlaybackOutputSnapshot, PlaybackOutputState, StreamInfo,
        VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES, VideoFrameConvertContext, VideoRecoveryPointKind,
        decoded_video_frame_start_action, packet_is_video_recovery_point,
        packet_is_video_seek_point,
    };
    use super::{
        AudioTimelineGapEvidence, DecodedVideoFrameDiagnostic, DoviFrameMetadata,
        DoviRpuNalInspection, EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS,
        HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT,
        HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY, HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
        HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS, HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
        HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER, HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS,
        HEVC_SAME_HARDWARE_CACHED_REBUILD_MAX_PACKET_LEAD_NSECS,
        HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT, HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS,
        HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME, HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT,
        HEVC_STARTUP_IN_FLIGHT_HARD_AFTER, HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL,
        HEVC_STARTUP_WATCHDOG_RETRY_AFTER, HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER,
        HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT, HevcAdmittedVideoProgress,
        HevcAdmittedVideoProgressObservation, HevcDecodeChainFallback,
        HevcDecodeChainFallbackLoopAction, HevcDecodeChainFallbackReason,
        HevcDecodeChainFallbackRecord, HevcDecodeChainRecoveryAction, HevcDecodeChainResetScope,
        HevcDecodeChainWatchdog, HevcDecodeChainWatchdogInput, HevcDecodeHealthState,
        HevcDecodePacketDiagnosticWindow, HevcDecodePacketEvidenceScope, HevcDecodeRecoveryAction,
        HevcDecodedFrameGapAction, HevcDecodedFrameGapObservation, HevcHwReplayJournal,
        HevcLowLevelSeekLanding, HevcPostFallbackRebufferObservation,
        HevcPostSoftRecoverySkippedPacketObservation, HevcSameHardwareRecoveryAttempt,
        HevcSameHardwareRecoveryAttemptKind, HevcSameHardwareRecoveryPhase,
        HevcSameHardwareRecoveryTransaction, HevcSeekPrerollProgressObservation,
        HevcStartupStallObservation, HevcStreamFormat, PendingVideoDecodePacket,
        PlaybackBlockReason, PlaybackGeneration, StrippedHevcDoviDecodeAction,
        VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
        VideoDecodePacketQueues, VideoDecodePacketStatus, VideoDecodePipeline, VideoDecodeRecovery,
        VideoDecodeRecoveryScope, VideoDecodeWorkerInfo, VideoDecodeWorkerSnapshot,
        VideoDecodeWorkerState, hevc_cra_low_level_landing_repeats,
        hevc_decode_chain_fallback_loop_action, hevc_decode_chain_fallback_record_after,
        hevc_decode_chain_recovery_record_after_reset, hevc_decode_packet_evidence_scope,
        hevc_dovi_decode_action_for_inspection, hevc_drain_video_result_progressed,
        hevc_hw_replay_packets, hevc_low_level_seek_would_repeat_cra,
        hevc_safe_anchor_can_roll_past_preserved_evidence, hevc_same_hardware_reopen_mode,
        hevc_startup_in_flight_packet_should_arm, hevc_startup_zero_output_timeout,
        hevc_zero_output_log_milestone, requeue_backpressured_video_decode_input,
        runtime_hevc_software_fallback_allowed, take_next_video_decode_input,
        video_decode_error_requires_hevc_resource_pressure_recovery,
        video_decode_pending_input_snapshot,
    };

    #[test]
    fn zero_output_logging_uses_only_threshold_and_doubling_milestones() {
        let logged = (1..=64)
            .filter(|count| hevc_zero_output_log_milestone(*count))
            .collect::<Vec<_>>();
        assert_eq!(logged, vec![1, 2, 4, 8, 16, 24, 30, 32, 64]);
    }

    #[test]
    fn post_commit_hevc_replay_status_stays_in_decode_recovery_scope() {
        assert_eq!(
            hevc_decode_packet_evidence_scope(false, false, false, true),
            HevcDecodePacketEvidenceScope::DecodeRecovery,
            "a replay PacketDone can outlive both recovery transactions"
        );
        assert_eq!(
            hevc_decode_packet_evidence_scope(false, false, false, false),
            HevcDecodePacketEvidenceScope::Playback,
            "only a fresh live packet may re-arm the playback watchdog"
        );
    }

    #[test]
    fn exact_seek_scope_precedes_decode_recovery_and_replay_scopes() {
        assert_eq!(
            hevc_decode_packet_evidence_scope(true, true, true, true),
            HevcDecodePacketEvidenceScope::ExactSeek
        );
        assert_eq!(
            hevc_decode_packet_evidence_scope(false, true, false, false),
            HevcDecodePacketEvidenceScope::DecodeRecovery
        );
        assert_eq!(
            hevc_decode_packet_evidence_scope(false, false, true, false),
            HevcDecodePacketEvidenceScope::DecodeRecovery
        );
    }

    #[test]
    fn raw_vulkan_oom_bypasses_the_generic_recoverable_error_classifier() {
        let raw_error = "Vulkan decoder failed: VK_ERROR_OUT_OF_DEVICE_MEMORY";
        assert!(!super::video_decode_error_is_recoverable(raw_error));
        assert!(video_decode_error_requires_hevc_resource_pressure_recovery(
            raw_error,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
        ));
        assert!(
            !video_decode_error_requires_hevc_resource_pressure_recovery(
                raw_error,
                ffi::AVCodecID::AV_CODEC_ID_H264,
                true,
            )
        );
        assert!(
            !video_decode_error_requires_hevc_resource_pressure_recovery(
                raw_error,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                false,
            )
        );
    }

    #[test]
    fn repeated_cra_low_level_landing_ignores_range_and_packet_identity() {
        let first = HevcLowLevelSeekLanding {
            transaction_id: 11,
            target_nsecs: 62_521_000_000,
            seek_position_nsecs: 61_521_000_000,
            anchor_nsecs: 59_768_000_000,
            anchor_kind: VideoRecoveryPointKind::Cra,
            range_id: Some(2),
            anchor_packet_id: Some(41),
        };
        let repeated = HevcLowLevelSeekLanding {
            range_id: Some(3),
            anchor_packet_id: Some(93),
            ..first
        };
        assert!(hevc_cra_low_level_landing_repeats(first, repeated));

        let different_anchor = HevcLowLevelSeekLanding {
            anchor_nsecs: first.anchor_nsecs + 1,
            ..repeated
        };
        assert!(!hevc_cra_low_level_landing_repeats(first, different_anchor));
    }

    #[test]
    fn same_cra_tuple_suppresses_second_low_level_seek() {
        let landing = HevcLowLevelSeekLanding {
            transaction_id: 11,
            target_nsecs: 62_521_000_000,
            seek_position_nsecs: 61_521_000_000,
            anchor_nsecs: 59_768_000_000,
            anchor_kind: VideoRecoveryPointKind::Cra,
            range_id: Some(2),
            anchor_packet_id: Some(41),
        };

        assert!(hevc_low_level_seek_would_repeat_cra(
            Some(landing),
            landing.target_nsecs,
            landing.seek_position_nsecs,
        ));
        assert!(!hevc_low_level_seek_would_repeat_cra(
            Some(landing),
            landing.target_nsecs + 1,
            landing.seek_position_nsecs,
        ));
    }

    #[test]
    fn exact_seek_first_frame_releases_hevc_startup_watchdog() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            first_zero_output_at: Some(now - Duration::from_secs(1)),
            startup_in_flight_stall_started_at: Some(now - Duration::from_secs(1)),
            startup_watchdog_retry_not_before: Some(now),
            startup_watchdog_last_rejection_at: Some(now),
            startup_watchdog_last_rejection_reason: Some("test"),
            startup_watchdog_suppressed_rejections: 17,
            ..HevcDecodeChainWatchdog::default()
        };
        assert!(watchdog.startup_watchdog_deadline(true).is_some());

        watchdog.complete_startup_watchdog_after_first_frame();

        assert!(watchdog.startup_watchdog_deadline(true).is_none());
        assert!(watchdog.startup_watchdog_completed);
        assert!(watchdog.startup_watchdog_last_rejection_at.is_none());
        assert_eq!(watchdog.startup_watchdog_suppressed_rejections, 0);

        watchdog.reset_transient_after_progress(None, Some(184_740_000_000), now);
        assert!(watchdog.startup_watchdog_completed);
        assert!(watchdog.startup_watchdog_deadline(true).is_none());
    }

    fn snapshot(
        state: VideoDecodeWorkerState,
        pending_input_packets: usize,
        submitted_not_consumed_packets: usize,
    ) -> VideoDecodeWorkerSnapshot {
        VideoDecodeWorkerSnapshot {
            state,
            queued_frames: 0,
            queue_capacity: VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES,
            pending_input_packets,
            pending_input_capacity: VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            submitted_not_consumed_packets,
            command_queue_capacity: 4,
            completed_packets: 0,
            ..VideoDecodeWorkerSnapshot::default()
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
            first_frame_needed: state.first_video_frame_pending(),
            first_frame_presented: !state.first_video_frame_pending(),
            initial_av_start_pending: state.first_video_frame_pending(),
            output_clock_running: state == PlaybackOutputState::Playing,
            audio_start_target_nsecs: None,
            output_transition_deadline_ms: None,
            rebuffering,
            queued_video_frames: usize::from(queued_video_range_nsecs.is_some()),
            recovery_staging_frames: 0,
            recovery_staging_frame_budget: None,
            committed_output_high_water_nsecs: queued_video_range_nsecs.map(|(_, end)| end),
            recovery_staged_high_water_nsecs: None,
            decode_recovery_audio_ready_latched: false,
            queued_video_coverage_nsecs: queued_video_range_nsecs
                .map(|(start, end)| end.saturating_sub(start))
                .unwrap_or_default(),
            queued_video_duration_nsecs: queued_video_range_nsecs
                .map(|(start, end)| end.saturating_sub(start))
                .unwrap_or_default(),
            queued_video_range_span_nsecs: queued_video_range_nsecs
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
            scheduler_dropped_video_frames: 0,
            recent_coordinator_stall_nsecs: None,
            recent_coordinator_stall_age_nsecs: None,
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
            synchronized_audio_timeline_gap_checked: true,
            synchronized_audio_timeline_gap: None,
            cache_sequence_contiguous: true,
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
            hardware_accelerated: true,
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
            decode_recovery_active: false,
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
    fn hevc_zero_output_watchdog_enters_suspected_at_24_without_flushing() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let low_water = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        for packet_index in 0..23_u64 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    1_600_000_000 + packet_index * 1_000_000,
                    low_water,
                    demux_watermark(false),
                    1_250_000_000,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        let action = watchdog.observe_packet(hevc_watchdog_input(
            1_623_000_000,
            low_water,
            demux_watermark(false),
            1_250_000_000,
        ));

        assert_eq!(action, HevcDecodeChainRecoveryAction::None);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
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
    fn hevc_zero_output_packet_status_refreshes_in_flight_progress_deadline() {
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
        assert_eq!(watchdog.startup_in_flight_deadline(), None);

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

        assert_eq!(watchdog.take_fallback(), None);
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
    fn hevc_startup_watchdog_pauses_for_input_and_rearms_on_submission() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 6,
            first_zero_output_at: Some(now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
            ..Default::default()
        };

        assert!(watchdog.startup_watchdog_deadline(true).is_some());
        assert!(watchdog.suspend_startup_watchdog_for_input_wait());
        assert_eq!(watchdog.startup_watchdog_deadline(true), None);

        watchdog.resume_startup_watchdog_after_packet_submission(now);

        assert_eq!(
            watchdog.startup_watchdog_deadline(true),
            Some(now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER)
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
    fn six_zero_output_preroll_packets_continue_to_clean_target_frame() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
        let target_nsecs = 184_692_319_900;
        for packet_nsecs in [
            179_900_000_000,
            180_166_666_667,
            180_200_000_000,
            180_233_333_333,
            180_266_666_666,
            180_299_999_999,
        ] {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    packet_nsecs,
                    startup,
                    demux_watermark(false),
                    target_nsecs,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(watchdog.zero_output_packets, 6);
        assert_eq!(watchdog.take_fallback(), None);
        assert!(watchdog.suspend_startup_watchdog_for_input_wait());

        watchdog.resume_startup_watchdog_after_packet_submission(Instant::now());
        let mut target =
            hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
        target.decoded_frames = 1;

        assert_eq!(
            watchdog.observe_packet(target),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.take_fallback(), None);
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

    fn hevc_packet(nal_header: u8, id: u8, pts_millis: i64, key: bool) -> AvPacket {
        let mut packet = packet_from_data(&[0, 0, 0, 3, nal_header, 0x01, id]);
        unsafe {
            (*packet.as_mut_ptr()).pts = pts_millis;
            (*packet.as_mut_ptr()).dts = pts_millis;
            if key {
                (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
            }
        }
        packet
    }

    #[test]
    fn hevc_hw_replay_journal_starts_only_at_safe_idr_or_bla() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let trail = hevc_packet(0x02, 1, 1_000, false);
        assert!(
            !journal
                .remember(&trail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );

        let cra = hevc_packet(0x2a, 2, 1_040, true);
        assert!(
            !journal
                .remember(&cra, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 0);

        let idr = hevc_packet(0x26, 3, 1_080, true);
        assert!(
            journal
                .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.anchor_nsecs, Some(1_080_000_000));
        assert!(matches!(
            journal.anchor_kind,
            Some(VideoRecoveryPointKind::Idr)
        ));

        let bla = hevc_packet(0x20, 4, 1_120, true);
        assert!(
            journal
                .remember(&bla, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.anchor_nsecs, Some(1_120_000_000));
        assert!(matches!(
            journal.anchor_kind,
            Some(VideoRecoveryPointKind::Bla)
        ));
    }

    #[test]
    fn hevc_hw_replay_uses_cached_safe_anchor_verdict_after_payload_rewrite() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let mut packet = hevc_packet(0x02, 1, 1_000, false);
        packet.set_read_diagnostic(AvPacketReadDiagnostic {
            read_sequence: 7,
            cache_generation: 3,
            read_range_id: 2,
            packet_id: 41,
            stream_offset: 1,
            storage: AvPacketStorageKind::Memory,
            read_index_before: 8,
            read_index_after: 9,
            reader_head_before: Some(41),
            reader_head_after: Some(42),
            previous_read_packet_id: Some(40),
            previous_read_generation: Some(3),
            previous_expected_next_packet_id: Some(41),
            sequence_contiguous: Some(true),
            packet_start_nsecs: Some(1_000_000_000),
            packet_end_nsecs: Some(1_033_333_333),
            timeline_anchor: true,
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Idr,
            safe_seek_point: true,
        });

        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("cached safe packet refs")
        );
        assert_eq!(journal.anchor_nsecs, Some(1_000_000_000));
        assert_eq!(journal.anchor_kind, Some(VideoRecoveryPointKind::Idr));
    }

    #[test]
    fn problem_trace_19_rolls_safe_idr_already_covered_by_decoded_output() {
        let safe_idr_nsecs = 1_194_866_655_556;
        let decoded_output_end_nsecs = 1_195_499_988_766;
        assert!(hevc_safe_anchor_can_roll_past_preserved_evidence(
            true,
            Some(safe_idr_nsecs),
            Some(decoded_output_end_nsecs),
            false,
        ));
        assert!(
            !hevc_safe_anchor_can_roll_past_preserved_evidence(
                true,
                Some(decoded_output_end_nsecs.saturating_add(1)),
                Some(decoded_output_end_nsecs),
                false,
            ),
            "an IDR ahead of decoded output can belong to the active failure and must not replace the protected prefix"
        );
        assert!(
            !hevc_safe_anchor_can_roll_past_preserved_evidence(
                true,
                Some(safe_idr_nsecs),
                Some(decoded_output_end_nsecs),
                true,
            ),
            "a frozen recovery cutoff keeps its current replay prefix immutable"
        );
    }

    #[test]
    fn hevc_hw_replay_rejects_preroll_that_does_not_cover_target() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let target_nsecs = 184_692_319_900;
        let mut journal = HevcHwReplayJournal::default();
        for (id, pts_millis, key) in [(0_u8, 179_900_i64, true), (1_u8, 180_166_i64, false)] {
            let packet = hevc_packet(if key { 0x26 } else { 0x02 }, id, pts_millis, key);
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }

        assert!(
            journal
                .clone_complete(target_nsecs)
                .expect("packet refs")
                .is_none(),
            "preroll has not reached the exact seek target yet"
        );
        assert!(
            journal
                .clone_replayable(target_nsecs, target_nsecs)
                .expect("packet refs")
                .is_none(),
            "a safe anchor without high-water coverage must fall back to cached seek"
        );
    }

    #[test]
    fn problem_trace_29_recovery_replays_safe_idr_one_frame_after_frozen_target() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let target_nsecs = 1_080_633_000_000;
        let required_high_water_nsecs = 1_082_066_000_000;
        let mut journal = HevcHwReplayJournal::default();
        let next_safe_idr = hevc_packet(0x26, 0, 1_080_666, true);
        let covered_tail = hevc_packet(0x02, 1, 1_082_100, false);

        for packet in [&next_safe_idr, &covered_tail] {
            assert!(
                journal
                    .remember(packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("recovery journal packet refs")
            );
        }

        assert!(
            journal
                .clone_complete(target_nsecs)
                .expect("exact-seek journal refs")
                .is_none(),
            "exact seek must not claim coverage before the journal anchor"
        );
        assert_eq!(
            journal
                .clone_replayable(target_nsecs, required_high_water_nsecs)
                .expect("recovery journal refs")
                .expect("the next-frame IDR is a bounded recovery boundary")
                .len(),
            2
        );
    }

    #[test]
    fn hevc_hw_replay_rejects_forward_anchor_beyond_recoverable_gap() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let target_nsecs = 1_000_000_000_u64;
        let mut journal = HevcHwReplayJournal::default();
        let late_idr_millis = i64::try_from(
            target_nsecs.saturating_add(HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS) / 1_000_000 + 2,
        )
        .expect("timestamp fits");
        let late_idr = hevc_packet(0x26, 0, late_idr_millis, true);

        assert!(
            journal
                .remember(&late_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("late IDR packet refs")
        );
        assert!(
            journal
                .clone_replayable(target_nsecs, target_nsecs)
                .expect("recovery journal refs")
                .is_none(),
            "recovery must not skip an unbounded interval to a later IDR"
        );
    }

    #[test]
    fn problem_trace_20_00_retains_high_bitrate_replay_through_recovery_cutoff() {
        const LEGACY_BYTE_LIMIT: usize = 32 * 1024 * 1024;
        const OBSERVED_PREFIX_BYTES: usize = 33_409_248;
        const PROJECTED_CUTOFF_TAIL_BYTES: usize = 4 * 1024 * 1024;

        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let target_nsecs = 1_200_166_000_000;
        let required_high_water_nsecs = 1_201_066_000_000;
        let mut journal = HevcHwReplayJournal::default();
        let anchor = hevc_packet(0x26, 0, 1_192_133, true);
        let covered_tail = hevc_packet(0x02, 1, 1_201_100, false);

        assert!(
            journal
                .remember(&anchor, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("anchor packet refs")
        );
        let projected_cutoff_bytes =
            OBSERVED_PREFIX_BYTES.saturating_add(PROJECTED_CUTOFF_TAIL_BYTES);
        assert!(projected_cutoff_bytes > LEGACY_BYTE_LIMIT);
        journal.total_bytes = projected_cutoff_bytes.saturating_sub(covered_tail.byte_len());
        assert!(
            journal
                .remember(&covered_tail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
                .expect("cutoff packet refs"),
            "the byte budget must preserve the complete bounded recovery interval"
        );
        assert_eq!(journal.total_bytes, projected_cutoff_bytes);
        assert!(
            journal
                .clone_replayable(target_nsecs, required_high_water_nsecs)
                .expect("recovery journal refs")
                .is_some(),
            "the 20:00 recovery should replay instead of rebuilding from the 1194.866s cached IDR"
        );
    }

    #[test]
    fn hevc_hw_replay_retains_problem_7_233_second_idr_interval() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let idr = hevc_packet(0x26, 0, 683_400, true);
        let recovery_packet = hevc_packet(0x02, 1, 690_633, false);

        assert!(
            journal
                .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("IDR packet refs")
        );
        assert!(
            journal
                .remember(
                    &recovery_packet,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("recovery packet refs")
        );
        assert_eq!(journal.len(), 2);
        assert!(
            journal
                .clone_replayable(690_633_000_000, 690_633_000_000)
                .expect("journal packet refs")
                .is_some()
        );
    }

    #[test]
    fn hevc_hw_replay_locks_previous_safe_idr_until_recovery_starts() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let previous_idr_millis = 677_866_i64;
        let next_idr_millis = 690_633_i64;
        let previous_idr = hevc_packet(0x26, 0, previous_idr_millis, true);
        assert!(
            journal
                .remember(&previous_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
                .expect("previous IDR packet refs")
        );

        for packet_index in 1..300_i64 {
            let pts_millis =
                previous_idr_millis + (next_idr_millis - previous_idr_millis) * packet_index / 300;
            let packet = hevc_packet(0x02, packet_index as u8, pts_millis, false);
            assert!(
                journal
                    .remember_preserving_safe_anchor(
                        &packet,
                        ffi::AVCodecID::AV_CODEC_ID_HEVC,
                        time_base,
                    )
                    .expect("preroll packet refs")
            );
        }
        let next_idr = hevc_packet(0x26, 255, next_idr_millis, true);
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &next_idr,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("next IDR packet refs")
        );

        assert_eq!(journal.anchor_nsecs, Some(677_866_000_000));
        assert_eq!(journal.len(), 301);
        assert!(
            journal
                .clone_replayable(683_400_000_000, 690_633_000_000)
                .expect("locked journal packet refs")
                .is_some(),
            "the newer 690.633s IDR must not replace the safe anchor before recovery"
        );
    }

    #[test]
    fn reopen_replay_includes_live_packets_consumed_after_flush_replay() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let anchor_millis = 677_866_i64;
        let target_nsecs = 681_266_000_000_u64;
        let now = Instant::now();
        let mut journal = HevcHwReplayJournal::default();

        for packet_index in 0..217_i64 {
            let pts_millis = anchor_millis + packet_index * 33;
            let packet = hevc_packet(
                if packet_index == 0 { 0x26 } else { 0x02 },
                packet_index as u8,
                pts_millis,
                packet_index == 0,
            );
            assert!(
                journal
                    .remember_preserving_safe_anchor(
                        &packet,
                        ffi::AVCodecID::AV_CODEC_ID_HEVC,
                        time_base,
                    )
                    .expect("flush journal packet refs")
            );
        }
        let flush_cutoff_nsecs = u64::try_from(anchor_millis + 216 * 33)
            .expect("positive cutoff")
            .saturating_mul(1_000_000);
        let first_replay = journal
            .clone_replayable(target_nsecs, flush_cutoff_nsecs)
            .expect("first replay refs")
            .expect("flush replay coverage");
        assert_eq!(first_replay.len(), 217);

        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.set_root_evidence(111, Some(flush_cutoff_nsecs), Some(target_nsecs));
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
        transaction.observe_packet(10, Some(flush_cutoff_nsecs), 1);

        let mut reopen_cutoff_nsecs = flush_cutoff_nsecs;
        for packet_index in 217..237_i64 {
            let pts_millis = anchor_millis + packet_index * 33;
            reopen_cutoff_nsecs = u64::try_from(pts_millis)
                .expect("positive packet timestamp")
                .saturating_mul(1_000_000);
            let packet = hevc_packet(0x02, packet_index as u8, pts_millis, false);
            assert!(
                journal
                    .remember_preserving_safe_anchor(
                        &packet,
                        ffi::AVCodecID::AV_CODEC_ID_HEVC,
                        time_base,
                    )
                    .expect("live packet refs")
            );
            transaction.observe_packet(10, Some(reopen_cutoff_nsecs), 0);
        }
        assert_eq!(
            transaction.advance_after_attempt_failure(
                "flush replay exhausted",
                now + Duration::from_secs(1),
                HardwareDecodeMode::Auto,
            ),
            HevcDecodeRecoveryAction::ReopenSameHardware
        );
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(reopen_cutoff_nsecs)
        );

        let reopen_replay = journal
            .clone_replayable(
                target_nsecs,
                transaction
                    .replay_required_high_water_nsecs
                    .expect("reopen cutoff"),
            )
            .expect("second replay refs")
            .expect("reopen replay covers live cutoff");
        assert_eq!(reopen_replay.len(), 237);
        assert!(reopen_replay.len() > first_replay.len());
        assert_eq!(journal.anchor_nsecs, Some(677_866_000_000));
        assert_eq!(journal.high_water_nsecs, Some(reopen_cutoff_nsecs));
    }

    #[test]
    fn hevc_hw_replay_preserves_demux_order_and_uses_fresh_generations() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        for id in 0..8_u8 {
            let packet = hevc_packet(
                if id == 0 { 0x26 } else { 0x02 },
                id,
                i64::from(id) * 40,
                id == 0,
            );
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }

        let mut playback_generation = PlaybackGeneration::default();
        let replay = hevc_hw_replay_packets(
            journal
                .clone_complete(160_000_000)
                .expect("packet refs")
                .expect("safe journal covering target"),
            &mut playback_generation,
        );

        assert_eq!(journal.len(), 8, "replay must preserve the source journal");
        assert_eq!(
            journal
                .clone_complete(160_000_000)
                .expect("packet refs")
                .expect("journal remains reusable")
                .len(),
            8
        );
        assert_eq!(replay.len(), 8);
        for (index, pending) in replay.iter().enumerate() {
            assert_eq!(pending.generation, index as u64 + 1);
            assert_eq!(
                pending.packet.data().and_then(|data| data.last()),
                Some(&(index as u8))
            );
            assert!(pending.realign_after_decode_recovery);
            assert!(!pending.hevc_startup_in_flight_watchdog);
            assert!(pending.from_hevc_hw_replay);
            assert!(pending.hevc_decode_recovery_evidence_scoped);
        }
    }

    #[test]
    fn hevc_hw_replay_stays_ahead_of_live_packets_after_backpressure() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        for id in 0..3_u8 {
            let packet = hevc_packet(
                if id == 0 { 0x26 } else { 0x02 },
                id,
                i64::from(id) * 40,
                id == 0,
            );
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }

        let mut generation = PlaybackGeneration::default();
        let mut replay = hevc_hw_replay_packets(
            journal
                .clone_complete(40_000_000)
                .expect("packet refs")
                .expect("safe journal covering target"),
            &mut generation,
        );
        let mut regular = VideoDecodePacketQueues::default();
        assert!(
            regular
                .push_pending_input(PendingVideoDecodePacket {
                    generation: generation.advance(),
                    packet: hevc_packet(0x02, 9, 120, false),
                    realign_after_decode_recovery: true,
                    hevc_startup_in_flight_watchdog: false,
                    from_hevc_hw_replay: false,
                    hevc_decode_recovery_evidence_scoped: false,
                })
                .is_ok()
        );

        let blocked_replay =
            take_next_video_decode_input(&mut regular, &mut replay).expect("first replay packet");
        assert!(blocked_replay.from_hevc_hw_replay);
        requeue_backpressured_video_decode_input(&mut regular, &mut replay, blocked_replay);

        let mut ids = Vec::new();
        while let Some(packet) = take_next_video_decode_input(&mut regular, &mut replay) {
            ids.push(
                *packet
                    .packet
                    .data()
                    .and_then(|data| data.last())
                    .expect("packet id"),
            );
        }
        assert_eq!(ids, vec![0, 1, 2, 9]);
    }

    #[test]
    fn hevc_hw_replay_journal_invalidates_entire_gop_after_duration_limit() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let idr = hevc_packet(0x26, 0, 0, true);
        assert!(
            journal
                .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        let beyond_limit = hevc_packet(
            0x02,
            1,
            i64::try_from(HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS / 1_000_000 + 1)
                .expect("duration fits"),
            false,
        );
        assert!(
            !journal
                .remember(&beyond_limit, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 0);

        let tail = hevc_packet(0x02, 2, 6_040, false);
        assert!(
            !journal
                .remember(&tail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert!(journal.clone_complete(0).expect("packet refs").is_none());
    }

    #[test]
    fn frozen_resource_pressure_cutoff_keeps_completed_safe_idr_prefix_replayable() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        let idr = hevc_packet(0x26, 0, 0, true);
        assert!(
            journal
                .remember_preserving_safe_anchor(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
                .expect("IDR packet refs")
        );
        let covered = hevc_packet(0x02, 1, 9_000, false);
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &covered,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("covered packet refs")
        );
        let beyond_limit = hevc_packet(
            0x02,
            2,
            i64::try_from(HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS / 1_000_000 + 1)
                .expect("duration fits"),
            false,
        );
        assert!(
            !journal
                .remember_preserving_safe_anchor(
                    &beyond_limit,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("bounded packet refs")
        );
        assert!(journal.coverage_exhausted);
        assert!(journal.coverage_contiguous);
        assert!(
            journal
                .clone_replayable(1_000_000_000, 9_000_000_000)
                .expect("frozen journal refs")
                .is_some(),
            "a later limit must not poison the already-covered frozen cutoff"
        );
        assert!(
            journal
                .clone_replayable(1_000_000_000, 14_000_000_000)
                .expect("incomplete cutoff check")
                .is_none(),
            "the completed prefix must not claim coverage it never recorded"
        );
    }

    #[test]
    fn hevc_drain_grace_counts_only_video_worker_results() {
        let before = VideoDecodeWorkerSnapshot {
            result_produced_sequence: 41,
            result_consumed_sequence: 41,
            ..VideoDecodeWorkerSnapshot::default()
        };
        assert!(
            !hevc_drain_video_result_progressed(before, before),
            "audio/output activity cannot extend the video decoder drain grace"
        );

        let produced = VideoDecodeWorkerSnapshot {
            result_produced_sequence: 42,
            ..before
        };
        assert!(hevc_drain_video_result_progressed(before, produced));

        let consumed = VideoDecodeWorkerSnapshot {
            result_consumed_sequence: 42,
            ..before
        };
        assert!(hevc_drain_video_result_progressed(before, consumed));
    }

    #[test]
    fn hevc_hw_replay_requires_safe_anchor_interval_to_cover_target() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        for (id, pts_millis) in [(0_u8, 1_000_i64), (1, 1_040)] {
            let packet = hevc_packet(if id == 0 { 0x26 } else { 0x02 }, id, pts_millis, id == 0);
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }
        assert!(
            journal
                .clone_complete(900_000_000)
                .expect("packet refs")
                .is_none()
        );

        for (id, pts_millis) in [(0_u8, 0_i64), (1, 40)] {
            let packet = hevc_packet(if id == 0 { 0x26 } else { 0x02 }, id, pts_millis, id == 0);
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }
        assert!(
            journal
                .clone_complete(100_000_000)
                .expect("packet refs")
                .is_none()
        );
    }

    #[test]
    fn hevc_hw_replay_journal_invalidates_on_packet_or_byte_limit() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut journal = HevcHwReplayJournal::default();
        for id in 0..HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS {
            let packet = hevc_packet(
                if id == 0 { 0x26 } else { 0x02 },
                id as u8,
                i64::try_from(id).expect("packet index fits"),
                id == 0,
            );
            assert!(
                journal
                    .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                    .expect("packet refs")
            );
        }
        let overflow = hevc_packet(0x02, 0xff, 300, false);
        assert!(
            !journal
                .remember(&overflow, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 0);

        let idr = hevc_packet(0x26, 0, 0, true);
        assert!(
            journal
                .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        journal.total_bytes = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES;
        assert!(
            !journal
                .remember(&overflow, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
        assert_eq!(journal.len(), 0);
    }

    #[test]
    fn hevc_hw_replay_reports_matching_pending_capacity() {
        assert_eq!(
            video_decode_pending_input_snapshot(0, HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS),
            (
                HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
                HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS
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
    fn force_vulkan_runtime_failure_does_not_silently_switch_to_software() {
        assert!(runtime_hevc_software_fallback_allowed(
            HardwareDecodeMode::Auto
        ));
        assert!(!runtime_hevc_software_fallback_allowed(
            HardwareDecodeMode::ForceVulkan
        ));
        assert!(!runtime_hevc_software_fallback_allowed(
            HardwareDecodeMode::Off
        ));
        assert_eq!(
            hevc_same_hardware_reopen_mode(),
            HardwareDecodeMode::ForceVulkan
        );
    }

    #[test]
    fn resource_pressure_transaction_freezes_cutoff_and_skips_idr_drain_scan() {
        let now = Instant::now();
        let target_nsecs = 716_633_333_333;
        let cutoff_nsecs = 724_100_000_000;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ResourcePressure,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(
            fallback,
            10,
            Some("Cannot allocate memory".to_string()),
            now,
        );
        transaction.set_root_evidence(0, Some(cutoff_nsecs), Some(target_nsecs));

        assert!(transaction.resource_pressure());
        assert!(transaction.resource_pressure_demux_admission_stopped());
        assert!(transaction.resource_pressure_decoder_input_stopped());
        assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Flushing);
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::FlushSameHardware
        );
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(cutoff_nsecs)
        );
        assert!(transaction.claim_resource_pressure_external_release(7));
        assert!(
            !transaction.claim_resource_pressure_external_release(7),
            "repeated OOM results from one decoder epoch must not restart external release"
        );
        assert!(
            transaction.claim_resource_pressure_external_release(8),
            "a flush/reopen epoch may own a fresh bounded set of Vulkan frames"
        );

        transaction.record_resource_pressure_error(
            "Cannot allocate memory",
            Some(790_166_666_666),
            now + Duration::from_millis(1),
        );
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(cutoff_nsecs),
            "later failed packets must not advance the frozen recovery cutoff"
        );
        assert_eq!(transaction.target_nsecs, target_nsecs);

        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        assert!(
            transaction.resource_pressure_demux_admission_stopped(),
            "future demux packets remain frozen throughout resource-pressure replay"
        );
        assert!(
            !transaction.resource_pressure_decoder_input_stopped(),
            "the already-bounded journal must remain replayable"
        );

        let ordinary = HevcSameHardwareRecoveryTransaction::new(
            HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            },
            10,
            None,
            now,
        );
        assert!(
            transaction.resource_pressure(),
            "OOM reopen must release first"
        );
        assert!(
            !ordinary.resource_pressure(),
            "ordinary corruption recovery keeps the atomic open-first swap"
        );
        assert!(!ordinary.resource_pressure_demux_admission_stopped());
        assert!(!ordinary.resource_pressure_decoder_input_stopped());
    }

    #[test]
    fn first_oom_preempts_active_ordinary_recovery_and_freezes_its_boundary() {
        let now = Instant::now();
        let ordinary_target_nsecs = 681_266_667_000;
        let oom_target_nsecs = 716_633_333_333;
        let oom_cutoff_nsecs = 724_100_000_000;
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(
            HevcDecodeChainFallback {
                target_nsecs: ordinary_target_nsecs,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            },
            10,
            None,
            now,
        );
        transaction.set_root_evidence(111, Some(690_633_333_333), Some(ordinary_target_nsecs));
        transaction.flush_attempts = HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        let attempt_id =
            transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 20, now);
        transaction.observe_packet(20, Some(690_633_333_333), 0);

        transaction.promote_to_resource_pressure(
            oom_target_nsecs,
            Some(oom_cutoff_nsecs),
            "Cannot allocate memory",
            now + Duration::from_millis(1),
        );
        if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
        } else {
            transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
        }

        assert!(transaction.resource_pressure());
        assert_eq!(
            transaction.reason,
            HevcDecodeChainFallbackReason::ResourcePressure
        );
        assert_eq!(transaction.target_nsecs, oom_target_nsecs);
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(oom_cutoff_nsecs)
        );
        assert_eq!(transaction.root_zero_output_packets, 0);
        assert_eq!(
            transaction.root_input_high_water_nsecs,
            Some(oom_cutoff_nsecs)
        );
        assert_eq!(
            transaction.root_output_high_water_nsecs,
            Some(oom_target_nsecs)
        );
        assert_eq!(transaction.attempt_ledger.len(), 1);
        assert_eq!(transaction.attempt_ledger[0].attempt_id, attempt_id);
        assert_eq!(
            transaction.attempt_ledger[0].outcome,
            "preempted_by_resource_pressure"
        );
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::ReopenSameHardware,
            "an OOM after the flush attempt must select release-first reopen"
        );

        transaction.promote_to_resource_pressure(
            790_166_666_666,
            Some(790_200_000_000),
            "Cannot allocate memory again",
            now + Duration::from_millis(2),
        );
        assert_eq!(transaction.target_nsecs, oom_target_nsecs);
        assert_eq!(transaction.observed_target_nsecs, oom_target_nsecs);
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(oom_cutoff_nsecs),
            "later OOM packets must not move the first OOM recovery cutoff"
        );
        assert_eq!(transaction.attempt_ledger.len(), 1);
        assert_eq!(transaction.resource_pressure_errors, 2);
    }

    #[test]
    fn repeated_oom_diagnostics_are_aggregated_once_per_second() {
        let now = Instant::now();
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(
            HevcDecodeChainFallback {
                target_nsecs: 716_633_333_333,
                reason: HevcDecodeChainFallbackReason::ResourcePressure,
            },
            0,
            Some("Cannot allocate memory".to_string()),
            now,
        );

        transaction.record_resource_pressure_error(
            "Cannot allocate memory",
            Some(724_100_000_000),
            now,
        );
        assert_eq!(transaction.resource_pressure_errors, 1);
        assert_eq!(transaction.last_resource_pressure_log_at, Some(now));
        assert_eq!(transaction.suppressed_resource_pressure_errors, 0);

        transaction.record_resource_pressure_error(
            "Cannot allocate memory",
            Some(724_133_333_333),
            now + Duration::from_millis(1),
        );
        assert_eq!(transaction.resource_pressure_errors, 2);
        assert_eq!(transaction.last_resource_pressure_log_at, Some(now));
        assert_eq!(transaction.suppressed_resource_pressure_errors, 1);

        let summary_at = now + Duration::from_secs(1);
        transaction.record_resource_pressure_error(
            "Cannot allocate memory",
            Some(724_166_666_666),
            summary_at,
        );
        assert_eq!(transaction.resource_pressure_errors, 3);
        assert_eq!(transaction.last_resource_pressure_log_at, Some(summary_at));
        assert_eq!(transaction.suppressed_resource_pressure_errors, 0);
    }

    #[test]
    fn repeated_failure_after_one_millisecond_does_not_advance_flush_attempt() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 184_692_319_900,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);

        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::DrainPendingResults
        );
        transaction.drain_recorded = true;
        transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::FlushSameHardware
        );
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                5,
                now + Duration::from_millis(1),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::None,
            "a repeated fallback before the one-second admitted-progress deadline is merged"
        );
        assert_eq!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
        );
        assert_eq!(transaction.last_result_produced_sequence, 5);
    }

    #[test]
    fn root_failure_count_does_not_fail_attempt_with_current_epoch_progress() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 681_266_667_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
        transaction.set_root_evidence(111, Some(690_633_333_333), Some(680_900_000_000));
        transaction.flush_attempts = 1;
        transaction.reopen_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            20,
            now,
        );

        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(1),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation: 20,
                    frame_timeline_nsecs: 681_266_667_000,
                    frame_duration_nsecs: 40_000_000,
                    current_start_position_nsecs: 681_266_667_000,
                    before_queue_end_nsecs: Some(681_266_667_000),
                    after_queue_end_nsecs: Some(681_826_667_000),
                },
                now,
            ),
            HevcAdmittedVideoProgress::Partial,
            "a fresh Vulkan decoder needs sustained progress before root evidence is cleared"
        );
        transaction.observe_packet(20, Some(681_860_000_000), 0);
        let attempt = transaction.active_attempt.as_ref().expect("active attempt");
        assert_eq!(transaction.root_zero_output_packets, 111);
        assert_eq!(attempt.consecutive_zero_output_packets, 1);
        assert_eq!(attempt.input_high_water_nsecs, Some(681_860_000_000));
        assert_eq!(attempt.admitted_span_after_catch_up_nsecs, 560_000_000);

        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                4,
                now + Duration::from_millis(1),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::None,
            "root evidence must not be reused as the current attempt counter"
        );
        assert_eq!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
        );
    }

    #[test]
    fn failed_cached_rebuild_drains_pending_decoder_work_before_terminal_action() {
        let now = Instant::now();
        let target_nsecs = 694_233_333_333;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 2_570, None, now);
        transaction.flush_attempts = 1;
        transaction.reopen_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            3_736,
            now,
        );
        transaction
            .active_attempt
            .as_mut()
            .expect("cached rebuild attempt")
            .hard_failure = Some("attempt packet lead reached one second");

        let pending = VideoDecodeWorkerSnapshot {
            state: VideoDecodeWorkerState::Decoding,
            submitted_not_consumed_packets: 1,
            completed_packets: 9,
            ..Default::default()
        };
        assert!(transaction.failed_attempt_needs_decoder_drain(pending, now));

        let drained = VideoDecodeWorkerSnapshot {
            state: VideoDecodeWorkerState::NeedPacket,
            ..Default::default()
        };
        assert!(!transaction.failed_attempt_needs_decoder_drain(drained, now));
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                2_570,
                now,
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::FailExplicitly,
            "ForceVulkan may fail only after pending decoder work is drained"
        );
    }

    #[test]
    fn cached_safe_idr_rebuild_scans_past_ordinary_zero_output_limit_for_next_idr() {
        let now = Instant::now();
        let target_nsecs = 459_500_000_000;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            30,
            now,
        );

        for packet in 1..=60 {
            transaction.observe_packet(30, Some(target_nsecs + packet * 33_333_333), 0);
        }

        let attempt = transaction.active_attempt.as_ref().expect("cached rebuild");
        assert_eq!(attempt.consecutive_zero_output_packets, 60);
        assert_eq!(attempt.hard_failure, None);
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                0,
                now + Duration::from_millis(10),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::None,
            "a final cached rebuild must be allowed to reach the next IDR"
        );

        transaction.observe_packet(30, Some(target_nsecs + 2_033_333_333), 1);
        let attempt = transaction.active_attempt.as_ref().expect("cached rebuild");
        assert_eq!(attempt.consecutive_zero_output_packets, 0);
        assert_eq!(attempt.hard_failure, None);
    }

    #[test]
    fn cached_safe_idr_rebuild_keeps_a_bounded_five_second_packet_lead() {
        let now = Instant::now();
        let target_nsecs = 459_500_000_000;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            30,
            now,
        );
        transaction.observe_packet(
            30,
            Some(
                target_nsecs
                    .saturating_add(HEVC_SAME_HARDWARE_CACHED_REBUILD_MAX_PACKET_LEAD_NSECS),
            ),
            0,
        );

        assert_eq!(
            transaction
                .active_attempt
                .as_ref()
                .expect("cached rebuild")
                .hard_failure,
            Some("cached rebuild packet lead reached five seconds")
        );
    }

    #[test]
    fn failed_replay_does_not_wait_for_wrapper_owned_unsent_packets() {
        let now = Instant::now();
        let target_nsecs = 50_500_000_000;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 2_405, None, now);
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(
            2,
            HevcSameHardwareRecoveryAttemptKind::FlushReplay,
            3_727,
            now,
        );
        transaction
            .active_attempt
            .as_mut()
            .expect("flush replay attempt")
            .hard_failure = Some("attempt packet lead reached one second");

        let wrapper_only_pending = VideoDecodeWorkerSnapshot {
            state: VideoDecodeWorkerState::NeedPacket,
            pending_input_packets: 32,
            pending_input_capacity: 8,
            ..Default::default()
        };
        assert!(wrapper_only_pending.pending_input_full());
        assert!(
            !transaction.failed_attempt_needs_decoder_drain(wrapper_only_pending, now),
            "unsent wrapper packets are cleared by flush/reopen and cannot keep recovery draining"
        );
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                2_405,
                now,
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::ReopenSameHardware
        );
    }

    #[test]
    fn delayed_drain_output_covering_target_cancels_destructive_recovery() {
        let now = Instant::now();
        let target_nsecs = 645_866_666_666;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 1_943, None, now);
        transaction.set_root_evidence(29, Some(647_266_666_666), Some(target_nsecs));

        let progress = transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(4),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 2_855,
                frame_timeline_nsecs: target_nsecs.saturating_add(1),
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: target_nsecs,
                before_queue_end_nsecs: Some(target_nsecs),
                after_queue_end_nsecs: Some(target_nsecs.saturating_add(33_333_334)),
            },
            now + Duration::from_millis(1),
        );

        assert_eq!(progress, HevcAdmittedVideoProgress::Stable);
        assert!(transaction.drain_recorded);
        assert_eq!(transaction.flush_attempts, 0);
        assert_eq!(transaction.reopen_attempts, 0);
        assert_eq!(
            transaction.root_output_high_water_nsecs,
            Some(target_nsecs.saturating_add(33_333_334))
        );
    }

    #[test]
    fn discontinuous_future_drain_output_does_not_cancel_recovery() {
        let now = Instant::now();
        let target_nsecs = 645_866_666_666;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 1_943, None, now);

        let progress = transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(4),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 2_855,
                frame_timeline_nsecs: target_nsecs.saturating_add(1),
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: target_nsecs,
                before_queue_end_nsecs: Some(target_nsecs.saturating_sub(1_000_000_000)),
                after_queue_end_nsecs: Some(target_nsecs.saturating_add(33_333_334)),
            },
            now + Duration::from_millis(1),
        );

        assert_eq!(progress, HevcAdmittedVideoProgress::None);
        assert!(!transaction.drain_recorded);
        assert_eq!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::DrainingResults
        );
    }

    #[test]
    fn current_epoch_uses_fixed_catch_up_barrier_before_stable_progress() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.set_root_evidence(111, Some(1_000_000_000), Some(960_000_000));
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(8, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 20, now);
        transaction.observe_packet(20, Some(1_000_000_000), 1);

        let old_epoch_progress = HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 19,
            frame_timeline_nsecs: 1_000_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_000_000_000),
            after_queue_end_nsecs: Some(1_499_000_000),
        };
        assert_eq!(
            transaction.observe_admitted_video_progress(old_epoch_progress, now),
            HevcAdmittedVideoProgress::None
        );

        let current_epoch_progress = HevcAdmittedVideoProgressObservation {
            generation: 20,
            ..old_epoch_progress
        };
        assert_eq!(
            transaction.observe_admitted_video_progress(current_epoch_progress, now),
            HevcAdmittedVideoProgress::Partial
        );
        transaction.observe_packet(20, Some(2_000_000_000), 0);
        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    frame_timeline_nsecs: 1_499_000_000,
                    before_queue_end_nsecs: Some(1_499_000_000),
                    after_queue_end_nsecs: Some(1_500_000_000),
                    ..current_epoch_progress
                },
                now + Duration::from_millis(1),
            ),
            HevcAdmittedVideoProgress::Partial,
            "new input must not move the barrier frozen at first recovered output"
        );
        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    frame_timeline_nsecs: 1_500_000_000,
                    before_queue_end_nsecs: Some(1_500_000_000),
                    after_queue_end_nsecs: Some(3_000_000_000),
                    ..current_epoch_progress
                },
                now + Duration::from_millis(2),
            ),
            HevcAdmittedVideoProgress::Stable,
            "same-decoder flush recovery needs two seconds of contiguous admitted progress"
        );
        let attempt = transaction.active_attempt.as_ref().expect("active attempt");
        assert_eq!(attempt.catch_up_barrier_nsecs, Some(1_000_000_000));
        assert_eq!(attempt.input_high_water_nsecs, Some(2_000_000_000));
        assert_eq!(attempt.admitted_span_after_catch_up_nsecs, 2_000_000_000);
        assert_eq!(transaction.root_zero_output_packets, 111);
    }

    #[test]
    fn problem_trace_28_short_flush_recovery_escalates_instead_of_restarting_loop() {
        let now = Instant::now();
        let target_nsecs = 1_081_499_988_889;
        let generation = 12_273;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 7_478, None, now);
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::FlushReplay,
            generation,
            now,
        );
        transaction.observe_packet(generation, Some(target_nsecs), 1);
        transaction
            .active_attempt
            .as_mut()
            .expect("flush replay attempt")
            .output_commit_observed = true;

        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(28),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation,
                    frame_timeline_nsecs: target_nsecs,
                    frame_duration_nsecs: 33_333_333,
                    current_start_position_nsecs: 1_080_633_322_222,
                    before_queue_end_nsecs: Some(target_nsecs),
                    after_queue_end_nsecs: Some(target_nsecs.saturating_add(533_333_328)),
                },
                now + Duration::from_millis(46),
            ),
            HevcAdmittedVideoProgress::Partial,
            "the 16-frame replay burst must not complete the same-decoder transaction"
        );

        for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
            transaction.observe_packet(
                generation,
                Some(
                    target_nsecs
                        .saturating_add(533_333_328)
                        .saturating_add(packet_index.saturating_mul(33_333_333)),
                ),
                0,
            );
        }
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                7_478,
                now + Duration::from_millis(150),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::ReopenSameHardware,
            "recurrence must reopen Vulkan instead of launching another flush replay"
        );
        assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);
    }

    #[test]
    fn problem_trace_29_short_vulkan_prefix_escalates_to_bounded_cached_rebuild() {
        let now = Instant::now();
        let target_nsecs = 1_146_366_655_555;
        let generation = 6_722;
        let fallback = HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 308, None, now);
        transaction.flush_attempts = 1;
        transaction.reopen_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            5,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            generation,
            now,
        );
        transaction.observe_packet(generation, Some(target_nsecs), 1);
        transaction
            .active_attempt
            .as_mut()
            .expect("Vulkan reopen attempt")
            .output_commit_observed = true;

        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(15),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation,
                    frame_timeline_nsecs: target_nsecs,
                    frame_duration_nsecs: 33_333_333,
                    current_start_position_nsecs: 1_144_766_655_556,
                    before_queue_end_nsecs: Some(target_nsecs),
                    after_queue_end_nsecs: Some(target_nsecs.saturating_add(499_999_995)),
                },
                now + Duration::from_millis(266),
            ),
            HevcAdmittedVideoProgress::Partial,
            "the reopened decoder's 500ms prefix must not complete the transaction"
        );

        for packet_index in 1..=HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
            transaction.observe_packet(
                generation,
                Some(
                    target_nsecs
                        .saturating_add(499_999_995)
                        .saturating_add(packet_index.saturating_mul(33_333_333)),
                ),
                0,
            );
        }
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                308,
                now + Duration::from_millis(430),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::RebuildFromCachedSeek,
            "ForceVulkan must use the final authoritative safe-IDR rebuild"
        );
        assert_eq!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::RebuildingFromCache
        );
        assert_eq!(
            transaction
                .attempt_ledger
                .last()
                .expect("Vulkan reopen ledger")
                .outcome,
            "escalated_to_cache_rebuild"
        );

        let rebuild_generation = generation + 100;
        transaction
            .begin_cached_rebuild(5, rebuild_generation, now + Duration::from_millis(500))
            .expect("one cached rebuild is allowed");
        let damaged_prefix_start_nsecs = target_nsecs.saturating_add(733_333_326);
        let damaged_prefix_end_nsecs = damaged_prefix_start_nsecs.saturating_add(499_999_995);
        transaction.observe_packet(rebuild_generation, Some(damaged_prefix_start_nsecs), 1);
        transaction
            .active_attempt
            .as_mut()
            .expect("cached rebuild attempt")
            .output_commit_observed = true;
        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(15),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation: rebuild_generation,
                    frame_timeline_nsecs: damaged_prefix_start_nsecs,
                    frame_duration_nsecs: 33_333_333,
                    current_start_position_nsecs: target_nsecs,
                    before_queue_end_nsecs: Some(target_nsecs),
                    after_queue_end_nsecs: Some(damaged_prefix_end_nsecs),
                },
                now + Duration::from_millis(700),
            ),
            HevcAdmittedVideoProgress::Partial,
            "the cached rebuild must remain armed across the damaged GOP"
        );

        for packet_index in 1..=120_u64 {
            transaction.observe_packet(
                rebuild_generation,
                Some(
                    damaged_prefix_end_nsecs
                        .saturating_add(packet_index.saturating_mul(33_333_333)),
                ),
                0,
            );
        }
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                308,
                now + Duration::from_secs(4),
                HardwareDecodeMode::ForceVulkan,
            ),
            HevcDecodeRecoveryAction::None,
            "the final cached rebuild must wait for the next IDR instead of restarting"
        );

        let next_idr_nsecs = 1_152_433_322_222;
        transaction.observe_packet(rebuild_generation, Some(next_idr_nsecs), 1);
        assert_eq!(
            transaction.observe_admitted_video_progress(
                HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(15),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation: rebuild_generation,
                    frame_timeline_nsecs: next_idr_nsecs,
                    frame_duration_nsecs: 33_333_333,
                    current_start_position_nsecs: target_nsecs,
                    before_queue_end_nsecs: Some(damaged_prefix_end_nsecs),
                    after_queue_end_nsecs: Some(next_idr_nsecs.saturating_add(2_000_000_000)),
                },
                now + Duration::from_secs(5),
            ),
            HevcAdmittedVideoProgress::Stable,
            "two clean seconds after the next IDR prove recovery"
        );
    }

    #[test]
    fn problem_trace_19_output_commit_accepts_one_frame_boundary_tolerance() {
        let now = Instant::now();
        let generation = 23_478;
        let target_nsecs = 1_196_199_988_759;
        let catch_up_barrier_nsecs = 1_200_599_988_888;
        let recovered_end_nsecs = 1_202_566_655_555;
        let observation = HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(19),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation,
            frame_timeline_nsecs: catch_up_barrier_nsecs,
            frame_duration_nsecs: 33_333_333,
            current_start_position_nsecs: target_nsecs,
            before_queue_end_nsecs: Some(catch_up_barrier_nsecs),
            after_queue_end_nsecs: Some(recovered_end_nsecs),
        };

        let mut speculative = HevcSameHardwareRecoveryAttempt::new(
            3,
            11,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            generation,
            target_nsecs,
            now,
        );
        speculative.input_high_water_nsecs = Some(catch_up_barrier_nsecs);
        assert_eq!(
            speculative.observe_admitted_video_progress(observation, now),
            HevcAdmittedVideoProgress::Partial,
            "speculative recovery must retain the full two-second stability requirement"
        );

        let mut committed = HevcSameHardwareRecoveryAttempt::new(
            3,
            11,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            generation,
            target_nsecs,
            now,
        );
        committed.input_high_water_nsecs = Some(catch_up_barrier_nsecs);
        committed.output_commit_observed = true;
        assert_eq!(
            committed.observe_admitted_video_progress(observation, now),
            HevcAdmittedVideoProgress::Stable,
            "an atomically committed 30fps window one frame below two seconds must not fail later under normal VO backpressure"
        );
        assert_eq!(committed.admitted_span_after_catch_up_nsecs, 1_966_666_667);
    }

    #[test]
    fn completed_attempt_cannot_shrink_required_replay_cutoff() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 681_266_667_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let root_cutoff_nsecs = 690_633_333_333;
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.set_root_evidence(111, Some(root_cutoff_nsecs), Some(681_000_000_000));
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
        transaction.observe_packet(10, Some(683_000_000_000), 0);

        assert_eq!(
            transaction.advance_after_attempt_failure(
                "attempt ended before root cutoff",
                now + Duration::from_millis(1),
                HardwareDecodeMode::Auto,
            ),
            HevcDecodeRecoveryAction::ReopenSameHardware
        );
        assert_eq!(
            transaction.replay_required_high_water_nsecs,
            Some(root_cutoff_nsecs)
        );
        assert_eq!(
            transaction
                .attempt_ledger
                .last()
                .expect("flush attempt ledger")
                .input_high_water_nsecs,
            Some(683_000_000_000)
        );
    }

    #[test]
    fn force_vulkan_orders_flush_reopen_cached_idr_then_explicit_failure() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 716_633_333_333,
            reason: HevcDecodeChainFallbackReason::ResourcePressure,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(
            fallback,
            4,
            Some("Cannot allocate memory".to_string()),
            now,
        );

        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
        transaction.record_replay(0, false, now + Duration::from_millis(1));
        assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::ReopenSameHardware
        );

        transaction.reopen_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            20,
            now + Duration::from_millis(2),
        );
        transaction.record_replay(0, true, now + Duration::from_millis(3));
        assert_eq!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::RebuildingFromCache
        );
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::RebuildFromCachedSeek
        );
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::Auto),
            HevcDecodeRecoveryAction::RequestSoftwareFallback
        );
        assert_eq!(transaction.attempt_ledger.len(), 2);
        assert_eq!(
            transaction.attempt_ledger[0].kind,
            HevcSameHardwareRecoveryAttemptKind::FlushReplay
        );
        assert_eq!(transaction.attempt_ledger[0].outcome, "journal_incomplete");
        assert_eq!(
            transaction.attempt_ledger[1].kind,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay
        );
        assert_eq!(transaction.attempt_ledger[1].outcome, "journal_incomplete");

        transaction
            .begin_cached_rebuild(3, 30, now + Duration::from_millis(4))
            .expect("one cached rebuild is allowed");
        assert_eq!(transaction.cached_rebuild_attempts, 1);
        assert_eq!(
            transaction
                .active_attempt
                .as_ref()
                .expect("cached rebuild attempt")
                .kind,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild
        );
        transaction.fail("cached safe-IDR rebuild failed");
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::ForceVulkan),
            HevcDecodeRecoveryAction::FailExplicitly
        );
        assert_eq!(
            transaction.pending_action(HardwareDecodeMode::Auto),
            HevcDecodeRecoveryAction::RequestSoftwareFallback
        );
        let error = transaction.terminal_error(
            now + Duration::from_millis(5),
            HardwareDecodeMode::ForceVulkan,
        );
        assert!(error.contains("cached_rebuild_attempts=1"));
        assert!(error.contains("kind=cached_safe_idr_rebuild"));
    }

    #[test]
    fn same_hardware_reopen_failure_rebuilds_cache_for_force_and_uses_software_for_auto() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 184_692_319_900,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        };
        let mut forced = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
        forced.flush_attempts = 1;
        forced.reopen_attempts = 1;
        forced.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        forced.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            20,
            now,
        );
        forced
            .active_attempt
            .as_mut()
            .expect("forced attempt")
            .hard_failure = Some("unbridged continuous decode gap");
        assert_eq!(
            forced.advance_after_repeated_failure_if_idle(4, now, HardwareDecodeMode::ForceVulkan,),
            HevcDecodeRecoveryAction::RebuildFromCachedSeek
        );
        assert_eq!(
            forced.phase,
            HevcSameHardwareRecoveryPhase::RebuildingFromCache
        );

        let mut automatic = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
        automatic.flush_attempts = 1;
        automatic.reopen_attempts = 1;
        automatic.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        automatic.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            20,
            now,
        );
        automatic
            .active_attempt
            .as_mut()
            .expect("automatic attempt")
            .hard_failure = Some("unbridged continuous decode gap");
        assert_eq!(
            automatic.advance_after_repeated_failure_if_idle(4, now, HardwareDecodeMode::Auto,),
            HevcDecodeRecoveryAction::RequestSoftwareFallback
        );
        assert_eq!(
            automatic.phase,
            HevcSameHardwareRecoveryPhase::RebuildingFromCache
        );
    }

    #[test]
    fn same_hardware_replay_timeout_advances_flush_then_reopen_once() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 681_266_667_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
        transaction.flush_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);

        let timed_out = now + HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT;
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                4,
                timed_out,
                HardwareDecodeMode::Auto,
            ),
            HevcDecodeRecoveryAction::ReopenSameHardware
        );
        assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);

        transaction.reopen_attempts = 1;
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            3,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            20,
            timed_out,
        );
        assert_eq!(
            transaction.advance_after_repeated_failure_if_idle(
                4,
                timed_out + HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT,
                HardwareDecodeMode::Auto,
            ),
            HevcDecodeRecoveryAction::RequestSoftwareFallback
        );
        assert_eq!(transaction.flush_attempts, 1);
        assert_eq!(transaction.reopen_attempts, 1);
    }

    #[test]
    fn same_hardware_recovery_transaction_has_a_hard_wall_time_bound() {
        let now = Instant::now();
        let transaction = HevcSameHardwareRecoveryTransaction::new(
            HevcDecodeChainFallback {
                target_nsecs: 184_692_319_900,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            },
            0,
            None,
            now,
        );
        assert!(
            !transaction
                .expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME - Duration::from_nanos(1))
        );
        assert!(transaction.expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME));
    }

    #[test]
    fn recent_committed_output_progress_defers_wall_time_failure_until_idle() {
        let now = Instant::now();
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 1_186_466_655_435,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        };
        let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
        transaction.begin_attempt(
            5,
            HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
            8_672,
            now,
        );
        let progress_at =
            now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME - Duration::from_millis(50);
        let attempt = transaction
            .active_attempt
            .as_mut()
            .expect("cached rebuild attempt");
        attempt.output_commit_observed = true;
        attempt.last_admitted_progress_at = Some(progress_at);

        assert!(
            !transaction.expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME),
            "fresh committed frames must not be failed by the transaction's absolute age"
        );
        assert!(
            transaction.expired(progress_at + HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT),
            "the same transaction must still fail after admitted output really goes idle"
        );
    }

    #[test]
    fn four_worker_results_waiting_for_main_consumption_cannot_trigger_stall() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let now = Instant::now();
        let mut produced_not_consumed = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
        produced_not_consumed.submitted_sequence = 4;
        produced_not_consumed.result_produced_sequence = 4;
        produced_not_consumed.result_consumed_sequence = 0;
        produced_not_consumed.oldest_submitted_packet_nsecs = Some(184_400_000_000);
        watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: produced_not_consumed,
            now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
            output_snapshot: output_snapshot(PlaybackOutputState::Syncing, true, false, None, None),
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 184_692_319_900,
        });

        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.startup_in_flight_deadline(), None);
    }

    #[test]
    fn hevc_startup_in_flight_packet_arms_only_near_target_not_during_long_preroll() {
        let target_nsecs = 184_692_319_900;
        assert!(!hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            Some(179_900_000_000),
            target_nsecs,
        ));
        assert!(!hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            Some(180_166_666_667),
            target_nsecs,
        ));
        assert!(hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            true,
            Some(184_400_000_000),
            target_nsecs,
        ));
        assert!(!hevc_startup_in_flight_packet_should_arm(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            true,
            Some(target_nsecs),
            target_nsecs,
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
    fn exact_low_level_seek_decodes_from_226s_cra_and_gates_output_at_235s_target() {
        let transaction_id = 23;
        let anchor_nsecs = 226_810_000_000;
        let target_nsecs = 235_235_000_000;
        let next_cra_nsecs = 237_237_000_000;
        let mut recovery = VideoDecodeRecovery::default();
        let mut cra_packet = packet_from_data(&[
            0, 0, 0, 3, 0x2a, 0x01, 0xaa, // CRA_NUT
        ]);
        unsafe {
            (*cra_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }
        recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);
        assert!(recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));

        let landing = HevcLowLevelSeekLanding {
            transaction_id,
            target_nsecs,
            seek_position_nsecs: 234_235_000_000,
            anchor_nsecs,
            anchor_kind: VideoRecoveryPointKind::Cra,
            range_id: Some(1),
            anchor_packet_id: Some(430),
        };
        recovery.enable_hevc_low_level_recovery_point(landing);
        assert!(matches!(
            recovery.recovery_scope(),
            VideoDecodeRecoveryScope::ExactLowLevelSeek { .. }
        ));
        assert!(recovery.requires_exact_seek_output());
        assert!(recovery.should_skip_nonref_for_seek_preroll(Some(anchor_nsecs), false));
        assert!(recovery.should_skip_nonref_for_seek_preroll(
            Some(target_nsecs - EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS - 1),
            false,
        ));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(
            Some(target_nsecs - EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS),
            false,
        ));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(target_nsecs), false));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(None, false));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(anchor_nsecs), true,));
        assert!(
            recovery
                .observe_exact_seek_packet_progress(Some(anchor_nsecs))
                .is_some()
        );
        assert!(
            recovery
                .observe_exact_seek_packet_progress(Some(anchor_nsecs))
                .is_none(),
            "duplicate packet PTS is not recovery progress"
        );
        assert!(
            recovery
                .observe_exact_seek_packet_progress(Some(anchor_nsecs - 1))
                .is_none(),
            "backward packet PTS is not recovery progress"
        );
        assert!(
            recovery
                .observe_exact_seek_packet_progress(Some(anchor_nsecs + 41_000_000))
                .is_some(),
            "forward packet PTS refreshes exact-seek recovery progress"
        );
        assert!(!recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
        assert!(recovery.accept_recovery_point(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));

        assert_eq!(
            decoded_video_frame_start_action(
                target_nsecs - 1,
                target_nsecs,
                false,
                recovery.requires_exact_seek_output(),
            ),
            super::super::DecodedVideoFrameStartAction::DropBeforeStart
        );
        assert_eq!(
            decoded_video_frame_start_action(
                target_nsecs,
                target_nsecs,
                false,
                recovery.requires_exact_seek_output(),
            ),
            super::super::DecodedVideoFrameStartAction::Use { realign: false }
        );
        assert!(target_nsecs < next_cra_nsecs);

        recovery
            .finish_seek_bootstrap_after_target_frame(target_nsecs)
            .expect("target frame completes the exact low-level transaction");
        let completion = recovery
            .take_exact_seek_completion()
            .expect("exact transaction records its first eligible frame");
        assert_eq!(completion.transaction_id, transaction_id);
        assert_eq!(completion.first_eligible_frame_nsecs, target_nsecs);
        assert_eq!(completion.first_eligible_delta_nsecs, 0);
        assert!(!recovery.requires_exact_seek_output());
    }

    #[test]
    fn exact_seek_nonref_skip_does_not_reenable_after_reordered_packet_pts() {
        let target_nsecs = 694_233_333_333;
        let mut recovery = VideoDecodeRecovery {
            recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
                transaction_id: 5,
                target_nsecs,
            },
            ..Default::default()
        };
        let before_target_nsecs = 694_100_000_000;
        let after_target_nsecs = 694_300_000_000;

        assert!(recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), false,));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(after_target_nsecs), false,));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), false,));
    }

    #[test]
    fn bounded_cached_rebuild_disables_nonref_skip_across_reordered_packet_pts() {
        let target_nsecs = 694_233_333_333;
        let mut recovery = VideoDecodeRecovery {
            recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
                transaction_id: 5,
                target_nsecs,
            },
            ..Default::default()
        };
        let before_target_nsecs = 694_100_000_000;
        let after_target_nsecs = 694_300_000_000;

        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), true,));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(after_target_nsecs), true,));
        assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), true,));
    }

    #[test]
    fn exact_seek_decoder_results_are_isolated_from_playback_root_evidence() {
        let target_nsecs = 235_235_000_000;
        let recovery_scope = VideoDecodeRecoveryScope::ExactLowLevelSeek {
            transaction_id: 23,
            target_nsecs,
            seek_position_nsecs: 234_235_000_000,
            actual_anchor_nsecs: 226_810_000_000,
            actual_anchor_kind: VideoRecoveryPointKind::Cra,
        };
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog::default();

        for index in 0..50_u64 {
            assert!(watchdog.observe_exact_seek_decoder_result(
                recovery_scope,
                Some(231_000_000_000 + index * 33_333_333),
                0,
                true,
                now + Duration::from_micros(index),
            ));
        }
        assert_eq!(watchdog.exact_seek_zero_output_packets, 50);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);

        watchdog.complete_exact_seek_evidence_scope(
            23,
            target_nsecs + 33_333_333,
            false,
            false,
            now + Duration::from_millis(1),
        );
        assert_eq!(watchdog.exact_seek_transaction_id, None);
        assert_eq!(watchdog.exact_seek_zero_output_packets, 0);
        assert_eq!(
            watchdog.recent_output_high_water_nsecs,
            Some(target_nsecs + 33_333_333)
        );
        assert_eq!(
            watchdog.last_decoded_video_end_nsecs,
            Some(target_nsecs + 33_333_333)
        );

        let committed_end_nsecs = target_nsecs + 66_666_666;
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((target_nsecs + 33_333_333, committed_end_nsecs)),
            Some(33_333_333),
        );
        for index in 1..=3_u64 {
            let mut input = hevc_watchdog_input(
                committed_end_nsecs + index * 33_333_333,
                output,
                demux_watermark(false),
                committed_end_nsecs,
            );
            input.now = now + Duration::from_millis(index + 1);
            assert_eq!(
                watchdog.observe_packet(input),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(watchdog.recent_zero_output_packets, 3);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn cached_exact_seek_preroll_zero_outputs_stay_in_the_seek_scope() {
        let target_nsecs = 1_050_500_000_000;
        let recovery_scope = VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 29,
            target_nsecs,
        };
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog::default();

        for index in 0..58_u64 {
            assert!(watchdog.observe_exact_seek_decoder_result(
                recovery_scope,
                Some(1_047_733_333_000 + index * 33_333_333),
                0,
                true,
                now + Duration::from_micros(index),
            ));
        }
        assert_eq!(watchdog.exact_seek_zero_output_packets, 58);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
        assert_eq!(watchdog.pending_fallback, None);
        assert_eq!(watchdog.completed_exact_seek_transaction_id, None);
        assert_eq!(watchdog.completed_exact_seek_landing_nsecs, None);

        watchdog.complete_exact_seek_evidence_scope(
            29,
            1_050_533_333_000,
            false,
            true,
            now + Duration::from_millis(1),
        );
        assert_eq!(watchdog.exact_seek_transaction_id, None);
        assert_eq!(watchdog.completed_exact_seek_transaction_id, Some(29));
        assert_eq!(
            watchdog.completed_exact_seek_landing_nsecs,
            Some(1_050_533_333_000)
        );
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert_eq!(
            watchdog.recent_output_high_water_nsecs,
            Some(1_050_533_333_000)
        );
        assert_eq!(watchdog.pending_fallback, None);

        watchdog.reset();
        assert_eq!(watchdog.completed_exact_seek_transaction_id, None);
        assert_eq!(watchdog.completed_exact_seek_landing_nsecs, None);
    }

    #[test]
    fn problem_trace_191_exact_seek_zero_outputs_requests_recovery_instead_of_4_867s_hold() {
        let now = Instant::now();
        let transaction_id = 17;
        let target_nsecs = 765_666_666_667;
        let first_eligible_frame_nsecs = 765_766_666_667;
        let first_eligible_end_nsecs = 765_800_000_000;
        let seek_input_high_water_nsecs = 770_966_666_666;
        let mut watchdog = HevcDecodeChainWatchdog {
            exact_seek_transaction_id: Some(transaction_id),
            exact_seek_zero_output_packets: 191,
            exact_seek_input_high_water_nsecs: Some(seek_input_high_water_nsecs),
            ..Default::default()
        };

        watchdog.complete_exact_seek_evidence_scope(
            transaction_id,
            first_eligible_frame_nsecs,
            false,
            true,
            now,
        );

        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 191);
        assert!(watchdog.recent_packet_lead_exceeded);
        assert_eq!(
            watchdog.recent_input_packet_high_water_nsecs,
            Some(seek_input_high_water_nsecs)
        );
        assert_eq!(
            watchdog.recent_output_high_water_nsecs,
            Some(first_eligible_frame_nsecs)
        );
        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(17),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 11_153,
                frame_timeline_nsecs: first_eligible_frame_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: target_nsecs,
                before_queue_end_nsecs: None,
                after_queue_end_nsecs: Some(first_eligible_end_nsecs),
            }),
            HevcAdmittedVideoProgress::Partial
        );
        assert_eq!(watchdog.recent_zero_output_packets, 191);
        assert!(watchdog.recent_packet_lead_exceeded);

        let snapshot = output_snapshot(
            PlaybackOutputState::Syncing,
            false,
            false,
            Some((first_eligible_frame_nsecs, first_eligible_end_nsecs)),
            Some(33_333_333),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 770_666_666_667;
        gap.duration_nsecs = 33_333_333;
        gap.previous_expected_next_nsecs = Some(first_eligible_end_nsecs);
        gap.previous_gap_nsecs = Some(4_866_666_667);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = first_eligible_end_nsecs;
        gap.audio_played_timeline_nsecs = Some(target_nsecs);
        gap.demux_watermark = DemuxReaderWatermark {
            video_forward_nsecs: Some(25_766_655_556),
            audio_forward_nsecs: Some(30_255_600_907),
            selected_min_forward_nsecs: Some(25_766_655_556),
            ..Default::default()
        };
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::DropForFallback
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: first_eligible_end_nsecs,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
    }

    #[test]
    fn exact_seek_completion_inside_decode_recovery_preserves_root_evidence() {
        let now = Instant::now();
        let root_output_nsecs = 681_266_667_000;
        let root_input_nsecs = 690_633_333_333;
        let recovery_scope = VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 31,
            target_nsecs: root_output_nsecs,
        };
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 111,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(root_input_nsecs),
            recent_output_high_water_nsecs: Some(root_output_nsecs),
            last_decoded_video_end_nsecs: Some(root_output_nsecs),
            ..HevcDecodeChainWatchdog::default()
        };

        assert!(watchdog.observe_exact_seek_decoder_result(
            recovery_scope,
            Some(714_266_667_000),
            0,
            true,
            now,
        ));
        watchdog.complete_exact_seek_evidence_scope(
            31,
            716_533_333_000,
            true,
            true,
            now + Duration::from_millis(1),
        );

        assert_eq!(watchdog.exact_seek_transaction_id, None);
        assert_eq!(watchdog.exact_seek_zero_output_packets, 0);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 111);
        assert!(watchdog.recent_packet_lead_exceeded);
        assert_eq!(
            watchdog.recent_input_packet_high_water_nsecs,
            Some(root_input_nsecs)
        );
        assert_eq!(
            watchdog.recent_output_high_water_nsecs,
            Some(root_output_nsecs),
            "an uncommitted cached-rebuild frame must not advance root output"
        );
        assert_eq!(
            watchdog.last_decoded_video_end_nsecs,
            Some(root_output_nsecs)
        );
    }

    #[test]
    fn decode_recovery_pts_gap_is_routed_without_mutating_root_watchdog() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 30,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(690_633_333_333),
            recent_output_high_water_nsecs: Some(681_266_667_000),
            ..HevcDecodeChainWatchdog::default()
        };
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((681_000_000_000, 681_266_667_000)),
            Some(266_667_000),
        );
        let mut observation =
            decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
        observation.previous_expected_next_nsecs = Some(681_266_667_000);
        observation.timeline_nsecs = 690_633_333_000;
        observation.previous_gap_nsecs = Some(9_366_666_000);
        observation.decode_recovery_active = true;

        assert_eq!(
            watchdog.observe_decoded_frame_gap(observation),
            HevcDecodedFrameGapAction::Admit
        );
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 30);
        assert_eq!(watchdog.pending_fallback, None);

        observation.audio_timeline_gap = Some(AudioTimelineGapEvidence {
            previous_end_nsecs: 681_266_667_000,
            next_start_nsecs: 690_633_333_000,
        });
        assert_eq!(
            watchdog.observe_decoded_frame_gap(observation),
            HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap
        );
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 30);
        assert_eq!(
            watchdog.recent_output_high_water_nsecs,
            Some(681_266_667_000)
        );
    }

    #[test]
    fn decode_recovery_suspends_secondary_watchdogs_but_keeps_root_evidence() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 30,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(2_000_000_000),
            recent_output_high_water_nsecs: Some(1_000_000_000),
            pending_fallback: Some(HevcDecodeChainFallback {
                target_nsecs: 1_000_000_000,
                reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
            }),
            post_fallback_rebuffer_underfill_started_at: Some(now),
            first_zero_output_at: Some(now),
            startup_in_flight_stall_started_at: Some(now),
            startup_watchdog_retry_not_before: Some(now),
            startup_waiting_for_input: true,
            ..HevcDecodeChainWatchdog::default()
        };

        watchdog.suspend_playback_watchdogs_for_decode_recovery();

        assert_eq!(watchdog.pending_fallback, None);
        assert_eq!(watchdog.post_fallback_rebuffer_underfill_started_at, None);
        assert_eq!(watchdog.first_zero_output_at, None);
        assert_eq!(watchdog.startup_in_flight_stall_started_at, None);
        assert_eq!(watchdog.startup_watchdog_retry_not_before, None);
        assert!(!watchdog.startup_waiting_for_input);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 30);
        assert_eq!(
            watchdog.recent_input_packet_high_water_nsecs,
            Some(2_000_000_000)
        );
        assert_eq!(watchdog.recent_output_high_water_nsecs, Some(1_000_000_000));
    }

    #[test]
    fn active_decode_recovery_cannot_retrigger_the_playback_watchdog() {
        let now = Instant::now();
        let mut watchdog = HevcDecodeChainWatchdog {
            recent_zero_output_packets: 29,
            recent_input_packet_high_water_nsecs: Some(2_000_000_000),
            recent_output_high_water_nsecs: Some(1_000_000_000),
            health_state: HevcDecodeHealthState::Suspected,
            ..HevcDecodeChainWatchdog::default()
        };

        for index in 0..222_u64 {
            watchdog.observe_packet_during_decode_recovery(
                true,
                u64::from(index == 0),
                now + Duration::from_micros(index),
            );
        }

        assert_eq!(watchdog.recent_zero_output_packets, 29);
        assert_eq!(
            watchdog.recent_input_packet_high_water_nsecs,
            Some(2_000_000_000)
        );
        assert_eq!(watchdog.recent_output_high_water_nsecs, Some(1_000_000_000));
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.take_fallback(), None);
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
    fn hevc_zero_output_watchdog_hard_fallback_does_not_wait_for_low_water() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let low_water = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        for packet_index in 0..23_u64 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    1_600_000_000 + packet_index * 1_000_000,
                    low_water,
                    demux_watermark(false),
                    1_250_000_000,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_623_000_000,
                low_water,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);

        let stable_output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((900_000_000, 2_000_000_000)),
            Some(1_100_000_000),
        );
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                3_100_000_000,
                stable_output,
                demux_watermark(false),
                1_333_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 2_000_000_000,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );
    }

    #[test]
    fn post_soft_recovery_skips_reach_hard_fallback_without_waiting_for_idr() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            recent_audio_timeline_gap_checked: true,
            recent_input_packet_high_water_nsecs: Some(1_500_000_000),
            last_decoded_video_end_nsecs: Some(1_000_000_000),
            ..Default::default()
        };
        let low_water = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );

        for skipped_index in 0..5_u64 {
            watchdog.observe_post_soft_recovery_skipped_packet(
                HevcPostSoftRecoverySkippedPacketObservation {
                    session_id: PlaybackSessionId(1),
                    packet_nsecs: Some(1_510_000_000 + skipped_index * 10_000_000),
                    cache_sequence_contiguous: true,
                    hardware_accelerated: true,
                    output_snapshot: low_water,
                    demux_watermark: demux_watermark(false),
                    has_audio_output: true,
                    fallback_target_nsecs: 900_000_000,
                },
            );
            assert_eq!(watchdog.pending_fallback(), None);
        }
        watchdog.observe_post_soft_recovery_skipped_packet(
            HevcPostSoftRecoverySkippedPacketObservation {
                session_id: PlaybackSessionId(1),
                packet_nsecs: Some(1_560_000_000),
                cache_sequence_contiguous: true,
                hardware_accelerated: true,
                output_snapshot: low_water,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 900_000_000,
            },
        );

        assert_eq!(watchdog.post_soft_recovery_skipped_packets, 6);
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 1_000_000_000,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            })
        );

        let mut one_second_lead = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            recent_audio_timeline_gap_checked: true,
            recent_input_packet_high_water_nsecs: Some(1_500_000_000),
            last_decoded_video_end_nsecs: Some(1_000_000_000),
            ..Default::default()
        };
        one_second_lead.observe_post_soft_recovery_skipped_packet(
            HevcPostSoftRecoverySkippedPacketObservation {
                session_id: PlaybackSessionId(1),
                packet_nsecs: Some(2_000_000_000),
                cache_sequence_contiguous: true,
                hardware_accelerated: true,
                output_snapshot: low_water,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 900_000_000,
            },
        );
        assert_eq!(
            one_second_lead
                .take_fallback()
                .map(|fallback| fallback.reason),
            Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
        );

        let mut demux_unhealthy = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            recent_audio_timeline_gap_checked: true,
            recent_input_packet_high_water_nsecs: Some(1_500_000_000),
            last_decoded_video_end_nsecs: Some(1_000_000_000),
            ..Default::default()
        };
        demux_unhealthy.observe_post_soft_recovery_skipped_packet(
            HevcPostSoftRecoverySkippedPacketObservation {
                session_id: PlaybackSessionId(1),
                packet_nsecs: Some(2_000_000_000),
                cache_sequence_contiguous: true,
                hardware_accelerated: true,
                output_snapshot: low_water,
                demux_watermark: demux_watermark(true),
                has_audio_output: true,
                fallback_target_nsecs: 900_000_000,
            },
        );
        assert_eq!(demux_unhealthy.take_fallback(), None);
    }

    #[test]
    fn post_soft_recovery_skips_wait_while_output_queue_is_stable() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            recent_audio_timeline_gap_checked: true,
            recent_input_packet_high_water_nsecs: Some(1_500_000_000),
            last_decoded_video_end_nsecs: Some(1_000_000_000),
            ..Default::default()
        };
        let mut stable_output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((1_000_000_000, 2_460_000_000)),
            Some(1_460_000_000),
        );
        stable_output.video_decode_underfill = true;

        watchdog.observe_post_soft_recovery_skipped_packet(
            HevcPostSoftRecoverySkippedPacketObservation {
                session_id: PlaybackSessionId(1),
                packet_nsecs: Some(2_000_000_000),
                cache_sequence_contiguous: true,
                hardware_accelerated: true,
                output_snapshot: stable_output,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 900_000_000,
            },
        );

        assert_eq!(watchdog.post_soft_recovery_skipped_packets, 0);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn strong_high_water_fallback_is_not_discarded_by_preexisting_progress_grace() {
        assert!(!HevcDecodeChainFallbackReason::ZeroOutputRebuffer.invalidated_by_video_progress());
        assert!(
            !HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput.invalidated_by_video_progress()
        );
        assert!(
            HevcDecodeChainFallbackReason::StartupInFlightStall.invalidated_by_video_progress()
        );
    }

    fn assert_sparse_output_progress_does_not_erase_high_water_failure(
        packet_count: u64,
        packet_span_nsecs: u64,
    ) {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let base_nsecs = 100_000_000_000_u64;
        let mut output_end_nsecs = base_nsecs;
        for packet_index in 0..packet_count {
            let packet_offset_nsecs = if packet_count > 1 {
                packet_span_nsecs.saturating_mul(packet_index) / (packet_count - 1)
            } else {
                0
            };
            let output = output_snapshot(
                PlaybackOutputState::Playing,
                false,
                false,
                Some((base_nsecs.saturating_sub(40_000_000), output_end_nsecs)),
                Some(
                    output_end_nsecs
                        .saturating_sub(base_nsecs)
                        .saturating_add(40_000_000),
                ),
            );
            let _ = watchdog.observe_packet(hevc_watchdog_input(
                base_nsecs.saturating_add(packet_offset_nsecs),
                output,
                demux_watermark(false),
                output_end_nsecs,
            ));

            // A lone admitted frame breaks only the consecutive run. It must not
            // erase the high-water evidence without 500ms of caught-up output.
            if (packet_index + 1).is_multiple_of(17) {
                let before = output_end_nsecs;
                output_end_nsecs = output_end_nsecs.saturating_add(40_000_000);
                watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                    session_id: PlaybackSessionId(1),
                    codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    generation: 1,
                    frame_timeline_nsecs: before,
                    frame_duration_nsecs: 40_000_000,
                    current_start_position_nsecs: base_nsecs,
                    before_queue_end_nsecs: Some(before),
                    after_queue_end_nsecs: Some(output_end_nsecs),
                });
            }
        }

        assert!(
            watchdog.recent_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
        );
        assert_eq!(
            watchdog.take_fallback().map(|fallback| fallback.reason),
            Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
        );
    }

    #[test]
    fn hevc_68_packet_trace_with_sparse_output_keeps_failure_evidence() {
        assert_sparse_output_progress_does_not_erase_high_water_failure(68, 2_300_000_000);
    }

    #[test]
    fn hevc_132_packet_trace_with_sparse_output_keeps_failure_evidence() {
        assert_sparse_output_progress_does_not_erase_high_water_failure(132, 4_400_000_000);
    }

    #[test]
    fn synchronized_audio_gap_suppresses_packet_level_high_water_fallback() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        let synchronized_gap = AudioTimelineGapEvidence {
            previous_end_nsecs: 1_000_000_000,
            next_start_nsecs: 2_000_000_000,
        };

        for packet_index in 0..40_u64 {
            let mut input = hevc_watchdog_input(
                2_000_000_000 + packet_index * 40_000_000,
                output,
                demux_watermark(false),
                1_000_000_000,
            );
            input.synchronized_audio_timeline_gap =
                (packet_index == 23).then_some(synchronized_gap);
            assert_eq!(
                watchdog.observe_packet(input),
                HevcDecodeChainRecoveryAction::None
            );
        }

        assert!(watchdog.recent_zero_output_packets >= 30);
        assert!(watchdog.recent_packet_lead_exceeded);
        assert_eq!(
            watchdog.recent_synchronized_audio_timeline_gap,
            Some(synchronized_gap)
        );
        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
    }

    #[test]
    fn high_water_waits_until_synchronized_audio_gap_was_checked() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );

        for packet_index in 0..40_u64 {
            let mut input = hevc_watchdog_input(
                2_000_000_000 + packet_index * 40_000_000,
                output,
                demux_watermark(false),
                1_000_000_000,
            );
            input.synchronized_audio_timeline_gap_checked = false;
            assert_eq!(
                watchdog.observe_packet(input),
                HevcDecodeChainRecoveryAction::None
            );
        }

        assert!(!watchdog.recent_audio_timeline_gap_checked);
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn hevc_high_water_survives_scheduled_queue_drain_and_keeps_gap_boundary_target() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 960_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 0,
            before_queue_end_nsecs: Some(960_000_000),
            after_queue_end_nsecs: Some(1_000_000_000),
        });
        let drained_output = output_snapshot(PlaybackOutputState::Playing, false, true, None, None);
        for packet_index in 0..68_u64 {
            let _ = watchdog.observe_packet(hevc_watchdog_input(
                1_500_000_000 + packet_index * 10_000_000,
                drained_output,
                demux_watermark(false),
                900_000_000,
            ));
        }

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 1_000_000_000,
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
            decode_recovery_active: false,
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
            decode_recovery_active: false,
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
    fn hevc_zero_output_watchdog_resets_after_decoder_or_admitted_video_progress() {
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
        assert_eq!(watchdog.zero_output_packets, 0);

        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_166_000_000,
                snapshot,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 1);

        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_133_000_000,
            frame_duration_nsecs: 40_000_000,
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
            generation: 1,
            frame_timeline_nsecs: 1_000_000_000,
            frame_duration_nsecs: 41_666_666,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: None,
            after_queue_end_nsecs: Some(1_041_666_666),
        });

        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn first_admitted_progress_preserves_high_water_before_stable_output_gap() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            zero_output_packets: 178,
            recent_zero_output_packets: 178,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(144_967_000_000),
            recent_output_high_water_nsecs: Some(144_733_333_333),
            recent_audio_timeline_gap_checked: true,
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
            generation: 1,
            frame_timeline_nsecs: 144_700_000_000,
            frame_duration_nsecs: 33_333_333,
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
        gap.demux_watermark = demux_watermark(false);

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::DropForFallback
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 144_733_333_333,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
    }

    #[test]
    fn software_clean_233ms_gap_bridges_despite_stale_hardware_evidence() {
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 63,
            recent_zero_output_packets: 175,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(1_039_633_333_332),
            recent_output_high_water_nsecs: Some(1_036_766_666_666),
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((1_035_300_000_000, 1_036_766_666_666)),
            Some(1_466_666_666),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.hardware_accelerated = false;
        gap.timeline_nsecs = 1_037_000_000_000;
        gap.duration_nsecs = 33_333_333;
        gap.previous_expected_next_nsecs = Some(1_036_766_666_666);
        gap.previous_gap_nsecs = Some(233_333_334);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 1_036_766_666_666;
        gap.demux_watermark = demux_watermark(false);

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
        );
        assert_eq!(watchdog.take_fallback(), None);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert!(!watchdog.recent_packet_lead_exceeded);
    }

    #[test]
    fn software_zero_output_packets_do_not_accumulate_hardware_high_water() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        let mut input = hevc_watchdog_input(
            1_100_000_000,
            snapshot,
            demux_watermark(false),
            1_000_000_000,
        );
        input.hardware_accelerated = false;

        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 1);
        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
        assert!(!watchdog.recent_packet_lead_exceeded);
    }

    #[test]
    fn decoder_output_breaks_only_consecutive_hardware_zero_output_run() {
        let mut watchdog = HevcDecodeChainWatchdog {
            zero_output_packets: 24,
            recent_zero_output_packets: 24,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(1_800_000_000),
            recent_output_high_water_nsecs: Some(1_000_000_000),
            ..Default::default()
        };
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        );
        let mut input = hevc_watchdog_input(
            1_840_000_000,
            snapshot,
            demux_watermark(false),
            1_000_000_000,
        );
        input.decoded_frames = 1;

        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.recent_zero_output_packets, 24);
        assert_eq!(
            watchdog.recent_input_packet_high_water_nsecs,
            Some(1_800_000_000)
        );
        assert!(watchdog.recent_packet_lead_exceeded);
    }

    #[test]
    fn problem_trace_222_zero_outputs_withholds_7_233s_recovery_idr() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let last_output_end_nsecs = 683_400_000_000_u64;
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((682_400_000_000, last_output_end_nsecs)),
            Some(1_000_000_000),
        );
        let first_zero_packet_nsecs = 683_933_333_333_u64;
        for packet_index in 0..222_u64 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    first_zero_packet_nsecs.saturating_add(packet_index.saturating_mul(33_333_333)),
                    output,
                    demux_watermark(false),
                    last_output_end_nsecs,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(watchdog.recent_zero_output_packets, 222);
        assert_eq!(
            watchdog.pending_fallback().map(|fallback| fallback.reason),
            Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
        );

        let recovery_idr_nsecs = 690_633_333_333;
        let mut decoder_output = hevc_watchdog_input(
            recovery_idr_nsecs,
            output,
            demux_watermark(false),
            last_output_end_nsecs,
        );
        decoder_output.decoded_frames = 1;
        assert_eq!(
            watchdog.observe_packet(decoder_output),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.zero_output_packets, 0);
        assert_eq!(watchdog.recent_zero_output_packets, 222);
        assert!(watchdog.recent_input_packet_high_water_nsecs.is_some());

        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
        gap.timeline_nsecs = recovery_idr_nsecs;
        gap.duration_nsecs = 33_333_333;
        gap.previous_expected_next_nsecs = Some(last_output_end_nsecs);
        gap.previous_gap_nsecs = Some(7_233_333_333);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = last_output_end_nsecs;
        gap.audio_played_timeline_nsecs = Some(last_output_end_nsecs);
        gap.demux_watermark = demux_watermark(false);
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::DropForFallback
        );
        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: last_output_end_nsecs,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
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
    fn clean_hevc_keyframe_bridges_bounded_initial_gap_from_10_08_seek_log() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Syncing,
            false,
            false,
            Some((610_133_333_333, 610_166_666_666)),
            Some(33_333_333),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 613_166_666_667;
        gap.duration_nsecs = 33_333_333;
        gap.previous_expected_next_nsecs = Some(610_166_666_666);
        gap.previous_gap_nsecs = Some(3_000_000_001);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 610_166_666_666;
        gap.audio_played_timeline_nsecs = Some(608_300_000_000);
        gap.demux_watermark = DemuxReaderWatermark {
            video_forward_nsecs: Some(38_833_333_334),
            audio_forward_nsecs: Some(41_122_539_683),
            selected_min_forward_nsecs: Some(38_833_333_334),
            ..Default::default()
        };
        gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
            key_frame: true,
            ..Default::default()
        };

        assert_eq!(
            watchdog.observe_decoded_frame_gap(gap),
            HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
        );
        assert_eq!(watchdog.take_fallback(), None);
    }

    #[test]
    fn clean_hevc_keyframe_does_not_bridge_large_initial_gap_beyond_hold_limit() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Syncing,
            false,
            false,
            Some((174_066_666_667, 174_116_666_666)),
            Some(49_999_999),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 179_116_666_667;
        gap.duration_nsecs = 16_666_666;
        gap.previous_expected_next_nsecs = Some(174_116_666_666);
        gap.previous_gap_nsecs = Some(5_000_000_001);
        gap.max_gap_nsecs = 200_000_000;
        gap.fallback_target_nsecs = 174_116_666_666;
        gap.audio_played_timeline_nsecs = Some(174_066_666_667);
        gap.demux_watermark = DemuxReaderWatermark {
            video_forward_nsecs: Some(10_000_000_000),
            audio_forward_nsecs: Some(10_000_000_000),
            selected_min_forward_nsecs: Some(10_000_000_000),
            ..Default::default()
        };
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
    fn clean_hevc_keyframe_does_not_expand_playing_gap_policy() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((174_066_666_667, 174_116_666_666)),
            Some(49_999_999),
        );
        let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
        gap.timeline_nsecs = 175_116_666_667;
        gap.duration_nsecs = 16_666_666;
        gap.previous_expected_next_nsecs = Some(174_116_666_666);
        gap.previous_gap_nsecs = Some(1_000_000_001);
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
    fn strong_hevc_high_water_gap_drops_even_while_output_is_stable() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 24,
            recent_soft_recovery_attempted: true,
            recent_packet_lead_exceeded: true,
            recent_audio_timeline_gap_checked: true,
            ..Default::default()
        };
        let mut snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((1_000_000_000, 2_460_000_000)),
            Some(1_460_000_000),
        );
        snapshot.video_decode_underfill = true;
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
            watchdog.take_fallback().map(|fallback| fallback.reason),
            Some(HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput)
        );
    }

    #[test]
    fn hardware_high_water_enters_suspected_without_low_water() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let mut snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((1_000_000_000, 2_460_000_000)),
            Some(1_460_000_000),
        );
        snapshot.video_decode_underfill = true;

        for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    3_000_000_000 + packet_index * 1_000_000,
                    snapshot,
                    demux_watermark(false),
                    2_460_000_000,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }

        assert_eq!(watchdog.take_fallback(), None);
        assert!(!watchdog.soft_recovery_attempted);
        assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
        assert_eq!(
            watchdog.recent_zero_output_packets,
            HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
        );
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
    fn hevc_recent_gap_evidence_clears_only_after_500ms_caught_up_progress() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 24,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(1_000_000_000),
            recent_output_high_water_nsecs: Some(960_000_000),
            pending_fallback: Some(HevcDecodeChainFallback {
                target_nsecs: 1_000_000_000,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            }),
            ..Default::default()
        };
        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: 1_000_000_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(1_000_000_000),
                after_queue_end_nsecs: Some(1_499_000_000),
            }),
            HevcAdmittedVideoProgress::Partial
        );

        assert_eq!(watchdog.recent_zero_output_packets, 24);
        assert!(watchdog.recent_packet_lead_exceeded);
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 499_000_000);
        assert!(watchdog.pending_fallback.is_some());

        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: 1_499_000_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(1_499_000_000),
                after_queue_end_nsecs: Some(1_500_000_000),
            }),
            HevcAdmittedVideoProgress::Stable
        );
        watchdog.clear_recent_gap_evidence();
        watchdog.pending_fallback = None;

        assert_eq!(watchdog.recent_zero_output_packets, 0);
        assert!(!watchdog.recent_packet_lead_exceeded);
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 0);
        assert_eq!(watchdog.pending_fallback, None);
    }

    #[test]
    fn non_contiguous_admitted_frame_resets_healthy_recovery_window() {
        let mut watchdog = HevcDecodeChainWatchdog {
            health_state: HevcDecodeHealthState::Suspected,
            recent_zero_output_packets: 24,
            recent_packet_lead_exceeded: true,
            recent_input_packet_high_water_nsecs: Some(1_000_000_000),
            recent_output_high_water_nsecs: Some(960_000_000),
            ..Default::default()
        };
        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: 1_000_000_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(1_000_000_000),
                after_queue_end_nsecs: Some(1_300_000_000),
            }),
            HevcAdmittedVideoProgress::Partial
        );
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 300_000_000);

        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: 1_800_000_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(1_300_000_000),
                after_queue_end_nsecs: Some(1_840_000_000),
            }),
            HevcAdmittedVideoProgress::Partial
        );
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 0);

        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: 1_840_000_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 1_000_000_000,
                before_queue_end_nsecs: Some(1_840_000_000),
                after_queue_end_nsecs: Some(2_339_000_000),
            }),
            HevcAdmittedVideoProgress::Partial
        );
        assert_eq!(watchdog.healthy_admitted_progress_nsecs, 499_000_000);
        assert_eq!(watchdog.recent_zero_output_packets, 24);
    }

    #[test]
    fn dropped_before_start_does_not_count_as_admitted_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 900_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_000_000_000),
            after_queue_end_nsecs: Some(1_040_000_000),
        });
        assert!(watchdog.last_video_progress_at.is_none());
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
            generation: 1,
            frame_timeline_nsecs: 1_050_000_000,
            frame_duration_nsecs: 40_000_000,
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
    fn hevc_zero_output_pts_gap_fallback_survives_first_admitted_video_progress() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );
        for packet_index in 0..23_u64 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    253_000_000_000 + packet_index * 1_000_000,
                    snapshot,
                    demux_watermark(false),
                    252_900_000_000,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_500_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
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
            generation: 1,
            frame_timeline_nsecs: 257_720_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 252_900_000_000,
            before_queue_end_nsecs: Some(252_920_000_000),
            after_queue_end_nsecs: Some(257_760_000_000),
        });

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 252_920_000_000,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
    }

    #[test]
    fn hevc_high_water_pts_gap_evidence_survives_preroll_below_input_high_water() {
        let mut watchdog = HevcDecodeChainWatchdog::default();
        let snapshot = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((252_760_000_000, 252_920_000_000)),
            Some(40_000_000),
        );
        for packet_index in 0..23_u64 {
            assert_eq!(
                watchdog.observe_packet(hevc_watchdog_input(
                    253_000_000_000 + packet_index * 1_000_000,
                    snapshot,
                    demux_watermark(false),
                    252_900_000_000,
                )),
                HevcDecodeChainRecoveryAction::None
            );
        }
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_500_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
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

        assert_eq!(
            watchdog.take_fallback(),
            Some(HevcDecodeChainFallback {
                target_nsecs: 252_920_000_000,
                reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
            })
        );
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
            recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
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
            HevcDecodeChainRecoveryAction::None
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
