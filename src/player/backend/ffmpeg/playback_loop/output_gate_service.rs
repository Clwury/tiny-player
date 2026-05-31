use super::output_gate::{OutputGateResumeStatus, service_output_gate_resume_if_ready};
use super::output_rebuffer::demux_reader_ready_for_output;
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
    let played_until_nsecs = audio_output_snapshot.map(|snapshot| snapshot.played_timeline_nsecs);
    let output_snapshot = context
        .pipeline
        .output_scheduler
        .snapshot_for_played_until(played_until_nsecs);
    let video_output_underflowing = output_snapshot.underflowing();
    let demux_cache_insufficient = demux_cache_insufficient_for_output_rebuffer(
        video_output_underflowing,
        has_audio_output,
        render_backlogged,
        || context.demux_cache.reader_watermark(),
    );
    context
        .pipeline
        .output_scheduler
        .maybe_enter_video_output_rebuffer(
            Instant::now(),
            video_output_underflowing,
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

fn demux_cache_insufficient_for_output_rebuffer<F>(
    video_output_underflowing: bool,
    has_audio_output: bool,
    render_backlogged: bool,
    demux_reader_watermark: F,
) -> bool
where
    F: FnOnce() -> DemuxReaderWatermark,
{
    if !video_output_underflowing || !has_audio_output || render_backlogged {
        return false;
    }

    !demux_reader_ready_for_output(demux_reader_watermark(), has_audio_output)
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

    fn ready_demux_watermark() -> DemuxReaderWatermark {
        let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
        DemuxReaderWatermark {
            video_forward_nsecs: Some(target_nsecs),
            audio_forward_nsecs: Some(target_nsecs),
            selected_min_forward_nsecs: Some(target_nsecs),
            video_underrun: false,
            audio_underrun: false,
            video_idle: false,
            audio_idle: false,
            underrun: false,
            idle: false,
            forward_bytes: 1024,
        }
    }

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
    fn output_gate_rebuffer_does_not_query_demux_when_render_is_backlogged() {
        assert!(!demux_cache_insufficient_for_output_rebuffer(
            true,
            true,
            true,
            || panic!("render backlog must not ask demux cache for rebuffer")
        ));
    }

    #[test]
    fn output_gate_rebuffer_uses_demux_watermark_only_for_real_output_underflow() {
        let insufficient = demux_cache_insufficient_for_output_rebuffer(true, true, false, || {
            DemuxReaderWatermark {
                video_forward_nsecs: Some(1),
                audio_forward_nsecs: Some(1),
                selected_min_forward_nsecs: Some(1),
                ..ready_demux_watermark()
            }
        });
        assert!(insufficient);

        assert!(!demux_cache_insufficient_for_output_rebuffer(
            false,
            true,
            false,
            || panic!("non-underflow output must not ask demux cache for rebuffer")
        ));
        assert!(!demux_cache_insufficient_for_output_rebuffer(
            true,
            false,
            false,
            || panic!("video-only output must not ask demux cache for audio rebuffer")
        ));
    }
}
