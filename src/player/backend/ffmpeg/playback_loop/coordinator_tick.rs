use super::decode_pipeline_service::DecodePipelineServiceContext;
use super::decoder_input_service::{DecoderInputServiceContext, DecoderInputServiceOutcome};
use super::output_gate_service::OutputGateServiceContext;
use super::output_queue_service::{OutputQueueAfterDecoderInputContext, OutputQueueServiceContext};
use super::playback_services::PlaybackPipelineServices;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackTickStatus {
    Continue,
    Eof,
    Stopped,
}

pub(super) struct PlaybackTickContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) services: &'a mut PlaybackPipelineServices,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_playback_tick(
    context: PlaybackTickContext<'_>,
) -> std::result::Result<PlaybackTickStatus, String> {
    let decode_status =
        context
            .services
            .decode_pipeline
            .service_once(DecodePipelineServiceContext {
                pipeline: &mut *context.pipeline,
                control: context.control,
                session_id: context.session_id,
                event_tx: context.event_tx,
                vo_queue: context.vo_queue,
                frame_presented: context.frame_presented,
                demux_reader_watermark: || context.demux_cache.reader_watermark(),
            })?;
    if decode_status.interrupted() {
        return Ok(PlaybackTickStatus::Continue);
    }

    if context
        .services
        .output_queue
        .before_decoder_input(OutputQueueServiceContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            pipeline: &mut *context.pipeline,
            control: context.control,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            playback_wait: &context.services.wait,
            playback_telemetry: &mut context.services.telemetry,
        })?
        .should_continue()
    {
        return Ok(PlaybackTickStatus::Continue);
    }

    let output_gate_status =
        context
            .services
            .output_gate
            .service_or_wait(OutputGateServiceContext {
                session_id: context.session_id,
                demux_cache: context.demux_cache,
                pipeline: &mut *context.pipeline,
                control: context.control,
                event_tx: context.event_tx,
                vo_queue: context.vo_queue,
                frame_presented: context.frame_presented,
                playback_wait: &context.services.wait,
                playback_telemetry: &mut context.services.telemetry,
            })?;
    if output_gate_status.should_continue() {
        return Ok(PlaybackTickStatus::Continue);
    }

    let video_admission_pressure = context.pipeline.video_packet_admission_pressure(
        output_gate_status.played_until_nsecs,
        output_gate_status.has_audio_output,
    );
    match context
        .services
        .decoder_input
        .service_or_wait(DecoderInputServiceContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            pipeline: &mut *context.pipeline,
            video_admission_pressure,
            control: context.control,
            should_wait_for_demux: output_gate_status.should_wait_for_demux,
            video_output_waiting_for_demux: output_gate_status.video_output_waiting_for_demux,
        })? {
        DecoderInputServiceOutcome::Ready => {}
        DecoderInputServiceOutcome::Backpressured => {
            context
                .services
                .output_queue
                .after_decoder_input_backpressure_or_wait(OutputQueueServiceContext {
                    session_id: context.session_id,
                    demux_cache: context.demux_cache,
                    pipeline: &mut *context.pipeline,
                    control: context.control,
                    event_tx: context.event_tx,
                    vo_queue: context.vo_queue,
                    frame_presented: context.frame_presented,
                    playback_wait: &context.services.wait,
                    playback_telemetry: &mut context.services.telemetry,
                })?;
            return Ok(PlaybackTickStatus::Continue);
        }
        DecoderInputServiceOutcome::WouldBlock => {
            context
                .services
                .output_queue
                .after_demux_would_block_or_wait(OutputQueueServiceContext {
                    session_id: context.session_id,
                    demux_cache: context.demux_cache,
                    pipeline: &mut *context.pipeline,
                    control: context.control,
                    event_tx: context.event_tx,
                    vo_queue: context.vo_queue,
                    frame_presented: context.frame_presented,
                    playback_wait: &context.services.wait,
                    playback_telemetry: &mut context.services.telemetry,
                })?;
            return Ok(PlaybackTickStatus::Continue);
        }
        DecoderInputServiceOutcome::Continue => return Ok(PlaybackTickStatus::Continue),
        DecoderInputServiceOutcome::Eof => return Ok(PlaybackTickStatus::Eof),
        DecoderInputServiceOutcome::Stopped => return Ok(PlaybackTickStatus::Stopped),
    };
    if context.control.has_pending_seek() {
        return Ok(PlaybackTickStatus::Continue);
    }
    context
        .services
        .output_queue
        .after_decoder_input(OutputQueueAfterDecoderInputContext {
            session_id: context.session_id,
            pipeline: &mut *context.pipeline,
            control: context.control,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
        })?;
    Ok(PlaybackTickStatus::Continue)
}
