use super::*;

impl PlaybackOutputScheduler {
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
            || !matches!(
                transaction.audio_prepare_phase,
                InitialAudioPreparePhase::Preparing | InitialAudioPreparePhase::Prepared
            )
            || transaction.audio_prepare_epoch != Some(token.audio_epoch)
            || transaction.audio_prepare_token.is_some_and(|previous| {
                token.staged_range_nsecs.0 != previous.staged_range_nsecs.0
                    || token.staged_until_nsecs < previous.staged_until_nsecs
                    || token.staged_frames < previous.staged_frames
                    || token.staged_samples < previous.staged_samples
            })
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

    pub(super) fn fail_initial_av_start_transaction_with_anchor(
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
            "initial A/V start transaction failed; entering rebuffer at the retained anchor"
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
}
