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
    Exhausted,
}

impl AudioRealignPhase {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Flushing => "flushing",
            Self::AwaitingCoverage => "awaiting_coverage",
            Self::Covered => "covered",
            Self::FallbackUsed => "fallback_used",
            Self::Exhausted => "exhausted",
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
        AudioRealignPhase::Covered | AudioRealignPhase::Exhausted => {
            if audio_realign_target_matches(
                current.target_timeline_nsecs,
                request.target_timeline_nsecs,
            ) {
                current.request = request;
                return AudioRealignRequestAction::Coalesce {
                    transaction: *current,
                    reason: if current.phase == AudioRealignPhase::Exhausted {
                        AudioRealignCoalesceReason::LowLevelFallbackAlreadyUsed
                    } else {
                        AudioRealignCoalesceReason::CoverageSatisfied
                    },
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
    if coverage.ready {
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
        AudioRealignPhase::Covered | AudioRealignPhase::Exhausted
    ) {
        return None;
    }
    let stalled_for = now.saturating_duration_since(transaction.last_progress_at);
    let absolute_wall_time_exhausted =
        now.saturating_duration_since(transaction.started_at) >= AUDIO_REALIGN_MAX_WALL_TIME;
    // Coordinator polling frequency and decoded PTS distance do not measure
    // a stall. Only actual elapsed time can exhaust a recovery attempt.
    let attempt_stalled = stalled_for >= AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;
    let terminal_bound_exhausted = absolute_wall_time_exhausted
        || (transaction.phase == AudioRealignPhase::FallbackUsed && attempt_stalled);
    let recovery_bound_exhausted = if transaction.phase == AudioRealignPhase::FallbackUsed {
        terminal_bound_exhausted
    } else {
        attempt_stalled || terminal_bound_exhausted
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

#[path = "playback_pipeline_state/audio_realign.rs"]
mod audio_realign;
#[path = "playback_pipeline_state/cached_seek.rs"]
mod cached_seek;
#[path = "playback_pipeline_state/decode_recovery.rs"]
mod decode_recovery;
#[path = "playback_pipeline_state/decoder_drain.rs"]
mod decoder_drain;
#[path = "playback_pipeline_state/decoder_input.rs"]
mod decoder_input;

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
        AUDIO_REALIGN_MAX_WALL_TIME, AUDIO_REALIGN_TARGET_TOLERANCE_NSECS,
        AudioDecodeWorkerSnapshot, AudioDecodeWorkerState, AudioRealignCoalesceReason,
        AudioRealignCoverage, AudioRealignPhase, AudioRealignRequestAction,
        AudioRealignTransaction, AudioRecoveryWatchdogAction,
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
        poll_audio_recovery_watchdog, retain_pending_audio_for_realign_once,
        update_audio_realign_progress,
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
    fn repeated_audio_realign_polls_do_not_exhaust_recovery_before_timeout() {
        let target = 18_060_000_000;
        let mut transaction = audio_realign_transaction(target);
        let now = transaction.started_at;
        transaction.phase = AudioRealignPhase::AwaitingCoverage;
        let no_coverage = AudioRealignCoverage {
            protected_target_nsecs: 850_000_000,
            ..AudioRealignCoverage::default()
        };

        for _ in 0..10_000 {
            update_audio_realign_progress(
                &mut transaction,
                idle_audio_snapshot(),
                no_coverage,
                now,
            );
            assert!(
                poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), now)
                    .is_none()
            );
        }
        let fallback_at = now + AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;
        assert!(matches!(
            poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), fallback_at),
            Some(AudioRecoveryWatchdogAction::LowLevelFallback { .. })
        ));

        for _ in 0..10_000 {
            update_audio_realign_progress(
                &mut transaction,
                idle_audio_snapshot(),
                no_coverage,
                fallback_at,
            );
            assert!(
                poll_audio_recovery_watchdog(&mut transaction, idle_audio_snapshot(), fallback_at)
                    .is_none()
            );
        }
        assert!(matches!(
            poll_audio_recovery_watchdog(
                &mut transaction,
                idle_audio_snapshot(),
                fallback_at + AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER,
            ),
            Some(AudioRecoveryWatchdogAction::FallbackExhausted { .. })
        ));
    }

    #[test]
    fn audio_realign_decoder_progress_defers_stall_but_preserves_wall_time_bound() {
        let mut transaction = audio_realign_transaction(18_060_000_000);
        transaction.phase = AudioRealignPhase::AwaitingCoverage;
        let now = transaction.started_at + AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER;
        let mut worker = idle_audio_snapshot();
        worker.state = AudioDecodeWorkerState::Recovering;
        worker.last_result_progress_elapsed = Some(Duration::from_millis(10));
        update_audio_realign_progress(
            &mut transaction,
            worker,
            AudioRealignCoverage::default(),
            now,
        );
        assert!(poll_audio_recovery_watchdog(&mut transaction, worker, now).is_none());

        let deadline = transaction.started_at + AUDIO_REALIGN_MAX_WALL_TIME;
        update_audio_realign_progress(
            &mut transaction,
            worker,
            AudioRealignCoverage::default(),
            deadline,
        );
        assert!(matches!(
            poll_audio_recovery_watchdog(&mut transaction, worker, deadline),
            Some(AudioRecoveryWatchdogAction::LowLevelFallback { .. })
        ));
    }

    #[test]
    fn exhausted_audio_realign_coalesces_same_target_but_allows_a_new_target() {
        let target = 1_400_584_044_444;
        let mut transaction = Some(audio_realign_transaction(target));
        transaction.as_mut().unwrap().phase = AudioRealignPhase::Exhausted;
        for _ in 0..8 {
            assert!(matches!(
                observe_audio_realign_request(&mut transaction, audio_realign_request(target)),
                AudioRealignRequestAction::Coalesce {
                    reason: AudioRealignCoalesceReason::LowLevelFallbackAlreadyUsed,
                    ..
                }
            ));
        }
        assert!(
            poll_audio_recovery_watchdog(
                transaction.as_mut().unwrap(),
                idle_audio_snapshot(),
                Instant::now() + Duration::from_secs(10),
            )
            .is_none()
        );
        assert!(matches!(
            observe_audio_realign_request(
                &mut transaction,
                audio_realign_request(target + AUDIO_REALIGN_TARGET_TOLERANCE_NSECS + 1),
            ),
            AudioRealignRequestAction::Start
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
}
