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
use super::output_gate_service::{OutputGateServiceContext, OutputGateServiceStatus};
use super::output_queue_service::{OutputQueueAfterDecoderInputContext, OutputQueueServiceContext};
use super::playback_services::PlaybackPipelineServices;
use super::video_decode_pipeline::{HevcDecodeChainRecoveryAction, HevcStartupStallObservation};
use super::video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState};
use super::{
    DemuxPacketCache, DemuxReaderWatermark, FfmpegControl, HttpRingCache,
    PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER, PLAYBACK_COORDINATOR_TICK_TIMING_LOG_AFTER,
    PlaybackBlockReason, PlaybackPipelineState, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    duration_nsecs,
};
use ffmpeg_sys_next as ffi;

const STARTUP_FIRST_FRAME_DECODER_WARMUP_BUDGET: Duration = Duration::from_millis(4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackTickStatus {
    Continue,
    ForceLowLevelSeek,
    ForceRebufferAudioRealign,
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
    mut context: PlaybackTickContext<'_>,
) -> std::result::Result<PlaybackTickStatus, String> {
    let tick_started_at = Instant::now();
    let mut timing = PlaybackTickTiming::default();

    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "decode_pipeline",
        "enter",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_decode_pipeline_enter",
        None,
    )? {
        return Ok(status);
    }
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "decode_pipeline",
        "exit",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_decode_pipeline_exit",
        None,
    )? {
        return Ok(status);
    }
    if let Some(status) = hevc_decode_chain_fallback_tick_status(
        context
            .pipeline
            .video_decode_pipeline
            .hevc_decode_chain_fallback_pending(),
    ) {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            status,
            "hevc_decode_chain_fallback_pending",
            None,
        ));
    }
    if context
        .pipeline
        .output_scheduler
        .rebuffer_audio_realign_request_pending()
    {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::ForceRebufferAudioRealign,
            "rebuffer_audio_realign_requested",
            None,
        ));
    }
    if decode_status.interrupted() {
        if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
            context.session_id,
            context.pipeline,
            context.demux_cache.cached_reader_watermark(),
            tick_started_at,
            timing,
            "playback_watchdog_decode_pipeline_interrupted",
            None,
        )? {
            return Ok(status);
        }
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "decode_pipeline_interrupted",
            None,
        ));
    }

    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_queue_before",
        "enter",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_queue_before_enter",
        None,
    )? {
        return Ok(status);
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_queue_before",
        "exit",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_queue_before_exit",
        None,
    )? {
        return Ok(status);
    }
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

    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_gate",
        "enter",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_gate_enter",
        None,
    )? {
        return Ok(status);
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_gate",
        "exit",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_gate_exit",
        None,
    )? {
        return Ok(status);
    }
    if let Some(status) = service_hevc_startup_stall_watchdog(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
    )? {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            status,
            "hevc_startup_first_frame_watchdog",
            None,
        ));
    }
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "decoder_input",
        "enter",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_decoder_input_enter",
        None,
    )? {
        return Ok(status);
    }
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "decoder_input",
        "exit",
    );
    if let Some(status) = service_hevc_startup_stall_watchdog(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
    )? {
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            status,
            "hevc_startup_watchdog_decoder_input_exit",
            Some(decoder_input_outcome),
        ));
    }
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_decoder_input_exit",
        Some(decoder_input_outcome),
    )? {
        return Ok(status);
    }
    match decoder_input_outcome {
        DecoderInputServiceOutcome::Ready => {}
        DecoderInputServiceOutcome::Backpressured => {
            if let Some(status) =
                service_startup_first_frame_decoder_warmup(&mut context, output_gate_status)?
            {
                return Ok(finish_playback_tick(
                    context.session_id,
                    tick_started_at,
                    timing,
                    status,
                    "startup_first_frame_decoder_warmup",
                    Some(decoder_input_outcome),
                ));
            }
            log_cached_seek_watchdog_tick_stage(
                context.session_id,
                context.pipeline,
                "output_queue_after",
                "enter",
            );
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "cached_seek_watchdog_output_queue_after_backpressure_enter",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            log_cached_seek_watchdog_tick_stage(
                context.session_id,
                context.pipeline,
                "output_queue_after",
                "exit",
            );
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "cached_seek_watchdog_output_queue_after_backpressure_exit",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            log_cached_seek_watchdog_tick_stage(
                context.session_id,
                context.pipeline,
                "output_queue_after",
                "enter",
            );
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "cached_seek_watchdog_output_queue_after_would_block_enter",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            log_cached_seek_watchdog_tick_stage(
                context.session_id,
                context.pipeline,
                "output_queue_after",
                "exit",
            );
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "cached_seek_watchdog_output_queue_after_would_block_exit",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "playback_watchdog_decoder_input_interrupted",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "playback_watchdog_decoder_input_eof",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
            if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                tick_started_at,
                timing,
                "playback_watchdog_decoder_input_stopped",
                Some(decoder_input_outcome),
            )? {
                return Ok(status);
            }
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
        if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
            context.session_id,
            context.pipeline,
            context.demux_cache.cached_reader_watermark(),
            tick_started_at,
            timing,
            "playback_watchdog_pending_seek",
            Some(decoder_input_outcome),
        )? {
            return Ok(status);
        }
        return Ok(finish_playback_tick(
            context.session_id,
            tick_started_at,
            timing,
            PlaybackTickStatus::Continue,
            "pending_seek",
            Some(decoder_input_outcome),
        ));
    }
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_queue_after",
        "enter",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_queue_after_enter",
        Some(decoder_input_outcome),
    )? {
        return Ok(status);
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
    log_cached_seek_watchdog_tick_stage(
        context.session_id,
        context.pipeline,
        "output_queue_after",
        "exit",
    );
    if let Some(status) = finish_if_playback_watchdog_deadline_elapsed(
        context.session_id,
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
        tick_started_at,
        timing,
        "cached_seek_watchdog_output_queue_after_exit",
        Some(decoder_input_outcome),
    )? {
        return Ok(status);
    }
    log_ready_tick_empty_output_diagnostic(
        context.session_id,
        context.pipeline,
        context.demux_cache,
        output_gate_status,
        decoder_input_outcome,
    );
    Ok(finish_playback_tick(
        context.session_id,
        tick_started_at,
        timing,
        PlaybackTickStatus::Continue,
        "ready",
        Some(decoder_input_outcome),
    ))
}

