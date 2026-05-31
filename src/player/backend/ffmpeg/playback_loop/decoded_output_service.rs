use super::decoded_audio_frame::drain_ready_audio_decode_output;
use super::decoded_video_frame::drain_ready_video_decode_output;
use super::*;

#[derive(Default)]
pub(super) struct VideoDecodeOutputService;

impl VideoDecodeOutputService {
    pub(super) fn drain<F>(
        &mut self,
        context: VideoDecodeOutputServiceContext<'_, F>,
    ) -> std::result::Result<bool, String>
    where
        F: FnMut() -> DemuxReaderWatermark,
    {
        drain_ready_video_decode_output(
            &mut context.pipeline.video_decode_pipeline,
            context.pipeline.audio_decode_pipeline.as_mut(),
            context.pipeline.video_stream,
            &mut context.pipeline.video_decode_recovery,
            &mut context.pipeline.playback_generation,
            &mut context.pipeline.decoded_video_frame_count,
            &mut context.pipeline.dropped_video_frames_before_start_count,
            context.pipeline.video_frame_duration_nsecs,
            &mut context.pipeline.video_clock,
            &mut context.pipeline.playback_timeline_origin_nsecs,
            context
                .pipeline
                .audio_stream
                .and_then(|stream| stream.start_nsecs),
            &mut context.pipeline.audio_clock,
            &mut context.pipeline.scheduler,
            context.pipeline.audio_output.as_ref(),
            &mut context.pipeline.output_scheduler,
            &mut context.pipeline.dovi_pipeline,
            &mut context.pipeline.buffered_reporter,
            context.control,
            context.session_id,
            context.event_tx,
            context.vo_queue,
            context.frame_presented,
            &mut context.pipeline.position_reporter,
            &mut context.pipeline.subtitle_pipeline,
            &mut context.pipeline.video_frame_prepare_worker,
            &mut context.pipeline.current_start_position_nsecs,
            context.demux_reader_watermark,
        )
    }
}

pub(super) struct VideoDecodeOutputServiceContext<'a, F>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) session_id: PlaybackSessionId,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) demux_reader_watermark: F,
}

#[derive(Default)]
pub(super) struct AudioDecodeOutputService;

impl AudioDecodeOutputService {
    pub(super) fn drain(
        &mut self,
        context: AudioDecodeOutputServiceContext<'_>,
    ) -> std::result::Result<bool, String> {
        drain_ready_audio_decode_output(
            context.pipeline.audio_decode_pipeline.as_mut(),
            context.pipeline.audio_output.as_ref(),
            &mut context.pipeline.audio_clock,
            context.pipeline.current_start_position_nsecs,
            &mut context.pipeline.dropped_audio_frames_before_start_count,
            &mut context.pipeline.output_scheduler,
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            &mut context.pipeline.position_reporter,
            context.event_tx,
            &mut context.pipeline.subtitle_pipeline,
            &mut context.pipeline.buffered_reporter,
        )
    }
}

pub(super) struct AudioDecodeOutputServiceContext<'a> {
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) session_id: PlaybackSessionId,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}

#[derive(Default)]
pub(super) struct SubtitleDecodeOutputService;

impl SubtitleDecodeOutputService {
    pub(super) fn drain(
        &mut self,
        context: SubtitleDecodeOutputServiceContext<'_>,
    ) -> std::result::Result<bool, String> {
        context
            .pipeline
            .subtitle_pipeline
            .drain_ready_decode_output(
                context.pipeline.audio_output.as_ref(),
                context.control,
                context.session_id,
                context.event_tx,
            )
    }
}

pub(super) struct SubtitleDecodeOutputServiceContext<'a> {
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) session_id: PlaybackSessionId,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}
