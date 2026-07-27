use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{PlaybackPipelineWaitContext, PlaybackPipelineWaitService};
use super::video_output_gate::{
    audio_clock_availability, service_audio_clocked_video_queue_if_playing,
    service_decode_backpressure_step, service_video_clocked_video_queue_if_audio_clock_unavailable,
};
use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::{
    DemuxPacketCache, FfmpegControl, PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER,
    PlaybackPipelineState,
};

#[derive(Default)]
pub(super) struct OutputQueueService;

impl OutputQueueService {
    pub(super) fn before_decoder_input(
        &mut self,
        context: OutputQueueServiceContext<'_>,
    ) -> std::result::Result<OutputQueueServiceResult, String> {
        let session_id = context.session_id;
        let started_at = Instant::now();
        let result = service_output_queues_before_decoder_input(context);
        if let Ok(result) = result.as_ref() {
            log_output_queue_service_timing(
                session_id,
                "before_decoder_input",
                started_at.elapsed(),
                Some(result.status),
                result.planned_wait,
            );
        }
        result
    }

    pub(super) fn after_decoder_input(
        &mut self,
        context: OutputQueueAfterDecoderInputContext<'_>,
    ) -> std::result::Result<(), String> {
        let session_id = context.session_id;
        let started_at = Instant::now();
        let result = service_output_queues_after_decoder_input(context);
        if result.is_ok() {
            log_output_queue_service_timing(
                session_id,
                "after_decoder_input",
                started_at.elapsed(),
                None,
                Duration::ZERO,
            );
        }
        result
    }

    pub(super) fn after_decoder_input_backpressure_or_wait(
        &mut self,
        context: OutputQueueServiceContext<'_>,
    ) -> std::result::Result<OutputQueueServiceResult, String> {
        let session_id = context.session_id;
        let started_at = Instant::now();
        let result = service_output_queues_after_decoder_input_backpressure_or_wait(context);
        if let Ok(result) = result.as_ref() {
            log_output_queue_service_timing(
                session_id,
                "after_decoder_input_backpressure_or_wait",
                started_at.elapsed(),
                Some(result.status),
                result.planned_wait,
            );
        }
        result
    }

