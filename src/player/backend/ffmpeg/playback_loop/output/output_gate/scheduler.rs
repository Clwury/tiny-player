#[cfg(test)]
use super::DecodedAudio;
use super::RebufferResumeAnchor;
use super::{
    AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER, AUDIO_OUTPUT_ACTIVITY_STALL_AFTER,
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_REBUFFER_LOOP_DETECTION_WINDOW, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, Arc, AtomicBool,
    AudioClockHandle, AudioContinuityRejectionSummary, AudioOutput, AudioOutputActivitySnapshot,
    AudioOutputActivityWatchdog, AudioOutputActivityWatchdogAction,
    AudioOutputActivityWatchdogEvent, AudioOutputLifecycle, AudioOutputSnapshot,
    AudioReaderGapWatchdog, AudioRealignCoverage, AudioResumeWaterline, AudioSyncDropLogSummary,
    BackendEvent, DECODE_RECOVERY_AUDIO_READY_HYSTERESIS_NSECS,
    DECODE_RECOVERY_DECODER_IN_FLIGHT_ALLOWANCE, DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
    DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS, DECODE_RECOVERY_MAX_WALL_TIME,
    DECODE_RECOVERY_STAGING_NSECS, DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS,
    DecodeRecoveryDisposition, DecodeRecoveryDropForFallback, DecodeRecoveryGapProvenance,
    DecodeRecoveryPhase, DecodeRecoverySource, DecodeRecoveryTransaction, Duration, FfmpegControl,
    INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL, INITIAL_AUDIO_START_RETRY_INTERVAL,
    INITIAL_AV_START_HARD_TIMEOUT, INITIAL_SYNC_LOG_SUMMARY_INTERVAL, InitialAudioDeferLogState,
    InitialAudioDeferObservation, InitialAudioPreparePhase, InitialAudioPrepareToken,
    InitialAudioTransientRetry, InitialAvStartTransaction, InitialSyncLogDecision,
    InitialSyncLogObservation, InitialSyncLogState, Instant, OUTPUT_GATE_PERIODIC_PROBE_INTERVAL,
    OutputGateBlockLogEmission, OutputGateBlockLogState, OutputServiceDemand,
    PENDING_AUDIO_CONTINUITY_TOLERANCE, PendingAudioRetentionAnchorSource,
    PendingAudioRetentionPlan, PendingStartAudio, PendingStartAudioPressureLevel,
    PlaybackBlockReason, PlaybackOutputScheduler, PlaybackOutputSnapshot, PlaybackOutputState,
    PlaybackResumeWaterline, PlaybackScheduler, PlaybackSessionId, PrestartAudioOwnership,
    PrestartAudioOwnershipLogState, QueuedVideoFrame, RebufferAudioRealignRequest,
    ScheduledVideoQueue, Sender, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER,
    VideoDeadlineService, VideoOutputQueue, VideoOutputUnderflowClassification,
    clear_video_output_rebuffer, decode_recovery_gap_within_limit, duration_nsecs,
    enter_video_output_rebuffer, finish_video_output_rebuffer_if_ready,
    video_output_rebuffer_should_enter, video_output_underflow_classification,
};
use ffmpeg_sys_next as ffi;

const REBUFFER_EMPTY_AUDIO_OUTPUT_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_OBSERVATIONS: u8 = 3;
const AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR: Duration = Duration::from_secs(2);
const DECODE_RECOVERY_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
const AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME: Duration = Duration::from_secs(2);
const AUDIO_READER_GAP_WATCHDOG_MAX_OBSERVATIONS: u64 = 64;
const AUDIO_READER_GAP_WATCHDOG_MAX_PTS_SPAN_NSECS: u64 = 5_000_000_000;
const AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
const AUDIO_SYNC_DROP_LOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

