use std::{
    os::raw::c_int,
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, RenderSize, VideoOutputQueue, VideoOutputQueueSnapshot},
};

use super::audio_decode_worker::{
    AudioDecodePacketResult, AudioDecodeWorkerSnapshot, AudioDecodeWorkerState, AudioDecodedFrame,
};
use super::audio_output_gate::recover_pending_start_audio_after_underrun;
use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoded_audio_frame::process_audio_decode_drain_result;
use super::drain_phase::{PlaybackDrainPhase, PlaybackDrainResults};
use super::output_gate::AudioOutputActivityWatchdogAction;
use super::pending_audio_queue::PendingStartAudio;
use super::playback_block::{
    VideoOutputResourcePressure, video_decode_block_reason_with_output_queue,
    video_output_resource_pressure,
};
use super::playback_wait_service::PlaybackLoopDeadline;
use super::video_decode_drain_frame_processor::{
    VideoDecodeDrainFrameProcessor, VideoDecodeDrainProcessStatus,
};
use super::video_decode_pipeline::{
    VideoDecodeRecoveryScope, VideoPacketAdmissionContext, VideoPacketAdmissionPressure,
    hevc_startup_zero_output_timeout,
};
use super::video_decode_worker::{
    VideoDecodeDrainResult, VideoDecodeWorkerSnapshot, VideoDecodeWorkerState,
};
use super::video_frame_prepare_worker::VideoFramePrepareWorkerSnapshot;
use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN,
    AudioDecodePipeline, AudioOutput, AudioOutputLifecycle, AudioRealignCoverage,
    AudioResumeWaterline, AvPacket, BufferedReporter, DemuxCachedSeekInfo, DemuxReaderWatermark,
    DoviPipeline, FfmpegControl, PlaybackBlockReason, PlaybackGeneration, PlaybackOutputScheduler,
    PlaybackOutputSnapshot, PlaybackOutputState, PlaybackScheduler, PositionReporter,
    RebufferAudioRealignRequest, StreamInfo, SubtitleDecodeContext, SubtitlePipeline,
    TimestampMapper, VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VideoDecodePipeline, VideoDecodeRecovery,
    VideoFramePrepareWorker, duration_nsecs,
};

const CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT: Duration = Duration::from_millis(2_500);
const CACHED_SEEK_SOFTWARE_BASE_TIMEOUT: Duration = Duration::from_millis(8_000);
const CACHED_SEEK_SOFTWARE_UHD_EXTRA_TIMEOUT: Duration = Duration::from_millis(4_000);
const CACHED_SEEK_SOFTWARE_MAX_TIMEOUT: Duration = Duration::from_millis(30_000);
const CACHED_SEEK_UHD_PIXEL_THRESHOLD: u64 = 6_000_000;
const CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS: u64 = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS;
const AUDIO_REALIGN_TARGET_TOLERANCE_NSECS: u64 = 500_000_000;
pub(super) const AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER: Duration = Duration::from_millis(500);
pub(super) const AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER: Duration = Duration::from_secs(2);
const AUDIO_REALIGN_MAX_WALL_TIME: Duration = Duration::from_secs(5);
const AUDIO_REALIGN_MAX_OBSERVATIONS: u64 = 64;
const AUDIO_REALIGN_FALLBACK_AFTER_OBSERVATIONS: u64 = 32;
const AUDIO_REALIGN_MAX_PTS_SPAN_NSECS: u64 = 5_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CachedSeekRecoveryFallbackReason {
    FirstVideoFrameTimeout,
    VideoPacketLimit,
}

impl CachedSeekRecoveryFallbackReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::FirstVideoFrameTimeout => "first_video_frame_timeout",
            Self::VideoPacketLimit => "video_packet_limit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CachedSeekRecoveryFallbackAction {
    SoftRecover,
    RecoverHardware,
    LowLevelSeek,
    RecoveryExhausted,
}

