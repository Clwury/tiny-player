#[cfg(test)]
use super::DecodedAudio;
#[cfg(test)]
use super::QueuedVideoFrame;
#[cfg(test)]
use super::RebufferResumeAnchor;
use super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_REBUFFER_LOOP_DETECTION_WINDOW, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AudioOutput, AudioOutputSnapshot, AudioResumeWaterline,
    Duration, FfmpegControl, Instant, PendingStartAudio, PendingStartAudioPressureLevel,
    PlaybackOutputScheduler, PlaybackOutputState, PlaybackResumeWaterline, PlaybackSessionId,
    RebufferAudioRealignRequest, ScheduledVideoQueue,
    VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VideoOutputUnderflowClassification,
    clear_video_output_rebuffer, duration_nsecs, enter_video_output_rebuffer,
    finish_video_output_rebuffer_if_ready, video_output_rebuffer_should_enter,
    video_output_underflow_classification,
};
use ffmpeg_sys_next as ffi;

const REBUFFER_EMPTY_AUDIO_OUTPUT_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_DROPS: u8 = 3;
const AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR: Duration = Duration::from_secs(2);

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
    pub(in crate::player::backend::ffmpeg) fn new() -> Self {
        let playback_output_state = PlaybackOutputState::Syncing;
        Self {
            scheduled_video_queue: ScheduledVideoQueue::default(),
            pending_start_audio: PendingStartAudio::default(),
            first_video_frame_pending: playback_output_state.first_video_frame_pending(),
            playback_output_state,
            video_output_underrun_started_at: None,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
            audio_sync_drop_before_timeline_nsecs: None,
            rebuffer_audio_realign_request: None,
            syncing_started_at: Some(Instant::now()),
            defer_pending_start_audio_flush_once: false,
            startup_pending_audio_pressure_context_active: false,
            pending_start_audio_pressure_level: PendingStartAudioPressureLevel::Normal,
            startup_first_frame_stall_logged: false,
            recent_audio_output_underrun_window_started_at: None,
            recent_audio_output_underruns: 0,
            rebuffer_far_ahead_audio_drop_count: 0,
            audio_gap_recovery_until: None,
            audio_gap_recovery_target_nsecs: None,
            initial_delayed_audio_start_timeline_nsecs: None,
            initial_audio_gap_at_video_start_timeline_nsecs: None,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn reset(&mut self, control: &FfmpegControl) {
        self.scheduled_video_queue.clear();
        self.pending_start_audio.clear();
        self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        self.startup_first_frame_stall_logged = false;
        self.initial_delayed_audio_start_timeline_nsecs = None;
        self.initial_audio_gap_at_video_start_timeline_nsecs = None;
        self.startup_pending_audio_pressure_context_active = false;
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.set_state(PlaybackOutputState::Syncing);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_bootstrap_after_seek = false;
        self.video_decode_underfill = false;
        self.rebuffer_empty_audio_output_blocked = false;
        self.audio_sync_drop_before_timeline_nsecs = None;
        self.rebuffer_audio_realign_request = None;
        self.rebuffer_far_ahead_audio_drop_count = 0;
        self.audio_gap_recovery_until = None;
        self.audio_gap_recovery_target_nsecs = None;
        self.recent_audio_output_underrun_window_started_at = None;
        self.recent_audio_output_underruns = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer(&mut self, control: &FfmpegControl) {
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        self.clear_video_bootstrap_after_seek("clear_rebuffer");
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_far_ahead_audio_drop_count = 0;
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
        if state == PlaybackOutputState::Syncing || !state.first_video_frame_pending() {
            self.startup_first_frame_stall_logged = false;
        }
        if state != PlaybackOutputState::Ready {
            self.initial_delayed_audio_start_timeline_nsecs = None;
        }
        if !state.first_video_frame_pending() {
            self.initial_audio_gap_at_video_start_timeline_nsecs = None;
        }
        if state != PlaybackOutputState::Playing {
            self.defer_pending_start_audio_flush_once = false;
            self.startup_pending_audio_pressure_context_active = false;
            self.pending_start_audio_pressure_level = PendingStartAudioPressureLevel::Normal;
        }
        if !state.rebuffering() {
            self.rebuffer_empty_audio_output_blocked = false;
            self.video_decode_underfill = false;
        }
        if state == PlaybackOutputState::Playing {
            self.rebuffer_far_ahead_audio_drop_count = 0;
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
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_far_ahead_audio_drop_count = 0;
        self.clear_video_bootstrap_after_seek("rebuffer_waterline_ready");
        self.sync_first_video_frame_pending();
        true
    }

    pub(in crate::player::backend::ffmpeg) fn observe_rebuffer_far_ahead_audio_frame(
        &mut self,
        far_ahead_audio_timeline_nsecs: u64,
        current_start_position_nsecs: u64,
        audio_output_pending_nsecs: Option<u64>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.playback_output_state.rebuffering()
            && !self.playback_output_state.first_video_frame_pending()
        {
            self.rebuffer_far_ahead_audio_drop_count = 0;
            return None;
        }
        self.rebuffer_far_ahead_audio_drop_count =
            self.rebuffer_far_ahead_audio_drop_count.saturating_add(1);

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
        let audio_output_empty = audio_output_pending_nsecs == Some(0);
        let realign_needed = audio_output_empty || !pending_audio_covers_target;
        let bypass_drop_threshold =
            self.rebuffer_empty_audio_output_blocked && self.playback_output_state.rebuffering();
        if (!bypass_drop_threshold
            && self.rebuffer_far_ahead_audio_drop_count
                < REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_DROPS)
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
                far_ahead_drop_count = self.rebuffer_far_ahead_audio_drop_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                realign_needed,
                bypass_drop_threshold,
                "observed FFmpeg rebuffer audio far ahead of video target"
            );
            return None;
        }

        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs,
            far_ahead_drop_count: self.rebuffer_far_ahead_audio_drop_count,
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
                far_ahead_drop_count = request.far_ahead_drop_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                bypass_drop_threshold,
                "requested FFmpeg rebuffer audio realign to video target"
            );
        }
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn request_rebuffer_audio_reader_head_realign_if_needed(
        &mut self,
        reader_head_start_nsecs: u64,
        audio_waterline: AudioResumeWaterline,
        current_start_position_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.rebuffer_empty_audio_output_blocked || !self.playback_output_state.rebuffering() {
            return None;
        }
        if self.rebuffer_audio_realign_request.is_some() || !audio_waterline.below_target() {
            return None;
        }
        let far_ahead_threshold_nsecs = audio_waterline
            .resume_timeline_nsecs
            .saturating_add(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));
        if reader_head_start_nsecs <= far_ahead_threshold_nsecs {
            return None;
        }

        let (first_video_timeline_nsecs, _) = self.scheduled_video_queue.range_nsecs()?;
        let pending_audio_covers_resume = self
            .pending_start_audio
            .buffered_until_from(audio_waterline.resume_timeline_nsecs)
            .is_some();
        let audio_output_empty = audio_waterline.audio_output_pending_nsecs == Some(0);
        let realign_needed = audio_output_empty || !pending_audio_covers_resume;
        if !realign_needed {
            tracing::debug!(
                session_id = ?session_id,
                reason = "rebuffer_audio_reader_far_ahead",
                reader_head_start_nsecs,
                resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
                far_ahead_threshold_nsecs,
                current_start_position_nsecs,
                first_video_timeline_nsecs,
                pending_audio_covers_resume,
                audio_output_pending_ms = ?audio_waterline
                    .audio_output_pending_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "observed FFmpeg rebuffer audio reader far ahead but pending audio covers resume"
            );
            return None;
        }

        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let queued_video_covers_resume = self
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_waterline.resume_timeline_nsecs)
            .is_some();
        let target_timeline_nsecs = if queued_video_covers_resume {
            audio_waterline.resume_timeline_nsecs
        } else {
            first_video_timeline_nsecs
        };
        let anchor_timeline_nsecs = self
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .unwrap_or(audio_waterline.resume_timeline_nsecs);
        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs: reader_head_start_nsecs,
            far_ahead_drop_count: 0,
            reason: "rebuffer_audio_reader_far_ahead",
        };
        self.rebuffer_audio_realign_request = Some(request);
        tracing::debug!(
            session_id = ?session_id,
            reason = request.reason,
            reader_head_start_nsecs,
            resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
            far_ahead_threshold_nsecs,
            current_start_position_nsecs,
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            queued_video_frames = self.scheduled_video_queue.len(),
            queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
            queued_video_range_nsecs = ?queued_video_range_nsecs,
            queued_video_covers_resume,
            pending_audio_start_nsecs = ?audio_waterline.pending_audio_start_nsecs,
            pending_audio_covers_resume,
            pending_audio_forward_ms = ?audio_waterline
                .pending_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            audio_output_pending_ms = ?audio_waterline
                .audio_output_pending_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?audio_waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "requested FFmpeg immediate rebuffer audio reader realign"
        );
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer_far_ahead_audio_observation(
        &mut self,
    ) {
        self.rebuffer_far_ahead_audio_drop_count = 0;
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
            self.rebuffer_far_ahead_audio_drop_count = 0;
        }
        request
    }

    pub(in crate::player::backend::ffmpeg) fn reset_audio_after_rebuffer_realign(
        &mut self,
        target_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let cleared_pending_audio_frames = self.pending_start_audio.len();
        let cleared_pending_audio_ms =
            self.pending_start_audio.buffered_duration().as_secs_f64() * 1000.0;
        self.pending_start_audio.clear();
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_audio_realign_request = None;
        self.rebuffer_far_ahead_audio_drop_count = 0;
        self.set_audio_sync_drop_before_timeline_nsecs(target_timeline_nsecs, session_id, reason);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            cleared_pending_audio_frames,
            cleared_pending_audio_ms,
            "reset FFmpeg audio output scheduler after rebuffer realign"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_far_ahead_reference_timeline_nsecs(
        &self,
        current_start_position_nsecs: u64,
    ) -> u64 {
        let resume_reference_nsecs = if self.playback_output_state.rebuffering() {
            self.rebuffer_audio_realign_target(current_start_position_nsecs)
                .map(|(target_timeline_nsecs, _, _)| target_timeline_nsecs)
                .or_else(|| {
                    self.video_output_rebuffer_anchor
                        .map(|anchor| anchor.timeline_nsecs)
                })
        } else if self.playback_output_state.first_video_frame_pending() {
            self.scheduled_video_queue
                .range_nsecs()
                .map(|(first_video_timeline_nsecs, _)| first_video_timeline_nsecs)
        } else {
            None
        };
        resume_reference_nsecs
            .unwrap_or(current_start_position_nsecs)
            .max(current_start_position_nsecs)
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
        let startup_or_restart = self.playback_output_state.first_video_frame_pending()
            || self.video_bootstrap_after_seek;
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
            .then(|| {
                self.video_output_underrun_started_at
                    .map(|started| started.elapsed())
            })
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