    pub(super) fn after_demux_would_block_or_wait(
        &mut self,
        context: OutputQueueServiceContext<'_>,
    ) -> std::result::Result<OutputQueueServiceResult, String> {
        let session_id = context.session_id;
        let started_at = Instant::now();
        let result = service_output_queues_after_demux_would_block_or_wait(context);
        if let Ok(result) = result.as_ref() {
            log_output_queue_service_timing(
                session_id,
                "after_demux_would_block_or_wait",
                started_at.elapsed(),
                Some(result.status),
                result.planned_wait,
            );
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputQueueServiceStatus {
    Idle,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputQueueServiceResult {
    pub(super) status: OutputQueueServiceStatus,
    pub(super) planned_wait: Duration,
}

impl OutputQueueServiceResult {
    pub(super) fn should_continue(self) -> bool {
        self.status.should_continue()
    }
}

impl OutputQueueServiceStatus {
    pub(super) fn should_continue(self) -> bool {
        matches!(self, Self::Continue)
    }
}

pub(super) struct OutputQueueServiceContext<'a> {
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

pub(super) struct OutputQueueAfterDecoderInputContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
}

fn service_output_queues_before_decoder_input(
    mut context: OutputQueueServiceContext<'_>,
) -> std::result::Result<OutputQueueServiceResult, String> {
    service_audio_clocked_video_queue_if_playing(
        context.pipeline.audio_output.as_ref(),
        context.control,
        &mut context.pipeline.output_scheduler,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    )?;

    let audio_clock = audio_clock_availability(
        context.pipeline.audio_output.as_ref(),
        context
            .pipeline
            .output_scheduler
            .audio_output_clock_stall_fallback_active(),
    )?;
    if service_video_clocked_video_queue_if_audio_clock_unavailable(
        &context.pipeline.scheduler,
        context.control,
        &mut context.pipeline.output_scheduler,
        audio_clock,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    ) {
        if context.control.has_pending_seek() {
            return Ok(OutputQueueServiceResult {
                status: OutputQueueServiceStatus::Continue,
                planned_wait: Duration::ZERO,
            });
        }
        if context
            .pipeline
            .output_scheduler
            .scheduled_video_queue_limit_reached(
                context.pipeline.subtitle_pipeline.needs_prefetch(),
            )
        {
            let planned_wait =
                wait_after_output_queue_stall(&mut context, "scheduled_video_queue_limit");
            return Ok(OutputQueueServiceResult {
                status: OutputQueueServiceStatus::Continue,
                planned_wait,
            });
        }
    }

    Ok(OutputQueueServiceResult {
        status: OutputQueueServiceStatus::Idle,
        planned_wait: Duration::ZERO,
    })
}

fn service_output_queues_after_decoder_input(
    context: OutputQueueAfterDecoderInputContext<'_>,
) -> std::result::Result<(), String> {
    service_audio_clocked_video_queue_if_playing(
        context.pipeline.audio_output.as_ref(),
        context.control,
        &mut context.pipeline.output_scheduler,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    )?;
    Ok(())
}

fn service_output_queues_after_decoder_input_backpressure_or_wait(
    mut context: OutputQueueServiceContext<'_>,
) -> std::result::Result<OutputQueueServiceResult, String> {
    observe_output_queue_stall(&mut context, "demux_decoder_backpressure");
    let audio_clock = audio_clock_availability(
        context.pipeline.audio_output.as_ref(),
        context
            .pipeline
            .output_scheduler
            .audio_output_clock_stall_fallback_active(),
    )?;
    let made_progress = service_decode_backpressure_step(
        &context.pipeline.scheduler,
        context.pipeline.audio_output.as_ref(),
        audio_clock,
        context.control,
        &mut context.pipeline.output_scheduler,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    )?;
    let planned_wait = if made_progress {
        Duration::ZERO
    } else {
        wait_after_output_queue_stall(&mut context, "demux_decoder_backpressure_wait")
    };
    Ok(OutputQueueServiceResult {
        status: OutputQueueServiceStatus::Continue,
        planned_wait,
    })
}

fn service_output_queues_after_demux_would_block_or_wait(
    mut context: OutputQueueServiceContext<'_>,
) -> std::result::Result<OutputQueueServiceResult, String> {
    observe_output_queue_stall(&mut context, "demux_would_block");
    let planned_wait = if service_output_queues_after_demux_would_block(&mut context)? {
        Duration::ZERO
    } else {
        wait_after_output_queue_stall(&mut context, "demux_would_block_wait")
    };
    Ok(OutputQueueServiceResult {
        status: OutputQueueServiceStatus::Continue,
        planned_wait,
    })
}

fn service_output_queues_after_demux_would_block(
    context: &mut OutputQueueServiceContext<'_>,
) -> std::result::Result<bool, String> {
    let audio_progressed = service_audio_clocked_video_queue_if_playing(
        context.pipeline.audio_output.as_ref(),
        context.control,
        &mut context.pipeline.output_scheduler,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    )?;
    let audio_clock = audio_clock_availability(
        context.pipeline.audio_output.as_ref(),
        context
            .pipeline
            .output_scheduler
            .audio_output_clock_stall_fallback_active(),
    )?;
    let video_progressed = service_video_clocked_video_queue_if_audio_clock_unavailable(
        &context.pipeline.scheduler,
        context.control,
        &mut context.pipeline.output_scheduler,
        audio_clock,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
    );
    Ok(audio_progressed || video_progressed)
}

fn observe_output_queue_stall(
    context: &mut OutputQueueServiceContext<'_>,
    stall_reason: &'static str,
) {
    let mut wait_context = PlaybackPipelineWaitContext {
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
    };
    context
        .playback_wait
        .observe_stall(&mut wait_context, stall_reason);
}

fn wait_after_output_queue_stall(
    context: &mut OutputQueueServiceContext<'_>,
    stall_reason: &'static str,
) -> Duration {
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
    )
}

fn log_output_queue_service_timing(
    session_id: PlaybackSessionId,
    phase: &'static str,
    elapsed: Duration,
    status: Option<OutputQueueServiceStatus>,
    planned_wait: Duration,
) {
    let active_elapsed = output_queue_active_elapsed(elapsed, planned_wait);
    tracing::trace!(
        session_id = ?session_id,
        phase,
        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
        planned_wait_ms = planned_wait.as_secs_f64() * 1000.0,
        active_elapsed_ms = active_elapsed.as_secs_f64() * 1000.0,
        status = ?status,
        "FFmpeg output queue service timing"
    );
    if active_elapsed < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        phase,
        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
        planned_wait_ms = planned_wait.as_secs_f64() * 1000.0,
        active_elapsed_ms = active_elapsed.as_secs_f64() * 1000.0,
        status = ?status,
        "FFmpeg output queue service completed slowly"
    );
}

fn output_queue_active_elapsed(elapsed: Duration, planned_wait: Duration) -> Duration {
    elapsed.saturating_sub(planned_wait)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{OutputQueueServiceStatus, output_queue_active_elapsed};

    #[test]
    fn output_queue_service_status_reports_continue() {
        assert!(OutputQueueServiceStatus::Continue.should_continue());
        assert!(!OutputQueueServiceStatus::Idle.should_continue());
    }

    #[test]
    fn planned_poll_wait_is_not_classified_as_slow_output_work() {
        assert_eq!(
            output_queue_active_elapsed(Duration::from_millis(5), Duration::from_millis(5),),
            Duration::ZERO
        );
        assert_eq!(
            output_queue_active_elapsed(Duration::from_millis(8), Duration::from_millis(5),),
            Duration::from_millis(3)
        );
    }
}
