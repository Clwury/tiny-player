pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use std::{
    sync::{Arc, atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::QueuedVideoFrame;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::audio_output_gate::{
    DelayedAudioStartSilencePolicy, flush_pending_start_audio, pending_audio_underrun_recovery_plan,
    push_decoded_audio_to_output, recover_pending_start_audio_after_underrun, stage_pending_audio,
};
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::audio_output_gate::{
    AudioStageCheckpoint, AudioStageResult, stage_pending_audio_with_checkpoint,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::output_rebuffer::{
    AudioClockResumeDecision, AudioResumeWaterline, AudioResumeWaterlineInput,
    InitialOutputSyncDecision, PlaybackOutputState, PlaybackResumeWaterline, RebufferResumeAnchor,
    VideoOutputUnderflowClassification,
    audio_output_buffered_until_for_resume, clear_video_output_rebuffer, enter_video_output_rebuffer,
    finish_video_output_rebuffer_if_ready,
    rebuffer_playback_resume_waterline_after_cache_pause,
    rebuffer_playback_resume_waterline_after_prolonged_wait, should_block_for_demux_read,
    playback_resume_waterline_blocked_on, video_output_rebuffer_should_enter,
    video_output_underflow_classification,
};
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::output_rebuffer::{
    DecodedOutputReadiness, PrefetchReadiness, ResumeAnchorSource,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::pending_audio_queue::PendingStartAudio;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::PlaybackBlockReason;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::scheduled_video_queue::{
    ScheduledVideoQueue, VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::video_decode_pipeline::HevcDecodeChainStats;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::video_decode_worker::{
    VideoDecodeWorkerSnapshot, VideoDecodeWorkerState,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::video_output_gate::{
    present_video_frame_to_vo, report_first_video_frame_presented,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_VIDEO_LEAD_DURATION,
    AUDIO_OUTPUT_STEADY_TARGET_DURATION, AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION,
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_REBUFFER_LOOP_DETECTION_WINDOW,
    AUDIO_REBUFFER_PREFILL_LOOP_TARGET, AUDIO_REBUFFER_PREFILL_TARGET,
    AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioClockHandle, AudioClockMode, AudioOutput,
    AudioOutputActivitySnapshot, AudioOutputLifecycle, AudioOutputServiceStage, AudioOutputSnapshot,
    AudioOutputStableSnapshot, BufferedReporter, DecodedAudio, DemuxPacketCache,
    DemuxReaderWatermark, FfmpegControl,
    OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER, PENDING_AUDIO_CONTINUITY_TOLERANCE,
    PENDING_START_AUDIO_BACKPRESSURE_DURATION, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PlaybackScheduler, PositionReporter,
    SubtitlePipeline, VideoDeadlineService,
    VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VIDEO_OUTPUT_START_FIRST_FRAME_STALL_LOG_AFTER,
    VIDEO_OUTPUT_START_FAST_READY_DURATION, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER,
    duration_nsecs, nsecs_to_seconds,
};
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::AudioOutputUnstableSnapshot;

#[path = "output_gate/audio_pressure.rs"]
mod audio_pressure;
#[path = "output_gate/demux_watermark.rs"]
mod demux_watermark;
#[path = "output_gate/discard.rs"]
mod discard;
#[path = "output_gate/initial_admission.rs"]
mod initial_admission;
#[path = "output_gate/initial_audio.rs"]
mod initial_audio;
#[path = "output_gate/initial_start.rs"]
mod initial_start;
#[path = "output_gate/resume.rs"]
mod resume;
#[path = "output_gate/scheduler.rs"]
mod scheduler;
#[path = "output_gate/snapshot.rs"]
mod snapshot;
#[cfg(test)]
#[path = "output_gate/tests.rs"]
mod tests;

pub(in crate::player::backend::ffmpeg::playback_loop) use audio_pressure::DecodedAudioAdmission;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use audio_pressure::{
    PendingAudioPressureContext, audio_output_contiguous_start_timeline_nsecs,
};
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use audio_pressure::{
    audio_output_flush_until_timeline_nsecs, playing_pending_audio_limit_duration,
    playing_pending_audio_pressure_clear_duration, playing_pending_audio_warn_entry_duration,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use demux_watermark::timed_output_gate_demux_watermark;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use discard::discard_decoded_video_before_output_gate_resume_if_ready;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use initial_admission::{
    InitialStartAdmission, InitialStartAdmissionInput, InitialStartBlockReason,
    initial_start_admission,
};
pub(in crate::player::backend::ffmpeg::playback_loop) use initial_audio::{
    InitialAudioPreparePhase, InitialAudioPrepareToken, PrestartAudioOwnership,
    PrestartAudioOwnershipInput, classify_prestart_audio_ownership,
};
pub(in crate::player::backend::ffmpeg) use initial_start::expire_initial_av_start_hard_deadline;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use initial_start::service_initial_video_clock_until_audio_start;
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use initial_start::{
    InitialAudioAmmunitionSnapshot, InitialAudioCommitCheckpoint, InitialAudioNoPayloadDisposition,
    InitialAudioStartAction, abort_initial_audio_stage_for_test,
    abort_initial_av_start_for_discontinuity_change,
    commit_initial_audio_stage_with_checkpoints_for_test, commit_initial_av_start,
    fail_initial_av_start_after_unstable_snapshot_deadline, initial_audio_clock_reset_required,
    initial_audio_no_payload_disposition, initial_audio_start_action,
    initial_audio_start_ammunition_ready, release_initial_seek_transition_after_clock_reset,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use resume::OutputGateResumeTiming;
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use resume::{
    MAX_REBUFFER_AUDIO_LEAD_NSECS, StaleRebufferPendingAudio, stale_rebuffer_pending_audio,
    stale_rebuffer_pending_audio_ahead,
};
pub(in crate::player::backend::ffmpeg) use resume::{
    OutputGateResumeStatus, service_output_gate_resume_if_ready,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PendingStartAudioPressureLevel {
    Normal,
    Warn,
    ForceRecovery,
    HardReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct RebufferAudioRealignRequest {
    pub(in crate::player::backend::ffmpeg) target_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) anchor_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) first_video_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) far_ahead_audio_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) far_ahead_observation_count: u8,
    pub(in crate::player::backend::ffmpeg) reason: &'static str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioRealignCoverage {
    pub(in crate::player::backend::ffmpeg) audio_accepted_start_timeline_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) start_gap_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) contiguous_coverage_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) protected_target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) ready: bool,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct AudioReaderGapWatchdog {
    target_timeline_nsecs: u64,
    started_at: Instant,
    last_progress_nsecs: u64,
    last_progress_at: Instant,
    observations: u64,
    first_observed_pts_nsecs: Option<u64>,
    last_observed_pts_nsecs: Option<u64>,
    request_issued: bool,
}

#[derive(Clone, Copy, Debug)]
struct AudioContinuityRejectionSummary {
    first_rejected_pts_nsecs: u64,
    last_rejected_pts_nsecs: u64,
    rejected_count: u64,
    largest_gap_nsecs: u64,
    last_log_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct AudioSyncDropLogSummary {
    drop_before_timeline_nsecs: u64,
    started_at: Instant,
    last_log_at: Instant,
    total_dropped_frames: u64,
    suppressed_since_last_log: u64,
    first_raw_timestamp: i64,
    last_raw_timestamp: i64,
    first_timeline_nsecs: u64,
    last_timeline_nsecs: u64,
    last_buffered_until_nsecs: u64,
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    INITIAL_AV_START_HARD_TIMEOUT: Duration = Duration::from_secs(3);
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    INITIAL_AUDIO_START_MIN_AMMUNITION: Duration = Duration::from_millis(200);
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    INITIAL_AUDIO_START_RETRY_INTERVAL: Duration = Duration::from_millis(8);
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    OUTPUT_GATE_PERIODIC_PROBE_INTERVAL: Duration = Duration::from_millis(500);
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    INITIAL_SYNC_LOG_SUMMARY_INTERVAL: Duration = Duration::from_millis(500);

/// Compile-time allowlist for the short initial-AO retry. Data dependencies
/// park on output-generation changes; terminal states rebuffer immediately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) enum InitialAudioTransientRetry {
    OutputSnapshotBusy,
    StableSnapshotUnstable,
    AudioStageWouldBlock,
    PreparedSnapshotUnstable,
    VideoOutputWouldBlock,
}

impl InitialAudioTransientRetry {
    fn as_str(self) -> &'static str {
        match self {
            Self::OutputSnapshotBusy => "audio_output_snapshot_busy",
            Self::StableSnapshotUnstable => "audio_output_snapshot_unstable",
            Self::AudioStageWouldBlock => "audio_stage_would_block_without_payload",
            Self::PreparedSnapshotUnstable => "prepared_snapshot_unstable",
            Self::VideoOutputWouldBlock => "initial_video_publish_would_block",
        }
    }
}

pub(in crate::player::backend::ffmpeg) const AUDIO_OUTPUT_ACTIVITY_STALL_AFTER: Duration =
    Duration::from_millis(250);
pub(in crate::player::backend::ffmpeg) const AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER: Duration =
    Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioOutputActivityWatchdogAction {
    ReleaseSeekTransition,
    WarnFrozenClock,
    RecoverAndReanchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputActivityWatchdogEvent {
    pub(in crate::player::backend::ffmpeg) action: AudioOutputActivityWatchdogAction,
    pub(in crate::player::backend::ffmpeg) stalled_for: Duration,
}

#[derive(Clone, Copy, Debug)]
struct AudioOutputActivityWatchdog {
    stalled_since: Instant,
    last_played_timeline_nsecs: u64,
    last_callback_count: u64,
    last_consumed_callback_count: u64,
    last_silenced_callback_count: u64,
    last_underrun_count: u64,
    warning_emitted: bool,
    seek_release_attempted: bool,
    recovery_started: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum OutputServiceDemand {
    #[default]
    None,
    PeriodicProbe,
    OutputStateChanged,
    DecodeRecovery,
    AudioStartDue,
    HardDeadline,
}

impl OutputServiceDemand {
    pub(in crate::player::backend::ffmpeg) fn is_due(self) -> bool {
        self != Self::None
    }

    pub(in crate::player::backend::ffmpeg) fn audio_start_due(self) -> bool {
        self == Self::AudioStartDue
    }

    pub(in crate::player::backend::ffmpeg) fn hard_deadline_due(self) -> bool {
        self == Self::HardDeadline
    }

    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PeriodicProbe => "periodic_probe",
            Self::OutputStateChanged => "output_state_changed",
            Self::DecodeRecovery => "decode_recovery",
            Self::AudioStartDue => "audio_start_due",
            Self::HardDeadline => "hard_deadline",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialSyncLogObservation
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_video_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_audio_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_video_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) strict_video_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) demux_min_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) blocked_on: &'static str,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) due_kind:
        OutputServiceDemand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialSyncLogDecision {
    Changed { suppressed_repeats: u64 },
    Summary { repeated_observations: u64 },
    Suppressed,
}

#[derive(Clone, Copy, Debug)]
struct InitialSyncLogState {
    observation: InitialSyncLogObservation,
    output_generation: u64,
    last_logged_at: Instant,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialAudioDeferObservation
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_start_target_nsecs:
        u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pending_covers_target: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) device_covers_target: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) ammunition_at_threshold:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_ledger_observed:
        bool,
}

#[derive(Clone, Copy, Debug)]
struct InitialAudioDeferLogState {
    observation: InitialAudioDeferObservation,
    last_logged_at: Instant,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug)]
