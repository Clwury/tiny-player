use std::sync::{
    Arc,
    atomic::AtomicBool,
    mpsc::{Receiver, Sender},
};
use std::time::Instant;

#[cfg(test)]
use std::os::raw::c_int;

#[cfg(test)]
use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendEventKind},
    render_host::VideoOutputQueue,
};

use super::playback_pipeline_state::CachedSeekRecoveryFallbackAction;
use super::playback_reset_service::{
    PlaybackSeekBufferingPolicy, PlaybackSeekResetContext, service_playback_seek_reset,
};
use super::video_decode_pipeline::{
    HevcDecodeChainFallbackLoopAction, HevcDecodeChainFallbackReason,
};
use super::{
    AudioDecodePipeline, AudioOutput, BufferedReporter, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DemuxPacketCache, DemuxPacketCacheInput, DemuxReaderWatermark, DoviPipeline,
    END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS, FfmpegCommand, FfmpegControl,
    FfmpegPlaybackInput, OpenedPlaybackInput, PlaybackCommandContext, PlaybackCommandServiceStatus,
    PlaybackCoordinatorGateContext, PlaybackEofDrainContext, PlaybackEofDrainStatus,
    PlaybackGeneration, PlaybackOutputScheduler, PlaybackOutputSnapshot, PlaybackPipelineServices,
    PlaybackPipelineState, PlaybackScheduler, PlaybackSession, PlaybackTickContext,
    PlaybackTickStatus, PositionReporter, SubtitlePipeline, TimestampMapper,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VideoDecodePipeline, VideoDecodeRecovery,
    VideoFramePrepareWorker, audio_codec_requires_recovery_point, duration_nsecs, nsecs_to_seconds,
    open_playback_input_with_fallback, playback_audio_info_from_stream,
    playback_video_info_from_worker, preroll_seek_position_seconds,
    service_hevc_startup_stall_watchdog_if_due, service_playback_commands,
    service_playback_eof_drain, service_playback_tick, should_cache_http_url,
    video_seek_preroll_nsecs,
};

