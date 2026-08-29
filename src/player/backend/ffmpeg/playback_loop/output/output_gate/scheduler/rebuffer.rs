use super::*;

impl PlaybackOutputScheduler {
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

    pub(in crate::player::backend::ffmpeg) fn output_fill_phase(&self) -> bool {
        self.restart_pending() || self.playback_output_state.rebuffering()
    }

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

    pub(super) fn refresh_video_deadline_service_active(&self) {
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

    pub(super) fn clear_video_bootstrap_after_seek(&mut self, reason: &'static str) {
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

    pub(super) fn audio_gap_recovery_suppresses_rebuffer(
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
}