struct PrestartAudioOwnershipLogState {
    ownership: PrestartAudioOwnership,
    transaction_id: u64,
    last_logged_at: Instant,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingAudioBackpressureLogState {
    reason: &'static str,
    started_at: Instant,
    last_logged_at: Instant,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug)]
struct OutputGateBlockLogState {
    blocked_on: PlaybackBlockReason,
    detail: &'static str,
    started_at: Instant,
    last_logged_at: Instant,
    suppressed_repeats: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum PendingAudioRetentionAnchorSource
{
    InitialTransaction,
    Rebuffer,
    UnpresentedVideo,
}

impl PendingAudioRetentionAnchorSource {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn as_str(
        self,
    ) -> &'static str {
        match self {
            Self::InitialTransaction => "initial_transaction",
            Self::Rebuffer => "rebuffer",
            Self::UnpresentedVideo => "unpresented_video",
        }
    }
}

/// The single timeline boundary that owns pending audio while output is
/// stopped.  In particular, `Primed` never derives this value from the
/// scheduled-video queue: that queue is allowed to advance when a frame is
/// published, while an initial A/V transaction's audio target is immutable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct PendingAudioRetentionPlan
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) anchor_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) source:
        PendingAudioRetentionAnchorSource,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct OutputGateBlockLogEmission
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) log_kind: &'static str,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) suppressed_repeats: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) blocked_for: Duration,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialAvStartTransaction
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) discontinuity_epoch: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) seek_generation: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) video_anchor_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_start_target_nsecs:
        u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) started_at: Instant,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_start_due_at: Instant,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) next_audio_start_retry_at:
        Instant,
    /// `true` when audio preparation is waiting for newly decoded ownership
    /// instead of a transient AO retry. Housekeeping changes re-arm the
    /// transaction immediately; otherwise only the hard deadline is due.
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_retry_waiting_for_state_change:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) hard_deadline_at: Instant,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_frame_presented: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_prepare_phase:
        InitialAudioPreparePhase,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_prepare_epoch:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_prepare_token:
        Option<InitialAudioPrepareToken>,
    /// Explicit delayed-start ownership committed while the transaction is
    /// active. This is never inferred from a moving video queue.
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) committed_bounded_delayed_audio_start_nsecs:
        Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialAvStartDecision {
    Waiting,
    Commit,
    Rebuffer,
}

