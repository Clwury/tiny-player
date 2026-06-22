use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::player::render_host::PlaybackSessionId;

use super::pending_audio_queue::PendingStartAudio;
use super::playback_block::PlaybackBlockReason;
use super::scheduled_video_queue::{
    ScheduledVideoQueue, queued_video_buffered_until_from_nsecs, queued_video_forward_nsecs_from,
    queued_video_range_nsecs,
};
use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_OUTPUT_VIDEO_LEAD_DURATION,
    AUDIO_VIDEO_REBUFFER_DRIFT_RESET_THRESHOLD, AudioOutput, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DemuxReaderWatermark, FfmpegControl, PENDING_AUDIO_CONTINUITY_TOLERANCE, QueuedVideoFrame,
    VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_ENTER_AFTER, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_MIN_STABLE_RESUME_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER, VIDEO_OUTPUT_START_PREBUFFER_DURATION,
    VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, VIDEO_OUTPUT_UNDERRUN_FAST_RECOVERY_AFTER,
    duration_nsecs, queued_video_duration, queued_video_frames_have_vulkan,
    queued_video_limit_duration, queued_video_target_duration,
};
#[cfg(test)]
use super::{
    VIDEO_OUTPUT_REBUFFER_RESUME_FRAMES, VIDEO_OUTPUT_START_PREBUFFER_FRAMES,
    queued_video_target_frames,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum PlaybackOutputState {
    Syncing,
    Ready,
    Playing,
    Rebuffering,
}

impl PlaybackOutputState {
    pub(in crate::player::backend::ffmpeg) fn first_video_frame_pending(self) -> bool {
        matches!(self, Self::Syncing | Self::Ready)
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffering(self) -> bool {
        matches!(self, Self::Rebuffering)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioClockResumeDecision {
    pub(in crate::player::backend::ffmpeg) timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) reset_audio_to_video: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct InitialOutputSyncDecision {
    pub(in crate::player::backend::ffmpeg) video_resume_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) audio_start_timeline_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) delayed_audio_start_timeline_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) drop_audio_before_timeline_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) stale_audio_preroll_until_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) stale_audio_preroll_gap_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) allow_initial_audio_gap_at_video_start: bool,
    pub(in crate::player::backend::ffmpeg) reset_audio_to_video: bool,
}

impl InitialOutputSyncDecision {
    pub(in crate::player::backend::ffmpeg) fn audio_clock_resume_decision(
        self,
    ) -> AudioClockResumeDecision {
        AudioClockResumeDecision {
            timeline_nsecs: self.video_resume_timeline_nsecs,
            reset_audio_to_video: self.reset_audio_to_video,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct RebufferResumeAnchor {
    pub(in crate::player::backend::ffmpeg) timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) reset_to_video_when_decoded_queue_misses_anchor: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PlaybackResumeWaterline {
    pub(in crate::player::backend::ffmpeg) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) decoded_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) decoded_audio_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) delayed_audio_start_gap_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) demux_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) demux_audio_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) demux_min_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) decoded_video_ready: bool,
    pub(in crate::player::backend::ffmpeg) decoded_audio_ready: bool,
    pub(in crate::player::backend::ffmpeg) demux_ready: bool,
}

impl PlaybackResumeWaterline {
    pub(in crate::player::backend::ffmpeg) fn ready(self) -> bool {
        self.decoded_video_ready && self.decoded_audio_ready && self.demux_ready
    }

