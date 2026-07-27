use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_QUEUE_LIMIT_DURATION,
    AUDIO_OUTPUT_STEADY_TARGET_DURATION, AUDIO_OUTPUT_VIDEO_LEAD_DURATION,
    AUDIO_REBUFFER_PREFILL_LOOP_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AtomicBool,
    AudioClockMode, AudioOutput, AudioOutputSnapshot, AudioResumeWaterline,
    AudioResumeWaterlineInput, BackendEvent, BufferedReporter, DecodedAudio,
    DelayedAudioStartSilencePolicy, Duration, FfmpegControl, Instant,
    PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION, PLAYING_PENDING_AUDIO_HARD_RESET_DURATION,
    PendingAudioBackpressureLogState, PendingAudioRetentionAnchorSource, PendingAudioRetentionPlan,
    PendingStartAudioPressureLevel, PlaybackOutputScheduler, PlaybackOutputState,
    PlaybackSessionId, PositionReporter, SubtitlePipeline, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VideoOutputQueue, audio_output_buffered_until_for_resume,
    duration_nsecs, enter_video_output_rebuffer, flush_pending_start_audio,
    pending_audio_underrun_recovery_plan, push_decoded_audio_to_output,
    recover_pending_start_audio_after_underrun,
};
use super::{PENDING_START_AUDIO_BACKPRESSURE_DURATION, Sender};

// AAC commonly advances in 1024-sample frames (about 21.3ms at 48kHz). Give
// the warning edge one whole decoded-frame of entry hysteresis so a single
// 850ms -> 859ms admission overshoot does not generate a warning per session.
const PLAYING_PENDING_AUDIO_WARN_ENTRY_FRAME_TOLERANCE: Duration = Duration::from_millis(24);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum PendingAudioPressureContext
{
    StartupSync,
    RebufferResume,
    PlayingSteady,
}

impl PendingAudioPressureContext {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn as_str(
        self,
    ) -> &'static str {
        match self {
            Self::StartupSync => "startup_sync",
            Self::RebufferResume => "rebuffer_resume",
            Self::PlayingSteady => "playing_steady",
        }
    }
}

impl PendingStartAudioPressureLevel {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn from_duration(
        duration: Duration,
    ) -> Self {
        if duration >= PLAYING_PENDING_AUDIO_HARD_RESET_DURATION {
            Self::HardReset
        } else if duration >= PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION {
            Self::ForceRecovery
        } else if duration >= playing_pending_audio_warn_entry_duration() {
            Self::Warn
        } else {
            Self::Normal
        }
    }