fn hevc_decode_chain_fallback_tick_status(pending: bool) -> Option<PlaybackTickStatus> {
    pending.then_some(PlaybackTickStatus::ForceLowLevelSeek)
}

fn service_hevc_startup_stall_watchdog(
    session_id: PlaybackSessionId,
    pipeline: &mut PlaybackPipelineState,
    demux_watermark: DemuxReaderWatermark,
) -> std::result::Result<Option<PlaybackTickStatus>, String> {
    let output_snapshot = pipeline.output_scheduler.snapshot();
    let fallback_target_nsecs = output_snapshot
        .video_output_rebuffer_anchor
        .map(|anchor| anchor.timeline_nsecs)
        .or_else(|| {
            output_snapshot
                .queued_video_range_nsecs
                .map(|(start, _)| start)
        })
        .unwrap_or(pipeline.current_start_position_nsecs);
    match pipeline
        .video_decode_pipeline
        .observe_hevc_startup_stall(HevcStartupStallObservation {
            session_id,
            codec_id: pipeline.video_stream.codec_id,
            hardware_accelerated: pipeline.video_decode_pipeline.info().hardware_accelerated,
            video_decode_snapshot: pipeline.video_decode_pipeline.snapshot(),
            now: Instant::now(),
            output_snapshot,
            demux_watermark,
            has_audio_output: pipeline.audio_output.is_some(),
            fallback_target_nsecs,
        }) {
        HevcDecodeChainRecoveryAction::None => {}
        HevcDecodeChainRecoveryAction::SoftRecovery => {
            pipeline.soft_recover_hevc_decode_chain(session_id)?;
            return Ok(Some(PlaybackTickStatus::Continue));
        }
    }

    if pipeline
        .video_decode_pipeline
        .hevc_decode_chain_fallback_pending()
    {
        return Ok(Some(PlaybackTickStatus::ForceLowLevelSeek));
    }
    Ok(None)
}

