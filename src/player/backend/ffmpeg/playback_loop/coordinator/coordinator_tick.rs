use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::decode_pipeline_service::DecodePipelineServiceContext;
use super::decoder_input_service::{DecoderInputServiceContext, DecoderInputServiceOutcome};
use super::output_gate_service::OutputGateServiceContext;
use super::output_queue_service::{OutputQueueAfterDecoderInputContext, OutputQueueServiceContext};
use super::playback_services::PlaybackPipelineServices;
use super::{
    DemuxPacketCache, FfmpegControl, HttpRingCache, PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER,
    PLAYBACK_COORDINATOR_TICK_TIMING_LOG_AFTER, PlaybackPipelineState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackTickStatus {
    Continue,
    Eof,
    Stopped,
}

pub(super) struct PlaybackTickContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) http_cache: Option<&'a HttpRingCache>,
    pub(super) services: &'a mut PlaybackPipelineServices,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
}

#[derive(Clone, Copy, Default)]
struct PlaybackTickTiming {
    decode_pipeline: Duration,
    output_queue_before: Duration,
    output_gate: Duration,
    decoder_input: Duration,
    output_queue_after: Duration,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_playback_tick(
    context: PlaybackTickContext<'_>,
) -> std::result::Result<PlaybackTickStatus, String> {
    let tick_started_at = Instant::now();
    let mut timing = PlaybackTickTiming::default();

    let stage_started_at = Instant::now();
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
                demux_reader_watermark: || context.demux_cache.cached_reader_watermark(),
            })?;
    timing.decode_pipeline = stage_started_at.elapsed();
    if decode_status.interrupted() {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "decode_pipeline_interrupted",
            None,
        ));
    }

    let stage_started_at = Instant::now();
    let output_queue_before_status =
        context
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
            })?;
    timing.output_queue_before = stage_started_at.elapsed();
    if output_queue_before_status.should_continue() {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "output_queue_before_decoder_input",
            None,
        ));
    }

    let stage_started_at = Instant::now();
    let output_gate_status =
        context
            .services
            .output_gate
            .service_or_wait(OutputGateServiceContext {
                session_id: context.session_id,
                demux_cache: context.demux_cache,
                http_cache: context.http_cache,
                pipeline: &mut *context.pipeline,
                control: context.control,
                event_tx: context.event_tx,
                vo_queue: context.vo_queue,
                frame_presented: context.frame_presented,
                playback_wait: &context.services.wait,
                playback_telemetry: &mut context.services.telemetry,
            })?;
    timing.output_gate = stage_started_at.elapsed();
    if output_gate_status.should_continue() {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "output_gate",
            None,
        ));
    }

    let video_admission_pressure = context.pipeline.video_packet_admission_pressure(
        output_gate_status.played_until_nsecs,
        output_gate_status.has_audio_output,
        context.vo_queue.snapshot(),
    );
    let stage_started_at = Instant::now();
    let decoder_input_outcome =
        context
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
            })?;
    timing.decoder_input = stage_started_at.elapsed();
    match decoder_input_outcome {
        DecoderInputServiceOutcome::Ready => {}
        DecoderInputServiceOutcome::Backpressured => {
            let stage_started_at = Instant::now();
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
            timing.output_queue_after = stage_started_at.elapsed();
            return Ok(finish_playback_tick(
                context.session_id,
                tick_started_at,
                timing,
                PlaybackTickStatus::Continue,
                "decoder_input_backpressured",
                Some(decoder_input_outcome),
            ));
        }
        DecoderInputServiceOutcome::WouldBlock => {
            let stage_started_at = Instant::now();
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
            timing.output_queue_after = stage_started_at.elapsed();
            return Ok(finish_playback_tick(
                context.session_id,
                tick_started_at,
                timing,
                PlaybackTickStatus::Continue,
                "decoder_input_would_block",
                Some(decoder_input_outcome),
            ));
        }
        DecoderInputServiceOutcome::Continue => {
            return Ok(finish_playback_tick(
                context.session_id,
                tick_started_at,
                timing,
                PlaybackTickStatus::Continue,
                "decoder_input_interrupted",
                Some(decoder_input_outcome),
            ));
        }
        DecoderInputServiceOutcome::Eof => {
            return Ok(finish_playback_tick(
                context.session_id,
                tick_started_at,
                timing,
                PlaybackTickStatus::Eof,
                "decoder_input_eof",
                Some(decoder_input_outcome),
            ));
        }
        DecoderInputServiceOutcome::Stopped => {
            return Ok(finish_playback_tick(
                context.session_id,
                tick_started_at,
                timing,
                PlaybackTickStatus::Stopped,
                "decoder_input_stopped",
                Some(decoder_input_outcome),
            ));
        }
    };
    if context.control.has_pending_seek() {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "pending_seek",
            Some(decoder_input_outcome),
        ));
    }
    let stage_started_at = Instant::now();
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
    timing.output_queue_after = stage_started_at.elapsed();
    Ok(finish_playback_tick(
        context.session_id,
        tick_started_at,
        timing,
        PlaybackTickStatus::Continue,
        "ready",
        Some(decoder_input_outcome),
    ))
}

fn finish_playback_tick(
    session_id: PlaybackSessionId,
    tick_started_at: Instant,
    timing: PlaybackTickTiming,
    status: PlaybackTickStatus,
    exit_reason: &'static str,
    decoder_input_outcome: Option<DecoderInputServiceOutcome>,
) -> PlaybackTickStatus {
    log_playback_tick_timing(
        session_id,
        tick_started_at.elapsed(),
        timing,
        status,
        exit_reason,
        decoder_input_outcome,
    );
    status
}

fn log_playback_tick_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    timing: PlaybackTickTiming,
    status: PlaybackTickStatus,
    exit_reason: &'static str,
    decoder_input_outcome: Option<DecoderInputServiceOutcome>,
) {
    tracing::trace!(
        session_id = ?session_id,
        status = ?status,
        exit_reason,
        decoder_input_outcome = ?decoder_input_outcome,
        total_ms = total.as_secs_f64() * 1000.0,
        decode_pipeline_ms = timing.decode_pipeline.as_secs_f64() * 1000.0,
        output_queue_before_ms = timing.output_queue_before.as_secs_f64() * 1000.0,
        output_gate_ms = timing.output_gate.as_secs_f64() * 1000.0,
        decoder_input_ms = timing.decoder_input.as_secs_f64() * 1000.0,
        output_queue_after_ms = timing.output_queue_after.as_secs_f64() * 1000.0,
        "FFmpeg playback coordinator tick timing"
    );
    if total < PLAYBACK_COORDINATOR_TICK_TIMING_LOG_AFTER
        && timing.decode_pipeline < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && timing.output_queue_before < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && timing.output_gate < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && timing.decoder_input < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && timing.output_queue_after < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        status = ?status,
        exit_reason,
        decoder_input_outcome = ?decoder_input_outcome,
        total_ms = total.as_secs_f64() * 1000.0,
        decode_pipeline_ms = timing.decode_pipeline.as_secs_f64() * 1000.0,
        output_queue_before_ms = timing.output_queue_before.as_secs_f64() * 1000.0,
        output_gate_ms = timing.output_gate.as_secs_f64() * 1000.0,
        decoder_input_ms = timing.decoder_input.as_secs_f64() * 1000.0,
        output_queue_after_ms = timing.output_queue_after.as_secs_f64() * 1000.0,
        "FFmpeg playback coordinator tick completed slowly"
    );
}
