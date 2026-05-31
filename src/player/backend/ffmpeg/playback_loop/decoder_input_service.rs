use super::decode::DecodeInputRetryStatus;
use super::demux_packet_pump::{
    DemuxPacketPump, DemuxPacketPumpAdmissionContext, DemuxPacketPumpResult,
};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::video_decode_pipeline::VideoPacketAdmissionPressure;
use super::*;

#[derive(Debug, PartialEq, Eq)]
enum DecoderInputServiceStatus {
    Progress,
    Backpressured,
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecoderInputServiceOutcome {
    Ready,
    Backpressured,
    WouldBlock,
    Continue,
    Eof,
    Stopped,
}

#[derive(Default)]
pub(super) struct DecoderInputService {
    demux_packet_pump: DemuxPacketPump,
}

impl DecoderInputService {
    pub(super) fn service_or_wait(
        &mut self,
        mut context: DecoderInputServiceContext<'_>,
    ) -> std::result::Result<DecoderInputServiceOutcome, String> {
        match service_decoder_input_once(self, &mut context) {
            DecoderInputServiceStatus::Progress => Ok(DecoderInputServiceOutcome::Ready),
            DecoderInputServiceStatus::Backpressured => {
                Ok(DecoderInputServiceOutcome::Backpressured)
            }
            DecoderInputServiceStatus::Eof => Ok(DecoderInputServiceOutcome::Eof),
            DecoderInputServiceStatus::WouldBlock => Ok(DecoderInputServiceOutcome::WouldBlock),
            DecoderInputServiceStatus::Interrupted if context.control.should_stop() => {
                Ok(DecoderInputServiceOutcome::Stopped)
            }
            DecoderInputServiceStatus::Interrupted => Ok(DecoderInputServiceOutcome::Continue),
            DecoderInputServiceStatus::Error(error) => {
                if context.control.has_pending_seek() {
                    Ok(DecoderInputServiceOutcome::Continue)
                } else {
                    Err(error)
                }
            }
        }
    }
}

pub(super) struct DecoderInputServiceContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) video_admission_pressure: VideoPacketAdmissionPressure,
    pub(super) control: &'a FfmpegControl,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
}

fn service_decoder_input_once(
    service: &mut DecoderInputService,
    context: &mut DecoderInputServiceContext<'_>,
) -> DecoderInputServiceStatus {
    let retry_status = match context
        .pipeline
        .retry_pending_decoder_inputs(context.session_id)
    {
        Ok(status) => status,
        Err(error) => return DecoderInputServiceStatus::Error(error),
    };
    if retry_status.backpressured() {
        return DecoderInputServiceStatus::Backpressured;
    }

    let result = service
        .demux_packet_pump
        .poll_and_admit_packet(DemuxPacketPumpAdmissionContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            pipeline: context.pipeline,
            video_admission_pressure: context.video_admission_pressure,
            should_wait_for_demux: context.should_wait_for_demux,
            video_output_waiting_for_demux: context.video_output_waiting_for_demux,
        });
    decoder_input_status_after_retry(retry_status, result)
}

fn decoder_input_status_after_retry(
    retry_status: DecodeInputRetryStatus,
    result: DemuxPacketPumpResult,
) -> DecoderInputServiceStatus {
    let status = decoder_input_status_from_pump(result);
    if retry_status.made_progress()
        && matches!(
            status,
            DecoderInputServiceStatus::WouldBlock | DecoderInputServiceStatus::Eof
        )
    {
        DecoderInputServiceStatus::Progress
    } else {
        status
    }
}

fn decoder_input_status_from_pump(result: DemuxPacketPumpResult) -> DecoderInputServiceStatus {
    match result {
        DemuxPacketPumpResult::Progress => DecoderInputServiceStatus::Progress,
        DemuxPacketPumpResult::Backpressured => DecoderInputServiceStatus::Backpressured,
        DemuxPacketPumpResult::Eof => DecoderInputServiceStatus::Eof,
        DemuxPacketPumpResult::WouldBlock => DecoderInputServiceStatus::WouldBlock,
        DemuxPacketPumpResult::Interrupted => DecoderInputServiceStatus::Interrupted,
        DemuxPacketPumpResult::Error(error) => DecoderInputServiceStatus::Error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_input_service_maps_pump_statuses() {
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Progress),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Backpressured),
            DecoderInputServiceStatus::Backpressured
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::WouldBlock),
            DecoderInputServiceStatus::WouldBlock
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Error("decode".to_string())),
            DecoderInputServiceStatus::Error("decode".to_string())
        );
    }

    #[test]
    fn decoder_input_service_preserves_pending_input_progress() {
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::WouldBlock
            ),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::Eof
            ),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::Backpressured
            ),
            DecoderInputServiceStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_service_outcomes_keep_output_layer_separate() {
        assert_ne!(
            DecoderInputServiceOutcome::Ready,
            DecoderInputServiceOutcome::Backpressured
        );
        assert_ne!(
            DecoderInputServiceOutcome::Backpressured,
            DecoderInputServiceOutcome::WouldBlock
        );
    }
}