impl InitialAvStartTransaction {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn decision(
        self,
        now: Instant,
        cache_commit_available: bool,
    ) -> InitialAvStartDecision {
        if now >= self.hard_deadline_at {
            return InitialAvStartDecision::Rebuffer;
        }
        if now < self.audio_start_due_at || !cache_commit_available {
            return InitialAvStartDecision::Waiting;
        }
        InitialAvStartDecision::Commit
    }
}

pub(in crate::player::backend::ffmpeg) struct PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg::playback_loop) scheduled_video_queue:
        ScheduledVideoQueue,
    video_deadline_service: Option<VideoDeadlineService>,
    video_deadline_audio_clock_available: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) pending_start_audio: PendingStartAudio,
    pub(in crate::player::backend::ffmpeg::playback_loop) playback_output_state:
        PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_frame_needed: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_frame_presented: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) output_clock_running: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_underrun_started_at:
        Option<Instant>,
    /// Single wall-clock origin for every `Rebuffering` transaction.  This is
    /// deliberately separate from the underflow detector: the latter can be
    /// cleared or restarted while the output gate is already paused, whereas
    /// fallback deadlines must remain monotonic for the whole transaction.
    rebuffer_started_at: Option<Instant>,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_bootstrap_after_seek: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_decode_underfill: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) rebuffer_empty_audio_output_blocked: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) audio_sync_drop_before_timeline_nsecs:
        Option<u64>,
    audio_sync_drop_log_summary: Option<AudioSyncDropLogSummary>,
    pub(in crate::player::backend::ffmpeg::playback_loop) rebuffer_audio_realign_request:
        Option<RebufferAudioRealignRequest>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_reader_gap_watchdog:
        Option<AudioReaderGapWatchdog>,
    audio_continuity_rejection_summary: Option<AudioContinuityRejectionSummary>,
    pub(in crate::player::backend::ffmpeg::playback_loop) decode_recovery_transaction:
        Option<DecodeRecoveryTransaction>,
    syncing_started_at: Option<Instant>,
    generation_reset_started_at: Instant,
    defer_pending_start_audio_flush_once: bool,
    startup_pending_audio_pressure_context_active: bool,
    pending_start_audio_pressure_level: PendingStartAudioPressureLevel,
    startup_first_frame_stall_logged: bool,
    recent_audio_output_underrun_window_started_at: Option<Instant>,
    recent_audio_output_underruns: u8,
    rebuffer_far_ahead_audio_observation_count: u8,
    audio_gap_recovery_until: Option<Instant>,
    audio_gap_recovery_target_nsecs: Option<u64>,
    initial_delayed_audio_start_timeline_nsecs: Option<u64>,
    initial_audio_gap_at_video_start_timeline_nsecs: Option<u64>,
    initial_av_start_transaction: Option<InitialAvStartTransaction>,
    last_initial_audio_prepare_terminal_phase: Option<InitialAudioPreparePhase>,
    next_initial_av_start_transaction_id: u64,
    initial_av_pair_started_at: Option<Instant>,
    initial_sync_log_state: Option<InitialSyncLogState>,
    initial_audio_defer_log_state: Option<InitialAudioDeferLogState>,
    prestart_audio_ownership_log_state: Option<PrestartAudioOwnershipLogState>,
    pending_audio_backpressure_log_state: Option<PendingAudioBackpressureLogState>,
    output_gate_block_log_state: Option<OutputGateBlockLogState>,
    /// Transaction fence advanced only by media discontinuities (seek, flush,
    /// track replacement, or decoder reset).  Packet sequencing continues to
    /// use `PlaybackGeneration` and must never invalidate a restart.
    discontinuity_epoch: u64,
    output_housekeeping_generation: u64,
    output_housekeeping_serviced_generation: u64,
    last_output_housekeeping_service_at: Option<Instant>,
    video_clock_anchor_valid: bool,
    audio_output_activity_watchdog: Option<AudioOutputActivityWatchdog>,
    audio_output_clock_stall_fallback_active: bool,
}

pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_STAGING_NSECS: u64 = 500_000_000;
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_AUDIO_READY_HYSTERESIS_NSECS: u64 =
    25_000_000;
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_DECODER_IN_FLIGHT_ALLOWANCE: usize = 4;
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_HOLD_GAP_MAX_NSECS: u64 =
    5_000_000_000;
// mpv's calculate_frame_duration() accepts three milliseconds of mux
// timestamp rounding plus 0.1ms of slack. Apply the same tolerance at tiny's
// bounded recovery limit: a nominal five-second GOP can otherwise measure
// 5,000,041,668ns after rational timestamps and integer frame durations are
// converted to nanoseconds.
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS: u64 =
    3_100_000;
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS: u64 =
    5_000_000_000;
pub(in crate::player::backend::ffmpeg) const DECODE_RECOVERY_MAX_WALL_TIME: Duration =
    Duration::from_secs(5);

pub(in crate::player::backend::ffmpeg) fn decode_recovery_gap_within_limit(
    gap_nsecs: u64,
    max_gap_nsecs: u64,
) -> bool {
    gap_nsecs <= max_gap_nsecs.saturating_add(DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodeRecoveryDisposition {
    #[default]
    Exact,
    HoldGap,
    Reanchor,
    DropForFallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodeRecoverySource {
    DecoderError,
    FlushReplay,
    VulkanReopenReplay,
    CachedSafeIdrRebuild,
    SoftwareFallback,
    LowLevelSeek,
}

impl DecodeRecoverySource {
    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::DecoderError => "decoder_error",
            Self::FlushReplay => "flush_replay",
            Self::VulkanReopenReplay => "vulkan_reopen_replay",
            Self::CachedSafeIdrRebuild => "cached_safe_idr_rebuild",
            Self::SoftwareFallback => "software_fallback",
            Self::LowLevelSeek => "low_level_seek",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodeRecoveryGapProvenance {
    #[default]
    ContinuousMedia,
    ConfirmedSynchronizedTimelineGap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct DecodeRecoveryDropForFallback {
    pub(in crate::player::backend::ffmpeg) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) first_frame_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) gap_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) source: DecodeRecoverySource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodeRecoveryPhase {
    SwitchingDecoder,
    Replaying,
    Buffered,
    Committed,
    CommittedGap,
    Reanchored,
    DroppedForFallback,
    Failed,
}

impl DecodeRecoveryPhase {
    pub(in crate::player::backend::ffmpeg) fn terminal(self) -> bool {
        matches!(
            self,
            Self::Committed
                | Self::CommittedGap
                | Self::Reanchored
                | Self::DroppedForFallback
                | Self::Failed
        )
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop) struct DecodeRecoveryTransaction {
    pub(in crate::player::backend::ffmpeg::playback_loop) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) resume_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) phase: DecodeRecoveryPhase,
    pub(in crate::player::backend::ffmpeg::playback_loop) disposition: DecodeRecoveryDisposition,
    pub(in crate::player::backend::ffmpeg::playback_loop) source: DecodeRecoverySource,
    pub(in crate::player::backend::ffmpeg::playback_loop) gap_provenance:
        DecodeRecoveryGapProvenance,
    pub(in crate::player::backend::ffmpeg::playback_loop) drop_for_fallback:
        Option<DecodeRecoveryDropForFallback>,
    pub(in crate::player::backend::ffmpeg::playback_loop) staging_queue: ScheduledVideoQueue,
    pub(in crate::player::backend::ffmpeg::playback_loop) staging_frame_budget: Option<usize>,
    pub(in crate::player::backend::ffmpeg::playback_loop) staging_budget_reached: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) audio_ready_latched: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) started_at: Instant,
    pub(in crate::player::backend::ffmpeg::playback_loop) barrier_started_at: Option<Instant>,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_staged_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) last_staged_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) largest_confirmed_gap_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) bridged_gap_count: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) rejected_frame_count: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_rejected_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) last_rejected_frame_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) last_rejection_log_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) state: PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg) first_video_frame_pending: bool,
    pub(in crate::player::backend::ffmpeg) first_frame_needed: bool,
    pub(in crate::player::backend::ffmpeg) first_frame_presented: bool,
    pub(in crate::player::backend::ffmpeg) initial_av_start_pending: bool,
    pub(in crate::player::backend::ffmpeg) output_clock_running: bool,
    pub(in crate::player::backend::ffmpeg) audio_start_target_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) output_transition_deadline_ms: Option<u64>,
    pub(in crate::player::backend::ffmpeg) rebuffering: bool,
    pub(in crate::player::backend::ffmpeg) queued_video_frames: usize,
    pub(in crate::player::backend::ffmpeg) recovery_staging_frames: usize,
    pub(in crate::player::backend::ffmpeg) recovery_staging_frame_budget: Option<usize>,
    pub(in crate::player::backend::ffmpeg) committed_output_high_water_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) recovery_staged_high_water_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) decode_recovery_audio_ready_latched: bool,
    /// Sum of queued frame intervals. Timeline holes are deliberately excluded.
    pub(in crate::player::backend::ffmpeg) queued_video_coverage_nsecs: u64,
    /// Compatibility alias for `queued_video_coverage_nsecs`.
    pub(in crate::player::backend::ffmpeg) queued_video_duration_nsecs: u64,
    /// Full first-frame-to-last-frame-end span, for diagnostics only.
    pub(in crate::player::backend::ffmpeg) queued_video_range_span_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queued_video_range_nsecs: Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg) queued_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) queued_video_contiguous_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) queued_video_largest_gap_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) video_output_low_water: bool,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_frames: usize,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
    pub(in crate::player::backend::ffmpeg) video_bootstrap_after_seek: bool,
    pub(in crate::player::backend::ffmpeg) video_decode_underfill: bool,
    pub(in crate::player::backend::ffmpeg) rebuffer_empty_audio_output_blocked: bool,
    pub(in crate::player::backend::ffmpeg) scheduler_dropped_video_frames: u64,
    pub(in crate::player::backend::ffmpeg) recent_coordinator_stall_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) recent_coordinator_stall_age_nsecs: Option<u64>,
}