fn decode_recovery_staging_frame_budget(
    frame_duration_nsecs: u64,
    vo_queue_capacity: usize,
) -> usize {
    let target_frames = DECODE_RECOVERY_STAGING_NSECS
        .checked_div(frame_duration_nsecs)
        .map(|whole_frames| {
            whole_frames.saturating_add(u64::from(
                !DECODE_RECOVERY_STAGING_NSECS.is_multiple_of(frame_duration_nsecs),
            ))
        })
        .and_then(|frames| usize::try_from(frames).ok())
        .unwrap_or(1)
        .max(1);
    target_frames
        .saturating_add(DECODE_RECOVERY_DECODER_IN_FLIGHT_ALLOWANCE)
        .saturating_add(vo_queue_capacity)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioReaderGapWatchdogDecision {
    Covered,
    InputPending,
    Waiting,
    Request,
    RequestAlreadyIssued,
}

#[derive(Clone, Copy)]
struct AudioReaderGapWatchdogObservation {
    target_timeline_nsecs: u64,
    progress_nsecs: u64,
    has_resume_coverage: bool,
    input_can_fill_gap: bool,
    observed_pts_nsecs: Option<u64>,
    force_immediate_realign: bool,
    now: Instant,
}

#[cfg(test)]
mod audio_gap_watchdog_tests {
    use super::{
        AUDIO_READER_GAP_WATCHDOG_MAX_OBSERVATIONS, AudioReaderGapWatchdogDecision, Instant,
        observe_audio_reader_gap_watchdog,
    };

    #[test]
    fn forced_audio_realign_bypasses_gap_watchdog_delay() {
        let mut watchdog = None;
        assert_eq!(
            observe_audio_reader_gap_watchdog(
                &mut watchdog,
                super::AudioReaderGapWatchdogObservation {
                    target_timeline_nsecs: 1_000_000_000,
                    progress_nsecs: 0,
                    has_resume_coverage: true,
                    input_can_fill_gap: true,
                    observed_pts_nsecs: Some(8_000_000_000),
                    force_immediate_realign: true,
                    now: Instant::now(),
                },
            ),
            AudioReaderGapWatchdogDecision::Request
        );
    }

    #[test]
    fn moving_target_cannot_reset_absolute_observation_bound() {
        let mut watchdog = None;
        let now = Instant::now();
        for observation in 0..AUDIO_READER_GAP_WATCHDOG_MAX_OBSERVATIONS - 1 {
            assert_eq!(
                observe_audio_reader_gap_watchdog(
                    &mut watchdog,
                    super::AudioReaderGapWatchdogObservation {
                        target_timeline_nsecs: 1_000_000_000 + observation,
                        progress_nsecs: 0,
                        has_resume_coverage: false,
                        input_can_fill_gap: true,
                        observed_pts_nsecs: Some(8_000_000_000),
                        force_immediate_realign: false,
                        now,
                    },
                ),
                AudioReaderGapWatchdogDecision::InputPending
            );
        }
        assert_eq!(
            observe_audio_reader_gap_watchdog(
                &mut watchdog,
                super::AudioReaderGapWatchdogObservation {
                    target_timeline_nsecs: 2_000_000_000,
                    progress_nsecs: 0,
                    has_resume_coverage: false,
                    input_can_fill_gap: true,
                    observed_pts_nsecs: Some(8_000_000_000),
                    force_immediate_realign: false,
                    now,
                },
            ),
            AudioReaderGapWatchdogDecision::Request
        );
    }
}

fn observe_audio_reader_gap_watchdog(
    watchdog: &mut Option<AudioReaderGapWatchdog>,
    observation: AudioReaderGapWatchdogObservation,
) -> AudioReaderGapWatchdogDecision {
    let AudioReaderGapWatchdogObservation {
        target_timeline_nsecs,
        progress_nsecs,
        has_resume_coverage,
        input_can_fill_gap,
        observed_pts_nsecs,
        force_immediate_realign,
        now,
    } = observation;
    if has_resume_coverage && !force_immediate_realign {
        *watchdog = None;
        return AudioReaderGapWatchdogDecision::Covered;
    }
    let current = watchdog.get_or_insert(AudioReaderGapWatchdog {
        target_timeline_nsecs,
        started_at: now,
        last_progress_nsecs: progress_nsecs,
        last_progress_at: now,
        observations: 0,
        first_observed_pts_nsecs: observed_pts_nsecs,
        last_observed_pts_nsecs: observed_pts_nsecs,
        request_issued: false,
    });
    if current.target_timeline_nsecs != target_timeline_nsecs {
        // A moving rebuffer target must not restart the absolute watchdog.
        current.target_timeline_nsecs = target_timeline_nsecs;
        current.last_progress_nsecs = progress_nsecs;
    } else if progress_nsecs > current.last_progress_nsecs {
        current.last_progress_nsecs = progress_nsecs;
        current.last_progress_at = now;
        current.request_issued = false;
    }
    current.observations = current.observations.saturating_add(1);
    if let Some(observed_pts_nsecs) = observed_pts_nsecs {
        current
            .first_observed_pts_nsecs
            .get_or_insert(observed_pts_nsecs);
        current.last_observed_pts_nsecs = Some(observed_pts_nsecs);
    }
    if current.request_issued {
        return AudioReaderGapWatchdogDecision::RequestAlreadyIssued;
    }
    let observed_pts_span_nsecs = current
        .first_observed_pts_nsecs
        .zip(current.last_observed_pts_nsecs)
        .map(|(first, last)| first.abs_diff(last))
        .unwrap_or_default();
    let absolute_bound_exhausted = now.saturating_duration_since(current.started_at)
        >= AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME
        || current.observations >= AUDIO_READER_GAP_WATCHDOG_MAX_OBSERVATIONS
        || observed_pts_span_nsecs >= AUDIO_READER_GAP_WATCHDOG_MAX_PTS_SPAN_NSECS;
    if input_can_fill_gap && !force_immediate_realign && !absolute_bound_exhausted {
        return AudioReaderGapWatchdogDecision::InputPending;
    }
    if !force_immediate_realign
        && !absolute_bound_exhausted
        && now.saturating_duration_since(current.last_progress_at)
            < VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER
    {
        return AudioReaderGapWatchdogDecision::Waiting;
    }
    current.request_issued = true;
    AudioReaderGapWatchdogDecision::Request
}

struct AudioGapRecoveryRebufferSuppressionInput {
    now: Instant,
    queued_video_forward_nsecs: Option<u64>,
    audio_output_pending_nsecs: Option<u64>,
    demux_min_forward_nsecs: Option<u64>,
    render_backlogged: bool,
    vo_queued_frames: usize,
    session_id: PlaybackSessionId,
}

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn record_coordinator_tick(
        &mut self,
        elapsed: Duration,
    ) {
        self.scheduled_video_queue
            .record_coordinator_tick(elapsed, Instant::now());
    }

    pub(in crate::player::backend::ffmpeg) fn audio_realign_coverage(
        &self,
        resume_timeline_nsecs: u64,
        target_nsecs: u64,
    ) -> AudioRealignCoverage {
        let protected_target_nsecs =
            target_nsecs.saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        let direct_until_nsecs = self
            .pending_start_audio
            .buffered_until_from(resume_timeline_nsecs);
        let delayed_range = direct_until_nsecs
            .is_none()
            .then(|| {
                self.pending_start_audio
                    .contiguous_range_nsecs()
                    .filter(|(start_nsecs, _)| {
                        *start_nsecs >= resume_timeline_nsecs
                            && start_nsecs.saturating_sub(resume_timeline_nsecs)
                                <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
                    })
            })
            .flatten();
        let audio_accepted_start_timeline_nsecs = direct_until_nsecs
            .map(|_| resume_timeline_nsecs)
            .or_else(|| delayed_range.map(|(start_nsecs, _)| start_nsecs));
        let contiguous_coverage_nsecs = direct_until_nsecs
            .map(|end_nsecs| end_nsecs.saturating_sub(resume_timeline_nsecs))
            .or_else(|| {
                delayed_range.map(|(start_nsecs, end_nsecs)| end_nsecs.saturating_sub(start_nsecs))
            });
        AudioRealignCoverage {
            audio_accepted_start_timeline_nsecs,
            start_gap_nsecs: audio_accepted_start_timeline_nsecs
                .map(|accepted_start| accepted_start.saturating_sub(resume_timeline_nsecs)),
            contiguous_coverage_nsecs,
            protected_target_nsecs,
            ready: contiguous_coverage_nsecs
                .is_some_and(|coverage| coverage >= protected_target_nsecs),
        }
    }

    pub(in crate::player::backend::ffmpeg) fn new() -> Self {
        let playback_output_state = PlaybackOutputState::Syncing;
        Self {
            scheduled_video_queue: ScheduledVideoQueue::default(),
            video_deadline_service: None,
            video_deadline_audio_clock_available: false,
            pending_start_audio: PendingStartAudio::default(),
            first_frame_needed: true,
            first_frame_presented: false,
            output_clock_running: false,
            playback_output_state,
            video_output_underrun_started_at: None,
            rebuffer_started_at: None,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
            audio_sync_drop_before_timeline_nsecs: None,
            audio_sync_drop_log_summary: None,
            rebuffer_audio_realign_request: None,
            audio_reader_gap_watchdog: None,
            audio_continuity_rejection_summary: None,
            decode_recovery_transaction: None,
            syncing_started_at: Some(Instant::now()),
            generation_reset_started_at: Instant::now(),
            defer_pending_start_audio_flush_once: false,
            startup_pending_audio_pressure_context_active: false,
            pending_start_audio_pressure_level: PendingStartAudioPressureLevel::Normal,
            startup_first_frame_stall_logged: false,
            recent_audio_output_underrun_window_started_at: None,
            recent_audio_output_underruns: 0,
            rebuffer_far_ahead_audio_observation_count: 0,
            audio_gap_recovery_until: None,
            audio_gap_recovery_target_nsecs: None,
            initial_delayed_audio_start_timeline_nsecs: None,
            initial_audio_gap_at_video_start_timeline_nsecs: None,
            initial_av_start_transaction: None,
            last_initial_audio_prepare_terminal_phase: None,
            next_initial_av_start_transaction_id: 1,
            initial_av_pair_started_at: None,
            initial_sync_log_state: None,
            initial_audio_defer_log_state: None,
            prestart_audio_ownership_log_state: None,
            pending_audio_backpressure_log_state: None,
            output_gate_block_log_state: None,
            discontinuity_epoch: 0,
            output_housekeeping_generation: 1,
            output_housekeeping_serviced_generation: 0,
            last_output_housekeeping_service_at: None,
            video_clock_anchor_valid: false,
            audio_output_activity_watchdog: None,
            audio_output_clock_stall_fallback_active: false,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn reset(&mut self, control: &FfmpegControl) {
        self.reset_for_presentation_session(control, None);
    }

    pub(in crate::player::backend::ffmpeg) fn reset_for_session(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) {
        self.reset_for_presentation_session(control, Some(session_id));
    }

    fn reset_for_presentation_session(
        &mut self,
        control: &FfmpegControl,
        session_id: Option<PlaybackSessionId>,
    ) {
        self.generation_reset_started_at = Instant::now();
        if let Some(session_id) = session_id {
            self.scheduled_video_queue
                .clear_and_bind_presentation_session(session_id);
        } else {
            self.scheduled_video_queue.clear();
        }
        self.pending_start_audio.clear();
        self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        self.startup_first_frame_stall_logged = false;
        self.initial_delayed_audio_start_timeline_nsecs = None;
        self.initial_audio_gap_at_video_start_timeline_nsecs = None;
        self.initial_av_start_transaction = None;
        self.last_initial_audio_prepare_terminal_phase = None;
        self.initial_av_pair_started_at = None;
        self.initial_sync_log_state = None;
        self.initial_audio_defer_log_state = None;
        self.prestart_audio_ownership_log_state = None;
        self.pending_audio_backpressure_log_state = None;
        self.output_gate_block_log_state = None;
        self.output_housekeeping_generation = 1;
        self.output_housekeeping_serviced_generation = 0;
        self.last_output_housekeeping_service_at = None;
        self.first_frame_needed = true;
        self.first_frame_presented = false;
        self.output_clock_running = false;
        self.startup_pending_audio_pressure_context_active = false;
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.set_state(PlaybackOutputState::Syncing);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Syncing);
        self.video_output_underrun_started_at = None;
        self.rebuffer_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_bootstrap_after_seek = false;
        self.video_decode_underfill = false;
        self.rebuffer_empty_audio_output_blocked = false;
        self.finish_audio_sync_drop_log_summary(control.session_id(), "output_reset");
        self.audio_sync_drop_before_timeline_nsecs = None;
        self.rebuffer_audio_realign_request = None;
        self.audio_reader_gap_watchdog = None;
        self.audio_continuity_rejection_summary = None;
        self.decode_recovery_transaction = None;
        self.rebuffer_far_ahead_audio_observation_count = 0;
        self.audio_gap_recovery_until = None;
        self.audio_gap_recovery_target_nsecs = None;
        self.recent_audio_output_underrun_window_started_at = None;
        self.recent_audio_output_underruns = 0;
        self.video_clock_anchor_valid = false;
        self.reset_audio_output_activity_watchdog();
    }

    pub(in crate::player::backend::ffmpeg) fn begin_decode_recovery(
        &mut self,
        transaction_id: u64,
        target_nsecs: u64,
        source: DecodeRecoverySource,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) {
        if self.playback_output_state.rebuffering() || control.is_output_rebuffer_paused() {
            clear_video_output_rebuffer(&mut self.playback_output_state, control);
            self.set_state(PlaybackOutputState::Playing);
        }
        let discarded_future_frames = self.scheduled_video_queue.discard_at_or_after(target_nsecs);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        let started_at = Instant::now();
        self.decode_recovery_transaction = Some(DecodeRecoveryTransaction {
            transaction_id: transaction_id.max(1),
            target_nsecs,
            resume_nsecs: target_nsecs,
            phase: DecodeRecoveryPhase::SwitchingDecoder,
            disposition: DecodeRecoveryDisposition::Exact,
            source,
            gap_provenance: DecodeRecoveryGapProvenance::ContinuousMedia,
            drop_for_fallback: None,
            staging_queue: ScheduledVideoQueue::default(),
            staging_frame_budget: None,
            staging_budget_reached: false,
            audio_ready_latched: false,
            started_at,
            barrier_started_at: None,
            first_staged_frame_nsecs: None,
            last_staged_frame_nsecs: None,
            largest_confirmed_gap_nsecs: 0,
            bridged_gap_count: 0,
            rejected_frame_count: 0,
            first_rejected_frame_nsecs: None,
            last_rejected_frame_nsecs: None,
            last_rejection_log_at: None,
        });
        // Decoder input can become blocked as soon as recovery owns frame
        // admission. Wake the output side independently so the transaction is
        // not waiting for another demux/decode event to make progress.
        self.note_output_housekeeping_change();
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = transaction_id.max(1),
            target_nsecs,
            recovery_source = source.as_str(),
            discarded_future_frames,
            retained_video_range = ?self.scheduled_video_queue.range_nsecs(),
            "started independent HEVC decode recovery output transaction"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn mark_decode_recovery_replaying(
        &mut self,
        transaction_id: u64,
    ) {
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return;
        };
        if transaction.transaction_id == transaction_id
            && transaction.phase == DecodeRecoveryPhase::SwitchingDecoder
        {
            transaction.phase = DecodeRecoveryPhase::Replaying;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_phase(
        &self,
    ) -> Option<DecodeRecoveryPhase> {
        self.decode_recovery_transaction
            .as_ref()
            .map(|transaction| transaction.phase)
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_active(&self) -> bool {
        self.decode_recovery_phase()
            .is_some_and(|phase| !phase.terminal())
    }

    fn decode_recovery_drained_boundary_ready(&self) -> bool {
        self.scheduled_video_queue.is_empty()
            && self
                .decode_recovery_transaction
                .as_ref()
                .is_some_and(|transaction| {
                    transaction.phase == DecodeRecoveryPhase::Buffered
                        && transaction.source == DecodeRecoverySource::DecoderError
                        && transaction.disposition == DecodeRecoveryDisposition::Reanchor
                })
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_owns_video_admission(&self) -> bool {
        self.decode_recovery_phase().is_some_and(|phase| {
            !matches!(
                phase,
                DecodeRecoveryPhase::Committed
                    | DecodeRecoveryPhase::CommittedGap
                    | DecodeRecoveryPhase::Reanchored
            )
        })
    }

    pub(in crate::player::backend::ffmpeg) fn committed_video_queue_end_nsecs(
        &self,
    ) -> Option<u64> {
        self.scheduled_video_queue
            .range_nsecs()
            .map(|(_, end_nsecs)| end_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn admitted_video_queue_end_nsecs(&self) -> Option<u64> {
        self.committed_video_queue_end_nsecs()
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staged_high_water_nsecs(
        &self,
    ) -> Option<u64> {
        self.recovery_staging_range_nsecs()
            .map(|(_, end_nsecs)| end_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_range_nsecs(
        &self,
    ) -> Option<(u64, u64)> {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .and_then(|transaction| transaction.staging_queue.range_nsecs())
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_frames(&self) -> usize {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .map(|transaction| transaction.staging_queue.len())
            .unwrap_or_default()
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_frame_budget(
        &self,
    ) -> Option<usize> {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .and_then(|transaction| transaction.staging_frame_budget)
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_video_admission_blocked(
        &self,
    ) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .is_some_and(|transaction| match transaction.phase {
                DecodeRecoveryPhase::Committed
                | DecodeRecoveryPhase::CommittedGap
                | DecodeRecoveryPhase::Reanchored => false,
                DecodeRecoveryPhase::Buffered
                | DecodeRecoveryPhase::DroppedForFallback
                | DecodeRecoveryPhase::Failed => true,
                DecodeRecoveryPhase::SwitchingDecoder | DecodeRecoveryPhase::Replaying => {
                    transaction.staging_budget_reached
                }
            })
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_output_committed(&self) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .is_some_and(|transaction| {
                matches!(
                    transaction.phase,
                    DecodeRecoveryPhase::Committed
                        | DecodeRecoveryPhase::CommittedGap
                        | DecodeRecoveryPhase::Reanchored
                )
            })
    }

    pub(in crate::player::backend::ffmpeg) fn confirm_decode_recovery_synchronized_timeline_gap(
        &mut self,
    ) {
        if let Some(transaction) = self.decode_recovery_transaction.as_mut()
            && !transaction.phase.terminal()
        {
            transaction.gap_provenance =
                DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn take_decode_recovery_drop_for_fallback(
        &mut self,
    ) -> Option<DecodeRecoveryDropForFallback> {
        self.decode_recovery_transaction
            .as_mut()
            .and_then(|transaction| transaction.drop_for_fallback.take())
    }

    pub(in crate::player::backend::ffmpeg) fn release_vulkan_frames_for_resource_pressure(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> usize {
        let released_scheduled_frames = self.scheduled_video_queue.len();
        let released_scheduled_range = self.scheduled_video_queue.range_nsecs();
        self.scheduled_video_queue.clear();
        let mut transaction_id = None;
        let mut released_staging_frames = 0;
        let mut released_staging_range = None;
        if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
            transaction_id = Some(transaction.transaction_id);
            released_staging_frames = transaction.staging_queue.len();
            released_staging_range = transaction.staging_queue.range_nsecs();
            transaction.staging_queue.clear();
            transaction.staging_budget_reached = true;
            transaction.phase = DecodeRecoveryPhase::Failed;
        }
        control.set_output_rebuffer_paused(true);
        let released_frames = released_scheduled_frames.saturating_add(released_staging_frames);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = ?transaction_id,
            released_scheduled_video_frames = released_scheduled_frames,
            released_scheduled_video_range = ?released_scheduled_range,
            released_recovery_staging_frames = released_staging_frames,
            released_recovery_staging_range = ?released_staging_range,
            released_vulkan_frames = released_frames,
            "released scheduler-owned Vulkan frames after decoder resource pressure"
        );
        released_frames
    }

    pub(in crate::player::backend::ffmpeg) fn update_decode_recovery_audio_ready(
        &mut self,
        has_audio_output: bool,
        audio_snapshot: Option<AudioOutputSnapshot>,
        audio_decode_queued_nsecs: u64,
    ) -> bool {
        if !has_audio_output {
            if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
                transaction.audio_ready_latched = true;
            }
            return true;
        }
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return true;
        };
        if transaction.disposition == DecodeRecoveryDisposition::Reanchor {
            transaction.audio_ready_latched = true;
            return true;
        }
        if transaction.audio_ready_latched {
            return true;
        }

        let device_pending_nsecs = audio_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs)
            .unwrap_or_default();
        let pending_start_forward_nsecs = self
            .pending_start_audio
            .forward_duration_from(transaction.resume_nsecs)
            .unwrap_or_default();
        let available_nsecs = device_pending_nsecs
            .saturating_add(audio_decode_queued_nsecs)
            .saturating_add(pending_start_forward_nsecs);
        let ready_threshold_nsecs = DECODE_RECOVERY_STAGING_NSECS
            .saturating_sub(DECODE_RECOVERY_AUDIO_READY_HYSTERESIS_NSECS);
        if available_nsecs >= ready_threshold_nsecs {
            transaction.audio_ready_latched = true;
            tracing::debug!(
                transaction_id = transaction.transaction_id,
                resume_nsecs = transaction.resume_nsecs,
                available_audio_ms = available_nsecs as f64 / 1_000_000.0,
                device_pending_ms = device_pending_nsecs as f64 / 1_000_000.0,
                decoded_audio_ms = audio_decode_queued_nsecs as f64 / 1_000_000.0,
                pending_start_forward_ms = pending_start_forward_nsecs as f64 / 1_000_000.0,
                ready_threshold_ms = ready_threshold_nsecs as f64 / 1_000_000.0,
                "latched audio readiness for HEVC decode recovery commit"
            );
        }
        transaction.audio_ready_latched
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_audio_ready_latched(&self) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .map(|transaction| transaction.audio_ready_latched)
            .unwrap_or(true)
    }

    pub(in crate::player::backend::ffmpeg) fn stage_decode_recovery_frame(
        &mut self,
        mut frame: QueuedVideoFrame,
        vo_queue_capacity: usize,
        session_id: PlaybackSessionId,
    ) -> bool {
        let now = Instant::now();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return false;
        };
        if transaction.phase.terminal() {
            return false;
        }

        let staging_frame_budget = *transaction.staging_frame_budget.get_or_insert_with(|| {
            decode_recovery_staging_frame_budget(frame.duration_nsecs, vo_queue_capacity)
        });
        if transaction.staging_queue.len() >= staging_frame_budget {
            if !transaction.staging_budget_reached {
                transaction.staging_budget_reached = true;
                tracing::warn!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    phase = ?transaction.phase,
                    staging_frames = transaction.staging_queue.len(),
                    staging_frame_budget,
                    recovery_staged_high_water_nsecs = ?transaction
                        .staging_queue
                        .range_nsecs()
                        .map(|(_, end)| end),
                    "HEVC decode recovery staging reached its hard Vulkan-frame budget"
                );
            }
            return true;
        }

        let original_frame_nsecs = frame.timeline_nsecs;
        let frame_end_nsecs = frame.timeline_nsecs.saturating_add(frame.duration_nsecs);
        if frame_end_nsecs <= transaction.target_nsecs {
            transaction.rejected_frame_count = transaction.rejected_frame_count.saturating_add(1);
            transaction
                .first_rejected_frame_nsecs
                .get_or_insert(original_frame_nsecs);
            transaction.last_rejected_frame_nsecs = Some(original_frame_nsecs);
            let should_log = transaction.rejected_frame_count == 1
                || transaction.last_rejection_log_at.is_none_or(|last_log_at| {
                    now.saturating_duration_since(last_log_at)
                        >= DECODE_RECOVERY_REJECTION_LOG_INTERVAL
                });
            if should_log {
                transaction.last_rejection_log_at = Some(now);
                tracing::debug!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    target_nsecs = transaction.target_nsecs,
                    rejected_frame_nsecs = original_frame_nsecs,
                    rejected_frame_end_nsecs = frame_end_nsecs,
                    rejected_frame_count = transaction.rejected_frame_count,
                    "rate-limited pre-boundary frame rejection during HEVC decode recovery"
                );
            }
            return true;
        }

        if transaction.staging_queue.is_empty() {
            if frame.timeline_nsecs < transaction.target_nsecs {
                frame.timeline_nsecs = transaction.target_nsecs;
                frame.duration_nsecs = frame_end_nsecs.saturating_sub(transaction.target_nsecs);
            } else {
                let initial_gap_nsecs = frame
                    .timeline_nsecs
                    .saturating_sub(transaction.target_nsecs);
                transaction.largest_confirmed_gap_nsecs = transaction
                    .largest_confirmed_gap_nsecs
                    .max(initial_gap_nsecs);
                if transaction.source == DecodeRecoverySource::DecoderError
                    && initial_gap_nsecs > DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS
                {
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        recovery_source = transaction.source.as_str(),
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        decoder_output_gap_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                        "scheduled HEVC decoder-error gap for boundary reanchor"
                    );
                } else if initial_gap_nsecs > 0
                    && decode_recovery_gap_within_limit(
                        initial_gap_nsecs,
                        DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                    )
                {
                    transaction.disposition = DecodeRecoveryDisposition::HoldGap;
                    transaction.bridged_gap_count = transaction.bridged_gap_count.saturating_add(1);
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        first_software_frame_nsecs = frame.timeline_nsecs,
                        confirmed_gap_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                        hold_gap_limit_ms =
                            DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                        timestamp_tolerance_ms =
                            DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS as f64 / 1_000_000.0,
                        "accepted recovery frame after confirmed media timeline gap"
                    );
                } else if !decode_recovery_gap_within_limit(
                    initial_gap_nsecs,
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                ) {
                    if transaction.gap_provenance
                        == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
                    {
                        transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                        transaction.resume_nsecs = frame.timeline_nsecs;
                        tracing::warn!(
                            session_id = ?session_id,
                            transaction_id = transaction.transaction_id,
                            recovery_source = transaction.source.as_str(),
                            gap_provenance = ?transaction.gap_provenance,
                            target_nsecs = transaction.target_nsecs,
                            resume_nsecs = transaction.resume_nsecs,
                            discontinuity_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                            hold_gap_limit_ms =
                                DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                            "reanchoring HEVC decode recovery at confirmed synchronized media discontinuity"
                        );
                    } else {
                        transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                        transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                        transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                            transaction_id: transaction.transaction_id,
                            target_nsecs: transaction.target_nsecs,
                            first_frame_nsecs: frame.timeline_nsecs,
                            gap_nsecs: initial_gap_nsecs,
                            source: transaction.source,
                        });
                        tracing::warn!(
                            session_id = ?session_id,
                            transaction_id = transaction.transaction_id,
                            recovery_source = transaction.source.as_str(),
                            gap_provenance = ?transaction.gap_provenance,
                            target_nsecs = transaction.target_nsecs,
                            first_frame_nsecs = frame.timeline_nsecs,
                            discontinuity_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                            "withheld recovery frame at unbridged continuous decode gap"
                        );
                        self.note_output_housekeeping_change();
                        return true;
                    }
                }
            }
        } else if let Some((previous_pts_nsecs, previous_duration_nsecs)) =
            transaction.staging_queue.back_timing_nsecs()
        {
            let previous_end_nsecs = previous_pts_nsecs.saturating_add(previous_duration_nsecs);
            let gap_nsecs = frame.timeline_nsecs.saturating_sub(previous_end_nsecs);
            if gap_nsecs > 0 {
                transaction.largest_confirmed_gap_nsecs =
                    transaction.largest_confirmed_gap_nsecs.max(gap_nsecs);
                if transaction.source == DecodeRecoverySource::DecoderError
                    && gap_nsecs > DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS
                {
                    let discarded_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    transaction.first_staged_frame_nsecs = None;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        decoder_output_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                        discarded_staging_frames,
                        "rescheduled HEVC decoder-error recovery at later boundary gap"
                    );
                } else if decode_recovery_gap_within_limit(
                    gap_nsecs,
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                ) {
                    if transaction
                        .staging_queue
                        .extend_back_duration_to(frame.timeline_nsecs)
                        .is_some()
                    {
                        transaction.bridged_gap_count =
                            transaction.bridged_gap_count.saturating_add(1);
                        if transaction.disposition == DecodeRecoveryDisposition::Exact {
                            transaction.disposition = DecodeRecoveryDisposition::HoldGap;
                        }
                    }
                } else if transaction.gap_provenance
                    == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
                {
                    let discarded_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    transaction.first_staged_frame_nsecs = None;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        discontinuity_ms = gap_nsecs as f64 / 1_000_000.0,
                        discarded_staging_frames,
                        "reanchoring HEVC decode recovery at large staging discontinuity"
                    );
                } else {
                    let released_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                    transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                    transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                        transaction_id: transaction.transaction_id,
                        target_nsecs: transaction.target_nsecs,
                        first_frame_nsecs: frame.timeline_nsecs,
                        gap_nsecs,
                        source: transaction.source,
                    });
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        recovery_source = transaction.source.as_str(),
                        gap_provenance = ?transaction.gap_provenance,
                        target_nsecs = transaction.target_nsecs,
                        first_frame_nsecs = frame.timeline_nsecs,
                        discontinuity_ms = gap_nsecs as f64 / 1_000_000.0,
                        released_staging_frames,
                        "withheld staged recovery frame at unbridged continuous decode gap"
                    );
                    self.note_output_housekeeping_change();
                    return true;
                }
            }
        }

        // HoldGap and Reanchor both account for a timeline interval where the
        // decoder returned no usable pictures; that skipped interval is not
        // replay work. Match mpv's discontinuity handling and measure the
        // bounded buffer built after the first usable frame/new anchor.
        let replay_span_origin_nsecs = match transaction.disposition {
            DecodeRecoveryDisposition::HoldGap => transaction
                .first_staged_frame_nsecs
                .unwrap_or(frame.timeline_nsecs),
            DecodeRecoveryDisposition::Reanchor => transaction.resume_nsecs,
            DecodeRecoveryDisposition::Exact | DecodeRecoveryDisposition::DropForFallback => {
                transaction.target_nsecs
            }
        };
        let replay_span_nsecs = frame_end_nsecs.saturating_sub(replay_span_origin_nsecs);
        if replay_span_nsecs >= DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS {
            let discarded_staging_frames = transaction.staging_queue.len();
            transaction.staging_queue.clear();
            transaction.staging_budget_reached = true;
            if transaction.gap_provenance
                == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
            {
                transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                transaction.resume_nsecs = frame.timeline_nsecs;
                transaction.first_staged_frame_nsecs = Some(frame.timeline_nsecs);
                transaction.last_staged_frame_nsecs = Some(frame.timeline_nsecs);
                transaction.staging_queue.push_queued(frame);
                transaction.phase = DecodeRecoveryPhase::Buffered;
                tracing::warn!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    disposition = ?transaction.disposition,
                    replay_span_origin_nsecs,
                    replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                    replay_span_limit_ms =
                        DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS as f64 / 1_000_000.0,
                    discarded_staging_frames,
                    staging_frames = transaction.staging_queue.len(),
                    resume_nsecs = transaction.resume_nsecs,
                    "bounded HEVC recovery reanchored at the replay-span limit"
                );
            } else {
                let gap_nsecs = frame
                    .timeline_nsecs
                    .saturating_sub(transaction.target_nsecs);
                transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                    transaction_id: transaction.transaction_id,
                    target_nsecs: transaction.target_nsecs,
                    first_frame_nsecs: frame.timeline_nsecs,
                    gap_nsecs,
                    source: transaction.source,
                });
                tracing::error!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    disposition = ?transaction.disposition,
                    replay_span_origin_nsecs,
                    replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                    replay_span_limit_ms =
                        DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS as f64 / 1_000_000.0,
                    discarded_staging_frames,
                    first_frame_nsecs = frame.timeline_nsecs,
                    gap_ms = gap_nsecs as f64 / 1_000_000.0,
                    "failed bounded HEVC recovery at the replay-span limit"
                );
                self.note_output_housekeeping_change();
            }
            return true;
        }

        transaction
            .first_staged_frame_nsecs
            .get_or_insert(frame.timeline_nsecs);
        transaction.last_staged_frame_nsecs = Some(frame.timeline_nsecs);
        transaction.staging_queue.push_queued(frame);
        let staging_forward_nsecs = transaction
            .staging_queue
            .strict_forward_nsecs_from(transaction.resume_nsecs)
            .unwrap_or_default();
        let wall_time_exhausted =
            now.saturating_duration_since(transaction.started_at) >= DECODE_RECOVERY_MAX_WALL_TIME;
        let replay_span_exhausted = replay_span_nsecs >= DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS;
        let force_bounded_reanchor = staging_forward_nsecs < DECODE_RECOVERY_STAGING_NSECS
            && (wall_time_exhausted || replay_span_exhausted)
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor;
        let entered_buffered = transaction.phase != DecodeRecoveryPhase::Buffered
            && (staging_forward_nsecs >= DECODE_RECOVERY_STAGING_NSECS || force_bounded_reanchor);
        if entered_buffered {
            transaction.phase = DecodeRecoveryPhase::Buffered;
            tracing::debug!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                recovery_source = transaction.source.as_str(),
                gap_provenance = ?transaction.gap_provenance,
                target_nsecs = transaction.target_nsecs,
                resume_nsecs = transaction.resume_nsecs,
                disposition = ?transaction.disposition,
                staging_frames = transaction.staging_queue.len(),
                staging_forward_ms = staging_forward_nsecs as f64 / 1_000_000.0,
                replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                wall_elapsed_ms = now
                    .saturating_duration_since(transaction.started_at)
                    .as_secs_f64()
                    * 1_000.0,
                bounded_reanchor = force_bounded_reanchor,
                bridged_gap_count = transaction.bridged_gap_count,
                largest_confirmed_gap_ms =
                    transaction.largest_confirmed_gap_nsecs as f64 / 1_000_000.0,
                "buffered gap-aware frames for atomic decode recovery commit"
            );
        }
        if transaction.staging_queue.len() >= staging_frame_budget
            && !transaction.staging_budget_reached
        {
            transaction.staging_budget_reached = true;
            tracing::warn!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                phase = ?transaction.phase,
                staging_frames = transaction.staging_queue.len(),
                staging_frame_budget,
                recovery_staged_high_water_nsecs = ?transaction
                    .staging_queue
                    .range_nsecs()
                    .map(|(_, end)| end),
                "HEVC decode recovery staging reached its hard Vulkan-frame budget"
            );
        }
        if entered_buffered {
            // mpv wakes its core whenever decoder/output state changes. Do the
            // same here because cache backpressure may otherwise prevent the
            // coordinator from revisiting the output gate.
            self.note_output_housekeeping_change();
        }
        true
    }

    pub(in crate::player::backend::ffmpeg) fn check_decode_recovery_deadline(
        &mut self,
        now: Instant,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<(), String> {
        let drained_decoder_error_boundary = self.decode_recovery_drained_boundary_ready();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return Ok(());
        };
        if transaction.phase.terminal()
            || now.saturating_duration_since(transaction.started_at) < DECODE_RECOVERY_MAX_WALL_TIME
        {
            return Ok(());
        }
        if drained_decoder_error_boundary {
            // The transaction is already commit-ready. Let the output service
            // enter its barrier and commit below instead of turning a single
            // scheduling race at the wall-time boundary into a fatal stall.
            return Ok(());
        }

        transaction.phase = DecodeRecoveryPhase::Failed;
        let transaction_id = transaction.transaction_id;
        let target_nsecs = transaction.target_nsecs;
        let resume_nsecs = transaction.resume_nsecs;
        let disposition = transaction.disposition;
        let source = transaction.source;
        let gap_provenance = transaction.gap_provenance;
        let elapsed = now.saturating_duration_since(transaction.started_at);
        let staging_frames = transaction.staging_queue.len();
        let first_staged_frame_nsecs = transaction.first_staged_frame_nsecs;
        let last_staged_frame_nsecs = transaction.last_staged_frame_nsecs;
        let rejected_frame_count = transaction.rejected_frame_count;
        let first_rejected_frame_nsecs = transaction.first_rejected_frame_nsecs;
        let last_rejected_frame_nsecs = transaction.last_rejected_frame_nsecs;
        transaction.staging_queue.clear();
        control.set_output_rebuffer_paused(false);
        tracing::error!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            resume_nsecs,
            disposition = ?disposition,
            recovery_source = source.as_str(),
            gap_provenance = ?gap_provenance,
            elapsed_ms = elapsed.as_secs_f64() * 1_000.0,
            staging_frames,
            first_staged_frame_nsecs,
            last_staged_frame_nsecs,
            rejected_frame_count,
            first_rejected_frame_nsecs,
            last_rejected_frame_nsecs,
            "HEVC decode recovery exceeded bounded wall-time deadline"
        );
        Err(format!(
            "HEVC 解码恢复事务 {transaction_id} 在 {:.0}ms 内未形成可提交输出（目标 {:.3}s，策略 {:?}）",
            elapsed.as_secs_f64() * 1_000.0,
            target_nsecs as f64 / 1_000_000_000.0,
            disposition,
        ))
    }

    pub(in crate::player::backend::ffmpeg) fn commit_decode_recovery_if_buffered(
        &mut self,
        control: &FfmpegControl,
        scheduler: &mut PlaybackScheduler,
        audio_output: Option<&AudioOutput>,
        current_start_position_nsecs: &mut u64,
        audio_ready: bool,
        session_id: PlaybackSessionId,
    ) -> bool {
        let Some(transaction) = self.decode_recovery_transaction.as_ref() else {
            return false;
        };
        if transaction.phase != DecodeRecoveryPhase::Buffered {
            return false;
        }
        if transaction.source == DecodeRecoverySource::DecoderError
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor
            && transaction.barrier_started_at.is_none()
        {
            // Preserve already-decoded good frames until playback reaches the
            // exact broken-chain boundary. Reanchoring earlier would trade the
            // visible gap for an even larger, premature content skip.
            return false;
        }
        if transaction.barrier_started_at.is_some()
            && !audio_ready
            && transaction.disposition != DecodeRecoveryDisposition::Reanchor
        {
            return false;
        }

        let transaction_id = transaction.transaction_id;
        let target_nsecs = transaction.target_nsecs;
        let resume_nsecs = transaction.resume_nsecs;
        let disposition = transaction.disposition;
        let source = transaction.source;
        let gap_provenance = transaction.gap_provenance;
        let barrier_elapsed = transaction
            .barrier_started_at
            .map(|started_at| started_at.elapsed());
        let first_staged_frame_nsecs = transaction.first_staged_frame_nsecs;
        let last_staged_frame_nsecs = transaction.last_staged_frame_nsecs;
        let bridged_gap_count = transaction.bridged_gap_count;
        let largest_confirmed_gap_nsecs = transaction.largest_confirmed_gap_nsecs;
        let rejected_frame_count = transaction.rejected_frame_count;
        let first_rejected_frame_nsecs = transaction.first_rejected_frame_nsecs;
        let last_rejected_frame_nsecs = transaction.last_rejected_frame_nsecs;
        let recovery_elapsed = transaction.started_at.elapsed();

        let mut extended_visible_frame = false;
        let mut discarded_retained_video_frames = 0usize;
        if disposition == DecodeRecoveryDisposition::HoldGap {
            extended_visible_frame = first_staged_frame_nsecs.is_some_and(|first_frame_nsecs| {
                self.scheduled_video_queue
                    .extend_back_duration_to(first_frame_nsecs)
                    .is_some()
            });
        } else if disposition == DecodeRecoveryDisposition::Reanchor {
            discarded_retained_video_frames = self.scheduled_video_queue.len();
            self.scheduled_video_queue.clear();
        }

        let appended_frames = {
            let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
                return false;
            };
            self.scheduled_video_queue
                .append_from(&mut transaction.staging_queue)
        };

        let terminal_phase = match disposition {
            DecodeRecoveryDisposition::Exact => DecodeRecoveryPhase::Committed,
            DecodeRecoveryDisposition::HoldGap => DecodeRecoveryPhase::CommittedGap,
            DecodeRecoveryDisposition::Reanchor => DecodeRecoveryPhase::Reanchored,
            DecodeRecoveryDisposition::DropForFallback => DecodeRecoveryPhase::DroppedForFallback,
        };
        if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
            transaction.phase = terminal_phase;
        }

        let mut dropped_pending_audio_frames = 0usize;
        if disposition == DecodeRecoveryDisposition::Reanchor {
            dropped_pending_audio_frames = self.pending_start_audio.len();
            self.pending_start_audio.clear();
            self.set_audio_sync_drop_before_timeline_nsecs(
                resume_nsecs,
                session_id,
                "decode_recovery_large_discontinuity",
            );
            if let Some(audio_output) = audio_output {
                audio_output.reset_clock(resume_nsecs);
            }
            scheduler.reset(resume_nsecs);
            self.mark_video_clock_anchor_valid();
            *current_start_position_nsecs = resume_nsecs;
        } else if let Some(elapsed) = barrier_elapsed {
            scheduler.delay_by(elapsed);
            self.mark_video_clock_anchor_valid();
        }

        if barrier_elapsed.is_some()
            || disposition == DecodeRecoveryDisposition::Reanchor
            || control.is_output_rebuffer_paused()
        {
            control.set_output_rebuffer_paused(false);
            self.video_output_rebuffer_anchor = None;
            self.video_output_underrun_started_at = None;
            self.rebuffer_started_at = None;
            self.video_decode_underfill = false;
            self.clear_video_bootstrap_after_seek("decode_recovery_commit");
            self.set_state(PlaybackOutputState::Playing);
        }
        tracing::warn!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            resume_nsecs,
            disposition = ?disposition,
            recovery_source = source.as_str(),
            gap_provenance = ?gap_provenance,
            terminal_phase = ?terminal_phase,
            appended_frames,
            barrier_elapsed_ms = ?barrier_elapsed.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            recovery_elapsed_ms = recovery_elapsed.as_secs_f64() * 1_000.0,
            first_staged_frame_nsecs,
            last_staged_frame_nsecs,
            bridged_gap_count,
            largest_confirmed_gap_ms = largest_confirmed_gap_nsecs as f64 / 1_000_000.0,
            extended_visible_frame,
            discarded_retained_video_frames,
            dropped_pending_audio_frames,
            rejected_frame_count,
            first_rejected_frame_nsecs,
            last_rejected_frame_nsecs,
            committed_video_range = ?self.scheduled_video_queue.range_nsecs(),
            "atomically committed frames for HEVC decode recovery"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn maybe_enter_decode_recovery_barrier(
        &mut self,
        now: Instant,
        media_clock_nsecs: u64,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> bool {
        let retained_video_queue_empty = self.scheduled_video_queue.is_empty();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return false;
        };
        let buffered_decoder_reanchor = transaction.phase == DecodeRecoveryPhase::Buffered
            && transaction.source == DecodeRecoverySource::DecoderError
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor;
        let retained_boundary_drained = buffered_decoder_reanchor && retained_video_queue_empty;
        if (transaction.phase == DecodeRecoveryPhase::Buffered && !buffered_decoder_reanchor)
            || transaction.phase.terminal()
            || transaction.barrier_started_at.is_some()
            || (media_clock_nsecs < transaction.target_nsecs && !retained_boundary_drained)
        {
            return false;
        }
        transaction.barrier_started_at = Some(now);
        let target_nsecs = transaction.target_nsecs;
        let transaction_id = transaction.transaction_id;
        self.set_state(PlaybackOutputState::Rebuffering);
        self.rebuffer_started_at = Some(now);
        self.video_output_rebuffer_anchor = Some(RebufferResumeAnchor {
            timeline_nsecs: target_nsecs,
            reset_to_video_when_decoded_queue_misses_anchor: false,
        });
        control.set_output_rebuffer_paused(true);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            media_clock_nsecs,
            retained_video_queue_empty,
            boundary_reason = if retained_boundary_drained {
                "retained_video_queue_drained"
            } else {
                "media_clock"
            },
            "entered dedicated decode recovery barrier at gap boundary"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer(&mut self, control: &FfmpegControl) {
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.video_output_underrun_started_at = None;
        self.rebuffer_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        self.clear_video_bootstrap_after_seek("clear_rebuffer");
        self.rebuffer_empty_audio_output_blocked = false;
        self.audio_reader_gap_watchdog = None;
        self.rebuffer_far_ahead_audio_observation_count = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffering(&self) -> bool {
        self.playback_output_state.rebuffering()
    }

    /// True while the output is building up the decoded buffer to (re)start
    /// playback (initial sync or rebuffer). During this phase the soft Vulkan
    /// frame-pressure throttle is lifted so decode can reach the resume waterline.
    pub(in crate::player::backend::ffmpeg) fn output_fill_phase(&self) -> bool {
        self.restart_pending() || self.playback_output_state.rebuffering()
    }

    /// The sole restart gate predicate.  `Syncing` means the decoded sides are
    /// still filling; `Primed` means the video side is ready and the atomic A/V
    /// commit is waiting on audio.  Ordinary underrun recovery is legal only
    /// after this becomes false and the state reaches `Playing`.
    pub(in crate::player::backend::ffmpeg) fn restart_pending(&self) -> bool {
        self.playback_output_state.restart_pending()
    }

    pub(in crate::player::backend::ffmpeg) fn discontinuity_epoch(&self) -> u64 {
        self.discontinuity_epoch
    }

    pub(in crate::player::backend::ffmpeg) fn restart_fallback_deadline_armed(&self) -> bool {
        if self.playback_output_state.rebuffering() {
            return self.rebuffer_started_at.is_some();
        }
        self.restart_pending()
            && (self.initial_av_start_transaction.is_some()
                || self.initial_av_pair_started_at.is_some()
                || self.syncing_started_at.is_some())
    }

    pub(in crate::player::backend::ffmpeg) fn advance_discontinuity_epoch(&mut self) -> u64 {
        self.discontinuity_epoch = self.discontinuity_epoch.saturating_add(1);
        self.discontinuity_epoch
    }

    pub(in crate::player::backend::ffmpeg) fn set_state(&mut self, state: PlaybackOutputState) {
        let previous_state = self.playback_output_state;
        self.playback_output_state = state;
        self.refresh_video_deadline_service_active();
        if state.rebuffering() {
            // Every path into Rebuffering owns the same monotonic fallback
            // deadline.  Callers that know the observation time may replace
            // this value immediately after the transition.
            self.rebuffer_started_at.get_or_insert_with(Instant::now);
        } else if previous_state.rebuffering() {
            self.rebuffer_started_at = None;
        }
        if (state == PlaybackOutputState::Syncing && previous_state != state)
            || state == PlaybackOutputState::Playing
            || state.rebuffering()
        {
            self.initial_sync_log_state = None;
            self.initial_audio_defer_log_state = None;
            self.prestart_audio_ownership_log_state = None;
            self.pending_audio_backpressure_log_state = None;
            self.output_gate_block_log_state = None;
        }
        if state == PlaybackOutputState::Syncing || state.rebuffering() {
            self.video_clock_anchor_valid = false;
        }
        self.syncing_started_at = (state == PlaybackOutputState::Syncing).then(Instant::now);
        if state == PlaybackOutputState::Syncing {
            self.first_frame_needed = self.scheduled_video_queue.is_empty();
            self.first_frame_presented = false;
            self.output_clock_running = false;
        } else if state == PlaybackOutputState::Primed {
            self.first_frame_needed = false;
            self.output_clock_running = true;
        } else if state == PlaybackOutputState::Playing {
            self.first_frame_needed = false;
            self.initial_av_start_transaction = None;
            self.initial_av_pair_started_at = None;
            self.output_clock_running = true;
        } else if state.rebuffering() {
            self.first_frame_needed = false;
            self.initial_av_start_transaction = None;
            self.initial_av_pair_started_at = None;
            self.output_clock_running = false;
            if !previous_state.rebuffering() {
                self.note_output_housekeeping_change();
            }
        }
        if state == PlaybackOutputState::Syncing || !self.restart_pending() {
            self.startup_first_frame_stall_logged = false;
        }
        if state != PlaybackOutputState::Primed {
            self.initial_delayed_audio_start_timeline_nsecs = None;
        }
        if !self.restart_pending() {
            self.initial_audio_gap_at_video_start_timeline_nsecs = None;
        }
        if state != PlaybackOutputState::Playing {
            self.defer_pending_start_audio_flush_once = false;
            self.startup_pending_audio_pressure_context_active = false;
            self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
            self.reset_audio_output_activity_watchdog();
        }
        if !state.rebuffering() {
            self.rebuffer_empty_audio_output_blocked = false;
            self.video_decode_underfill = false;
        }
        if state == PlaybackOutputState::Playing {
            self.rebuffer_far_ahead_audio_observation_count = 0;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn start_video_deadline_service(
        &mut self,
        audio_clock: Option<AudioClockHandle>,
        session_id: PlaybackSessionId,
        vo_queue: VideoOutputQueue,
        frame_presented: Arc<AtomicBool>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<(), String> {
        if self.video_deadline_service.is_some() {
            return Err("视频截止时间服务已经启动".to_string());
        }
        self.video_deadline_audio_clock_available = audio_clock.is_some();
        self.scheduled_video_queue
            .clear_and_bind_presentation_session(session_id);
        let queue = self.scheduled_video_queue.attach_deadline_service();
        self.video_deadline_service = Some(VideoDeadlineService::spawn(
            queue,
            audio_clock,
            vo_queue,
            frame_presented,
            event_tx,
        )?);
        self.refresh_video_deadline_service_active();
        Ok(())
    }

    pub(in crate::player::backend::ffmpeg) fn update_video_deadline_audio_clock(
        &mut self,
        audio_clock: Option<AudioClockHandle>,
    ) {
        self.video_deadline_audio_clock_available = audio_clock.is_some();
        if let Some(service) = &self.video_deadline_service {
            service.update_audio_clock(audio_clock);
        }
        self.refresh_video_deadline_service_active();
    }

    pub(in crate::player::backend::ffmpeg) fn set_video_deadline_audio_clock_available(
        &mut self,
        available: bool,
    ) {
        if self.video_deadline_audio_clock_available == available {
            return;
        }
        self.video_deadline_audio_clock_available = available;
        self.refresh_video_deadline_service_active();
    }

    fn refresh_video_deadline_service_active(&self) {
        self.scheduled_video_queue.set_deadline_service_active(
            self.playback_output_state == PlaybackOutputState::Playing
                && self.video_deadline_audio_clock_available,
        );
    }

    pub(in crate::player::backend::ffmpeg) fn observe_audio_output_activity(
        &mut self,
        now: Instant,
        activity: AudioOutputActivitySnapshot,
        eligible: bool,
        seek_transition_paused: bool,
    ) -> Option<AudioOutputActivityWatchdogEvent> {
        if !eligible || activity.shared_buffer_pending_nsecs == 0 {
            self.reset_audio_output_activity_watchdog();
            return None;
        }

        let Some(watchdog) = self.audio_output_activity_watchdog.as_mut() else {
            self.audio_output_activity_watchdog = Some(AudioOutputActivityWatchdog {
                stalled_since: now,
                last_played_timeline_nsecs: activity.played_timeline_nsecs,
                last_callback_count: activity.callback_count,
                last_consumed_callback_count: activity.consumed_callback_count,
                last_silenced_callback_count: activity.silenced_callback_count,
                last_underrun_count: activity.underrun_count,
                warning_emitted: false,
                seek_release_attempted: false,
                recovery_started: false,
            });
            return None;
        };

        let callback_progress = activity.consumed_callback_count
            > watchdog.last_consumed_callback_count
            || (activity.played_timeline_nsecs > watchdog.last_played_timeline_nsecs
                && activity.callback_count > watchdog.last_callback_count
                && activity.silenced_callback_count == watchdog.last_silenced_callback_count);
        let underrun_observed = activity.underrun_count > watchdog.last_underrun_count;
        watchdog.last_played_timeline_nsecs = activity.played_timeline_nsecs;
        watchdog.last_callback_count = activity.callback_count;
        watchdog.last_consumed_callback_count = activity.consumed_callback_count;
        watchdog.last_silenced_callback_count = activity.silenced_callback_count;
        watchdog.last_underrun_count = activity.underrun_count;
        if callback_progress || underrun_observed {
            watchdog.stalled_since = now;
            watchdog.warning_emitted = false;
            watchdog.seek_release_attempted = false;
            watchdog.recovery_started = false;
            self.audio_output_clock_stall_fallback_active = false;
            return None;
        }

        let stalled_for = now.saturating_duration_since(watchdog.stalled_since);
        if stalled_for < AUDIO_OUTPUT_ACTIVITY_STALL_AFTER {
            return None;
        }
        self.audio_output_clock_stall_fallback_active = true;

        if seek_transition_paused && !watchdog.seek_release_attempted {
            watchdog.seek_release_attempted = true;
            watchdog.warning_emitted = true;
            return Some(AudioOutputActivityWatchdogEvent {
                action: AudioOutputActivityWatchdogAction::ReleaseSeekTransition,
                stalled_for,
            });
        }
        if !watchdog.warning_emitted {
            watchdog.warning_emitted = true;
            return Some(AudioOutputActivityWatchdogEvent {
                action: AudioOutputActivityWatchdogAction::WarnFrozenClock,
                stalled_for,
            });
        }
        if stalled_for >= AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER && !watchdog.recovery_started {
            watchdog.recovery_started = true;
            return Some(AudioOutputActivityWatchdogEvent {
                action: AudioOutputActivityWatchdogAction::RecoverAndReanchor,
                stalled_for,
            });
        }
        None
    }

    pub(in crate::player::backend::ffmpeg) fn audio_output_clock_stall_fallback_active(
        &self,
    ) -> bool {
        self.audio_output_clock_stall_fallback_active
    }

    pub(in crate::player::backend::ffmpeg) fn reset_audio_output_activity_watchdog(&mut self) {
        self.audio_output_activity_watchdog = None;
        self.audio_output_clock_stall_fallback_active = false;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn note_video_output_rebuffer_started(
        &mut self,
        now: Instant,
    ) {
        if self.playback_output_state.rebuffering() {
            self.rebuffer_started_at.get_or_insert(now);
            self.video_clock_anchor_valid = false;
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn mark_video_clock_anchor_valid(
        &mut self,
    ) {
        self.video_clock_anchor_valid = true;
        self.output_clock_running = true;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn video_clock_anchor_valid(
        &self,
    ) -> bool {
        self.video_clock_anchor_valid
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn rebuffer_pause_elapsed(
        &self,
    ) -> Option<Duration> {
        self.playback_output_state.rebuffering().then(|| {
            self.rebuffer_started_at
                .map(|started_at| started_at.elapsed())
        })?
    }

    pub(in crate::player::backend::ffmpeg) fn finish_rebuffer_if_ready(
        &mut self,
        waterline: PlaybackResumeWaterline,
        session_id: PlaybackSessionId,
    ) -> bool {
        if !finish_video_output_rebuffer_if_ready(
            &mut self.playback_output_state,
            waterline,
            session_id,
        ) {
            return false;
        }
        self.video_output_underrun_started_at = None;
        self.rebuffer_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_far_ahead_audio_observation_count = 0;
        self.clear_video_bootstrap_after_seek("rebuffer_waterline_ready");
        true
    }

    pub(in crate::player::backend::ffmpeg) fn observe_rebuffer_far_ahead_audio_frame(
        &mut self,
        far_ahead_audio_timeline_nsecs: u64,
        current_start_position_nsecs: u64,
        audio_output_pending_nsecs: Option<u64>,
        force_immediate_realign: bool,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.playback_output_state.rebuffering() && !self.restart_pending() {
            self.rebuffer_far_ahead_audio_observation_count = 0;
            return None;
        }
        self.rebuffer_far_ahead_audio_observation_count = self
            .rebuffer_far_ahead_audio_observation_count
            .saturating_add(1);

        let (target_timeline_nsecs, anchor_timeline_nsecs, first_video_timeline_nsecs) =
            self.rebuffer_audio_realign_target(current_start_position_nsecs)?;
        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let queued_video_covers_target = self
            .scheduled_video_queue
            .buffered_until_from_nsecs(target_timeline_nsecs)
            .is_some();
        let first_video_after_anchor_gap_ms =
            (i128::from(first_video_timeline_nsecs) - i128::from(anchor_timeline_nsecs)) as f64
                / 1_000_000.0;
        let far_ahead_audio_delta_ms = (i128::from(far_ahead_audio_timeline_nsecs)
            - i128::from(target_timeline_nsecs)) as f64
            / 1_000_000.0;
        let pending_audio_covers_target = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .is_some();
        let pending_audio_near_resume_target = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .map(|buffered_until_nsecs| {
                let pending_forward_nsecs =
                    buffered_until_nsecs.saturating_sub(target_timeline_nsecs);
                let protected_target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
                pending_forward_nsecs >= protected_target_nsecs
            })
            .unwrap_or(false);
        let audio_output_empty = audio_output_pending_nsecs == Some(0);
        let audio_output_continuous = audio_output_pending_nsecs.is_some_and(|pending| pending > 0);
        let pending_audio_continuous =
            pending_audio_covers_target || pending_audio_near_resume_target;
        let recent_coordinator_stall = self
            .scheduled_video_queue
            .recent_coordinator_stall(Instant::now());
        if !force_immediate_realign
            && recent_coordinator_stall.is_some()
            && (audio_output_continuous || pending_audio_continuous)
        {
            self.rebuffer_far_ahead_audio_observation_count = 0;
            tracing::debug!(
                session_id = ?session_id,
                reason,
                far_ahead_audio_timeline_nsecs,
                target_timeline_nsecs,
                audio_output_continuous,
                pending_audio_continuous,
                recent_coordinator_stall_ms = ?recent_coordinator_stall
                    .map(|stall| stall.elapsed.as_secs_f64() * 1000.0),
                recent_coordinator_stall_age_ms = ?recent_coordinator_stall
                    .map(|stall| stall.age.as_secs_f64() * 1000.0),
                "suppressed FFmpeg audio realign after coordinator stall with continuous audio"
            );
            return None;
        }
        let progress_nsecs = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .map(|until| until.saturating_sub(target_timeline_nsecs))
            .unwrap_or_default()
            .max(audio_output_pending_nsecs.unwrap_or_default());
        let gap_watchdog_decision = observe_audio_reader_gap_watchdog(
            &mut self.audio_reader_gap_watchdog,
            AudioReaderGapWatchdogObservation {
                target_timeline_nsecs,
                progress_nsecs,
                has_resume_coverage: pending_audio_continuous || audio_output_continuous,
                input_can_fill_gap: false,
                observed_pts_nsecs: Some(far_ahead_audio_timeline_nsecs),
                force_immediate_realign,
                now: Instant::now(),
            },
        );
        if gap_watchdog_decision != AudioReaderGapWatchdogDecision::Request {
            tracing::trace!(
                session_id = ?session_id,
                target_timeline_nsecs,
                far_ahead_audio_timeline_nsecs,
                gap_watchdog_decision = ?gap_watchdog_decision,
                pending_audio_continuous,
                audio_output_continuous,
                progress_ms = progress_nsecs as f64 / 1_000_000.0,
                "deferred FFmpeg decoded-audio realign until continuity gap watchdog expires"
            );
            return None;
        }
        let realign_needed = !pending_audio_near_resume_target
            && (force_immediate_realign || audio_output_empty || !pending_audio_covers_target);
        let bypass_observation_threshold = force_immediate_realign
            || (self.rebuffer_empty_audio_output_blocked
                && self.playback_output_state.rebuffering());
        if (!bypass_observation_threshold
            && self.rebuffer_far_ahead_audio_observation_count
                < REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_OBSERVATIONS)
            || !realign_needed
        {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                far_ahead_audio_timeline_nsecs,
                target_timeline_nsecs,
                anchor_timeline_nsecs,
                first_video_timeline_nsecs,
                queued_video_frames = self.scheduled_video_queue.len(),
                queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                queued_video_range_nsecs = ?queued_video_range_nsecs,
                queued_video_covers_target,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = self.rebuffer_far_ahead_audio_observation_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                pending_audio_near_resume_target,
                realign_needed,
                bypass_observation_threshold,
                force_immediate_realign,
                "observed FFmpeg rebuffer audio far ahead of video target"
            );
            return None;
        }

        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs,
            far_ahead_observation_count: self.rebuffer_far_ahead_audio_observation_count,
            reason,
        };
        if self.rebuffer_audio_realign_request.is_none() {
            self.rebuffer_audio_realign_request = Some(request);
            tracing::debug!(
                session_id = ?session_id,
                reason,
                target_timeline_nsecs,
                anchor_timeline_nsecs,
                first_video_timeline_nsecs,
                far_ahead_audio_timeline_nsecs,
                queued_video_frames = self.scheduled_video_queue.len(),
                queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                queued_video_range_nsecs = ?queued_video_range_nsecs,
                queued_video_covers_target,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = request.far_ahead_observation_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                pending_audio_near_resume_target,
                bypass_observation_threshold,
                force_immediate_realign,
                "requested FFmpeg rebuffer audio realign to video target"
            );
        }
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn request_output_wait_audio_reader_head_realign_if_needed(
        &mut self,
        reader_head_start_nsecs: u64,
        audio_waterline: AudioResumeWaterline,
        current_start_position_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.waiting_for_output_resume() || self.rebuffer_audio_realign_request.is_some() {
            return None;
        }
        let (target_timeline_nsecs, anchor_timeline_nsecs, first_video_timeline_nsecs) =
            self.rebuffer_audio_realign_target(current_start_position_nsecs)?;
        let pending_audio_buffered_until_nsecs = self
            .pending_start_audio
            .buffered_until_from(audio_waterline.resume_timeline_nsecs);
        let pending_audio_covers_resume = pending_audio_buffered_until_nsecs.is_some();
        let protected_audio_target_nsecs = audio_waterline
            .target_nsecs
            .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        let accepted_start_within_tolerance = audio_waterline
            .audio_accepted_start_timeline_nsecs
            .is_some()
            && audio_waterline
                .audio_accepted_start_gap_nsecs
                .is_some_and(|gap| gap <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE));
        let pending_audio_near_target = accepted_start_within_tolerance
            && audio_waterline
                .accepted_contiguous_coverage_nsecs
                .is_some_and(|coverage| coverage >= protected_audio_target_nsecs);
        if pending_audio_near_target {
            self.audio_reader_gap_watchdog = None;
            return None;
        }
        let audio_output_empty = audio_waterline.audio_output_pending_nsecs == Some(0);
        let audio_output_continuous = audio_waterline
            .audio_output_pending_nsecs
            .is_some_and(|pending| pending > 0);
        let pending_audio_continuous = pending_audio_covers_resume || pending_audio_near_target;
        let recent_coordinator_stall = self
            .scheduled_video_queue
            .recent_coordinator_stall(Instant::now());
        if recent_coordinator_stall.is_some()
            && (audio_output_continuous || pending_audio_continuous)
        {
            tracing::debug!(
                session_id = ?session_id,
                reader_head_start_nsecs,
                resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
                audio_output_continuous,
                pending_audio_continuous,
                recent_coordinator_stall_ms = ?recent_coordinator_stall
                    .map(|stall| stall.elapsed.as_secs_f64() * 1000.0),
                recent_coordinator_stall_age_ms = ?recent_coordinator_stall
                    .map(|stall| stall.age.as_secs_f64() * 1000.0),
                "suppressed FFmpeg audio reader realign after coordinator stall with continuous audio"
            );
            return None;
        }
        let pending_resume_coverage = pending_audio_covers_resume
            || (accepted_start_within_tolerance
                && audio_waterline
                    .accepted_contiguous_coverage_nsecs
                    .is_some_and(|coverage| coverage > 0));
        let decoded_resume_coverage = audio_waterline
            .decoded_audio_forward_nsecs
            .is_some_and(|coverage| coverage > 0);
        let audio_output_resume_coverage = audio_waterline
            .audio_output_buffered_until_nsecs
            .is_some_and(|until| until > audio_waterline.resume_timeline_nsecs);
        let has_resume_coverage =
            pending_resume_coverage || decoded_resume_coverage || audio_output_resume_coverage;
        let input_can_fill_gap = audio_waterline.audio_decode_in_flight_packets > 0;
        let progress_nsecs = audio_waterline
            .accepted_contiguous_coverage_nsecs
            .unwrap_or_default()
            .max(
                audio_waterline
                    .decoded_audio_forward_nsecs
                    .unwrap_or_default(),
            )
            .max(
                audio_waterline
                    .audio_output_buffered_until_nsecs
                    .map(|until| until.saturating_sub(audio_waterline.resume_timeline_nsecs))
                    .unwrap_or_default(),
            );
        let gap_watchdog_decision = observe_audio_reader_gap_watchdog(
            &mut self.audio_reader_gap_watchdog,
            AudioReaderGapWatchdogObservation {
                target_timeline_nsecs,
                progress_nsecs,
                has_resume_coverage,
                input_can_fill_gap,
                observed_pts_nsecs: Some(reader_head_start_nsecs),
                force_immediate_realign: false,
                now: Instant::now(),
            },
        );
        if gap_watchdog_decision != AudioReaderGapWatchdogDecision::Request {
            tracing::trace!(
                session_id = ?session_id,
                target_timeline_nsecs,
                reader_head_start_nsecs,
                gap_watchdog_decision = ?gap_watchdog_decision,
                has_resume_coverage,
                pending_resume_coverage,
                decoded_resume_coverage,
                audio_output_resume_coverage,
                input_can_fill_gap,
                progress_ms = progress_nsecs as f64 / 1_000_000.0,
                "deferred FFmpeg audio reader realign until a real continuity gap stalls"
            );
            return None;
        }
        let blocked_rebuffer_recovery = self.rebuffer_empty_audio_output_blocked
            && self.playback_output_state.rebuffering()
            && audio_waterline.below_target()
            && !pending_audio_near_target
            && (audio_output_empty || !pending_audio_covers_resume);
        let pending_contiguous_until_nsecs = self
            .pending_start_audio
            .contiguous_range_nsecs()
            .filter(|(start_nsecs, _)| {
                *start_nsecs
                    <= audio_waterline
                        .resume_timeline_nsecs
                        .saturating_add(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN))
            })
            .map(|(_, end_nsecs)| end_nsecs);
        let decoded_contiguous_until_nsecs = audio_waterline
            .decoded_audio_forward_nsecs
            .map(|forward_nsecs| {
                audio_waterline
                    .resume_timeline_nsecs
                    .saturating_add(forward_nsecs)
            })
            .or(pending_contiguous_until_nsecs)
            .or(audio_waterline.audio_output_buffered_until_nsecs);
        let in_flight_allowance_nsecs = duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            .saturating_mul(
                u64::try_from(audio_waterline.audio_decode_in_flight_packets).unwrap_or(u64::MAX),
            );
        let proactive_reader_limit_nsecs = decoded_contiguous_until_nsecs.map(|until_nsecs| {
            until_nsecs
                .saturating_add(audio_waterline.audio_decode_queued_nsecs)
                .saturating_add(in_flight_allowance_nsecs)
                .saturating_add(duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
        });
        let blocked_rebuffer_reader_limit_nsecs = audio_waterline
            .resume_timeline_nsecs
            .saturating_add(
                audio_waterline
                    .target_nsecs
                    .max(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            )
            .saturating_add(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        let (reason, reader_limit_nsecs) = if blocked_rebuffer_recovery {
            (
                "rebuffer_audio_reader_far_ahead",
                blocked_rebuffer_reader_limit_nsecs,
            )
        } else {
            (
                "output_wait_audio_reader_continuity_gap",
                proactive_reader_limit_nsecs?,
            )
        };
        if reader_head_start_nsecs <= reader_limit_nsecs {
            return None;
        }

        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let queued_video_covers_target = self
            .scheduled_video_queue
            .buffered_until_from_nsecs(target_timeline_nsecs)
            .is_some();
        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs: reader_head_start_nsecs,
            far_ahead_observation_count: 0,
            reason,
        };
        self.rebuffer_audio_realign_request = Some(request);
        tracing::debug!(
            session_id = ?session_id,
            reason = request.reason,
            reader_head_start_nsecs,
            resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
            reader_limit_nsecs,
            proactive_reader_limit_nsecs,
            blocked_rebuffer_reader_limit_nsecs,
            blocked_rebuffer_recovery,
            current_start_position_nsecs,
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            queued_video_frames = self.scheduled_video_queue.len(),
            queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
            queued_video_range_nsecs = ?queued_video_range_nsecs,
            queued_video_covers_target,
            pending_audio_start_nsecs = ?audio_waterline.pending_audio_start_nsecs,
            audio_accepted_start = ?audio_waterline.audio_accepted_start_timeline_nsecs,
            start_gap_ms = ?audio_waterline
                .audio_accepted_start_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            contiguous_coverage_ms = ?audio_waterline
                .accepted_contiguous_coverage_nsecs
                .map(|coverage| coverage as f64 / 1_000_000.0),
            pending_audio_covers_resume,
            accepted_start_within_tolerance,
            pending_audio_near_target,
            protected_audio_target_ms = protected_audio_target_nsecs as f64 / 1_000_000.0,
            pending_contiguous_until_nsecs,
            decoded_contiguous_until_nsecs,
            pending_audio_forward_ms = ?audio_waterline
                .pending_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            audio_decode_queued_ms = audio_waterline.audio_decode_queued_nsecs as f64
                / 1_000_000.0,
            audio_decode_in_flight_packets = audio_waterline.audio_decode_in_flight_packets,
            in_flight_allowance_ms = in_flight_allowance_nsecs as f64 / 1_000_000.0,
            audio_output_pending_ms = ?audio_waterline
                .audio_output_pending_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?audio_waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "requested FFmpeg output-wait audio reader realign before resume"
        );
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn record_audio_continuity_rejection(
        &mut self,
        rejected_pts_nsecs: u64,
        reference_nsecs: u64,
        request: Option<RebufferAudioRealignRequest>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let now = Instant::now();
        let gap_nsecs = rejected_pts_nsecs.saturating_sub(reference_nsecs);
        if self
            .audio_continuity_rejection_summary
            .as_ref()
            .is_some_and(|summary| summary.last_rejected_pts_nsecs == rejected_pts_nsecs)
        {
            return;
        }
        let summary = self.audio_continuity_rejection_summary.get_or_insert(
            AudioContinuityRejectionSummary {
                first_rejected_pts_nsecs: rejected_pts_nsecs,
                last_rejected_pts_nsecs: rejected_pts_nsecs,
                rejected_count: 0,
                largest_gap_nsecs: 0,
                last_log_at: now
                    .checked_sub(AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL)
                    .unwrap_or(now),
            },
        );
        summary.last_rejected_pts_nsecs = rejected_pts_nsecs;
        summary.rejected_count = summary.rejected_count.saturating_add(1);
        summary.largest_gap_nsecs = summary.largest_gap_nsecs.max(gap_nsecs);
        if now.saturating_duration_since(summary.last_log_at)
            < AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL
        {
            return;
        }
        summary.last_log_at = now;
        tracing::warn!(
            session_id = ?session_id,
            reason,
            first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
            last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
            rejected_count = summary.rejected_count,
            largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
            rebuffer_audio_realign_target_nsecs =
                ?request.map(|request| request.target_timeline_nsecs),
            "rate-limited FFmpeg audio continuity rejection summary"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer_far_ahead_audio_observation(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.rebuffer_far_ahead_audio_observation_count = 0;
        if let Some(summary) = self.audio_continuity_rejection_summary.take() {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
                last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
                rejected_count = summary.rejected_count,
                largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
                "completed FFmpeg audio continuity rejection series"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffer_audio_realign_request_pending(
        &self,
    ) -> bool {
        self.rebuffer_audio_realign_request.is_some()
    }

    pub(in crate::player::backend::ffmpeg) fn take_rebuffer_audio_realign_request(
        &mut self,
    ) -> Option<RebufferAudioRealignRequest> {
        let request = self.rebuffer_audio_realign_request.take();
        if request.is_some() {
            self.rebuffer_far_ahead_audio_observation_count = 0;
        }
        request
    }

    pub(in crate::player::backend::ffmpeg) fn defer_audio_reader_gap_watchdog_after_input_pending(
        &mut self,
        target_timeline_nsecs: u64,
    ) {
        if let Some(watchdog) = self.audio_reader_gap_watchdog.as_mut()
            && watchdog.target_timeline_nsecs == target_timeline_nsecs
        {
            watchdog.last_progress_at = Instant::now();
            watchdog.request_issued = false;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn prepare_audio_after_rebuffer_realign(
        &mut self,
        target_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        debug_assert!(
            self.pending_start_audio.is_empty(),
            "pending audio must be retained transactionally before realign"
        );
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_audio_realign_request = None;
        self.audio_reader_gap_watchdog = None;
        self.rebuffer_far_ahead_audio_observation_count = 0;
        if let Some(summary) = self.audio_continuity_rejection_summary.take() {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
                last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
                rejected_count = summary.rejected_count,
                largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
                "completed FFmpeg audio continuity rejection series after realign"
            );
        }
        self.set_audio_sync_drop_before_timeline_nsecs(target_timeline_nsecs, session_id, reason);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            "prepared FFmpeg audio output scheduler for transactional rebuffer realign"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_far_ahead_reference_timeline_nsecs(
        &self,
        current_start_position_nsecs: u64,
        audio_snapshot: Option<AudioOutputSnapshot>,
    ) -> u64 {
        let actual_audio_timeline_nsecs = audio_snapshot
            .map(|snapshot| {
                if snapshot.total_pending_nsecs > 0 {
                    snapshot
                        .played_timeline_nsecs
                        .max(snapshot.buffered_until_timeline_nsecs)
                } else {
                    snapshot.played_timeline_nsecs
                }
            })
            .unwrap_or(current_start_position_nsecs);
        let resume_reference_nsecs = if self.playback_output_state.rebuffering() {
            self.rebuffer_audio_realign_target(current_start_position_nsecs)
                .map(|(target_timeline_nsecs, _, _)| target_timeline_nsecs)
                .or_else(|| {
                    self.video_output_rebuffer_anchor
                        .map(|anchor| anchor.timeline_nsecs)
                })
        } else if self.restart_pending() {
            self.scheduled_video_queue
                .range_nsecs()
                .map(|(first_video_timeline_nsecs, _)| first_video_timeline_nsecs)
        } else {
            None
        };
        resume_reference_nsecs
            .unwrap_or(current_start_position_nsecs)
            .max(current_start_position_nsecs)
            .max(actual_audio_timeline_nsecs)
    }

    fn rebuffer_audio_realign_target(
        &self,
        current_start_position_nsecs: u64,
    ) -> Option<(u64, u64, u64)> {
        let (first_video_timeline_nsecs, _) = self.scheduled_video_queue.range_nsecs()?;
        let anchor_timeline_nsecs = self
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .unwrap_or(current_start_position_nsecs);
        let target_timeline_nsecs = if first_video_timeline_nsecs <= anchor_timeline_nsecs
            && self
                .scheduled_video_queue
                .buffered_until_from_nsecs(anchor_timeline_nsecs)
                .is_some()
        {
            anchor_timeline_nsecs
        } else {
            first_video_timeline_nsecs
        };
        Some((
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
        ))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn begin_audio_gap_recovery(
        &mut self,
        target_timeline_nsecs: u64,
        now: Instant,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.audio_gap_recovery_until = now.checked_add(AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR);
        self.audio_gap_recovery_target_nsecs = Some(target_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            suppress_rebuffer_ms =
                AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR.as_secs_f64() * 1000.0,
            "entered FFmpeg audio gap recovery after video-clock resume"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn resume_after_confirmed_audio_media_gap(
        &mut self,
        target_timeline_nsecs: u64,
        far_ahead_audio_timeline_nsecs: u64,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> u64 {
        let first_video_timeline_nsecs = self
            .scheduled_video_queue
            .range_nsecs()
            .map(|(start_nsecs, _)| start_nsecs)
            .unwrap_or(target_timeline_nsecs);
        let resume_timeline_nsecs = first_video_timeline_nsecs.max(target_timeline_nsecs);
        control.set_output_rebuffer_paused(false);
        self.video_output_underrun_started_at = None;
        self.rebuffer_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_audio_realign_request = None;
        self.audio_reader_gap_watchdog = None;
        self.set_state(PlaybackOutputState::Playing);
        self.mark_video_clock_anchor_valid();
        self.begin_audio_gap_recovery(
            resume_timeline_nsecs,
            Instant::now(),
            session_id,
            "bounded_audio_realign_confirmed_media_gap",
        );
        self.clear_rebuffer_far_ahead_audio_observation(
            session_id,
            "bounded_audio_realign_confirmed_media_gap",
        );
        tracing::warn!(
            session_id = ?session_id,
            target_timeline_nsecs,
            first_video_timeline_nsecs,
            resume_timeline_nsecs,
            far_ahead_audio_timeline_nsecs,
            confirmed_audio_gap_ms = far_ahead_audio_timeline_nsecs
                .saturating_sub(target_timeline_nsecs) as f64
                / 1_000_000.0,
            video_clock_anchor_valid = self.video_clock_anchor_valid(),
            "resumed FFmpeg video clock after bounded audio realign confirmed a media gap"
        );
        resume_timeline_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn audio_gap_recovery_active(&self) -> bool {
        self.audio_gap_recovery_until.is_some()
    }

    pub(in crate::player::backend::ffmpeg) fn clear_audio_gap_recovery_if_audio_ready(
        &mut self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        played_until_nsecs: Option<u64>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> bool {
        let Some(target_timeline_nsecs) = self.audio_gap_recovery_target_nsecs else {
            return false;
        };
        let audio_attach_timeline_nsecs = played_until_nsecs
            .unwrap_or(target_timeline_nsecs)
            .max(target_timeline_nsecs);
        let audio_output_covers = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)
                && snapshot.buffered_until_timeline_nsecs > audio_attach_timeline_nsecs
        });
        if !audio_output_covers {
            return false;
        }
        self.audio_gap_recovery_until = None;
        self.audio_gap_recovery_target_nsecs = None;
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            audio_attach_timeline_nsecs,
            audio_output_covers,
            "cleared FFmpeg audio gap recovery after audio reattached"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn set_rebuffer_empty_audio_output_blocked(
        &mut self,
        blocked: bool,
    ) {
        self.rebuffer_empty_audio_output_blocked =
            blocked && self.playback_output_state.rebuffering();
    }

    pub(in crate::player::backend::ffmpeg) fn set_audio_sync_drop_before_timeline_nsecs(
        &mut self,
        drop_before_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        if self
            .audio_sync_drop_before_timeline_nsecs
            .is_some_and(|current| current >= drop_before_timeline_nsecs)
        {
            return;
        }
        self.finish_audio_sync_drop_log_summary(session_id, "drop_before_advanced");
        self.audio_sync_drop_before_timeline_nsecs = Some(drop_before_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs,
            "set FFmpeg audio sync drop-before timeline"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_sync_drop_before_timeline_nsecs(
        &self,
    ) -> Option<u64> {
        self.audio_sync_drop_before_timeline_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn record_audio_sync_drop_before_frame(
        &mut self,
        raw_timestamp: i64,
        timeline_nsecs: u64,
        buffered_until_nsecs: u64,
        output_snapshot: PlaybackOutputSnapshot,
        session_id: PlaybackSessionId,
    ) {
        let Some(drop_before_timeline_nsecs) = self.audio_sync_drop_before_timeline_nsecs else {
            return;
        };
        if self
            .audio_sync_drop_log_summary
            .is_some_and(|summary| summary.drop_before_timeline_nsecs != drop_before_timeline_nsecs)
        {
            self.finish_audio_sync_drop_log_summary(session_id, "drop_before_changed");
        }

        let now = Instant::now();
        let summary = self
            .audio_sync_drop_log_summary
            .get_or_insert(AudioSyncDropLogSummary {
                drop_before_timeline_nsecs,
                started_at: now,
                last_log_at: now,
                total_dropped_frames: 0,
                suppressed_since_last_log: 0,
                first_raw_timestamp: raw_timestamp,
                last_raw_timestamp: raw_timestamp,
                first_timeline_nsecs: timeline_nsecs,
                last_timeline_nsecs: timeline_nsecs,
                last_buffered_until_nsecs: buffered_until_nsecs,
            });
        summary.total_dropped_frames = summary.total_dropped_frames.saturating_add(1);
        summary.last_raw_timestamp = raw_timestamp;
        summary.last_timeline_nsecs = timeline_nsecs;
        summary.last_buffered_until_nsecs = buffered_until_nsecs;
        let first = summary.total_dropped_frames == 1;
        let periodic_summary = !first
            && now.saturating_duration_since(summary.last_log_at)
                >= AUDIO_SYNC_DROP_LOG_SUMMARY_INTERVAL;
        if !first && !periodic_summary {
            summary.suppressed_since_last_log = summary.suppressed_since_last_log.saturating_add(1);
            return;
        }
        let suppressed_dropped_frames = std::mem::take(&mut summary.suppressed_since_last_log);
        summary.last_log_at = now;
        let summary = *summary;
        tracing::debug!(
            session_id = ?session_id,
            raw_timestamp,
            timeline_nsecs,
            buffered_until_nsecs,
            drop_before_timeline_nsecs,
            total_dropped_frames = summary.total_dropped_frames,
            suppressed_dropped_frames,
            log_kind = if first { "first" } else { "periodic_summary" },
            elapsed_ms = now
                .saturating_duration_since(summary.started_at)
                .as_secs_f64()
                * 1_000.0,
            output_state = ?output_snapshot.state,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            rebuffering = output_snapshot.rebuffering,
            "dropping FFmpeg audio frames before rebuffer audio sync drop-before"
        );
    }

    fn finish_audio_sync_drop_log_summary(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let Some(summary) = self.audio_sync_drop_log_summary.take() else {
            return;
        };
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs = summary.drop_before_timeline_nsecs,
            total_dropped_frames = summary.total_dropped_frames,
            suppressed_dropped_frames = summary.suppressed_since_last_log,
            first_raw_timestamp = summary.first_raw_timestamp,
            last_raw_timestamp = summary.last_raw_timestamp,
            first_timeline_nsecs = summary.first_timeline_nsecs,
            last_timeline_nsecs = summary.last_timeline_nsecs,
            last_buffered_until_nsecs = summary.last_buffered_until_nsecs,
            elapsed_ms = summary.started_at.elapsed().as_secs_f64() * 1_000.0,
            "finished aggregated FFmpeg audio sync drop-before sequence"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn clear_audio_sync_drop_before_if_covered(
        &mut self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> bool {
        let Some(drop_before_timeline_nsecs) = self.audio_sync_drop_before_timeline_nsecs else {
            return false;
        };
        let audio_output_covers_drop_before = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs > 0
                && snapshot.buffered_until_timeline_nsecs > drop_before_timeline_nsecs
        });
        if !audio_output_covers_drop_before {
            return false;
        }
        self.audio_sync_drop_before_timeline_nsecs = None;
        self.finish_audio_sync_drop_log_summary(session_id, reason);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs,
            audio_output_covers_drop_before,
            "cleared FFmpeg audio sync drop-before timeline after coverage"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn observe_audio_output_underrun_for_rebuffer(
        &mut self,
        now: Instant,
        session_id: PlaybackSessionId,
    ) {
        let window_active = self
            .recent_audio_output_underrun_window_started_at
            .is_some_and(|started_at| {
                now.saturating_duration_since(started_at) <= AUDIO_REBUFFER_LOOP_DETECTION_WINDOW
            });
        if !window_active {
            self.recent_audio_output_underrun_window_started_at = Some(now);
            self.recent_audio_output_underruns = 1;
            return;
        }

        self.recent_audio_output_underruns = self.recent_audio_output_underruns.saturating_add(1);
        if self.audio_rebuffer_loop_active() {
            tracing::debug!(
                session_id = ?session_id,
                recent_audio_output_underruns = self.recent_audio_output_underruns,
                loop_window_ms = AUDIO_REBUFFER_LOOP_DETECTION_WINDOW.as_secs_f64() * 1000.0,
                "detected repeated FFmpeg audio output underruns; using loop recovery waterline"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_rebuffer_loop_active(
        &self,
    ) -> bool {
        self.recent_audio_output_underruns >= 2
            && self
                .recent_audio_output_underrun_window_started_at
                .is_some_and(|started_at| {
                    started_at.elapsed() <= AUDIO_REBUFFER_LOOP_DETECTION_WINDOW
                })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_rebuffer_prefill_target_nsecs(
        &self,
        queued_video_contiguous_forward_nsecs: Option<u64>,
    ) -> u64 {
        let base_target = if self.audio_rebuffer_loop_active() {
            AUDIO_REBUFFER_PREFILL_LOOP_TARGET
        } else {
            AUDIO_REBUFFER_PREFILL_TARGET
        };
        let mut target_nsecs = duration_nsecs(base_target.min(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION));
        if let Some(video_forward_nsecs) = queued_video_contiguous_forward_nsecs {
            target_nsecs = target_nsecs.min(video_forward_nsecs);
        }
        target_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn begin_video_bootstrap_after_seek(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.video_bootstrap_after_seek = true;
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        self.rebuffer_empty_audio_output_blocked = false;
        self.set_state(PlaybackOutputState::Syncing);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            output_state = ?self.playback_output_state,
            queued_video_frames = self.scheduled_video_queue.len(),
            queued_video_ms = self.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
            "started post-seek video bootstrap for FFmpeg output"
        );
    }

    fn clear_video_bootstrap_after_seek(&mut self, reason: &'static str) {
        if !self.video_bootstrap_after_seek {
            return;
        }
        self.video_bootstrap_after_seek = false;
        tracing::debug!(
            reason,
            output_state = ?self.playback_output_state,
            queued_video_frames = self.scheduled_video_queue.len(),
            queued_video_ms = self.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
            "cleared post-seek video bootstrap for FFmpeg output"
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn maybe_enter_video_output_rebuffer(
        &mut self,
        now: Instant,
        video_output_underflowing: bool,
        queued_video_forward_nsecs: Option<u64>,
        output_underrun: bool,
        demux_cache_insufficient: bool,
        demux_min_forward_nsecs: Option<u64>,
        render_backlogged: bool,
        vo_queued_frames: usize,
        has_audio_output: bool,
        pending_audio_recoverable: bool,
        control: &FfmpegControl,
        audio_output: Option<&AudioOutput>,
        audio_output_pending_nsecs: Option<u64>,
        session_id: PlaybackSessionId,
        decoded_video_forward_nsecs: Option<u64>,
    ) -> bool {
        if self.audio_gap_recovery_suppresses_rebuffer(AudioGapRecoveryRebufferSuppressionInput {
            now,
            queued_video_forward_nsecs,
            audio_output_pending_nsecs,
            demux_min_forward_nsecs,
            render_backlogged,
            vo_queued_frames,
            session_id,
        }) {
            self.video_output_underrun_started_at = None;
            return false;
        }
        let classification = video_output_underflow_classification(
            self.playback_output_state,
            self.video_bootstrap_after_seek,
            demux_cache_insufficient,
            demux_min_forward_nsecs,
        );
        let startup_or_restart = self.restart_pending() || self.video_bootstrap_after_seek;
        if classification == VideoOutputUnderflowClassification::StartupDecodeStabilizing {
            self.video_output_underrun_started_at = None;
            tracing::debug!(
                session_id = ?session_id,
                classification = classification.as_str(),
                queued_video_ms = self.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
                demux_forward_ms = ?demux_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_video_forward_ms = ?decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                startup_or_restart,
                restart_complete = !startup_or_restart,
                "video_output_underflow_classified"
            );
            return false;
        }
        if !video_output_rebuffer_should_enter(
            &mut self.video_output_underrun_started_at,
            now,
            video_output_underflowing,
            queued_video_forward_nsecs,
            output_underrun,
            demux_cache_insufficient,
            demux_min_forward_nsecs,
            render_backlogged,
            has_audio_output,
            pending_audio_recoverable,
            self.playback_output_state,
        ) {
            return false;
        }
        let underrun_elapsed = self
            .video_output_underrun_started_at
            .map(|started_at| now.saturating_duration_since(started_at))
            .unwrap_or_default();
        self.video_decode_underfill = classification.decode_underfill();
        self.video_output_rebuffer_anchor = enter_video_output_rebuffer(
            &mut self.playback_output_state,
            control,
            audio_output,
            &self.scheduled_video_queue,
            session_id,
            underrun_elapsed,
            decoded_video_forward_nsecs,
            demux_min_forward_nsecs,
            classification,
            startup_or_restart,
        );
        self.note_video_output_rebuffer_started(now);
        // Reclaim Vulkan frame-pool budget held by decoded frames that end at/before
        // the rebuffer anchor: the audio clock paused at the anchor and never runs
        // backwards, so those frames can never be presented, yet they count against
        // the frame-pressure budget without contributing to the resume waterline
        // (which measures forward from the anchor). Skip when we will reset the audio
        // clock back to the decoded-video front, since those frames are then kept.
        if let Some(anchor) = self.video_output_rebuffer_anchor
            && !anchor.reset_to_video_when_decoded_queue_misses_anchor
        {
            let dropped = self
                .scheduled_video_queue
                .discard_before(anchor.timeline_nsecs);
            if dropped > 0 {
                tracing::debug!(
                    session_id = ?session_id,
                    dropped_pre_anchor_frames = dropped,
                    anchor_timeline_nsecs = anchor.timeline_nsecs,
                    remaining_queued_frames = self.scheduled_video_queue.len(),
                    "dropped pre-anchor decoded video frames to reclaim frame-pool budget on rebuffer entry"
                );
            }
        }
        true
    }

    fn audio_gap_recovery_suppresses_rebuffer(
        &mut self,
        input: AudioGapRecoveryRebufferSuppressionInput,
    ) -> bool {
        let Some(recovery_until) = self.audio_gap_recovery_until else {
            return false;
        };
        if input.now >= recovery_until {
            tracing::debug!(
                session_id = ?input.session_id,
                recovery_target_timeline_nsecs = ?self.audio_gap_recovery_target_nsecs,
                "expired FFmpeg audio gap recovery rebuffer suppression"
            );
            self.audio_gap_recovery_until = None;
            self.audio_gap_recovery_target_nsecs = None;
            return false;
        }
        if input.audio_output_pending_nsecs != Some(0) {
            return false;
        }
        let video_ready = input.queued_video_forward_nsecs.is_some_and(|duration| {
            duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
        });
        let demux_ready = input.demux_min_forward_nsecs.is_none_or(|duration| {
            duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
        });
        if !video_ready || !demux_ready {
            return false;
        }
        if self.scheduled_video_queue.limit_reached(false)
            && input.vo_queued_frames == 0
            && !input.render_backlogged
        {
            tracing::debug!(
                session_id = ?input.session_id,
                recovery_target_timeline_nsecs = ?self.audio_gap_recovery_target_nsecs,
                queued_video_frames = self.scheduled_video_queue.len(),
                queued_video_ms = self.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
                vo_queued_frames = input.vo_queued_frames,
                render_backlogged = input.render_backlogged,
                audio_output_pending_ms =
                    ?input.audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                "allowing FFmpeg output recovery to drain video clock because audio gap recovery has no audio clock"
            );
            return false;
        }
        tracing::debug!(
            session_id = ?input.session_id,
            recovery_target_timeline_nsecs = ?self.audio_gap_recovery_target_nsecs,
            recovery_remaining_ms =
                recovery_until.saturating_duration_since(input.now).as_secs_f64() * 1000.0,
            queued_video_forward_ms =
                ?input.queued_video_forward_nsecs.map(|duration| duration as f64 / 1_000_000.0),
            demux_min_forward_ms =
                ?input.demux_min_forward_nsecs.map(|duration| duration as f64 / 1_000_000.0),
            audio_output_pending_ms =
                ?input.audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
            "suppressed FFmpeg rebuffer while waiting for delayed audio start"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn mark_first_frame_queued(&mut self) {
        self.note_output_housekeeping_change();
        if self.restart_pending() {
            self.first_frame_needed = false;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn mark_first_frame_presented(
        &mut self,
    ) -> Option<Duration> {
        if !self.restart_pending() {
            return None;
        }
        let first_presentation = !self.first_frame_presented;
        self.first_frame_needed = false;
        self.first_frame_presented = true;
        if let Some(transaction) = self.initial_av_start_transaction.as_mut() {
            transaction.first_frame_presented = true;
        }
        first_presentation.then(|| self.generation_reset_started_at.elapsed())
    }

    pub(in crate::player::backend::ffmpeg) fn mark_first_frame_presentation_failed(&mut self) {
        if !self.restart_pending() || self.first_frame_presented {
            return;
        }
        self.first_frame_needed = self.scheduled_video_queue.is_empty();
        self.note_output_housekeeping_change();
    }

    pub(in crate::player::backend::ffmpeg) fn initial_start_phase(&self) -> &'static str {
        if self.initial_av_start_transaction.is_some() {
            "primed_waiting_audio"
        } else if self.restart_pending() && self.first_frame_needed {
            "waiting_first_frame"
        } else if self.restart_pending() {
            "buffering_startup"
        } else if self.playback_output_state == PlaybackOutputState::Playing {
            "playing"
        } else if self.playback_output_state.rebuffering() {
            "rebuffering"
        } else {
            "idle"
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_av_start_transaction(
        &self,
    ) -> Option<InitialAvStartTransaction> {
        self.initial_av_start_transaction
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn begin_initial_av_start_transaction(
        &mut self,
        video_anchor_nsecs: u64,
        audio_start_target_nsecs: u64,
        now: Instant,
    ) -> InitialAvStartTransaction {
        self.begin_initial_av_start_transaction_for_generations(
            video_anchor_nsecs,
            audio_start_target_nsecs,
            0,
            now,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn begin_initial_av_start_transaction_for_generations(
        &mut self,
        video_anchor_nsecs: u64,
        audio_start_target_nsecs: u64,
        seek_generation: u64,
        now: Instant,
    ) -> InitialAvStartTransaction {
        if let Some(transaction) = self.initial_av_start_transaction {
            return transaction;
        }
        let audio_start_delay =
            Duration::from_nanos(audio_start_target_nsecs.saturating_sub(video_anchor_nsecs));
        let started_at = self.initial_av_pair_started_at.unwrap_or(now);
        let transaction_id = self.next_initial_av_start_transaction_id.max(1);
        let discontinuity_epoch = self.discontinuity_epoch;
        self.next_initial_av_start_transaction_id = transaction_id.saturating_add(1).max(1);
        let audio_start_due_at = now + audio_start_delay;
        let committed_bounded_delayed_audio_start_nsecs = self
            .candidate_bounded_delayed_audio_start_for_retention_plan(PendingAudioRetentionPlan {
                anchor_timeline_nsecs: audio_start_target_nsecs,
                source: PendingAudioRetentionAnchorSource::InitialTransaction,
            });
        let transaction = InitialAvStartTransaction {
            transaction_id,
            discontinuity_epoch,
            seek_generation,
            video_anchor_nsecs,
            audio_start_target_nsecs,
            started_at,
            audio_start_due_at,
            next_audio_start_retry_at: audio_start_due_at,
            audio_retry_waiting_for_state_change: false,
            hard_deadline_at: started_at + INITIAL_AV_START_HARD_TIMEOUT,
            first_frame_presented: self.first_frame_presented,
            audio_prepare_phase: InitialAudioPreparePhase::Collecting,
            audio_prepare_epoch: None,
            audio_prepare_token: None,
            committed_bounded_delayed_audio_start_nsecs,
        };
        self.initial_av_start_transaction = Some(transaction);
        self.last_initial_audio_prepare_terminal_phase = None;
        self.initial_audio_defer_log_state = None;
        self.prestart_audio_ownership_log_state = None;
        self.initial_delayed_audio_start_timeline_nsecs = Some(audio_start_target_nsecs);
        self.set_state(PlaybackOutputState::Primed);
        tracing::debug!(
            transaction_id,
            discontinuity_epoch,
            seek_generation,
            target_nsecs = audio_start_target_nsecs,
            initial_audio_phase = InitialAudioPreparePhase::Collecting.as_str(),
            "initial audio prepare transaction phase changed"
        );
        transaction
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn note_initial_av_pair(
        &mut self,
        now: Instant,
    ) {
        self.initial_av_pair_started_at.get_or_insert(now);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn begin_initial_audio_prepare(
        &mut self,
        transaction_id: u64,
        audio_epoch: u64,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if transaction.transaction_id != transaction_id
            || transaction.audio_prepare_phase != InitialAudioPreparePhase::Collecting
        {
            return false;
        }
        transaction.audio_prepare_phase = InitialAudioPreparePhase::Preparing;
        transaction.audio_prepare_epoch = Some(audio_epoch);
        transaction.audio_prepare_token = None;
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn finish_initial_audio_prepare(
        &mut self,
        token: InitialAudioPrepareToken,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if transaction.transaction_id != token.transaction_id
            || transaction.discontinuity_epoch != token.discontinuity_epoch
            || transaction.seek_generation != token.seek_generation
            || transaction.audio_start_target_nsecs != token.target_nsecs
            || transaction.audio_prepare_phase != InitialAudioPreparePhase::Preparing
            || transaction.audio_prepare_epoch != Some(token.audio_epoch)
        {
            return false;
        }
        transaction.audio_prepare_phase = InitialAudioPreparePhase::Prepared;
        transaction.audio_prepare_token = Some(token);
        tracing::debug!(
            transaction_id = token.transaction_id,
            discontinuity_epoch = token.discontinuity_epoch,
            seek_generation = token.seek_generation,
            audio_epoch = token.audio_epoch,
            target_nsecs = token.target_nsecs,
            staged_range_nsecs = ?token.staged_range_nsecs,
            staged_frames = token.staged_frames,
            staged_samples = token.staged_samples,
            staged_until_nsecs = token.staged_until_nsecs,
            initial_audio_phase = InitialAudioPreparePhase::Prepared.as_str(),
            "initial audio prepare transaction phase changed"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn initial_audio_prepare_phase(
        &self,
    ) -> InitialAudioPreparePhase {
        self.initial_av_start_transaction
            .map(|transaction| transaction.audio_prepare_phase)
            .or(self.last_initial_audio_prepare_terminal_phase)
            .unwrap_or({
                if self.restart_pending() {
                    InitialAudioPreparePhase::Collecting
                } else {
                    InitialAudioPreparePhase::Aborted
                }
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn initial_audio_prepare_token(
        &self,
    ) -> Option<InitialAudioPrepareToken> {
        self.initial_av_start_transaction
            .and_then(|transaction| transaction.audio_prepare_token)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn initial_audio_prepare_target_nsecs(
        &self,
    ) -> Option<u64> {
        self.initial_av_start_transaction
            .map(|transaction| transaction.audio_start_target_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn abort_initial_audio_prepare(
        &mut self,
        transaction_id: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> Option<InitialAudioPrepareToken> {
        let mut transaction = self.initial_av_start_transaction.take()?;
        if transaction.transaction_id != transaction_id {
            self.initial_av_start_transaction = Some(transaction);
            return None;
        }
        let token = transaction.audio_prepare_token;
        let audio_epoch = token
            .map(|token| token.audio_epoch)
            .or(transaction.audio_prepare_epoch);
        transaction.audio_prepare_phase = InitialAudioPreparePhase::Aborted;
        self.last_initial_audio_prepare_terminal_phase = Some(InitialAudioPreparePhase::Aborted);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id,
            reason,
            discontinuity_epoch = transaction.discontinuity_epoch,
            seek_generation = transaction.seek_generation,
            audio_epoch = ?audio_epoch,
            target_nsecs = transaction.audio_start_target_nsecs,
            staged_range_nsecs = ?token.map(|token| token.staged_range_nsecs),
            staged_frames = token.map(|token| token.staged_frames).unwrap_or_default(),
            initial_audio_phase = InitialAudioPreparePhase::Aborted.as_str(),
            "initial audio prepare transaction phase changed"
        );
        // Keep the original seek anchor and already-presented first-frame
        // evidence. A retry receives a fresh transaction id.
        self.playback_output_state = PlaybackOutputState::Primed;
        self.refresh_video_deadline_service_active();
        self.output_clock_running = true;
        self.note_output_housekeeping_change();
        token
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_av_pair_watchdog_expired(
        &self,
        now: Instant,
    ) -> bool {
        self.restart_pending()
            && self
                .initial_av_pair_started_at
                .is_some_and(|started_at| now >= started_at + INITIAL_AV_START_HARD_TIMEOUT)
    }

    pub(in crate::player::backend::ffmpeg) fn output_service_demand(
        &self,
        now: Instant,
    ) -> OutputServiceDemand {
        if self
            .initial_av_start_transaction
            .is_some_and(|transaction| now >= transaction.hard_deadline_at)
            || self.initial_av_pair_watchdog_expired(now)
        {
            return OutputServiceDemand::HardDeadline;
        }
        if self
            .initial_av_start_transaction
            .is_some_and(|transaction| now >= transaction.next_audio_start_retry_at)
        {
            return OutputServiceDemand::AudioStartDue;
        }
        if self.output_housekeeping_generation != self.output_housekeeping_serviced_generation {
            return OutputServiceDemand::OutputStateChanged;
        }
        if self.decode_recovery_drained_boundary_ready() {
            return OutputServiceDemand::DecodeRecovery;
        }

        let startup_probe_due = self.restart_pending()
            && self.initial_av_start_transaction.is_none()
            && !self.scheduled_video_queue.is_empty()
            && self.syncing_started_at.is_some_and(|started_at| {
                let fallback_at = started_at + VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER;
                now >= fallback_at
                    && self
                        .last_output_housekeeping_service_at
                        .is_none_or(|serviced_at| serviced_at < fallback_at)
            });
        let decode_recovery_probe_due = self.decode_recovery_active()
            && self
                .last_output_housekeeping_service_at
                .is_none_or(|serviced_at| {
                    now.saturating_duration_since(serviced_at)
                        >= OUTPUT_GATE_PERIODIC_PROBE_INTERVAL
                });
        let periodic_probe_due = (self.playback_output_state.rebuffering()
            || (self.restart_pending()
                && self.initial_av_start_transaction.is_none()
                && self.initial_av_pair_started_at.is_some()))
            && self
                .last_output_housekeeping_service_at
                .is_none_or(|serviced_at| {
                    now.saturating_duration_since(serviced_at)
                        >= OUTPUT_GATE_PERIODIC_PROBE_INTERVAL
                });
        if decode_recovery_probe_due {
            OutputServiceDemand::DecodeRecovery
        } else if startup_probe_due || periodic_probe_due {
            OutputServiceDemand::PeriodicProbe
        } else {
            OutputServiceDemand::None
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn defer_initial_audio_start_retry(
        &mut self,
        now: Instant,
        reason: InitialAudioTransientRetry,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if now >= transaction.hard_deadline_at {
            return false;
        }
        transaction.audio_retry_waiting_for_state_change = false;
        transaction.next_audio_start_retry_at = now
            .checked_add(INITIAL_AUDIO_START_RETRY_INTERVAL)
            .unwrap_or(transaction.hard_deadline_at)
            .min(transaction.hard_deadline_at);
        tracing::trace!(
            transaction_id = transaction.transaction_id,
            reason = reason.as_str(),
            retry_in_ms = transaction
                .next_audio_start_retry_at
                .saturating_duration_since(now)
                .as_secs_f64()
                * 1_000.0,
            "scheduled bounded retry for transient initial audio output contention"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn preserve_initial_audio_prepare_for_retry(
        &mut self,
        transaction_id: u64,
        now: Instant,
        reason: InitialAudioTransientRetry,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if transaction.transaction_id != transaction_id
            || matches!(
                transaction.audio_prepare_phase,
                InitialAudioPreparePhase::Committed | InitialAudioPreparePhase::Aborted
            )
        {
            return false;
        }
        if transaction.audio_prepare_phase == InitialAudioPreparePhase::Preparing
            && transaction.audio_prepare_token.is_none()
        {
            transaction.audio_prepare_phase = InitialAudioPreparePhase::Collecting;
            transaction.audio_prepare_epoch = None;
        }
        transaction.audio_retry_waiting_for_state_change = false;
        transaction.next_audio_start_retry_at = now
            .checked_add(INITIAL_AUDIO_START_RETRY_INTERVAL)
            .unwrap_or(transaction.hard_deadline_at)
            .min(transaction.hard_deadline_at);
        tracing::debug!(
            transaction_id,
            reason = reason.as_str(),
            retry_in_ms = transaction
                .next_audio_start_retry_at
                .saturating_duration_since(now)
                .as_secs_f64()
                * 1000.0,
            initial_audio_phase = transaction.audio_prepare_phase.as_str(),
            "preserved initial audio transaction after retryable AO busy result"
        );
        true
    }

    /// Park an initial start that is missing decoded audio until ownership
    /// changes. This avoids turning a data dependency into an 8 ms polling
    /// loop while keeping the transaction hard deadline armed.
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn wait_initial_audio_start_for_state_change(
        &mut self,
        transaction_id: u64,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if transaction.transaction_id != transaction_id {
            return false;
        }
        transaction.audio_retry_waiting_for_state_change = true;
        transaction.next_audio_start_retry_at = transaction.hard_deadline_at;
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn observe_initial_sync_log(
        &mut self,
        observation: InitialSyncLogObservation,
        now: Instant,
    ) -> InitialSyncLogDecision {
        let output_generation = self.output_housekeeping_generation;
        let Some(state) = self.initial_sync_log_state.as_mut() else {
            self.initial_sync_log_state = Some(InitialSyncLogState {
                observation,
                output_generation,
                last_logged_at: now,
                suppressed_repeats: 0,
            });
            return InitialSyncLogDecision::Changed {
                suppressed_repeats: 0,
            };
        };
        if state.observation != observation || state.output_generation != output_generation {
            let suppressed_repeats = state.suppressed_repeats;
            *state = InitialSyncLogState {
                observation,
                output_generation,
                last_logged_at: now,
                suppressed_repeats: 0,
            };
            return InitialSyncLogDecision::Changed { suppressed_repeats };
        }

        state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
        if now.saturating_duration_since(state.last_logged_at) >= INITIAL_SYNC_LOG_SUMMARY_INTERVAL
        {
            let repeated_observations = state.suppressed_repeats;
            state.last_logged_at = now;
            state.suppressed_repeats = 0;
            InitialSyncLogDecision::Summary {
                repeated_observations,
            }
        } else {
            InitialSyncLogDecision::Suppressed
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn observe_initial_audio_defer_log(
        &mut self,
        observation: InitialAudioDeferObservation,
        now: Instant,
    ) -> InitialSyncLogDecision {
        let Some(state) = self.initial_audio_defer_log_state.as_mut() else {
            self.initial_audio_defer_log_state = Some(InitialAudioDeferLogState {
                observation,
                last_logged_at: now,
                suppressed_repeats: 0,
            });
            return InitialSyncLogDecision::Changed {
                suppressed_repeats: 0,
            };
        };
        if state.observation != observation {
            let suppressed_repeats = state.suppressed_repeats;
            *state = InitialAudioDeferLogState {
                observation,
                last_logged_at: now,
                suppressed_repeats: 0,
            };
            return InitialSyncLogDecision::Changed { suppressed_repeats };
        }

        state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
        if now.saturating_duration_since(state.last_logged_at)
            >= INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL
        {
            let repeated_observations = state.suppressed_repeats;
            state.last_logged_at = now;
            state.suppressed_repeats = 0;
            InitialSyncLogDecision::Summary {
                repeated_observations,
            }
        } else {
            InitialSyncLogDecision::Suppressed
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn note_output_housekeeping_change(
        &mut self,
    ) {
        self.output_housekeeping_generation = self.output_housekeeping_generation.saturating_add(1);
        if let Some(transaction) = self.initial_av_start_transaction.as_mut()
            && transaction.audio_retry_waiting_for_state_change
        {
            transaction.audio_retry_waiting_for_state_change = false;
            transaction.next_audio_start_retry_at = Instant::now();
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn observe_prestart_audio_ownership_log(
        &mut self,
        ownership: PrestartAudioOwnership,
        transaction_id: u64,
        now: Instant,
    ) -> InitialSyncLogDecision {
        let Some(state) = self.prestart_audio_ownership_log_state.as_mut() else {
            self.prestart_audio_ownership_log_state = Some(PrestartAudioOwnershipLogState {
                ownership,
                transaction_id,
                last_logged_at: now,
                suppressed_repeats: 0,
            });
            return InitialSyncLogDecision::Changed {
                suppressed_repeats: 0,
            };
        };
        if state.ownership != ownership || state.transaction_id != transaction_id {
            let suppressed_repeats = state.suppressed_repeats;
            *state = PrestartAudioOwnershipLogState {
                ownership,
                transaction_id,
                last_logged_at: now,
                suppressed_repeats: 0,
            };
            return InitialSyncLogDecision::Changed { suppressed_repeats };
        }
        state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
        if now.saturating_duration_since(state.last_logged_at)
            >= INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL
        {
            let repeated_observations = state.suppressed_repeats;
            state.last_logged_at = now;
            state.suppressed_repeats = 0;
            InitialSyncLogDecision::Summary {
                repeated_observations,
            }
        } else {
            InitialSyncLogDecision::Suppressed
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn observe_output_gate_block_log(
        &mut self,
        blocked_on: PlaybackBlockReason,
        detail: &'static str,
        now: Instant,
    ) -> Option<OutputGateBlockLogEmission> {
        const SUMMARY_INTERVAL: Duration = Duration::from_secs(1);
        match self.output_gate_block_log_state.as_mut() {
            Some(state) if state.blocked_on == blocked_on && state.detail == detail => {
                if now.saturating_duration_since(state.last_logged_at) < SUMMARY_INTERVAL {
                    state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
                    return None;
                }
                let emission = OutputGateBlockLogEmission {
                    log_kind: "periodic_summary",
                    suppressed_repeats: state.suppressed_repeats,
                    blocked_for: now.saturating_duration_since(state.started_at),
                };
                state.last_logged_at = now;
                state.suppressed_repeats = 0;
                Some(emission)
            }
            _ => {
                self.output_gate_block_log_state = Some(OutputGateBlockLogState {
                    blocked_on,
                    detail,
                    started_at: now,
                    last_logged_at: now,
                    suppressed_repeats: 0,
                });
                Some(OutputGateBlockLogEmission {
                    log_kind: "state_changed",
                    suppressed_repeats: 0,
                    blocked_for: Duration::ZERO,
                })
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn clear_output_gate_block_log(
        &mut self,
    ) {
        self.output_gate_block_log_state = None;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn mark_output_housekeeping_serviced(
        &mut self,
    ) {
        self.mark_output_housekeeping_serviced_at(Instant::now());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn mark_output_housekeeping_serviced_at(
        &mut self,
        now: Instant,
    ) {
        self.output_housekeeping_serviced_generation = self.output_housekeeping_generation;
        self.last_output_housekeeping_service_at = Some(now);
    }

    pub(in crate::player::backend::ffmpeg) fn output_housekeeping_deadline(
        &self,
    ) -> Option<Instant> {
        let now = Instant::now();
        let transaction_deadline = self.initial_av_start_transaction.map(|transaction| {
            transaction
                .next_audio_start_retry_at
                .min(transaction.hard_deadline_at)
        });
        let pair_deadline = self
            .initial_av_pair_started_at
            .map(|started_at| started_at + INITIAL_AV_START_HARD_TIMEOUT);
        let startup_fallback_deadline = (self.restart_pending()
            && self.initial_av_start_transaction.is_none()
            && !self.scheduled_video_queue.is_empty())
        .then(|| {
            self.syncing_started_at
                .map(|started_at| started_at + VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER)
        })
        .flatten()
        .filter(|fallback_at| {
            self.last_output_housekeeping_service_at
                .is_none_or(|serviced_at| serviced_at < *fallback_at)
        });
        let periodic_probe_deadline = (self.playback_output_state.rebuffering()
            || (self.restart_pending()
                && self.initial_av_start_transaction.is_none()
                && self.initial_av_pair_started_at.is_some())
            || self.decode_recovery_active())
        .then(|| {
            self.last_output_housekeeping_service_at
                .map(|serviced_at| serviced_at + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL)
                .unwrap_or(now + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL)
        });
        let decode_recovery_boundary_deadline =
            self.decode_recovery_drained_boundary_ready().then_some(now);
        [
            transaction_deadline,
            pair_deadline,
            startup_fallback_deadline,
            periodic_probe_deadline,
            decode_recovery_boundary_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn fail_initial_av_start_transaction(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.fail_initial_av_start_transaction_with_anchor(control, session_id, reason, None);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn fail_initial_av_start_transaction_at_anchor(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        reason: &'static str,
        fallback_anchor_nsecs: u64,
    ) {
        self.fail_initial_av_start_transaction_with_anchor(
            control,
            session_id,
            reason,
            Some(fallback_anchor_nsecs),
        );
    }

    fn fail_initial_av_start_transaction_with_anchor(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        reason: &'static str,
        fallback_anchor_override_nsecs: Option<u64>,
    ) {
        let transaction = self.initial_av_start_transaction.take();
        let pair_started_at = self.initial_av_pair_started_at;
        let fallback_anchor_nsecs = fallback_anchor_override_nsecs
            .or_else(|| transaction.map(|transaction| transaction.audio_start_target_nsecs))
            .or_else(|| {
                self.scheduled_video_queue
                    .range_nsecs()
                    .map(|(first_video_nsecs, _)| first_video_nsecs)
            });
        tracing::warn!(
            session_id = ?session_id,
            reason,
            video_anchor_nsecs = ?transaction.map(|transaction| transaction.video_anchor_nsecs),
            audio_start_target_nsecs = ?transaction
                .map(|transaction| transaction.audio_start_target_nsecs),
            fallback_anchor_nsecs,
            elapsed_ms = ?transaction
                .map(|transaction| transaction.started_at.elapsed().as_secs_f64() * 1000.0)
                .or_else(|| pair_started_at
                    .map(|started_at| started_at.elapsed().as_secs_f64() * 1000.0)),
            first_frame_presented = self.first_frame_presented,
            pending_audio_frames = self.pending_start_audio.len(),
            pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                * 1000.0,
            pending_audio_range_nsecs = ?self.pending_start_audio.range_nsecs(),
            pending_audio_contiguous_range_nsecs =
                ?self.pending_start_audio.contiguous_range_nsecs(),
            pending_audio_covers_target = transaction.is_some_and(|transaction| {
                self.pending_start_audio
                    .buffered_until_from(transaction.audio_start_target_nsecs)
                    .is_some_and(|buffered_until| {
                        buffered_until > transaction.audio_start_target_nsecs
                    })
            }),
            first_retained_video_timeline_nsecs = ?self
                .scheduled_video_queue
                .range_nsecs()
                .map(|(start, _)| start),
            potential_content_skip_ms = ?transaction.and_then(|transaction| {
                self.scheduled_video_queue
                    .range_nsecs()
                    .map(|(start, _)| {
                        start.saturating_sub(transaction.audio_start_target_nsecs) as f64
                            / 1_000_000.0
                    })
            }),
            "initial A/V start transaction exhausted its wall-time bound"
        );
        self.set_state(PlaybackOutputState::Rebuffering);
        self.rebuffer_started_at = Some(Instant::now());
        self.video_output_rebuffer_anchor =
            fallback_anchor_nsecs.map(|timeline_nsecs| RebufferResumeAnchor {
                timeline_nsecs,
                reset_to_video_when_decoded_queue_misses_anchor: false,
            });
        control.set_output_rebuffer_paused(true);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn commit_initial_av_start_transaction(
        &mut self,
    ) {
        self.defer_next_pending_start_audio_flush_after_initial_start();
        self.initial_delayed_audio_start_timeline_nsecs = None;
        self.set_state(PlaybackOutputState::Playing);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn commit_initial_audio_prepare(
        &mut self,
        token: InitialAudioPrepareToken,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction.as_mut() else {
            return false;
        };
        if transaction.transaction_id != token.transaction_id
            || transaction.discontinuity_epoch != token.discontinuity_epoch
            || transaction.seek_generation != token.seek_generation
            || transaction.audio_prepare_phase != InitialAudioPreparePhase::Prepared
            || transaction.audio_prepare_token != Some(token)
        {
            return false;
        }
        // Scheduler ownership commits before the callback-visible control word.
        // Retain the token until AO compare-and-commit succeeds so an activation
        // race can still roll the staged payload back losslessly.
        self.playback_output_state = PlaybackOutputState::Playing;
        self.refresh_video_deadline_service_active();
        self.first_frame_needed = false;
        self.output_clock_running = true;
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn finalize_initial_audio_prepare(
        &mut self,
        token: InitialAudioPrepareToken,
        session_id: PlaybackSessionId,
    ) -> bool {
        let clean_commit = self.playback_output_state == PlaybackOutputState::Playing
            && self
                .initial_av_start_transaction
                .is_some_and(|transaction| {
                    transaction.audio_prepare_phase == InitialAudioPreparePhase::Prepared
                        && transaction.audio_prepare_token == Some(token)
                });
        if clean_commit {
            if let Some(transaction) = self.initial_av_start_transaction.as_mut() {
                transaction.audio_prepare_phase = InitialAudioPreparePhase::Committed;
            }
            self.last_initial_audio_prepare_terminal_phase =
                Some(InitialAudioPreparePhase::Committed);
            tracing::debug!(
                session_id = ?session_id,
                transaction_id = token.transaction_id,
                discontinuity_epoch = token.discontinuity_epoch,
                seek_generation = token.seek_generation,
                audio_epoch = token.audio_epoch,
                target_nsecs = token.target_nsecs,
                staged_range_nsecs = ?token.staged_range_nsecs,
                staged_frames = token.staged_frames,
                staged_samples = token.staged_samples,
                initial_audio_phase = InitialAudioPreparePhase::Committed.as_str(),
                "initial audio prepare transaction phase changed"
            );
        } else {
            tracing::error!(
                session_id = ?session_id,
                transaction_id = token.transaction_id,
                discontinuity_epoch = token.discontinuity_epoch,
                seek_generation = token.seek_generation,
                audio_epoch = token.audio_epoch,
                target_nsecs = token.target_nsecs,
                observed_transaction_id = ?self
                    .initial_av_start_transaction
                    .map(|transaction| transaction.transaction_id),
                observed_initial_audio_phase = ?self
                    .initial_av_start_transaction
                    .map(|transaction| transaction.audio_prepare_phase.as_str()),
                observed_restart_pending = self.restart_pending(),
                observed_output_state = ?self.playback_output_state,
                initial_audio_phase = "recovered",
                "recovered initial audio commit bookkeeping after AO activation"
            );
        }
        self.defer_next_pending_start_audio_flush_after_initial_start();
        self.initial_delayed_audio_start_timeline_nsecs = None;
        self.initial_av_start_transaction = None;
        self.initial_av_pair_started_at = None;
        self.initial_audio_defer_log_state = None;
        self.prestart_audio_ownership_log_state = None;
        clean_commit
    }

    pub(in crate::player::backend::ffmpeg) fn scheduled_video_queue_limit_reached(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> bool {
        self.scheduled_video_queue
            .limit_reached(needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn scheduled_video_queue_len(&self) -> usize {
        self.scheduled_video_queue.len()
    }

    pub(in crate::player::backend::ffmpeg) fn audio_clocked_video_wait_duration(
        &self,
        played_until_nsecs: u64,
    ) -> Option<Duration> {
        if self.restart_pending()
            || self.playback_output_state.rebuffering()
            || self
                .scheduled_video_queue
                .deadline_service_owns_presentation()
        {
            return None;
        }
        self.scheduled_video_queue
            .audio_clock_wait_duration(played_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn video_decode_skip_nonref_for_pressure(
        &self,
        codec_id: ffi::AVCodecID,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        audio_output_pending_nsecs: Option<u64>,
        skip_nonref_active: bool,
    ) -> bool {
        self.scheduled_video_queue.skip_nonref_for_pressure(
            codec_id,
            self.playback_output_state,
            played_until_nsecs,
            has_audio_output,
            audio_output_pending_nsecs,
            skip_nonref_active,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn startup_sync_elapsed(
        &self,
    ) -> Option<Duration> {
        (self.playback_output_state == PlaybackOutputState::Syncing)
            .then(|| self.syncing_started_at.map(|started| started.elapsed()))
            .flatten()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn rebuffer_wait_elapsed(
        &self,
    ) -> Option<Duration> {
        self.playback_output_state
            .rebuffering()
            .then(|| self.rebuffer_started_at.map(|started| started.elapsed()))
            .flatten()
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffer_empty_audio_output_watchdog_delay(
        &self,
    ) -> Option<Duration> {
        if !self.rebuffer_empty_audio_output_blocked || !self.playback_output_state.rebuffering() {
            return None;
        }

        let resume_timeline_nsecs = self.rebuffer_empty_audio_output_resume_timeline_nsecs()?;
        let pending_audio_gap_delay = self
            .pending_start_audio
            .first_start_at_or_after(resume_timeline_nsecs)
            .map(|pending_audio_start_nsecs| {
                Duration::from_nanos(
                    pending_audio_start_nsecs.saturating_sub(resume_timeline_nsecs),
                )
            })
            .unwrap_or(REBUFFER_EMPTY_AUDIO_OUTPUT_WAKE_INTERVAL);
        let fallback_remaining = VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER
            .checked_sub(self.rebuffer_wait_elapsed().unwrap_or_default())
            .unwrap_or(Duration::ZERO);

        Some(
            REBUFFER_EMPTY_AUDIO_OUTPUT_WAKE_INTERVAL
                .min(fallback_remaining)
                .min(pending_audio_gap_delay),
        )
    }

    fn rebuffer_empty_audio_output_resume_timeline_nsecs(&self) -> Option<u64> {
        let first_video_nsecs = self
            .scheduled_video_queue
            .range_nsecs()
            .map(|(start, _)| start)?;
        let rebuffer_anchor_nsecs = self
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .unwrap_or(first_video_nsecs);

        Some(first_video_nsecs.max(rebuffer_anchor_nsecs))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn mark_startup_first_frame_stall_logged(
        &mut self,
    ) -> bool {
        if self.startup_first_frame_stall_logged {
            return false;
        }
        self.startup_first_frame_stall_logged = true;
        true
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn push_decoded_video_for_test(
        &mut self,
        frame: QueuedVideoFrame,
    ) {
        self.scheduled_video_queue.push_queued(frame);
        self.mark_first_frame_queued();
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn push_pending_start_audio_for_test(
        &mut self,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
    ) {
        self.pending_start_audio
            .push(audio, start_timeline_nsecs, end_timeline_nsecs);
        self.refresh_initial_bounded_delayed_audio_start_plan();
        self.note_output_housekeeping_change();
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_video_output_underrun_started_at_for_test(
        &mut self,
        started_at: Instant,
    ) {
        self.video_output_underrun_started_at = Some(started_at);
        if self.playback_output_state.rebuffering() {
            self.rebuffer_started_at = Some(started_at);
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn expire_audio_reader_gap_watchdog_for_test(&mut self) {
        if let Some(watchdog) = self.audio_reader_gap_watchdog.as_mut() {
            watchdog.last_progress_at =
                Instant::now() - VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER;
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn video_output_underrun_started_for_test(
        &self,
    ) -> bool {
        self.video_output_underrun_started_at.is_some()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_video_output_rebuffer_anchor_for_test(
        &mut self,
        anchor: RebufferResumeAnchor,
    ) {
        self.video_output_rebuffer_anchor = Some(anchor);
    }
}

#[cfg(test)]
mod decode_recovery_tests {
    use crate::player::render_host::{
        DecodedFrame, FramePixels, FramePts, PlaybackSessionId, RenderSize,
    };

    use super::{
        AudioOutputSnapshot, DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS, DECODE_RECOVERY_MAX_WALL_TIME,
        DecodeRecoveryDisposition, DecodeRecoveryPhase, DecodeRecoverySource, Duration,
        FfmpegControl, Instant, OUTPUT_GATE_PERIODIC_PROBE_INTERVAL, OutputServiceDemand,
        PlaybackOutputScheduler, PlaybackOutputState, PlaybackScheduler, QueuedVideoFrame,
        decode_recovery_staging_frame_budget,
    };

    fn frame(timeline_nsecs: u64, duration_nsecs: u64, old_hardware: bool) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: old_hardware,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            duration_nsecs,
            source_duration_nsecs: duration_nsecs,
        }
    }

    #[test]
    fn decode_recovery_discards_future_hw_frames_and_atomically_splices_software() {
        let session_id = PlaybackSessionId(1);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        for timeline_nsecs in [920_000_000, 960_000_000, 1_000_000_000, 1_040_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }

        output.begin_decode_recovery(
            7,
            1_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(7);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Replaying)
        );
        assert!(output.scheduled_video_queue.with_frames(|frames| {
            frames
                .iter()
                .all(|queued| queued.timeline_nsecs < 1_000_000_000)
        }));

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(900_000_000);
        let mut current_start_position_nsecs = 900_000_000;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Committed)
        );
        assert!(!output.decode_recovery_owns_video_admission());
        assert!(!output.decode_recovery_video_admission_blocked());
        assert_eq!(output.scheduled_video_queue.len(), 15);
        assert!(output.scheduled_video_queue.with_frames(|frames| {
            frames
                .iter()
                .filter(|queued| queued.timeline_nsecs >= 1_000_000_000)
                .all(|queued| !queued.frame.key_frame)
        }));
        assert_eq!(
            output.scheduled_video_queue.range_nsecs(),
            Some((920_000_000, 1_520_000_000))
        );
    }

    #[test]
    fn decode_recovery_barrier_pauses_at_target_until_staging_is_buffered() {
        let session_id = PlaybackSessionId(2);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            9,
            2_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(9);

        assert!(!output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            1_999_999_999,
            &control,
            session_id,
        ));
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            2_000_000_000,
            &control,
            session_id,
        ));
        assert!(control.is_output_rebuffer_paused());
        assert_eq!(
            output.playback_output_state,
            PlaybackOutputState::Rebuffering
        );

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(2_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        let mut scheduler = PlaybackScheduler::new(2_000_000_000);
        let mut current_start_position_nsecs = 2_000_000_000;
        assert!(!output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert!(control.is_output_rebuffer_paused());
        assert_eq!(
            output.playback_output_state,
            PlaybackOutputState::Rebuffering
        );
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert!(!control.is_output_rebuffer_paused());
        assert_eq!(output.playback_output_state, PlaybackOutputState::Playing);
    }

    #[test]
    fn decode_recovery_staging_does_not_advance_committed_progress_watermark() {
        let session_id = PlaybackSessionId(8);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            15,
            2_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(15);
        assert_eq!(output.admitted_video_queue_end_nsecs(), None);

        assert!(output.stage_decode_recovery_frame(
            frame(2_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));
        assert!(output.scheduled_video_queue.is_empty());
        assert_eq!(output.admitted_video_queue_end_nsecs(), None);
        assert_eq!(
            output.recovery_staged_high_water_nsecs(),
            Some(2_040_000_000)
        );
    }

    #[test]
    fn decode_recovery_buffered_phase_stops_input_and_caps_in_flight_staging() {
        let session_id = PlaybackSessionId(81);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            81,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(81);

        let frame_duration_nsecs = 33_333_333;
        let budget = decode_recovery_staging_frame_budget(frame_duration_nsecs, 3);
        assert_eq!(budget, 23);
        for index in 0..64_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(
                    1_000_000_000 + index * frame_duration_nsecs,
                    frame_duration_nsecs,
                    false,
                ),
                3,
                session_id,
            ));
            if output.decode_recovery_phase() == Some(DecodeRecoveryPhase::Buffered) {
                assert!(output.decode_recovery_video_admission_blocked());
            }
        }
        assert!(output.decode_recovery_video_admission_blocked());
        assert_eq!(output.recovery_staging_frame_budget(), Some(budget));
        assert_eq!(output.recovery_staging_frames(), budget);
        assert!(output.recovery_staging_frames() <= 24);
    }

    #[test]
    fn decode_recovery_audio_ready_latches_across_500_to_452ms_drop() {
        let session_id = PlaybackSessionId(82);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.begin_decode_recovery(
            82,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        let snapshot = |pending_nsecs| AudioOutputSnapshot {
            played_timeline_nsecs: 1_000_000_000,
            buffered_until_timeline_nsecs: 1_000_000_000 + pending_nsecs,
            shared_pending_nsecs: pending_nsecs,
            queue_pending_nsecs: 0,
            total_pending_nsecs: pending_nsecs,
            queue_frames: 0,
            queue_generation: 1,
            ..AudioOutputSnapshot::default()
        };

        assert!(output.update_decode_recovery_audio_ready(true, Some(snapshot(500_000_000)), 0,));
        assert!(output.update_decode_recovery_audio_ready(true, Some(snapshot(452_000_000)), 0,));
        assert!(output.decode_recovery_audio_ready_latched());

        let mut aggregate = PlaybackOutputScheduler::new();
        aggregate.begin_decode_recovery(
            83,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        assert!(aggregate.update_decode_recovery_audio_ready(
            true,
            Some(snapshot(452_000_000)),
            882_000_000,
        ));
    }

    #[test]
    fn coordinator_tick_can_commit_buffered_recovery_without_another_video_frame() {
        let session_id = PlaybackSessionId(84);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            84,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(84);
        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        let staged_frames_before_tick = output.recovery_staging_frames();
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(1_000_000_000);
        let mut current_start_position_nsecs = 1_000_000_000;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(staged_frames_before_tick, 13);
        assert_eq!(output.recovery_staging_frames(), 0);
        assert_eq!(output.scheduled_video_queue.len(), 13);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Committed)
        );
    }

    #[test]
    fn resource_pressure_releases_all_recovery_staging_references() {
        let session_id = PlaybackSessionId(85);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        for timeline_nsecs in [880_000_000, 920_000_000, 960_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }
        output.begin_decode_recovery(
            85,
            1_000_000_000,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(85);
        for index in 0..7_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }

        assert_eq!(
            output.release_vulkan_frames_for_resource_pressure(&control, session_id),
            10
        );
        assert!(output.scheduled_video_queue.is_empty());
        assert_eq!(output.recovery_staging_frames(), 0);
        assert_eq!(output.recovery_staged_high_water_nsecs(), None);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Failed)
        );
        assert!(control.is_output_rebuffer_paused());
    }

    #[test]
    fn recovery_snapshot_reports_committed_and_staged_watermarks_separately() {
        let session_id = PlaybackSessionId(86);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output
            .scheduled_video_queue
            .push_queued(frame(960_000_000, 40_000_000, true));
        output.begin_decode_recovery(
            86,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(86);
        assert!(output.stage_decode_recovery_frame(
            frame(1_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));

        let snapshot = output.snapshot();
        assert_eq!(snapshot.queued_video_frames, 1);
        assert_eq!(snapshot.recovery_staging_frames, 1);
        assert_eq!(snapshot.recovery_staging_frame_budget, Some(20));
        assert_eq!(
            snapshot.committed_output_high_water_nsecs,
            Some(1_000_000_000)
        );
        assert_eq!(
            snapshot.recovery_staged_high_water_nsecs,
            Some(1_040_000_000)
        );
        assert!(!snapshot.decode_recovery_audio_ready_latched);
    }

    #[test]
    fn decode_recovery_commits_log_derived_233ms_and_1_9s_gaps() {
        let session_id = PlaybackSessionId(3);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 1_036_766_666_666;
        let first_frame_nsecs = 1_037_000_000_000;
        let second_frame_nsecs = 1_038_933_333_333;
        let retained_frame_nsecs = target_nsecs - 33_333_333;
        output
            .scheduled_video_queue
            .push_queued(frame(retained_frame_nsecs, 33_333_333, true));
        output.begin_decode_recovery(
            11,
            target_nsecs,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(11);

        assert!(output.stage_decode_recovery_frame(
            frame(first_frame_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Replaying)
        );
        assert!(output.stage_decode_recovery_frame(
            frame(second_frame_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::HoldGap);
        assert_eq!(
            transaction.first_staged_frame_nsecs,
            Some(first_frame_nsecs)
        );
        assert_eq!(transaction.bridged_gap_count, 2);
        assert_eq!(
            transaction
                .staging_queue
                .with_frames(|frames| frames.front().expect("first staged frame").duration_nsecs),
            second_frame_nsecs - first_frame_nsecs
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::CommittedGap)
        );
        assert!(output.decode_recovery_output_committed());
        assert_eq!(current_start_position_nsecs, target_nsecs);
        let retained_frame_timing = output.scheduled_video_queue.with_frames(|frames| {
            let retained = frames.front().expect("retained visible frame");
            (retained.timeline_nsecs, retained.duration_nsecs)
        });
        assert_eq!(retained_frame_timing.0, retained_frame_nsecs);
        assert_eq!(
            retained_frame_timing.1,
            first_frame_nsecs - retained_frame_nsecs
        );
        assert_eq!(
            output
                .scheduled_video_queue
                .with_frames(|frames| frames.get(1).expect("committed first frame").timeline_nsecs),
            first_frame_nsecs
        );
    }

    #[test]
    fn decode_recovery_accepts_log_derived_mux_rounded_five_second_gap() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 367_466_645_832;
        let first_frame_nsecs = 372_466_687_500;
        output.begin_decode_recovery(
            5,
            target_nsecs,
            DecodeRecoverySource::CachedSafeIdrRebuild,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(5);

        assert_eq!(first_frame_nsecs - target_nsecs, 5_000_041_668);
        assert!(output.stage_decode_recovery_frame(
            frame(first_frame_nsecs, 33_333_332, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::HoldGap);
        assert_eq!(
            transaction.first_staged_frame_nsecs,
            Some(first_frame_nsecs)
        );
        assert_eq!(transaction.staging_queue.len(), 1);
        assert!(transaction.drop_for_fallback.is_none());

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::CommittedGap)
        );
        assert_eq!(current_start_position_nsecs, target_nsecs);
    }

    #[test]
    fn decode_recovery_reanchors_large_discontinuity_without_unbounded_replay() {
        let session_id = PlaybackSessionId(4);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 3_000_000_000;
        let resume_nsecs = 9_100_000_000;
        output.begin_decode_recovery(
            12,
            target_nsecs,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.confirm_decode_recovery_synchronized_timeline_gap();
        output.mark_decode_recovery_replaying(12);

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
        assert!(scheduler.current_timeline_nsecs() >= resume_nsecs);
        assert_eq!(
            output.audio_sync_drop_before_timeline_nsecs(),
            Some(resume_nsecs)
        );
    }

    #[test]
    fn decoder_error_reanchor_waits_for_last_good_frame_boundary() {
        let session_id = PlaybackSessionId(9);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 797_366_645_832;
        let resume_nsecs = 799_500_000_000;
        for timeline_nsecs in [target_nsecs - 80_000_000, target_nsecs - 40_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }
        output.begin_decode_recovery(
            10_591,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(10_591);

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .disposition,
            DecodeRecoveryDisposition::Reanchor
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs - 500_000_000);
        let mut current_start_position_nsecs = target_nsecs - 500_000_000;
        assert!(
            !output.commit_decode_recovery_if_buffered(
                &control,
                &mut scheduler,
                None,
                &mut current_start_position_nsecs,
                true,
                session_id,
            ),
            "the buffered IDR must not discard good queued frames early"
        );
        assert!(!output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs - 1,
            &control,
            session_id,
        ));
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs,
            &control,
            session_id,
        ));
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
        assert_eq!(
            output.scheduled_video_queue.range_nsecs(),
            Some((resume_nsecs, resume_nsecs + 13 * 40_000_000))
        );
        assert_eq!(
            output.audio_sync_drop_before_timeline_nsecs(),
            Some(resume_nsecs)
        );
    }

    #[test]
    fn decoder_error_reanchor_commits_when_audio_underruns_one_frame_before_boundary() {
        let session_id = PlaybackSessionId(9);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);

        // Exact values from the 14:41 failure: the retained queue ended at
        // 881.300s, but native audio exhausted its last real samples roughly
        // one frame earlier and could no longer advance the master clock.
        let target_nsecs = 881_300_020_832;
        let underrun_clock_nsecs = 881_266_458_916;
        let resume_nsecs = 884_500_000_000;
        output.scheduled_video_queue.push_queued(frame(
            target_nsecs - 33_333_332,
            33_333_332,
            true,
        ));
        let before_recovery = Instant::now();
        output.mark_output_housekeeping_serviced_at(before_recovery);
        output.begin_decode_recovery(
            6_343,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        assert_eq!(
            output.output_service_demand(before_recovery),
            OutputServiceDemand::OutputStateChanged,
            "starting recovery must wake output independently of decoder input"
        );
        output.mark_output_housekeeping_serviced_at(before_recovery);
        output.mark_decode_recovery_replaying(6_343);
        for index in 0..16_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 33_333_332, 33_333_332, false,),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(
            output.output_service_demand(Instant::now()),
            OutputServiceDemand::OutputStateChanged,
            "buffering the recovery transaction must publish another output wakeup"
        );

        let serviced_at = Instant::now();
        output.mark_output_housekeeping_serviced_at(serviced_at);
        assert_eq!(
            output.output_service_demand(
                serviced_at + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL - Duration::from_nanos(1)
            ),
            OutputServiceDemand::None
        );
        assert_eq!(
            output.output_service_demand(serviced_at + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL),
            OutputServiceDemand::DecodeRecovery,
            "an active recovery must keep waking output even if decoder input is blocked"
        );
        assert!(!output.maybe_enter_decode_recovery_barrier(
            serviced_at,
            underrun_clock_nsecs,
            &control,
            session_id,
        ));

        // Once the retained frame has been handed to VO, waiting for the audio
        // clock to reach its end timestamp is impossible during underrun.
        output.scheduled_video_queue.clear();
        assert_eq!(
            output.output_service_demand(serviced_at),
            OutputServiceDemand::DecodeRecovery
        );
        assert!(
            output
                .output_housekeeping_deadline()
                .is_some_and(|deadline| deadline <= Instant::now()),
            "a drained recovery boundary must make output housekeeping immediately due"
        );
        output
            .decode_recovery_transaction
            .as_mut()
            .expect("transaction")
            .started_at = Instant::now()
            .checked_sub(DECODE_RECOVERY_MAX_WALL_TIME + Duration::from_millis(1))
            .expect("deadline start");
        assert!(
            output
                .check_decode_recovery_deadline(Instant::now(), &control, session_id)
                .is_ok(),
            "a drained commit-ready boundary must win a race with the wall deadline"
        );
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            underrun_clock_nsecs,
            &control,
            session_id,
        ));

        let mut scheduler = PlaybackScheduler::new(underrun_clock_nsecs);
        let mut current_start_position_nsecs = underrun_clock_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
    }

    #[test]
    fn decoder_error_reanchor_does_not_count_log_derived_skipped_gap_as_replay() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);

        let target_nsecs = 955_566_645_832;
        let resume_nsecs = 961_300_000_000;
        output.scheduled_video_queue.push_queued(frame(
            target_nsecs - 33_333_332,
            33_333_332,
            true,
        ));
        output.begin_decode_recovery(
            6_082,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(6_082);

        assert_eq!(resume_nsecs - target_nsecs, 5_733_354_168);
        for index in 0..16_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 33_333_332, 33_333_332, false,),
                3,
                session_id,
            ));
        }
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.phase, DecodeRecoveryPhase::Buffered);
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::Reanchor);
        assert_eq!(transaction.resume_nsecs, resume_nsecs);
        assert!(transaction.drop_for_fallback.is_none());
        assert_eq!(transaction.staging_queue.len(), 16);

        output.scheduled_video_queue.clear();
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs - 33_333_332,
            &control,
            session_id,
        ));
        let mut scheduler = PlaybackScheduler::new(target_nsecs - 33_333_332);
        let mut current_start_position_nsecs = target_nsecs - 33_333_332;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
    }

    #[test]
    fn decode_recovery_withholds_large_continuous_gap_for_bounded_fallback() {
        let session_id = PlaybackSessionId(6);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 681_266_667_000;
        let resume_nsecs = target_nsecs + 9_000_000_000;
        output.begin_decode_recovery(
            14,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_output_housekeeping_serviced_at(Instant::now());
        output.mark_decode_recovery_replaying(14);

        assert!(output.stage_decode_recovery_frame(
            frame(resume_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::DroppedForFallback)
        );
        assert_eq!(
            output.output_service_demand(Instant::now()),
            OutputServiceDemand::OutputStateChanged,
            "terminal fallback must wake output while decoder admission is blocked"
        );
        assert!(!output.decode_recovery_output_committed());
        assert!(output.decode_recovery_owns_video_admission());
        assert!(output.decode_recovery_video_admission_blocked());
        assert!(output.scheduled_video_queue.is_empty());
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
        assert_eq!(output.audio_sync_drop_before_timeline_nsecs(), None);

        let drop = output
            .take_decode_recovery_drop_for_fallback()
            .expect("bounded fallback request");
        assert_eq!(drop.transaction_id, 14);
        assert_eq!(drop.target_nsecs, target_nsecs);
        assert_eq!(drop.first_frame_nsecs, resume_nsecs);
        assert_eq!(drop.gap_nsecs, 9_000_000_000);
        assert_eq!(drop.source, DecodeRecoverySource::VulkanReopenReplay);
        assert!(
            !output.stage_decode_recovery_frame(
                frame(resume_nsecs + 33_333_333, 33_333_333, false),
                3,
                session_id,
            ),
            "frames arriving before coordinator fallback must remain withheld"
        );
        assert!(output.scheduled_video_queue.is_empty());

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(!output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(current_start_position_nsecs, target_nsecs);
        assert_eq!(output.audio_sync_drop_before_timeline_nsecs(), None);
    }

    #[test]
    fn replay_span_limit_makes_a_terminal_decision_and_releases_staging() {
        let session_id = PlaybackSessionId(61);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            61,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(61);
        assert!(output.stage_decode_recovery_frame(
            frame(target_nsecs, 40_000_000, false),
            3,
            session_id,
        ));
        assert!(output.stage_decode_recovery_frame(
            frame(
                target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS,
                40_000_000,
                false,
            ),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::DroppedForFallback)
        );
        assert_eq!(output.recovery_staging_frames(), 0);
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
        assert!(output.take_decode_recovery_drop_for_fallback().is_some());
    }

    #[test]
    fn first_hold_gap_frame_end_does_not_count_as_unbounded_replay() {
        let session_id = PlaybackSessionId(63);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            63,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(63);
        assert!(output.stage_decode_recovery_frame(
            frame(
                target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS - 20_000_000,
                40_000_000,
                false,
            ),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(output.recovery_staging_frames(), 1);
        assert!(output.take_decode_recovery_drop_for_fallback().is_none());
        assert_eq!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .disposition,
            DecodeRecoveryDisposition::HoldGap
        );
    }

    #[test]
    fn replay_span_limit_reanchors_only_a_confirmed_synchronized_gap() {
        let session_id = PlaybackSessionId(62);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            62,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.confirm_decode_recovery_synchronized_timeline_gap();
        output.mark_decode_recovery_replaying(62);
        assert!(output.stage_decode_recovery_frame(
            frame(target_nsecs, 40_000_000, false),
            3,
            session_id,
        ));
        let resume_nsecs = target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS;
        assert!(output.stage_decode_recovery_frame(
            frame(resume_nsecs, 40_000_000, false),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::Reanchor);
        assert_eq!(transaction.resume_nsecs, resume_nsecs);
        assert_eq!(transaction.staging_queue.len(), 1);
    }

    #[test]
    fn decode_recovery_wall_deadline_enters_explicit_failed_state() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            13,
            4_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(13);
        assert!(output.stage_decode_recovery_frame(
            frame(4_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));
        output
            .decode_recovery_transaction
            .as_mut()
            .expect("transaction")
            .started_at = Instant::now()
            .checked_sub(DECODE_RECOVERY_MAX_WALL_TIME + Duration::from_millis(1))
            .expect("deadline start");

        assert!(
            output
                .check_decode_recovery_deadline(Instant::now(), &control, session_id)
                .is_err()
        );
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Failed)
        );
        assert!(!output.decode_recovery_active());
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
    }

    #[test]
    fn audio_sync_drop_before_logs_first_periodic_and_final_summaries() {
        let session_id = PlaybackSessionId(7);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.set_audio_sync_drop_before_timeline_nsecs(24_000_000_000, session_id, "test");

        for frame_index in 0..209_u64 {
            let snapshot = output.snapshot();
            output.record_audio_sync_drop_before_frame(
                frame_index as i64,
                23_000_000_000 + frame_index * 10_000_000,
                23_010_000_000 + frame_index * 10_000_000,
                snapshot,
                session_id,
            );
        }
        let summary = output
            .audio_sync_drop_log_summary
            .as_ref()
            .expect("active drop summary");
        assert_eq!(summary.total_dropped_frames, 209);
        assert_eq!(summary.suppressed_since_last_log, 208);

        output
            .audio_sync_drop_log_summary
            .as_mut()
            .expect("active drop summary")
            .last_log_at = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("periodic summary timestamp");
        let snapshot = output.snapshot();
        output.record_audio_sync_drop_before_frame(
            209,
            23_999_000_000,
            24_000_000_000,
            snapshot,
            session_id,
        );
        let summary = output
            .audio_sync_drop_log_summary
            .as_ref()
            .expect("active drop summary");
        assert_eq!(summary.total_dropped_frames, 210);
        assert_eq!(summary.suppressed_since_last_log, 0);

        output.finish_audio_sync_drop_log_summary(session_id, "test_complete");
        assert!(output.audio_sync_drop_log_summary.is_none());
    }
}
