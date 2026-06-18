use super::{
    AudioClockResumeDecision, PlaybackOutputScheduler, PlaybackResumeWaterline, PlaybackSessionId,
    RebufferResumeAnchor,
};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn discard_decoded_video_before_output_gate_resume_if_ready(
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
