use super::OutputGateResumeStatus;
use super::{
    AtomicBool, AudioClockMode, AudioOutput, AudioOutputLifecycle, AudioOutputServiceStage,
    AudioOutputSnapshot, AudioOutputStableSnapshot, BackendEvent, BufferedReporter,
    DelayedAudioStartSilencePolicy, DemuxPacketCache, FfmpegControl,
    INITIAL_AUDIO_START_MIN_AMMUNITION, InitialAudioDeferObservation, InitialAudioPreparePhase,
    InitialAudioPrepareToken, InitialAudioTransientRetry, InitialAvStartDecision,
    InitialSyncLogDecision, Instant, PendingAudioRetentionAnchorSource, PlaybackOutputScheduler,
    PlaybackScheduler, PlaybackSessionId, PositionReporter, PrestartAudioOwnership,
    PrestartAudioOwnershipInput, QueuedVideoFrame, Sender, SubtitlePipeline,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VideoOutputQueue, classify_prestart_audio_ownership,
    duration_nsecs, nsecs_to_seconds, present_video_frame_to_vo,
    report_first_video_frame_presented, stage_pending_audio,
};
use crate::player::backend::{BackendDiagnostic, BackendEventKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialAudioNoPayloadDisposition
{
    RetryTransient,
    RebufferTerminal,
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_audio_no_payload_disposition(
    would_block: bool,
) -> InitialAudioNoPayloadDisposition {
    if would_block {
        InitialAudioNoPayloadDisposition::RetryTransient
    } else {
        InitialAudioNoPayloadDisposition::RebufferTerminal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialAudioAmmunitionSnapshot
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_start_target_nsecs:
        u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pending_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pending_audio_range_nsecs:
        Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pending_audio_contiguous_range_nsecs:
        Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pending_audio_buffered_until_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) device_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) device_audio_range_nsecs:
        Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) device_audio_buffered_until_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_ledger_observed:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_estimated_range_nsecs:
        Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) total_audio_nsecs: u64,
}

impl InitialAudioAmmunitionSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn from_ledgers(
        output_scheduler: &PlaybackOutputScheduler,
        audio_snapshot: Option<AudioOutputSnapshot>,
        decoded_audio_nsecs: u64,
        audio_start_target_nsecs: u64,
    ) -> Self {
        Self::from_optional_ledgers(
            output_scheduler,
            audio_snapshot,
            Some(decoded_audio_nsecs),
            audio_start_target_nsecs,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn from_optional_ledgers(
        output_scheduler: &PlaybackOutputScheduler,
        audio_snapshot: Option<AudioOutputSnapshot>,
        decoded_audio_nsecs: Option<u64>,
        audio_start_target_nsecs: u64,
    ) -> Self {
        let decoded_audio_ledger_observed = decoded_audio_nsecs.is_some();
        let decoded_audio_nsecs = decoded_audio_nsecs.unwrap_or_default();
        let pending_audio_nsecs =
            duration_nsecs(output_scheduler.pending_start_audio.buffered_duration());
        let pending_audio_range_nsecs = output_scheduler.pending_start_audio.range_nsecs();
        let pending_audio_contiguous_range_nsecs = output_scheduler
            .pending_start_audio
            .contiguous_range_nsecs();
        let pending_audio_buffered_until_nsecs = output_scheduler
            .pending_start_audio
            .buffered_until_from(audio_start_target_nsecs)
            .filter(|buffered_until| *buffered_until > audio_start_target_nsecs);
        let device_audio_nsecs = audio_snapshot
            .map(|snapshot| {
                snapshot
                    .shared_payload_nsecs
                    .saturating_add(snapshot.queue_pending_nsecs)
                    .saturating_add(snapshot.worker_in_flight_nsecs)
            })
            .unwrap_or_default();
        let device_audio_range_nsecs = audio_snapshot
            .filter(|snapshot| {
                snapshot.shared_payload_nsecs > 0
                    || snapshot.queue_pending_nsecs > 0
                    || snapshot.worker_in_flight_nsecs > 0
            })
            .and_then(|snapshot| snapshot.payload_range_nsecs);
        // This is deliberately a defensive OR ledger. The normal Primed path
        // keeps AO empty, but an older/racing route may already have placed the
        // head at the device. Never count an empty clock positioned past target.
        let device_audio_buffered_until_nsecs = audio_snapshot
            .and_then(|snapshot| snapshot.payload_range_nsecs)
            .filter(|(start, end)| {
                *start <= audio_start_target_nsecs && *end > audio_start_target_nsecs
            })
            .map(|(_, end)| end);
        let decoded_audio_estimated_range_nsecs = (decoded_audio_nsecs > 0)
            .then(|| {
                pending_audio_contiguous_range_nsecs
                    .map(|(_, end)| end)
                    .or_else(|| device_audio_range_nsecs.map(|(_, end)| end))
                    .map(|start| (start, start.saturating_add(decoded_audio_nsecs)))
            })
            .flatten();
        let total_audio_nsecs = pending_audio_nsecs
            .saturating_add(device_audio_nsecs)
            .saturating_add(decoded_audio_nsecs);
        Self {
            audio_start_target_nsecs,
            pending_audio_nsecs,
            pending_audio_range_nsecs,
            pending_audio_contiguous_range_nsecs,
            pending_audio_buffered_until_nsecs,
            device_audio_nsecs,
            device_audio_range_nsecs,
            device_audio_buffered_until_nsecs,
            decoded_audio_nsecs,
            decoded_audio_ledger_observed,
            decoded_audio_estimated_range_nsecs,
            total_audio_nsecs,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn pending_covers_target(
        self,
    ) -> bool {
        self.pending_audio_buffered_until_nsecs.is_some()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn device_covers_target(
        self,
    ) -> bool {
        self.device_audio_buffered_until_nsecs.is_some()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn covers_target(
        self,
    ) -> bool {
        self.pending_covers_target() || self.device_covers_target()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn reaches_force_start_threshold(
        self,
    ) -> bool {
        self.total_audio_nsecs >= duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialAudioStartAction {
    WaitingForTransaction,
    WaitingForCompleteLedger,
    DeferBelowThreshold,
    CommitCovered,
    CommitDegraded,
    FailNoAmmunition,
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_audio_start_action(
    decision: InitialAvStartDecision,
    ammunition: InitialAudioAmmunitionSnapshot,
) -> InitialAudioStartAction {
    if decision == InitialAvStartDecision::Waiting {
        return InitialAudioStartAction::WaitingForTransaction;
    }
    if decision == InitialAvStartDecision::Rebuffer {
        return InitialAudioStartAction::FailNoAmmunition;
    }
    if ammunition.covers_target() {
        return InitialAudioStartAction::CommitCovered;
    }
    if ammunition.reaches_force_start_threshold() {
        return InitialAudioStartAction::CommitDegraded;
    }
    if !ammunition.decoded_audio_ledger_observed {
        return InitialAudioStartAction::WaitingForCompleteLedger;
    }
    InitialAudioStartAction::DeferBelowThreshold
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_audio_start_ammunition_ready(
    output_scheduler: &PlaybackOutputScheduler,
    audio_snapshot: Option<AudioOutputSnapshot>,
    audio_start_timeline_nsecs: u64,
) -> bool {
    InitialAudioAmmunitionSnapshot::from_ledgers(
        output_scheduler,
        audio_snapshot,
        0,
        audio_start_timeline_nsecs,
    )
    .covers_target()
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_audio_clock_reset_required(
    ammunition: InitialAudioAmmunitionSnapshot,
) -> bool {
    !ammunition.device_covers_target()
}

fn log_initial_audio_start_defer(
    output_scheduler: &mut PlaybackOutputScheduler,
    ammunition: InitialAudioAmmunitionSnapshot,
    presented_video_frames: usize,
    session_id: PlaybackSessionId,
    now: Instant,
) {
    debug_assert!(!ammunition.covers_target());
    debug_assert!(
        !ammunition.reaches_force_start_threshold(),
        "initial audio defer is only valid below the force-start threshold"
    );
    let observation = InitialAudioDeferObservation {
        audio_start_target_nsecs: ammunition.audio_start_target_nsecs,
        pending_covers_target: ammunition.pending_covers_target(),
        device_covers_target: ammunition.device_covers_target(),
        ammunition_at_threshold: ammunition.reaches_force_start_threshold(),
        decoded_audio_ledger_observed: ammunition.decoded_audio_ledger_observed,
    };
    let (defer_log_kind, repeated_observations) =
        match output_scheduler.observe_initial_audio_defer_log(observation, now) {
            InitialSyncLogDecision::Changed { suppressed_repeats } => {
                ("state_changed", suppressed_repeats)
            }
            InitialSyncLogDecision::Summary {
                repeated_observations,
            } => ("periodic_summary", repeated_observations),
            InitialSyncLogDecision::Suppressed => return,
        };
    tracing::debug!(
        session_id = ?session_id,
        presented_video_frames,
        audio_start_target_nsecs = ammunition.audio_start_target_nsecs,
        initial_start_phase = "ready_waiting_audio_buffer",
        defer_log_kind,
        repeated_observations,
        pending_audio_frames = output_scheduler.pending_start_audio.len(),
        pending_audio_ms = ammunition.pending_audio_nsecs as f64 / 1_000_000.0,
        pending_audio_range_nsecs = ?ammunition.pending_audio_range_nsecs,
        pending_audio_contiguous_range_nsecs =
            ?ammunition.pending_audio_contiguous_range_nsecs,
        pending_audio_buffered_until_nsecs =
            ?ammunition.pending_audio_buffered_until_nsecs,
        device_audio_ms = ammunition.device_audio_nsecs as f64 / 1_000_000.0,
        device_audio_range_nsecs = ?ammunition.device_audio_range_nsecs,
        device_audio_buffered_until_nsecs =
            ?ammunition.device_audio_buffered_until_nsecs,
        audio_decode_queued_ms = ammunition.decoded_audio_nsecs as f64 / 1_000_000.0,
        audio_decode_ledger_observed = ammunition.decoded_audio_ledger_observed,
        audio_decode_estimated_range_nsecs =
            ?ammunition.decoded_audio_estimated_range_nsecs,
        total_audio_ms = ammunition.total_audio_nsecs as f64 / 1_000_000.0,
        ammunition_threshold_ms =
            duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION) as f64 / 1_000_000.0,
        audio_output_lifecycle = AudioOutputLifecycle::Ready.as_str(),
        clock_mode = AudioClockMode::SyncingVideo.as_str(),
        "deferred native audio output start until decoded audio is queued"
    );
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn release_initial_seek_transition_after_clock_reset(
    control: &FfmpegControl,
) -> bool {
    control.finish_seek_audio_pause()
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn commit_initial_av_start(
    output_scheduler: &mut PlaybackOutputScheduler,
    control: &FfmpegControl,
) -> OutputGateResumeStatus {
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    output_scheduler.commit_initial_av_start_transaction();
    OutputGateResumeStatus::Resumed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialAudioCommitCheckpoint
{
    SchedulerCommitted,
    ControlCommitted,
    StreamPlayed,
}

struct InitialAudioStageGuard<'a> {
    scheduler: &'a mut PlaybackOutputScheduler,
    output: &'a AudioOutput,
    token: InitialAudioPrepareToken,
    session_id: PlaybackSessionId,
    event_tx: Sender<BackendEvent>,
    abort_reason: &'static str,
    terminal: bool,
}

impl<'a> InitialAudioStageGuard<'a> {
    fn new(
        scheduler: &'a mut PlaybackOutputScheduler,
        output: &'a AudioOutput,
        token: InitialAudioPrepareToken,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) -> Self {
        Self {
            scheduler,
            output,
            token,
            session_id,
            event_tx: event_tx.clone(),
            abort_reason: "initial_audio_prepare_scope_exit",
            terminal: false,
        }
    }

    fn abort(&mut self, reason: &'static str) {
        self.abort_reason = reason;
    }

    fn preserve_for_retry(&mut self, reason: InitialAudioTransientRetry) {
        self.abort_reason = reason.as_str();
        if self.scheduler.preserve_initial_audio_prepare_for_retry(
            self.token.transaction_id,
            Instant::now(),
            reason,
        ) {
            self.terminal = true;
        }
    }

    fn preserve_for_terminal_cleanup(&mut self, reason: &'static str) {
        self.abort_reason = reason;
        self.terminal = true;
    }

    fn set_token(&mut self, token: InitialAudioPrepareToken) {
        self.token = token;
    }

    fn commit(&mut self, control: &FfmpegControl) -> bool {
        self.commit_with_checkpoints(control, |_, _| {})
    }

    fn commit_with_checkpoints(
        &mut self,
        control: &FfmpegControl,
        mut at_checkpoint: impl FnMut(InitialAudioCommitCheckpoint, bool),
    ) -> bool {
        let _stage = self
            .output
            .begin_service_stage(AudioOutputServiceStage::ControlCommit);
        if !self.scheduler.commit_initial_audio_prepare(self.token) {
            self.abort_reason = "scheduler_token_validation_failed";
            return false;
        }
        at_checkpoint(
            InitialAudioCommitCheckpoint::SchedulerCommitted,
            self.output.stream_active(),
        );
        if !self.output.commit_audio_output_control(
            self.token.audio_epoch,
            self.token.seek_generation,
            control,
        ) {
            self.abort_reason = "audio_compare_and_commit_failed";
            return false;
        }
        at_checkpoint(
            InitialAudioCommitCheckpoint::ControlCommitted,
            self.output.stream_active(),
        );
        if control.should_interrupt() || control.seek_generation() != self.token.seek_generation {
            self.abort_reason = "generation_changed_before_stream_play";
            return false;
        }
        if let Err(error) = self.output.play_committed_audio_output() {
            control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
            self.abort_reason = "audio_stream_play_failed";
            tracing::warn!(
                session_id = ?self.session_id,
                transaction_id = self.token.transaction_id,
                %error,
                "failed to play native audio stream after scheduler/control commit"
            );
            return false;
        }
        at_checkpoint(
            InitialAudioCommitCheckpoint::StreamPlayed,
            self.output.stream_active(),
        );
        // Physical AO start is the point of no return. A later bookkeeping
        // mismatch must complete through the scheduler's recovered terminal
        // path; it must never let Drop roll active audio back as Aborted.
        self.terminal = true;
        if !self
            .scheduler
            .finalize_initial_audio_prepare(self.token, self.session_id)
        {
            let _ = self.event_tx.send(BackendEvent::new(
                self.session_id,
                BackendEventKind::Diagnostic(BackendDiagnostic {
                    code: "ffmpeg_initial_audio_commit_recovered",
                    message: format!(
                        "transaction={} phase=recovered reason=audio_commit_finalize_validation_failed discontinuity_epoch={} seek_generation={} audio_epoch={} target={}",
                        self.token.transaction_id,
                        self.token.discontinuity_epoch,
                        self.token.seek_generation,
                        self.token.audio_epoch,
                        self.token.target_nsecs,
                    ),
                }),
            ));
        }
        true
    }

    fn scheduler(&self) -> &PlaybackOutputScheduler {
        self.scheduler
    }

    fn scheduler_mut(&mut self) -> &mut PlaybackOutputScheduler {
        self.scheduler
    }
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn commit_initial_audio_stage_with_checkpoints_for_test(
    scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    token: InitialAudioPrepareToken,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
    control: &FfmpegControl,
    at_checkpoint: impl FnMut(InitialAudioCommitCheckpoint, bool),
) -> bool {
    let mut guard = InitialAudioStageGuard::new(scheduler, output, token, session_id, event_tx);
    guard.commit_with_checkpoints(control, at_checkpoint)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn abort_initial_audio_stage_for_test(
    scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    token: InitialAudioPrepareToken,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
    reason: &'static str,
) {
    let mut guard = InitialAudioStageGuard::new(scheduler, output, token, session_id, event_tx);
    guard.abort(reason);
}

impl Drop for InitialAudioStageGuard<'_> {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        match self
            .output
            .try_abort_staged_audio(self.token.audio_epoch, self.token.target_nsecs)
        {
            Ok(Some(frames)) => self
                .scheduler
                .pending_start_audio
                .restore_staged_frames(frames),
            Ok(None) => tracing::warn!(
                session_id = ?self.session_id,
                transaction_id = self.token.transaction_id,
                audio_epoch = self.token.audio_epoch,
                reason = self.abort_reason,
                "epoch-fenced busy AO while aborting initial prepare"
            ),
            Err(error) => tracing::warn!(
                session_id = ?self.session_id,
                transaction_id = self.token.transaction_id,
                audio_epoch = self.token.audio_epoch,
                reason = self.abort_reason,
                %error,
                "failed to rebuild pending audio while aborting initial prepare"
            ),
        }
        self.scheduler.abort_initial_audio_prepare(
            self.token.transaction_id,
            self.session_id,
            self.abort_reason,
        );
        let _ = self.event_tx.send(BackendEvent::new(
            self.session_id,
            BackendEventKind::Diagnostic(BackendDiagnostic {
                code: "ffmpeg_initial_audio_prepare_aborted",
                message: format!(
                    "transaction={} phase=aborted reason={} discontinuity_epoch={} seek_generation={} audio_epoch={} target={}",
                    self.token.transaction_id,
                    self.abort_reason,
                    self.token.discontinuity_epoch,
                    self.token.seek_generation,
                    self.token.audio_epoch,
                    self.token.target_nsecs,
                ),
            }),
        ));
    }
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn fail_initial_av_start_after_unstable_snapshot_deadline(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    transaction_decision: InitialAvStartDecision,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) -> bool {
    if transaction_decision != InitialAvStartDecision::Rebuffer {
        return false;
    }
    let Some(transaction) = output_scheduler.initial_av_start_transaction() else {
        return false;
    };
    let target_nsecs = transaction.audio_start_target_nsecs;

    if matches!(
        transaction.audio_prepare_phase,
        InitialAudioPreparePhase::Preparing | InitialAudioPreparePhase::Prepared
    ) {
        if let Some(audio_epoch) = transaction.audio_prepare_epoch {
            // A stable snapshot is intentionally not required for rollback.
            // The epoch-fenced abort owns the queue reset and restores staged
            // frames before the scheduler publishes the terminal rebuffer.
            let rollback_token =
                transaction
                    .audio_prepare_token
                    .unwrap_or(InitialAudioPrepareToken {
                        transaction_id: transaction.transaction_id,
                        discontinuity_epoch: transaction.discontinuity_epoch,
                        seek_generation: transaction.seek_generation,
                        audio_epoch,
                        target_nsecs,
                        staged_range_nsecs: (target_nsecs, target_nsecs),
                        staged_frames: 0,
                        staged_samples: 0,
                        staged_until_nsecs: target_nsecs,
                    });
            let mut stage_guard = InitialAudioStageGuard::new(
                output_scheduler,
                output,
                rollback_token,
                session_id,
                event_tx,
            );
            stage_guard.abort("audio_snapshot_unstable_hard_deadline");
            drop(stage_guard);
        } else {
            output_scheduler.abort_initial_audio_prepare(
                transaction.transaction_id,
                session_id,
                "audio_snapshot_unstable_hard_deadline_missing_epoch",
            );
        }
    }

    output_scheduler.fail_initial_av_start_transaction_at_anchor(
        control,
        session_id,
        "audio_snapshot_unstable_hard_deadline",
        target_nsecs,
    );
    true
}

/// Ends an expired initial A/V transaction without taking either AO mutex.
///
/// This is deliberately callable by the coordinator before any status or
/// stable snapshot. Epoch fencing makes queue/callback publications from the
/// old prepare stale; physical queue cleanup is left to a later bounded AO
/// service. The fallback anchor always remains the transaction's original
/// audio target.
pub(in crate::player::backend::ffmpeg) fn expire_initial_av_start_hard_deadline(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: Option<&AudioOutput>,
    now: Instant,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
) -> bool {
    if !output_scheduler
        .output_service_demand(now)
        .hard_deadline_due()
    {
        return false;
    }

    let transaction = output_scheduler.initial_av_start_transaction();
    let fallback_anchor_nsecs = transaction
        .map(|transaction| transaction.audio_start_target_nsecs)
        .or_else(|| output_scheduler.initial_audio_prepare_target_nsecs());
    if let (Some(output), Some(fallback_anchor_nsecs)) = (output, fallback_anchor_nsecs) {
        let fenced_epoch = output.fence_clock_without_wait(fallback_anchor_nsecs);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = ?transaction.map(|transaction| transaction.transaction_id),
            fallback_anchor_nsecs,
            fenced_epoch,
            "epoch-fenced expired initial A/V transaction before AO service"
        );
    }
    if let Some(transaction) = transaction {
        output_scheduler.abort_initial_audio_prepare(
            transaction.transaction_id,
            session_id,
            "initial_av_start_hard_deadline_external",
        );
    }
    if let Some(fallback_anchor_nsecs) = fallback_anchor_nsecs {
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_av_start_hard_deadline_external",
            fallback_anchor_nsecs,
        );
    } else {
        output_scheduler.fail_initial_av_start_transaction(
            control,
            session_id,
            "initial_av_pair_hard_deadline_external",
        );
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialVideoPublishResult {
    Published(usize),
    AlreadyPublished,
    WouldBlock,
    MissingAnchor,
    Interrupted,
}

#[allow(clippy::too_many_arguments)]
fn publish_initial_video_for_audio_commit(
    output_scheduler: &mut PlaybackOutputScheduler,
    transaction: super::InitialAvStartTransaction,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    scheduler: &mut PlaybackScheduler,
) -> InitialVideoPublishResult {
    if output_scheduler.first_frame_presented {
        return InitialVideoPublishResult::AlreadyPublished;
    }
    let Some((first_video_nsecs, _)) = output_scheduler.scheduled_video_queue.range_nsecs() else {
        return InitialVideoPublishResult::MissingAnchor;
    };
    if first_video_nsecs != transaction.video_anchor_nsecs
        || first_video_nsecs > transaction.audio_start_target_nsecs
    {
        return InitialVideoPublishResult::MissingAnchor;
    }

    let mut presented_video_frames = 0usize;
    while let Some((front_timeline_nsecs, _)) = output_scheduler.scheduled_video_queue.range_nsecs()
    {
        if front_timeline_nsecs > transaction.audio_start_target_nsecs {
            break;
        }
        if control.should_interrupt() {
            return InitialVideoPublishResult::Interrupted;
        }
        if !scheduler.ready_for(front_timeline_nsecs) {
            return InitialVideoPublishResult::WouldBlock;
        }

        let Some(frame) = output_scheduler.scheduled_video_queue.pop_front() else {
            break;
        };
        let retry_frame = QueuedVideoFrame {
            frame: frame.frame.clone(),
            timeline_nsecs: frame.timeline_nsecs,
            duration_nsecs: frame.duration_nsecs,
        };
        let timeline_nsecs = frame.timeline_nsecs;
        let duration_nsecs = frame.duration_nsecs;
        subtitle_pipeline.update_overlay(timeline_nsecs, session_id, event_tx);
        let admitted = present_video_frame_to_vo(
            frame.frame,
            timeline_nsecs,
            Some(timeline_nsecs.saturating_add(duration_nsecs)),
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            buffered_reporter,
        );
        if !admitted {
            output_scheduler
                .scheduled_video_queue
                .push_queued(retry_frame);
            output_scheduler.mark_first_frame_presentation_failed();
            return InitialVideoPublishResult::WouldBlock;
        }

        let first_presentation = !output_scheduler.first_frame_presented;
        let first_present_elapsed = output_scheduler.mark_first_frame_presented();
        if first_presentation {
            report_first_video_frame_presented(session_id, event_tx);
            tracing::debug!(
                session_id = ?session_id,
                first_present_ms = ?first_present_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1_000.0),
                first_video_timeline_nsecs = timeline_nsecs,
                transaction_id = transaction.transaction_id,
                "published first video frame after initial audio payload became ready"
            );
        }
        presented_video_frames = presented_video_frames.saturating_add(1);
    }

    if output_scheduler.first_frame_presented {
        InitialVideoPublishResult::Published(presented_video_frames)
    } else {
        InitialVideoPublishResult::MissingAnchor
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn abort_initial_av_start_for_discontinuity_change(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    transaction: super::InitialAvStartTransaction,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) -> bool {
    let current_seek_generation = control.seek_generation();
    let current_discontinuity_epoch = output_scheduler.discontinuity_epoch();
    if transaction.discontinuity_epoch == current_discontinuity_epoch
        && transaction.seek_generation == current_seek_generation
    {
        return false;
    }

    let mut restored_frames = 0usize;
    let fenced_epoch = if let Some(audio_epoch) = transaction.audio_prepare_epoch {
        match output.try_abort_staged_audio(audio_epoch, transaction.audio_start_target_nsecs) {
            Ok(Some(frames)) => {
                restored_frames = frames.len();
                output_scheduler
                    .pending_start_audio
                    .restore_staged_frames(frames);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                %error,
                "failed to restore staged audio during generation rollback"
            ),
        }
        output.audio_epoch()
    } else {
        output.fence_clock_without_wait(transaction.audio_start_target_nsecs)
    };
    output_scheduler.abort_initial_audio_prepare(
        transaction.transaction_id,
        session_id,
        "initial_av_start_discontinuity_changed",
    );
    let _ = event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::Diagnostic(BackendDiagnostic {
            code: "ffmpeg_initial_audio_prepare_aborted",
            message: format!(
                "transaction={} phase=aborted reason=discontinuity_changed discontinuity_epoch={}->{} seek_generation={}->{} fenced_epoch={} restored_frames={} target={}",
                transaction.transaction_id,
                transaction.discontinuity_epoch,
                current_discontinuity_epoch,
                transaction.seek_generation,
                current_seek_generation,
                fenced_epoch,
                restored_frames,
                transaction.audio_start_target_nsecs,
            ),
        }),
    ));
    true
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn service_initial_video_clock_until_audio_start(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    demux_cache: Option<&DemuxPacketCache>,
    delayed_audio_start_timeline_nsecs: u64,
    audio_decode_queued_nsecs: Option<u64>,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    current_start_position_nsecs: &mut u64,
    scheduler: &mut PlaybackScheduler,
) -> std::result::Result<OutputGateResumeStatus, String> {
    let now = Instant::now();
    let transaction = if let Some(transaction) = output_scheduler.initial_av_start_transaction() {
        transaction
    } else {
        let Some((first_video_timeline_nsecs, _)) =
            output_scheduler.scheduled_video_queue.range_nsecs()
        else {
            return Ok(OutputGateResumeStatus::Waiting);
        };
        let transaction = output_scheduler.begin_initial_av_start_transaction_for_generations(
            first_video_timeline_nsecs,
            delayed_audio_start_timeline_nsecs,
            control.seek_generation(),
            now,
        );
        if first_video_timeline_nsecs > *current_start_position_nsecs {
            *current_start_position_nsecs = first_video_timeline_nsecs;
            subtitle_pipeline.realign_cues_for_position(first_video_timeline_nsecs);
            buffered_reporter.reset_to(
                nsecs_to_seconds(first_video_timeline_nsecs),
                session_id,
                event_tx,
            );
        }
        scheduler.reset(first_video_timeline_nsecs);
        output_scheduler.mark_video_clock_anchor_valid();
        tracing::debug!(
            session_id = ?session_id,
            first_video_timeline_nsecs,
            delayed_audio_start_timeline_nsecs,
            delayed_audio_start_gap_ms = delayed_audio_start_timeline_nsecs
                .saturating_sub(first_video_timeline_nsecs) as f64
                / 1_000_000.0,
            initial_start_phase = "primed",
            first_frame_presented = false,
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            output_transition_deadline_ms = transaction
                .hard_deadline_at
                .saturating_duration_since(now)
                .as_secs_f64()
                * 1000.0,
            silence_fill_reason = "not_filled",
            clock_mode = AudioClockMode::SyncingVideo.as_str(),
            "starting video-clocked initial playback until first FFmpeg audio frame"
        );
        transaction
    };

    if abort_initial_av_start_for_discontinuity_change(
        output_scheduler,
        output,
        transaction,
        control,
        session_id,
        event_tx,
    ) {
        return Ok(OutputGateResumeStatus::Waiting);
    }

    // Keep the target frame retained until AO has a staged payload.  The
    // final commit publishes video first and activates AO immediately after,
    // so queue advancement can no longer alter the transaction's audio anchor.
    let mut presented_video_frames = 0usize;

    let now = Instant::now();
    let transaction = output_scheduler
        .initial_av_start_transaction()
        .unwrap_or(transaction);
    if control.should_interrupt() {
        return Ok(OutputGateResumeStatus::Waiting);
    }
    let cache_commit_available = !control.is_cache_paused() || demux_cache.is_some();
    let transaction_decision = transaction.decision(now, cache_commit_available);
    if transaction_decision == InitialAvStartDecision::Waiting {
        if now >= transaction.audio_start_due_at && !cache_commit_available {
            output_scheduler.wait_initial_audio_start_for_state_change(transaction.transaction_id);
            return Ok(OutputGateResumeStatus::WaitingForDemux);
        }
        tracing::trace!(
            session_id = ?session_id,
            presented_video_frames,
            delayed_audio_start_timeline_nsecs,
            initial_start_phase = "waiting_audio_deadline",
            first_frame_presented = transaction.first_frame_presented,
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            cache_commit_available,
            output_transition_deadline_ms = transaction
                .hard_deadline_at
                .saturating_duration_since(now)
                .as_secs_f64()
                * 1000.0,
            clock_mode = AudioClockMode::SyncingVideo.as_str(),
            "presented initial FFmpeg video frames without probing AO before AudioStartDue"
        );
        return Ok(OutputGateResumeStatus::Waiting);
    }
    let audio_snapshot = match output.stable_snapshot()? {
        AudioOutputStableSnapshot::Stable(snapshot) => snapshot,
        AudioOutputStableSnapshot::SnapshotUnstable(unstable) => {
            let (log_kind, repeated_observations) = match output_scheduler
                .observe_prestart_audio_ownership_log(
                    PrestartAudioOwnership::SnapshotUnstable,
                    transaction.transaction_id,
                    Instant::now(),
                ) {
                InitialSyncLogDecision::Changed { suppressed_repeats } => {
                    (Some("state_changed"), suppressed_repeats)
                }
                InitialSyncLogDecision::Summary {
                    repeated_observations,
                } => (Some("periodic_summary"), repeated_observations),
                InitialSyncLogDecision::Suppressed => (None, 0),
            };
            if let Some(log_kind) = log_kind {
                let hard_deadline_expired =
                    transaction_decision == InitialAvStartDecision::Rebuffer;
                tracing::warn!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    discontinuity_epoch = transaction.discontinuity_epoch,
                    seek_generation = transaction.seek_generation,
                    audio_epoch = unstable.audio_epoch,
                    stable_version = unstable.observed_version,
                    snapshot_attempts = unstable.attempts,
                    target_nsecs = transaction.audio_start_target_nsecs,
                    initial_audio_ownership = PrestartAudioOwnership::SnapshotUnstable.as_str(),
                    ownership_log_kind = log_kind,
                    repeated_observations,
                    hard_deadline_expired,
                    "observed unstable AO snapshot during initial audio transaction"
                );
                let _ = event_tx.send(BackendEvent::new(
                    session_id,
                    BackendEventKind::Diagnostic(BackendDiagnostic {
                        code: "ffmpeg_audio_snapshot_unstable",
                        message: format!(
                            "transaction={} phase={} epoch={} version={} target={} action={}",
                            transaction.transaction_id,
                            transaction.audio_prepare_phase.as_str(),
                            unstable.audio_epoch,
                            unstable.observed_version,
                            transaction.audio_start_target_nsecs,
                            if hard_deadline_expired {
                                "rebuffer"
                            } else {
                                "defer"
                            },
                        ),
                    }),
                ));
            }
            if transaction_decision == InitialAvStartDecision::Rebuffer {
                expire_initial_av_start_hard_deadline(
                    output_scheduler,
                    Some(output),
                    Instant::now(),
                    control,
                    session_id,
                );
                return Ok(OutputGateResumeStatus::Rebuffering);
            }
            output_scheduler.defer_initial_audio_start_retry(
                Instant::now(),
                InitialAudioTransientRetry::StableSnapshotUnstable,
            );
            return Ok(OutputGateResumeStatus::Waiting);
        }
    };
    let ownership = classify_prestart_audio_ownership(PrestartAudioOwnershipInput {
        phase: transaction.audio_prepare_phase,
        token: transaction.audio_prepare_token,
        current_audio_epoch: output.audio_epoch(),
        current_seek_generation: control.seek_generation(),
        target_nsecs: transaction.audio_start_target_nsecs,
        snapshot: AudioOutputStableSnapshot::Stable(audio_snapshot),
    });
    if matches!(
        ownership,
        PrestartAudioOwnership::StaleEpoch | PrestartAudioOwnership::UnexpectedCurrentEpoch
    ) {
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            initial_audio_phase = transaction.audio_prepare_phase.as_str(),
            initial_audio_ownership = ownership.as_str(),
            discontinuity_epoch = transaction.discontinuity_epoch,
            seek_generation = transaction.seek_generation,
            audio_epoch = audio_snapshot.audio_epoch,
            queue_generation = audio_snapshot.queue_generation,
            stable_version = ?audio_snapshot.stable_version,
            shared_payload_ms = audio_snapshot.shared_payload_nsecs as f64 / 1_000_000.0,
            driver_delay_ms = audio_snapshot.driver_delay_nsecs as f64 / 1_000_000.0,
            queue_ms = audio_snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
            worker_in_flight_ms = audio_snapshot.worker_in_flight_nsecs as f64 / 1_000_000.0,
            prepared_range = ?audio_snapshot.payload_range_nsecs,
            pending_range = ?output_scheduler.pending_start_audio.range_nsecs(),
            target_nsecs = transaction.audio_start_target_nsecs,
            "recovering unexpected pre-start AO ownership without panicking"
        );
        let _ = event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::Diagnostic(BackendDiagnostic {
                code: "ffmpeg_prestart_audio_ownership_recovered",
                message: format!(
                    "transaction={} phase={} ownership={} epoch={} target={}",
                    transaction.transaction_id,
                    transaction.audio_prepare_phase.as_str(),
                    ownership.as_str(),
                    audio_snapshot.audio_epoch,
                    transaction.audio_start_target_nsecs,
                ),
            }),
        ));
        if let Some(recovered) = output.try_abort_staged_audio(
            audio_snapshot.audio_epoch,
            transaction.audio_start_target_nsecs,
        )? {
            output_scheduler
                .pending_start_audio
                .restore_staged_frames(recovered);
        }
        output_scheduler.abort_initial_audio_prepare(
            transaction.transaction_id,
            session_id,
            ownership.as_str(),
        );
        return Ok(OutputGateResumeStatus::Waiting);
    }
    let ammunition = InitialAudioAmmunitionSnapshot::from_optional_ledgers(
        output_scheduler,
        Some(audio_snapshot),
        audio_decode_queued_nsecs,
        delayed_audio_start_timeline_nsecs,
    );
    debug_assert_eq!(
        ammunition.covers_target(),
        initial_audio_start_ammunition_ready(
            output_scheduler,
            Some(audio_snapshot),
            delayed_audio_start_timeline_nsecs,
        )
    );
    output_scheduler.refresh_initial_bounded_delayed_audio_start_plan();
    let Some(retention_plan) = output_scheduler.pending_audio_retention_plan() else {
        tracing::error!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            output_state = ?output_scheduler.playback_output_state,
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            "active initial audio transaction has no immutable retention plan"
        );
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_audio_retention_plan_missing",
            transaction.audio_start_target_nsecs,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    };
    if retention_plan.source != PendingAudioRetentionAnchorSource::InitialTransaction
        || retention_plan.anchor_timeline_nsecs != transaction.audio_start_target_nsecs
    {
        tracing::error!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            retention_anchor_nsecs = retention_plan.anchor_timeline_nsecs,
            retention_anchor_source = retention_plan.source.as_str(),
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            "active initial audio transaction changed its immutable retention anchor"
        );
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_audio_retention_anchor_changed",
            transaction.audio_start_target_nsecs,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    }
    if transaction_decision == InitialAvStartDecision::Commit
        && !ammunition.covers_target()
        && let Some((delayed_start_nsecs, delay_nsecs)) =
            output_scheduler.unbounded_delayed_audio_start_for_retention_plan(retention_plan)
    {
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            delayed_start_nsecs,
            delay_ms = delay_nsecs as f64 / 1_000_000.0,
            allowed_delay_ms = VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE.as_secs_f64() * 1_000.0,
            "initial audio delay exceeded the bounded staging policy; rebuffering immediately"
        );
        let _ = event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::Diagnostic(BackendDiagnostic {
                code: "ffmpeg_initial_audio_delay_exceeds_bound",
                message: format!(
                    "transaction={} target={} delayed_start={} delay_nsecs={} action=rebuffer",
                    transaction.transaction_id,
                    transaction.audio_start_target_nsecs,
                    delayed_start_nsecs,
                    delay_nsecs,
                ),
            }),
        ));
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_audio_delay_exceeds_bound",
            transaction.audio_start_target_nsecs,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    }
    let start_action = initial_audio_start_action(transaction_decision, ammunition);
    let degraded_commit = match start_action {
        InitialAudioStartAction::FailNoAmmunition => {
            tracing::warn!(
                session_id = ?session_id,
                delayed_audio_start_timeline_nsecs,
                pending_audio_ms = ammunition.pending_audio_nsecs as f64 / 1_000_000.0,
                pending_audio_range_nsecs = ?ammunition.pending_audio_range_nsecs,
                pending_audio_contiguous_range_nsecs =
                    ?ammunition.pending_audio_contiguous_range_nsecs,
                device_audio_ms = ammunition.device_audio_nsecs as f64 / 1_000_000.0,
                device_audio_range_nsecs = ?ammunition.device_audio_range_nsecs,
                audio_decode_queued_ms = ammunition.decoded_audio_nsecs as f64 / 1_000_000.0,
                audio_decode_estimated_range_nsecs =
                    ?ammunition.decoded_audio_estimated_range_nsecs,
                total_audio_ms = ammunition.total_audio_nsecs as f64 / 1_000_000.0,
                ammunition_threshold_ms =
                    duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION) as f64 / 1_000_000.0,
                first_retained_video_timeline_nsecs = ?output_scheduler
                    .scheduled_video_queue
                    .range_nsecs()
                    .map(|(start, _)| start),
                "initial A/V start hard deadline found no usable audio ammunition"
            );
            expire_initial_av_start_hard_deadline(
                output_scheduler,
                Some(output),
                Instant::now(),
                control,
                session_id,
            );
            return Ok(OutputGateResumeStatus::Rebuffering);
        }
        InitialAudioStartAction::WaitingForTransaction => {
            return Ok(OutputGateResumeStatus::Waiting);
        }
        InitialAudioStartAction::WaitingForCompleteLedger => {
            tracing::trace!(
                session_id = ?session_id,
                delayed_audio_start_timeline_nsecs,
                pending_audio_ms = ammunition.pending_audio_nsecs as f64 / 1_000_000.0,
                device_audio_ms = ammunition.device_audio_nsecs as f64 / 1_000_000.0,
                audio_decode_ledger_observed = false,
                total_audio_ms = ammunition.total_audio_nsecs as f64 / 1_000_000.0,
                "deferred initial A/V start decision until all audio ledgers are observed"
            );
            output_scheduler.wait_initial_audio_start_for_state_change(transaction.transaction_id);
            return Ok(OutputGateResumeStatus::WaitingForDecodedAudio);
        }
        InitialAudioStartAction::DeferBelowThreshold => {
            control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
            log_initial_audio_start_defer(
                output_scheduler,
                ammunition,
                presented_video_frames,
                session_id,
                now,
            );
            output_scheduler.wait_initial_audio_start_for_state_change(transaction.transaction_id);
            return Ok(OutputGateResumeStatus::WaitingForDecodedAudio);
        }
        InitialAudioStartAction::CommitCovered => false,
        InitialAudioStartAction::CommitDegraded => true,
    };

    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    if retention_plan.source != PendingAudioRetentionAnchorSource::InitialTransaction
        || !output_scheduler.initial_audio_retention_invariant_satisfied(retention_plan)
    {
        tracing::error!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            retention_anchor_nsecs = retention_plan.anchor_timeline_nsecs,
            retention_anchor_source = retention_plan.source.as_str(),
            audio_start_target_nsecs = transaction.audio_start_target_nsecs,
            pending_audio_range_nsecs = ?output_scheduler.pending_start_audio.range_nsecs(),
            prepared_range_nsecs = ?transaction
                .audio_prepare_token
                .map(|token| token.staged_range_nsecs),
            "active initial audio transaction violated payload retention invariant"
        );
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_audio_retention_invariant_violated",
            transaction.audio_start_target_nsecs,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    }

    if ownership == PrestartAudioOwnership::PreparedCurrentEpoch {
        let Some(token) = transaction.audio_prepare_token else {
            return Ok(OutputGateResumeStatus::Waiting);
        };
        let mut stage_guard =
            InitialAudioStageGuard::new(output_scheduler, output, token, session_id, event_tx);
        if Instant::now() >= transaction.hard_deadline_at {
            stage_guard.preserve_for_terminal_cleanup("hard_deadline_before_prepared_commit");
            drop(stage_guard);
            expire_initial_av_start_hard_deadline(
                output_scheduler,
                Some(output),
                Instant::now(),
                control,
                session_id,
            );
            return Ok(OutputGateResumeStatus::Rebuffering);
        }
        match publish_initial_video_for_audio_commit(
            stage_guard.scheduler_mut(),
            transaction,
            control,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
            scheduler,
        ) {
            InitialVideoPublishResult::Published(count) => {
                presented_video_frames = presented_video_frames.saturating_add(count);
            }
            InitialVideoPublishResult::AlreadyPublished => {}
            InitialVideoPublishResult::WouldBlock => {
                stage_guard.preserve_for_retry(InitialAudioTransientRetry::VideoOutputWouldBlock);
                return Ok(OutputGateResumeStatus::Waiting);
            }
            InitialVideoPublishResult::Interrupted => {
                stage_guard.abort("initial_video_publish_interrupted");
                return Ok(OutputGateResumeStatus::Waiting);
            }
            InitialVideoPublishResult::MissingAnchor => {
                stage_guard.abort("initial_video_publish_missing_anchor");
                drop(stage_guard);
                output_scheduler.fail_initial_av_start_transaction_at_anchor(
                    control,
                    session_id,
                    "initial_video_publish_missing_anchor",
                    transaction.audio_start_target_nsecs,
                );
                return Ok(OutputGateResumeStatus::Rebuffering);
            }
        }
        if !stage_guard.commit(control) {
            stage_guard.abort("prepared_audio_commit_interrupted");
            return Ok(OutputGateResumeStatus::Waiting);
        }
        tracing::debug!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            presented_video_frames,
            "atomically published initial video and activated prepared audio"
        );
        drop(stage_guard);
        return Ok(OutputGateResumeStatus::Resumed);
    }

    if control.should_interrupt() {
        return Ok(OutputGateResumeStatus::Waiting);
    }

    let bounded_delayed_audio_start_nsecs =
        output_scheduler.bounded_delayed_audio_start_for_retention_plan(retention_plan);
    let delayed_start_silence_policy = if bounded_delayed_audio_start_nsecs.is_some() {
        DelayedAudioStartSilencePolicy::Allow
    } else {
        DelayedAudioStartSilencePolicy::Skip
    };

    // Seek/reset already established the current AO epoch. Repeating reset
    // here races the callback and invalidates the very payload being staged.
    let reset_audio_clock = false;
    output.deactivate();
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    if control.should_interrupt() {
        return Ok(OutputGateResumeStatus::Waiting);
    }

    let audio_epoch = output.audio_epoch();
    if !output_scheduler.begin_initial_audio_prepare(transaction.transaction_id, audio_epoch) {
        return Ok(OutputGateResumeStatus::Waiting);
    }
    let preparing_token = InitialAudioPrepareToken {
        transaction_id: transaction.transaction_id,
        discontinuity_epoch: transaction.discontinuity_epoch,
        seek_generation: transaction.seek_generation,
        audio_epoch,
        target_nsecs: transaction.audio_start_target_nsecs,
        staged_range_nsecs: (
            transaction.audio_start_target_nsecs,
            transaction.audio_start_target_nsecs,
        ),
        staged_frames: 0,
        staged_samples: 0,
        staged_until_nsecs: transaction.audio_start_target_nsecs,
    };
    let mut stage_guard = InitialAudioStageGuard::new(
        output_scheduler,
        output,
        preparing_token,
        session_id,
        event_tx,
    );

    let audio_flush_start_timeline_nsecs = retention_plan.anchor_timeline_nsecs;
    let audio_flush_until_timeline_nsecs = stage_guard
        .scheduler()
        .scheduled_video_queue
        .buffered_until_from_nsecs(audio_flush_start_timeline_nsecs)
        .or_else(|| {
            stage_guard
                .scheduler()
                .pending_start_audio
                .buffered_until_from(audio_flush_start_timeline_nsecs)
        })
        .or_else(|| {
            bounded_delayed_audio_start_nsecs.and_then(|delayed_start_nsecs| {
                stage_guard
                    .scheduler()
                    .pending_start_audio
                    .buffered_until_from(delayed_start_nsecs)
            })
        })
        .unwrap_or(audio_flush_start_timeline_nsecs);
    let stage_attempt = {
        let _stage = output.begin_service_stage(AudioOutputServiceStage::StagePending);
        let scheduler = stage_guard.scheduler_mut();
        stage_pending_audio(
            &mut scheduler.pending_start_audio,
            output,
            audio_epoch,
            audio_flush_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::AudioStarted,
            delayed_start_silence_policy,
            control,
            &mut scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )
    };
    let stage_result = match stage_attempt {
        Ok(result) => result,
        Err(error) => {
            stage_guard.abort("audio_stage_error");
            return Err(error);
        }
    };
    if control.should_interrupt() || stage_result.interrupted {
        stage_guard.abort("audio_stage_interrupted");
        return Ok(OutputGateResumeStatus::Waiting);
    }
    let Some(staged_range_nsecs) = stage_result.staged_range_nsecs else {
        if initial_audio_no_payload_disposition(stage_result.would_block)
            == InitialAudioNoPayloadDisposition::RetryTransient
        {
            stage_guard.preserve_for_retry(InitialAudioTransientRetry::AudioStageWouldBlock);
            return Ok(OutputGateResumeStatus::Waiting);
        }
        tracing::error!(
            session_id = ?session_id,
            transaction_id = transaction.transaction_id,
            retention_anchor_nsecs = retention_plan.anchor_timeline_nsecs,
            retention_anchor_source = retention_plan.source.as_str(),
            bounded_delayed_audio_start_nsecs,
            pending_audio_range_nsecs = ?stage_guard
                .scheduler()
                .pending_start_audio
                .range_nsecs(),
            audio_flush_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            "initial audio staging produced no payload from a non-transient input state"
        );
        let _ = event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::Diagnostic(BackendDiagnostic {
                code: "ffmpeg_initial_audio_stage_no_payload_terminal",
                message: format!(
                    "transaction={} target={} pending={:?} action=rebuffer",
                    transaction.transaction_id,
                    transaction.audio_start_target_nsecs,
                    stage_guard.scheduler().pending_start_audio.range_nsecs(),
                ),
            }),
        ));
        stage_guard.abort("initial_audio_stage_no_payload_terminal");
        drop(stage_guard);
        output_scheduler.fail_initial_av_start_transaction_at_anchor(
            control,
            session_id,
            "initial_audio_stage_no_payload_terminal",
            transaction.audio_start_target_nsecs,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    };
    let token = InitialAudioPrepareToken {
        staged_until_nsecs: staged_range_nsecs.1,
        staged_range_nsecs,
        staged_frames: stage_result.staged_frames,
        staged_samples: stage_result.staged_samples,
        ..preparing_token
    };
    stage_guard.set_token(token);
    if !stage_guard
        .scheduler_mut()
        .finish_initial_audio_prepare(token)
    {
        stage_guard.abort("prepared_token_rejected");
        return Ok(OutputGateResumeStatus::Waiting);
    }

    let started_audio_snapshot = match output.prepared_snapshot() {
        Ok(AudioOutputStableSnapshot::Stable(snapshot)) => snapshot,
        Ok(AudioOutputStableSnapshot::SnapshotUnstable(_)) => {
            stage_guard.preserve_for_retry(InitialAudioTransientRetry::PreparedSnapshotUnstable);
            return Ok(OutputGateResumeStatus::Waiting);
        }
        Err(error) => {
            stage_guard.abort("prepared_snapshot_error");
            return Err(error);
        }
    };
    let prepared_ownership = classify_prestart_audio_ownership(PrestartAudioOwnershipInput {
        phase: InitialAudioPreparePhase::Prepared,
        token: Some(token),
        current_audio_epoch: output.audio_epoch(),
        current_seek_generation: control.seek_generation(),
        target_nsecs: transaction.audio_start_target_nsecs,
        snapshot: AudioOutputStableSnapshot::Stable(started_audio_snapshot),
    });
    if prepared_ownership != PrestartAudioOwnership::PreparedCurrentEpoch {
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = token.transaction_id,
            initial_audio_phase = InitialAudioPreparePhase::Prepared.as_str(),
            initial_audio_ownership = prepared_ownership.as_str(),
            discontinuity_epoch = token.discontinuity_epoch,
            seek_generation = token.seek_generation,
            audio_epoch = started_audio_snapshot.audio_epoch,
            queue_generation = started_audio_snapshot.queue_generation,
            stable_version = ?started_audio_snapshot.stable_version,
            shared_payload_ms = started_audio_snapshot.shared_payload_nsecs as f64 / 1_000_000.0,
            driver_delay_ms = started_audio_snapshot.driver_delay_nsecs as f64 / 1_000_000.0,
            queue_ms = started_audio_snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
            worker_in_flight_ms = started_audio_snapshot.worker_in_flight_nsecs as f64 / 1_000_000.0,
            prepared_range = ?started_audio_snapshot.payload_range_nsecs,
            pending_range = ?stage_guard.scheduler().pending_start_audio.range_nsecs(),
            target_nsecs = token.target_nsecs,
            "aborting invalid prepared initial audio ownership without panicking"
        );
        let rebuffer_at_original_target = !token.covers_target();
        stage_guard.abort(if !rebuffer_at_original_target {
            prepared_ownership.as_str()
        } else {
            "prepared_audio_does_not_cover_seek_target"
        });
        if rebuffer_at_original_target {
            // Publish Prepared -> Aborted and restore pending ownership before
            // entering the bounded fallback at the original seek target.
            drop(stage_guard);
            output_scheduler.fail_initial_av_start_transaction_at_anchor(
                control,
                session_id,
                "prepared_audio_does_not_cover_seek_target",
                token.target_nsecs,
            );
        }
        return Ok(OutputGateResumeStatus::Waiting);
    }
    if control.should_interrupt() {
        stage_guard.abort("interrupt_before_scheduler_commit");
        return Ok(OutputGateResumeStatus::Waiting);
    }
    if Instant::now() >= transaction.hard_deadline_at {
        stage_guard.preserve_for_terminal_cleanup("hard_deadline_before_scheduler_commit");
        drop(stage_guard);
        expire_initial_av_start_hard_deadline(
            output_scheduler,
            Some(output),
            Instant::now(),
            control,
            session_id,
        );
        return Ok(OutputGateResumeStatus::Rebuffering);
    }
    match publish_initial_video_for_audio_commit(
        stage_guard.scheduler_mut(),
        transaction,
        control,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
        scheduler,
    ) {
        InitialVideoPublishResult::Published(count) => {
            presented_video_frames = presented_video_frames.saturating_add(count);
        }
        InitialVideoPublishResult::AlreadyPublished => {}
        InitialVideoPublishResult::WouldBlock => {
            stage_guard.preserve_for_retry(InitialAudioTransientRetry::VideoOutputWouldBlock);
            return Ok(OutputGateResumeStatus::Waiting);
        }
        InitialVideoPublishResult::Interrupted => {
            stage_guard.abort("initial_video_publish_interrupted");
            return Ok(OutputGateResumeStatus::Waiting);
        }
        InitialVideoPublishResult::MissingAnchor => {
            stage_guard.abort("initial_video_publish_missing_anchor");
            drop(stage_guard);
            output_scheduler.fail_initial_av_start_transaction_at_anchor(
                control,
                session_id,
                "initial_video_publish_missing_anchor",
                transaction.audio_start_target_nsecs,
            );
            return Ok(OutputGateResumeStatus::Rebuffering);
        }
    }
    if control.is_cache_paused()
        && let Some(demux_cache) = demux_cache
    {
        demux_cache.clear_cache_pause_for_decoded_resume();
    }
    if !stage_guard.commit(control) {
        return Ok(OutputGateResumeStatus::Waiting);
    }
    tracing::debug!(
        session_id = ?session_id,
        transaction_id = token.transaction_id,
        presented_video_frames,
        delayed_audio_start_timeline_nsecs,
        audio_flush_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        degraded_commit,
        reset_audio_clock,
        audio_epoch = token.audio_epoch,
        stable_version = ?started_audio_snapshot.stable_version,
        shared_payload_ms = started_audio_snapshot.shared_payload_nsecs as f64 / 1_000_000.0,
        driver_delay_ms = started_audio_snapshot.driver_delay_nsecs as f64 / 1_000_000.0,
        queue_ms = started_audio_snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        worker_in_flight_ms = started_audio_snapshot.worker_in_flight_nsecs as f64 / 1_000_000.0,
        prepared_range = ?started_audio_snapshot.payload_range_nsecs,
        staged_frames = token.staged_frames,
        staged_samples = token.staged_samples,
        pending_audio_frames = stage_guard.scheduler().pending_start_audio.len(),
        pending_audio_ms = stage_guard
            .scheduler()
            .pending_start_audio
            .buffered_duration()
            .as_secs_f64()
            * 1000.0,
        initial_start_phase = "playing",
        first_frame_presented = stage_guard.scheduler().first_frame_presented,
        audio_start_target_nsecs = transaction.audio_start_target_nsecs,
        cache_pause_cleared = demux_cache.is_some(),
        audio_output_lifecycle = AudioOutputLifecycle::Playing.as_str(),
        clock_mode = AudioClockMode::AudioStarted.as_str(),
        "started native audio output after atomic initial A/V commit"
    );
    drop(stage_guard);
    Ok(OutputGateResumeStatus::Resumed)
}
