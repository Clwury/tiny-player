#![cfg(test)]

use crate::player::render_host::{DecodedFrame, FramePixels, FramePts, RenderSize};

use super::super::{
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, QueuedVideoFrame, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
};
use super::{
    AudioClockResumeDecision, AudioOutputSnapshot, PlaybackResumeWaterline, ResumeAnchorSource,
    duration_nsecs,
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
        delayed_audio_start_timeline_nsecs: None,
        allow_audio_gap_at_video_resume: false,
        resume_anchor_source: ResumeAnchorSource::Video,
    }
}
fn waterline(decoded_video_ready: bool) -> PlaybackResumeWaterline {
    PlaybackResumeWaterline {
        target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        audio_resume_waterline: None,
        decoded_video_forward_nsecs: decoded_video_ready
            .then_some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        decoded_audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        delayed_audio_start_gap_nsecs: None,
        allow_audio_gap_at_video_resume: false,
        resume_anchor_source: ResumeAnchorSource::Video,
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
        buffered_until_timeline_nsecs: played_timeline_nsecs.saturating_add(total_pending_nsecs),
        shared_pending_nsecs: total_pending_nsecs,
        queue_pending_nsecs: 0,
        total_pending_nsecs,
        queue_frames: 0,
        queue_generation: 0,
    }
}

#[path = "tests/demux_watermark.rs"]
mod demux_watermark;
#[path = "tests/initial_start.rs"]
mod initial_start;
#[path = "tests/resume.rs"]
mod resume;
