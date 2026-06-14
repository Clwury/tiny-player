use super::output_gate::{OutputGateResumeStatus, service_output_gate_resume_if_ready};
use super::output_rebuffer::demux_reader_ready_for_output;
use super::playback_block::video_output_resource_pressure;
use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{PlaybackPipelineWaitContext, PlaybackPipelineWaitService};
use super::*;

#[derive(Default)]
pub(super) struct OutputGateService;

impl OutputGateService {
    pub(super) fn service_or_wait(
        &mut self,
        context: OutputGateServiceContext<'_>,
    ) -> std::result::Result<OutputGateServiceStatus, String> {
        service_output_gate_or_wait(context)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputGateServiceOutcome {
    Ready,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputGateServiceStatus {
    pub(super) outcome: OutputGateServiceOutcome,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
    pub(super) played_until_nsecs: Option<u64>,
    pub(super) has_audio_output: bool,
}

impl OutputGateServiceStatus {
    pub(super) fn should_continue(self) -> bool {
        matches!(self.outcome, OutputGateServiceOutcome::Continue)
    }
}

pub(super) struct OutputGateServiceContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) playback_wait: &'a PlaybackPipelineWaitService,
    pub(super) playback_telemetry: &'a mut PlaybackPipelineTelemetry,
}

fn service_output_gate_or_wait(
    mut context: OutputGateServiceContext<'_>,
) -> std::result::Result<OutputGateServiceStatus, String> {
    let status = output_gate_service_status(&mut context)?;
    let current_start_position_nsecs = context.pipeline.current_start_position_nsecs;
    let video_decode_snapshot = context.pipeline.video_decode_pipeline.snapshot();
    let output_resource_pressure = video_output_resource_pressure(
        context
            .pipeline
            .output_scheduler
            .scheduled_video_queue_len(),
        video_decode_snapshot.queued_frames,
        video_decode_snapshot.in_flight_packets,
        context
            .pipeline
            .video_decode_pipeline
            .info()
            .hardware_accelerated,
        context
            .pipeline
            .output_scheduler
            .scheduled_video_queue_limit_reached(
                context.pipeline.subtitle_pipeline.needs_prefetch(),
            ),
        context.pipeline.output_scheduler.output_fill_phase(),
    );
    match service_output_gate_resume_if_ready(
        &mut context.pipeline.output_scheduler,
        context.pipeline.audio_output.as_ref(),
        context.control,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
        current_start_position_nsecs,
        &mut context.pipeline.current_start_position_nsecs,
        &mut context.pipeline.scheduler,
        output_resource_pressure,
        || context.demux_cache.reader_watermark(),
    )? {
        OutputGateResumeStatus::Resumed => Ok(OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Continue,
            ..status
        }),
        OutputGateResumeStatus::WaitingForDemux => {
            wait_after_output_gate_stall(&mut context, "output_gate_demux_wait");
            Ok(OutputGateServiceStatus {
                outcome: OutputGateServiceOutcome::Continue,
                ..status
            })
        }
        OutputGateResumeStatus::Idle | OutputGateResumeStatus::Waiting => Ok(status),
    }
}

