use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{PlaybackPipelineWaitContext, PlaybackPipelineWaitService};
use super::video_output_gate::{
    AudioClockedVideoDrainStatus, service_audio_clocked_video_drain_step,
    service_video_clocked_video_queue,
};
use std::sync::{atomic::AtomicBool, mpsc::Sender};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::{
    AudioOutput, AudioOutputDrainStatus, DemuxPacketCache, FfmpegControl, PlaybackPipelineState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputDrainStatus {
    Complete,
    SeekPending,
    Stopped,
}

#[derive(Default)]
pub(super) struct OutputDrainService;

impl OutputDrainService {
    pub(super) fn drain_until_idle(
        &mut self,
        context: &mut OutputDrainContext<'_>,
    ) -> std::result::Result<OutputDrainStatus, String> {
        drain_output_until_idle(context)
    }
}

pub(super) struct OutputDrainContext<'a> {
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

pub(super) fn drain_output_until_idle(
    context: &mut OutputDrainContext<'_>,
) -> std::result::Result<OutputDrainStatus, String> {
    if context.pipeline.audio_output.is_some() {
        drain_audio_clocked_output_until_idle(context)?;
        if let Some(status) = output_drain_stop_or_seek_status(context.control) {
            return Ok(status);
        }
        drain_audio_output_until_idle(context)?;
    } else {
        drain_video_clocked_output_until_idle(context);
    }
    Ok(output_drain_stop_or_seek_status(context.control).unwrap_or(OutputDrainStatus::Complete))
}

fn drain_audio_clocked_output_until_idle(
    context: &mut OutputDrainContext<'_>,
) -> std::result::Result<(), String> {
    loop {
        let drain_status = {
            let Some(output) = context.pipeline.audio_output.as_ref() else {
                return Ok(());
            };
            service_audio_clocked_video_drain_step(
                output,
                context.control,
                &mut context.pipeline.output_scheduler,
                context.session_id,
                context.vo_queue,
                context.frame_presented,
                &mut context.pipeline.position_reporter,
                context.event_tx,
                &mut context.pipeline.subtitle_pipeline,
            )?
        };
        match drain_status {
            AudioClockedVideoDrainStatus::Drained | AudioClockedVideoDrainStatus::Interrupted => {
                break;
            }
            AudioClockedVideoDrainStatus::WaitingAudio { .. } => {}
        }
        if output_drain_stop_or_seek_status(context.control).is_some() {
            return Ok(());
        }
        if !drain_status.made_progress() {
            wait_after_output_drain_stall(context, "eof_audio_clocked_output_drain");
        }
    }
    Ok(())
}

fn drain_audio_output_until_idle(
    context: &mut OutputDrainContext<'_>,
) -> std::result::Result<(), String> {
    let Some(deadline) = context
        .pipeline
        .audio_output
        .as_ref()
        .map(AudioOutput::drain_deadline)
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    loop {
        let drain_status = {
            let Some(output) = context.pipeline.audio_output.as_ref() else {
                return Ok(());
            };
            output.drain_step(deadline, context.control)?
        };
        match drain_status {
            AudioOutputDrainStatus::Drained | AudioOutputDrainStatus::Interrupted => {
                break;
            }
            AudioOutputDrainStatus::Waiting => {}
        }
        if output_drain_stop_or_seek_status(context.control).is_some() {
            return Ok(());
        }
        wait_after_output_drain_stall(context, "eof_audio_output_drain");
    }
    Ok(())
}

fn drain_video_clocked_output_until_idle(context: &mut OutputDrainContext<'_>) {
    while context
        .pipeline
        .output_scheduler
        .snapshot()
        .queued_video_frames
        > 0
    {
        if context.control.is_paused() {
            context
                .playback_wait
                .wait_poll_interval_and_delay_scheduler(&mut context.pipeline.scheduler);
            continue;
        }
        let presented = service_video_clocked_video_queue(
            &context.pipeline.scheduler,
            context.control,
            &mut context.pipeline.output_scheduler,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            &mut context.pipeline.position_reporter,
            context.event_tx,
            &mut context.pipeline.subtitle_pipeline,
            &mut context.pipeline.buffered_reporter,
        );
        if output_drain_stop_or_seek_status(context.control).is_some() {
            return;
        }
        if !presented {
            wait_after_output_drain_stall(context, "eof_video_clocked_output_drain");
        }
    }
}

fn wait_after_output_drain_stall(context: &mut OutputDrainContext<'_>, stall_reason: &'static str) {
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

fn output_drain_stop_or_seek_status(control: &FfmpegControl) -> Option<OutputDrainStatus> {
    if control.should_stop() {
        Some(OutputDrainStatus::Stopped)
    } else if control.has_pending_seek() {
        Some(OutputDrainStatus::SeekPending)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::OutputDrainStatus;

    #[test]
    fn output_drain_statuses_are_distinct() {
        assert_ne!(OutputDrainStatus::Complete, OutputDrainStatus::SeekPending);
        assert_ne!(OutputDrainStatus::SeekPending, OutputDrainStatus::Stopped);
    }
}
