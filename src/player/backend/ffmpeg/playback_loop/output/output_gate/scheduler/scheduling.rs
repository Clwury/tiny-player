use super::*;

impl PlaybackOutputScheduler {
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

    pub(super) fn rebuffer_empty_audio_output_resume_timeline_nsecs(&self) -> Option<u64> {
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
