use super::{
    AudioDecodePipeline, AvPacket, DoviPipeline, FfmpegControl, PlaybackGeneration,
    PlaybackOutputScheduler, StreamInfo, SubtitlePipeline, VideoDecodePipeline,
    VideoDecodeRecovery,
};

pub(super) fn service_video_decode_recovery_result(
    context: VideoDecodeRecoveryServiceContext<'_>,
) -> std::result::Result<(), String> {
    let recovery_result = context.video_decode_pipeline.recover_error_if_needed(
        context.result,
        context.playback_generation,
        context.video_stream.codec_id,
        context.packet,
        context.video_decode_recovery,
        context.realign_after_decode_recovery,
    );
    if context.video_decode_recovery.waiting_for_keyframe() {
        if context.realign_after_decode_recovery {
            context.output_scheduler.reset(context.control);
            let generation = context.playback_generation.advance();
            if let Some(worker) = context.audio_decode_pipeline {
                worker.flush_buffers(generation)?;
            }
            context.subtitle_pipeline.flush_decode_state(generation)?;
        }
        context.video_decode_pipeline.clear_packets();
        context.dovi_pipeline.reset();
    }
    recovery_result
}

pub(super) struct VideoDecodeRecoveryServiceContext<'a> {
    pub(super) result: std::result::Result<(), String>,
    pub(super) packet: &'a AvPacket,
    pub(super) realign_after_decode_recovery: bool,
    pub(super) video_stream: StreamInfo,
    pub(super) playback_generation: &'a mut PlaybackGeneration,
    pub(super) video_decode_pipeline: &'a mut VideoDecodePipeline,
    pub(super) audio_decode_pipeline: Option<&'a mut AudioDecodePipeline>,
    pub(super) subtitle_pipeline: &'a mut SubtitlePipeline,
    pub(super) video_decode_recovery: &'a mut VideoDecodeRecovery,
    pub(super) output_scheduler: &'a mut PlaybackOutputScheduler,
    pub(super) dovi_pipeline: &'a mut DoviPipeline,
    pub(super) control: &'a FfmpegControl,
}
