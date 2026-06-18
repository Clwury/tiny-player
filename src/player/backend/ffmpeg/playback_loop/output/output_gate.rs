use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

#[cfg(test)]
use super::QueuedVideoFrame;
use super::audio_output_gate::{
    DelayedAudioStartSilencePolicy, flush_pending_start_audio,
    pending_audio_underrun_recovery_plan, push_decoded_audio_to_output,
    recover_pending_start_audio_after_underrun,
};
use super::output_rebuffer::{
    AudioClockResumeDecision, PlaybackOutputState, PlaybackResumeWaterline, RebufferResumeAnchor,
    audio_output_buffered_until_for_resume, clear_video_output_rebuffer,
    enter_video_output_rebuffer, finish_video_output_rebuffer_if_ready,
    rebuffer_playback_resume_waterline_after_cache_pause,
    rebuffer_playback_resume_waterline_after_prolonged_wait, should_block_for_demux_read,
    video_output_rebuffer_should_enter,
};
use super::pending_audio_queue::PendingStartAudio;
use super::scheduled_video_queue::ScheduledVideoQueue;
use super::video_output_gate::{present_first_queued_video_frame, present_video_frame_to_vo};
use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioClockMode, AudioOutput,
    AudioOutputSnapshot, BufferedReporter, DecodedAudio, DemuxPacketCache, DemuxReaderWatermark,
    FfmpegControl, OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER, PENDING_AUDIO_CONTINUITY_TOLERANCE,
    PENDING_START_AUDIO_BACKPRESSURE_DURATION, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PlaybackScheduler, PositionReporter,
    SubtitlePipeline, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, duration_nsecs, nsecs_to_seconds,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PendingStartAudioPressureLevel {
    Normal,
    Warn,
    ForceRecovery,
    HardReset,
}