    fn threshold(self) -> Duration {
        match self {
            Self::Normal => Duration::ZERO,
            Self::Warn => playing_pending_audio_warn_entry_duration(),
            Self::ForceRecovery => PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
            Self::HardReset => PLAYING_PENDING_AUDIO_HARD_RESET_DURATION,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::ForceRecovery => "force_recovery",
            Self::HardReset => "hard_reset",
        }
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_output_contiguous_start_timeline_nsecs(
    snapshot: AudioOutputSnapshot,
) -> u64 {
    if snapshot.total_pending_nsecs > 0 {
        snapshot.buffered_until_timeline_nsecs
    } else {
        snapshot.played_timeline_nsecs
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn playing_pending_audio_limit_duration()
-> Duration {
    AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn playing_pending_audio_warn_entry_duration()
-> Duration {
    playing_pending_audio_limit_duration()
        .saturating_add(PLAYING_PENDING_AUDIO_WARN_ENTRY_FRAME_TOLERANCE)
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn playing_pending_audio_pressure_clear_duration()
-> Duration {
    playing_pending_audio_limit_duration().saturating_sub(Duration::from_millis(100))
}

pub(in crate::player::backend::ffmpeg::playback_loop) enum DecodedAudioAdmission {
    Accepted,
    AcceptedAndBackpressured,
    Deferred(DecodedAudio),
}

fn startup_pending_audio_backpressure_duration() -> Duration {
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION
        .min(PENDING_START_AUDIO_BACKPRESSURE_DURATION)
        .min(
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
                .saturating_sub(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN),
        )
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_output_flush_until_timeline_nsecs(
    snapshot: AudioOutputSnapshot,
    video_lead_until_timeline_nsecs: u64,
    target_duration: Duration,
) -> u64 {
    let max_audio_until_nsecs = snapshot
        .played_timeline_nsecs
        .saturating_add(duration_nsecs(target_duration));
    video_lead_until_timeline_nsecs.min(max_audio_until_nsecs)
}

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn pending_audio_retention_plan(
        &self,
    ) -> Option<PendingAudioRetentionPlan> {
        match self.playback_output_state {
            PlaybackOutputState::Primed => {
                self.initial_av_start_transaction()
                    .map(|transaction| PendingAudioRetentionPlan {
                        anchor_timeline_nsecs: transaction.audio_start_target_nsecs,
                        source: PendingAudioRetentionAnchorSource::InitialTransaction,
                    })
            }
            PlaybackOutputState::Rebuffering => {
                self.video_output_rebuffer_anchor
                    .map(|anchor| PendingAudioRetentionPlan {
                        anchor_timeline_nsecs: anchor.timeline_nsecs,
                        source: PendingAudioRetentionAnchorSource::Rebuffer,
                    })
            }
            PlaybackOutputState::Syncing if !self.first_frame_presented => self
                .scheduled_video_queue
                .range_nsecs()
                .map(|(first_video_nsecs, _)| PendingAudioRetentionPlan {
                    anchor_timeline_nsecs: first_video_nsecs,
                    source: PendingAudioRetentionAnchorSource::UnpresentedVideo,
                }),
            PlaybackOutputState::Syncing | PlaybackOutputState::Playing => None,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn bounded_delayed_audio_start_for_retention_plan(
        &self,
        plan: PendingAudioRetentionPlan,
    ) -> Option<u64> {
        if plan.source == PendingAudioRetentionAnchorSource::InitialTransaction
            && let Some(transaction) = self.initial_av_start_transaction()
        {
            let delayed_start_nsecs = transaction.committed_bounded_delayed_audio_start_nsecs?;
            let delay_nsecs = delayed_start_nsecs.saturating_sub(plan.anchor_timeline_nsecs);
            return (plan.anchor_timeline_nsecs == transaction.audio_start_target_nsecs
                && delay_nsecs > duration_nsecs(super::PENDING_AUDIO_CONTINUITY_TOLERANCE)
                && delay_nsecs <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
                && self
                    .pending_start_audio
                    .buffered_until_from(delayed_start_nsecs)
                    .is_some_and(|until| until > delayed_start_nsecs))
            .then_some(delayed_start_nsecs);
        }
        self.candidate_bounded_delayed_audio_start_for_retention_plan(plan)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn candidate_bounded_delayed_audio_start_for_retention_plan(
        &self,
        plan: PendingAudioRetentionPlan,
    ) -> Option<u64> {
        if self
            .pending_start_audio
            .buffered_until_from(plan.anchor_timeline_nsecs)
            .is_some_and(|until| until > plan.anchor_timeline_nsecs)
        {
            return None;
        }
        let delayed_start_nsecs = self
            .pending_start_audio
            .first_start_at_or_after(plan.anchor_timeline_nsecs)?;
        let delay_nsecs = delayed_start_nsecs.saturating_sub(plan.anchor_timeline_nsecs);
        (delay_nsecs > duration_nsecs(super::PENDING_AUDIO_CONTINUITY_TOLERANCE)
            && delay_nsecs <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE))
        .then_some(delayed_start_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn refresh_initial_bounded_delayed_audio_start_plan(
        &mut self,
    ) -> Option<u64> {
        let transaction = self.initial_av_start_transaction()?;
        let plan = PendingAudioRetentionPlan {
            anchor_timeline_nsecs: transaction.audio_start_target_nsecs,
            source: PendingAudioRetentionAnchorSource::InitialTransaction,
        };
        let delayed_start_nsecs =
            self.candidate_bounded_delayed_audio_start_for_retention_plan(plan);
        if let Some(transaction) = self.initial_av_start_transaction.as_mut() {
            transaction.committed_bounded_delayed_audio_start_nsecs = delayed_start_nsecs;
        }
        delayed_start_nsecs
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn unbounded_delayed_audio_start_for_retention_plan(
        &self,
        plan: PendingAudioRetentionPlan,
    ) -> Option<(u64, u64)> {
        if self
            .pending_start_audio
            .buffered_until_from(plan.anchor_timeline_nsecs)
            .is_some_and(|until| until > plan.anchor_timeline_nsecs)
        {
            return None;
        }
        let delayed_start_nsecs = self
            .pending_start_audio
            .first_start_at_or_after(plan.anchor_timeline_nsecs)?;
        let delay_nsecs = delayed_start_nsecs.saturating_sub(plan.anchor_timeline_nsecs);
        (delay_nsecs > duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE))
            .then_some((delayed_start_nsecs, delay_nsecs))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_audio_retention_invariant_satisfied(
        &self,
        plan: PendingAudioRetentionPlan,
    ) -> bool {
        let Some(transaction) = self.initial_av_start_transaction() else {
            return false;
        };
        if plan.source != PendingAudioRetentionAnchorSource::InitialTransaction
            || plan.anchor_timeline_nsecs != transaction.audio_start_target_nsecs
        {
            return false;
        }
        transaction.audio_prepare_token.is_some_and(|token| {
            token.transaction_id == transaction.transaction_id
                && token.discontinuity_epoch == transaction.discontinuity_epoch
                && token.seek_generation == transaction.seek_generation
                && token.target_nsecs == transaction.audio_start_target_nsecs
                && token.covers_target()
        }) || self
            .pending_start_audio
            .buffered_until_from(transaction.audio_start_target_nsecs)
            .is_some_and(|until| until > transaction.audio_start_target_nsecs)
            || self
                .bounded_delayed_audio_start_for_retention_plan(plan)
                .is_some()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn trim_pending_audio_to_retention_plan(
        &mut self,
        plan: PendingAudioRetentionPlan,
        sample_rate: i32,
        channels: i32,
        session_id: PlaybackSessionId,
    ) -> usize {
        if let Some(transaction) = self.initial_av_start_transaction()
            && (plan.source != PendingAudioRetentionAnchorSource::InitialTransaction
                || plan.anchor_timeline_nsecs != transaction.audio_start_target_nsecs)
        {
            tracing::error!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                transaction_audio_target_nsecs = transaction.audio_start_target_nsecs,
                rejected_trim_anchor_nsecs = plan.anchor_timeline_nsecs,
                retention_anchor_source = plan.source.as_str(),
                pending_audio_range_nsecs = ?self.pending_start_audio.range_nsecs(),
                "rejected pending audio trim that would move past the initial transaction target"
            );
            return 0;
        }

        self.pending_start_audio
            .trim_before(plan.anchor_timeline_nsecs, sample_rate, channels)
    }

    pub(in crate::player::backend::ffmpeg) fn waiting_for_output_resume(&self) -> bool {
        self.restart_pending() || self.playback_output_state.rebuffering()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_output_steady_target_duration(
        &self,
        _audio_snapshot: AudioOutputSnapshot,
    ) -> Duration {
        if self.audio_rebuffer_loop_active() {
            return AUDIO_REBUFFER_PREFILL_LOOP_TARGET.min(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION);
        }
        AUDIO_OUTPUT_STEADY_TARGET_DURATION.min(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION)
    }

    pub(in crate::player::backend::ffmpeg) fn pending_start_audio_backpressured(&self) -> bool {
        if self.restart_pending() || self.startup_pending_audio_pressure_context_active {
            return self.pending_start_audio.contiguous_duration()
                >= startup_pending_audio_backpressure_duration();
        }
        let buffered_duration = self.pending_start_audio.buffered_duration();
        if self.playback_output_state == PlaybackOutputState::Playing {
            return buffered_duration >= playing_pending_audio_limit_duration();
        }
        if buffered_duration < PENDING_START_AUDIO_BACKPRESSURE_DURATION {
            return false;
        }
        !self.restart_pending() || !self.scheduled_video_queue.is_empty()
    }

    pub(in crate::player::backend::ffmpeg) fn output_wait_audio_input_backpressured(&self) -> bool {
        if self.decode_recovery_active() {
            return false;
        }
        if !self.waiting_for_output_resume() {
            return false;
        }
        let Some(retention_plan) = self.pending_audio_retention_plan() else {
            return false;
        };
        let resume_reference_nsecs = Some(retention_plan.anchor_timeline_nsecs);
        let effective_contiguous_coverage_nsecs =
            if let Some(resume_reference_nsecs) = resume_reference_nsecs {
                self.pending_start_audio
                    .forward_duration_from(resume_reference_nsecs)
                    .or_else(|| {
                        let delayed_start_nsecs =
                            self.bounded_delayed_audio_start_for_retention_plan(retention_plan)?;
                        self.pending_start_audio
                            .forward_duration_from(delayed_start_nsecs)
                    })
                    .unwrap_or_default()
            } else {
                self.pending_start_audio
                    .contiguous_range_nsecs()
                    .map(|(start, end)| end.saturating_sub(start))
                    .unwrap_or_default()
            };
        let suppression_threshold_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
            .saturating_add(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        effective_contiguous_coverage_nsecs >= suppression_threshold_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn pending_audio_contiguous_range_nsecs(
        &self,
    ) -> Option<(u64, u64)> {
        self.pending_start_audio.contiguous_range_nsecs()
    }

    pub(in crate::player::backend::ffmpeg) fn pending_audio_timeline_gap_near(
        &self,
        initial_previous_end_nsecs: Option<u64>,
        expected_previous_end_nsecs: u64,
        expected_next_start_nsecs: u64,
        min_gap_nsecs: u64,
        endpoint_tolerance_nsecs: u64,
    ) -> Option<(u64, u64)> {
        self.pending_start_audio.timeline_gap_near(
            initial_previous_end_nsecs,
            expected_previous_end_nsecs,
            expected_next_start_nsecs,
            min_gap_nsecs,
            endpoint_tolerance_nsecs,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn audio_resume_waterline_for_output_wait(
        &self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        audio_decode_queued_nsecs: u64,
        audio_decode_in_flight_packets: usize,
        current_start_position_nsecs: u64,
        target_nsecs: u64,
        demux_audio_forward_nsecs: Option<u64>,
        demux_audio_cached_packets: Option<usize>,
    ) -> Option<AudioResumeWaterline> {
        if !self.waiting_for_output_resume() {
            return None;
        }

        let retention_plan = self.pending_audio_retention_plan()?;
        let previous_audio_played_until = audio_snapshot
            .map(|snapshot| snapshot.played_timeline_nsecs)
            .unwrap_or(current_start_position_nsecs);
        let audio_output_buffered_until_nsecs = if self.playback_output_state.rebuffering() {
            audio_snapshot
                .filter(|snapshot| snapshot.total_pending_nsecs > 0)
                .map(|snapshot| snapshot.buffered_until_timeline_nsecs)
        } else {
            None
        };
        let mut resume_decision = if self.playback_output_state.rebuffering() {
            self.scheduled_video_queue
                .rebuffer_audio_clock_resume_decision(
                    &self.pending_start_audio,
                    retention_plan.anchor_timeline_nsecs,
                    audio_output_buffered_until_nsecs,
                    audio_snapshot.map(|snapshot| snapshot.total_pending_nsecs),
                    self.video_output_rebuffer_anchor.is_some_and(|anchor| {
                        anchor.reset_to_video_when_decoded_queue_misses_anchor
                    }),
                )
        } else {
            self.scheduled_video_queue
                .initial_output_sync_decision(
                    &self.pending_start_audio,
                    previous_audio_played_until,
                )
                .map(|decision| decision.audio_clock_resume_decision())
        }
        .unwrap_or_default();
        resume_decision.timeline_nsecs = retention_plan.anchor_timeline_nsecs;
        if retention_plan.source == PendingAudioRetentionAnchorSource::InitialTransaction {
            resume_decision.delayed_audio_start_timeline_nsecs =
                self.bounded_delayed_audio_start_for_retention_plan(retention_plan);
        }
        let resume_audio_output_buffered_until_nsecs = audio_output_buffered_until_for_resume(
            resume_decision,
            audio_output_buffered_until_nsecs,
        );

        Some(AudioResumeWaterline::from_input(
            AudioResumeWaterlineInput {
                pending_audio: &self.pending_start_audio,
                resume_timeline_nsecs: resume_decision.timeline_nsecs,
                target_nsecs,
                delayed_audio_start_timeline_nsecs: resume_decision
                    .delayed_audio_start_timeline_nsecs,
                audio_output_buffered_until_nsecs: resume_audio_output_buffered_until_nsecs,
                audio_output_pending_nsecs: audio_snapshot
                    .map(|snapshot| snapshot.total_pending_nsecs),
                audio_decode_queued_nsecs,
                audio_decode_in_flight_packets,
                demux_audio_forward_nsecs,
                demux_audio_cached_packets,
            },
        ))
    }

    pub(in crate::player::backend::ffmpeg) fn audio_resume_waterline_below_input_suppression(
        &self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        audio_decode_queued_nsecs: u64,
        audio_decode_in_flight_packets: usize,
        current_start_position_nsecs: u64,
    ) -> bool {
        self.audio_resume_waterline_for_output_wait(
            audio_snapshot,
            audio_decode_queued_nsecs,
            audio_decode_in_flight_packets,
            current_start_position_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            None,
            None,
        )
        .is_some_and(|waterline| {
            !waterline.reaches_target_with_margin(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn discard_stale_pending_audio_before_output_resume(
        &mut self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        audio_decode_queued_nsecs: u64,
        audio_decode_in_flight_packets: usize,
        current_start_position_nsecs: u64,
        sample_rate: i32,
        channels: i32,
        session_id: PlaybackSessionId,
    ) -> Option<AudioResumeWaterline> {
        let retention_plan = self.pending_audio_retention_plan()?;
        let resume_timeline_nsecs = retention_plan.anchor_timeline_nsecs;
        let dropped_audio_frames = self.trim_pending_audio_to_retention_plan(
            retention_plan,
            sample_rate,
            channels,
            session_id,
        );
        let waterline = self.audio_resume_waterline_for_output_wait(
            audio_snapshot,
            audio_decode_queued_nsecs,
            audio_decode_in_flight_packets,
            current_start_position_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            None,
            None,
        )?;
        if dropped_audio_frames == 0 {
            return Some(waterline);
        }

        tracing::debug!(
            session_id = ?session_id,
            dropped_audio_frames,
            resume_timeline_nsecs,
            retention_anchor_source = retention_plan.source.as_str(),
            pending_audio_frames = self.pending_start_audio.len(),
            pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                * 1000.0,
            "discarded stale pending FFmpeg audio before output resume anchor"
        );
        self.audio_resume_waterline_for_output_wait(
            audio_snapshot,
            audio_decode_queued_nsecs,
            audio_decode_in_flight_packets,
            current_start_position_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            None,
            None,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn pending_start_audio_can_recover_output(
        &self,
        audio_snapshot: Option<AudioOutputSnapshot>,
    ) -> bool {
        if self.playback_output_state != PlaybackOutputState::Playing
            || self.pending_start_audio.is_empty()
        {
            return false;
        }
        let Some(audio_snapshot) = audio_snapshot else {
            return false;
        };

        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        if pending_audio_underrun_recovery_plan(
            &self.pending_start_audio,
            audio_snapshot.played_timeline_nsecs,
            audio_snapshot.total_pending_nsecs,
            queued_video_range_nsecs.map(|(start, _)| start),
            queued_video_range_nsecs.map(|(_, end)| end),
        )
        .is_some()
        {
            return true;
        }

        let audio_start_timeline_nsecs =
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot);
        let video_lead_until_timeline_nsecs = self
            .scheduled_video_queue
            .audio_output_lead_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or(audio_start_timeline_nsecs);
        let audio_flush_until_timeline_nsecs = audio_output_flush_until_timeline_nsecs(
            audio_snapshot,
            video_lead_until_timeline_nsecs,
            self.audio_output_steady_target_duration(audio_snapshot),
        );
        audio_flush_until_timeline_nsecs > audio_start_timeline_nsecs
            && self
                .pending_start_audio
                .buffered_until_from(audio_start_timeline_nsecs)
                .is_some_and(|buffered_until| buffered_until > audio_start_timeline_nsecs)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg::playback_loop) fn push_decoded_audio_or_buffer(
        &mut self,
        output: &AudioOutput,
        control: &FfmpegControl,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        subtitle_pipeline: &mut SubtitlePipeline,
        buffered_reporter: &mut BufferedReporter,
        decoder_drain: bool,
    ) -> std::result::Result<DecodedAudioAdmission, String> {
        self.note_output_housekeeping_change();
        if !self.pending_start_audio_backpressured() {
            self.pending_audio_backpressure_log_state = None;
        }
        if self.restart_pending() {
            // Packet arrival only transfers decoded ownership into the pending
            // side of the active restart.  AO ownership is validated by the
            // transaction's prepare/commit path; ordinary packets are not a
            // discontinuity and must never abort or fence that transaction.
            self.pending_start_audio
                .push(audio, start_timeline_nsecs, end_timeline_nsecs);
            self.refresh_initial_bounded_delayed_audio_start_plan();
            return Ok(DecodedAudioAdmission::Accepted);
        }
        let mut audio_snapshot = output.snapshot()?;
        if decoder_drain {
            self.pending_start_audio
                .push(audio, start_timeline_nsecs, end_timeline_nsecs);
            self.report_playing_pending_start_audio_pressure(
                session_id,
                "decoder_drain_audio_buffered",
            );
            return Ok(DecodedAudioAdmission::Accepted);
        }
        if self.playback_output_state == PlaybackOutputState::Playing {
            if !self.pending_start_audio.is_empty() {
                self.flush_pending_start_audio_if_ready(
                    output,
                    control,
                    session_id,
                    vo_queue,
                    frame_presented,
                    position_reporter,
                    event_tx,
                    subtitle_pipeline,
                    buffered_reporter,
                )?;
                if self.playback_output_state != PlaybackOutputState::Playing {
                    return Ok(self.defer_decoded_audio_for_backpressure(
                        audio,
                        start_timeline_nsecs,
                        end_timeline_nsecs,
                        session_id,
                        "output_state_changed_while_flushing",
                    ));
                }
                audio_snapshot = output.snapshot()?;
            }
            let audio_start_timeline_nsecs =
                audio_output_contiguous_start_timeline_nsecs(audio_snapshot);
            let dropped_audio_frames = self
                .pending_start_audio
                .discard_before(audio_start_timeline_nsecs);
            if dropped_audio_frames > 0 {
                tracing::debug!(
                    session_id = ?session_id,
                    dropped_audio_frames,
                    audio_start_timeline_nsecs,
                    pending_audio_frames = self.pending_start_audio.len(),
                    pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                        * 1000.0,
                    "discarded stale pending FFmpeg audio before steady-state output push"
                );
            }
            self.report_playing_pending_start_audio_pressure(
                session_id,
                "before_decoded_audio_push",
            );
            if self.recover_runaway_playing_pending_audio_if_needed(
                output,
                control,
                session_id,
                "before_decoded_audio_push",
            )? {
                return Ok(self.defer_decoded_audio_for_backpressure(
                    audio,
                    start_timeline_nsecs,
                    end_timeline_nsecs,
                    session_id,
                    "runaway_pending_audio_recovery",
                ));
            }
            if self.pending_start_audio_backpressured() {
                return Ok(self.defer_decoded_audio_for_backpressure(
                    audio,
                    start_timeline_nsecs,
                    end_timeline_nsecs,
                    session_id,
                    "pending_audio_high_water",
                ));
            }
        }
        if self.decoded_audio_can_push_directly(
            start_timeline_nsecs,
            end_timeline_nsecs,
            audio_snapshot.buffered_until_timeline_nsecs,
        ) {
            push_decoded_audio_to_output(
                output,
                control,
                audio,
                start_timeline_nsecs,
                end_timeline_nsecs,
                &mut self.pending_start_audio,
                &mut self.scheduled_video_queue,
                session_id,
                vo_queue,
                frame_presented,
                position_reporter,
                event_tx,
                subtitle_pipeline,
                buffered_reporter,
            )?;
        } else {
            self.pending_start_audio
                .push(audio, start_timeline_nsecs, end_timeline_nsecs);
            self.report_playing_pending_start_audio_pressure(session_id, "decoded_audio_buffered");
            if self.recover_runaway_playing_pending_audio_if_needed(
                output,
                control,
                session_id,
                "decoded_audio_buffered",
            )? {
                return Ok(DecodedAudioAdmission::AcceptedAndBackpressured);
            }
        }
        Ok(DecodedAudioAdmission::Accepted)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn defer_decoded_audio_for_backpressure(
        &mut self,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> DecodedAudioAdmission {
        const SUMMARY_INTERVAL: Duration = Duration::from_secs(1);
        let now = Instant::now();
        let (log_kind, suppressed_repeats, blocked_for) =
            match self.pending_audio_backpressure_log_state.as_mut() {
                Some(state) if state.reason == reason => {
                    if now.saturating_duration_since(state.last_logged_at) < SUMMARY_INTERVAL {
                        state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
                        return DecodedAudioAdmission::Deferred(audio);
                    }
                    let suppressed_repeats = state.suppressed_repeats;
                    state.suppressed_repeats = 0;
                    state.last_logged_at = now;
                    (
                        "periodic_summary",
                        suppressed_repeats,
                        now.saturating_duration_since(state.started_at),
                    )
                }
                _ => {
                    self.pending_audio_backpressure_log_state =
                        Some(PendingAudioBackpressureLogState {
                            reason,
                            started_at: now,
                            last_logged_at: now,
                            suppressed_repeats: 0,
                        });
                    ("state_changed", 0, Duration::ZERO)
                }
            };
        tracing::debug!(
            session_id = ?session_id,
            reason,
            log_kind,
            suppressed_repeats,
            blocked_ms = blocked_for.as_secs_f64() * 1000.0,
            pending_audio_frames = self.pending_start_audio.len(),
            pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                * 1000.0,
            pending_audio_range_nsecs = ?self.pending_start_audio.range_nsecs(),
            pending_audio_contiguous_range_nsecs =
                ?self.pending_start_audio.contiguous_range_nsecs(),
            pending_audio_first_gap_nsecs = ?self.pending_start_audio.first_gap_nsecs(),
            pending_audio_limit_ms =
            playing_pending_audio_limit_duration().as_secs_f64() * 1000.0,
            start_timeline_nsecs,
            end_timeline_nsecs,
            "returned decoded FFmpeg audio to the decoder pipeline at pending-audio backpressure"
        );
        DecodedAudioAdmission::Deferred(audio)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg::playback_loop) fn flush_pending_start_audio_if_ready(
        &mut self,
        output: &AudioOutput,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        subtitle_pipeline: &mut SubtitlePipeline,
        buffered_reporter: &mut BufferedReporter,
    ) -> std::result::Result<(), String> {
        if self.restart_pending() || self.playback_output_state.rebuffering() {
            return Ok(());
        }
        if self.pending_start_audio.is_empty() {
            self.defer_pending_start_audio_flush_once = false;
            self.startup_pending_audio_pressure_context_active = false;
            return Ok(());
        }
        if self.defer_pending_start_audio_flush_once {
            self.defer_pending_start_audio_flush_once = false;
            self.clear_startup_pending_audio_pressure_context_if_ready();
            return Ok(());
        }
        if recover_pending_start_audio_after_underrun(
            &mut self.pending_start_audio,
            output,
            control,
            &mut self.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )? {
            return Ok(());
        }
        let audio_snapshot = output.snapshot()?;
        let audio_start_timeline_nsecs =
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot);
        let video_lead_until_timeline_nsecs = self
            .scheduled_video_queue
            .audio_output_lead_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or(audio_start_timeline_nsecs);
        let audio_flush_until_timeline_nsecs = audio_output_flush_until_timeline_nsecs(
            audio_snapshot,
            video_lead_until_timeline_nsecs,
            self.audio_output_steady_target_duration(audio_snapshot),
        );
        let made_progress = flush_pending_start_audio(
            &mut self.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::AudioStarted,
            DelayedAudioStartSilencePolicy::Skip,
            control,
            &mut self.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        self.report_playing_pending_start_audio_pressure(session_id, "pending_audio_flush");
        if !made_progress {
            self.recover_runaway_playing_pending_audio_if_needed(
                output,
                control,
                session_id,
                "pending_audio_flush_blocked",
            )?;
        }
        Ok(())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn decoded_audio_can_push_directly(
        &self,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        audio_output_buffered_until_timeline_nsecs: u64,
    ) -> bool {
        !self.restart_pending()
            && !self.playback_output_state.rebuffering()
            && self.pending_start_audio.is_empty()
            && start_timeline_nsecs >= audio_output_buffered_until_timeline_nsecs
            && self
                .scheduled_video_queue
                .audio_output_lead_until_from_nsecs(start_timeline_nsecs)
                .is_some_and(|limit| end_timeline_nsecs <= limit)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn defer_next_pending_start_audio_flush(
        &mut self,
    ) {
        self.defer_pending_start_audio_flush_once = true;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn defer_next_pending_start_audio_flush_after_initial_start(
        &mut self,
    ) {
        self.defer_pending_start_audio_flush_once = true;
        self.startup_pending_audio_pressure_context_active = true;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn pending_audio_pressure_context(
        &self,
    ) -> PendingAudioPressureContext {
        if self.restart_pending() || self.startup_pending_audio_pressure_context_active {
            PendingAudioPressureContext::StartupSync
        } else if self.playback_output_state.rebuffering() {
            PendingAudioPressureContext::RebufferResume
        } else {
            PendingAudioPressureContext::PlayingSteady
        }
    }

    fn clear_startup_pending_audio_pressure_context_if_ready(&mut self) {
        if self.startup_pending_audio_pressure_context_active
            && (self.pending_start_audio.is_empty()
                || self.pending_start_audio.buffered_duration()
                    <= playing_pending_audio_pressure_clear_duration())
        {
            self.startup_pending_audio_pressure_context_active = false;
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn report_playing_pending_start_audio_pressure(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.clear_startup_pending_audio_pressure_context_if_ready();
        let pressure_context = self.pending_audio_pressure_context();
        if self.playback_output_state != PlaybackOutputState::Playing {
            self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
            return;
        }
        let pending_duration = self.pending_start_audio.buffered_duration();
        let level = PendingStartAudioPressureLevel::from_duration(pending_duration);
        if level == PendingStartAudioPressureLevel::Normal {
            if self.pending_start_audio_pressure_level >= PendingStartAudioPressureLevel::Warn
                && pending_duration >= playing_pending_audio_pressure_clear_duration()
            {
                self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Warn;
                return;
            }
            self.pending_start_audio_pressure_level = level;
            return;
        }

        for crossed in [
            PendingStartAudioPressureLevel::Warn,
            PendingStartAudioPressureLevel::ForceRecovery,
            PendingStartAudioPressureLevel::HardReset,
        ] {
            if self.pending_start_audio_pressure_level < crossed && level >= crossed {
                tracing::warn!(
                    session_id = ?session_id,
                    reason,
                    pressure_level = crossed.label(),
                    pending_audio_pressure_context = pressure_context.as_str(),
                    startup_pending_pressure_suppressed_hard_reset =
                        pressure_context == PendingAudioPressureContext::StartupSync
                            && crossed == PendingStartAudioPressureLevel::HardReset,
                    pending_audio_frames = self.pending_start_audio.len(),
                    pending_audio_ms = pending_duration.as_secs_f64() * 1000.0,
                    pending_audio_range_nsecs = ?self.pending_start_audio.range_nsecs(),
                    pending_audio_contiguous_range_nsecs =
                        ?self.pending_start_audio.contiguous_range_nsecs(),
                    pending_audio_first_gap_nsecs = ?self.pending_start_audio.first_gap_nsecs(),
                    threshold_ms = crossed.threshold().as_secs_f64() * 1000.0,
                    playing_pending_audio_limit_ms =
                        playing_pending_audio_limit_duration().as_secs_f64() * 1000.0,
                    "playing FFmpeg pending audio exceeded steady-state limit"
                );
            }
        }
        self.pending_start_audio_pressure_level = level;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn recover_runaway_playing_pending_audio_if_needed(
        &mut self,
        output: &AudioOutput,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> std::result::Result<bool, String> {
        if self.playback_output_state != PlaybackOutputState::Playing
            || self.pending_start_audio.buffered_duration()
                < PLAYING_PENDING_AUDIO_HARD_RESET_DURATION
        {
            return Ok(false);
        }
        self.clear_startup_pending_audio_pressure_context_if_ready();
        let pressure_context = self.pending_audio_pressure_context();
        if pressure_context != PendingAudioPressureContext::PlayingSteady {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                pending_audio_pressure_context = pressure_context.as_str(),
                startup_pending_pressure_suppressed_hard_reset =
                    pressure_context == PendingAudioPressureContext::StartupSync,
                pending_audio_frames = self.pending_start_audio.len(),
                pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                    * 1000.0,
                hard_reset_threshold_ms =
                    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION.as_secs_f64() * 1000.0,
                "suppressed FFmpeg pending audio hard reset outside steady-state playback"
            );
            return Ok(false);
        }

        let audio_snapshot = output.snapshot()?;
        let audio_contiguous_start_nsecs =
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot);
        let dropped_stale_audio_frames = self
            .pending_start_audio
            .discard_before(audio_contiguous_start_nsecs);
        if self.pending_start_audio.buffered_duration() < PLAYING_PENDING_AUDIO_HARD_RESET_DURATION
        {
            if dropped_stale_audio_frames > 0 {
                tracing::warn!(
                    session_id = ?session_id,
                    reason,
                    dropped_stale_audio_frames,
                    audio_contiguous_start_nsecs,
                    pending_audio_frames = self.pending_start_audio.len(),
                    pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                        * 1000.0,
                    "discarded stale runaway FFmpeg pending audio before hard reset"
                );
            }
            return Ok(false);
        }

        let recovery_timeline_nsecs = match self.scheduled_video_queue.range_nsecs() {
            Some((start, end))
                if audio_contiguous_start_nsecs >= start && audio_contiguous_start_nsecs < end =>
            {
                audio_contiguous_start_nsecs
            }
            Some((start, _)) => start,
            None => audio_contiguous_start_nsecs,
        };
        let retained_pending_audio_frames = self.pending_start_audio.len();
        let retained_pending_audio_ms =
            self.pending_start_audio.buffered_duration().as_secs_f64() * 1000.0;
        let decoded_video_forward_nsecs = self
            .scheduled_video_queue
            .forward_nsecs_from(recovery_timeline_nsecs);
        self.video_output_rebuffer_anchor = enter_video_output_rebuffer(
            &mut self.playback_output_state,
            control,
            Some(output),
            &self.scheduled_video_queue,
            session_id,
            Duration::ZERO,
            decoded_video_forward_nsecs,
            None,
            super::VideoOutputUnderflowClassification::DemuxRebuffer,
            false,
        );
        self.note_video_output_rebuffer_started(Instant::now());
        tracing::warn!(
            session_id = ?session_id,
            reason,
            dropped_stale_audio_frames,
            retained_pending_audio_frames,
            retained_pending_audio_ms,
            pending_audio_range_nsecs = ?self.pending_start_audio.range_nsecs(),
            pending_audio_contiguous_range_nsecs =
                ?self.pending_start_audio.contiguous_range_nsecs(),
            pending_audio_first_gap_nsecs = ?self.pending_start_audio.first_gap_nsecs(),
            audio_played_timeline_nsecs = audio_snapshot.played_timeline_nsecs,
            audio_buffered_until_timeline_nsecs = audio_snapshot.buffered_until_timeline_nsecs,
            recovery_timeline_nsecs,
            decoded_video_range = ?self.scheduled_video_queue.range_nsecs(),
            decoded_video_forward_ms = ?decoded_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "entered lossless FFmpeg audio-pressure recovery with pending audio retained"
        );
        Ok(true)
    }
}