pub(super) fn service_hevc_startup_stall_watchdog_if_due(
    session_id: PlaybackSessionId,
    pipeline: &mut PlaybackPipelineState,
    demux_watermark: DemuxReaderWatermark,
    checkpoint: &'static str,
) -> std::result::Result<Option<PlaybackTickStatus>, String> {
    let Some(deadline) = pipeline
        .video_decode_pipeline
        .hevc_startup_stall_watchdog_deadline()
    else {
        return Ok(None);
    };
    let now = Instant::now();
    if now < deadline {
        return Ok(None);
    }

    let output_snapshot = pipeline.output_scheduler.snapshot();
    let video_decode_snapshot = pipeline.video_decode_pipeline.snapshot();
    let fallback_pending_before = pipeline
        .video_decode_pipeline
        .hevc_decode_chain_fallback_pending();
    let reject_reason = hevc_startup_watchdog_reject_reason(
        pipeline.video_stream.codec_id,
        pipeline.video_decode_pipeline.info().hardware_accelerated,
        output_snapshot.queued_video_frames,
        video_decode_snapshot,
    );
    tracing::debug!(
        session_id = ?session_id,
        checkpoint,
        overdue_ms = now.saturating_duration_since(deadline).as_secs_f64() * 1000.0,
        video_decode_state = ?video_decode_snapshot.state,
        in_flight_packets = video_decode_snapshot.in_flight_packets,
        completed_packets = video_decode_snapshot.completed_packets,
        decoded_queued_frames = video_decode_snapshot.queued_frames,
        queued_video_frames = output_snapshot.queued_video_frames,
        output_rebuffering = output_snapshot.rebuffering,
        fallback_pending = fallback_pending_before,
        reject_reason,
        "HEVC startup watchdog deadline reached"
    );

    let status = service_hevc_startup_stall_watchdog(session_id, pipeline, demux_watermark)?;
    if status.is_none() {
        tracing::warn!(
            session_id = ?session_id,
            checkpoint,
            overdue_ms = now.saturating_duration_since(deadline).as_secs_f64() * 1000.0,
            video_decode_state = ?video_decode_snapshot.state,
            in_flight_packets = video_decode_snapshot.in_flight_packets,
            completed_packets = video_decode_snapshot.completed_packets,
            decoded_queued_frames = video_decode_snapshot.queued_frames,
            queued_video_frames = output_snapshot.queued_video_frames,
            output_rebuffering = output_snapshot.rebuffering,
            fallback_pending_before,
            fallback_pending_after = pipeline
                .video_decode_pipeline
                .hevc_decode_chain_fallback_pending(),
            reject_reason,
            "HEVC startup watchdog deadline reached but did not trigger fallback"
        );
    }
    Ok(status)
}

fn hevc_startup_watchdog_reject_reason(
    codec_id: ffi::AVCodecID,
    hardware_accelerated: bool,
    queued_video_frames: usize,
    video_decode_snapshot: VideoDecodeWorkerSnapshot,
) -> &'static str {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return "non_hevc";
    }
    if !hardware_accelerated {
        return "software_decoder";
    }
    if !matches!(
        video_decode_snapshot.state,
        VideoDecodeWorkerState::Decoding
    ) {
        return "decoder_not_decoding";
    }
    if video_decode_snapshot.in_flight_packets == 0 {
        return "no_in_flight_packets";
    }
    if video_decode_snapshot.completed_packets > 0 {
        return "completed_packets_pending";
    }
    if video_decode_snapshot.queued_frames > 0 {
        return "decoded_frames_queued";
    }
    if queued_video_frames > 0 {
        return "output_frames_queued";
    }
    "eligible"
}

