use super::{AudioOutput, DoviPipeline, PlaybackScheduler, StreamInfo, TimestampMapper};

#[allow(clippy::too_many_arguments)]
pub(super) fn reset_playback_timeline_state(
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    video_frame_duration_nsecs: u64,
    current_start_position_nsecs: u64,
    video_clock: &mut TimestampMapper,
    playback_timeline_origin_nsecs: &mut Option<u64>,
    audio_clock: &mut TimestampMapper,
    scheduler: &mut PlaybackScheduler,
    audio_output: Option<&AudioOutput>,
    dovi_pipeline: &mut DoviPipeline,
) {
    *video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    *playback_timeline_origin_nsecs = video_stream.start_nsecs;
    *audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );
    scheduler.reset(current_start_position_nsecs);
    if let Some(output) = audio_output {
        output.reset_clock(current_start_position_nsecs);
    }
    dovi_pipeline.reset();
}
