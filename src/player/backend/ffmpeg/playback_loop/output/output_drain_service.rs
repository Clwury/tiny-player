use super::audio_output_gate::{DelayedAudioStartSilencePolicy, flush_pending_start_audio};
use super::output_gate::{OutputGateResumeStatus, service_initial_video_clock_until_audio_start};
use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{
    PlaybackLoopDeadline, PlaybackPipelineWaitContext, PlaybackPipelineWaitService,
};
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
    AudioClockMode, AudioOutput, AudioOutputDrainStatus, AudioOutputLifecycle, DemuxPacketCache,
    FfmpegControl, PlaybackPipelineState,
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
        let audio_progress = service_eof_pending_audio(context)?;
        if output_drain_stop_or_seek_status(context.control).is_some() {
            return Ok(());
        }
        if context.pipeline.output_scheduler.restart_pending() {
            // The initial transaction owns the unpublished video anchor even
            // at EOF. Do not let the ordinary drain pop it during AO retries.
            wait_after_output_drain_stall(context, "eof_initial_audio_prefill");
            continue;
        }
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
            AudioClockedVideoDrainStatus::Interrupted => {
                break;
            }
            AudioClockedVideoDrainStatus::Drained
                if context
                    .pipeline
                    .output_scheduler
                    .pending_start_audio
                    .is_empty() =>
            {
                break;
            }
            AudioClockedVideoDrainStatus::Drained => {}
            AudioClockedVideoDrainStatus::WaitingAudio { .. } => {}
        }
        if output_drain_stop_or_seek_status(context.control).is_some() {
            return Ok(());
        }
        if !audio_progress && !drain_status.made_progress() {
            wait_after_output_drain_stall(context, "eof_audio_clocked_output_drain");
        }
    }
    Ok(())
}

fn service_eof_pending_audio(
    context: &mut OutputDrainContext<'_>,
) -> std::result::Result<bool, String> {
    let pipeline = &mut *context.pipeline;
    let Some(output) = pipeline.audio_output.as_ref() else {
        return Ok(false);
    };
    if context.control.should_interrupt() {
        return Ok(false);
    }
    if pipeline.output_scheduler.restart_pending()
        && !pipeline.output_scheduler.scheduled_video_queue.is_empty()
        && (!pipeline.output_scheduler.pending_start_audio.is_empty()
            || pipeline
                .output_scheduler
                .initial_audio_prepare_token()
                .is_some())
    {
        let target_nsecs = pipeline
            .output_scheduler
            .initial_audio_prepare_target_nsecs()
            .unwrap_or(pipeline.current_start_position_nsecs);
        let status = service_initial_video_clock_until_audio_start(
            &mut pipeline.output_scheduler,
            output,
            Some(context.demux_cache),
            target_nsecs,
            Some(0),
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            &mut pipeline.position_reporter,
            context.event_tx,
            &mut pipeline.subtitle_pipeline,
            &mut pipeline.buffered_reporter,
            &mut pipeline.current_start_position_nsecs,
            &mut pipeline.scheduler,
        )?;
        match status {
            OutputGateResumeStatus::Resumed => {}
            OutputGateResumeStatus::Rebuffering => {
                pipeline.output_scheduler.clear_rebuffer(context.control);
            }
            _ => return Ok(false),
        }
    }
    if pipeline.output_scheduler.restart_pending()
        && (pipeline.output_scheduler.scheduled_video_queue.is_empty()
            || (pipeline.output_scheduler.pending_start_audio.is_empty()
                && pipeline
                    .output_scheduler
                    .initial_audio_prepare_token()
                    .is_none()))
    {
        // No video anchor remains at EOF; only the final audio tail can drain.
        output.activate_current_audio_output(context.control);
        pipeline
            .output_scheduler
            .set_state(super::PlaybackOutputState::Playing);
    }
    if pipeline.output_scheduler.pending_start_audio.is_empty() {
        let _ = output.finish_audio_input()?;
        context
            .control
            .set_audio_output_lifecycle(AudioOutputLifecycle::Draining);
        return Ok(false);
    }
    // EOF can leave less than a prefill window, or audio beyond the last
    // video frame. Drain all remaining PCM without a video lead waterline.
    let snapshot = output.snapshot()?;
    let mut start_nsecs = snapshot
        .buffered_until_timeline_nsecs
        .max(snapshot.played_timeline_nsecs);
    if snapshot.total_pending_nsecs == 0
        && let Some(first_nsecs) = pipeline
            .output_scheduler
            .pending_start_audio
            .first_start_timeline_nsecs()
        && first_nsecs
            > start_nsecs.saturating_add(super::duration_nsecs(
                super::AUDIO_OUTPUT_VIDEO_LEAD_DURATION,
            ))
    {
        // A terminal timestamp gap cannot acquire more packets at EOF.
        // Resume at the remaining payload once the previous audio has drained.
        output.reset_clock(first_nsecs);
        start_nsecs = first_nsecs;
    }
    let end_nsecs = pipeline
        .output_scheduler
        .pending_start_audio
        .range_nsecs()
        .map(|(_, end)| end)
        .unwrap_or(start_nsecs);
    let made_progress = flush_pending_start_audio(
        &mut pipeline.output_scheduler.pending_start_audio,
        output,
        start_nsecs,
        end_nsecs,
        AudioClockMode::AudioStarted,
        DelayedAudioStartSilencePolicy::Allow,
        context.control,
        &mut pipeline.output_scheduler.scheduled_video_queue,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut pipeline.position_reporter,
        context.event_tx,
        &mut pipeline.subtitle_pipeline,
        &mut pipeline.buffered_reporter,
    )?;
    context
        .control
        .set_audio_output_lifecycle(AudioOutputLifecycle::Draining);
    Ok(made_progress)
}

fn drain_audio_output_until_idle(
    context: &mut OutputDrainContext<'_>,
) -> std::result::Result<(), String> {
    let Some(mut deadline) = context
        .pipeline
        .audio_output
        .as_ref()
        .map(AudioOutput::drain_deadline)
        .transpose()?
        .flatten()
    else {
        return Ok(());
    };
    let mut previous_rate = context.control.playback_rate();
    let mut was_paused = context.control.is_paused();
    loop {
        let rate = context.control.playback_rate();
        let paused = context.control.is_paused();
        if rate != previous_rate || was_paused {
            if let Some(output) = context.pipeline.audio_output.as_ref()
                && let Some(updated) = output.drain_deadline()?
            {
                deadline = updated;
            }
            previous_rate = rate;
        }
        was_paused = paused;
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
        context
            .pipeline
            .scheduler
            .set_playback_rate(context.control.playback_rate());
        if context.control.is_paused() {
            let playback_loop_deadline = if context.control.is_user_paused() {
                PlaybackLoopDeadline::default()
            } else {
                context.pipeline.playback_loop_deadline()
            };
            context
                .playback_wait
                .wait_poll_interval_and_delay_scheduler_until(
                    &mut context.pipeline.scheduler,
                    playback_loop_deadline,
                );
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
            playback_loop_deadline: context.pipeline.playback_loop_deadline(),
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