fn startup_first_frame_decoder_warmup_needed(
    pipeline: &PlaybackPipelineState,
    demux_watermark: DemuxReaderWatermark,
) -> bool {
    let output_snapshot = pipeline.output_scheduler.snapshot();
    if !output_snapshot.first_video_frame_pending
        || output_snapshot.queued_video_frames > 0
        || demux_watermark.underrun
        || demux_watermark.video_underrun
        || (pipeline.audio_output.is_some() && demux_watermark.audio_underrun)
        || demux_watermark
            .selected_min_forward_nsecs
            .is_none_or(|forward| forward < duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION))
    {
        return false;
    }

    let snapshot = pipeline.video_decode_pipeline.snapshot();
    let blocked_on = pipeline
        .decoder_input_snapshot(false)
        .video_decode_blocked_on;
    snapshot.queued_frames == 0
        && (snapshot.in_flight_packets > 0
            || snapshot.completed_packets > 0
            || matches!(
                blocked_on,
                Some(
                    PlaybackBlockReason::DecoderInFlight
                        | PlaybackBlockReason::DecoderOutputPending
                        | PlaybackBlockReason::PacketQueueFull
                )
            ))
}

fn service_startup_first_frame_decoder_warmup(
    context: &mut PlaybackTickContext<'_>,
    output_gate_status: OutputGateServiceStatus,
) -> std::result::Result<Option<PlaybackTickStatus>, String> {
    if !startup_first_frame_decoder_warmup_needed(
        context.pipeline,
        context.demux_cache.cached_reader_watermark(),
    ) {
        return Ok(None);
    }

    let started_at = Instant::now();
    let deadline = started_at + STARTUP_FIRST_FRAME_DECODER_WARMUP_BUDGET;
    let mut iterations = 0_u64;
    let mut made_progress = false;
    let mut last_decoder_input_outcome = None;

    while Instant::now() < deadline
        && startup_first_frame_decoder_warmup_needed(
            context.pipeline,
            context.demux_cache.cached_reader_watermark(),
        )
    {
        iterations = iterations.saturating_add(1);
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
        made_progress |= decode_status.made_progress();
        if decode_status.interrupted() || context.control.has_pending_seek() {
            if let Some(status) = service_hevc_startup_stall_watchdog_if_due(
                context.session_id,
                context.pipeline,
                context.demux_cache.cached_reader_watermark(),
                "startup_first_frame_decoder_warmup_interrupted",
            )? {
                return Ok(Some(status));
            }
            return Ok(Some(PlaybackTickStatus::Continue));
        }
        if context
            .pipeline
            .video_decode_pipeline
            .hevc_decode_chain_fallback_pending()
        {
            return Ok(Some(PlaybackTickStatus::ForceLowLevelSeek));
        }

        if let Some(status) = service_hevc_startup_stall_watchdog(
            context.session_id,
            context.pipeline,
            context.demux_cache.cached_reader_watermark(),
        )? {
            return Ok(Some(status));
        }

        let video_admission_pressure = context.pipeline.video_packet_admission_pressure(
            output_gate_status.played_until_nsecs,
            output_gate_status.has_audio_output,
            context.vo_queue.snapshot(),
        );
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
                    video_output_waiting_for_demux: output_gate_status
                        .video_output_waiting_for_demux,
                })?;
        last_decoder_input_outcome = Some(decoder_input_outcome);
        match decoder_input_outcome {
            DecoderInputServiceOutcome::Ready => made_progress = true,
            DecoderInputServiceOutcome::Backpressured | DecoderInputServiceOutcome::WouldBlock => {}
            DecoderInputServiceOutcome::Continue => {
                if let Some(status) = service_hevc_startup_stall_watchdog_if_due(
                    context.session_id,
                    context.pipeline,
                    context.demux_cache.cached_reader_watermark(),
                    "startup_first_frame_decoder_warmup_decoder_input_interrupted",
                )? {
                    return Ok(Some(status));
                }
                return Ok(Some(PlaybackTickStatus::Continue));
            }
            DecoderInputServiceOutcome::Eof => {
                if let Some(status) = service_hevc_startup_stall_watchdog_if_due(
                    context.session_id,
                    context.pipeline,
                    context.demux_cache.cached_reader_watermark(),
                    "startup_first_frame_decoder_warmup_decoder_input_eof",
                )? {
                    return Ok(Some(status));
                }
                return Ok(Some(PlaybackTickStatus::Eof));
            }
            DecoderInputServiceOutcome::Stopped => {
                if let Some(status) = service_hevc_startup_stall_watchdog_if_due(
                    context.session_id,
                    context.pipeline,
                    context.demux_cache.cached_reader_watermark(),
                    "startup_first_frame_decoder_warmup_decoder_input_stopped",
                )? {
                    return Ok(Some(status));
                }
                return Ok(Some(PlaybackTickStatus::Stopped));
            }
        }
        if !decode_status.made_progress()
            && !matches!(decoder_input_outcome, DecoderInputServiceOutcome::Ready)
        {
            break;
        }
    }

    if made_progress {
        let snapshot = context.pipeline.video_decode_pipeline.snapshot();
        tracing::debug!(
            session_id = ?context.session_id,
            iterations,
            elapsed_ms = started_at.elapsed().as_secs_f64() * 1000.0,
            last_decoder_input_outcome = ?last_decoder_input_outcome,
            video_decode_state = ?snapshot.state,
            video_decode_queued_frames = snapshot.queued_frames,
            video_decode_in_flight_packets = snapshot.in_flight_packets,
            video_decode_completed_packets = snapshot.completed_packets,
            "serviced startup first-frame decoder warmup after decoder input backpressure"
        );
        return Ok(Some(PlaybackTickStatus::Continue));
    }

    Ok(None)
}