pub(in crate::player::backend::ffmpeg) fn run_ffmpeg_playback(
    mut source: FfmpegPlaybackInput,
    video_output_queue: VideoOutputQueue,
    event_tx: Sender<BackendEvent>,
    control: Arc<FfmpegControl>,
    command_rx: Receiver<FfmpegCommand>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let mut session = PlaybackSession::new(source.session_id, source.start_position_seconds);
    control.set_session_id(session.id());
    let OpenedPlaybackInput {
        mut input,
        stream_catalog,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder: opened_audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    let initial_playback_file_info = input.playback_file_info();
    let video_decode_pipeline = VideoDecodePipeline::spawn(video_decoder)?;
    let initial_playback_video_info =
        playback_video_info_from_worker(video_stream, video_decode_pipeline.info());
    let playback_generation = PlaybackGeneration::default();
    if let Some(device) = video_decode_pipeline.info().vulkan_device.clone() {
        video_output_queue.request_vulkan_prewarm(session.id(), device);
    }
    if source.start_position_seconds > 0.0 {
        let seek_position_seconds =
            preroll_seek_position_seconds(video_stream.codec_id, source.start_position_seconds);
        tracing::debug!(
            target_position_seconds = source.start_position_seconds,
            seek_position_seconds,
            preroll_nsecs = video_seek_preroll_nsecs(video_stream.codec_id),
            codec = ?video_stream.codec_id,
            "applying FFmpeg initial seek preroll"
        );
        input.seek_stream(video_stream, seek_position_seconds)?;
    }
    let duration_seconds = input.duration_seconds();
    let http_cache = input.cached_io_cache();
    if let Some(cache) = &http_cache {
        cache.set_duration_seconds(duration_seconds);
    }
    let input_cacheable = should_cache_http_url(&source.url);
    let demux_cache_config = source
        .cache_config
        .clone()
        .resolved_for_cacheable_input(input_cacheable);
    let should_wait_initial_demux_cache = demux_cache_config.demuxer_cache_wait;
    let demux_cache = DemuxPacketCache::spawn(
        DemuxPacketCacheInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds: source.start_position_seconds,
            session_id: session.id(),
            cache_config: demux_cache_config,
        },
        Arc::clone(&control),
        event_tx.clone(),
    )?;
    let video_frame_prepare_worker =
        VideoFramePrepareWorker::spawn(video_output_queue.buffer_pool())?;
    let current_start_position_nsecs = session.start_position_nsecs();
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let playback_timeline_origin_nsecs = video_stream.start_nsecs;
    let video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    let scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    let position_reporter = PositionReporter::default();
    let dovi_pipeline = DoviPipeline::default();
    let subtitle_pipeline = SubtitlePipeline::new(
        subtitle_stream,
        subtitle_decoder,
        &source,
        current_start_position_nsecs,
    )?;

    let mut audio_output = None;
    let mut audio_decode_pipeline = None;
    if let Some(decoder) = opened_audio_decoder {
        match AudioOutput::new(Arc::clone(&control)) {
            Ok(output) => {
                match AudioDecodePipeline::spawn(decoder, output.sample_rate(), output.channels()) {
                    Ok(worker) => {
                        let audio_info = worker.info();
                        tracing::debug!(
                            sample_rate = audio_info.output_rate,
                            channels = audio_info.output_channels,
                            "initialized native FFmpeg audio output and decode worker"
                        );
                        audio_output = Some(output);
                        audio_decode_pipeline = Some(worker);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "FFmpeg audio decode worker initialization failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
            }
        }
    }
    if should_wait_initial_demux_cache {
        tracing::debug!(
            session_id = ?session.id(),
            "waiting for initial FFmpeg demux cache fill before playback restart"
        );
        demux_cache.wait_until_initial_cache_fill()?;
    }
    let audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );
    if let Some(output) = &audio_output {
        output.reset_clock(current_start_position_nsecs);
    }
    let initial_playback_audio_info =
        playback_audio_info_from_stream(audio_stream, audio_output.as_ref());

    if let Some(duration) = duration_seconds {
        let _ = event_tx.send(BackendEvent::new(
            session.id(),
            BackendEventKind::DurationChanged(duration),
        ));
    }
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackFileInfoChanged(initial_playback_file_info),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(initial_playback_video_info),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackAudioInfoChanged(initial_playback_audio_info),
    ));
    let emit_playback_buffered_events = false;
    let buffered_reporter =
        BufferedReporter::new_with_events(audio_output.is_some(), emit_playback_buffered_events);
    let output_scheduler = PlaybackOutputScheduler::new();
    let mut video_decode_recovery = VideoDecodeRecovery::default();
    video_decode_recovery
        .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
    let mut pipeline_services = PlaybackPipelineServices::default();
    let mut pipeline = PlaybackPipelineState {
        video_stream,
        video_frame_duration_nsecs,
        video_decode_pipeline,
        audio_decode_pipeline,
        subtitle_pipeline,
        video_decode_recovery,
        playback_generation,
        audio_stream,
        decoded_video_frame_count: 0,
        dropped_video_frames_before_start_count: 0,
        dropped_audio_frames_before_start_count: 0,
        video_clock,
        playback_timeline_origin_nsecs,
        audio_clock,
        audio_output,
        scheduler,
        output_scheduler,
        dovi_pipeline,
        buffered_reporter,
        position_reporter,
        video_frame_prepare_worker,
        current_start_position_nsecs,
        video_packet_count: 0,
        video_decode_skip_nonref_active: false,
        cached_seek_recovery_watchdog: None,
        cached_seek_recovery_attempt: None,
        rebuffer_audio_realign_attempt: None,
    };
    pipeline.buffered_reporter.reset_to(
        source.start_position_seconds.max(0.0),
        session.id(),
        &event_tx,
    );
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::Buffering(true),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::SubtitleChanged(None),
    ));

    'playback_coordinator: loop {
        while !control.should_stop() {
            match service_playback_commands(PlaybackCommandContext {
                source: &mut source,
                session: &mut session,
                control: &control,
                command_rx: &command_rx,
                http_cache: http_cache.as_ref(),
                stream_catalog: &stream_catalog,
                demux_cache: &demux_cache,
                vo_queue: &video_output_queue,
                pipeline: &mut pipeline,
                emit_playback_buffered_events,
                event_tx: &event_tx,
            })? {
                PlaybackCommandServiceStatus::Idle => {}
                PlaybackCommandServiceStatus::Continue => continue,
                PlaybackCommandServiceStatus::Stopped => break,
            }

            if service_cached_seek_recovery_fallback_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
            )? {
                continue;
            }

            if service_hevc_startup_stall_watchdog_due_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
                "coordinator_gate_enter",
            )? {
                continue;
            }

            let playback_loop_deadline = pipeline.playback_loop_deadline();
            if pipeline_services
                .coordinator_gate
                .service(PlaybackCoordinatorGateContext {
                    control: &control,
                    output_scheduler: &pipeline.output_scheduler,
                    scheduler: &mut pipeline.scheduler,
                    playback_wait: &pipeline_services.wait,
                    playback_loop_deadline,
                })
                .should_continue()
            {
                if service_hevc_startup_stall_watchdog_due_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                    "coordinator_gate_continue",
                )? {
                    continue;
                }
                if service_cached_seek_recovery_fallback_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                )? {
                    continue;
                }
                continue;
            }

            let tick_status = service_playback_tick(PlaybackTickContext {
                session_id: session.id(),
                demux_cache: &demux_cache,
                http_cache: http_cache.as_ref(),
                services: &mut pipeline_services,
                pipeline: &mut pipeline,
                control: &control,
                event_tx: &event_tx,
                vo_queue: &video_output_queue,
                frame_presented: &frame_presented,
            })?;
            if matches!(tick_status, PlaybackTickStatus::ForceRebufferAudioRealign) {
                if service_rebuffer_audio_realign_seek_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                )? {
                    continue;
                }
                tracing::debug!(
                    session_id = ?session.id(),
                    "playback tick requested rebuffer audio realign without pending request"
                );
                continue;
            }
            if matches!(tick_status, PlaybackTickStatus::ForceLowLevelSeek) {
                if service_cached_seek_recovery_fallback_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                )? {
                    continue;
                }
                tracing::debug!(
                    session_id = ?session.id(),
                    "playback tick requested forced low-level seek without pending fallback"
                );
                continue;
            }
            if service_cached_seek_recovery_fallback_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
            )? {
                continue;
            }
            match tick_status {
                PlaybackTickStatus::Continue => continue,
                PlaybackTickStatus::ForceLowLevelSeek => continue,
                PlaybackTickStatus::ForceRebufferAudioRealign => continue,
                PlaybackTickStatus::Eof | PlaybackTickStatus::Stopped => break,
            }
        }

        if control.should_stop() {
            return Ok(());
        }
        match service_playback_eof_drain(PlaybackEofDrainContext {
            session_id: session.id(),
            duration_seconds,
            demux_cache: &demux_cache,
            services: &mut pipeline_services,
            pipeline: &mut pipeline,
            control: &control,
            event_tx: &event_tx,
            vo_queue: &video_output_queue,
            frame_presented: &frame_presented,
        })? {
            PlaybackEofDrainStatus::Complete | PlaybackEofDrainStatus::Stopped => return Ok(()),
            PlaybackEofDrainStatus::SeekPending => continue 'playback_coordinator,
        }
    }
}