    pub(in crate::player::backend::ffmpeg) fn decoded_ready(self) -> bool {
        self.decoded_video_ready && self.decoded_audio_ready
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PlaybackResumeWaterlineOptions {
    audio_output_buffered_until_nsecs: Option<u64>,
    allow_delayed_audio_start: bool,
    initial_delayed_audio_start_timeline_nsecs: Option<u64>,
    allow_initial_audio_gap_at_video_start: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DelayedStartDecodedAudioForward {
    forward_nsecs: u64,
    gap_nsecs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RebufferAudioAnchorResetContext {
    reset_to_video_when_decoded_queue_misses_anchor: bool,
    first_video_nsecs: u64,
    played_until_nsecs: u64,
    decoded_video_buffered_until_nsecs: Option<u64>,
    audio_output_buffered_until_nsecs: Option<u64>,
    stable_resume_nsecs: u64,
}

#[derive(Clone, Copy, Debug)]
struct VideoOutputRebufferEntryContext {
    underrun_started_at: Option<Instant>,
    now: Instant,
    video_output_underflowing: bool,
    queued_video_forward_nsecs: Option<u64>,
    output_underrun: bool,
    demux_cache_insufficient: bool,
    demux_min_forward_nsecs: Option<u64>,
    render_backlogged: bool,
    has_audio_output: bool,
    pending_audio_recoverable: bool,
    output_state: PlaybackOutputState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VideoOutputRebufferEntryDecision {
    underrun_started_at: Option<Instant>,
    should_enter: bool,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_should_enter(
    underrun_started_at: &mut Option<Instant>,
    now: Instant,
    video_output_underflowing: bool,
    queued_video_forward_nsecs: Option<u64>,
    output_underrun: bool,
    demux_cache_insufficient: bool,
    demux_min_forward_nsecs: Option<u64>,
    render_backlogged: bool,
    has_audio_output: bool,
    pending_audio_recoverable: bool,
    output_state: PlaybackOutputState,
) -> bool {
    let decision = video_output_rebuffer_entry_decision(VideoOutputRebufferEntryContext {
        underrun_started_at: *underrun_started_at,
        now,
        video_output_underflowing,
        queued_video_forward_nsecs,
        output_underrun,
        demux_cache_insufficient,
        demux_min_forward_nsecs,
        render_backlogged,
        has_audio_output,
        pending_audio_recoverable,
        output_state,
    });
    *underrun_started_at = decision.underrun_started_at;
    decision.should_enter
}

fn video_output_rebuffer_entry_decision(
    context: VideoOutputRebufferEntryContext,
) -> VideoOutputRebufferEntryDecision {
    if context.output_state.rebuffering() {
        return VideoOutputRebufferEntryDecision {
            underrun_started_at: context.underrun_started_at,
            should_enter: false,
        };
    }
    let demux_forward_low_water = context.demux_min_forward_nsecs.is_none_or(|duration| {
        duration <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
    });
    let queued_video_near_drain = context
        .queued_video_forward_nsecs
        .is_none_or(|duration| duration <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));
    if !context.render_backlogged
        && context.has_audio_output
        && context.demux_cache_insufficient
        && demux_forward_low_water
        && queued_video_near_drain
    {
        return VideoOutputRebufferEntryDecision {
            underrun_started_at: Some(context.underrun_started_at.unwrap_or(context.now)),
            should_enter: true,
        };
    }
    if !context.video_output_underflowing
        || context.render_backlogged
        || !context.has_audio_output
        || (!context.demux_cache_insufficient && !context.output_underrun)
    {
        return VideoOutputRebufferEntryDecision {
            underrun_started_at: None,
            should_enter: false,
        };
    }

    let started_at = context.underrun_started_at.unwrap_or(context.now);
    let underrun_elapsed = context.now.saturating_duration_since(started_at);
    let should_enter = if context.output_underrun
        && !context.demux_cache_insufficient
        && context.pending_audio_recoverable
    {
        underrun_elapsed >= VIDEO_OUTPUT_UNDERRUN_FAST_RECOVERY_AFTER
    } else {
        context.output_underrun || underrun_elapsed >= VIDEO_OUTPUT_REBUFFER_ENTER_AFTER
    };
    VideoOutputRebufferEntryDecision {
        underrun_started_at: Some(started_at),
        should_enter,
    }
}

pub(in crate::player::backend::ffmpeg) fn demux_reader_ready_for_output(
    demux_watermark: DemuxReaderWatermark,
    has_audio_output: bool,
) -> bool {
    let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    let demux_min_forward_nsecs = if has_audio_output {
        demux_watermark
            .video_forward_nsecs
            .zip(demux_watermark.audio_forward_nsecs)
            .map(|(video, audio)| video.min(audio))
    } else {
        demux_watermark.video_forward_nsecs
    }
    .or(demux_watermark.selected_min_forward_nsecs);
    let demux_underrun = demux_watermark.underrun
        || demux_watermark.video_underrun
        || (has_audio_output && demux_watermark.audio_underrun);
    let demux_idle = demux_watermark.idle
        || (demux_watermark.video_idle && (!has_audio_output || demux_watermark.audio_idle));
    !demux_underrun
        && (demux_idle || demux_min_forward_nsecs.is_some_and(|duration| duration >= target_nsecs))
}

pub(in crate::player::backend::ffmpeg) fn should_block_for_demux_read(
    output_state: PlaybackOutputState,
) -> bool {
    output_state.first_video_frame_pending() || output_state.rebuffering()
}

pub(in crate::player::backend::ffmpeg) fn video_decode_should_skip_nonref_for_pressure(
    output_state: PlaybackOutputState,
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    played_until_nsecs: Option<u64>,
    has_audio_output: bool,
    audio_output_pending_nsecs: Option<u64>,
    skip_nonref_active: bool,
) -> bool {
    if !has_audio_output || output_state.first_video_frame_pending() {
        return false;
    }
    if output_state.rebuffering() {
        return false;
    }

    let Some(played_until_nsecs) = played_until_nsecs else {
        return false;
    };
    let low_water_duration = if queued_video_frames_have_vulkan(queued_video_frames) {
        VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION
    } else {
        VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION
    };
    let pressure_duration =
        if skip_nonref_active && queued_video_frames_have_vulkan(queued_video_frames) {
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
        } else {
            low_water_duration
        };
    let audio_output_low_water = audio_output_pending_nsecs
        .is_some_and(|pending| pending < duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION));
    queued_video_forward_nsecs_from(queued_video_frames, played_until_nsecs).is_none_or(
        |forward_nsecs| {
            forward_nsecs <= duration_nsecs(pressure_duration)
                || (audio_output_low_water
                    && forward_nsecs <= duration_nsecs(VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION))
        },
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_resume_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> Duration {
    video_output_rebuffer_resume_duration_with_resource_pressure(
        queued_video_frames,
        needs_prefetch,
        false,
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_resume_duration_with_resource_pressure(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
    output_resource_pressure: bool,
) -> Duration {
    video_output_rebuffer_resume_duration_from_timeline(
        queued_video_frames,
        needs_prefetch,
        output_resource_pressure,
        None,
    )
}

fn video_output_rebuffer_resume_duration_from_timeline(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
    output_resource_pressure: bool,
    resume_timeline_nsecs: Option<u64>,
) -> Duration {
    let target_duration =
        video_output_rebuffer_target_duration(queued_video_frames, needs_prefetch);
    if !output_resource_pressure {
        return target_duration;
    }
    let Some(resource_budget_duration) =
        video_output_rebuffer_resource_budget_duration(queued_video_frames, resume_timeline_nsecs)
    else {
        return target_duration;
    };
    // Resource pressure may cap excessive prebuffering, but sub-second resumes
    // are too easy for the audio clock to consume before the decoder catches up.
    let minimum_stable_duration =
        target_duration.min(VIDEO_OUTPUT_REBUFFER_MIN_STABLE_RESUME_DURATION);
    target_duration.min(resource_budget_duration.max(minimum_stable_duration))
}

fn video_output_rebuffer_target_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> Duration {
    queued_video_target_duration(queued_video_frames, needs_prefetch)
        .max(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        .min(queued_video_limit_duration(
            queued_video_frames,
            needs_prefetch,
        ))
}

fn video_output_rebuffer_resource_budget_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    resume_timeline_nsecs: Option<u64>,
) -> Option<Duration> {
    if !queued_video_frames_have_vulkan(queued_video_frames) {
        return None;
    }

    let timeline_nsecs = resume_timeline_nsecs
        .unwrap_or_else(|| queued_video_frames.front().unwrap().timeline_nsecs);
    let budget_nsecs = queued_video_forward_nsecs_from(queued_video_frames, timeline_nsecs)?;
    (budget_nsecs > 0).then_some(Duration::from_nanos(budget_nsecs))
}

pub(in crate::player::backend::ffmpeg) fn video_output_start_prebuffer_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> Duration {
    queued_video_target_duration(queued_video_frames, needs_prefetch)
        .min(VIDEO_OUTPUT_START_PREBUFFER_DURATION)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_resume_frames(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> usize {
    queued_video_target_frames(queued_video_frames, needs_prefetch)
        .min(VIDEO_OUTPUT_REBUFFER_RESUME_FRAMES)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn video_output_start_prebuffer_frames(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> usize {
    queued_video_target_frames(queued_video_frames, needs_prefetch)
        .min(VIDEO_OUTPUT_START_PREBUFFER_FRAMES)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_resume_reached(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> bool {
    if queued_video_frames.is_empty() {
        return false;
    }
    let Some(decoded_video_forward_nsecs) = queued_video_forward_nsecs(queued_video_frames) else {
        return queued_video_frames.len()
            >= video_output_rebuffer_resume_frames(queued_video_frames, needs_prefetch);
    };
    decoded_video_forward_nsecs
        >= duration_nsecs(video_output_rebuffer_resume_duration(
            queued_video_frames,
            needs_prefetch,
        ))
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn decoded_video_start_prebuffer_reached(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> bool {
    if queued_video_frames.is_empty() {
        return false;
    }
    let Some(decoded_video_forward_nsecs) = queued_video_forward_nsecs(queued_video_frames) else {
        return queued_video_frames.len()
            >= video_output_start_prebuffer_frames(queued_video_frames, needs_prefetch);
    };
    decoded_video_forward_nsecs
        >= duration_nsecs(video_output_start_prebuffer_duration(
            queued_video_frames,
            needs_prefetch,
        ))
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn audio_clock_resume_timeline_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
) -> Option<u64> {
    audio_clock_resume_decision(queued_video_frames, pending_audio, played_until_nsecs)
        .map(|decision| decision.timeline_nsecs)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn audio_clock_resume_decision(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
) -> Option<AudioClockResumeDecision> {
    let first_video_nsecs = queued_video_frames.front()?.timeline_nsecs;
    let first_audio_nsecs = pending_audio
        .first_start_timeline_nsecs()
        .unwrap_or(first_video_nsecs);
    let audio_video_start_nsecs = first_video_nsecs.max(first_audio_nsecs);
    if played_until_nsecs.saturating_sub(audio_video_start_nsecs)
        > duration_nsecs(AUDIO_VIDEO_REBUFFER_DRIFT_RESET_THRESHOLD)
    {
        Some(AudioClockResumeDecision {
            timeline_nsecs: audio_video_start_nsecs,
            reset_audio_to_video: true,
        })
    } else {
        Some(AudioClockResumeDecision {
            timeline_nsecs: audio_video_start_nsecs.max(played_until_nsecs),
            reset_audio_to_video: false,
        })
    }
}

pub(in crate::player::backend::ffmpeg) fn rebuffer_audio_clock_resume_decision(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
    audio_output_buffered_until_nsecs: Option<u64>,
    reset_to_video_when_decoded_queue_misses_anchor: bool,
) -> Option<AudioClockResumeDecision> {
    let first_video_nsecs = queued_video_frames.front()?.timeline_nsecs;
    let first_audio_nsecs = pending_audio
        .first_start_timeline_nsecs()
        .unwrap_or(first_video_nsecs);
    let audio_video_start_nsecs = first_video_nsecs.max(first_audio_nsecs);
    if reset_to_video_when_decoded_queue_misses_anchor && first_video_nsecs < played_until_nsecs {
        let decoded_video_buffered_until_nsecs =
            queued_video_buffered_until_from_nsecs(queued_video_frames, played_until_nsecs);
        if rebuffer_audio_anchor_reset_required(RebufferAudioAnchorResetContext {
            reset_to_video_when_decoded_queue_misses_anchor,
            first_video_nsecs,
            played_until_nsecs,
            decoded_video_buffered_until_nsecs,
            audio_output_buffered_until_nsecs,
            stable_resume_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        }) {
            return Some(AudioClockResumeDecision {
                timeline_nsecs: first_video_nsecs,
                reset_audio_to_video: true,
            });
        }
    }

    let video_resume_nsecs = first_video_nsecs.max(played_until_nsecs);
    let output_audio_covers_video_resume = audio_output_buffered_until_nsecs
        .is_some_and(|buffered_until| buffered_until > video_resume_nsecs);
    let pending_audio_covers_video_resume = pending_audio
        .forward_duration_from(video_resume_nsecs)
        .is_some();
    let audio_video_start_nsecs =
        if output_audio_covers_video_resume || pending_audio_covers_video_resume {
            video_resume_nsecs
        } else {
            audio_video_start_nsecs
        };

    if played_until_nsecs.saturating_sub(audio_video_start_nsecs)
        > duration_nsecs(AUDIO_VIDEO_REBUFFER_DRIFT_RESET_THRESHOLD)
    {
        Some(AudioClockResumeDecision {
            timeline_nsecs: audio_video_start_nsecs,
            reset_audio_to_video: true,
        })
    } else {
        Some(AudioClockResumeDecision {
            timeline_nsecs: audio_video_start_nsecs.max(played_until_nsecs),
            reset_audio_to_video: false,
        })
    }
}

fn rebuffer_audio_anchor_reset_required(context: RebufferAudioAnchorResetContext) -> bool {
    if !context.reset_to_video_when_decoded_queue_misses_anchor
        || context.first_video_nsecs >= context.played_until_nsecs
    {
        return false;
    }

    let decoded_video_anchor_forward_nsecs = context
        .decoded_video_buffered_until_nsecs
        .map(|buffered_until| buffered_until.saturating_sub(context.played_until_nsecs));
    let decoded_video_anchor_window_unstable = decoded_video_anchor_forward_nsecs
        .is_none_or(|duration| duration < context.stable_resume_nsecs);
    let output_audio_runs_past_decoded_video = context
        .audio_output_buffered_until_nsecs
        .zip(context.decoded_video_buffered_until_nsecs)
        .is_some_and(|(audio_until, video_until)| audio_until > video_until);

    decoded_video_anchor_window_unstable || output_audio_runs_past_decoded_video
}

pub(in crate::player::backend::ffmpeg) fn initial_output_sync_decision(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
) -> Option<InitialOutputSyncDecision> {
    let first_video = queued_video_frames.front()?;
    let first_video_nsecs = first_video.timeline_nsecs;
    let first_audio_nsecs = pending_audio.first_start_timeline_nsecs();
    let gap_tolerance_nsecs = duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE);
    let mut delayed_audio_start_timeline_nsecs = first_audio_nsecs
        .filter(|audio_start| *audio_start > first_video_nsecs.saturating_add(gap_tolerance_nsecs));
    let mut drop_audio_before_timeline_nsecs = None;
    let mut stale_audio_preroll_until_nsecs = None;
    let mut stale_audio_preroll_gap_nsecs = None;
    let mut allow_initial_audio_gap_at_video_start = false;

    if let Some(first_audio_nsecs) =
        first_audio_nsecs.filter(|audio_start| *audio_start < first_video_nsecs)
    {
        let pending_audio_buffered_until_nsecs = pending_audio
            .buffered_until_from(first_audio_nsecs)
            .unwrap_or(first_audio_nsecs);
        if pending_audio_buffered_until_nsecs < first_video_nsecs {
            drop_audio_before_timeline_nsecs = Some(first_video_nsecs);
            stale_audio_preroll_until_nsecs = Some(pending_audio_buffered_until_nsecs);
            let preroll_gap_nsecs =
                first_video_nsecs.saturating_sub(pending_audio_buffered_until_nsecs);
            stale_audio_preroll_gap_nsecs = Some(preroll_gap_nsecs);

            delayed_audio_start_timeline_nsecs = pending_audio
                .first_start_at_or_after(first_video_nsecs)
                .filter(|audio_start| {
                    let delayed_gap_nsecs = audio_start.saturating_sub(first_video_nsecs);
                    delayed_gap_nsecs > gap_tolerance_nsecs
                        && delayed_gap_nsecs <= duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
                });

            let stale_preroll_gap_tolerance_nsecs = first_video
                .duration_nsecs
                .saturating_mul(2)
                .max(100_000_000)
                .saturating_add(gap_tolerance_nsecs);
            allow_initial_audio_gap_at_video_start = delayed_audio_start_timeline_nsecs.is_none()
                && preroll_gap_nsecs <= stale_preroll_gap_tolerance_nsecs;
        }
    }

    Some(InitialOutputSyncDecision {
        video_resume_timeline_nsecs: first_video_nsecs.max(played_until_nsecs),
        audio_start_timeline_nsecs: first_audio_nsecs,
        delayed_audio_start_timeline_nsecs,
        drop_audio_before_timeline_nsecs,
        stale_audio_preroll_until_nsecs,
        stale_audio_preroll_gap_nsecs,
        allow_initial_audio_gap_at_video_start,
        reset_audio_to_video: false,
    })
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn initial_audio_clock_resume_decision(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
) -> Option<AudioClockResumeDecision> {
    initial_output_sync_decision(queued_video_frames, pending_audio, played_until_nsecs)
        .map(InitialOutputSyncDecision::audio_clock_resume_decision)
}

pub(in crate::player::backend::ffmpeg) fn audio_output_buffered_until_for_resume(
    resume_decision: AudioClockResumeDecision,
    audio_output_buffered_until_nsecs: Option<u64>,
) -> Option<u64> {
    if resume_decision.reset_audio_to_video {
        None
    } else {
        audio_output_buffered_until_nsecs
    }
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn queued_video_forward_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
) -> Option<u64> {
    let first_timeline_nsecs = queued_video_frames.front()?.timeline_nsecs;
    queued_video_forward_nsecs_from(queued_video_frames, first_timeline_nsecs)
}

pub(in crate::player::backend::ffmpeg) fn decoded_audio_forward_nsecs_from(
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    audio_output_buffered_until_nsecs: Option<u64>,
) -> Option<u64> {
    let mut buffered_until_nsecs = audio_output_buffered_until_nsecs
        .filter(|buffered_until| *buffered_until > resume_timeline_nsecs)
        .unwrap_or(resume_timeline_nsecs);

    if let Some(pending_buffered_until_nsecs) =
        pending_audio.buffered_until_from(buffered_until_nsecs)
    {
        buffered_until_nsecs = pending_buffered_until_nsecs;
    } else if buffered_until_nsecs == resume_timeline_nsecs {
        return pending_audio.forward_duration_from(resume_timeline_nsecs);
    }

    (buffered_until_nsecs > resume_timeline_nsecs)
        .then_some(buffered_until_nsecs.saturating_sub(resume_timeline_nsecs))
}

fn delayed_start_decoded_audio_forward_from(
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
) -> Option<DelayedStartDecodedAudioForward> {
    let first_audio_start_nsecs = pending_audio.first_start_at_or_after(resume_timeline_nsecs)?;
    let delayed_start_gap_nsecs = first_audio_start_nsecs.saturating_sub(resume_timeline_nsecs);
    if delayed_start_gap_nsecs > duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION) {
        return None;
    }
    pending_audio
        .forward_duration_from(first_audio_start_nsecs)
        .map(|pending_forward_nsecs| DelayedStartDecodedAudioForward {
            forward_nsecs: delayed_start_gap_nsecs.saturating_add(pending_forward_nsecs),
            gap_nsecs: delayed_start_gap_nsecs,
        })
}

fn pending_audio_rebuffer_recovery_forward_from(
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
) -> Option<DelayedStartDecodedAudioForward> {
    let first_audio_start_nsecs = pending_audio.first_start_timeline_nsecs()?;
    let pending_duration_nsecs = duration_nsecs(pending_audio.buffered_duration());
    if pending_duration_nsecs == 0 {
        return None;
    }
    let gap_nsecs = first_audio_start_nsecs.saturating_sub(resume_timeline_nsecs);
    if gap_nsecs > duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION) {
        return None;
    }
    let skipped_before_resume_nsecs = resume_timeline_nsecs.saturating_sub(first_audio_start_nsecs);
    let forward_nsecs = gap_nsecs
        .saturating_add(pending_duration_nsecs.saturating_sub(skipped_before_resume_nsecs));
    (forward_nsecs > 0).then_some(DelayedStartDecodedAudioForward {
        forward_nsecs,
        gap_nsecs,
    })
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn playback_resume_waterline(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    needs_prefetch: bool,
    has_audio_output: bool,
) -> PlaybackResumeWaterline {
    playback_resume_waterline_with_target(
        queued_video_frames,
        pending_audio,
        resume_timeline_nsecs,
        demux_watermark,
        duration_nsecs(video_output_rebuffer_resume_duration(
            queued_video_frames,
            needs_prefetch,
        )),
        has_audio_output,
        PlaybackResumeWaterlineOptions::default(),
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    audio_output_buffered_until_nsecs: Option<u64>,
    needs_prefetch: bool,
    has_audio_output: bool,
) -> PlaybackResumeWaterline {
    rebuffer_playback_resume_waterline_with_resource_pressure(
        queued_video_frames,
        pending_audio,
        resume_timeline_nsecs,
        demux_watermark,
        audio_output_buffered_until_nsecs,
        needs_prefetch,
        has_audio_output,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline_with_resource_pressure(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    audio_output_buffered_until_nsecs: Option<u64>,
    needs_prefetch: bool,
    has_audio_output: bool,
    output_resource_pressure: bool,
) -> PlaybackResumeWaterline {
    playback_resume_waterline_with_target(
        queued_video_frames,
        pending_audio,
        resume_timeline_nsecs,
        demux_watermark,
        duration_nsecs(video_output_rebuffer_resume_duration_from_timeline(
            queued_video_frames,
            needs_prefetch,
            output_resource_pressure,
            Some(resume_timeline_nsecs),
        )),
        has_audio_output,
        PlaybackResumeWaterlineOptions {
            audio_output_buffered_until_nsecs,
            allow_delayed_audio_start: audio_output_buffered_until_nsecs.is_none(),
            ..PlaybackResumeWaterlineOptions::default()
        },
    )
}

pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline_after_prolonged_wait(
    mut waterline: PlaybackResumeWaterline,
    rebuffer_wait_elapsed: Option<Duration>,
) -> PlaybackResumeWaterline {
    if waterline.ready()
        || rebuffer_wait_elapsed
            .is_none_or(|elapsed| elapsed < VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER)
        || !waterline.demux_ready
    {
        return waterline;
    }

    let Some(decoded_video_forward_nsecs) = waterline.decoded_video_forward_nsecs else {
        return waterline;
    };
    // Once the rebuffer has stalled past the longer audio fallback window with video
    // and demux both available, stop waiting on the audio track: a structurally lagging
    // audio queue must not freeze playback forever. Above the standard stall timeout
    // we still prefer a stable 1s decoded-video window, but after the audio-stall
    // timeout a low-water video window is enough to resume and let the audio clock
    // resynchronize through the output-gate resume path.
    let audio_stall_timed_out = rebuffer_wait_elapsed
        .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER);
    let minimum_decoded_window_nsecs =
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_MIN_STABLE_RESUME_DURATION);
    let near_stable_decoded_window_nsecs =
        minimum_decoded_window_nsecs.saturating_sub(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let low_water_decoded_window_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION);
    let decoded_video_window_ready = decoded_video_forward_nsecs >= minimum_decoded_window_nsecs
        || (waterline.decoded_audio_ready
            && decoded_video_forward_nsecs >= near_stable_decoded_window_nsecs)
        || (audio_stall_timed_out && decoded_video_forward_nsecs >= low_water_decoded_window_nsecs);
    if !decoded_video_window_ready {
        return waterline;
    }

    if !audio_stall_timed_out
        && waterline.delayed_audio_start_gap_nsecs.is_some_and(|gap| {
            decoded_video_forward_nsecs.saturating_sub(gap)
                < duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
        })
    {
        return waterline;
    }

    let relaxed_target_nsecs = waterline.target_nsecs.min(decoded_video_forward_nsecs);
    if !audio_stall_timed_out
        && !waterline.decoded_audio_ready
        && waterline
            .decoded_audio_forward_nsecs
            .is_none_or(|duration| duration < relaxed_target_nsecs)
    {
        return waterline;
    }

    waterline.target_nsecs = relaxed_target_nsecs;
    waterline.decoded_video_ready = true;
    waterline.decoded_audio_ready = true;
    waterline
}

pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline_after_cache_pause(
    mut waterline: PlaybackResumeWaterline,
    rebuffer_wait_elapsed: Option<Duration>,
    cache_paused: bool,
) -> PlaybackResumeWaterline {
    if waterline.ready()
        || !cache_paused
        || !waterline.decoded_ready()
        || rebuffer_wait_elapsed
            .is_none_or(|elapsed| elapsed < VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER)
        || !rebuffer_cache_pause_demux_fallback_ready(waterline)
    {
        return waterline;
    }

    waterline.demux_ready = true;
    waterline
}

fn rebuffer_cache_pause_demux_fallback_ready(waterline: PlaybackResumeWaterline) -> bool {
    waterline.demux_ready
        || waterline.demux_min_forward_nsecs.is_some_and(|duration| {
            duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        })
}

pub(in crate::player::backend::ffmpeg) fn initial_playback_resume_waterline_after_stale_audio_preroll_wait(
    mut waterline: PlaybackResumeWaterline,
    sync_decision: Option<InitialOutputSyncDecision>,
    startup_sync_elapsed: Option<Duration>,
) -> PlaybackResumeWaterline {
    if waterline.ready()
        || waterline.decoded_audio_ready
        || !waterline.decoded_video_ready
        || startup_sync_elapsed
            .is_none_or(|elapsed| elapsed < VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER)
        || sync_decision.is_none_or(|decision| decision.stale_audio_preroll_gap_nsecs.is_none())
    {
        return waterline;
    }

    waterline.decoded_audio_ready = true;
    waterline
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn initial_playback_resume_waterline(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    delayed_audio_start_timeline_nsecs: Option<u64>,
    allow_initial_audio_gap_at_video_start: bool,
    demux_watermark: DemuxReaderWatermark,
    needs_prefetch: bool,
    has_audio_output: bool,
) -> PlaybackResumeWaterline {
    playback_resume_waterline_with_target(
        queued_video_frames,
        pending_audio,
        resume_timeline_nsecs,
        demux_watermark,
        duration_nsecs(video_output_start_prebuffer_duration(
            queued_video_frames,
            needs_prefetch,
        )),
        has_audio_output,
        PlaybackResumeWaterlineOptions {
            initial_delayed_audio_start_timeline_nsecs: delayed_audio_start_timeline_nsecs,
            allow_initial_audio_gap_at_video_start,
            ..PlaybackResumeWaterlineOptions::default()
        },
    )
}

pub(in crate::player::backend::ffmpeg) fn playback_resume_waterline_with_target(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    target_nsecs: u64,
    has_audio_output: bool,
    options: PlaybackResumeWaterlineOptions,
) -> PlaybackResumeWaterline {
    let decoded_video_forward_nsecs =
        queued_video_forward_nsecs_from(queued_video_frames, resume_timeline_nsecs);
    let audio_start_timeline_nsecs = resume_timeline_nsecs;
    let direct_decoded_audio_forward_nsecs = has_audio_output.then(|| {
        decoded_audio_forward_nsecs_from(
            pending_audio,
            audio_start_timeline_nsecs,
            options.audio_output_buffered_until_nsecs,
        )
    });
    let initial_delayed_decoded_audio_forward_nsecs = (has_audio_output
        && direct_decoded_audio_forward_nsecs.flatten().is_none())
    .then(|| {
        let delayed_audio_start_timeline_nsecs =
            options.initial_delayed_audio_start_timeline_nsecs?;
        let delayed_start_gap_nsecs =
            delayed_audio_start_timeline_nsecs.saturating_sub(audio_start_timeline_nsecs);
        (delayed_start_gap_nsecs > duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
            .then_some(delayed_start_gap_nsecs)?
            .checked_add(pending_audio.forward_duration_from(delayed_audio_start_timeline_nsecs)?)
    })
    .flatten();
    let delayed_decoded_audio_forward = (has_audio_output
        && direct_decoded_audio_forward_nsecs.flatten().is_none()
        && initial_delayed_decoded_audio_forward_nsecs.is_none()
        && options.allow_delayed_audio_start)
        .then(|| {
            delayed_start_decoded_audio_forward_from(pending_audio, audio_start_timeline_nsecs)
        })
        .flatten();
    let rebuffer_recovery_decoded_audio_forward = (has_audio_output
        && options.allow_delayed_audio_start)
        .then(|| {
            pending_audio_rebuffer_recovery_forward_from(pending_audio, audio_start_timeline_nsecs)
        })
        .flatten();
    let decoded_audio_forward_nsecs = direct_decoded_audio_forward_nsecs
        .flatten()
        .or(initial_delayed_decoded_audio_forward_nsecs)
        .or_else(|| delayed_decoded_audio_forward.map(|delayed| delayed.forward_nsecs))
        .max(rebuffer_recovery_decoded_audio_forward.map(|delayed| delayed.forward_nsecs));
    let initial_audio_gap_at_video_start_ready = has_audio_output
        && options.allow_initial_audio_gap_at_video_start
        && direct_decoded_audio_forward_nsecs.flatten().is_none()
        && initial_delayed_decoded_audio_forward_nsecs.is_none()
        && delayed_decoded_audio_forward.is_none();
    let initial_delayed_audio_start_gap_nsecs = options
        .initial_delayed_audio_start_timeline_nsecs
        .map(|timeline| timeline.saturating_sub(audio_start_timeline_nsecs))
        .filter(|gap| *gap > duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE));
    let delayed_audio_start_gap_nsecs = initial_delayed_audio_start_gap_nsecs.or_else(|| {
        delayed_decoded_audio_forward
            .or(rebuffer_recovery_decoded_audio_forward)
            .map(|delayed| delayed.gap_nsecs)
            .filter(|gap| *gap > duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
    });
    let demux_min_forward_nsecs = if has_audio_output {
        demux_watermark
            .video_forward_nsecs
            .zip(demux_watermark.audio_forward_nsecs)
            .map(|(video, audio)| video.min(audio))
    } else {
        demux_watermark.video_forward_nsecs
    }
    .or(demux_watermark.selected_min_forward_nsecs);
    let initial_video_clock_until_delayed_audio = initial_delayed_audio_start_gap_nsecs.is_some();
    let decoded_video_ready = decoded_video_forward_nsecs.is_some_and(|duration| {
        duration >= target_nsecs
            && (initial_video_clock_until_delayed_audio
                || delayed_audio_start_gap_nsecs.is_none_or(|gap| {
                    duration.saturating_sub(gap)
                        >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION)
                }))
    });
    let decoded_audio_ready = !has_audio_output
        || initial_audio_gap_at_video_start_ready
        || decoded_audio_forward_nsecs.is_some_and(|duration| duration >= target_nsecs);
    let demux_underrun = demux_watermark.underrun
        || demux_watermark.video_underrun
        || (has_audio_output && demux_watermark.audio_underrun);
    let demux_idle = demux_watermark.idle
        || (demux_watermark.video_idle && (!has_audio_output || demux_watermark.audio_idle));
    let demux_ready = !demux_underrun
        && (demux_idle || demux_min_forward_nsecs.is_some_and(|duration| duration >= target_nsecs));
    PlaybackResumeWaterline {
        target_nsecs,
        decoded_video_forward_nsecs,
        decoded_audio_forward_nsecs,
        delayed_audio_start_gap_nsecs,
        demux_video_forward_nsecs: demux_watermark.video_forward_nsecs,
        demux_audio_forward_nsecs: demux_watermark.audio_forward_nsecs,
        demux_min_forward_nsecs,
        decoded_video_ready,
        decoded_audio_ready,
        demux_ready,
    }
}

pub(in crate::player::backend::ffmpeg) fn playback_resume_waterline_blocked_on(
    waterline: PlaybackResumeWaterline,
) -> PlaybackBlockReason {
    if !waterline.decoded_video_ready {
        PlaybackBlockReason::DecodedVideoQueue
    } else if !waterline.decoded_audio_ready {
        PlaybackBlockReason::DecodedAudioQueue
    } else if !waterline.demux_ready {
        PlaybackBlockReason::DemuxCache
    } else {
        PlaybackBlockReason::OutputGate
    }
}

fn should_log_resume_waterline_wait(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    waterline: PlaybackResumeWaterline,
) -> bool {
    waterline.decoded_ready() && !waterline.demux_ready
        || (!queued_video_frames.is_empty() && queued_video_frames.len().is_multiple_of(30))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn log_playback_resume_waterline_wait(
    session_id: PlaybackSessionId,
    context: &'static str,
    playback_output_state: PlaybackOutputState,
    resume_timeline_nsecs: u64,
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    waterline: PlaybackResumeWaterline,
    demux_watermark: DemuxReaderWatermark,
) {
    if !should_log_resume_waterline_wait(queued_video_frames, waterline) {
        return;
    }

    tracing::debug!(
        session_id = ?session_id,
        blocked_on = playback_resume_waterline_blocked_on(waterline).as_str(),
        context,
        playback_output_state = ?playback_output_state,
        resume_timeline_nsecs,
        target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
        decoded_video_range = ?queued_video_range_nsecs(queued_video_frames),
        decoded_video_ms = ?waterline
            .decoded_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?waterline
            .decoded_audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_video_ready = waterline.decoded_video_ready,
        decoded_audio_ready = waterline.decoded_audio_ready,
        demux_ready = waterline.demux_ready,
        pending_audio_start_nsecs = pending_audio.first_start_timeline_nsecs(),
        pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
        queued_video_frames = queued_video_frames.len(),
        queued_video_ms = queued_video_duration(queued_video_frames).as_secs_f64() * 1000.0,
        demux_video_ms = ?waterline
            .demux_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_audio_ms = ?waterline
            .demux_audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?waterline
            .demux_min_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_underrun = demux_watermark.underrun,
        demux_idle = demux_watermark.idle,
        demux_video_underrun = demux_watermark.video_underrun,
        demux_audio_underrun = demux_watermark.audio_underrun,
        demux_video_idle = demux_watermark.video_idle,
        demux_audio_idle = demux_watermark.audio_idle,
        forward_bytes = demux_watermark.forward_bytes,
        "waiting for FFmpeg playback resume waterline"
    );
}

pub(in crate::player::backend::ffmpeg) fn enter_video_output_rebuffer(
    output_state: &mut PlaybackOutputState,
    control: &FfmpegControl,
    audio_output: Option<&AudioOutput>,
    queued_video_frames: &ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    underrun_elapsed: Duration,
    decoded_video_forward_nsecs: Option<u64>,
) -> Option<RebufferResumeAnchor> {
    if output_state.rebuffering() {
        return None;
    }
    let audio_paused_timeline_nsecs = audio_output
        .and_then(|output| output.snapshot().ok())
        .map(|snapshot| snapshot.played_timeline_nsecs);
    let decoded_video_unstable = decoded_video_forward_nsecs
        .is_none_or(|duration| duration < duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION));
    let resume_anchor = audio_paused_timeline_nsecs.map(|timeline_nsecs| RebufferResumeAnchor {
        timeline_nsecs,
        reset_to_video_when_decoded_queue_misses_anchor: decoded_video_unstable
            || queued_video_frames
                .buffered_until_from_nsecs(timeline_nsecs)
                .is_none(),
    });
    *output_state = PlaybackOutputState::Rebuffering;
    control.set_output_rebuffer_paused(true);
    tracing::debug!(
        session_id = ?session_id,
        blocked_on = "video_output_underflow",
        queued_video_frames = queued_video_frames.len(),
        queued_video_ms = queued_video_frames.duration().as_secs_f64() * 1000.0,
        decoded_video_range = ?queued_video_frames.range_nsecs(),
        decoded_video_forward_ms = ?decoded_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        audio_paused_timeline_nsecs,
        reset_to_video_when_decoded_queue_misses_anchor = resume_anchor
            .is_some_and(|anchor| anchor.reset_to_video_when_decoded_queue_misses_anchor),
        underrun_ms = underrun_elapsed.as_secs_f64() * 1000.0,
        "entering FFmpeg decoded video rebuffer after output underflow"
    );
    resume_anchor
}

pub(in crate::player::backend::ffmpeg) fn finish_video_output_rebuffer_if_ready(
    output_state: &mut PlaybackOutputState,
    waterline: PlaybackResumeWaterline,
    session_id: PlaybackSessionId,
) -> bool {
    if !output_state.rebuffering() || !waterline.ready() {
        return false;
    }
    *output_state = PlaybackOutputState::Ready;
    tracing::debug!(
        session_id = ?session_id,
        blocked_on = playback_resume_waterline_blocked_on(waterline).as_str(),
        target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
        decoded_video_ms = ?waterline
            .decoded_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?waterline
            .decoded_audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_video_ms = ?waterline
            .demux_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_audio_ms = ?waterline
            .demux_audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?waterline
            .demux_min_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        "leaving FFmpeg decoded video rebuffer after combined waterline"
    );
    true
}

pub(in crate::player::backend::ffmpeg) fn clear_video_output_rebuffer(
    output_state: &mut PlaybackOutputState,
    control: &FfmpegControl,
) {
    if !output_state.rebuffering() && !control.is_output_rebuffer_paused() {
        return;
    }
    *output_state = PlaybackOutputState::Syncing;
    control.set_output_rebuffer_paused(false);
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use crate::player::render_host::{DecodedFrame, FramePixels, FramePts, RenderSize};

    use super::super::DecodedAudio;
    use super::{
        DEFAULT_VIDEO_FRAME_DURATION_NSECS, DemuxReaderWatermark, InitialOutputSyncDecision,
        PendingStartAudio, PlaybackResumeWaterline, QueuedVideoFrame,
        RebufferAudioAnchorResetContext, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
        VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
        VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER,
        duration_nsecs, initial_playback_resume_waterline_after_stale_audio_preroll_wait,
        rebuffer_audio_anchor_reset_required, rebuffer_playback_resume_waterline,
        rebuffer_playback_resume_waterline_after_cache_pause,
        rebuffer_playback_resume_waterline_after_prolonged_wait,
    };

    fn queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
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

    fn queued_video_window(start_nsecs: u64, duration_nsecs: u64) -> VecDeque<QueuedVideoFrame> {
        let mut frames = VecDeque::new();
        let mut timeline_nsecs = start_nsecs;
        while timeline_nsecs.saturating_sub(start_nsecs) < duration_nsecs {
            frames.push_back(queued_video_frame(timeline_nsecs));
            timeline_nsecs = timeline_nsecs.saturating_add(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
        }
        frames
    }

    fn ready_demux_watermark(forward_nsecs: u64) -> DemuxReaderWatermark {
        DemuxReaderWatermark {
            video_forward_nsecs: Some(forward_nsecs),
            audio_forward_nsecs: Some(forward_nsecs),
            selected_min_forward_nsecs: Some(forward_nsecs),
            video_underrun: false,
            audio_underrun: false,
            video_idle: false,
            audio_idle: false,
            underrun: false,
            idle: false,
            forward_bytes: 0,
        }
    }

    fn rebuffer_waterline(
        decoded_video_forward_nsecs: u64,
        decoded_audio_forward_nsecs: u64,
        demux_forward_nsecs: u64,
        demux_ready: bool,
    ) -> PlaybackResumeWaterline {
        PlaybackResumeWaterline {
            target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            decoded_video_forward_nsecs: Some(decoded_video_forward_nsecs),
            decoded_audio_forward_nsecs: Some(decoded_audio_forward_nsecs),
            delayed_audio_start_gap_nsecs: None,
            demux_video_forward_nsecs: Some(demux_forward_nsecs),
            demux_audio_forward_nsecs: Some(demux_forward_nsecs),
            demux_min_forward_nsecs: Some(demux_forward_nsecs),
            decoded_video_ready: decoded_video_forward_nsecs
                >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            decoded_audio_ready: decoded_audio_forward_nsecs
                >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            demux_ready,
        }
    }

    fn anchor_reset_context(
        decoded_video_buffered_until_nsecs: Option<u64>,
        audio_output_buffered_until_nsecs: Option<u64>,
    ) -> RebufferAudioAnchorResetContext {
        RebufferAudioAnchorResetContext {
            reset_to_video_when_decoded_queue_misses_anchor: true,
            first_video_nsecs: 1_000_000_000,
            played_until_nsecs: 2_000_000_000,
            decoded_video_buffered_until_nsecs,
            audio_output_buffered_until_nsecs,
            stable_resume_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        }
    }

    fn stale_preroll_sync_decision(
        stale_audio_preroll_gap_nsecs: Option<u64>,
    ) -> InitialOutputSyncDecision {
        InitialOutputSyncDecision {
            video_resume_timeline_nsecs: 1_000_000_000,
            audio_start_timeline_nsecs: Some(800_000_000),
            delayed_audio_start_timeline_nsecs: None,
            drop_audio_before_timeline_nsecs: Some(1_000_000_000),
            stale_audio_preroll_until_nsecs: stale_audio_preroll_gap_nsecs
                .map(|gap| 1_000_000_000u64.saturating_sub(gap)),
            stale_audio_preroll_gap_nsecs,
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        }
    }

    #[test]
    fn rebuffer_resume_waterline_uses_pending_audio_recovery_window() {
        let resume_timeline_nsecs = 1_000_000_000;
        let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
        let mut pending_audio = PendingStartAudio::default();
        pending_audio.push(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 40_000_000,
            },
            resume_timeline_nsecs,
            resume_timeline_nsecs + 40_000_000,
        );
        pending_audio.push(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: target_nsecs,
            },
            resume_timeline_nsecs + 50_000_000,
            resume_timeline_nsecs + 50_000_000 + target_nsecs,
        );
        let queued_video_frames = queued_video_window(resume_timeline_nsecs, target_nsecs);
        let waterline = rebuffer_playback_resume_waterline(
            &queued_video_frames,
            &pending_audio,
            resume_timeline_nsecs,
            ready_demux_watermark(target_nsecs),
            None,
            false,
            true,
        );

        assert!(waterline.decoded_video_ready);
        assert!(waterline.decoded_audio_ready);
        assert_eq!(
            waterline.decoded_audio_forward_nsecs,
            Some(target_nsecs + 40_000_000)
        );
        assert!(waterline.ready());
    }

    #[test]
    fn rebuffer_audio_anchor_reset_requires_stable_decoded_video_window() {
        assert!(rebuffer_audio_anchor_reset_required(anchor_reset_context(
            Some(2_500_000_000),
            None,
        )));
        assert!(!rebuffer_audio_anchor_reset_required(anchor_reset_context(
            Some(3_000_000_000),
            None,
        )));
    }

    #[test]
    fn rebuffer_audio_anchor_reset_when_output_audio_runs_past_decoded_video() {
        assert!(rebuffer_audio_anchor_reset_required(anchor_reset_context(
            Some(3_250_000_000),
            Some(3_500_000_000),
        )));
    }

    #[test]
    fn rebuffer_audio_anchor_reset_respects_disabled_anchor_policy() {
        let mut context = anchor_reset_context(None, None);
        context.reset_to_video_when_decoded_queue_misses_anchor = false;

        assert!(!rebuffer_audio_anchor_reset_required(context));
    }

    #[test]
    fn stale_initial_audio_preroll_fallback_does_not_forge_audio_forward() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            0,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            true,
        );

        let before_timeout = initial_playback_resume_waterline_after_stale_audio_preroll_wait(
            waterline,
            Some(stale_preroll_sync_decision(Some(150_000_000))),
            Some(Duration::ZERO),
        );
        assert!(!before_timeout.decoded_audio_ready);

        let without_stale_marker = initial_playback_resume_waterline_after_stale_audio_preroll_wait(
            waterline,
            Some(stale_preroll_sync_decision(None)),
            Some(VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER),
        );
        assert!(!without_stale_marker.decoded_audio_ready);

        let after_timeout = initial_playback_resume_waterline_after_stale_audio_preroll_wait(
            waterline,
            Some(stale_preroll_sync_decision(Some(150_000_000))),
            Some(VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER),
        );
        assert!(after_timeout.decoded_audio_ready);
        assert_eq!(after_timeout.decoded_audio_forward_nsecs, Some(0));
    }

    #[test]
    fn prolonged_rebuffer_wait_does_not_relax_demux_waterline() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) * 2,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            0,
            false,
        );

        let waterline = rebuffer_playback_resume_waterline_after_prolonged_wait(
            waterline,
            Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
        );

        assert!(!waterline.ready());
        assert!(!waterline.demux_ready);
        assert!(!waterline.decoded_audio_ready);
        assert_eq!(waterline.demux_min_forward_nsecs, Some(0));
    }

    #[test]
    fn cache_pause_rebuffer_wait_keeps_waiting_with_empty_demux_window() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            0,
            false,
        );

        let waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
            true,
        );

        assert!(!waterline.ready());
        assert!(!waterline.demux_ready);
    }

    #[test]
    fn cache_pause_rebuffer_wait_keeps_waiting_with_low_water_demux_window() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            false,
        );

        let waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
            true,
        );

        assert!(!waterline.ready());
        assert!(!waterline.demux_ready);
    }

    #[test]
    fn cache_pause_rebuffer_wait_allows_stable_demux_window_to_resume() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            false,
        );

        let waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
            true,
        );

        assert!(waterline.ready());
        assert!(waterline.demux_ready);
    }

    #[test]
    fn cache_pause_rebuffer_wait_keeps_demux_waterline_without_cache_pause() {
        let waterline = rebuffer_waterline(
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            0,
            false,
        );

        let waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
            false,
        );

        assert!(!waterline.ready());
        assert!(!waterline.demux_ready);
    }
}
