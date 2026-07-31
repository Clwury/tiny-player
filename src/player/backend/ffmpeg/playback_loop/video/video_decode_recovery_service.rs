use ffmpeg_sys_next as ffi;

use crate::player::render_host::VideoOutputQueue;

use super::output_gate::DecodeRecoverySource;
use super::video_decode_pipeline::video_decode_error_is_recoverable;
use super::{
    AudioDecodePipeline, AvPacket, DoviPipeline, FfmpegControl, PlaybackGeneration,
    PlaybackOutputScheduler, StreamInfo, SubtitlePipeline, VideoDecodePipeline,
    VideoDecodeRecovery, VideoFramePrepareWorker,
};

fn hevc_decoder_error_output_recovery_target(
    result: &std::result::Result<(), String>,
    codec_id: ffi::AVCodecID,
    realign_after_decode_recovery: bool,
    decode_recovery_active: bool,
    committed_video_end_nsecs: Option<u64>,
) -> Option<u64> {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC
        || realign_after_decode_recovery
        || decode_recovery_active
        || !result.as_ref().is_err_and(|error| {
            video_decode_error_is_recoverable(error) && error.contains("code=-1094995529")
        })
    {
        return None;
    }
    committed_video_end_nsecs
}

pub(super) fn service_video_decode_recovery_result(
    context: VideoDecodeRecoveryServiceContext<'_>,
) -> std::result::Result<(), String> {
    let committed_video_end_nsecs = context.output_scheduler.committed_video_queue_end_nsecs();
    let decoder_error_output_recovery_target = hevc_decoder_error_output_recovery_target(
        &context.result,
        context.video_stream.codec_id,
        context.realign_after_decode_recovery,
        context.output_scheduler.decode_recovery_active(),
        committed_video_end_nsecs,
    );
    let release_resource_pressure_references =
        context.video_decode_pipeline.recover_error_if_needed(
            context.result,
            context.playback_generation,
            context.video_stream.codec_id,
            context.packet,
            context.video_decode_recovery,
            context.realign_after_decode_recovery,
            committed_video_end_nsecs,
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
    if let Some(target_nsecs) = decoder_error_output_recovery_target
        && context.video_decode_recovery.waiting_for_keyframe()
        && !context.output_scheduler.decode_recovery_active()
    {
        let transaction_id = context.playback_generation.current().max(1);
        let session_id = context.control.session_id();
        context.output_scheduler.begin_decode_recovery(
            transaction_id,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            context.control,
            session_id,
        );
        context
            .output_scheduler
            .mark_decode_recovery_replaying(transaction_id);
        tracing::warn!(
            ?session_id,
            transaction_id,
            target_nsecs,
            "armed boundary reanchor for explicit HEVC invalid-data recovery"
        );
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

#[cfg(test)]
mod tests {
    use ffmpeg_sys_next as ffi;

    use super::hevc_decoder_error_output_recovery_target;

    #[test]
    fn only_steady_state_hevc_invalid_data_arms_output_recovery() {
        let target_nsecs = 797_366_645_832;
        let invalid_data = Err(
            "FFmpeg 发送解码包失败：code=-1094995529, error=Invalid data found when processing input"
                .to_string(),
        );

        assert_eq!(
            hevc_decoder_error_output_recovery_target(
                &invalid_data,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                false,
                false,
                Some(target_nsecs),
            ),
            Some(target_nsecs)
        );
        assert_eq!(
            hevc_decoder_error_output_recovery_target(
                &invalid_data,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                true,
                false,
                Some(target_nsecs),
            ),
            None,
            "startup recovery already owns full timeline realignment"
        );
        assert_eq!(
            hevc_decoder_error_output_recovery_target(
                &invalid_data,
                ffi::AVCodecID::AV_CODEC_ID_H264,
                false,
                false,
                Some(target_nsecs),
            ),
            None
        );
        assert_eq!(
            hevc_decoder_error_output_recovery_target(
                &Ok(()),
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                false,
                false,
                Some(target_nsecs),
            ),
            None
        );
    }
}