impl CachedSeekRecoveryFallbackAction {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::SoftRecover => "soft_recover",
            Self::RecoverHardware => "recover_hardware",
            Self::LowLevelSeek => "low_level_seek",
            Self::RecoveryExhausted => "recovery_exhausted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryFallback {
    pub(super) target_nsecs: u64,
    pub(super) cached_seek: Option<DemuxCachedSeekInfo>,
    pub(super) reason: CachedSeekRecoveryFallbackReason,
    pub(super) action: CachedSeekRecoveryFallbackAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachedSeekRecoveryWatchdogDecision {
    Wait,
    WaitingCachedInput,
    Clear,
    Fallback(CachedSeekRecoveryFallbackReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryWatchdog {
    target_nsecs: u64,
    cached_seek: Option<DemuxCachedSeekInfo>,
    decoder_epoch: u64,
    recovery_transaction_id: u64,
    start_admitted_video_sequence: u64,
    started_at: Instant,
    last_progress_at: Instant,
    start_video_packet_count: u64,
    last_seek_preroll_frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryWatchdogSnapshot {
    pub(super) target_nsecs: u64,
    pub(super) elapsed: Duration,
    pub(super) remaining: Duration,
    pub(super) video_packets_since_seek: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryAttempt {
    target_nsecs: u64,
    soft_recoveries: u8,
    hardware_recoveries: u8,
    low_level_seeks: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedSeekRecoveryProgress {
    video_packets_since_seek: u64,
    video_decode_pending_input_packets: usize,
    video_decode_submitted_not_consumed_packets: usize,
    video_decode_completed_packets: usize,
    video_decode_queued_frames: usize,
    video_decode_state: VideoDecodeWorkerState,
    video_decode_results_pending: bool,
    seek_preroll_frames: u64,
}

impl Default for CachedSeekRecoveryProgress {
    fn default() -> Self {
        Self {
            video_packets_since_seek: 0,
            video_decode_pending_input_packets: 0,
            video_decode_submitted_not_consumed_packets: 0,
            video_decode_completed_packets: 0,
            video_decode_queued_frames: 0,
            video_decode_state: VideoDecodeWorkerState::NeedPacket,
            video_decode_results_pending: false,
            seek_preroll_frames: 0,
        }
    }
}

impl CachedSeekRecoveryProgress {
    fn from_decode_snapshot(
        video_packets_since_seek: u64,
        snapshot: VideoDecodeWorkerSnapshot,
        seek_preroll_frames: u64,
    ) -> Self {
        Self {
            video_packets_since_seek,
            video_decode_pending_input_packets: snapshot.pending_input_packets,
            video_decode_submitted_not_consumed_packets: snapshot.submitted_not_consumed_packets,
            video_decode_completed_packets: snapshot.completed_packets,
            video_decode_queued_frames: snapshot.queued_frames,
            video_decode_state: snapshot.state,
            video_decode_results_pending: snapshot.result_produced_sequence
                != snapshot.result_consumed_sequence,
            seek_preroll_frames,
        }
    }

    fn decoder_work_pending(self) -> bool {
        self.video_decode_pending_input_packets > 0
            || self.video_decode_submitted_not_consumed_packets > 0
            || self.video_decode_completed_packets > 0
            || self.video_decode_queued_frames > 0
            || self.video_decode_results_pending
    }

    fn waiting_cached_input(self) -> bool {
        self.video_decode_state == VideoDecodeWorkerState::NeedPacket
            && !self.decoder_work_pending()
    }

    fn has_actual_progress(self) -> bool {
        self.seek_preroll_frames > 0
            || (self.video_packets_since_seek > 0 && self.decoder_work_pending())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputStreamState {
    pub(super) stream_index: c_int,
    pub(super) packet_input_blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputSnapshot {
    pub(super) demux_streams: Vec<c_int>,
    pub(super) video_stream_index: c_int,
    pub(super) audio_stream_index: Option<c_int>,
    pub(super) subtitle_stream_index: Option<c_int>,
    pub(super) audio_resume_waterline: Option<AudioResumeWaterline>,
    pub(super) audio_output_low_water: bool,
    pub(super) video_decode_snapshot: VideoDecodeWorkerSnapshot,
    pub(super) video_decode_blocked_on: Option<PlaybackBlockReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CachePauseWorkSnapshot {
    pub(super) actual_decode_work: bool,
    pub(super) first_frame_input_demand: bool,
    pub(super) selected_streams: Vec<c_int>,
    pub(super) requested_streams: Vec<c_int>,
    pub(super) video_stream_index: c_int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioRealignTransaction {
    pub(super) transaction_id: u64,
    pub(super) target_timeline_nsecs: u64,
    pub(super) generation: u64,
    pub(super) started_at: Instant,
    pub(super) attempts: u8,
    pub(super) request: RebufferAudioRealignRequest,
    pub(super) phase: AudioRealignPhase,
    pub(super) coverage_nsecs: u64,
    pub(super) coverage_target_nsecs: u64,
    observations: u64,
    first_observed_pts_nsecs: Option<u64>,
    last_observed_pts_nsecs: Option<u64>,
    last_progress_at: Instant,
    warning_emitted: bool,
    fallback_exhausted_logged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioRealignPhase {
    Flushing,
    AwaitingCoverage,
    Covered,
    FallbackUsed,
    MediaGap,
}

impl AudioRealignPhase {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Flushing => "flushing",
            Self::AwaitingCoverage => "awaiting_coverage",
            Self::Covered => "covered",
            Self::FallbackUsed => "fallback_used",
            Self::MediaGap => "media_gap",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioRealignCoalesceReason {
    Flushing,
    AwaitingCoverage,
    CoverageSatisfied,
    LowLevelFallbackAlreadyUsed,
}

impl AudioRealignCoalesceReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Flushing => "flushing",
            Self::AwaitingCoverage => "awaiting_coverage",
            Self::CoverageSatisfied => "coverage_satisfied",
            Self::LowLevelFallbackAlreadyUsed => "low_level_fallback_already_used",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioRealignRequestAction {
    Start,
    Coalesce {
        transaction: AudioRealignTransaction,
        reason: AudioRealignCoalesceReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioRecoveryWatchdogAction {
    Warn {
        transaction: AudioRealignTransaction,
        worker: AudioDecodeWorkerSnapshot,
    },
    LowLevelFallback {
        transaction: AudioRealignTransaction,
        worker: AudioDecodeWorkerSnapshot,
        request: RebufferAudioRealignRequest,
    },
    FallbackExhausted {
        transaction: AudioRealignTransaction,
        worker: AudioDecodeWorkerSnapshot,
    },
}

fn audio_realign_target_matches(left: u64, right: u64) -> bool {
    left.abs_diff(right) <= AUDIO_REALIGN_TARGET_TOLERANCE_NSECS
}

fn observe_audio_realign_request(
    transaction: &mut Option<AudioRealignTransaction>,
    request: RebufferAudioRealignRequest,
) -> AudioRealignRequestAction {
    let Some(current) = transaction.as_mut() else {
        return AudioRealignRequestAction::Start;
    };
    match current.phase {
        AudioRealignPhase::Flushing => {
            let mut merged_request = request;
            merged_request.target_timeline_nsecs = current.target_timeline_nsecs;
            current.request = merged_request;
            AudioRealignRequestAction::Coalesce {
                transaction: *current,
                reason: AudioRealignCoalesceReason::Flushing,
            }
        }
        AudioRealignPhase::AwaitingCoverage => {
            let mut merged_request = request;
            merged_request.target_timeline_nsecs = current.target_timeline_nsecs;
            current.request = merged_request;
            AudioRealignRequestAction::Coalesce {
                transaction: *current,
                reason: AudioRealignCoalesceReason::AwaitingCoverage,
            }
        }
        AudioRealignPhase::FallbackUsed => AudioRealignRequestAction::Coalesce {
            transaction: *current,
            reason: AudioRealignCoalesceReason::LowLevelFallbackAlreadyUsed,
        },
        AudioRealignPhase::MediaGap => AudioRealignRequestAction::Coalesce {
            transaction: *current,
            reason: AudioRealignCoalesceReason::CoverageSatisfied,
        },
        AudioRealignPhase::Covered => {
            if audio_realign_target_matches(
                current.target_timeline_nsecs,
                request.target_timeline_nsecs,
            ) {
                current.request = request;
                return AudioRealignRequestAction::Coalesce {
                    transaction: *current,
                    reason: AudioRealignCoalesceReason::CoverageSatisfied,
                };
            }
            *transaction = None;
            AudioRealignRequestAction::Start
        }
    }
}

fn update_audio_realign_progress(
    transaction: &mut AudioRealignTransaction,
    worker: AudioDecodeWorkerSnapshot,
    coverage: AudioRealignCoverage,
    now: Instant,
) {
    transaction.observations = transaction.observations.saturating_add(1);
    if let Some(accepted_start_nsecs) = coverage.audio_accepted_start_timeline_nsecs {
        transaction
            .first_observed_pts_nsecs
            .get_or_insert(accepted_start_nsecs);
        transaction.last_observed_pts_nsecs = Some(
            accepted_start_nsecs
                .saturating_add(coverage.contiguous_coverage_nsecs.unwrap_or_default()),
        );
    }
    if transaction.phase == AudioRealignPhase::Flushing
        && worker.state != AudioDecodeWorkerState::Recovering
    {
        transaction.phase = AudioRealignPhase::AwaitingCoverage;
        transaction.last_progress_at = now;
    }
    if worker.state == AudioDecodeWorkerState::Recovering
        && let Some(progress_elapsed) = worker.last_result_progress_elapsed
        && let Some(progress_at) = now.checked_sub(progress_elapsed)
        && progress_at > transaction.last_progress_at
    {
        transaction.last_progress_at = progress_at;
    }
    let coverage_nsecs = coverage.contiguous_coverage_nsecs.unwrap_or_default();
    if coverage_nsecs > transaction.coverage_nsecs {
        transaction.coverage_nsecs = coverage_nsecs;
        transaction.last_progress_at = now;
    }
    if coverage.ready && transaction.phase != AudioRealignPhase::MediaGap {
        transaction.phase = AudioRealignPhase::Covered;
        transaction.last_progress_at = now;
    }
}

fn retain_pending_audio_for_realign_once(
    retained: &mut Option<PendingStartAudio>,
    active: &mut PendingStartAudio,
) -> bool {
    if retained.is_some() || active.is_empty() {
        return false;
    }
    *retained = Some(std::mem::take(active));
    true
}

fn audio_realign_completion_ready(
    transaction: Option<AudioRealignTransaction>,
    output_resumed: bool,
    recovery_complete: bool,
) -> bool {
    output_resumed
        && recovery_complete
        && transaction.is_some_and(|transaction| transaction.phase == AudioRealignPhase::Covered)
}

fn poll_audio_recovery_watchdog(
    transaction: &mut AudioRealignTransaction,
    worker: AudioDecodeWorkerSnapshot,
    now: Instant,
) -> Option<AudioRecoveryWatchdogAction> {
    if matches!(
        transaction.phase,
        AudioRealignPhase::Covered | AudioRealignPhase::MediaGap
    ) {
        return None;
    }
    let stalled_for = now.saturating_duration_since(transaction.last_progress_at);
    let observed_pts_span_nsecs = transaction
        .first_observed_pts_nsecs
        .zip(transaction.last_observed_pts_nsecs)
        .map(|(first, last)| first.abs_diff(last))
        .unwrap_or_default();
    let absolute_wall_time_exhausted =
        now.saturating_duration_since(transaction.started_at) >= AUDIO_REALIGN_MAX_WALL_TIME;
    let absolute_observation_bound_exhausted =
        transaction.observations >= AUDIO_REALIGN_MAX_OBSERVATIONS;
    let first_attempt_bound_exhausted = transaction.observations
        >= AUDIO_REALIGN_FALLBACK_AFTER_OBSERVATIONS
        || observed_pts_span_nsecs >= AUDIO_REALIGN_MAX_PTS_SPAN_NSECS;
    let attempt_stalled = stalled_for >= AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;
    let terminal_bound_exhausted = absolute_wall_time_exhausted
        || absolute_observation_bound_exhausted
        || (transaction.phase == AudioRealignPhase::FallbackUsed && attempt_stalled);
    let recovery_bound_exhausted = if transaction.phase == AudioRealignPhase::FallbackUsed {
        terminal_bound_exhausted
    } else {
        attempt_stalled || first_attempt_bound_exhausted || terminal_bound_exhausted
    };
    if recovery_bound_exhausted {
        if transaction.phase != AudioRealignPhase::FallbackUsed {
            transaction.phase = AudioRealignPhase::FallbackUsed;
            transaction.last_progress_at = now;
            transaction.attempts = transaction.attempts.saturating_add(1);
            let mut request = transaction.request;
            request.reason = "audio_realign_coverage_timeout";
            return Some(AudioRecoveryWatchdogAction::LowLevelFallback {
                transaction: *transaction,
                worker,
                request,
            });
        }
        if !transaction.fallback_exhausted_logged {
            transaction.fallback_exhausted_logged = true;
            return Some(AudioRecoveryWatchdogAction::FallbackExhausted {
                transaction: *transaction,
                worker,
            });
        }
        return None;
    }
    if stalled_for >= AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER && !transaction.warning_emitted {
        transaction.warning_emitted = true;
        return Some(AudioRecoveryWatchdogAction::Warn {
            transaction: *transaction,
            worker,
        });
    }
    None
}

pub(super) struct PlaybackPipelineState {
    pub(super) video_stream: StreamInfo,
    pub(super) video_frame_duration_nsecs: u64,
    pub(super) video_decode_pipeline: VideoDecodePipeline,
    pub(super) audio_decode_pipeline: Option<AudioDecodePipeline>,
    pub(super) subtitle_pipeline: SubtitlePipeline,
    pub(super) video_decode_recovery: VideoDecodeRecovery,
    pub(super) playback_generation: PlaybackGeneration,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) decoded_video_frame_count: u64,
    pub(super) dropped_video_frames_before_start_count: u64,
    pub(super) dropped_audio_frames_before_start_count: u64,
    pub(super) video_clock: TimestampMapper,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
    pub(super) audio_clock: TimestampMapper,
    pub(super) audio_output: Option<AudioOutput>,
    pub(super) scheduler: PlaybackScheduler,
    pub(super) output_scheduler: PlaybackOutputScheduler,
    pub(super) dovi_pipeline: DoviPipeline,
    pub(super) buffered_reporter: BufferedReporter,
    pub(super) position_reporter: PositionReporter,
    pub(super) video_frame_prepare_worker: VideoFramePrepareWorker,
    pub(super) current_start_position_nsecs: u64,
    pub(super) video_packet_count: u64,
    pub(super) video_decode_skip_nonref_active: bool,
    pub(super) initial_hevc_cached_exact_seek: bool,
    pub(super) cached_seek_recovery_watchdog: Option<CachedSeekRecoveryWatchdog>,
    pub(super) cached_seek_recovery_attempt: Option<CachedSeekRecoveryAttempt>,
    pub(super) audio_realign_transaction: Option<AudioRealignTransaction>,
    pub(super) audio_realign_retained_pending: Option<PendingStartAudio>,
    pub(super) audio_realign_retained_decoded_frames: Vec<(u64, AudioDecodedFrame)>,
    pub(super) next_recovery_transaction_id: u64,
    pub(super) active_recovery_transaction_id: u64,
}

fn mark_video_decode_skip_nonref_inactive(skip_nonref_active: &mut bool) -> bool {
    let was_active = *skip_nonref_active;
    *skip_nonref_active = false;
    was_active
}

impl PlaybackPipelineState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn service_audio_output_state_machine(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let control_before = control.audio_output_control_snapshot();
        let desired_lifecycle = match self.output_scheduler.playback_output_state {
            PlaybackOutputState::Syncing => AudioOutputLifecycle::Syncing,
            PlaybackOutputState::Primed | PlaybackOutputState::Rebuffering => {
                AudioOutputLifecycle::Ready
            }
            PlaybackOutputState::Playing
                if control_before.paused_by_seek_transition()
                    && control_before.lifecycle() == AudioOutputLifecycle::Syncing =>
            {
                // The scheduler can still describe the old Playing
                // generation while a cache-only seek is selecting its final
                // fallback. Do not infer readiness until the new generation
                // has reached Primed/Ready at least once.
                AudioOutputLifecycle::Syncing
            }
            PlaybackOutputState::Playing => AudioOutputLifecycle::Playing,
        };
        let previous_lifecycle = control.audio_output_lifecycle();
        let seek_transition_released =
            desired_lifecycle == AudioOutputLifecycle::Playing && control.finish_seek_audio_pause();
        let lifecycle_changed = control.set_audio_output_lifecycle(desired_lifecycle);
        if lifecycle_changed || seek_transition_released {
            tracing::debug!(
                session_id = ?session_id,
                previous_audio_output_lifecycle = previous_lifecycle.as_str(),
                audio_output_lifecycle = desired_lifecycle.as_str(),
                seek_transition_released,
                output_state = ?self.output_scheduler.playback_output_state,
                "coordinator reevaluated native audio output lifecycle"
            );
        }

        let Some(output) = self.audio_output.as_ref() else {
            self.output_scheduler.reset_audio_output_activity_watchdog();
            return Ok(());
        };
        let activity = output.activity_snapshot()?;
        let output_state = control.audio_output_control_snapshot();
        let supervision_enabled = self.output_scheduler.playback_output_state
            == PlaybackOutputState::Playing
            && !self.output_scheduler.restart_pending()
            && !output_state.externally_paused()
            && !control.has_pending_seek();
        let underrun_active = output.underrun_active();
        if supervision_enabled
            && underrun_active
            && self
                .output_scheduler
                .audio_output_clock_stall_fallback_active()
        {
            // A bounded re-anchor can briefly expose an empty AO. Keep the
            // video-clock escape hatch armed until the normal underrun refill
            // path reports real callback progress.
            return Ok(());
        }
        let eligible = supervision_enabled && !underrun_active;
        let Some(event) = self.output_scheduler.observe_audio_output_activity(
            now,
            activity,
            eligible,
            output_state.paused_by_seek_transition(),
        ) else {
            return Ok(());
        };

        match event.action {
            AudioOutputActivityWatchdogAction::ReleaseSeekTransition => {
                let released = control.finish_seek_audio_pause();
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    released_seek_transition = released,
                    played_timeline_nsecs = activity.played_timeline_nsecs,
                    shared_buffer_pending_ms =
                        activity.shared_buffer_pending_nsecs as f64 / 1_000_000.0,
                    callback_count = activity.callback_count,
                    consumed_callback_count = activity.consumed_callback_count,
                    silenced_callback_count = activity.silenced_callback_count,
                    underrun_count = activity.underrun_count,
                    audio_output_lifecycle = control.audio_output_lifecycle().as_str(),
                    "native audio output should be consuming but seek transition remained silent"
                );
            }
            AudioOutputActivityWatchdogAction::WarnFrozenClock => {
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    played_timeline_nsecs = activity.played_timeline_nsecs,
                    shared_buffer_pending_ms =
                        activity.shared_buffer_pending_nsecs as f64 / 1_000_000.0,
                    callback_count = activity.callback_count,
                    consumed_callback_count = activity.consumed_callback_count,
                    silenced_callback_count = activity.silenced_callback_count,
                    underrun_count = activity.underrun_count,
                    audio_output_lifecycle = control.audio_output_lifecycle().as_str(),
                    video_clock_fallback = true,
                    "native audio output clock stopped advancing while playback is active"
                );
            }
            AudioOutputActivityWatchdogAction::RecoverAndReanchor => {
                let reanchor_timeline_nsecs = self
                    .scheduler
                    .current_timeline_nsecs()
                    .max(activity.played_timeline_nsecs);
                control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
                output.reset_clock(reanchor_timeline_nsecs);
                let recovered = recover_pending_start_audio_after_underrun(
                    &mut self.output_scheduler.pending_start_audio,
                    output,
                    control,
                    &mut self.output_scheduler.scheduled_video_queue,
                    session_id,
                    vo_queue,
                    frame_presented,
                    &mut self.position_reporter,
                    event_tx,
                    &mut self.subtitle_pipeline,
                    &mut self.buffered_reporter,
                )?;
                if recovered {
                    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
                }
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    previous_played_timeline_nsecs = activity.played_timeline_nsecs,
                    reanchor_timeline_nsecs,
                    recovered_pending_audio = recovered,
                    pending_audio_frames = self.output_scheduler.pending_start_audio.len(),
                    pending_audio_ms = self
                        .output_scheduler
                        .pending_start_audio
                        .buffered_duration()
                        .as_secs_f64()
                        * 1000.0,
                    video_clock_fallback = true,
                    "bounded native audio output stall recovery reset and re-anchored the clock"
                );
            }
        }
        Ok(())
    }

    pub(super) fn cache_pause_work_snapshot(&self) -> CachePauseWorkSnapshot {
        let output = self.output_scheduler.snapshot();
        let decode = self.video_decode_pipeline.snapshot();
        let prepare = self.video_frame_prepare_worker.snapshot();
        let decoder_input = self.decoder_input_snapshot(false);
        let video_packet_input_available = decoder_input
            .demux_streams
            .contains(&decoder_input.video_stream_index);
        let mut selected_streams = vec![decoder_input.video_stream_index];
        selected_streams.extend(decoder_input.audio_stream_index);
        selected_streams.extend(decoder_input.subtitle_stream_index);
        selected_streams.sort_unstable();
        selected_streams.dedup();
        CachePauseWorkSnapshot {
            actual_decode_work: cache_pause_actual_decode_work_for(decode, prepare),
            first_frame_input_demand: cache_pause_first_frame_input_demand_for(
                output,
                video_packet_input_available,
            ),
            selected_streams,
            requested_streams: decoder_input.demux_streams,
            video_stream_index: decoder_input.video_stream_index,
        }
    }

    pub(super) fn output_backpressure_prefetch_should_pause(&self) -> bool {
        output_backpressure_prefetch_should_pause_for(
            self.output_scheduler.playback_output_state,
            self.video_decode_pipeline.snapshot().state,
            self.audio_decode_pipeline
                .as_ref()
                .map(|pipeline| pipeline.snapshot().state),
        )
    }

    pub(super) fn exact_seek_actual_anchor_nsecs(&self) -> Option<u64> {
        match self.video_decode_recovery.recovery_scope() {
            VideoDecodeRecoveryScope::ExactLowLevelSeek {
                actual_anchor_nsecs,
                ..
            } => Some(actual_anchor_nsecs),
            VideoDecodeRecoveryScope::ExactCachedSeek { .. } => self
                .cached_seek_recovery_watchdog
                .and_then(|watchdog| watchdog.cached_seek)
                .map(|cached_seek| cached_seek.anchor_nsecs),
            VideoDecodeRecoveryScope::SafeBoundary => None,
        }
    }

    pub(super) fn begin_recovery_transaction(&mut self) -> u64 {
        let transaction_id = self.next_recovery_transaction_id.max(1);
        self.next_recovery_transaction_id = transaction_id.saturating_add(1).max(1);
        self.active_recovery_transaction_id = transaction_id;
        transaction_id
    }

    pub(super) fn active_recovery_transaction_id(&self) -> u64 {
        self.active_recovery_transaction_id
    }

    pub(super) fn continue_recovery_transaction(&mut self, transaction_id: u64) {
        let transaction_id = transaction_id.max(1);
        self.active_recovery_transaction_id = transaction_id;
        self.next_recovery_transaction_id = self
            .next_recovery_transaction_id
            .max(transaction_id.saturating_add(1).max(1));
    }

    pub(super) fn advance_playback_generation(&mut self) -> u64 {
        self.output_scheduler.advance_discontinuity_epoch();
        self.playback_generation.advance()
    }

    pub(super) fn flush_playback_generation(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.restore_video_decode_skip_nonref_default(None, "playback_generation_flush")?;
        self.video_decode_pipeline
            .reset_hevc_decode_chain_recovery_transaction();
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(super) fn flush_playback_generation_preserving_hevc_same_hardware_recovery(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.restore_video_decode_skip_nonref_default(None, "same_vulkan_cached_safe_idr_rebuild")?;
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(super) fn observe_rebuffer_audio_realign_request(
        &mut self,
        request: RebufferAudioRealignRequest,
    ) -> AudioRealignRequestAction {
        self.refresh_audio_realign_progress();
        observe_audio_realign_request(&mut self.audio_realign_transaction, request)
    }

    pub(super) fn begin_audio_realign_transaction(
        &mut self,
        transaction_id: u64,
        request: RebufferAudioRealignRequest,
        generation: u64,
        started_at: Instant,
    ) {
        self.continue_recovery_transaction(transaction_id);
        self.audio_realign_transaction = Some(AudioRealignTransaction {
            transaction_id,
            target_timeline_nsecs: request.target_timeline_nsecs,
            generation,
            started_at,
            attempts: 1,
            request,
            phase: AudioRealignPhase::Flushing,
            coverage_nsecs: 0,
            coverage_target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)),
            observations: 0,
            first_observed_pts_nsecs: None,
            last_observed_pts_nsecs: None,
            last_progress_at: started_at,
            warning_emitted: false,
            fallback_exhausted_logged: false,
        });
    }

    pub(super) fn retain_audio_for_realign(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        if retain_pending_audio_for_realign_once(
            &mut self.audio_realign_retained_pending,
            &mut self.output_scheduler.pending_start_audio,
        ) {
            let retained = self
                .audio_realign_retained_pending
                .as_ref()
                .expect("retained pending audio was just installed");
            tracing::debug!(
                session_id = ?session_id,
                reason,
                retained_pending_audio_frames = retained.len(),
                retained_pending_audio_ms = retained.buffered_duration().as_secs_f64() * 1000.0,
                retained_pending_audio_range_nsecs = ?retained.range_nsecs(),
                retained_pending_audio_contiguous_range_nsecs = ?retained.contiguous_range_nsecs(),
                retained_pending_audio_first_gap_nsecs = ?retained.first_gap_nsecs(),
                "retained pending FFmpeg audio until realign reaches a terminal result"
            );
        }
        if let Some(audio_decode_pipeline) = self.audio_decode_pipeline.as_mut() {
            let retained_frames = audio_decode_pipeline.take_output_frames_for_realign();
            if !retained_frames.is_empty() {
                tracing::debug!(
                    session_id = ?session_id,
                    reason,
                    retained_decoded_frames = retained_frames.len(),
                    first_retained_generation = ?retained_frames
                        .first()
                        .map(|(generation, _)| *generation),
                    last_retained_generation = ?retained_frames
                        .last()
                        .map(|(generation, _)| *generation),
                    first_retained_raw_timestamp = ?retained_frames
                        .first()
                        .map(|(_, frame)| frame.raw_timestamp),
                    last_retained_raw_timestamp = ?retained_frames
                        .last()
                        .map(|(_, frame)| frame.raw_timestamp),
                    retained_decoded_ms = retained_frames
                        .iter()
                        .map(|(_, frame)| frame.audio.duration_nsecs)
                        .fold(0_u64, u64::saturating_add) as f64
                        / 1_000_000.0,
                    "retained decoded FFmpeg audio frames across realign"
                );
            }
            self.audio_realign_retained_decoded_frames
                .extend(retained_frames);
        }
    }

    fn discard_audio_retained_for_completed_realign(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let retained_pending = self.audio_realign_retained_pending.take();
        let retained_decoded = std::mem::take(&mut self.audio_realign_retained_decoded_frames);
        if retained_pending.is_none() && retained_decoded.is_empty() {
            return;
        }
        tracing::debug!(
            session_id = ?session_id,
            reason,
            discarded_retained_pending_audio_frames = retained_pending
                .as_ref()
                .map(PendingStartAudio::len)
                .unwrap_or_default(),
            discarded_retained_pending_audio_ms = retained_pending
                .as_ref()
                .map(PendingStartAudio::buffered_duration)
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0,
            discarded_retained_pending_audio_range_nsecs = ?retained_pending
                .as_ref()
                .and_then(PendingStartAudio::range_nsecs),
            discarded_retained_decoded_frames = retained_decoded.len(),
            discarded_retained_decoded_first_raw_timestamp = ?retained_decoded
                .first()
                .map(|(_, frame)| frame.raw_timestamp),
            discarded_retained_decoded_last_raw_timestamp = ?retained_decoded
                .last()
                .map(|(_, frame)| frame.raw_timestamp),
            "released pre-realign FFmpeg audio after realign reached a terminal result"
        );
    }

    pub(super) fn finish_audio_realign_as_confirmed_media_gap(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> Option<(AudioRealignTransaction, u64)> {
        let mut transaction = self.audio_realign_transaction.take()?;
        transaction.phase = AudioRealignPhase::MediaGap;
        let resume_timeline_nsecs = self
            .output_scheduler
            .resume_after_confirmed_audio_media_gap(
                transaction.target_timeline_nsecs,
                transaction.request.far_ahead_audio_timeline_nsecs,
                control,
                session_id,
            );
        self.scheduler.reset(resume_timeline_nsecs);
        if let Some(audio_output) = self.audio_output.as_ref() {
            audio_output.reset_clock(resume_timeline_nsecs);
        }
        self.current_start_position_nsecs =
            self.current_start_position_nsecs.max(resume_timeline_nsecs);
        self.discard_audio_retained_for_completed_realign(
            session_id,
            "audio_realign_confirmed_media_gap",
        );
        Some((transaction, resume_timeline_nsecs))
    }

    pub(super) fn update_audio_realign_recovery_generation(&mut self, generation: u64) {
        if let Some(transaction) = self.audio_realign_transaction.as_mut() {
            transaction.generation = generation;
            transaction.phase = AudioRealignPhase::FallbackUsed;
            transaction.coverage_nsecs = 0;
            transaction.last_progress_at = Instant::now();
        }
    }

    pub(super) fn poll_audio_recovery_watchdog(&mut self) -> Option<AudioRecoveryWatchdogAction> {
        self.refresh_audio_realign_progress();
        let worker = self.audio_decode_pipeline.as_ref()?.snapshot();
        let transaction = self.audio_realign_transaction.as_mut()?;
        poll_audio_recovery_watchdog(transaction, worker, Instant::now())
    }

    fn refresh_audio_realign_progress(&mut self) {
        let Some(transaction) = self.audio_realign_transaction else {
            return;
        };
        let Some(worker) = self
            .audio_decode_pipeline
            .as_ref()
            .map(AudioDecodePipeline::snapshot)
        else {
            return;
        };
        let coverage = self.output_scheduler.audio_realign_coverage(
            transaction.target_timeline_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        );
        if let Some(transaction) = self.audio_realign_transaction.as_mut() {
            let previous_phase = transaction.phase;
            let previous_coverage_nsecs = transaction.coverage_nsecs;
            update_audio_realign_progress(transaction, worker, coverage, Instant::now());
            if transaction.phase != previous_phase
                || transaction.coverage_nsecs != previous_coverage_nsecs
            {
                tracing::debug!(
                    transaction_id = transaction.transaction_id,
                    recovery_scope = "audio_realign",
                    target_timeline_nsecs = transaction.target_timeline_nsecs,
                    transaction_generation = transaction.generation,
                    previous_phase = previous_phase.as_str(),
                    phase = transaction.phase.as_str(),
                    audio_accepted_start = ?coverage.audio_accepted_start_timeline_nsecs,
                    start_gap_ms = ?coverage
                        .start_gap_nsecs
                        .map(|gap| gap as f64 / 1_000_000.0),
                    contiguous_coverage_ms = ?coverage
                        .contiguous_coverage_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    coverage_target_ms = coverage.protected_target_nsecs as f64 / 1_000_000.0,
                    recovery_satisfied = transaction.phase == AudioRealignPhase::Covered,
                    fallback_eligible = false,
                    "updated FFmpeg audio realign transaction coverage"
                );
            }
        }
    }

    pub(super) fn clear_audio_realign_transaction(&mut self) {
        self.audio_realign_transaction = None;
        self.audio_realign_retained_pending = None;
        self.audio_realign_retained_decoded_frames.clear();
    }

    pub(super) fn clear_audio_realign_transaction_after_resume(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<AudioRealignTransaction> {
        self.refresh_audio_realign_progress();
        let output_resumed = self.output_scheduler.snapshot().state == PlaybackOutputState::Playing;
        let recovery_complete = self
            .audio_decode_pipeline
            .as_ref()
            .is_none_or(|pipeline| pipeline.snapshot().state != AudioDecodeWorkerState::Recovering);
        if !audio_realign_completion_ready(
            self.audio_realign_transaction,
            output_resumed,
            recovery_complete,
        ) {
            return None;
        }
        let transaction = self.audio_realign_transaction.take()?;
        self.discard_audio_retained_for_completed_realign(
            session_id,
            "audio_realign_coverage_confirmed",
        );
        Some(transaction)
    }

    pub(super) fn restore_video_decode_skip_nonref_default(
        &mut self,
        session_id: Option<PlaybackSessionId>,
        reason: &'static str,
    ) -> std::result::Result<(), String> {
        self.video_decode_pipeline.set_skip_nonref_frames(false)?;
        let was_active =
            mark_video_decode_skip_nonref_inactive(&mut self.video_decode_skip_nonref_active);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            was_active,
            "resetting FFmpeg video decode nonref skip to default"
        );
        Ok(())
    }

    pub(super) fn soft_recover_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<(), String> {
        let transaction_id = self
            .video_decode_recovery
            .recovery_scope()
            .transaction_id()
            .unwrap_or_else(|| self.begin_recovery_transaction());
        self.active_recovery_transaction_id = transaction_id;
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.advance_playback_generation();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.video_decode_pipeline.clear_packets();
        self.dovi_pipeline.reset();
        tracing::debug!(
            session_id = ?session_id,
            transaction_id,
            recovery_scope = self.video_decode_recovery.recovery_scope().as_str(),
            generation,
            "soft recovered HEVC decode chain while waiting for first decoded video frame"
        );
        Ok(())
    }

    pub(super) fn soft_recover_cached_seek_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        let transaction_id = self
            .video_decode_recovery
            .recovery_scope()
            .transaction_id()
            .unwrap_or_else(|| self.begin_recovery_transaction());
        self.active_recovery_transaction_id = transaction_id;
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.advance_playback_generation();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.dovi_pipeline.reset();
        // Keep the safe hardware packet journal intact: it belongs to the subsequent
        // software fallback transaction, not to this lightweight hardware flush.
        let requeued_probe_packets = 0;
        tracing::debug!(
            session_id = ?session_id,
            transaction_id,
            recovery_scope = self.video_decode_recovery.recovery_scope().as_str(),
            generation,
            requeued_probe_packets,
            "soft recovered HEVC cached seek decode chain without low-level seek"
        );
        Ok(requeued_probe_packets)
    }

    pub(super) fn begin_cached_seek_recovery_watchdog(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(target_nsecs, None, session_id);
    }

    pub(super) fn begin_cached_seek_recovery_watchdog_for_hit(
        &mut self,
        cached_seek: DemuxCachedSeekInfo,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(
            cached_seek.target_nsecs,
            Some(cached_seek),
            session_id,
        );
    }

    pub(super) fn rearm_cached_seek_recovery_watchdog(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(
            target_nsecs,
            cached_seek,
            session_id,
        );
    }

    fn begin_cached_seek_recovery_watchdog_with_context(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
        session_id: PlaybackSessionId,
    ) {
        if self.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.cached_seek_recovery_watchdog = None;
            return;
        }
        let previous_target_nsecs = self
            .cached_seek_recovery_watchdog
            .map(|watchdog| watchdog.target_nsecs);
        let decoder_epoch = self.video_decode_pipeline.decoder_epoch();
        let recovery_transaction_id = self.active_recovery_transaction_id();
        let admitted_video_sequence = self.video_decode_pipeline.admitted_video_sequence();
        let (watchdog, started) = cached_seek_recovery_watchdog_after_begin(
            self.cached_seek_recovery_watchdog,
            target_nsecs,
            cached_seek,
            decoder_epoch,
            recovery_transaction_id,
            admitted_video_sequence,
            Instant::now(),
            self.video_packet_count,
        );
        self.cached_seek_recovery_watchdog = Some(watchdog);
        if started {
            tracing::debug!(
                ?session_id,
                target_nsecs,
                decoder_epoch,
                recovery_transaction_id,
                start_admitted_video_sequence = admitted_video_sequence,
                range_id = ?cached_seek.map(|info| info.range_id),
                anchor_packet_id = ?cached_seek.map(|info| info.anchor_packet_id),
                anchor_kind = ?cached_seek.map(|info| info.anchor_kind.as_str()),
                anchor_nsecs = ?cached_seek.map(|info| info.anchor_nsecs),
                preroll_nsecs = ?cached_seek.map(|info| info.preroll_nsecs),
                video_packet_count = self.video_packet_count,
                "started HEVC cached seek recovery watchdog"
            );
        } else {
            tracing::debug!(
                ?session_id,
                previous_target_nsecs,
                target_nsecs,
                decoder_epoch = watchdog.decoder_epoch,
                recovery_transaction_id = watchdog.recovery_transaction_id,
                start_admitted_video_sequence = watchdog.start_admitted_video_sequence,
                range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                total_elapsed_ms = watchdog.started_at.elapsed().as_secs_f64() * 1000.0,
                video_packets_since_start = self
                    .video_packet_count
                    .saturating_sub(watchdog.start_video_packet_count),
                "rearmed HEVC cached seek recovery watchdog after internal recovery"
            );
        }
    }

    pub(super) fn clear_cached_seek_recovery_watchdog(&mut self) {
        self.cached_seek_recovery_watchdog = None;
        self.cached_seek_recovery_attempt = None;
    }

    pub(super) fn active_cra_cached_seek(&self) -> Option<DemuxCachedSeekInfo> {
        self.cached_seek_recovery_watchdog
            .and_then(|watchdog| watchdog.cached_seek)
            .filter(|info| info.uses_cra_anchor())
    }

    pub(super) fn cached_seek_recovery_watchdog_deadline(&self) -> Option<Instant> {
        self.cached_seek_recovery_watchdog.map(|watchdog| {
            watchdog.last_progress_at + self.cached_seek_recovery_timeout(watchdog.target_nsecs)
        })
    }

    fn cached_seek_recovery_timeout(&self, target_nsecs: u64) -> Duration {
        let info = self.video_decode_pipeline.info();
        let stats = self.video_decode_pipeline.hevc_decode_chain_stats();
        cached_seek_recovery_timeout(
            info.hardware_accelerated,
            info.size,
            target_nsecs,
            stats.first_zero_output_packet_nsecs,
        )
    }

    pub(super) fn playback_loop_deadline(&self) -> PlaybackLoopDeadline {
        PlaybackLoopDeadline::from_cached_seek_recovery_watchdog(
            self.cached_seek_recovery_watchdog_deadline(),
        )
        .with_hevc_startup_stall_watchdog_deadline(
            self.video_decode_pipeline
                .hevc_startup_stall_watchdog_deadline(),
        )
        .with_audio_decode_recovery_watchdog_deadline(
            self.audio_decode_recovery_watchdog_deadline(),
        )
        .with_output_housekeeping_deadline(self.output_scheduler.output_housekeeping_deadline())
    }

    fn audio_decode_recovery_watchdog_deadline(&self) -> Option<Instant> {
        let transaction = self.audio_realign_transaction?;
        if matches!(
            transaction.phase,
            AudioRealignPhase::Covered | AudioRealignPhase::MediaGap
        ) || transaction.fallback_exhausted_logged
        {
            return None;
        }
        let threshold = if transaction.warning_emitted
            || transaction.phase == AudioRealignPhase::FallbackUsed
        {
            AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER
        } else {
            AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER
        };
        Some(
            (transaction.last_progress_at + threshold)
                .min(transaction.started_at + AUDIO_REALIGN_MAX_WALL_TIME),
        )
    }

    pub(super) fn cached_seek_recovery_watchdog_expired(&self) -> bool {
        self.cached_seek_recovery_watchdog_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(super) fn cached_seek_recovery_watchdog_snapshot(
        &self,
    ) -> Option<CachedSeekRecoveryWatchdogSnapshot> {
        let watchdog = self.cached_seek_recovery_watchdog?;
        let timeout = self.cached_seek_recovery_timeout(watchdog.target_nsecs);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(watchdog.started_at);
        let stalled = now.saturating_duration_since(watchdog.last_progress_at);
        Some(CachedSeekRecoveryWatchdogSnapshot {
            target_nsecs: watchdog.target_nsecs,
            elapsed,
            remaining: timeout.saturating_sub(stalled),
            video_packets_since_seek: self
                .video_packet_count
                .saturating_sub(watchdog.start_video_packet_count),
        })
    }

    pub(super) fn take_cached_seek_recovery_fallback(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<CachedSeekRecoveryFallback> {
        let mut watchdog = self.cached_seek_recovery_watchdog?;
        let now = Instant::now();
        let output_snapshot = self.output_scheduler.snapshot();
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let elapsed = now.saturating_duration_since(watchdog.started_at);
        let video_packets_since_seek = self
            .video_packet_count
            .saturating_sub(watchdog.start_video_packet_count);
        let progress = CachedSeekRecoveryProgress::from_decode_snapshot(
            video_packets_since_seek,
            video_decode_snapshot,
            self.video_decode_recovery.seek_bootstrap_preroll_frames(),
        );
        let admitted_video_sequence = self.video_decode_pipeline.admitted_video_sequence();
        let matching_admitted_progress = self.video_decode_pipeline.decoder_epoch()
            == watchdog.decoder_epoch
            && self.video_decode_pipeline.last_admitted_decoder_epoch()
                == Some(watchdog.decoder_epoch)
            && admitted_video_sequence > watchdog.start_admitted_video_sequence
            && self.active_recovery_transaction_id() == watchdog.recovery_transaction_id;
        if progress.seek_preroll_frames > watchdog.last_seek_preroll_frames {
            watchdog.last_seek_preroll_frames = progress.seek_preroll_frames;
            watchdog.last_progress_at = now;
        }
        self.cached_seek_recovery_watchdog = Some(watchdog);
        let timeout = self.cached_seek_recovery_timeout(watchdog.target_nsecs);
        let stalled = now.saturating_duration_since(watchdog.last_progress_at);
        tracing::trace!(
            ?session_id,
            target_nsecs = watchdog.target_nsecs,
            range_id = ?watchdog.cached_seek.map(|info| info.range_id),
            anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
            anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
            anchor_nsecs = ?watchdog.cached_seek.map(|info| info.anchor_nsecs),
            preroll_nsecs = ?watchdog.cached_seek.map(|info| info.preroll_nsecs),
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            stalled_ms = stalled.as_secs_f64() * 1000.0,
            remaining_ms = timeout.saturating_sub(stalled).as_secs_f64() * 1000.0,
            timeout_ms = timeout.as_secs_f64() * 1000.0,
            video_packets_since_seek,
            video_decode_pending_input_packets = progress.video_decode_pending_input_packets,
            video_decode_submitted_not_consumed_packets = progress.video_decode_submitted_not_consumed_packets,
            video_decode_completed_packets = progress.video_decode_completed_packets,
            video_decode_queued_frames = progress.video_decode_queued_frames,
            seek_preroll_frames = progress.seek_preroll_frames,
            decoder_work_pending = progress.decoder_work_pending(),
            queued_video_frames = output_snapshot.queued_video_frames,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            decoder_epoch = watchdog.decoder_epoch,
            recovery_transaction_id = watchdog.recovery_transaction_id,
            start_admitted_video_sequence = watchdog.start_admitted_video_sequence,
            admitted_video_sequence,
            matching_admitted_progress,
            "checked HEVC cached seek recovery watchdog"
        );
        match cached_seek_recovery_watchdog_decision(
            stalled,
            timeout,
            progress,
            matching_admitted_progress,
        ) {
            CachedSeekRecoveryWatchdogDecision::Wait => None,
            CachedSeekRecoveryWatchdogDecision::WaitingCachedInput => {
                watchdog.last_progress_at = now;
                self.cached_seek_recovery_watchdog = Some(watchdog);
                tracing::trace!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    video_packets_since_seek,
                    video_decode_state = ?progress.video_decode_state,
                    gate_reason = "waiting_cached_input",
                    "paused HEVC cached seek recovery watchdog while decoder waits for input"
                );
                None
            }
            CachedSeekRecoveryWatchdogDecision::Clear => {
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                    anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                    anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    preroll_decode_ms = elapsed.as_secs_f64() * 1000.0,
                    stalled_ms = stalled.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    decoder_epoch = watchdog.decoder_epoch,
                    recovery_transaction_id = watchdog.recovery_transaction_id,
                    admitted_video_sequence,
                    cached_seek_succeeded = true,
                    low_level_fallback = false,
                    "completed HEVC cached seek recovery at first target video frame"
                );
                self.cached_seek_recovery_watchdog = None;
                self.cached_seek_recovery_attempt = None;
                None
            }
            CachedSeekRecoveryWatchdogDecision::Fallback(reason) => {
                let action = self
                    .cached_seek_recovery_next_action(watchdog.target_nsecs, watchdog.cached_seek);
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                    anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                    anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                    reason = reason.as_str(),
                    action = action.as_str(),
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    stalled_ms = stalled.as_secs_f64() * 1000.0,
                    timeout_ms = timeout.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    video_decode_pending_input_packets =
                        progress.video_decode_pending_input_packets,
                    video_decode_submitted_not_consumed_packets = progress.video_decode_submitted_not_consumed_packets,
                    video_decode_completed_packets = progress.video_decode_completed_packets,
                    video_decode_queued_frames = progress.video_decode_queued_frames,
                    seek_preroll_frames = progress.seek_preroll_frames,
                    decoder_work_pending = progress.decoder_work_pending(),
                    max_video_packets = CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    recovery_waiting = self.video_decode_recovery.waiting_for_keyframe(),
                    recovery_skipped_packets = self.video_decode_recovery.skipped_packets(),
                    "HEVC cached seek recovery watchdog requesting fallback"
                );
                self.cached_seek_recovery_watchdog = None;
                Some(CachedSeekRecoveryFallback {
                    target_nsecs: watchdog.target_nsecs,
                    cached_seek: watchdog.cached_seek,
                    reason,
                    action,
                })
            }
        }
    }

    fn cached_seek_recovery_next_action(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
    ) -> CachedSeekRecoveryFallbackAction {
        cached_seek_recovery_next_action_for_attempt(
            &mut self.cached_seek_recovery_attempt,
            target_nsecs,
            self.video_decode_pipeline.info().hardware_accelerated,
            cached_seek.is_some_and(DemuxCachedSeekInfo::uses_cra_anchor),
        )
    }

    pub(super) fn decoder_outputs_pending_or_in_flight(&self) -> bool {
        self.video_decode_pipeline.has_pending_or_in_flight()
            || self
                .audio_decode_pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.has_pending_or_in_flight())
            || self.subtitle_pipeline.has_pending_or_in_flight()
    }

    pub(super) fn start_decoder_drain_phase(
        &mut self,
    ) -> std::result::Result<PlaybackDrainPhase, String> {
        PlaybackDrainPhase::start(
            &mut self.playback_generation,
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn poll_decoder_drain_phase(
        &mut self,
        drain_phase: &mut PlaybackDrainPhase,
    ) -> std::result::Result<Option<PlaybackDrainResults>, String> {
        drain_phase.poll(
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn video_drain_frame_processor(
        &mut self,
        video_drain_result: VideoDecodeDrainResult,
    ) -> VideoDecodeDrainFrameProcessor {
        let video_prepare_generation = self.playback_generation.advance();
        VideoDecodeDrainFrameProcessor::new(
            video_drain_result,
            video_prepare_generation,
            self.decoded_video_frame_count,
        )
    }

    pub(super) fn poll_video_drain_processor(
        &mut self,
        processor: &mut VideoDecodeDrainFrameProcessor,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<VideoDecodeDrainProcessStatus, String> {
        processor.poll(
            &self.video_decode_pipeline,
            self.video_frame_duration_nsecs,
            &mut self.video_clock,
            &mut self.playback_timeline_origin_nsecs,
            &mut self.subtitle_pipeline,
            &mut self.current_start_position_nsecs,
            &mut self.dovi_pipeline,
            self.audio_output.as_ref(),
            &mut self.output_scheduler,
            vo_queue,
            &mut self.video_frame_prepare_worker,
            control,
            session_id,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.buffered_reporter,
            &mut self.scheduler,
        )
    }

    pub(super) fn process_audio_drain_result(
        &mut self,
        audio_drain_result: AudioDecodePacketResult,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<(), String> {
        let audio_time_base = self
            .audio_decode_pipeline
            .as_ref()
            .map(|worker| worker.info().time_base);
        process_audio_decode_drain_result(
            audio_drain_result,
            self.audio_decode_pipeline.as_mut(),
            audio_time_base,
            control,
            self.audio_output.as_ref(),
            &mut self.audio_clock,
            self.current_start_position_nsecs,
            &mut self.dropped_audio_frames_before_start_count,
            &mut self.output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.subtitle_pipeline,
            &mut self.buffered_reporter,
        )
    }

    pub(super) fn retry_pending_decoder_inputs(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let video_retry_status = if self
            .output_scheduler
            .decode_recovery_video_admission_blocked()
            || self
                .video_decode_pipeline
                .hevc_resource_pressure_decoder_input_stopped()
        {
            None
        } else {
            Some(self.video_decode_pipeline.retry_pending_input(session_id)?)
        };

        let audio_retry_status = if self.audio_input_suppressed_until_output_resume() {
            None
        } else {
            self.audio_decode_pipeline
                .as_mut()
                .map(|worker| worker.retry_pending_input(session_id))
                .transpose()?
        };

        let subtitle_retry_status = self.subtitle_pipeline.retry_pending_input(
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )?;

        Ok(decoder_input_retry_status_from_streams([
            video_retry_status,
            audio_retry_status,
            Some(subtitle_retry_status),
        ]))
    }

    pub(super) fn video_decode_stream_index(&self) -> c_int {
        self.video_decode_pipeline.info().stream_index
    }

    pub(super) fn decoder_input_snapshot(
        &self,
        output_resource_pressure: bool,
    ) -> DecoderInputSnapshot {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let video_decode_blocked_on = video_decode_block_reason_with_output_queue(
            VideoDecodePipeline::block_reason_for(
                video_decode_snapshot,
                self.video_decode_pipeline.info(),
            ),
            output_resource_pressure,
        );
        let video_stream_index = self.video_decode_stream_index();
        let audio_decode_snapshot = self
            .audio_decode_pipeline
            .as_ref()
            .map(|pipeline| pipeline.snapshot());
        let audio_snapshot = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_snapshot, audio_decode_snapshot);
        let audio_output_low_water = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs < duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)
        });
        let audio_input_suppressed = !self.output_scheduler.decode_recovery_active()
            && (self
                .output_scheduler
                .output_wait_audio_input_backpressured()
                || audio_input_suppressed_until_output_resume_state(
                    self.audio_decode_pipeline.is_some(),
                    self.output_scheduler.waiting_for_output_resume(),
                    audio_resume_waterline,
                ));
        let audio_stream = self.audio_decode_pipeline.as_ref().map(|pipeline| {
            let audio_decode_snapshot =
                audio_decode_snapshot.expect("audio snapshot exists when pipeline exists");
            DecoderInputStreamState {
                stream_index: pipeline.info().stream_index,
                packet_input_blocked: audio_input_suppressed
                    || decoder_block_reason_blocks_packet_input(
                        AudioDecodePipeline::block_reason_for(audio_decode_snapshot),
                    ),
            }
        });
        let subtitle_stream = self.subtitle_pipeline.stream_index().map(|stream_index| {
            let subtitle_decode_blocked_on = self
                .subtitle_pipeline
                .snapshot()
                .and_then(SubtitlePipeline::block_reason_for);
            DecoderInputStreamState {
                stream_index,
                packet_input_blocked: decoder_block_reason_blocks_packet_input(
                    subtitle_decode_blocked_on,
                ),
            }
        });

        DecoderInputSnapshot {
            demux_streams: decoder_input_streams_for_state(
                DecoderInputStreamState {
                    stream_index: video_stream_index,
                    packet_input_blocked: decoder_block_reason_blocks_packet_input(
                        video_decode_blocked_on,
                    ),
                },
                audio_stream,
                subtitle_stream,
            ),
            video_stream_index,
            audio_stream_index: audio_stream.map(|stream| stream.stream_index),
            subtitle_stream_index: subtitle_stream.map(|stream| stream.stream_index),
            audio_resume_waterline,
            audio_output_low_water,
            video_decode_snapshot,
            video_decode_blocked_on,
        }
    }

    fn audio_input_suppressed_until_output_resume(&self) -> bool {
        if self.output_scheduler.decode_recovery_active() {
            return false;
        }
        let audio_decode_snapshot = self
            .audio_decode_pipeline
            .as_ref()
            .map(|pipeline| pipeline.snapshot());
        let audio_snapshot = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_snapshot, audio_decode_snapshot);
        audio_input_suppressed_until_output_resume_state(
            self.audio_decode_pipeline.is_some(),
            self.output_scheduler.waiting_for_output_resume(),
            audio_resume_waterline,
        )
    }

    fn audio_resume_waterline_for_output_wait(
        &self,
        audio_snapshot: Option<super::AudioOutputSnapshot>,
        audio_decode_snapshot: Option<AudioDecodeWorkerSnapshot>,
    ) -> Option<AudioResumeWaterline> {
        self.output_scheduler
            .audio_resume_waterline_for_output_wait(
                audio_snapshot,
                audio_decode_snapshot
                    .map(|snapshot| snapshot.queued_duration_nsecs)
                    .unwrap_or_default(),
                audio_decode_snapshot
                    .map(|snapshot| snapshot.in_flight_packets)
                    .unwrap_or_default(),
                self.current_start_position_nsecs,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                None,
                None,
            )
    }

    pub(super) fn video_packet_admission_pressure(
        &self,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        vo_snapshot: VideoOutputQueueSnapshot,
    ) -> VideoPacketAdmissionPressure {
        let output_snapshot = self
            .output_scheduler
            .snapshot_for_played_until(played_until_nsecs);
        let audio_output_pending_nsecs = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok())
            .map(|snapshot| snapshot.total_pending_nsecs);
        let output_resource_pressure = self.video_output_resource_pressure_for(
            output_snapshot,
            vo_snapshot,
            audio_output_pending_nsecs,
        );
        VideoPacketAdmissionPressure {
            output_snapshot,
            skip_nonref_for_pressure: self.output_scheduler.video_decode_skip_nonref_for_pressure(
                self.video_stream.codec_id,
                played_until_nsecs,
                has_audio_output,
                audio_output_pending_nsecs,
                self.video_decode_skip_nonref_active,
            ),
            played_until_nsecs,
            output_resource_pressure,
        }
    }

    pub(super) fn video_output_resource_pressure_for(
        &self,
        output_snapshot: PlaybackOutputSnapshot,
        vo_snapshot: VideoOutputQueueSnapshot,
        audio_output_pending_nsecs: Option<u64>,
    ) -> bool {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let prepare_snapshot = self.video_frame_prepare_worker.snapshot();
        let prepare_worker_frames = prepare_snapshot
            .pending_input_frames
            .saturating_add(prepare_snapshot.in_flight_frames)
            .saturating_add(prepare_snapshot.completed_frames);
        let scheduled_video_queue_limit_reached = self
            .output_scheduler
            .scheduled_video_queue_limit_reached(self.subtitle_pipeline.needs_prefetch());
        video_output_resource_pressure(VideoOutputResourcePressure {
            scheduled_video_frames: self.output_scheduler.scheduled_video_queue_len(),
            decoded_video_frames: video_decode_snapshot.queued_frames,
            submitted_not_consumed_video_packets: video_decode_snapshot
                .submitted_not_consumed_packets,
            prepare_worker_frames,
            recovery_staging_frames: self.output_scheduler.recovery_staging_frames(),
            hardware_accelerated: self.video_decode_pipeline.info().hardware_accelerated,
            scheduled_video_queue_limit_reached,
            decode_recovery_video_admission_blocked: self
                .output_scheduler
                .decode_recovery_video_admission_blocked()
                || self
                    .video_decode_pipeline
                    .hevc_resource_pressure_demux_admission_stopped(),
            fill_phase_for_output_start: self.output_scheduler.output_fill_phase(),
            video_frame_duration_nsecs: self.video_frame_duration_nsecs,
            vo_queue_capacity: vo_snapshot.queue_capacity,
            vo_queued_frames: vo_snapshot.queued_frames,
            queued_video_forward_nsecs: output_snapshot.queued_video_forward_nsecs,
            audio_output_pending_nsecs,
            render_backlogged: vo_snapshot.render_backlogged(),
        })
    }

    pub(super) fn admit_video_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
        pressure: VideoPacketAdmissionPressure,
        demux_watermark: DemuxReaderWatermark,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.video_decode_pipeline.admit_demux_packet(
            packet,
            &mut self.video_packet_count,
            &mut self.playback_generation,
            &mut self.video_decode_recovery,
            &mut self.dovi_pipeline,
            &mut self.video_decode_skip_nonref_active,
            VideoPacketAdmissionContext {
                session_id,
                video_stream: self.video_stream,
                output_snapshot: pressure.output_snapshot,
                demux_watermark,
                has_audio_output: self.audio_output.is_some(),
                skip_nonref_for_pressure: pressure.skip_nonref_for_pressure,
                played_until_nsecs: pressure.played_until_nsecs,
            },
        )
    }

    pub(super) fn admit_audio_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if let Some(pipeline) = self.audio_decode_pipeline.as_mut() {
            pipeline.admit_demux_packet(packet, &mut self.playback_generation, session_id)
        } else {
            Ok(DecodePacketAdmissionStatus::Dropped)
        }
    }

    pub(super) fn admit_subtitle_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.subtitle_pipeline.admit_demux_packet(
            packet,
            &mut self.playback_generation,
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )
    }
}

fn cache_pause_actual_decode_work_for(
    decode: VideoDecodeWorkerSnapshot,
    prepare: VideoFramePrepareWorkerSnapshot,
) -> bool {
    decode.pending_input_packets > 0
        || decode.submitted_not_consumed_packets > 0
        || decode.result_produced_sequence != decode.result_consumed_sequence
        || decode.queued_frames > 0
        || decode.completed_packets > 0
        || prepare.pending_input_frames > 0
        || prepare.in_flight_frames > 0
        || prepare.completed_frames > 0
}

fn output_backpressure_prefetch_should_pause_for(
    output_state: PlaybackOutputState,
    video_decode_state: VideoDecodeWorkerState,
    audio_decode_state: Option<AudioDecodeWorkerState>,
) -> bool {
    let output_gate_blocked = output_state.restart_pending() || output_state.rebuffering();
    let decoded_queue_full = video_decode_state == VideoDecodeWorkerState::OutputFull
        || audio_decode_state == Some(AudioDecodeWorkerState::OutputFull);
    output_gate_blocked && decoded_queue_full
}

fn cache_pause_first_frame_input_demand_for(
    output: PlaybackOutputSnapshot,
    video_packet_input_available: bool,
) -> bool {
    output.first_frame_needed && video_packet_input_available
}

fn cached_seek_recovery_fallback_reason(
    elapsed: Duration,
    timeout: Duration,
    progress: CachedSeekRecoveryProgress,
) -> Option<CachedSeekRecoveryFallbackReason> {
    if elapsed >= timeout {
        return Some(CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout);
    }
    if progress.video_packets_since_seek >= CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS
        && !progress.has_actual_progress()
    {
        return Some(CachedSeekRecoveryFallbackReason::VideoPacketLimit);
    }
    None
}

fn cached_seek_recovery_timeout(
    hardware_accelerated: bool,
    size: Option<RenderSize>,
    target_nsecs: u64,
    first_zero_output_packet_nsecs: Option<u64>,
) -> Duration {
    if hardware_accelerated {
        return CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT;
    }
    let uhd_extra = size
        .filter(|size| {
            u64::from(size.width).saturating_mul(u64::from(size.height))
                >= CACHED_SEEK_UHD_PIXEL_THRESHOLD
        })
        .map(|_| CACHED_SEEK_SOFTWARE_UHD_EXTRA_TIMEOUT)
        .unwrap_or_default();
    hevc_startup_zero_output_timeout(false, target_nsecs, first_zero_output_packet_nsecs)
        .max(CACHED_SEEK_SOFTWARE_BASE_TIMEOUT)
        .saturating_add(uhd_extra)
        .min(CACHED_SEEK_SOFTWARE_MAX_TIMEOUT)
}

#[allow(clippy::too_many_arguments)]
fn cached_seek_recovery_watchdog_after_begin(
    existing: Option<CachedSeekRecoveryWatchdog>,
    target_nsecs: u64,
    cached_seek: Option<DemuxCachedSeekInfo>,
    decoder_epoch: u64,
    recovery_transaction_id: u64,
    admitted_video_sequence: u64,
    now: Instant,
    video_packet_count: u64,
) -> (CachedSeekRecoveryWatchdog, bool) {
    if let Some(mut watchdog) = existing {
        watchdog.target_nsecs = target_nsecs;
        watchdog.cached_seek = cached_seek.or(watchdog.cached_seek);
        watchdog.decoder_epoch = decoder_epoch;
        watchdog.recovery_transaction_id = recovery_transaction_id;
        watchdog.start_admitted_video_sequence = admitted_video_sequence;
        watchdog.last_progress_at = now;
        watchdog.start_video_packet_count = video_packet_count;
        watchdog.last_seek_preroll_frames = 0;
        return (watchdog, false);
    }
    (
        CachedSeekRecoveryWatchdog {
            target_nsecs,
            cached_seek,
            decoder_epoch,
            recovery_transaction_id,
            start_admitted_video_sequence: admitted_video_sequence,
            started_at: now,
            last_progress_at: now,
            start_video_packet_count: video_packet_count,
            last_seek_preroll_frames: 0,
        },
        true,
    )
}

fn cached_seek_recovery_next_action_for_attempt(
    attempt: &mut Option<CachedSeekRecoveryAttempt>,
    target_nsecs: u64,
    hardware_accelerated: bool,
    cra_anchor: bool,
) -> CachedSeekRecoveryFallbackAction {
    let reset_attempt = attempt.is_none_or(|attempt| attempt.target_nsecs != target_nsecs);
    if reset_attempt {
        *attempt = Some(CachedSeekRecoveryAttempt {
            target_nsecs,
            ..Default::default()
        });
    }
    let attempt = attempt
        .as_mut()
        .expect("cached seek recovery attempt exists");
    if cra_anchor {
        if attempt.low_level_seeks == 0 {
            attempt.low_level_seeks = attempt.low_level_seeks.saturating_add(1);
            return CachedSeekRecoveryFallbackAction::LowLevelSeek;
        }
        return CachedSeekRecoveryFallbackAction::RecoveryExhausted;
    }
    if attempt.soft_recoveries == 0 {
        attempt.soft_recoveries = attempt.soft_recoveries.saturating_add(1);
        return CachedSeekRecoveryFallbackAction::SoftRecover;
    }
    if hardware_accelerated && attempt.hardware_recoveries == 0 {
        attempt.hardware_recoveries = attempt.hardware_recoveries.saturating_add(1);
        return CachedSeekRecoveryFallbackAction::RecoverHardware;
    }
    if attempt.low_level_seeks == 0 {
        attempt.low_level_seeks = attempt.low_level_seeks.saturating_add(1);
        return CachedSeekRecoveryFallbackAction::LowLevelSeek;
    }
    CachedSeekRecoveryFallbackAction::RecoveryExhausted
}

fn cached_seek_recovery_watchdog_decision(
    elapsed: Duration,
    timeout: Duration,
    progress: CachedSeekRecoveryProgress,
    matching_admitted_progress: bool,
) -> CachedSeekRecoveryWatchdogDecision {
    if matching_admitted_progress {
        return CachedSeekRecoveryWatchdogDecision::Clear;
    }
    if progress.waiting_cached_input() {
        return CachedSeekRecoveryWatchdogDecision::WaitingCachedInput;
    }
    cached_seek_recovery_fallback_reason(elapsed, timeout, progress)
        .map(CachedSeekRecoveryWatchdogDecision::Fallback)
        .unwrap_or(CachedSeekRecoveryWatchdogDecision::Wait)
}

fn decoder_input_retry_status_from_streams(
    statuses: impl IntoIterator<Item = Option<DecodeInputRetryStatus>>,
) -> DecodeInputRetryStatus {
    let mut made_progress = false;
    let mut backpressured = false;
    for status in statuses.into_iter().flatten() {
        made_progress |= status.made_progress();
        backpressured |= status.backpressured();
    }
    if backpressured {
        DecodeInputRetryStatus::Backpressured
    } else if made_progress {
        DecodeInputRetryStatus::Queued
    } else {
        DecodeInputRetryStatus::Idle
    }
}

fn decoder_block_reason_blocks_packet_input(blocked_on: Option<PlaybackBlockReason>) -> bool {
    matches!(
        blocked_on,
        Some(
            PlaybackBlockReason::PacketQueueFull
                | PlaybackBlockReason::DecoderRecovery
                | PlaybackBlockReason::DecoderInFlight
                | PlaybackBlockReason::DecoderOutputPending
                | PlaybackBlockReason::DecodedVideoQueue
                | PlaybackBlockReason::DecodedQueueFull
                | PlaybackBlockReason::HwSurfacePool
        )
    )
}

fn audio_input_suppressed_until_output_resume_state(
    has_audio_decode_pipeline: bool,
    output_waiting_for_resume: bool,
    audio_resume_waterline: Option<AudioResumeWaterline>,
) -> bool {
    // Total pending-start audio duration is not the same as continuous coverage
    // from the eventual resume timeline. Keep feeding audio until the same
    // resume waterline used by the output gate has headroom beyond the resume
    // target; stale preroll or disconnected pending audio must not close the
    // audio demux/decode path.
    has_audio_decode_pipeline
        && output_waiting_for_resume
        && audio_resume_waterline.is_some_and(|waterline| {
            waterline.reaches_target_with_margin(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
        })
}

fn decoder_input_streams_for_state(
    video: DecoderInputStreamState,
    audio: Option<DecoderInputStreamState>,
    subtitle: Option<DecoderInputStreamState>,
) -> Vec<c_int> {
    let mut streams = Vec::with_capacity(3);
    push_decoder_input_stream_if_open(&mut streams, video);
    if let Some(audio) = audio {
        push_decoder_input_stream_if_open(&mut streams, audio);
    }
    if let Some(subtitle) = subtitle {
        push_decoder_input_stream_if_open(&mut streams, subtitle);
    }
    streams
}

fn push_decoder_input_stream_if_open(streams: &mut Vec<c_int>, stream: DecoderInputStreamState) {
    if !stream.packet_input_blocked && !streams.contains(&stream.stream_index) {
        streams.push(stream.stream_index);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::raw::c_int,
        time::{Duration, Instant},
    };

    use crate::player::render_host::RenderSize;

    use super::super::decode::DecodeInputRetryStatus;
    use super::super::video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState};
    use super::super::video_frame_prepare_worker::{
        VideoFramePrepareWorkerSnapshot, VideoFramePrepareWorkerState,
    };
    use super::super::{
        AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioResumeWaterline, DecodedAudio,
        PendingStartAudio, PlaybackBlockReason, PlaybackOutputSnapshot, PlaybackOutputState,
        RebufferAudioRealignRequest, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, duration_nsecs,
    };
    use super::{
        AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER, AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER,
        AUDIO_REALIGN_FALLBACK_AFTER_OBSERVATIONS, AUDIO_REALIGN_MAX_OBSERVATIONS,
        AUDIO_REALIGN_TARGET_TOLERANCE_NSECS, AudioDecodeWorkerSnapshot, AudioDecodeWorkerState,
        AudioRealignCoalesceReason, AudioRealignCoverage, AudioRealignPhase,
        AudioRealignRequestAction, AudioRealignTransaction, AudioRecoveryWatchdogAction,
        CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT, CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS,
        CachedSeekRecoveryAttempt, CachedSeekRecoveryFallbackAction,
        CachedSeekRecoveryFallbackReason, CachedSeekRecoveryProgress, CachedSeekRecoveryWatchdog,
        CachedSeekRecoveryWatchdogDecision, DecoderInputStreamState,
        audio_input_suppressed_until_output_resume_state, audio_realign_completion_ready,
        cache_pause_actual_decode_work_for, cache_pause_first_frame_input_demand_for,
        cached_seek_recovery_fallback_reason, cached_seek_recovery_next_action_for_attempt,
        cached_seek_recovery_timeout, cached_seek_recovery_watchdog_after_begin,
        cached_seek_recovery_watchdog_decision, decoder_block_reason_blocks_packet_input,
        decoder_input_retry_status_from_streams, decoder_input_streams_for_state,
        mark_video_decode_skip_nonref_inactive, observe_audio_realign_request,
        output_backpressure_prefetch_should_pause_for, poll_audio_recovery_watchdog,
        retain_pending_audio_for_realign_once, update_audio_realign_progress,
    };

    fn audio_realign_request(target_timeline_nsecs: u64) -> RebufferAudioRealignRequest {
        RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs: target_timeline_nsecs.saturating_sub(20_000_000),
            first_video_timeline_nsecs: target_timeline_nsecs,
            far_ahead_audio_timeline_nsecs: target_timeline_nsecs.saturating_add(2_000_000_000),
            far_ahead_observation_count: 1,
            reason: "test_audio_realign",
        }
    }

    fn audio_realign_transaction(target_timeline_nsecs: u64) -> AudioRealignTransaction {
        let started_at = Instant::now();
        AudioRealignTransaction {
            transaction_id: 7,
            target_timeline_nsecs,
            generation: 9,
            started_at,
            attempts: 1,
            request: audio_realign_request(target_timeline_nsecs),
            phase: AudioRealignPhase::Flushing,
            coverage_nsecs: 0,
            coverage_target_nsecs: 850_000_000,
            observations: 0,
            first_observed_pts_nsecs: None,
            last_observed_pts_nsecs: None,
            last_progress_at: started_at,
            warning_emitted: false,
            fallback_exhausted_logged: false,
        }
    }

    fn idle_audio_snapshot() -> AudioDecodeWorkerSnapshot {
        AudioDecodeWorkerSnapshot {
            state: AudioDecodeWorkerState::NeedPacket,
            queued_frames: 0,
            queued_duration_nsecs: 0,
            duration_limit_nsecs: 1_000_000_000,
            pending_input_packets: 0,
            pending_input_capacity: 16,
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
    fn pending_audio_is_retained_until_realign_coverage_is_confirmed() {
        let target = 18_060_000_000;
        let mut active = PendingStartAudio::default();
        active.push(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 905_000_000,
            },
            target,
            target + 905_000_000,
        );
        let mut retained = None;

        assert!(retain_pending_audio_for_realign_once(
            &mut retained,
            &mut active
        ));
        assert!(active.is_empty());
        assert_eq!(retained.as_ref().map(PendingStartAudio::len), Some(1));

        let mut transaction = audio_realign_transaction(target);
        assert!(!audio_realign_completion_ready(
            Some(transaction),
            true,
            true
        ));
        transaction.phase = AudioRealignPhase::Covered;
        assert!(audio_realign_completion_ready(
            Some(transaction),
            true,
            true
        ));
        assert!(!audio_realign_completion_ready(
            Some(transaction),
            false,
            true
        ));
    }

    #[test]
    fn flush_ack_waits_for_coverage_and_only_marks_covered_at_protected_waterline() {
        let target = 18_060_000_000;
        let now = Instant::now();
        let mut transaction = audio_realign_transaction(target);

        update_audio_realign_progress(
            &mut transaction,
            idle_audio_snapshot(),
            AudioRealignCoverage {
                audio_accepted_start_timeline_nsecs: Some(target + 71_000_000),
                start_gap_nsecs: Some(71_000_000),
                contiguous_coverage_nsecs: Some(849_000_000),
                protected_target_nsecs: 850_000_000,
                ready: false,
            },
            now,
        );
        assert_eq!(transaction.phase, AudioRealignPhase::AwaitingCoverage);
        assert_eq!(transaction.coverage_nsecs, 849_000_000);

        let mut wrapped = Some(transaction);
        assert!(matches!(
            observe_audio_realign_request(&mut wrapped, audio_realign_request(target)),
            AudioRealignRequestAction::Coalesce {
                reason: AudioRealignCoalesceReason::AwaitingCoverage,
                ..
            }
        ));

        update_audio_realign_progress(
            wrapped.as_mut().unwrap(),
            idle_audio_snapshot(),
            AudioRealignCoverage {
                audio_accepted_start_timeline_nsecs: Some(target + 71_000_000),
                start_gap_nsecs: Some(71_000_000),
                contiguous_coverage_nsecs: Some(1_056_000_000),
                protected_target_nsecs: 850_000_000,
                ready: true,
            },
            now + Duration::from_millis(1),
        );
        assert_eq!(wrapped.unwrap().phase, AudioRealignPhase::Covered);
    }

    #[test]
    fn audio_realign_covered_is_terminal_and_never_falls_back_from_repeat_requests() {
        let target = 18_060_000_000;
        let mut transaction = Some(audio_realign_transaction(target));

        for _ in 0..8 {
            let recovering_action =
                observe_audio_realign_request(&mut transaction, audio_realign_request(target));
            assert!(matches!(
                recovering_action,
                AudioRealignRequestAction::Coalesce {
                    reason: AudioRealignCoalesceReason::Flushing,
                    ..
                }
            ));
        }
        assert_eq!(transaction.unwrap().attempts, 1);

        transaction.as_mut().unwrap().phase = AudioRealignPhase::AwaitingCoverage;
        let awaiting_action =
            observe_audio_realign_request(&mut transaction, audio_realign_request(target));
        assert!(matches!(
            awaiting_action,
            AudioRealignRequestAction::Coalesce {
                reason: AudioRealignCoalesceReason::AwaitingCoverage,
                ..
            }
        ));

        transaction.as_mut().unwrap().phase = AudioRealignPhase::Covered;
        for _ in 0..8 {
            let covered_action =
                observe_audio_realign_request(&mut transaction, audio_realign_request(target));
            assert!(matches!(
                covered_action,
                AudioRealignRequestAction::Coalesce {
                    reason: AudioRealignCoalesceReason::CoverageSatisfied,
                    ..
                }
            ));
        }
        let transaction = transaction.unwrap();
        assert_eq!(transaction.attempts, 1);
        assert_eq!(transaction.phase, AudioRealignPhase::Covered);
    }

    #[test]
    fn covered_938ms_audio_rejects_stale_239s_reader_request_without_second_seek() {
        let target = 237_237_000_000;
        let mut transaction = Some(audio_realign_transaction(target));
        transaction.as_mut().unwrap().phase = AudioRealignPhase::AwaitingCoverage;
        update_audio_realign_progress(
            transaction.as_mut().unwrap(),
            idle_audio_snapshot(),
            AudioRealignCoverage {
                audio_accepted_start_timeline_nsecs: Some(target),
                start_gap_nsecs: Some(0),
                contiguous_coverage_nsecs: Some(938_999_996),
                protected_target_nsecs: 850_000_000,
                ready: true,
            },
            Instant::now(),
        );
        let mut stale_request = audio_realign_request(target);
        stale_request.far_ahead_audio_timeline_nsecs = 239_136_000_000;

        assert!(matches!(
            observe_audio_realign_request(&mut transaction, stale_request),
            AudioRealignRequestAction::Coalesce {
                reason: AudioRealignCoalesceReason::CoverageSatisfied,
                ..
            }
        ));
        let transaction = transaction.unwrap();
        assert_eq!(transaction.phase, AudioRealignPhase::Covered);
        assert_eq!(transaction.attempts, 1);
    }

    #[test]
    fn changed_target_while_coverage_is_pending_is_coalesced() {
        let initial_target = 18_060_000_000;
        let changed_target = initial_target + AUDIO_REALIGN_TARGET_TOLERANCE_NSECS + 1;
        let mut transaction = Some(audio_realign_transaction(initial_target));

        transaction.as_mut().unwrap().phase = AudioRealignPhase::AwaitingCoverage;
        let action =
            observe_audio_realign_request(&mut transaction, audio_realign_request(changed_target));

        assert!(matches!(
            action,
            AudioRealignRequestAction::Coalesce {
                reason: AudioRealignCoalesceReason::AwaitingCoverage,
                ..
            }
        ));
        let transaction = transaction.unwrap();
        assert_eq!(transaction.target_timeline_nsecs, initial_target);
        assert_eq!(transaction.request.target_timeline_nsecs, initial_target);
        assert_eq!(transaction.attempts, 1);
    }

    #[test]
    fn audio_recovery_watchdog_warns_then_falls_back_only_once() {
        let mut transaction = audio_realign_transaction(18_060_000_000);
        transaction.phase = AudioRealignPhase::AwaitingCoverage;
        let now = Instant::now();
        transaction.last_progress_at = now - AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;

        let warning = poll_audio_recovery_watchdog(
            &mut transaction,
            idle_audio_snapshot(),
            now - AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER
                + AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER,
        );
        assert!(matches!(
            warning,
            Some(AudioRecoveryWatchdogAction::Warn { .. })
        ));
        assert!(
            poll_audio_recovery_watchdog(
                &mut transaction,
                idle_audio_snapshot(),
                now - AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER
                    + AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER,
            )
            .is_none()
        );

        let fallback = poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), now);
        assert!(matches!(
            fallback,
            Some(AudioRecoveryWatchdogAction::LowLevelFallback { .. })
        ));

        let after_fallback = now + AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;
        let exhausted =
            poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), after_fallback);
        assert!(matches!(
            exhausted,
            Some(AudioRecoveryWatchdogAction::FallbackExhausted { .. })
        ));
        assert!(
            poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), after_fallback,)
                .is_none()
        );
    }

    #[test]
    fn audio_realign_observation_bounds_force_one_fallback_then_terminal_gap() {
        let target = 18_060_000_000;
        let now = Instant::now();
        let mut transaction = audio_realign_transaction(target);
        transaction.phase = AudioRealignPhase::AwaitingCoverage;
        let no_coverage = AudioRealignCoverage {
            protected_target_nsecs: 850_000_000,
            ..AudioRealignCoverage::default()
        };

        for _ in 0..AUDIO_REALIGN_FALLBACK_AFTER_OBSERVATIONS {
            update_audio_realign_progress(
                &mut transaction,
                idle_audio_snapshot(),
                no_coverage,
                now,
            );
        }
        assert!(matches!(
            poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), now),
            Some(AudioRecoveryWatchdogAction::LowLevelFallback { .. })
        ));

        for _ in AUDIO_REALIGN_FALLBACK_AFTER_OBSERVATIONS..AUDIO_REALIGN_MAX_OBSERVATIONS {
            update_audio_realign_progress(
                &mut transaction,
                idle_audio_snapshot(),
                no_coverage,
                now,
            );
        }
        assert!(matches!(
            poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), now),
            Some(AudioRecoveryWatchdogAction::FallbackExhausted { .. })
        ));
    }

    #[test]
    fn low_level_fallback_can_still_complete_with_confirmed_coverage() {
        let target = 18_060_000_000;
        let mut transaction = audio_realign_transaction(target);
        transaction.phase = AudioRealignPhase::FallbackUsed;

        update_audio_realign_progress(
            &mut transaction,
            idle_audio_snapshot(),
            AudioRealignCoverage {
                audio_accepted_start_timeline_nsecs: Some(target),
                start_gap_nsecs: Some(0),
                contiguous_coverage_nsecs: Some(900_000_000),
                protected_target_nsecs: 850_000_000,
                ready: true,
            },
            Instant::now(),
        );

        assert_eq!(transaction.phase, AudioRealignPhase::Covered);
    }

    fn stream(stream_index: c_int, packet_input_blocked: bool) -> DecoderInputStreamState {
        DecoderInputStreamState {
            stream_index,
            packet_input_blocked,
        }
    }

    fn audio_waterline(decoded_audio_forward_nsecs: Option<u64>) -> AudioResumeWaterline {
        AudioResumeWaterline {
            resume_timeline_nsecs: 1_000_000_000,
            target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            audio_accepted_start_timeline_nsecs: Some(1_000_000_000),
            audio_accepted_start_gap_nsecs: Some(0),
            accepted_contiguous_coverage_nsecs: decoded_audio_forward_nsecs,
            audio_output_buffered_until_nsecs: None,
            audio_output_pending_nsecs: None,
            pending_audio_start_nsecs: Some(1_000_000_000),
            pending_audio_forward_nsecs: decoded_audio_forward_nsecs,
            decoded_audio_forward_nsecs,
            audio_decode_queued_nsecs: 0,
            audio_decode_in_flight_packets: 0,
            demux_audio_forward_nsecs: None,
            demux_audio_cached_packets: None,
            ready: decoded_audio_forward_nsecs.is_some_and(|duration| {
                duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
            }),
        }
    }

    fn output_snapshot(
        first_video_frame_pending: bool,
        queued_video_frames: usize,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state: PlaybackOutputState::Syncing,
            first_video_frame_pending,
            first_frame_needed: first_video_frame_pending && queued_video_frames == 0,
            first_frame_presented: false,
            initial_av_start_pending: first_video_frame_pending,
            output_clock_running: false,
            audio_start_target_nsecs: None,
            output_transition_deadline_ms: None,
            rebuffering: false,
            queued_video_frames,
            recovery_staging_frames: 0,
            recovery_staging_frame_budget: None,
            committed_output_high_water_nsecs: None,
            recovery_staged_high_water_nsecs: None,
            decode_recovery_audio_ready_latched: false,
            queued_video_coverage_nsecs: 0,
            queued_video_duration_nsecs: 0,
            queued_video_range_span_nsecs: 0,
            queued_video_range_nsecs: None,
            queued_video_forward_nsecs: None,
            queued_video_contiguous_forward_nsecs: None,
            queued_video_largest_gap_nsecs: None,
            video_output_low_water: false,
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

    #[test]
    fn cache_pause_drains_four_worker_results_before_waiting() {
        let mut decode = VideoDecodeWorkerSnapshot {
            state: VideoDecodeWorkerState::Decoding,
            submitted_not_consumed_packets: 4,
            submitted_sequence: 4,
            result_produced_sequence: 4,
            result_consumed_sequence: 0,
            ..VideoDecodeWorkerSnapshot::default()
        };
        let prepare = VideoFramePrepareWorkerSnapshot {
            state: VideoFramePrepareWorkerState::NeedFrame,
            pending_input_frames: 0,
            pending_input_capacity: 3,
            in_flight_frames: 0,
            completed_frames: 0,
            command_queue_capacity: 3,
        };

        assert!(cache_pause_actual_decode_work_for(decode, prepare));

        decode.submitted_not_consumed_packets = 0;
        decode.result_consumed_sequence = 4;
        decode.pending_input_packets = 1;
        assert!(cache_pause_actual_decode_work_for(decode, prepare));
        decode.pending_input_packets = 0;
        assert!(!cache_pause_actual_decode_work_for(decode, prepare));
        assert!(cache_pause_first_frame_input_demand_for(
            output_snapshot(true, 0),
            true,
        ));
        assert!(!cache_pause_first_frame_input_demand_for(
            output_snapshot(true, 0),
            false,
        ));
        assert!(!cache_pause_first_frame_input_demand_for(
            output_snapshot(false, 0),
            true,
        ));
    }

    fn cached_seek_progress(video_packets_since_seek: u64) -> CachedSeekRecoveryProgress {
        CachedSeekRecoveryProgress {
            video_packets_since_seek,
            ..Default::default()
        }
    }

    fn cached_seek_progress_with_decoder_work(
        video_packets_since_seek: u64,
    ) -> CachedSeekRecoveryProgress {
        CachedSeekRecoveryProgress {
            video_packets_since_seek,
            video_decode_submitted_not_consumed_packets: 1,
            ..Default::default()
        }
    }

    #[test]
    fn video_decode_skip_nonref_reset_marks_active_state_inactive() {
        let mut active = true;

        assert!(mark_video_decode_skip_nonref_inactive(&mut active));
        assert!(!active);
        assert!(!mark_video_decode_skip_nonref_inactive(&mut active));
    }

    #[test]
    fn cached_seek_recovery_fallback_waits_before_limits() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS - 1),
            ),
            None
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_rearms_deadline_across_internal_reset() {
        let started_at = std::time::Instant::now();
        let existing = CachedSeekRecoveryWatchdog {
            target_nsecs: 123_000_000_000,
            cached_seek: None,
            decoder_epoch: 3,
            recovery_transaction_id: 7,
            start_admitted_video_sequence: 41,
            started_at,
            last_progress_at: started_at,
            start_video_packet_count: 100,
            last_seek_preroll_frames: 12,
        };

        let (watchdog, started) = cached_seek_recovery_watchdog_after_begin(
            Some(existing),
            123_360_000_000,
            None,
            4,
            7,
            45,
            started_at + Duration::from_secs(1),
            180,
        );

        assert!(!started);
        assert_eq!(watchdog.target_nsecs, 123_360_000_000);
        assert_eq!(watchdog.started_at, started_at);
        assert_eq!(
            watchdog.last_progress_at,
            started_at + Duration::from_secs(1)
        );
        assert_eq!(watchdog.start_video_packet_count, 180);
        assert_eq!(watchdog.last_seek_preroll_frames, 0);
        assert_eq!(watchdog.decoder_epoch, 4);
        assert_eq!(watchdog.recovery_transaction_id, 7);
        assert_eq!(watchdog.start_admitted_video_sequence, 45);
    }

    #[test]
    fn cached_seek_recovery_fallback_triggers_on_first_frame_timeout() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
            ),
            Some(CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout)
        );
    }

    #[test]
    fn cached_seek_recovery_fallback_waits_before_rearmed_progress_deadline() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress_with_decoder_work(1),
            ),
            None
        );
    }

    #[test]
    fn cached_seek_recovery_fallback_triggers_on_video_packet_limit() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS),
            ),
            Some(CachedSeekRecoveryFallbackReason::VideoPacketLimit)
        );
    }

    #[test]
    fn cached_seek_recovery_actions_escalate_for_same_hardware_target() {
        let mut attempt = None::<CachedSeekRecoveryAttempt>;

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, false,),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, false,),
            CachedSeekRecoveryFallbackAction::RecoverHardware
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, false,),
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, false,),
            CachedSeekRecoveryFallbackAction::RecoveryExhausted
        );
    }

    #[test]
    fn cached_seek_recovery_actions_reset_for_new_target() {
        let mut attempt = Some(CachedSeekRecoveryAttempt {
            target_nsecs: 35_000_000_000,
            soft_recoveries: 1,
            hardware_recoveries: 1,
            low_level_seeks: 1,
        });

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 83_000_000_000, true, false,),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
    }

    #[test]
    fn cached_seek_recovery_actions_skip_hardware_recovery_when_decoder_is_software() {
        let mut attempt = None::<CachedSeekRecoveryAttempt>;

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(
                &mut attempt,
                35_000_000_000,
                false,
                false,
            ),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(
                &mut attempt,
                35_000_000_000,
                false,
                false,
            ),
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(
                &mut attempt,
                35_000_000_000,
                false,
                false,
            ),
            CachedSeekRecoveryFallbackAction::RecoveryExhausted
        );
    }

    #[test]
    fn cra_cached_seek_failure_permits_exactly_one_low_level_fallback() {
        let mut attempt = None::<CachedSeekRecoveryAttempt>;

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, true,),
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true, true,),
            CachedSeekRecoveryFallbackAction::RecoveryExhausted
        );
        assert_eq!(attempt.expect("attempt recorded").low_level_seeks, 1);
    }

    #[test]
    fn software_uhd_cached_seek_timeout_scales_with_long_gop_preroll() {
        let timeout = cached_seek_recovery_timeout(
            false,
            Some(RenderSize {
                width: 3840,
                height: 1620,
            }),
            669_625_000_000,
            Some(663_833_000_000),
        );

        assert_eq!(timeout, Duration::from_millis(23_584));
        assert_eq!(
            cached_seek_recovery_timeout(true, None, 669_625_000_000, Some(663_833_000_000)),
            CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_waits_before_deadline_without_first_video() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS - 1),
                false,
            ),
            CachedSeekRecoveryWatchdogDecision::WaitingCachedInput
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_waits_for_input_after_deadline_without_decoder_work() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
                false,
            ),
            CachedSeekRecoveryWatchdogDecision::WaitingCachedInput
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_waits_after_preroll_progress_rearms_deadline() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                Duration::ZERO,
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CachedSeekRecoveryProgress {
                    seek_preroll_frames: 1,
                    ..Default::default()
                },
                false,
            ),
            CachedSeekRecoveryWatchdogDecision::WaitingCachedInput
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_clears_only_for_matching_admitted_progress() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
                true,
            ),
            CachedSeekRecoveryWatchdogDecision::Clear
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_does_not_clear_for_four_old_queued_frames() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CachedSeekRecoveryProgress {
                    video_packets_since_seek: 0,
                    video_decode_queued_frames: 4,
                    ..Default::default()
                },
                false,
            ),
            CachedSeekRecoveryWatchdogDecision::Wait
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_does_not_clear_for_twenty_nine_old_queued_frames() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CachedSeekRecoveryProgress {
                    video_packets_since_seek: 0,
                    video_decode_queued_frames: 29,
                    ..Default::default()
                },
                false,
            ),
            CachedSeekRecoveryWatchdogDecision::Wait
        );
    }

    #[test]
    fn decoder_input_streams_skip_only_backpressured_streams() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, true),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![11, 12]
        );
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, true)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_deduplicate_shared_stream_indices() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(10, false)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_allow_all_streams_until_their_decoder_queue_is_full() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn audio_input_suppression_waits_until_resume_waterline_has_margin() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(duration_nsecs(
                VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
            ))))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
                    - 1
            )))
        ));
        assert!(audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(None))
        ));
    }

    #[test]
    fn decoder_input_keeps_audio_stream_open_when_resume_audio_is_below_target() {
        let audio_input_suppressed = audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
            ))),
        );

        assert!(!audio_input_suppressed);
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, audio_input_suppressed)),
                Some(stream(12, false))
            ),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn audio_input_suppression_only_applies_while_output_waits_for_video() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            false,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            false,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
    }

    #[test]
    fn decoder_block_reason_blocks_only_packet_input_pressure() {
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::PacketQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderRecovery
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderInFlight
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderOutputPending
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedVideoQueue
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::HwSurfacePool
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderInputEmpty
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::RenderWorker
        )));
        assert!(!decoder_block_reason_blocks_packet_input(None));
    }

    #[test]
    fn decoder_input_retry_status_keeps_backpressure_after_other_stream_progress() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Backpressured),
                Some(DecodeInputRetryStatus::Queued),
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_retry_status_reports_progress_without_backpressure() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                Some(DecodeInputRetryStatus::Queued),
                None,
            ]),
            DecodeInputRetryStatus::Queued
        );
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                None,
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Idle
        );
    }

    #[test]
    fn output_gate_with_full_decoded_queue_pauses_prefetch_only_until_playing() {
        assert!(output_backpressure_prefetch_should_pause_for(
            PlaybackOutputState::Syncing,
            VideoDecodeWorkerState::OutputFull,
            None,
        ));
        assert!(output_backpressure_prefetch_should_pause_for(
            PlaybackOutputState::Rebuffering,
            VideoDecodeWorkerState::NeedPacket,
            Some(AudioDecodeWorkerState::OutputFull),
        ));
        assert!(!output_backpressure_prefetch_should_pause_for(
            PlaybackOutputState::Playing,
            VideoDecodeWorkerState::OutputFull,
            Some(AudioDecodeWorkerState::OutputFull),
        ));
        assert!(!output_backpressure_prefetch_should_pause_for(
            PlaybackOutputState::Primed,
            VideoDecodeWorkerState::NeedPacket,
            Some(AudioDecodeWorkerState::NeedPacket),
        ));
    }
}