fn output_gate_service_status(
    context: &mut OutputGateServiceContext<'_>,
) -> std::result::Result<OutputGateServiceStatus, String> {
    let vo_snapshot = context.vo_queue.snapshot();
    let render_backlogged = vo_snapshot.render_backlogged();
    let has_audio_output = context.pipeline.audio_output.is_some();
    let audio_output_snapshot = context
        .pipeline
        .audio_output
        .as_ref()
        .map(AudioOutput::snapshot)
        .transpose()?;
    let output_underrun = context
        .pipeline
        .audio_output
        .as_ref()
        .is_some_and(AudioOutput::underrun_active);
    let played_until_nsecs = audio_output_snapshot.map(|snapshot| snapshot.played_timeline_nsecs);
    let output_snapshot = context
        .pipeline
        .output_scheduler
        .snapshot_for_played_until(played_until_nsecs);
    let output_underflowing = output_snapshot.underflowing()
        || audio_output_starving(output_snapshot, audio_output_snapshot);
    let demux_cache_insufficient =
        !demux_reader_ready_for_output(context.demux_cache.reader_watermark(), has_audio_output);
    context
        .pipeline
        .output_scheduler
        .maybe_enter_video_output_rebuffer(
            Instant::now(),
            output_underflowing,
            output_underrun,
            demux_cache_insufficient,
            render_backlogged,
            has_audio_output,
            context.control,
            context.pipeline.audio_output.as_ref(),
            context.session_id,
            output_snapshot.queued_video_forward_nsecs,
        );
    let should_wait_for_demux = context
        .pipeline
        .output_scheduler
        .snapshot()
        .should_wait_for_demux();

    Ok(OutputGateServiceStatus {
        outcome: OutputGateServiceOutcome::Ready,
        should_wait_for_demux,
        video_output_waiting_for_demux: output_snapshot.waiting_for_demux(),
        played_until_nsecs,
        has_audio_output,
    })
}

fn audio_output_starving(
    output_snapshot: PlaybackOutputSnapshot,
    audio_output_snapshot: Option<AudioOutputSnapshot>,
) -> bool {
    matches!(output_snapshot.state, PlaybackOutputState::Playing)
        && output_snapshot.queued_video_frames > 0
        && audio_output_snapshot.is_some_and(|snapshot| snapshot.total_pending_nsecs == 0)
}

fn wait_after_output_gate_stall(
    context: &mut OutputGateServiceContext<'_>,
    stall_reason: &'static str,
) {
    context.playback_wait.wait_after_stall(
        PlaybackPipelineWaitContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            video_decode_pipeline: &context.pipeline.video_decode_pipeline,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_gate_service_status_reports_continue() {
        let status = OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Continue,
            should_wait_for_demux: false,
            video_output_waiting_for_demux: false,
            played_until_nsecs: None,
            has_audio_output: true,
        };

        assert!(status.should_continue());
        assert!(
            !OutputGateServiceStatus {
                outcome: OutputGateServiceOutcome::Ready,
                ..status
            }
            .should_continue()
        );
    }

    #[test]
    fn audio_output_starving_tracks_audio_clock_output_underrun() {
        let playing_with_video = PlaybackOutputSnapshot {
            state: PlaybackOutputState::Playing,
            first_video_frame_pending: false,
            rebuffering: false,
            queued_video_frames: 20,
            queued_video_duration_nsecs: 800_000_000,
            queued_video_range_nsecs: Some((1_000_000_000, 1_800_000_000)),
            queued_video_forward_nsecs: Some(800_000_000),
            video_output_low_water: false,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
        };
        let underrun_audio = AudioOutputSnapshot {
            played_timeline_nsecs: 1_000_000_000,
            buffered_until_timeline_nsecs: 1_000_000_000,
            shared_pending_nsecs: 0,
            queue_pending_nsecs: 0,
            total_pending_nsecs: 0,
            queue_frames: 0,
            queue_generation: 7,
        };
        let buffered_audio = AudioOutputSnapshot {
            total_pending_nsecs: 100_000_000,
            buffered_until_timeline_nsecs: 1_100_000_000,
            ..underrun_audio
        };

        assert!(audio_output_starving(
            playing_with_video,
            Some(underrun_audio)
        ));
        assert!(!audio_output_starving(
            playing_with_video,
            Some(buffered_audio)
        ));
        assert!(!audio_output_starving(
            PlaybackOutputSnapshot {
                queued_video_frames: 0,
                ..playing_with_video
            },
            Some(underrun_audio)
        ));
        assert!(!audio_output_starving(
            PlaybackOutputSnapshot {
                state: PlaybackOutputState::Rebuffering,
                rebuffering: true,
                ..playing_with_video
            },
            Some(underrun_audio)
        ));
    }
}