fn rebuffer_audio_realign_requires_low_level_seek(
    attempts: u8,
    queued_video_covers_target: bool,
) -> bool {
    attempts > 1 && !queued_video_covers_target
}

fn rebuffer_audio_realign_can_preserve_video_queue(
    attempts: u8,
    queued_video_covers_target: bool,
    audio_stream_available: bool,
) -> bool {
    audio_stream_available
        && (!rebuffer_audio_realign_requires_low_level_seek(attempts, queued_video_covers_target)
            || queued_video_covers_target)
}

fn internal_recovery_seek_buffering_policy(
    output_snapshot: PlaybackOutputSnapshot,
) -> PlaybackSeekBufferingPolicy {
    let can_preserve_visible_frame = !output_snapshot.first_video_frame_pending
        && !output_snapshot.rebuffering
        && !output_snapshot.video_output_low_water
        && !output_snapshot.video_decode_underfill
        && output_snapshot.queued_video_frames > 0;
    if can_preserve_visible_frame {
        PlaybackSeekBufferingPolicy::PreserveVisibleFrame
    } else {
        PlaybackSeekBufferingPolicy::Emit
    }
}

fn service_rebuffer_audio_realign_seek_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    let Some(request) = pipeline
        .output_scheduler
        .take_rebuffer_audio_realign_request()
    else {
        return Ok(false);
    };
    let attempts = pipeline.observe_rebuffer_audio_realign_attempt(request.target_timeline_nsecs);
    let position_seconds = nsecs_to_seconds(request.target_timeline_nsecs);
    let audio_stream_index = pipeline.audio_stream.map(|stream| stream.index);
    let output_snapshot = pipeline.output_scheduler.snapshot();
    let audio_output_snapshot = pipeline
        .audio_output
        .as_ref()
        .and_then(|output| output.snapshot().ok());
    let queued_video_covers_target =
        output_snapshot
            .queued_video_range_nsecs
            .is_some_and(|(start, end)| {
                start <= request.target_timeline_nsecs && request.target_timeline_nsecs < end
            });
    let mut force_low_level_seek =
        rebuffer_audio_realign_requires_low_level_seek(attempts, queued_video_covers_target);
    let can_preserve_video_queue = rebuffer_audio_realign_can_preserve_video_queue(
        attempts,
        queued_video_covers_target,
        audio_stream_index.is_some(),
    );
    let first_video_after_anchor_gap_ms = (i128::from(request.first_video_timeline_nsecs)
        - i128::from(request.anchor_timeline_nsecs))
        as f64
        / 1_000_000.0;
    let far_ahead_audio_delta_ms = (i128::from(request.far_ahead_audio_timeline_nsecs)
        - i128::from(request.target_timeline_nsecs)) as f64
        / 1_000_000.0;
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        anchor_timeline_nsecs = request.anchor_timeline_nsecs,
        first_video_timeline_nsecs = request.first_video_timeline_nsecs,
        first_video_after_anchor_gap_ms,
        far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
        far_ahead_audio_delta_ms,
        far_ahead_drop_count = request.far_ahead_drop_count,
        attempts,
        force_low_level_seek,
        can_preserve_video_queue,
        audio_stream_index = ?audio_stream_index,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_covers_target,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_contiguous_forward_ms = ?output_snapshot
            .queued_video_contiguous_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_largest_gap_ms = ?output_snapshot
            .queued_video_largest_gap_nsecs
            .map(|gap| gap as f64 / 1_000_000.0),
        output_state = ?output_snapshot.state,
        output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        output_rebuffer_anchor = ?output_snapshot.video_output_rebuffer_anchor,
        audio_output_pending_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        audio_output_queue_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
        pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
        "evaluating FFmpeg rebuffer audio realign recovery path"
    );

    if can_preserve_video_queue && let Some(audio_stream_index) = audio_stream_index {
        let audio_realign_requires_recovery_point = pipeline
            .audio_stream
            .is_some_and(|stream| audio_codec_requires_recovery_point(stream.codec_id));
        let reader_realign = demux_cache.realign_stream_reader_to_timeline(
            audio_stream_index,
            request.target_timeline_nsecs,
            request.reason,
        );
        if reader_realign.is_none()
            && (!queued_video_covers_target || audio_realign_requires_recovery_point)
        {
            force_low_level_seek |= audio_realign_requires_recovery_point;
            tracing::debug!(
                session_id = ?session.id(),
                target_timeline_nsecs = request.target_timeline_nsecs,
                attempts,
                queued_video_covers_target,
                audio_stream_index,
                audio_realign_requires_recovery_point,
                force_low_level_seek,
                "FFmpeg rebuffer audio realign reader reposition unavailable"
            );
        } else {
            let generation = pipeline.advance_playback_generation();
            if let Some(audio_decode_pipeline) = pipeline.audio_decode_pipeline.as_mut() {
                audio_decode_pipeline.flush_buffers(generation)?;
            }
            pipeline.audio_clock = TimestampMapper::new(
                pipeline.audio_stream.and_then(|stream| stream.start_nsecs),
                request.target_timeline_nsecs,
                None,
            );
            if let Some(audio_output) = pipeline.audio_output.as_ref() {
                audio_output.reset_clock(request.target_timeline_nsecs);
            }
            pipeline
                .output_scheduler
                .reset_audio_after_rebuffer_realign(
                    request.target_timeline_nsecs,
                    session.id(),
                    request.reason,
                );
            pipeline.clear_rebuffer_audio_realign_attempt();
            control.set_cache_paused(false);
            tracing::debug!(
                session_id = ?session.id(),
                target_timeline_nsecs = request.target_timeline_nsecs,
                reason = request.reason,
                anchor_timeline_nsecs = request.anchor_timeline_nsecs,
                first_video_timeline_nsecs = request.first_video_timeline_nsecs,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
                far_ahead_audio_delta_ms,
                far_ahead_drop_count = request.far_ahead_drop_count,
                attempts,
                queued_video_frames = output_snapshot.queued_video_frames,
                queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
                queued_video_range = ?output_snapshot.queued_video_range_nsecs,
                queued_video_covers_target,
                audio_stream_index,
                reader_realign = ?reader_realign,
                playback_generation = generation,
                "handled FFmpeg rebuffer audio realign while preserving video queue"
            );
            return Ok(true);
        }
    }

    control.set_cache_paused(false);
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        anchor_timeline_nsecs = request.anchor_timeline_nsecs,
        first_video_timeline_nsecs = request.first_video_timeline_nsecs,
        first_video_after_anchor_gap_ms,
        far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
        far_ahead_audio_delta_ms,
        far_ahead_drop_count = request.far_ahead_drop_count,
        attempts,
        force_low_level_seek,
        can_preserve_video_queue,
        seek_generation,
        audio_stream_index = ?audio_stream_index,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_covers_target,
        audio_output_pending_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        "handling FFmpeg rebuffer audio realign with playback seek reset"
    );
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek,
        low_level_seek_reason: force_low_level_seek.then_some(request.reason),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: internal_recovery_seek_buffering_policy(output_snapshot),
        control,
        event_tx,
    })?;
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        attempts,
        force_low_level_seek,
        seek_generation,
        ?demux_seek_result,
        "handled FFmpeg rebuffer audio realign with playback seek reset"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn service_hevc_startup_stall_watchdog_due_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    checkpoint: &'static str,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    let Some(tick_status) = service_hevc_startup_stall_watchdog_if_due(
        session.id(),
        pipeline,
        demux_cache.cached_reader_watermark(),
        checkpoint,
    )?
    else {
        return Ok(false);
    };
    if matches!(tick_status, PlaybackTickStatus::ForceLowLevelSeek) {
        if service_cached_seek_recovery_fallback_if_needed(
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
        )? {
            return Ok(true);
        }
        tracing::debug!(
            session_id = ?session.id(),
            checkpoint,
            "HEVC startup watchdog requested forced low-level seek without pending fallback"
        );
    }
    Ok(true)
}

