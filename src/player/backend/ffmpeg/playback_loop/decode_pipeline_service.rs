use super::decoded_output_service::{
    AudioDecodeOutputService, AudioDecodeOutputServiceContext, SubtitleDecodeOutputService,
    SubtitleDecodeOutputServiceContext, VideoDecodeOutputService, VideoDecodeOutputServiceContext,
};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::*;

#[derive(Default)]
pub(super) struct DecodePipelineService {
    video_output: VideoDecodeOutputService,
    audio_output: AudioDecodeOutputService,
    subtitle_output: SubtitleDecodeOutputService,
}

impl DecodePipelineService {
    pub(super) fn service_once<F>(
        &mut self,
        context: DecodePipelineServiceContext<'_, F>,
    ) -> std::result::Result<DecodePipelineServiceStatus, String>
    where
        F: FnMut() -> DemuxReaderWatermark,
    {
        self.service_decode_pipelines_once(context)
    }

    fn service_decode_pipelines_once<F>(
        &mut self,
        context: DecodePipelineServiceContext<'_, F>,
    ) -> std::result::Result<DecodePipelineServiceStatus, String>
    where
        F: FnMut() -> DemuxReaderWatermark,
    {
        let mut status = DecodePipelineServiceStatus::default();
        status.record_progress(self.video_output.drain(VideoDecodeOutputServiceContext {
            pipeline: &mut *context.pipeline,
            control: context.control,
            session_id: context.session_id,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            demux_reader_watermark: context.demux_reader_watermark,
        })?);
        if context.control.should_stop() || context.control.has_pending_seek() {
            return Ok(DecodePipelineServiceStatus::from_interrupt());
        }

        status.record_progress(self.audio_output.drain(AudioDecodeOutputServiceContext {
            pipeline: &mut *context.pipeline,
            control: context.control,
            session_id: context.session_id,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            event_tx: context.event_tx,
        })?);
        if context.control.should_stop() || context.control.has_pending_seek() {
            return Ok(DecodePipelineServiceStatus::from_interrupt());
        }

        status.record_progress(
            self.subtitle_output
                .drain(SubtitleDecodeOutputServiceContext {
                    pipeline: &mut *context.pipeline,
                    control: context.control,
                    session_id: context.session_id,
                    event_tx: context.event_tx,
                })?,
        );
        if context.control.should_stop() || context.control.has_pending_seek() {
            return Ok(DecodePipelineServiceStatus::from_interrupt());
        }

        Ok(status)
    }
}

pub(super) struct DecodePipelineServiceContext<'a, F>
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DecodePipelineServiceStatus {
    made_progress: bool,
    interrupted: bool,
}

impl DecodePipelineServiceStatus {
    pub(super) fn made_progress(self) -> bool {
        self.made_progress
    }

    pub(super) fn interrupted(self) -> bool {
        self.interrupted
    }

    fn record_progress(&mut self, made_progress: bool) {
        self.made_progress |= made_progress;
    }

    fn from_interrupt() -> Self {
        Self {
            interrupted: true,
            ..Self::default()
        }
    }
}
