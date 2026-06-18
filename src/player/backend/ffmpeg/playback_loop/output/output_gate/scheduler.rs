#[cfg(test)]
use super::DecodedAudio;
#[cfg(test)]
use super::QueuedVideoFrame;
#[cfg(test)]
use super::RebufferResumeAnchor;
use super::{
    AudioOutput, Duration, FfmpegControl, Instant, PendingStartAudio,
    PendingStartAudioPressureLevel, PlaybackOutputScheduler, PlaybackOutputState,
    PlaybackResumeWaterline, PlaybackSessionId, ScheduledVideoQueue, clear_video_output_rebuffer,
    enter_video_output_rebuffer, finish_video_output_rebuffer_if_ready,
    video_output_rebuffer_should_enter,
};

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

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn sync_first_video_frame_pending(
        &mut self,
    ) {
        self.first_video_frame_pending = self.playback_output_state.first_video_frame_pending();
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
            .then(|| {
                self.video_output_underrun_started_at
                    .map(|started| started.elapsed())
            })
            .flatten()
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