impl PendingStartAudioPressureLevel {
    fn from_duration(duration: Duration) -> Self {
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

pub(in crate::player::backend::ffmpeg) struct PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg::playback_loop) scheduled_video_queue:
        ScheduledVideoQueue,
    pub(in crate::player::backend::ffmpeg::playback_loop) pending_start_audio: PendingStartAudio,
    pub(in crate::player::backend::ffmpeg::playback_loop) playback_output_state:
        PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_video_frame_pending: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_underrun_started_at:
        Option<Instant>,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
    syncing_started_at: Option<Instant>,
    defer_pending_start_audio_flush_once: bool,
    pending_start_audio_pressure_level: PendingStartAudioPressureLevel,
    initial_delayed_audio_start_timeline_nsecs: Option<u64>,
}

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn new() -> Self {
        let playback_output_state = PlaybackOutputState::Syncing;
        Self {
            scheduled_video_queue: ScheduledVideoQueue::default(),
            pending_start_audio: PendingStartAudio::default(),
            first_video_frame_pending: playback_output_state.first_video_frame_pending(),
            playback_output_state,
            video_output_underrun_started_at: None,
            video_output_rebuffer_anchor: None,
            syncing_started_at: Some(Instant::now()),
            defer_pending_start_audio_flush_once: false,
            pending_start_audio_pressure_level: PendingStartAudioPressureLevel::Normal,
            initial_delayed_audio_start_timeline_nsecs: None,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn reset(&mut self, control: &FfmpegControl) {
        self.scheduled_video_queue.clear();
        self.pending_start_audio.clear();
        self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        self.initial_delayed_audio_start_timeline_nsecs = None;
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.set_state(PlaybackOutputState::Syncing);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer(&mut self, control: &FfmpegControl) {
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.sync_first_video_frame_pending();
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffering(&self) -> bool {
        self.playback_output_state.rebuffering()
    }

    /// True while the output is building up the decoded buffer to (re)start
    /// playback (initial sync or rebuffer). During this phase the soft Vulkan
    /// frame-pressure throttle is lifted so decode can reach the resume waterline.
    pub(in crate::player::backend::ffmpeg) fn output_fill_phase(&self) -> bool {
        self.playback_output_state.first_video_frame_pending()
            || self.playback_output_state.rebuffering()
    }

    pub(in crate::player::backend::ffmpeg) fn set_state(&mut self, state: PlaybackOutputState) {
        self.playback_output_state = state;
        self.syncing_started_at = (state == PlaybackOutputState::Syncing).then(Instant::now);
        if state != PlaybackOutputState::Ready {
            self.initial_delayed_audio_start_timeline_nsecs = None;
        }
        if state != PlaybackOutputState::Playing {
            self.defer_pending_start_audio_flush_once = false;
            self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        }
        self.sync_first_video_frame_pending();
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
        self.video_output_rebuffer_anchor = None;
        self.sync_first_video_frame_pending();
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn maybe_enter_video_output_rebuffer(
        &mut self,
        now: Instant,
        video_output_underflowing: bool,
        output_underrun: bool,
        demux_cache_insufficient: bool,
        render_backlogged: bool,
        has_audio_output: bool,
        pending_audio_recoverable: bool,
        control: &FfmpegControl,
        audio_output: Option<&AudioOutput>,
        session_id: PlaybackSessionId,
        decoded_video_forward_nsecs: Option<u64>,
    ) -> bool {
        if !video_output_rebuffer_should_enter(
            &mut self.video_output_underrun_started_at,
            now,
            video_output_underflowing,
            output_underrun,
            demux_cache_insufficient,
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
        self.video_output_rebuffer_anchor = enter_video_output_rebuffer(
            &mut self.playback_output_state,
            control,
            audio_output,
            &self.scheduled_video_queue,
            session_id,
            underrun_elapsed,
            decoded_video_forward_nsecs,
        );
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
        self.sync_first_video_frame_pending();
        true
    }

    fn sync_first_video_frame_pending(&mut self) {
        self.first_video_frame_pending = self.playback_output_state.first_video_frame_pending();
    }

    pub(in crate::player::backend::ffmpeg) fn snapshot(&self) -> PlaybackOutputSnapshot {
        self.snapshot_for_played_until(None)
    }

    pub(in crate::player::backend::ffmpeg) fn snapshot_for_played_until(
        &self,
        played_until_nsecs: Option<u64>,
    ) -> PlaybackOutputSnapshot {
        let queued_video_duration_nsecs = self.scheduled_video_queue.duration_nsecs();
        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let can_measure_forward = !self.playback_output_state.first_video_frame_pending()
            && !self.playback_output_state.rebuffering();
        let queued_video_forward_nsecs = played_until_nsecs
            .filter(|_| can_measure_forward)
            .and_then(|played_until| self.scheduled_video_queue.forward_nsecs_from(played_until));
        let video_output_low_water = played_until_nsecs.is_some_and(|played_until| {
            can_measure_forward && self.scheduled_video_queue.low_water(played_until)
        });

        PlaybackOutputSnapshot {
            state: self.playback_output_state,
            first_video_frame_pending: self.first_video_frame_pending,
            rebuffering: self.playback_output_state.rebuffering(),
            queued_video_frames: self.scheduled_video_queue.len(),
            queued_video_duration_nsecs,
            queued_video_range_nsecs,
            queued_video_forward_nsecs,
            video_output_low_water,
            pending_start_audio_frames: self.pending_start_audio.len(),
            pending_start_audio_nsecs: duration_nsecs(self.pending_start_audio.buffered_duration()),
            video_output_rebuffer_anchor: self.video_output_rebuffer_anchor,
        }
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
        if self.playback_output_state.first_video_frame_pending()
            || self.playback_output_state.rebuffering()
        {
            return None;
        }
        self.scheduled_video_queue
            .audio_clock_wait_duration(played_until_nsecs)
    }

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

    pub(in crate::player::backend::ffmpeg) fn video_decode_skip_nonref_for_pressure(
        &self,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        audio_output_pending_nsecs: Option<u64>,
        skip_nonref_active: bool,
    ) -> bool {
        self.scheduled_video_queue.skip_nonref_for_pressure(
            self.playback_output_state,
            played_until_nsecs,
            has_audio_output,
            audio_output_pending_nsecs,
            skip_nonref_active,
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

    fn decoded_audio_can_push_directly(
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

    fn startup_sync_elapsed(&self) -> Option<Duration> {
        (self.playback_output_state == PlaybackOutputState::Syncing)
            .then(|| self.syncing_started_at.map(|started| started.elapsed()))
            .flatten()
    }

    fn rebuffer_wait_elapsed(&self) -> Option<Duration> {
        self.playback_output_state
            .rebuffering()
            .then(|| {
                self.video_output_underrun_started_at
                    .map(|started| started.elapsed())
            })
            .flatten()
    }

    fn defer_next_pending_start_audio_flush(&mut self) {
        self.defer_pending_start_audio_flush_once = true;
    }

    fn report_playing_pending_start_audio_pressure(
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

    fn recover_runaway_playing_pending_audio_if_needed(
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

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn push_decoded_video_for_test(
        &mut self,
        frame: QueuedVideoFrame,
    ) {
        self.scheduled_video_queue.push_queued(frame);
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
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_video_output_underrun_started_at_for_test(
        &mut self,
        started_at: Instant,
    ) {
        self.video_output_underrun_started_at = Some(started_at);
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

fn audio_output_contiguous_start_timeline_nsecs(snapshot: AudioOutputSnapshot) -> u64 {
    if snapshot.total_pending_nsecs > 0 {
        snapshot.buffered_until_timeline_nsecs
    } else {
        snapshot.played_timeline_nsecs
    }
}

fn playing_pending_audio_limit_duration() -> Duration {
    AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
}

fn playing_pending_audio_pressure_clear_duration() -> Duration {
    playing_pending_audio_limit_duration().saturating_sub(Duration::from_millis(100))
}

fn audio_output_flush_until_timeline_nsecs(
    snapshot: AudioOutputSnapshot,
    video_lead_until_timeline_nsecs: u64,
) -> u64 {
    let max_audio_until_nsecs = snapshot
        .played_timeline_nsecs
        .saturating_add(duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT));
    video_lead_until_timeline_nsecs.min(max_audio_until_nsecs)
}

fn initial_delayed_audio_start_timeline_nsecs(
    output_scheduler: &PlaybackOutputScheduler,
    resume_decision: AudioClockResumeDecision,
) -> Option<u64> {
    if let Some(audio_start_timeline_nsecs) =
        output_scheduler.initial_delayed_audio_start_timeline_nsecs
    {
        return Some(audio_start_timeline_nsecs);
    }
    if !output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        return None;
    }

    let (first_video_timeline_nsecs, _) = output_scheduler.scheduled_video_queue.range_nsecs()?;
    let first_audio_timeline_nsecs = output_scheduler
        .pending_start_audio
        .first_start_timeline_nsecs()?;
    let gap_tolerance_nsecs = duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE);
    (first_audio_timeline_nsecs > first_video_timeline_nsecs.saturating_add(gap_tolerance_nsecs)
        && resume_decision.timeline_nsecs >= first_audio_timeline_nsecs)
        .then_some(first_audio_timeline_nsecs)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) state: PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg) first_video_frame_pending: bool,
    pub(in crate::player::backend::ffmpeg) rebuffering: bool,
    pub(in crate::player::backend::ffmpeg) queued_video_frames: usize,
    pub(in crate::player::backend::ffmpeg) queued_video_duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queued_video_range_nsecs: Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg) queued_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) video_output_low_water: bool,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_frames: usize,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
}

impl PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) fn waiting_for_demux(self) -> bool {
        !self.first_video_frame_pending && self.queued_video_frames == 0
    }

    pub(in crate::player::backend::ffmpeg) fn underflowing(self) -> bool {
        self.waiting_for_demux() || self.video_output_low_water
    }

    pub(in crate::player::backend::ffmpeg) fn should_wait_for_demux(self) -> bool {
        should_block_for_demux_read(self.state)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum OutputGateResumeStatus {
    Idle,
    Waiting,
    WaitingForDemux,
    Resumed,
}

#[derive(Clone, Copy, Default)]
struct OutputGateResumeTiming {
    audio_snapshot: Duration,
    resume_decision: Duration,
    demux_watermark: Duration,
    waterline: Duration,
    fallback: Duration,
    resume_action: Duration,
    wait_log: Duration,
}

#[derive(Clone, Copy)]
struct OutputGateResumeLogContext {
    session_id: PlaybackSessionId,
    started_at: Instant,
    timing: OutputGateResumeTiming,
    status: OutputGateResumeStatus,
    output_state: PlaybackOutputState,
    queued_video_frames: usize,
    pending_audio_frames: usize,
    waterline: Option<PlaybackResumeWaterline>,
}

fn output_gate_resume_log_context(
    output_scheduler: &PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    started_at: Instant,
    timing: OutputGateResumeTiming,
    status: OutputGateResumeStatus,
    waterline: Option<PlaybackResumeWaterline>,
) -> OutputGateResumeLogContext {
    OutputGateResumeLogContext {
        session_id,
        started_at,
        timing,
        status,
        output_state: output_scheduler.playback_output_state,
        queued_video_frames: output_scheduler.scheduled_video_queue.len(),
        pending_audio_frames: output_scheduler.pending_start_audio.len(),
        waterline,
    }
}

fn finish_output_gate_resume_timing(context: OutputGateResumeLogContext) -> OutputGateResumeStatus {
    let total = context.started_at.elapsed();
    tracing::trace!(
        session_id = ?context.session_id,
        status = ?context.status,
        output_state = ?context.output_state,
        total_ms = total.as_secs_f64() * 1000.0,
        audio_snapshot_ms = context.timing.audio_snapshot.as_secs_f64() * 1000.0,
        resume_decision_ms = context.timing.resume_decision.as_secs_f64() * 1000.0,
        demux_watermark_ms = context.timing.demux_watermark.as_secs_f64() * 1000.0,
        waterline_ms = context.timing.waterline.as_secs_f64() * 1000.0,
        fallback_ms = context.timing.fallback.as_secs_f64() * 1000.0,
        resume_action_ms = context.timing.resume_action.as_secs_f64() * 1000.0,
        wait_log_ms = context.timing.wait_log.as_secs_f64() * 1000.0,
        queued_video_frames = context.queued_video_frames,
        pending_audio_frames = context.pending_audio_frames,
        waterline_ready = ?context.waterline.map(PlaybackResumeWaterline::ready),
        decoded_ready = ?context.waterline.map(PlaybackResumeWaterline::decoded_ready),
        target_ms = ?context.waterline.map(|waterline| waterline.target_nsecs as f64 / 1_000_000.0),
        decoded_video_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.demux_min_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        "FFmpeg output gate resume timing"
    );
    if total < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.audio_snapshot < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.resume_decision < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.demux_watermark < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.waterline < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.fallback < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.resume_action < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.wait_log < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return context.status;
    }
    tracing::debug!(
        session_id = ?context.session_id,
        status = ?context.status,
        output_state = ?context.output_state,
        total_ms = total.as_secs_f64() * 1000.0,
        audio_snapshot_ms = context.timing.audio_snapshot.as_secs_f64() * 1000.0,
        resume_decision_ms = context.timing.resume_decision.as_secs_f64() * 1000.0,
        demux_watermark_ms = context.timing.demux_watermark.as_secs_f64() * 1000.0,
        waterline_ms = context.timing.waterline.as_secs_f64() * 1000.0,
        fallback_ms = context.timing.fallback.as_secs_f64() * 1000.0,
        resume_action_ms = context.timing.resume_action.as_secs_f64() * 1000.0,
        wait_log_ms = context.timing.wait_log.as_secs_f64() * 1000.0,
        queued_video_frames = context.queued_video_frames,
        pending_audio_frames = context.pending_audio_frames,
        waterline_ready = ?context.waterline.map(PlaybackResumeWaterline::ready),
        decoded_ready = ?context.waterline.map(PlaybackResumeWaterline::decoded_ready),
        target_ms = ?context.waterline.map(|waterline| waterline.target_nsecs as f64 / 1_000_000.0),
        decoded_video_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.demux_min_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        "FFmpeg output gate resume completed slowly"
    );
    context.status
}

fn timed_output_gate_demux_watermark<F>(
    demux_watermark: &mut F,
    timing: &mut OutputGateResumeTiming,
) -> DemuxReaderWatermark
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let watermark = demux_watermark();
    timing.demux_watermark += started_at.elapsed();
    watermark
}

#[allow(clippy::too_many_arguments)]
fn service_initial_video_clock_until_audio_start(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    delayed_audio_start_timeline_nsecs: u64,
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
    let Some((first_video_timeline_nsecs, _)) =
        output_scheduler.scheduled_video_queue.range_nsecs()
    else {
        return Ok(OutputGateResumeStatus::Waiting);
    };

    if output_scheduler
        .initial_delayed_audio_start_timeline_nsecs
        .is_none()
    {
        output_scheduler.initial_delayed_audio_start_timeline_nsecs =
            Some(delayed_audio_start_timeline_nsecs);
        output_scheduler.set_state(PlaybackOutputState::Ready);
        if first_video_timeline_nsecs > *current_start_position_nsecs {
            *current_start_position_nsecs = first_video_timeline_nsecs;
            subtitle_pipeline.reset_cues_for_position(first_video_timeline_nsecs);
            buffered_reporter.reset_to(
                nsecs_to_seconds(first_video_timeline_nsecs),
                session_id,
                event_tx,
            );
        }
        scheduler.reset(first_video_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            first_video_timeline_nsecs,
            delayed_audio_start_timeline_nsecs,
            delayed_audio_start_gap_ms = delayed_audio_start_timeline_nsecs
                .saturating_sub(first_video_timeline_nsecs) as f64
                / 1_000_000.0,
            silence_fill_reason = "not_filled",
            clock_mode = AudioClockMode::SyncingVideo.as_str(),
            "starting video-clocked initial playback until first FFmpeg audio frame"
        );
    }

    let mut presented_video_frames = 0usize;
    while let Some((front_timeline_nsecs, _)) = output_scheduler.scheduled_video_queue.range_nsecs()
    {
        if front_timeline_nsecs >= delayed_audio_start_timeline_nsecs
            || !scheduler.ready_for(front_timeline_nsecs)
            || control.should_interrupt()
        {
            break;
        }

        let Some(frame) = output_scheduler.scheduled_video_queue.pop_front() else {
            break;
        };
        let timeline_nsecs = frame.timeline_nsecs;
        let duration_nsecs = frame.duration_nsecs;
        subtitle_pipeline.update_overlay(timeline_nsecs, session_id, event_tx);
        present_video_frame_to_vo(
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
        presented_video_frames = presented_video_frames.saturating_add(1);
    }

    if !scheduler.ready_for(delayed_audio_start_timeline_nsecs) || control.should_interrupt() {
        if presented_video_frames > 0 {
            tracing::trace!(
                session_id = ?session_id,
                presented_video_frames,
                delayed_audio_start_timeline_nsecs,
                clock_mode = AudioClockMode::SyncingVideo.as_str(),
                "presented initial FFmpeg video frames while waiting for first audio PTS"
            );
        }
        return Ok(OutputGateResumeStatus::Waiting);
    }

    output.reset_clock(delayed_audio_start_timeline_nsecs);
    let audio_flush_until_timeline_nsecs = output_scheduler
        .scheduled_video_queue
        .buffered_until_from_nsecs(delayed_audio_start_timeline_nsecs)
        .or_else(|| {
            output_scheduler
                .pending_start_audio
                .buffered_until_from(delayed_audio_start_timeline_nsecs)
        })
        .unwrap_or(delayed_audio_start_timeline_nsecs);
    flush_pending_start_audio(
        &mut output_scheduler.pending_start_audio,
        output,
        delayed_audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        AudioClockMode::AudioStarted,
        DelayedAudioStartSilencePolicy::Skip,
        control,
        &mut output_scheduler.scheduled_video_queue,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;
    output_scheduler.defer_next_pending_start_audio_flush();
    output_scheduler.initial_delayed_audio_start_timeline_nsecs = None;
    output_scheduler.set_state(PlaybackOutputState::Playing);
    tracing::debug!(
        session_id = ?session_id,
        presented_video_frames,
        delayed_audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        pending_audio_frames = output_scheduler.pending_start_audio.len(),
        pending_audio_ms = output_scheduler.pending_start_audio.buffered_duration().as_secs_f64()
            * 1000.0,
        clock_mode = AudioClockMode::AudioStarted.as_str(),
        "started native audio output after video-clocked initial gap"
    );
    Ok(OutputGateResumeStatus::Resumed)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_output_gate_resume_if_ready<F>(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: Option<&AudioOutput>,
    demux_cache: Option<&DemuxPacketCache>,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    fallback_timeline_nsecs: u64,
    current_start_position_nsecs: &mut u64,
    scheduler: &mut PlaybackScheduler,
    output_resource_pressure: bool,
    mut demux_watermark: F,
) -> std::result::Result<OutputGateResumeStatus, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let mut timing = OutputGateResumeTiming::default();
    let Some(output) = output else {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    };
    if !output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        && !output_scheduler.playback_output_state.rebuffering()
    {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    }
    if output_scheduler.scheduled_video_queue.is_empty() {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    }

    let needs_prefetch = subtitle_pipeline.needs_prefetch();
    let stage_started_at = Instant::now();
    let audio_snapshot = output.snapshot()?;
    timing.audio_snapshot = stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    let previous_audio_played_until = audio_snapshot.played_timeline_nsecs;
    let rebuffer_anchor = output_scheduler
        .playback_output_state
        .rebuffering()
        .then_some(output_scheduler.video_output_rebuffer_anchor)
        .flatten();
    let resume_audio_played_until = rebuffer_anchor
        .map(|anchor| anchor.timeline_nsecs)
        .unwrap_or(previous_audio_played_until);
    let audio_output_buffered_until_nsecs = if output_scheduler.playback_output_state.rebuffering()
    {
        Some(audio_snapshot.buffered_until_timeline_nsecs)
    } else {
        None
    };
    let resume_decision = if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        output_scheduler
            .scheduled_video_queue
            .initial_audio_clock_resume_decision(
                &output_scheduler.pending_start_audio,
                previous_audio_played_until,
            )
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_audio_clock_resume_decision(
                &output_scheduler.pending_start_audio,
                resume_audio_played_until,
                audio_output_buffered_until_nsecs,
                rebuffer_anchor
                    .is_some_and(|anchor| anchor.reset_to_video_when_decoded_queue_misses_anchor),
            )
    }
    .unwrap_or(AudioClockResumeDecision {
        timeline_nsecs: fallback_timeline_nsecs,
        reset_audio_to_video: false,
    });
    let resume_audio_output_buffered_until_nsecs =
        audio_output_buffered_until_for_resume(resume_decision, audio_output_buffered_until_nsecs);
    timing.resume_decision = stage_started_at.elapsed();

    let waterline_demux_watermark =
        timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
    let stage_started_at = Instant::now();
    let mut waterline = if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        output_scheduler
            .scheduled_video_queue
            .initial_playback_resume_waterline(
                &output_scheduler.pending_start_audio,
                resume_decision.timeline_nsecs,
                waterline_demux_watermark,
                needs_prefetch,
                true,
            )
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_playback_resume_waterline_with_resource_pressure(
                &output_scheduler.pending_start_audio,
                resume_decision.timeline_nsecs,
                waterline_demux_watermark,
                resume_audio_output_buffered_until_nsecs,
                needs_prefetch,
                true,
                output_resource_pressure,
            )
    };
    timing.waterline = stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        && !waterline.ready()
        && waterline.decoded_ready()
        && output_scheduler
            .startup_sync_elapsed()
            .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER)
    {
        tracing::debug!(
            session_id = ?session_id,
            startup_wait_ms = output_scheduler
                .startup_sync_elapsed()
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
            decoded_video_ms = ?waterline
                .decoded_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            decoded_audio_ms = ?waterline
                .decoded_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_min_ms = ?waterline
                .demux_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_video_ms = ?waterline
                .demux_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_ms = ?waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "startup output gate demux waterline timed out; allowing decoded queues to start"
        );
        waterline.demux_ready = true;
    }
    timing.fallback += stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler.playback_output_state.rebuffering() && !waterline.ready() {
        let cache_pause_waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            output_scheduler.rebuffer_wait_elapsed(),
            demux_cache.is_some() && control.is_cache_paused(),
        );
        if cache_pause_waterline.ready() {
            if let Some(demux_cache) = demux_cache {
                demux_cache.clear_cache_pause_for_decoded_resume();
            }
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                target_ms = cache_pause_waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?cache_pause_waterline
                    .decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?cache_pause_waterline
                    .decoded_audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?cache_pause_waterline
                    .demux_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "rebuffer cache pause stalled with decoded queues ready; resuming from decoded waterline"
            );
            waterline = cache_pause_waterline;
        }
    }
    timing.fallback += stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler.playback_output_state.rebuffering() && !waterline.ready() {
        let stalled_waterline = rebuffer_playback_resume_waterline_after_prolonged_wait(
            waterline,
            output_scheduler.rebuffer_wait_elapsed(),
        );
        if stalled_waterline.ready() {
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                original_target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                target_ms = stalled_waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?stalled_waterline
                    .decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?stalled_waterline
                    .decoded_audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?stalled_waterline
                    .demux_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "rebuffer output gate waited for stable decoded video target; resuming with available queues"
            );
        }
        waterline = stalled_waterline;
    }
    timing.fallback += stage_started_at.elapsed();
    if waterline.ready()
        && output_scheduler
            .playback_output_state
            .first_video_frame_pending()
        && let Some(delayed_audio_start_timeline_nsecs) =
            initial_delayed_audio_start_timeline_nsecs(output_scheduler, resume_decision)
    {
        let stage_started_at = Instant::now();
        let status = service_initial_video_clock_until_audio_start(
            output_scheduler,
            output,
            delayed_audio_start_timeline_nsecs,
            control,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
            current_start_position_nsecs,
            scheduler,
        )?;
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                status,
                Some(waterline),
            ),
        ));
    }
    if waterline.ready()
        && output_scheduler
            .playback_output_state
            .first_video_frame_pending()
    {
        let stage_started_at = Instant::now();
        output_scheduler.set_state(PlaybackOutputState::Ready);
        let Some(first_video) = present_first_queued_video_frame(
            output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            |first_video, output_scheduler| {
                if first_video.timeline_nsecs > *current_start_position_nsecs {
                    *current_start_position_nsecs = first_video.timeline_nsecs;
                    scheduler.reset(first_video.timeline_nsecs);
                    subtitle_pipeline.reset_cues_for_position(first_video.timeline_nsecs);
                    buffered_reporter.reset_to(
                        nsecs_to_seconds(first_video.timeline_nsecs),
                        session_id,
                        event_tx,
                    );
                }
                output.reset_clock(first_video.timeline_nsecs);
                tracing::debug!(
                    session_id = ?session_id,
                    pts = first_video.timeline_nsecs,
                    output_scheduler.scheduled_video_queue =
                        output_scheduler.scheduled_video_queue.len(),
                    decoded_video_range =
                        ?output_scheduler.scheduled_video_queue.range_nsecs(),
                    queued_video_ms =
                        output_scheduler.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                    target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                    demux_min_ms = ?waterline
                        .demux_min_forward_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    reset_audio_to_video = resume_decision.reset_audio_to_video,
                    "presenting first FFmpeg video frame from output gate"
                );
            },
        ) else {
            timing.resume_action += stage_started_at.elapsed();
            return Ok(finish_output_gate_resume_timing(
                output_gate_resume_log_context(
                    output_scheduler,
                    session_id,
                    started_at,
                    timing,
                    OutputGateResumeStatus::Waiting,
                    Some(waterline),
                ),
            ));
        };
        let audio_start_timeline_nsecs = first_video.timeline_nsecs;
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or_else(|| {
                first_video
                    .timeline_nsecs
                    .saturating_add(first_video.duration_nsecs)
            });
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::SyncingVideo,
            DelayedAudioStartSilencePolicy::Skip,
            control,
            &mut output_scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        output_scheduler.defer_next_pending_start_audio_flush();
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Resumed,
                Some(waterline),
            ),
        ));
    }
    if waterline.ready()
        && output_scheduler.playback_output_state.rebuffering()
        && output_scheduler.finish_rebuffer_if_ready(waterline, session_id)
    {
        let stage_started_at = Instant::now();
        // Keep pre-resume video while the restart waterline is still filling;
        // dropping it early can prevent the decoded window from ever catching up.
        discard_decoded_video_before_output_gate_resume_if_ready(
            output_scheduler,
            waterline,
            resume_decision,
            session_id,
            previous_audio_played_until,
            rebuffer_anchor,
        );
        let audio_start_timeline_nsecs = if resume_decision.reset_audio_to_video {
            output.reset_clock(resume_decision.timeline_nsecs);
            resume_decision.timeline_nsecs
        } else {
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot)
                .max(resume_decision.timeline_nsecs)
        };
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or(audio_start_timeline_nsecs);
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::UnderrunRecovery,
            DelayedAudioStartSilencePolicy::Skip,
            control,
            &mut output_scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        output_scheduler.defer_next_pending_start_audio_flush();
        control.set_output_rebuffer_paused(false);
        output_scheduler.set_state(PlaybackOutputState::Playing);
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Resumed,
                Some(waterline),
            ),
        ));
    }
    if !waterline.ready() {
        let demux_watermark = timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
        let stage_started_at = Instant::now();
        output_scheduler
            .scheduled_video_queue
            .log_resume_waterline_wait(
                session_id,
                "output_gate",
                output_scheduler.playback_output_state,
                resume_decision.timeline_nsecs,
                &output_scheduler.pending_start_audio,
                waterline,
                demux_watermark,
            );
        timing.wait_log += stage_started_at.elapsed();
    }
    if waterline.decoded_ready() && !waterline.demux_ready {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::WaitingForDemux,
                Some(waterline),
            ),
        ));
    }
    Ok(finish_output_gate_resume_timing(
        output_gate_resume_log_context(
            output_scheduler,
            session_id,
            started_at,
            timing,
            OutputGateResumeStatus::Waiting,
            Some(waterline),
        ),
    ))
}

