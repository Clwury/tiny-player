use super::decoded_output_service::{
    AudioDecodeOutputService, AudioDecodeOutputServiceContext, SubtitleDecodeOutputService,
    SubtitleDecodeOutputServiceContext, VideoDecodeOutputService, VideoDecodeOutputServiceContext,
};
use super::playback_pipeline_state::PlaybackPipelineState;
use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::{DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER, DemuxReaderWatermark, FfmpegControl};

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
        let started_at = Instant::now();
        let mut timing = DecodePipelineServiceTiming::default();
        let mut status = DecodePipelineServiceStatus::default();
        let stage_started_at = Instant::now();
        status.record_progress(self.video_output.drain(VideoDecodeOutputServiceContext {
            pipeline: &mut *context.pipeline,
            control: context.control,
            session_id: context.session_id,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            demux_reader_watermark: context.demux_reader_watermark,
        })?);
        timing.video_output = stage_started_at.elapsed();
        if context.control.should_stop() || context.control.has_pending_seek() {
            let interrupted = DecodePipelineServiceStatus::from_interrupt();
            log_decode_pipeline_service_timing(
                context.session_id,
                started_at.elapsed(),
                timing,
                interrupted,
                "after_video_output",
            );
            return Ok(interrupted);
        }

        let stage_started_at = Instant::now();
        status.record_progress(self.audio_output.drain(AudioDecodeOutputServiceContext {
            pipeline: &mut *context.pipeline,
            control: context.control,
            session_id: context.session_id,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            event_tx: context.event_tx,
        })?);
        timing.audio_output = stage_started_at.elapsed();
        if context.control.should_stop() || context.control.has_pending_seek() {
            let interrupted = DecodePipelineServiceStatus::from_interrupt();
            log_decode_pipeline_service_timing(
                context.session_id,
                started_at.elapsed(),
                timing,
                interrupted,
                "after_audio_output",
            );
            return Ok(interrupted);
        }

        let stage_started_at = Instant::now();
        status.record_progress(
            self.subtitle_output
                .drain(SubtitleDecodeOutputServiceContext {
                    pipeline: &mut *context.pipeline,
                    control: context.control,
                    session_id: context.session_id,
                    event_tx: context.event_tx,
                })?,
        );
        timing.subtitle_output = stage_started_at.elapsed();
        if context.control.should_stop() || context.control.has_pending_seek() {
            let interrupted = DecodePipelineServiceStatus::from_interrupt();
            log_decode_pipeline_service_timing(
                context.session_id,
                started_at.elapsed(),
                timing,
                interrupted,
                "after_subtitle_output",
            );
            return Ok(interrupted);
        }

        log_decode_pipeline_service_timing(
            context.session_id,
            started_at.elapsed(),
            timing,
            status,
            "complete",
        );
        Ok(status)
    }
}

#[derive(Clone, Copy, Default)]
struct DecodePipelineServiceTiming {
    video_output: Duration,
    audio_output: Duration,
    subtitle_output: Duration,
}

fn log_decode_pipeline_service_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    timing: DecodePipelineServiceTiming,
    status: DecodePipelineServiceStatus,
    exit_reason: &'static str,
) {
    tracing::trace!(
        session_id = ?session_id,
        exit_reason,
        made_progress = status.made_progress(),
        interrupted = status.interrupted(),
        total_ms = total.as_secs_f64() * 1000.0,
        video_output_ms = timing.video_output.as_secs_f64() * 1000.0,
        audio_output_ms = timing.audio_output.as_secs_f64() * 1000.0,
        subtitle_output_ms = timing.subtitle_output.as_secs_f64() * 1000.0,
        "FFmpeg decode pipeline service timing"
    );
    if total < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.video_output < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.audio_output < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.subtitle_output < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        exit_reason,
        made_progress = status.made_progress(),
        interrupted = status.interrupted(),
        total_ms = total.as_secs_f64() * 1000.0,
        video_output_ms = timing.video_output.as_secs_f64() * 1000.0,
        audio_output_ms = timing.audio_output.as_secs_f64() * 1000.0,
        subtitle_output_ms = timing.subtitle_output.as_secs_f64() * 1000.0,
        "FFmpeg decode pipeline service completed slowly"
    );
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
