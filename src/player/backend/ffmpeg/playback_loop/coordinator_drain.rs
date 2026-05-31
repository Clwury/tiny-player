use super::decoder_drain_service::{DecoderDrainContext, DecoderDrainService, DecoderDrainStatus};
use super::output_drain_service::{OutputDrainContext, OutputDrainService, OutputDrainStatus};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::playback_services::PlaybackPipelineServices;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackEofDrainStatus {
    Complete,
    SeekPending,
    Stopped,
}

pub(super) struct PlaybackEofDrainContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) duration_seconds: Option<f64>,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) services: &'a mut PlaybackPipelineServices,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
}

pub(super) fn service_playback_eof_drain(
    mut context: PlaybackEofDrainContext<'_>,
) -> std::result::Result<PlaybackEofDrainStatus, String> {
    if let Some(status) = decoder_drain_status_to_eof(with_decoder_drain_context(
        &mut context,
        |service, context| service.drain_decode_pipelines_until_idle(context),
    )?) {
        return Ok(status);
    }

    context
        .pipeline
        .output_scheduler
        .clear_rebuffer(context.control);

    if let Some(status) = decoder_drain_status_to_eof(with_decoder_drain_context(
        &mut context,
        |service, context| service.drain_decoder_outputs_until_complete(context),
    )?) {
        return Ok(status);
    }

    context.pipeline.buffered_reporter.report_value(
        context.duration_seconds,
        context.session_id,
        context.event_tx,
    );
    if let Some(status) = output_drain_status_to_eof(with_output_drain_context(
        &mut context,
        |service, context| service.drain_until_idle(context),
    )?) {
        return Ok(status);
    }

    Ok(PlaybackEofDrainStatus::Complete)
}

fn with_decoder_drain_context<T>(
    context: &mut PlaybackEofDrainContext<'_>,
    drain: impl FnOnce(
        &mut DecoderDrainService,
        &mut DecoderDrainContext<'_>,
    ) -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let services = &mut *context.services;
    drain(
        &mut services.decoder_drain,
        &mut DecoderDrainContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            decode_pipeline: &mut services.decode_pipeline,
            pipeline: &mut *context.pipeline,
            control: context.control,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            playback_wait: &services.wait,
            playback_telemetry: &mut services.telemetry,
        },
    )
}

fn with_output_drain_context<T>(
    context: &mut PlaybackEofDrainContext<'_>,
    drain: impl FnOnce(
        &mut OutputDrainService,
        &mut OutputDrainContext<'_>,
    ) -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    let services = &mut *context.services;
    drain(
        &mut services.output_drain,
        &mut OutputDrainContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            pipeline: &mut *context.pipeline,
            control: context.control,
            event_tx: context.event_tx,
            vo_queue: context.vo_queue,
            frame_presented: context.frame_presented,
            playback_wait: &services.wait,
            playback_telemetry: &mut services.telemetry,
        },
    )
}

fn decoder_drain_status_to_eof(status: DecoderDrainStatus) -> Option<PlaybackEofDrainStatus> {
    match status {
        DecoderDrainStatus::Complete => None,
        DecoderDrainStatus::SeekPending => Some(PlaybackEofDrainStatus::SeekPending),
        DecoderDrainStatus::Stopped => Some(PlaybackEofDrainStatus::Stopped),
    }
}

fn output_drain_status_to_eof(status: OutputDrainStatus) -> Option<PlaybackEofDrainStatus> {
    match status {
        OutputDrainStatus::Complete => None,
        OutputDrainStatus::SeekPending => Some(PlaybackEofDrainStatus::SeekPending),
        OutputDrainStatus::Stopped => Some(PlaybackEofDrainStatus::Stopped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_maps_decoder_drain_terminal_statuses() {
        assert_eq!(
            decoder_drain_status_to_eof(DecoderDrainStatus::Complete),
            None
        );
        assert_eq!(
            decoder_drain_status_to_eof(DecoderDrainStatus::SeekPending),
            Some(PlaybackEofDrainStatus::SeekPending)
        );
        assert_eq!(
            decoder_drain_status_to_eof(DecoderDrainStatus::Stopped),
            Some(PlaybackEofDrainStatus::Stopped)
        );
    }

    #[test]
    fn coordinator_maps_output_drain_terminal_statuses() {
        assert_eq!(
            output_drain_status_to_eof(OutputDrainStatus::Complete),
            None
        );
        assert_eq!(
            output_drain_status_to_eof(OutputDrainStatus::SeekPending),
            Some(PlaybackEofDrainStatus::SeekPending)
        );
        assert_eq!(
            output_drain_status_to_eof(OutputDrainStatus::Stopped),
            Some(PlaybackEofDrainStatus::Stopped)
        );
    }
}