fn service_cached_seek_recovery_fallback_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    if let Some(fallback) = pipeline.take_cached_seek_recovery_fallback(session.id()) {
        let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
        control.set_cache_paused(false);
        match fallback.action {
            CachedSeekRecoveryFallbackAction::RecoveryExhausted => {
                return Err(format!(
                    "HEVC cached seek recovery exhausted at {:.3}s after soft recovery, software fallback and low-level seek",
                    position_seconds
                ));
            }
            CachedSeekRecoveryFallbackAction::SoftRecover => {
                let requeued_probe_packets =
                    pipeline.soft_recover_cached_seek_hevc_decode_chain(session.id())?;
                pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
                tracing::debug!(
                    session_id = ?session.id(),
                    position_seconds,
                    target_nsecs = fallback.target_nsecs,
                    reason = fallback.reason.as_str(),
                    requeued_probe_packets,
                    "handled HEVC cached seek recovery fallback with soft decode recovery"
                );
                return Ok(true);
            }
            CachedSeekRecoveryFallbackAction::ReopenSoftware => {
                let reopened_software = pipeline
                    .video_decode_pipeline
                    .reopen_software_decoder(pipeline.video_stream)?;
                if reopened_software {
                    let playback_video_info = playback_video_info_from_worker(
                        pipeline.video_stream,
                        pipeline.video_decode_pipeline.info(),
                    );
                    let _ = event_tx.send(BackendEvent::new(
                        session.id(),
                        BackendEventKind::PlaybackInfoChanged(playback_video_info),
                    ));
                    let requeued_probe_packets = pipeline
                        .video_decode_pipeline
                        .requeue_hevc_startup_probe_packets(
                            &mut pipeline.playback_generation,
                            session.id(),
                        )?;
                    if requeued_probe_packets > 0 {
                        session.reset_to(session.id(), position_seconds);
                        pipeline.current_start_position_nsecs = session.start_position_nsecs();
                        pipeline.output_scheduler.reset(control);
                        pipeline.video_decode_recovery.reset();
                        pipeline.dovi_pipeline.reset();
                        pipeline.begin_cached_seek_recovery_watchdog(
                            fallback.target_nsecs,
                            session.id(),
                        );
                        tracing::debug!(
                            session_id = ?session.id(),
                            position_seconds,
                            target_nsecs = fallback.target_nsecs,
                            reason = fallback.reason.as_str(),
                            requeued_probe_packets,
                            "handled HEVC cached seek recovery fallback by requeuing probe packets on software decoder"
                        );
                        return Ok(true);
                    }
                    session.reset_to(session.id(), position_seconds);
                    pipeline.current_start_position_nsecs = session.start_position_nsecs();
                    pipeline.output_scheduler.reset(control);
                    pipeline.video_decode_recovery.reset_for_timeline_start(
                        pipeline.video_stream.codec_id,
                        pipeline.current_start_position_nsecs,
                    );
                    pipeline.dovi_pipeline.reset();
                    pipeline
                        .begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
                    tracing::warn!(
                        session_id = ?session.id(),
                        position_seconds,
                        target_nsecs = fallback.target_nsecs,
                        reason = fallback.reason.as_str(),
                        requeued_probe_packets,
                        "HEVC cached seek software reopen had no probe packets; continuing without low-level seek"
                    );
                    return Ok(true);
                }
            }
            CachedSeekRecoveryFallbackAction::LowLevelSeek => {}
        }
        let demux_watermark = demux_cache.cached_reader_watermark();
        let low_level_seek_required = matches!(
            fallback.action,
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        if !low_level_seek_required
            && !demux_reader_unusable_for_hevc_low_level_seek(demux_watermark)
        {
            pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
            tracing::debug!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                reason = fallback.reason.as_str(),
                action = fallback.action.as_str(),
                hevc_boundary_reset_required = true,
                reset_path = "forced_low_level",
                demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
                demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
                demux_underrun = demux_watermark.underrun,
                demux_video_underrun = demux_watermark.video_underrun,
                "deferring HEVC cached seek recovery low-level seek while demux reader is still usable"
            );
            return Ok(true);
        }
        let seek_generation = control.request_seek();
        session.reset_to(session.id(), position_seconds);
        pipeline.current_start_position_nsecs = session.start_position_nsecs();
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            action = fallback.action.as_str(),
            seek_generation,
            hevc_boundary_reset_required = true,
            reset_path = "forced_low_level",
            demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
            demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
            "handling HEVC cached seek recovery fallback with low-level seek"
        );
        let buffering_policy =
            internal_recovery_seek_buffering_policy(pipeline.output_scheduler.snapshot());
        let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
            seek_generation,
            force_low_level_seek: true,
            low_level_seek_reason: Some(fallback.reason.as_str()),
            session_id: session.id(),
            vo_queue,
            demux_cache,
            pipeline,
            emit_playback_buffered_events,
            buffering_policy,
            control,
            event_tx,
        })?;
        pipeline
            .video_decode_pipeline
            .remember_hevc_recovery_low_level_seek_target(fallback.target_nsecs);
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            action = fallback.action.as_str(),
            seek_generation,
            hevc_boundary_reset_required = true,
            reset_path = "forced_low_level",
            ?demux_seek_result,
            "handled HEVC cached seek recovery fallback with low-level seek"
        );
        return Ok(true);
    }
    let Some(fallback) = pipeline
        .video_decode_pipeline
        .take_hevc_decode_chain_fallback()
    else {
        return Ok(false);
    };
    let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
    control.set_cache_paused(false);

    if fallback.reason.invalidated_by_video_progress()
        && pipeline
            .video_decode_pipeline
            .hevc_recent_video_progress_grace_active(Instant::now())
    {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "discarded stale HEVC decode chain fallback after recent decoded video progress"
        );
        return Ok(true);
    }

    if pipeline.video_decode_pipeline.info().hardware_accelerated
        && fallback.reason.requires_repeat_before_hardware_downgrade()
        && !pipeline
            .video_decode_pipeline
            .has_prior_matching_hevc_decode_chain_fallback(fallback)
    {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "deferred HEVC hardware decoder downgrade until recovery failure repeats"
        );
        return Ok(true);
    }

    let loop_action = pipeline
        .video_decode_pipeline
        .hevc_decode_chain_fallback_loop_action(fallback);
    if loop_action == HevcDecodeChainFallbackLoopAction::RecoveryExhausted {
        return Err(format!(
            "HEVC 解码链恢复失败：目标 {:.3}s 在 cached、软件解码和低层 seek 后仍无视频输出（{}）",
            position_seconds,
            fallback.reason.as_str(),
        ));
    }
    if loop_action == HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_software_suppression(fallback);
        pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        tracing::warn!(
            session_id = ?session.id(),
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "suppressing repeated HEVC decode chain fallback low-level seek on software decoder"
        );
        return Ok(true);
    }

    let should_reopen_software = pipeline.video_decode_pipeline.info().hardware_accelerated
        && (hevc_decode_chain_fallback_reopens_software(fallback.reason)
            || loop_action == HevcDecodeChainFallbackLoopAction::ForceSoftware);
    let mut requeued_probe_packets = 0usize;
    if should_reopen_software {
        let reopened_software = pipeline
            .video_decode_pipeline
            .reopen_software_decoder(pipeline.video_stream)?;
        if reopened_software {
            let playback_video_info = playback_video_info_from_worker(
                pipeline.video_stream,
                pipeline.video_decode_pipeline.info(),
            );
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::PlaybackInfoChanged(playback_video_info),
            ));
            tracing::debug!(
                session_id = ?session.id(),
                reason = fallback.reason.as_str(),
                "reopened FFmpeg HEVC decoder in software mode after repeated startup recovery failure"
            );
            requeued_probe_packets = pipeline
                .video_decode_pipeline
                .requeue_hevc_startup_probe_packets(
                    &mut pipeline.playback_generation,
                    session.id(),
                )?;
            if requeued_probe_packets > 0 {
                session.reset_to(session.id(), position_seconds);
                pipeline.current_start_position_nsecs = session.start_position_nsecs();
                pipeline.output_scheduler.reset(control);
                pipeline.video_decode_recovery.reset();
                pipeline.dovi_pipeline.reset();
                pipeline
                    .video_decode_pipeline
                    .remember_hevc_decode_chain_fallback(fallback);
                tracing::debug!(
                    session_id = ?session.id(),
                    position_seconds,
                    target_nsecs = fallback.target_nsecs,
                    reason = fallback.reason.as_str(),
                    requeued_probe_packets,
                    "handled HEVC decode chain fallback by requeuing startup probe packets on software decoder"
                );
                return Ok(true);
            }
            session.reset_to(session.id(), position_seconds);
            pipeline.current_start_position_nsecs = session.start_position_nsecs();
            pipeline.output_scheduler.reset(control);
            pipeline.video_decode_recovery.reset_for_timeline_start(
                pipeline.video_stream.codec_id,
                pipeline.current_start_position_nsecs,
            );
            pipeline.dovi_pipeline.reset();
            pipeline
                .video_decode_pipeline
                .remember_hevc_decode_chain_fallback(fallback);
            tracing::warn!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                reason = fallback.reason.as_str(),
                requeued_probe_packets,
                "HEVC startup probe packets unavailable after software decoder reopen; continuing without low-level seek"
            );
            return Ok(true);
        }
    }
    let demux_watermark = demux_cache.cached_reader_watermark();
    let output_snapshot = pipeline.output_scheduler.snapshot();
    let startup_or_post_seek =
        output_snapshot.first_video_frame_pending || output_snapshot.video_bootstrap_after_seek;
    if hevc_decode_chain_fallback_should_suppress_low_level_seek(
        fallback.reason,
        fallback.target_nsecs,
        requeued_probe_packets,
        demux_watermark,
        startup_or_post_seek,
    ) {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        tracing::warn!(
            session_id = ?session.id(),
            reason = fallback.reason.as_str(),
            target_ms = fallback.target_nsecs as f64 / 1_000_000.0,
            probe_packets = requeued_probe_packets,
            demux_forward_ms = ?demux_watermark
                .video_forward_nsecs
                .or(demux_watermark.selected_min_forward_nsecs)
                .map(|duration| duration as f64 / 1_000_000.0),
            startup_or_post_seek,
            queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
            "hevc_low_level_seek_suppressed"
        );
        return Ok(true);
    }
    let boundary_reset_required =
        hevc_decode_chain_fallback_requires_boundary_reset(fallback.reason);
    let force_low_level_from_loop =
        loop_action == HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek;
    if !force_low_level_from_loop
        && !boundary_reset_required
        && !demux_reader_unusable_for_hevc_low_level_seek(demux_watermark)
    {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            hevc_boundary_reset_required = boundary_reset_required,
            reset_path = "forced_low_level",
            demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
            demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
            demux_underrun = demux_watermark.underrun,
            demux_video_underrun = demux_watermark.video_underrun,
            "deferring HEVC decode chain low-level seek while demux reader is still usable"
        );
        return Ok(true);
    }
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    let force_low_level_seek = force_low_level_from_loop || !boundary_reset_required;
    let reset_path = if force_low_level_seek {
        "forced_low_level"
    } else if boundary_reset_required {
        "cached_then_low_level"
    } else {
        "forced_low_level"
    };
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_nsecs = fallback.target_nsecs,
        reason = fallback.reason.as_str(),
        seek_generation,
        hevc_boundary_reset_required = boundary_reset_required,
        reset_path,
        demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
        demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
        demux_underrun = demux_watermark.underrun,
        demux_video_underrun = demux_watermark.video_underrun,
        "handling HEVC decode chain recovery fallback with boundary reset"
    );
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek,
        low_level_seek_reason: Some(fallback.reason.as_str()),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: internal_recovery_seek_buffering_policy(output_snapshot),
        control,
        event_tx,
    })?;
    if force_low_level_seek {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_low_level_seek(fallback);
    } else {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
    }
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_nsecs = fallback.target_nsecs,
        reason = fallback.reason.as_str(),
        seek_generation,
        hevc_boundary_reset_required = boundary_reset_required,
        reset_path,
        ?demux_seek_result,
        "handled HEVC decode chain recovery fallback with boundary reset"
    );
    Ok(true)
}