fn discard_decoded_video_before_output_gate_resume_if_ready(
    output_scheduler: &mut PlaybackOutputScheduler,
    waterline: PlaybackResumeWaterline,
    resume_decision: AudioClockResumeDecision,
    session_id: PlaybackSessionId,
    previous_audio_played_until: u64,
    rebuffer_anchor: Option<RebufferResumeAnchor>,
) -> usize {
    if !waterline.ready() {
        return 0;
    }
    let dropped_resume_video_frames = output_scheduler
        .scheduled_video_queue
        .discard_before(resume_decision.timeline_nsecs);
    if dropped_resume_video_frames > 0 {
        tracing::debug!(
            session_id = ?session_id,
            dropped_resume_video_frames,
            previous_audio_played_until,
            rebuffer_anchor_timeline_nsecs = rebuffer_anchor.map(|anchor| anchor.timeline_nsecs),
            resume_timeline_nsecs = resume_decision.timeline_nsecs,
            reset_audio_to_video = resume_decision.reset_audio_to_video,
            output_scheduler.playback_output_state = ?output_scheduler.playback_output_state,
            "discarded decoded FFmpeg video before output gate resume"
        );
    }
    dropped_resume_video_frames
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::player::render_host::{
        DecodedFrame, FramePixels, FramePts, PlaybackSessionId, RenderSize,
    };

    use super::super::{
        DEFAULT_VIDEO_FRAME_DURATION_NSECS, QueuedVideoFrame, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    };
    use super::{
        AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioClockResumeDecision,
        AudioOutputSnapshot, DecodedAudio, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
        PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PendingStartAudioPressureLevel,
        PlaybackOutputScheduler, PlaybackOutputState, PlaybackResumeWaterline,
        audio_output_contiguous_start_timeline_nsecs, audio_output_flush_until_timeline_nsecs,
        discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
        initial_delayed_audio_start_timeline_nsecs, playing_pending_audio_limit_duration,
        playing_pending_audio_pressure_clear_duration,
    };

    fn test_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        }
    }

    fn resume_decision() -> AudioClockResumeDecision {
        AudioClockResumeDecision {
            timeline_nsecs: 4_608_000_000,
            reset_audio_to_video: true,
        }
    }

    fn waterline(decoded_video_ready: bool) -> PlaybackResumeWaterline {
        PlaybackResumeWaterline {
            target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            decoded_video_forward_nsecs: decoded_video_ready
                .then_some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            decoded_audio_forward_nsecs: Some(duration_nsecs(
                VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
            )),
            delayed_audio_start_gap_nsecs: None,
            demux_video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            demux_audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            demux_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            decoded_video_ready,
            decoded_audio_ready: true,
            demux_ready: true,
        }
    }

    fn audio_snapshot(played_timeline_nsecs: u64, total_pending_nsecs: u64) -> AudioOutputSnapshot {
        AudioOutputSnapshot {
            played_timeline_nsecs,
            buffered_until_timeline_nsecs: played_timeline_nsecs
                .saturating_add(total_pending_nsecs),
            shared_pending_nsecs: total_pending_nsecs,
            queue_pending_nsecs: 0,
            total_pending_nsecs,
            queue_frames: 0,
            queue_generation: 0,
        }
    }

    #[test]
    fn audio_output_flush_until_caps_total_pending_audio() {
        let snapshot = audio_snapshot(10_000_000_000, 0);
        let video_lead_until = 12_000_000_000;

        assert_eq!(
            audio_output_flush_until_timeline_nsecs(snapshot, video_lead_until),
            10_000_000_000 + duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT)
        );
    }

    #[test]
    fn audio_output_flush_until_stops_when_output_already_past_limit() {
        let snapshot = audio_snapshot(10_000_000_000, duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT) + 1);
        let video_lead_until = 12_000_000_000;

        assert!(
            audio_output_flush_until_timeline_nsecs(snapshot, video_lead_until)
                < audio_output_contiguous_start_timeline_nsecs(snapshot)
        );
    }

    #[test]
    fn initial_delayed_audio_start_detects_video_clock_gap() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 20_000_000,
            },
            1_022_780_000,
            1_042_780_000,
        );

        assert_eq!(
            initial_delayed_audio_start_timeline_nsecs(
                &scheduler,
                AudioClockResumeDecision {
                    timeline_nsecs: 1_022_780_000,
                    reset_audio_to_video: false,
                },
            ),
            Some(1_022_780_000)
        );
    }

    #[test]
    fn initial_delayed_audio_start_ignores_small_continuity_gap() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 20_000_000,
            },
            1_000_001_000,
            1_020_001_000,
        );

        assert_eq!(
            initial_delayed_audio_start_timeline_nsecs(
                &scheduler,
                AudioClockResumeDecision {
                    timeline_nsecs: 1_000_000_000,
                    reset_audio_to_video: false,
                },
            ),
            None
        );
    }

    #[test]
    fn playing_pending_audio_pressure_levels_follow_steady_state_thresholds() {
        assert_eq!(
            playing_pending_audio_limit_duration(),
            AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
        );
        assert_eq!(
            PendingStartAudioPressureLevel::from_duration(
                playing_pending_audio_limit_duration() - Duration::from_nanos(1)
            ),
            PendingStartAudioPressureLevel::Normal
        );
        assert_eq!(
            PendingStartAudioPressureLevel::from_duration(playing_pending_audio_limit_duration()),
            PendingStartAudioPressureLevel::Warn
        );
        assert_eq!(
            PendingStartAudioPressureLevel::from_duration(
                PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION
            ),
            PendingStartAudioPressureLevel::ForceRecovery
        );
        assert_eq!(
            PendingStartAudioPressureLevel::from_duration(
                PLAYING_PENDING_AUDIO_HARD_RESET_DURATION
            ),
            PendingStartAudioPressureLevel::HardReset
        );
    }

    #[test]
    fn playing_pending_audio_pressure_uses_clear_hysteresis() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Playing);
        let limit = playing_pending_audio_limit_duration();
        let clear_duration = playing_pending_audio_pressure_clear_duration();
        assert!(clear_duration < limit);

        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: duration_nsecs(limit) + 1,
            },
            1_000_000_000,
            1_000_000_000 + duration_nsecs(limit) + 1,
        );
        scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
        assert_eq!(
            scheduler.pending_start_audio_pressure_level,
            PendingStartAudioPressureLevel::Warn
        );

        scheduler.pending_start_audio.clear();
        let near_limit = limit - Duration::from_millis(1);
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: duration_nsecs(near_limit),
            },
            2_000_000_000,
            2_000_000_000 + duration_nsecs(near_limit),
        );
        scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
        assert_eq!(
            scheduler.pending_start_audio_pressure_level,
            PendingStartAudioPressureLevel::Warn
        );

        scheduler.pending_start_audio.clear();
        let cleared = clear_duration - Duration::from_nanos(1);
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: duration_nsecs(cleared),
            },
            3_000_000_000,
            3_000_000_000 + duration_nsecs(cleared),
        );
        scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
        assert_eq!(
            scheduler.pending_start_audio_pressure_level,
            PendingStartAudioPressureLevel::Normal
        );
    }

    #[test]
    fn pending_start_audio_can_recover_playing_audio_output() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Playing);
        scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
        scheduler.push_decoded_video_for_test(test_queued_video_frame(1_300_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 300_000_000,
            },
            1_000_000_000,
            1_300_000_000,
        );

        assert!(
            scheduler
                .pending_start_audio_can_recover_output(Some(audio_snapshot(1_000_000_000, 0)))
        );
    }

    #[test]
    fn output_gate_keeps_pre_resume_video_until_waterline_ready() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        scheduler.push_decoded_video_for_test(test_queued_video_frame(4_400_000_000));

        let dropped = discard_decoded_video_before_output_gate_resume_if_ready(
            &mut scheduler,
            waterline(false),
            resume_decision(),
            PlaybackSessionId(1),
            4_423_755_102,
            None,
        );

        assert_eq!(dropped, 0);
        assert_eq!(scheduler.scheduled_video_queue.len(), 1);
        assert_eq!(
            scheduler.scheduled_video_queue.range_nsecs(),
            Some((
                4_400_000_000,
                4_400_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS
            ))
        );
    }

    #[test]
    fn output_gate_discards_pre_resume_video_once_waterline_ready() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        scheduler.push_decoded_video_for_test(test_queued_video_frame(4_400_000_000));

        let dropped = discard_decoded_video_before_output_gate_resume_if_ready(
            &mut scheduler,
            waterline(true),
            resume_decision(),
            PlaybackSessionId(1),
            4_423_755_102,
            None,
        );

        assert_eq!(dropped, 1);
        assert!(scheduler.scheduled_video_queue.is_empty());
    }

    #[test]
    fn decoded_audio_direct_push_requires_video_coverage_at_audio_start() {
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Playing);
        scheduler.push_decoded_video_for_test(test_queued_video_frame(8_840_000_000));

        assert!(scheduler.decoded_audio_can_push_directly(
            8_860_000_000,
            9_100_000_000,
            8_860_000_000
        ));
        assert!(!scheduler.decoded_audio_can_push_directly(
            9_080_000_000,
            9_120_000_000,
            9_080_000_000
        ));
        assert!(!scheduler.decoded_audio_can_push_directly(
            8_860_000_000,
            9_100_000_000,
            9_000_000_000
        ));
    }
}
