use super::*;

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
        audio_snapshot: Option<AudioOutputSnapshot>,
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
        let mut audio_accepted_start_timeline_nsecs = direct_until_nsecs
            .map(|_| resume_timeline_nsecs)
            .or_else(|| delayed_range.map(|(start_nsecs, _)| start_nsecs));
        let mut contiguous_coverage_nsecs = direct_until_nsecs
            .map(|end_nsecs| end_nsecs.saturating_sub(resume_timeline_nsecs))
            .or_else(|| {
                delayed_range.map(|(start_nsecs, end_nsecs)| end_nsecs.saturating_sub(start_nsecs))
            });
        let mut output_resumed = false;
        if let Some(snapshot) = audio_snapshot
            && snapshot.total_pending_nsecs > 0
            && let Some((payload_start, payload_end)) = snapshot.payload_range_nsecs
        {
            let reference = resume_timeline_nsecs.max(snapshot.played_timeline_nsecs);
            let accepted_start = reference.max(payload_start);
            if payload_start
                <= reference.saturating_add(duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE))
                && payload_end > accepted_start
            {
                // Samples transferred to AO still belong to this recovery.
                // Bound the interval by real payload so a PTS hole or a reset
                // clock alone cannot manufacture coverage.
                let output_coverage = payload_end
                    .saturating_sub(accepted_start)
                    .min(snapshot.total_pending_nsecs);
                let pending_extension = self
                    .pending_start_audio
                    .buffered_until_from(payload_end)
                    .unwrap_or(payload_end)
                    .saturating_sub(payload_end);
                let coverage = output_coverage.saturating_add(pending_extension);
                if coverage > contiguous_coverage_nsecs.unwrap_or_default() {
                    audio_accepted_start_timeline_nsecs = Some(accepted_start);
                    contiguous_coverage_nsecs = Some(coverage);
                }
                // Once the output gate has resumed with real audio, do not
                // require it to refill the original startup waterline again.
                output_resumed = self.playback_output_state == PlaybackOutputState::Playing
                    && snapshot.played_timeline_nsecs >= resume_timeline_nsecs
                    && output_coverage >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION);
            }
        }
        AudioRealignCoverage {
            audio_accepted_start_timeline_nsecs,
            start_gap_nsecs: audio_accepted_start_timeline_nsecs
                .map(|accepted_start| accepted_start.saturating_sub(resume_timeline_nsecs)),
            contiguous_coverage_nsecs,
            protected_target_nsecs,
            ready: output_resumed
                || contiguous_coverage_nsecs
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
            audio_input_eof: false,
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
            audio_realign_exhausted_range_nsecs: None,
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

    pub(super) fn reset_for_presentation_session(
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
        self.audio_input_eof = false;
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
        self.audio_realign_exhausted_range_nsecs = None;
        self.recent_audio_output_underrun_window_started_at = None;
        self.recent_audio_output_underruns = 0;
        self.video_clock_anchor_valid = false;
        self.reset_audio_output_activity_watchdog();
    }
}