fn hevc_decode_chain_fallback_reopens_software(reason: HevcDecodeChainFallbackReason) -> bool {
    matches!(
        reason,
        HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            | HevcDecodeChainFallbackReason::StartupInFlightStall
            | HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
            | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
            | HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill
    )
}

fn hevc_decode_chain_fallback_requires_boundary_reset(
    reason: HevcDecodeChainFallbackReason,
) -> bool {
    reason.requires_boundary_reset()
}

fn demux_reader_unusable_for_hevc_low_level_seek(watermark: DemuxReaderWatermark) -> bool {
    let video_forward_empty = watermark.video_forward_nsecs.unwrap_or_default() == 0;
    let selected_forward_empty = watermark.selected_min_forward_nsecs.unwrap_or_default() == 0;
    watermark.video_underrun && video_forward_empty && selected_forward_empty
}

fn demux_reader_healthy_for_hevc_low_level_seek_suppression(
    watermark: DemuxReaderWatermark,
) -> bool {
    let video_forward_nsecs = watermark
        .video_forward_nsecs
        .or(watermark.selected_min_forward_nsecs)
        .unwrap_or_default();
    !watermark.video_underrun
        && video_forward_nsecs >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
}

fn hevc_decode_chain_fallback_should_suppress_low_level_seek(
    reason: HevcDecodeChainFallbackReason,
    target_nsecs: u64,
    probe_packets: usize,
    demux_watermark: DemuxReaderWatermark,
    startup_or_post_seek: bool,
) -> bool {
    matches!(
        reason,
        HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
    ) && target_nsecs == 0
        && probe_packets == 0
        && startup_or_post_seek
        && demux_reader_healthy_for_hevc_low_level_seek_suppression(demux_watermark)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn playback_read_finished(
    read_result: c_int,
    duration_seconds: Option<f64>,
    buffered_until_seconds: Option<f64>,
) -> bool {
    read_result == ffi::AVERROR_EOF
        || (read_result == ffi::AVERROR(ffi::EIO)
            && playback_buffered_near_duration(duration_seconds, buffered_until_seconds))
}

pub(super) fn playback_buffered_near_duration(
    duration_seconds: Option<f64>,
    buffered_until_seconds: Option<f64>,
) -> bool {
    let Some(duration_seconds) = duration_seconds.filter(|duration| duration.is_finite()) else {
        return false;
    };
    let Some(buffered_until_seconds) =
        buffered_until_seconds.filter(|buffered_until| buffered_until.is_finite())
    else {
        return false;
    };

    duration_seconds > 0.0
        && buffered_until_seconds + END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS >= duration_seconds
}

#[cfg(test)]
mod tests {
    use super::{
        DemuxReaderWatermark, HevcDecodeChainFallbackReason, PlaybackOutputSnapshot,
        PlaybackSeekBufferingPolicy, demux_reader_unusable_for_hevc_low_level_seek,
        hevc_decode_chain_fallback_reopens_software,
        hevc_decode_chain_fallback_requires_boundary_reset,
        hevc_decode_chain_fallback_should_suppress_low_level_seek,
        internal_recovery_seek_buffering_policy, rebuffer_audio_realign_can_preserve_video_queue,
        rebuffer_audio_realign_requires_low_level_seek,
    };
    use crate::player::backend::ffmpeg::playback_loop::PlaybackOutputState;

    fn output_snapshot(
        state: PlaybackOutputState,
        queued_video_frames: usize,
        rebuffering: bool,
        video_output_low_water: bool,
        video_decode_underfill: bool,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state,
            first_video_frame_pending: state.first_video_frame_pending(),
            rebuffering,
            queued_video_frames,
            queued_video_duration_nsecs: 800_000_000,
            queued_video_range_nsecs: Some((1_000_000_000, 1_800_000_000)),
            queued_video_forward_nsecs: Some(800_000_000),
            queued_video_contiguous_forward_nsecs: Some(800_000_000),
            queued_video_largest_gap_nsecs: None,
            video_output_low_water,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill,
            rebuffer_empty_audio_output_blocked: false,
        }
    }

    #[test]
    fn hevc_startup_zero_output_hard_fallbacks_to_software() {
        assert!(hevc_decode_chain_fallback_reopens_software(
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
        ));
    }

    #[test]
    fn hevc_startup_in_flight_stall_fallbacks_to_software() {
        assert!(hevc_decode_chain_fallback_reopens_software(
            HevcDecodeChainFallbackReason::StartupInFlightStall
        ));
    }

    #[test]
    fn hevc_recovery_wait_rebuffer_reopens_software_decoder_when_hardware() {
        assert!(hevc_decode_chain_fallback_reopens_software(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
        ));
    }

    #[test]
    fn hevc_pts_gap_fallback_reopens_software_before_seek() {
        assert!(hevc_decode_chain_fallback_reopens_software(
            HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
        ));
    }

    #[test]
    fn internal_recovery_suppresses_buffering_while_visible_output_is_healthy() {
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Playing,
                48,
                false,
                false,
                false,
            )),
            PlaybackSeekBufferingPolicy::PreserveVisibleFrame
        );
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Playing,
                3,
                false,
                true,
                false,
            )),
            PlaybackSeekBufferingPolicy::Emit
        );
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Rebuffering,
                0,
                true,
                true,
                true,
            )),
            PlaybackSeekBufferingPolicy::Emit
        );
    }

    #[test]
    fn repeated_rebuffer_audio_realign_preserves_covering_video_queue() {
        assert!(!rebuffer_audio_realign_requires_low_level_seek(2, true));
        assert!(rebuffer_audio_realign_requires_low_level_seek(2, false));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(1, false));
    }

    #[test]
    fn rebuffer_audio_realign_service_keeps_covering_video_queue_on_audio_only_realign() {
        assert!(rebuffer_audio_realign_can_preserve_video_queue(
            2, true, true
        ));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(2, true));
        assert!(!rebuffer_audio_realign_can_preserve_video_queue(
            2, false, true
        ));
        assert!(!rebuffer_audio_realign_can_preserve_video_queue(
            2, true, false
        ));
    }

    #[test]
    fn hevc_decode_chain_hard_fallbacks_require_boundary_reset() {
        for reason in [
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            HevcDecodeChainFallbackReason::StartupInFlightStall,
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill,
            HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        ] {
            assert!(hevc_decode_chain_fallback_requires_boundary_reset(reason));
        }
    }

    #[test]
    fn hevc_decode_chain_boundary_reset_bypasses_forward_cache_deferral() {
        let demux_watermark = DemuxReaderWatermark {
            video_forward_nsecs: Some(1_000_000_000),
            selected_min_forward_nsecs: Some(1_000_000_000),
            video_underrun: false,
            underrun: false,
            ..DemuxReaderWatermark::default()
        };

        assert!(hevc_decode_chain_fallback_requires_boundary_reset(
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
        ));
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            demux_watermark
        ));
    }

    #[test]
    fn hevc_low_level_seek_waits_while_demux_reader_has_video_forward_cache() {
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(1_000_000_000),
                selected_min_forward_nsecs: Some(1_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_low_level_seek_requires_video_reader_underrun() {
        assert!(demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(0),
                selected_min_forward_nsecs: Some(0),
                video_underrun: true,
                underrun: true,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_low_level_seek_ignores_audio_only_underrun_with_video_forward_cache() {
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                audio_forward_nsecs: Some(0),
                selected_min_forward_nsecs: Some(0),
                audio_underrun: true,
                underrun: true,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_recovery_wait_zero_target_suppresses_low_level_seek_when_demux_is_healthy() {
        assert!(hevc_decode_chain_fallback_should_suppress_low_level_seek(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            0,
            0,
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                selected_min_forward_nsecs: Some(2_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            },
            true,
        ));
    }

    #[test]
    fn hevc_recovery_wait_zero_target_does_not_suppress_after_probe_requeue() {
        assert!(!hevc_decode_chain_fallback_should_suppress_low_level_seek(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            0,
            1,
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                selected_min_forward_nsecs: Some(2_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            },
            true,
        ));
    }
}
