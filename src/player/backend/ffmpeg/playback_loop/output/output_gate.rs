pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::QueuedVideoFrame;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::audio_output_gate::{
    DelayedAudioStartSilencePolicy, flush_pending_start_audio,
    pending_audio_underrun_recovery_plan, push_decoded_audio_to_output,
    recover_pending_start_audio_after_underrun,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::output_rebuffer::{
    AudioClockResumeDecision, InitialOutputSyncDecision, PlaybackOutputState,
    PlaybackResumeWaterline, RebufferResumeAnchor, audio_output_buffered_until_for_resume,
    clear_video_output_rebuffer, enter_video_output_rebuffer,
    finish_video_output_rebuffer_if_ready,
    initial_playback_resume_waterline_after_stale_audio_preroll_wait,
    rebuffer_playback_resume_waterline_after_cache_pause,
    rebuffer_playback_resume_waterline_after_prolonged_wait, should_block_for_demux_read,
    video_output_rebuffer_should_enter,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::pending_audio_queue::PendingStartAudio;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::scheduled_video_queue::ScheduledVideoQueue;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::video_output_gate::{
    present_first_queued_video_frame, present_video_frame_to_vo,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioClockMode, AudioOutput,
    AudioOutputSnapshot, BufferedReporter, DecodedAudio, DemuxPacketCache, DemuxReaderWatermark,
    FfmpegControl, OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    PENDING_START_AUDIO_BACKPRESSURE_DURATION, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PlaybackScheduler, PositionReporter,
    SubtitlePipeline, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, duration_nsecs, nsecs_to_seconds,
};

#[path = "output_gate/audio_pressure.rs"]
mod audio_pressure;
#[path = "output_gate/demux_watermark.rs"]
mod demux_watermark;
#[path = "output_gate/discard.rs"]
mod discard;
#[path = "output_gate/initial_start.rs"]
mod initial_start;
#[path = "output_gate/resume.rs"]
mod resume;
#[path = "output_gate/scheduler.rs"]
mod scheduler;
#[path = "output_gate/snapshot.rs"]
mod snapshot;
#[cfg(test)]
#[path = "output_gate/tests.rs"]
mod tests;

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use audio_pressure::audio_output_contiguous_start_timeline_nsecs;
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use audio_pressure::{
    audio_output_flush_until_timeline_nsecs, playing_pending_audio_limit_duration,
    playing_pending_audio_pressure_clear_duration,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use demux_watermark::timed_output_gate_demux_watermark;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use discard::discard_decoded_video_before_output_gate_resume_if_ready;
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use initial_start::{
    initial_delayed_audio_start_timeline_nsecs, service_initial_video_clock_until_audio_start,
};
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) use resume::OutputGateResumeTiming;
pub(in crate::player::backend::ffmpeg) use resume::{
    OutputGateResumeStatus, service_output_gate_resume_if_ready,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PendingStartAudioPressureLevel {
    Normal,
    Warn,
    ForceRecovery,
    HardReset,
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
    initial_audio_gap_at_video_start_timeline_nsecs: Option<u64>,
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
