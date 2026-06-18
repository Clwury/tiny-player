use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AtomicBool, AudioClockMode,
    AudioOutput, AudioOutputSnapshot, BackendEvent, BufferedReporter, DecodedAudio,
    DelayedAudioStartSilencePolicy, Duration, FfmpegControl,
    PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION, PLAYING_PENDING_AUDIO_HARD_RESET_DURATION,
    PendingStartAudioPressureLevel, PlaybackOutputScheduler, PlaybackOutputState,
    PlaybackSessionId, PositionReporter, SubtitlePipeline, VideoOutputQueue, duration_nsecs,
    enter_video_output_rebuffer, flush_pending_start_audio, pending_audio_underrun_recovery_plan,
    push_decoded_audio_to_output, recover_pending_start_audio_after_underrun,
};
use super::{PENDING_START_AUDIO_BACKPRESSURE_DURATION, Sender};

impl PendingStartAudioPressureLevel {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn from_duration(
        duration: Duration,
    ) -> Self {
        if duration >= PLAYING_PENDING_AUDIO_HARD_RESET_DURATION {
            Self::HardReset
        } else if duration >= PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION {
            Self::ForceRecovery
        } else if duration >= playing_pending_audio_limit_duration() {
            Self::Warn
        } else {
            Self::Normal
        }
    }

    fn threshold(self) -> Duration {
        match self {
            Self::Normal => Duration::ZERO,
            Self::Warn => playing_pending_audio_limit_duration(),
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

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn playing_pending_audio_pressure_clear_duration()
-> Duration {
    playing_pending_audio_limit_duration().saturating_sub(Duration::from_millis(100))
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_output_flush_until_timeline_nsecs(
    snapshot: AudioOutputSnapshot,
    video_lead_until_timeline_nsecs: u64,
) -> u64 {
    let max_audio_until_nsecs = snapshot
        .played_timeline_nsecs
        .saturating_add(duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT));
    video_lead_until_timeline_nsecs.min(max_audio_until_nsecs)
}

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn pending_start_audio_backpressured(&self) -> bool {
        let buffered_duration = self.pending_start_audio.buffered_duration();
        if self.playback_output_state == PlaybackOutputState::Playing {
            return buffered_duration >= playing_pending_audio_limit_duration();
        }
        if buffered_duration < PENDING_START_AUDIO_BACKPRESSURE_DURATION {
            return false;
        }
        !self.playback_output_state.first_video_frame_pending()
            || !self.scheduled_video_queue.is_empty()
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
    ) -> std::result::Result<(), String> {
        let mut audio_snapshot = output.snapshot()?;
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
                    return Ok(());
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
                return Ok(());
            }
            if self.pending_start_audio_backpressured() {
                tracing::debug!(
                    session_id = ?session_id,
                    pending_audio_frames = self.pending_start_audio.len(),
                    pending_audio_ms = self.pending_start_audio.buffered_duration().as_secs_f64()
                        * 1000.0,
                    pending_audio_limit_ms =
                        playing_pending_audio_limit_duration().as_secs_f64() * 1000.0,
                    start_timeline_nsecs,
                    end_timeline_nsecs,
                    "dropping decoded FFmpeg audio because steady-state pending audio is backpressured"
                );
                return Ok(());
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
                return Ok(());
            }
        }
        Ok(())
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
        if self.playback_output_state.first_video_frame_pending()
            || self.playback_output_state.rebuffering()
            || self.pending_start_audio.is_empty()
        {
            return Ok(());
        }
        if self.defer_pending_start_audio_flush_once {
            self.defer_pending_start_audio_flush_once = false;
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
        !self.playback_output_state.first_video_frame_pending()
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

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn report_playing_pending_start_audio_pressure(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
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
                    pending_audio_frames = self.pending_start_audio.len(),
                    pending_audio_ms = pending_duration.as_secs_f64() * 1000.0,
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

        let reset_timeline_nsecs = match self.scheduled_video_queue.range_nsecs() {
            Some((start, end))
                if audio_contiguous_start_nsecs >= start && audio_contiguous_start_nsecs < end =>
            {
                audio_contiguous_start_nsecs
            }
            Some((start, _)) => start,
            None => audio_contiguous_start_nsecs,
        };
        let cleared_pending_audio_frames = self.pending_start_audio.len();
        let cleared_pending_audio_ms =
            self.pending_start_audio.buffered_duration().as_secs_f64() * 1000.0;
        self.pending_start_audio.clear();
        self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        output.reset_clock(reset_timeline_nsecs);
        let decoded_video_forward_nsecs = self
            .scheduled_video_queue
            .forward_nsecs_from(reset_timeline_nsecs);
        self.video_output_rebuffer_anchor = enter_video_output_rebuffer(
            &mut self.playback_output_state,
            control,
            Some(output),
            &self.scheduled_video_queue,
            session_id,
            Duration::ZERO,
            decoded_video_forward_nsecs,
        );
        self.sync_first_video_frame_pending();
        tracing::warn!(
            session_id = ?session_id,
            reason,
            dropped_stale_audio_frames,
            cleared_pending_audio_frames,
            cleared_pending_audio_ms,
            audio_played_timeline_nsecs = audio_snapshot.played_timeline_nsecs,
            audio_buffered_until_timeline_nsecs = audio_snapshot.buffered_until_timeline_nsecs,
            reset_timeline_nsecs,
            decoded_video_range = ?self.scheduled_video_queue.range_nsecs(),
            decoded_video_forward_ms = ?decoded_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "hard-reset FFmpeg audio clock after runaway pending audio"
        );
        Ok(true)
    }
}
