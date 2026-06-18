use super::decode_pipeline_service::{DecodePipelineService, DecodePipelineServiceContext};
use super::drain_phase::{PlaybackDrainPhase, PlaybackDrainResults};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{PlaybackPipelineWaitContext, PlaybackPipelineWaitService};
use super::video_decode_drain_frame_processor::{
    VideoDecodeDrainFrameProcessor, VideoDecodeDrainProcessStatus,
};
use std::sync::{atomic::AtomicBool, mpsc::Sender};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::{DemuxPacketCache, FfmpegControl};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecoderDrainStatus {
    Complete,
    SeekPending,
    Stopped,
}

pub(super) enum DecoderDrainPhaseStatus {
    Complete(PlaybackDrainResults),
    SeekPending,
    Stopped,
}

#[derive(Default)]
pub(super) struct DecoderDrainService;

impl DecoderDrainService {
    pub(super) fn drain_decode_pipelines_until_idle(
        &mut self,
        context: &mut DecoderDrainContext<'_>,
    ) -> std::result::Result<DecoderDrainStatus, String> {
        drain_decode_pipelines_until_idle(context)
    }

    pub(super) fn drain_decoder_outputs_until_complete(
        &mut self,
        context: &mut DecoderDrainContext<'_>,
    ) -> std::result::Result<DecoderDrainStatus, String> {
        drain_decoder_outputs_until_complete(context)
    }
}

pub(super) struct DecoderDrainContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) decode_pipeline: &'a mut DecodePipelineService,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) playback_wait: &'a PlaybackPipelineWaitService,
    pub(super) playback_telemetry: &'a mut PlaybackPipelineTelemetry,
}

fn drain_decode_pipelines_until_idle(
    context: &mut DecoderDrainContext<'_>,
) -> std::result::Result<DecoderDrainStatus, String> {
    while context.pipeline.decoder_outputs_pending_or_in_flight()
        && !context.control.should_interrupt()
    {
        let decode_status = context
            .decode_pipeline
            .service_once(DecodePipelineServiceContext {
                pipeline: &mut *context.pipeline,
                control: context.control,
                session_id: context.session_id,
                event_tx: context.event_tx,
                vo_queue: context.vo_queue,
                frame_presented: context.frame_presented,
                demux_reader_watermark: || context.demux_cache.cached_reader_watermark(),
            })?;
        if decode_status.interrupted() {
            break;
        }
        if !decode_status.made_progress() {
            wait_after_decoder_drain_stall(context, "eof_decode_pipeline_drain");
        }
    }
    Ok(decoder_drain_stop_or_seek_status(context.control).unwrap_or(DecoderDrainStatus::Complete))
}

fn poll_decoder_drain_until_complete(
    context: &mut DecoderDrainContext<'_>,
    drain_phase: &mut PlaybackDrainPhase,
) -> std::result::Result<DecoderDrainPhaseStatus, String> {
    loop {
        if let Some(results) = context.pipeline.poll_decoder_drain_phase(drain_phase)? {
            return Ok(DecoderDrainPhaseStatus::Complete(results));
        }
        if let Some(status) = decoder_drain_stop_or_seek_status(context.control) {
            return Ok(match status {
                DecoderDrainStatus::Complete => unreachable!("complete status is synthesized"),
                DecoderDrainStatus::SeekPending => DecoderDrainPhaseStatus::SeekPending,
                DecoderDrainStatus::Stopped => DecoderDrainPhaseStatus::Stopped,
            });
        }
        wait_after_decoder_drain_stall(context, "decoder_drain");
    }
}

fn drain_decoder_outputs_until_complete(
    context: &mut DecoderDrainContext<'_>,
) -> std::result::Result<DecoderDrainStatus, String> {
    let mut drain_phase = context.pipeline.start_decoder_drain_phase()?;
    let drain_results = match poll_decoder_drain_until_complete(context, &mut drain_phase)? {
        DecoderDrainPhaseStatus::Complete(results) => results,
        DecoderDrainPhaseStatus::SeekPending => return Ok(DecoderDrainStatus::SeekPending),
        DecoderDrainPhaseStatus::Stopped => return Ok(DecoderDrainStatus::Stopped),
    };

    let mut video_drain_processor = context
        .pipeline
        .video_drain_frame_processor(drain_results.video);
    match poll_video_drain_frames_until_complete(context, &mut video_drain_processor)? {
        DecoderDrainStatus::Complete => {}
        DecoderDrainStatus::SeekPending => return Ok(DecoderDrainStatus::SeekPending),
        DecoderDrainStatus::Stopped => return Ok(DecoderDrainStatus::Stopped),
    }

    if let Some(audio_drain_result) = drain_results.audio {
        context.pipeline.process_audio_drain_result(
            audio_drain_result,
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            context.event_tx,
        )?;
        if let Some(status) = decoder_drain_stop_or_seek_status(context.control) {
            return Ok(status);
        }
    }

    Ok(DecoderDrainStatus::Complete)
}

fn poll_video_drain_frames_until_complete(
    context: &mut DecoderDrainContext<'_>,
    processor: &mut VideoDecodeDrainFrameProcessor,
) -> std::result::Result<DecoderDrainStatus, String> {
    loop {
        match context.pipeline.poll_video_drain_processor(
            processor,
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            context.event_tx,
        )? {
            VideoDecodeDrainProcessStatus::Complete => return Ok(DecoderDrainStatus::Complete),
            VideoDecodeDrainProcessStatus::Pending { made_progress } => {
                if let Some(status) = decoder_drain_stop_or_seek_status(context.control) {
                    return Ok(status);
                }
                if !made_progress {
                    wait_after_decoder_drain_stall(context, "video_drain_frame_prepare");
                }
            }
        }
    }
}

fn wait_after_decoder_drain_stall(
    context: &mut DecoderDrainContext<'_>,
    stall_reason: &'static str,
) {
    context.playback_wait.wait_after_stall(
        PlaybackPipelineWaitContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            video_decode_pipeline: &context.pipeline.video_decode_pipeline,
            video_frame_duration_nsecs: context.pipeline.video_frame_duration_nsecs,
            video_frame_prepare_worker: Some(&context.pipeline.video_frame_prepare_worker),
            audio_decode_pipeline: context.pipeline.audio_decode_pipeline.as_ref(),
            subtitle_pipeline: &context.pipeline.subtitle_pipeline,
            output_scheduler: &context.pipeline.output_scheduler,
            audio_output: context.pipeline.audio_output.as_ref(),
            vo_queue: context.vo_queue,
            playback_telemetry: &mut *context.playback_telemetry,
        },
        stall_reason,
    );
}

fn decoder_drain_stop_or_seek_status(control: &FfmpegControl) -> Option<DecoderDrainStatus> {
    if control.should_stop() {
        Some(DecoderDrainStatus::Stopped)
    } else if control.has_pending_seek() {
        Some(DecoderDrainStatus::SeekPending)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::DecoderDrainStatus;

    #[test]
    fn decoder_drain_statuses_are_distinct() {
        assert_ne!(
            DecoderDrainStatus::Complete,
            DecoderDrainStatus::SeekPending
        );
        assert_ne!(DecoderDrainStatus::SeekPending, DecoderDrainStatus::Stopped);
    }
}
