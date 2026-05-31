use super::coordinator_gate::PlaybackCoordinatorGateService;
use super::decode_pipeline_service::DecodePipelineService;
use super::decoder_drain_service::DecoderDrainService;
use super::decoder_input_service::DecoderInputService;
use super::output_drain_service::OutputDrainService;
use super::output_gate_service::OutputGateService;
use super::output_queue_service::OutputQueueService;
use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::PlaybackPipelineWaitService;

#[derive(Default)]
pub(super) struct PlaybackPipelineServices {
    pub(super) coordinator_gate: PlaybackCoordinatorGateService,
    pub(super) decode_pipeline: DecodePipelineService,
    pub(super) output_queue: OutputQueueService,
    pub(super) output_gate: OutputGateService,
    pub(super) output_drain: OutputDrainService,
    pub(super) decoder_drain: DecoderDrainService,
    pub(super) decoder_input: DecoderInputService,
    pub(super) wait: PlaybackPipelineWaitService,
    pub(super) telemetry: PlaybackPipelineTelemetry,
}
