use super::*;

pub(super) fn rebuffer_audio_realign_requires_low_level_seek(
    _attempts: u8,
    _queued_video_covers_target: bool,
) -> bool {
    false
}

pub(super) fn rebuffer_audio_realign_can_preserve_video_queue(
    attempts: u8,
    queued_video_covers_target: bool,
    audio_stream_available: bool,
) -> bool {
    audio_stream_available && attempts == 1 && queued_video_covers_target
}

pub(super) fn audio_realign_execution_decision(
    coverage: AudioRealignCoverage,
    in_flight_packets: usize,
) -> AudioRealignExecutionDecision {
    if coverage.ready {
        return AudioRealignExecutionDecision::CoverageSatisfied;
    }
    if in_flight_packets > 0 {
        return AudioRealignExecutionDecision::InputPending;
    }
    AudioRealignExecutionDecision::Execute
}

pub(super) fn internal_recovery_seek_buffering_policy(
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

pub(super) fn service_rebuffer_audio_realign_seek_if_needed(
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
    let audio_output_snapshot = pipeline
        .audio_output
        .as_ref()
        .and_then(|output| output.snapshot().ok());
    let coverage = pipeline.output_scheduler.audio_realign_coverage(
        request.target_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        audio_output_snapshot,
    );
    let audio_decode_snapshot = pipeline
        .audio_decode_pipeline
        .as_ref()
        .map(AudioDecodePipeline::snapshot);
    let retained_far_ahead_frame = pipeline
        .audio_decode_pipeline
        .as_ref()
        .is_some_and(AudioDecodePipeline::has_deferred_output_frame);
    let execution_decision = audio_realign_execution_decision(
        coverage,
        if retained_far_ahead_frame
            || pipeline
                .output_scheduler
                .pending_resume_audio_limit_reached()
        {
            0
        } else {
            audio_decode_snapshot
                .map(|snapshot| snapshot.in_flight_packets)
                .unwrap_or_default()
        },
    );
    if execution_decision != AudioRealignExecutionDecision::Execute {
        if execution_decision == AudioRealignExecutionDecision::InputPending {
            pipeline
                .output_scheduler
                .defer_audio_reader_gap_watchdog_after_input_pending(request.target_timeline_nsecs);
        }
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = ?pipeline
                .audio_realign_transaction
                .map(|transaction| transaction.transaction_id),
            recovery_scope = "audio_realign",
            target_timeline_nsecs = request.target_timeline_nsecs,
            reason = request.reason,
            arbitration_outcome = execution_decision.as_str(),
            audio_accepted_start = ?coverage.audio_accepted_start_timeline_nsecs,
            start_gap_ms = ?coverage
                .start_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            contiguous_coverage_ms = ?coverage
                .contiguous_coverage_nsecs
                .map(|coverage| coverage as f64 / 1_000_000.0),
            audio_output_pending_ms = ?audio_output_snapshot
                .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
            coverage_target_ms = coverage.protected_target_nsecs as f64 / 1_000_000.0,
            audio_decode_pending_input_packets = ?audio_decode_snapshot
                .map(|snapshot| snapshot.pending_input_packets),
            audio_decode_in_flight_packets = ?audio_decode_snapshot
                .map(|snapshot| snapshot.in_flight_packets),
            retained_far_ahead_frame,
            "discarded queued FFmpeg audio realign after live-state recheck"
        );
        return Ok(true);
    }
    match pipeline.observe_rebuffer_audio_realign_request(request) {
        AudioRealignRequestAction::Start => service_rebuffer_audio_realign_request(
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
            request,
            1,
            false,
        ),
        AudioRealignRequestAction::Coalesce {
            transaction,
            reason,
        } => {
            let worker = pipeline
                .audio_decode_pipeline
                .as_ref()
                .map(AudioDecodePipeline::snapshot);
            tracing::debug!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = request.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                recovery_satisfied = transaction.phase
                    == super::super::playback_pipeline_state::AudioRealignPhase::Covered,
                fallback_eligible = false,
                coalesce_reason = reason.as_str(),
                recovery_generation = ?worker.and_then(|snapshot| snapshot.recovery_generation),
                recovery_elapsed_ms = ?worker
                    .and_then(|snapshot| snapshot.recovery_elapsed)
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = ?worker.map(|snapshot| snapshot.flush_command_sent),
                in_flight_packets = ?worker.map(|snapshot| snapshot.in_flight_packets),
                stale_results_discarded = ?worker
                    .map(|snapshot| snapshot.stale_results_discarded),
                last_result_progress_ms = ?worker
                    .and_then(|snapshot| snapshot.last_result_progress_elapsed)
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                "coalesced repeated FFmpeg rebuffer audio realign request"
            );
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_rebuffer_audio_realign_request(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    request: RebufferAudioRealignRequest,
    attempts: u8,
    force_low_level_fallback: bool,
) -> std::result::Result<bool, String> {
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
    let mut force_low_level_seek = force_low_level_fallback
        || rebuffer_audio_realign_requires_low_level_seek(attempts, queued_video_covers_target);
    let can_preserve_video_queue = !force_low_level_seek
        && rebuffer_audio_realign_can_preserve_video_queue(
            attempts,
            queued_video_covers_target,
            audio_stream_index.is_some() && pipeline.audio_decode_pipeline.is_some(),
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
        far_ahead_observation_count = request.far_ahead_observation_count,
        attempts,
        force_low_level_seek,
        force_low_level_fallback,
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

    pipeline.retain_audio_for_realign(session.id(), request.reason);

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
            let recovery_started_at = Instant::now();
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
                .prepare_audio_after_rebuffer_realign(
                    request.target_timeline_nsecs,
                    session.id(),
                    request.reason,
                );
            let transaction_id = pipeline.begin_recovery_transaction();
            pipeline.begin_audio_realign_transaction(
                transaction_id,
                request,
                generation,
                recovery_started_at,
            );
            control.set_cache_paused(false);
            tracing::debug!(
                session_id = ?session.id(),
                transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = request.target_timeline_nsecs,
                reason = request.reason,
                anchor_timeline_nsecs = request.anchor_timeline_nsecs,
                first_video_timeline_nsecs = request.first_video_timeline_nsecs,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = request.far_ahead_observation_count,
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
    let start_audio_realign_transaction = pipeline.audio_realign_transaction.is_none();
    let audio_recovery_transaction_id = pipeline
        .audio_realign_transaction
        .map(|transaction| transaction.transaction_id);
    let recovery_started_at = Instant::now();
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id = ?audio_recovery_transaction_id,
        recovery_scope = "audio_realign",
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        anchor_timeline_nsecs = request.anchor_timeline_nsecs,
        first_video_timeline_nsecs = request.first_video_timeline_nsecs,
        first_video_after_anchor_gap_ms,
        far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
        far_ahead_audio_delta_ms,
        far_ahead_observation_count = request.far_ahead_observation_count,
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
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: audio_recovery_transaction_id,
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
    let recovery_generation = pipeline.playback_generation.current();
    if start_audio_realign_transaction {
        let transaction_id = pipeline.active_recovery_transaction_id();
        pipeline.begin_audio_realign_transaction(
            transaction_id,
            request,
            recovery_generation,
            recovery_started_at,
        );
    } else {
        pipeline.update_audio_realign_recovery_generation(recovery_generation);
    }
    let transaction_id = pipeline
        .audio_realign_transaction
        .map(|transaction| transaction.transaction_id);
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id = ?transaction_id,
        recovery_scope = "audio_realign",
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        attempts,
        force_low_level_seek,
        seek_generation,
        recovery_generation,
        ?demux_seek_result,
        "handled FFmpeg rebuffer audio realign with playback seek reset"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_audio_realign_recovery_watchdog_if_needed(
    context: &mut PlaybackRecoveryContext<'_>,
) -> std::result::Result<bool, String> {
    let session = &mut *context.session;
    let control = context.control;
    let demux_cache = context.demux_cache;
    let pipeline = &mut *context.pipeline;
    let vo_queue = context.vo_queue;
    let event_tx = context.event_tx;
    let emit_playback_buffered_events = context.emit_playback_buffered_events;
    if let Some(audio_decode_pipeline) = pipeline.audio_decode_pipeline.as_mut() {
        audio_decode_pipeline.service_worker()?;
    }
    if let Some(transaction) = pipeline.clear_audio_realign_transaction_after_resume(session.id()) {
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = transaction.transaction_id,
            recovery_scope = "audio_realign",
            target_timeline_nsecs = transaction.target_timeline_nsecs,
            transaction_generation = transaction.generation,
            transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
            attempts = transaction.attempts,
            transaction_phase = transaction.phase.as_str(),
            coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
            coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
            "cleared FFmpeg audio realign transaction after audio output resumed"
        );
        return Ok(false);
    }
    let Some(action) = pipeline.poll_audio_recovery_watchdog() else {
        return Ok(false);
    };
    match action {
        AudioRecoveryWatchdogAction::Warn {
            transaction,
            worker,
        } => {
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = false,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                "FFmpeg audio decoder recovery has made no progress for 500ms"
            );
            Ok(false)
        }
        AudioRecoveryWatchdogAction::LowLevelFallback {
            transaction,
            worker,
            request,
        } => {
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = true,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                fallback = "single_low_level_seek",
                arbitration_outcome = "watchdog_low_level_fallback",
                "FFmpeg audio decoder recovery timed out; executing bounded low-level fallback"
            );
            service_rebuffer_audio_realign_request(
                session,
                control,
                demux_cache,
                pipeline,
                vo_queue,
                event_tx,
                emit_playback_buffered_events,
                request,
                transaction.attempts,
                true,
            )
        }
        AudioRecoveryWatchdogAction::FallbackExhausted {
            transaction,
            worker,
        } => {
            tracing::error!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = false,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                fallback_suppressed = true,
                "FFmpeg audio decoder recovery remained stalled after bounded fallback"
            );
            let Some((terminal_transaction, resume_timeline_nsecs)) =
                pipeline.finish_exhausted_audio_realign(control, session.id())
            else {
                return Err(
                    "FFmpeg audio realign fallback exhausted without an active transaction"
                        .to_string(),
                );
            };
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = terminal_transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = terminal_transaction.target_timeline_nsecs,
                resume_timeline_nsecs,
                far_ahead_audio_timeline_nsecs =
                    terminal_transaction.request.far_ahead_audio_timeline_nsecs,
                transaction_elapsed_ms =
                    terminal_transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = terminal_transaction.attempts,
                transaction_phase = terminal_transaction.phase.as_str(),
                "exhausted bounded FFmpeg audio realign; continuing without another seek"
            );
            Ok(true)
        }
    }
}