fn log_ready_tick_empty_output_diagnostic(
    session_id: PlaybackSessionId,
    pipeline: &PlaybackPipelineState,
    demux_cache: &DemuxPacketCache,
    output_gate_status: OutputGateServiceStatus,
    decoder_input_outcome: DecoderInputServiceOutcome,
) {
    let output_snapshot = pipeline.output_scheduler.snapshot();
    if output_snapshot.queued_video_frames > 0
        || !(output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
    {
        return;
    }

    let demux_watermark = demux_cache.cached_reader_watermark();
    let demux_packet_snapshot = demux_cache.packet_queue_snapshot();
    let decoder_input =
        pipeline.decoder_input_snapshot(output_gate_status.output_resource_pressure);
    let video_decode_snapshot = decoder_input.video_decode_snapshot;
    tracing::debug!(
        session_id = ?session_id,
        decoder_input_outcome = ?decoder_input_outcome,
        output_gate_should_wait_for_demux = output_gate_status.should_wait_for_demux,
        output_gate_video_waiting_for_demux = output_gate_status.video_output_waiting_for_demux,
        output_gate_played_until_nsecs = ?output_gate_status.played_until_nsecs,
        has_audio_output = output_gate_status.has_audio_output,
        output_resource_pressure = output_gate_status.output_resource_pressure,
        output_state = ?output_snapshot.state,
        first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        output_rebuffer_anchor = ?output_snapshot.video_output_rebuffer_anchor,
        pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
        demux_packet_queued = demux_packet_snapshot.total_packets,
        demux_packet_bytes = demux_packet_snapshot.total_bytes,
        demux_packet_streams = ?demux_packet_snapshot.streams,
        demux_min_forward_ms = ?demux_watermark
            .selected_min_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_video_forward_ms = ?demux_watermark
            .video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_audio_forward_ms = ?demux_watermark
            .audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_underrun = demux_watermark.underrun,
        demux_video_underrun = demux_watermark.video_underrun,
        demux_audio_underrun = demux_watermark.audio_underrun,
        video_decode_blocked_on = ?decoder_input
            .video_decode_blocked_on
            .map(PlaybackBlockReason::as_str),
        video_decode_state = ?video_decode_snapshot.state,
        video_decode_queued_frames = video_decode_snapshot.queued_frames,
        video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
        video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
        video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
        video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
        video_decode_completed_packets = video_decode_snapshot.completed_packets,
        "FFmpeg playback coordinator ready tick exited with empty video output"
    );
}

fn log_cached_seek_watchdog_tick_stage(
    session_id: PlaybackSessionId,
    pipeline: &PlaybackPipelineState,
    stage: &'static str,
    event: &'static str,
) {
    let Some(watchdog) = pipeline.cached_seek_recovery_watchdog_snapshot() else {
        return;
    };
    let output_snapshot = pipeline.output_scheduler.snapshot();
    tracing::trace!(
        session_id = ?session_id,
        stage,
        event,
        target_nsecs = watchdog.target_nsecs,
        elapsed_ms = watchdog.elapsed.as_secs_f64() * 1000.0,
        remaining_ms = watchdog.remaining.as_secs_f64() * 1000.0,
        expired = pipeline.cached_seek_recovery_watchdog_expired(),
        video_packets_since_seek = watchdog.video_packets_since_seek,
        queued_video_frames = output_snapshot.queued_video_frames,
        first_video_frame_pending = output_snapshot.first_video_frame_pending,
        "FFmpeg playback coordinator watchdog stage"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackWatchdogDeadline {
    CachedSeekRecovery,
    HevcStartupStall,
}

fn playback_watchdog_deadline_due(
    cached_seek_recovery_expired: bool,
    hevc_startup_stall_deadline: Option<Instant>,
    now: Instant,
) -> Option<PlaybackWatchdogDeadline> {
    if cached_seek_recovery_expired {
        return Some(PlaybackWatchdogDeadline::CachedSeekRecovery);
    }
    hevc_startup_stall_deadline
        .is_some_and(|deadline| now >= deadline)
        .then_some(PlaybackWatchdogDeadline::HevcStartupStall)
}

fn finish_if_playback_watchdog_deadline_elapsed(
    session_id: PlaybackSessionId,
    pipeline: &mut PlaybackPipelineState,
    demux_watermark: DemuxReaderWatermark,
    tick_started_at: Instant,
    timing: PlaybackTickTiming,
    checkpoint: &'static str,
    decoder_input_outcome: Option<DecoderInputServiceOutcome>,
) -> std::result::Result<Option<PlaybackTickStatus>, String> {
    match playback_watchdog_deadline_due(
        pipeline.cached_seek_recovery_watchdog_expired(),
        pipeline
            .video_decode_pipeline
            .hevc_startup_stall_watchdog_deadline(),
        Instant::now(),
    ) {
        Some(PlaybackWatchdogDeadline::CachedSeekRecovery) => {
            if let Some(status) = finish_if_cached_seek_watchdog_deadline_elapsed(
                session_id,
                pipeline,
                tick_started_at,
                timing,
                checkpoint,
                decoder_input_outcome,
            ) {
                return Ok(Some(status));
            }
        }
        Some(PlaybackWatchdogDeadline::HevcStartupStall) => {
            if let Some(status) = finish_if_hevc_startup_watchdog_deadline_elapsed(
                session_id,
                pipeline,
                demux_watermark,
                tick_started_at,
                timing,
                checkpoint,
                decoder_input_outcome,
            )? {
                return Ok(Some(status));
            }
        }
        None => {}
    }
    Ok(None)
}

fn finish_if_hevc_startup_watchdog_deadline_elapsed(
    session_id: PlaybackSessionId,
    pipeline: &mut PlaybackPipelineState,
    demux_watermark: DemuxReaderWatermark,
    tick_started_at: Instant,
    timing: PlaybackTickTiming,
    checkpoint: &'static str,
    decoder_input_outcome: Option<DecoderInputServiceOutcome>,
) -> std::result::Result<Option<PlaybackTickStatus>, String> {
    let Some(status) = service_hevc_startup_stall_watchdog_if_due(
        session_id,
        pipeline,
        demux_watermark,
        checkpoint,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(finish_playback_tick(
        session_id,
        tick_started_at,
        timing,
        status,
        "hevc_startup_watchdog_deadline",
        decoder_input_outcome,
    )))
}

fn finish_if_cached_seek_watchdog_deadline_elapsed(
    session_id: PlaybackSessionId,
    pipeline: &PlaybackPipelineState,
    tick_started_at: Instant,
    timing: PlaybackTickTiming,
    checkpoint: &'static str,
    decoder_input_outcome: Option<DecoderInputServiceOutcome>,
) -> Option<PlaybackTickStatus> {
    if !pipeline.cached_seek_recovery_watchdog_expired() {
        return None;
    }
    if let Some(watchdog) = pipeline.cached_seek_recovery_watchdog_snapshot() {
        let output_snapshot = pipeline.output_scheduler.snapshot();
        tracing::debug!(
            session_id = ?session_id,
            checkpoint,
            target_nsecs = watchdog.target_nsecs,
            elapsed_ms = watchdog.elapsed.as_secs_f64() * 1000.0,
            remaining_ms = watchdog.remaining.as_secs_f64() * 1000.0,
            video_packets_since_seek = watchdog.video_packets_since_seek,
            queued_video_frames = output_snapshot.queued_video_frames,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            "HEVC cached seek recovery watchdog deadline reached at playback coordinator checkpoint"
        );
    }
    Some(finish_playback_tick(
        session_id,
        tick_started_at,
        timing,
        PlaybackTickStatus::Continue,
        checkpoint,
        decoder_input_outcome,
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ffmpeg_sys_next as ffi;

    use super::{
        PlaybackTickStatus, PlaybackWatchdogDeadline, VideoDecodeWorkerSnapshot,
        VideoDecodeWorkerState, hevc_decode_chain_fallback_tick_status,
        hevc_startup_watchdog_reject_reason, playback_watchdog_deadline_due,
    };

    fn video_decode_snapshot(
        state: VideoDecodeWorkerState,
        in_flight_packets: usize,
        completed_packets: usize,
        queued_frames: usize,
    ) -> VideoDecodeWorkerSnapshot {
        VideoDecodeWorkerSnapshot {
            state,
            queued_frames,
            queue_capacity: 8,
            pending_input_packets: 0,
            pending_input_capacity: 8,
            in_flight_packets,
            command_queue_capacity: 4,
            completed_packets,
        }
    }

    #[test]
    fn hevc_zero_output_rebuffer_fallback_preempts_demux_wait() {
        assert_eq!(
            hevc_decode_chain_fallback_tick_status(true),
            Some(PlaybackTickStatus::ForceLowLevelSeek)
        );
        assert_eq!(hevc_decode_chain_fallback_tick_status(false), None);
    }

    #[test]
    fn playback_watchdog_checkpoint_fires_hevc_startup_deadline() {
        let now = Instant::now();

        assert_eq!(
            playback_watchdog_deadline_due(false, Some(now - Duration::from_millis(1)), now),
            Some(PlaybackWatchdogDeadline::HevcStartupStall)
        );
        assert_eq!(
            playback_watchdog_deadline_due(false, Some(now + Duration::from_millis(1)), now),
            None
        );
    }

    #[test]
    fn playback_watchdog_checkpoint_preserves_cached_seek_priority() {
        let now = Instant::now();

        assert_eq!(
            playback_watchdog_deadline_due(true, Some(now - Duration::from_millis(1)), now),
            Some(PlaybackWatchdogDeadline::CachedSeekRecovery)
        );
    }

    #[test]
    fn hevc_startup_watchdog_reject_reason_marks_decoder_in_flight_as_eligible() {
        assert_eq!(
            hevc_startup_watchdog_reject_reason(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                true,
                0,
                video_decode_snapshot(VideoDecodeWorkerState::Decoding, 4, 0, 0),
            ),
            "eligible"
        );
    }

    #[test]
    fn hevc_startup_watchdog_reject_reason_identifies_non_fallback_states() {
        assert_eq!(
            hevc_startup_watchdog_reject_reason(
                ffi::AVCodecID::AV_CODEC_ID_H264,
                true,
                0,
                video_decode_snapshot(VideoDecodeWorkerState::Decoding, 4, 0, 0),
            ),
            "non_hevc"
        );
        assert_eq!(
            hevc_startup_watchdog_reject_reason(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                false,
                0,
                video_decode_snapshot(VideoDecodeWorkerState::Decoding, 4, 0, 0),
            ),
            "software_decoder"
        );
        assert_eq!(
            hevc_startup_watchdog_reject_reason(
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                true,
                0,
                video_decode_snapshot(VideoDecodeWorkerState::Decoding, 4, 1, 0),
            ),
            "completed_packets_pending"
        );
    }
}
