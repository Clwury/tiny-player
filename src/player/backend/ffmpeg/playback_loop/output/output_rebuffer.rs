use super::pending_audio_queue::PendingStartAudio;
use super::playback_block::PlaybackBlockReason;
use super::scheduled_video_queue::{
    ScheduledVideoQueue, queued_video_buffered_until_from_nsecs, queued_video_forward_nsecs_from,
    queued_video_range_nsecs,
};
use super::*;

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

pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_should_enter(
    underrun_started_at: &mut Option<Instant>,
    now: Instant,
    video_output_underflowing: bool,
    demux_cache_insufficient: bool,
    render_backlogged: bool,
    has_audio_output: bool,
    output_state: PlaybackOutputState,
) -> bool {
    if !video_output_underflowing
        || !demux_cache_insufficient
        || render_backlogged
        || !has_audio_output
    {
        *underrun_started_at = None;
        return false;
    }

    let started_at = underrun_started_at.get_or_insert(now);
    !output_state.rebuffering()
        && now.saturating_duration_since(*started_at) >= VIDEO_OUTPUT_REBUFFER_ENTER_AFTER
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
) -> bool {
    if !has_audio_output || output_state.first_video_frame_pending() {
        return false;
    }
    if output_state.rebuffering() {
        return true;
    }

    let Some(played_until_nsecs) = played_until_nsecs else {
        return false;
    };
    let low_water_duration = if queued_video_frames_have_vulkan(queued_video_frames) {
        VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION
    } else {
        VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION
    };
    queued_video_forward_nsecs_from(queued_video_frames, played_until_nsecs)
        .is_none_or(|forward_nsecs| forward_nsecs <= duration_nsecs(low_water_duration))
}

pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_resume_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_prefetch: bool,
) -> Duration {
    queued_video_target_duration(queued_video_frames, needs_prefetch)
        .min(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
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
    if reset_to_video_when_decoded_queue_misses_anchor
        && first_video_nsecs < played_until_nsecs
        && queued_video_buffered_until_from_nsecs(queued_video_frames, played_until_nsecs).is_none()
    {
        return Some(AudioClockResumeDecision {
            timeline_nsecs: audio_video_start_nsecs,
            reset_audio_to_video: true,
        });
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

pub(in crate::player::backend::ffmpeg) fn initial_audio_clock_resume_decision(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    played_until_nsecs: u64,
) -> Option<AudioClockResumeDecision> {
    let first_video_nsecs = queued_video_frames.front()?.timeline_nsecs;
    let first_audio_nsecs = pending_audio
        .first_start_timeline_nsecs()
        .unwrap_or(first_video_nsecs);
    let gap_tolerance_nsecs = duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE);
    let timeline_nsecs =
        if first_audio_nsecs > first_video_nsecs.saturating_add(gap_tolerance_nsecs) {
            first_audio_nsecs
        } else {
            first_video_nsecs
        };
    Some(AudioClockResumeDecision {
        timeline_nsecs: timeline_nsecs.max(played_until_nsecs),
        reset_audio_to_video: false,
    })
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
        None,
        duration_nsecs(video_output_rebuffer_resume_duration(
            queued_video_frames,
            needs_prefetch,
        )),
        has_audio_output,
    )
}

pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    audio_output_buffered_until_nsecs: Option<u64>,
    needs_prefetch: bool,
    has_audio_output: bool,
) -> PlaybackResumeWaterline {
    playback_resume_waterline_with_target(
        queued_video_frames,
        pending_audio,
        resume_timeline_nsecs,
        demux_watermark,
        audio_output_buffered_until_nsecs,
        duration_nsecs(video_output_rebuffer_resume_duration(
            queued_video_frames,
            needs_prefetch,
        )),
        has_audio_output,
    )
}

pub(in crate::player::backend::ffmpeg) fn initial_playback_resume_waterline(
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
        None,
        duration_nsecs(video_output_start_prebuffer_duration(
            queued_video_frames,
            needs_prefetch,
        )),
        has_audio_output,
    )
}

pub(in crate::player::backend::ffmpeg) fn playback_resume_waterline_with_target(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    pending_audio: &PendingStartAudio,
    resume_timeline_nsecs: u64,
    demux_watermark: DemuxReaderWatermark,
    audio_output_buffered_until_nsecs: Option<u64>,
    target_nsecs: u64,
    has_audio_output: bool,
) -> PlaybackResumeWaterline {
    let decoded_video_forward_nsecs =
        queued_video_forward_nsecs_from(queued_video_frames, resume_timeline_nsecs);
    let audio_start_timeline_nsecs = resume_timeline_nsecs;
    let decoded_audio_forward_nsecs = has_audio_output
        .then(|| {
            decoded_audio_forward_nsecs_from(
                pending_audio,
                audio_start_timeline_nsecs,
                audio_output_buffered_until_nsecs,
            )
        })
        .flatten();
    let demux_min_forward_nsecs = if has_audio_output {
        demux_watermark
            .video_forward_nsecs
            .zip(demux_watermark.audio_forward_nsecs)
            .map(|(video, audio)| video.min(audio))
    } else {
        demux_watermark.video_forward_nsecs
    }
    .or(demux_watermark.selected_min_forward_nsecs);
    let decoded_video_ready =
        decoded_video_forward_nsecs.is_some_and(|duration| duration >= target_nsecs);
    let decoded_audio_ready = !has_audio_output
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
    let resume_anchor = audio_paused_timeline_nsecs.map(|timeline_nsecs| RebufferResumeAnchor {
        timeline_nsecs,
        reset_to_video_when_decoded_queue_misses_anchor: queued_video_frames
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
