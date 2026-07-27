use crate::player::render_host::VideoOutputQueue;

use super::{
    AudioDecodePipeline, AvPacket, DoviPipeline, FfmpegControl, PlaybackGeneration,
    PlaybackOutputScheduler, StreamInfo, SubtitlePipeline, VideoDecodePipeline,
    VideoDecodeRecovery, VideoFramePrepareWorker,
};

pub(super) fn service_video_decode_recovery_result(
    context: VideoDecodeRecoveryServiceContext<'_>,
) -> std::result::Result<(), String> {
    let release_resource_pressure_references =
        context.video_decode_pipeline.recover_error_if_needed(
            context.result,
            context.playback_generation,
            context.video_stream.codec_id,
            context.packet,
            context.video_decode_recovery,
            context.realign_after_decode_recovery,
            context.output_scheduler.committed_video_queue_end_nsecs(),
        )?;
    if release_resource_pressure_references {
        let session_id = context.control.session_id();
        let released_scheduler_frames = context
            .output_scheduler
            .release_vulkan_frames_for_resource_pressure(context.control, session_id);
        let discarded_vo_frames = context.vo_queue.discard_pending_frames(session_id);
        context.video_decode_pipeline.clear_packets();
        context.dovi_pipeline.reset();
        let generation = context.playback_generation.advance();
        if let Err(error) = context
            .video_frame_prepare_worker
            .restart_after_resource_pressure(generation)
        {
            context
                .video_decode_pipeline
                .fail_hevc_same_hardware_recovery(format!(
                    "failed to retire frame-prepare worker after Vulkan resource pressure: {error}"
                ));
            tracing::error!(
                ?session_id,
                generation,
                released_scheduler_frames,
                discarded_vo_frames,
                %error,
                "could not confirm release of external Vulkan frame references"
            );
            return Ok(());
        }
        tracing::warn!(
            ?session_id,
            generation,
            released_scheduler_frames,
            discarded_vo_frames,
            "released all externally owned Vulkan frames at first decoder OOM"
        );
        return Ok(());
    }
    if context.video_decode_recovery.waiting_for_keyframe() {
        context
            .video_decode_pipeline
            .set_skip_nonref_frames(false)?;
        *context.video_decode_skip_nonref_active = false;
        if context.realign_after_decode_recovery
            && !context.output_scheduler.decode_recovery_active()
        {
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
    Ok(())
}

pub(super) struct VideoDecodeRecoveryServiceContext<'a> {
    pub(super) result: std::result::Result<(), String>,
    pub(super) packet: &'a AvPacket,
    pub(super) realign_after_decode_recovery: bool,
    pub(super) video_stream: StreamInfo,
    pub(super) playback_generation: &'a mut PlaybackGeneration,
    pub(super) video_decode_pipeline: &'a mut VideoDecodePipeline,
    pub(super) video_decode_skip_nonref_active: &'a mut bool,
    pub(super) audio_decode_pipeline: Option<&'a mut AudioDecodePipeline>,
    pub(super) subtitle_pipeline: &'a mut SubtitlePipeline,
    pub(super) video_decode_recovery: &'a mut VideoDecodeRecovery,
    pub(super) output_scheduler: &'a mut PlaybackOutputScheduler,
    pub(super) dovi_pipeline: &'a mut DoviPipeline,
    pub(super) video_frame_prepare_worker: &'a mut VideoFramePrepareWorker,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) control: &'a FfmpegControl,
}
