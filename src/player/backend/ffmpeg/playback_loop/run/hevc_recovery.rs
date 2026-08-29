use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn service_hevc_same_hardware_recovery_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    playback_wait: &PlaybackPipelineWaitService,
    decode_pipeline: &mut DecodePipelineService,
    frame_presented: &AtomicBool,
) -> std::result::Result<bool, String> {
    let now = Instant::now();
    if let Some(drop) = pipeline
        .output_scheduler
        .take_decode_recovery_drop_for_fallback()
    {
        if pipeline
            .video_decode_pipeline
            .hevc_same_hardware_recovery_target()
            .is_none()
        {
            return Err(format!(
                "{} recovery produced an unbridged continuous decode gap of {:.3}s at {:.3}s (first frame {:.3}s) after bounded decoder fallback was exhausted",
                drop.source.as_str(),
                drop.gap_nsecs as f64 / 1_000_000_000.0,
                drop.target_nsecs as f64 / 1_000_000_000.0,
                drop.first_frame_nsecs as f64 / 1_000_000_000.0,
            ));
        }
        pipeline
            .video_decode_pipeline
            .mark_hevc_same_hardware_unbridged_continuous_gap();
        tracing::warn!(
            session_id = ?session.id(),
            transaction_id = drop.transaction_id,
            attempt_id = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_attempt_id(),
            decoder_epoch = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_decoder_epoch(),
            recovery_source = drop.source.as_str(),
            target_nsecs = drop.target_nsecs,
            first_frame_nsecs = drop.first_frame_nsecs,
            gap_ms = drop.gap_nsecs as f64 / 1_000_000.0,
            "routed unbridged continuous decode gap back into bounded decoder fallback"
        );
    }
    let action = pipeline
        .video_decode_pipeline
        .pending_hevc_same_hardware_recovery_action(now);

    if action == HevcDecodeRecoveryAction::None {
        let Some(ticket) = pipeline
            .video_decode_pipeline
            .hevc_same_hardware_prewarm_ticket()
        else {
            return Ok(false);
        };
        match vo_queue.vulkan_prewarm_status(ticket) {
            VulkanPrewarmStatus::Pending => {
                let deadline = pipeline.playback_loop_deadline();
                playback_wait.wait_poll_interval_and_delay_scheduler_until(
                    &mut pipeline.scheduler,
                    deadline,
                );
                return Ok(true);
            }
            VulkanPrewarmStatus::Ready => {
                let target_nsecs = pipeline
                    .video_decode_pipeline
                    .hevc_same_hardware_recovery_target()
                    .ok_or_else(|| {
                        "Vulkan prewarm completed without a same-hardware target".to_string()
                    })?;
                if !pipeline
                    .video_decode_pipeline
                    .mark_hevc_same_hardware_prewarm_ready(now)
                {
                    return Err(
                        "Vulkan prewarm completed outside same-hardware recovery".to_string()
                    );
                }
                let replay_packets = pipeline
                    .video_decode_pipeline
                    .requeue_hevc_hw_replay_journal(
                        &mut pipeline.playback_generation,
                        target_nsecs,
                        session.id(),
                    )?;
                pipeline
                    .video_decode_pipeline
                    .record_hevc_same_hardware_replay(replay_packets, true, now);
                if replay_packets > 0 {
                    pipeline.dovi_pipeline.reset();
                    pipeline
                        .video_decode_recovery
                        .begin_verified_replay_from_safe_anchor(
                            pipeline.video_stream.codec_id,
                            target_nsecs,
                        );
                    pipeline
                        .output_scheduler
                        .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
                    pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
                }
                tracing::info!(
                    session_id = ?session.id(),
                    target_nsecs,
                    recovery_action = HevcDecodeRecoveryAction::ReplaySameHardware.as_str(),
                    replay_packets,
                    vulkan_device = ticket.device_key(),
                    same_hw_reopen_result = "prewarm_ready",
                    "replayed safe HEVC journal after same-Vulkan renderer prewarm"
                );
                return Ok(true);
            }
            VulkanPrewarmStatus::Failed(error) => {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(format!(
                        "Vulkan renderer prewarm failed: {error}"
                    ));
                return Ok(true);
            }
            VulkanPrewarmStatus::Stale => {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(
                        "Vulkan renderer prewarm request became stale before replay",
                    );
                return Ok(true);
            }
        }
    }

    if let Some(suppressed_repeats) = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_action_log_summary(action, now)
    {
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = pipeline.active_recovery_transaction_id(),
            attempt_id = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_attempt_id(),
            decoder_epoch = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_decoder_epoch(),
            recovery_action = action.as_str(),
            target_nsecs = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target(),
            requested_hw_mode = ?pipeline.video_decode_pipeline.requested_hardware_mode(),
            suppressed_repeats,
            "servicing bounded HEVC same-Vulkan recovery action"
        );
    }

    match action {
        HevcDecodeRecoveryAction::None => Ok(false),
        HevcDecodeRecoveryAction::DrainPendingResults => {
            let before = pipeline.video_decode_pipeline.snapshot();
            let max_passes = before
                .command_queue_capacity
                .saturating_add(before.queue_capacity)
                .saturating_add(1)
                .max(1);
            let mut made_progress = false;
            let mut passes = 0usize;
            while passes < max_passes {
                passes = passes.saturating_add(1);
                let status = decode_pipeline.service_once(DecodePipelineServiceContext {
                    pipeline,
                    control,
                    session_id: session.id(),
                    event_tx,
                    vo_queue,
                    frame_presented,
                    demux_reader_watermark: || demux_cache.cached_reader_watermark(),
                })?;
                made_progress |= status.made_progress();
                if status.interrupted() || !status.made_progress() {
                    break;
                }
            }
            let after = pipeline.video_decode_pipeline.snapshot();
            // mpv's lavc_process() receive-first loop only treats work from the
            // video decoder as decoder progress. Audio/subtitle output drained
            // by service_once must not keep a failed video decoder in grace.
            let video_result_progress = hevc_drain_video_result_progressed(before, after);
            let prepare_after = pipeline.video_frame_prepare_worker.snapshot();
            let decoder_work_pending = hevc_decoder_drain_work_pending(after)
                || prepare_after.pending_input_frames > 0
                || prepare_after.in_flight_frames > 0
                || prepare_after.completed_frames > 0;
            made_progress |= video_result_progress;
            let advanced = pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_drain_pass(
                    video_result_progress,
                    decoder_work_pending,
                    Instant::now(),
                );
            let drain_now = Instant::now();
            if let Some(suppressed_repeats) = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_drain_log_summary(advanced, drain_now)
            {
                tracing::debug!(
                    session_id = ?session.id(),
                    transaction_id = pipeline.active_recovery_transaction_id(),
                    attempt_id = ?pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_attempt_id(),
                    decoder_epoch = ?pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_decoder_epoch(),
                    passes,
                    made_progress,
                    video_result_progress,
                    decoder_work_pending,
                    advanced,
                    suppressed_repeats,
                    submitted_sequence = after.submitted_sequence,
                    result_produced_sequence = after.result_produced_sequence,
                    result_consumed_sequence = after.result_consumed_sequence,
                    submitted_not_consumed_packets = after.submitted_not_consumed_packets,
                    last_worker_progress_ms = ?after.last_result_produced_at.map(|at| {
                        drain_now.saturating_duration_since(at).as_secs_f64() * 1000.0
                    }),
                    "drained pending decode output for same-Vulkan recovery"
                );
            }
            if !advanced {
                let deadline = pipeline.playback_loop_deadline();
                playback_wait.wait_poll_interval_and_delay_scheduler_until(
                    &mut pipeline.scheduler,
                    deadline,
                );
            }
            Ok(true)
        }
        HevcDecodeRecoveryAction::FlushSameHardware => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "same-Vulkan flush has no recovery target".to_string())?;
            control.set_cache_paused(false);
            let discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
            let generation = pipeline.advance_playback_generation();
            pipeline
                .video_frame_prepare_worker
                .flush_generation(generation);
            pipeline.output_scheduler.begin_decode_recovery(
                pipeline.active_recovery_transaction_id(),
                target_nsecs,
                DecodeRecoverySource::FlushReplay,
                control,
                session.id(),
            );
            pipeline
                .video_decode_recovery
                .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            if let Err(error) = pipeline
                .video_decode_pipeline
                .begin_hevc_same_hardware_flush(generation, now)
            {
                tracing::warn!(
                    session_id = ?session.id(),
                    target_nsecs,
                    %error,
                    same_hw_recovery_phase = "flush_failed",
                    "same-Vulkan decoder flush could not be scheduled"
                );
                return Ok(true);
            }
            pipeline
                .video_decode_pipeline
                .reset_hevc_decoder_transient_preserving_gap_evidence(now);
            let replay_packets = pipeline
                .video_decode_pipeline
                .requeue_hevc_hw_replay_journal(
                    &mut pipeline.playback_generation,
                    target_nsecs,
                    session.id(),
                )?;
            pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_replay(replay_packets, false, now);
            if replay_packets > 0 {
                pipeline.dovi_pipeline.reset();
                pipeline
                    .video_decode_recovery
                    .begin_verified_replay_from_safe_anchor(
                        pipeline.video_stream.codec_id,
                        target_nsecs,
                    );
                pipeline
                    .output_scheduler
                    .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
                pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
            }
            tracing::info!(
                session_id = ?session.id(),
                target_nsecs,
                discarded_vo_frames,
                replay_packets,
                recovery_action = HevcDecodeRecoveryAction::ReplaySameHardware.as_str(),
                same_hw_recovery_phase = "replaying_after_flush",
                "flushed and replayed the current Vulkan decoder"
            );
            Ok(true)
        }
        HevcDecodeRecoveryAction::ReopenSameHardware => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "same-Vulkan reopen has no recovery target".to_string())?;
            let generation = pipeline.advance_playback_generation();
            let release_first = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_is_resource_pressure();
            let mut discarded_vo_frames = 0;
            let mut released_scheduler_frames = 0;
            if release_first {
                control.set_cache_paused(false);
                released_scheduler_frames = pipeline
                    .output_scheduler
                    .release_vulkan_frames_for_resource_pressure(control, session.id());
                discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
                if let Err(error) = pipeline
                    .video_frame_prepare_worker
                    .restart_after_resource_pressure(generation)
                {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_recovery(format!(
                            "same-Vulkan reopen could not retire frame-prepare worker: {error}"
                        ));
                    tracing::error!(
                        session_id = ?session.id(),
                        target_nsecs,
                        generation,
                        released_scheduler_frames,
                        discarded_vo_frames,
                        %error,
                        "aborted release-first Vulkan reopen before opening a second frame pool"
                    );
                    return Ok(true);
                }
                pipeline.output_scheduler.begin_decode_recovery(
                    pipeline.active_recovery_transaction_id(),
                    target_nsecs,
                    DecodeRecoverySource::VulkanReopenReplay,
                    control,
                    session.id(),
                );
                pipeline
                    .video_decode_recovery
                    .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            }
            let device = match pipeline
                .video_decode_pipeline
                .begin_hevc_same_hardware_reopen(pipeline.video_stream, generation, now)
            {
                Ok(device) => device,
                Err(error) => {
                    tracing::warn!(
                        session_id = ?session.id(),
                        target_nsecs,
                        %error,
                        same_hw_reopen_result = "failed",
                        "same-Vulkan decoder reopen failed"
                    );
                    return Ok(true);
                }
            };
            if !release_first {
                control.set_cache_paused(false);
                discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
                pipeline
                    .video_frame_prepare_worker
                    .flush_generation(generation);
            }
            pipeline.dovi_pipeline.reset();
            if !release_first {
                pipeline.output_scheduler.begin_decode_recovery(
                    pipeline.active_recovery_transaction_id(),
                    target_nsecs,
                    DecodeRecoverySource::VulkanReopenReplay,
                    control,
                    session.id(),
                );
                pipeline
                    .video_decode_recovery
                    .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            }
            let Some(ticket) = vo_queue.request_vulkan_prewarm(session.id(), device.clone()) else {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(
                        "new Vulkan device prewarm was rejected for the active playback session",
                    );
                return Ok(true);
            };
            pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_prewarm_request(ticket)?;
            let playback_video_info = playback_video_info_from_worker(
                pipeline.video_stream,
                pipeline.video_decode_pipeline.info(),
            );
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::PlaybackInfoChanged(playback_video_info),
            ));
            tracing::info!(
                session_id = ?session.id(),
                target_nsecs,
                discarded_vo_frames,
                released_scheduler_frames,
                release_first,
                vulkan_device = device.key(),
                same_hw_reopen_attempt = 1,
                same_hw_reopen_result = "opened_waiting_for_renderer_prewarm",
                "atomically replaced the HEVC decoder with a new Vulkan worker"
            );
            Ok(true)
        }
        HevcDecodeRecoveryAction::ReplaySameHardware => Ok(true),
        HevcDecodeRecoveryAction::RebuildFromCachedSeek => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "cached safe-IDR rebuild has no recovery target".to_string())?;
            let transaction_id = pipeline.active_recovery_transaction_id();
            let position_seconds = nsecs_to_seconds(target_nsecs);
            let previous_session_start_nsecs = session.start_position_nsecs();
            let previous_pipeline_start_nsecs = pipeline.current_start_position_nsecs;
            let seek_generation = control.request_seek();
            session.reset_to(session.id(), position_seconds);
            pipeline.current_start_position_nsecs = session.start_position_nsecs();
            let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
                position_seconds,
                seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
                seek_generation,
                force_low_level_seek: false,
                cache_only: true,
                require_safe_cached_anchor: true,
                preserve_hevc_same_hardware_recovery: true,
                recovery_transaction_id: Some(transaction_id),
                low_level_seek_reason: Some("same_vulkan_reopen_replay_failed"),
                session_id: session.id(),
                vo_queue,
                demux_cache,
                pipeline,
                emit_playback_buffered_events,
                buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
                control,
                event_tx,
            });
            let demux_seek_result = match demux_seek_result {
                Ok(result) => result,
                Err(error) => {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(format!(
                            "cached safe-IDR rebuild reset failed: {error}"
                        ));
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        %error,
                        "cached safe-IDR rebuild reset failed after Vulkan reopen"
                    );
                    return Ok(true);
                }
            };
            match demux_seek_result {
                DemuxSeekResult::Cached(info) => {
                    debug_assert!(info.anchor_is_safe_seek_point);
                    let generation = pipeline.playback_generation.current();
                    if let Err(error) = pipeline
                        .video_decode_pipeline
                        .begin_hevc_same_hardware_cached_rebuild(generation, Instant::now())
                    {
                        pipeline
                            .video_decode_pipeline
                            .fail_hevc_same_hardware_cached_rebuild(format!(
                                "cached safe-IDR rebuild could not start: {error}"
                            ));
                        return Ok(true);
                    }
                    pipeline.output_scheduler.begin_decode_recovery(
                        transaction_id,
                        target_nsecs,
                        DecodeRecoverySource::CachedSafeIdrRebuild,
                        control,
                        session.id(),
                    );
                    pipeline
                        .output_scheduler
                        .mark_decode_recovery_replaying(transaction_id);
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        playback_generation = generation,
                        range_id = info.range_id,
                        anchor_packet_id = info.anchor_packet_id,
                        anchor_kind = info.anchor_kind.as_str(),
                        anchor_nsecs = info.anchor_nsecs,
                        preroll_nsecs = info.preroll_nsecs,
                        "rebuilding reopened Vulkan decoder from cached safe IDR after replay failure"
                    );
                }
                DemuxSeekResult::Unavailable => {
                    session.reset_to(session.id(), nsecs_to_seconds(previous_session_start_nsecs));
                    pipeline.current_start_position_nsecs = previous_pipeline_start_nsecs;
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(
                            "demux cache has no preceding safe IDR covering the recovery target",
                        );
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        "cached safe-IDR rebuild was unavailable after Vulkan reopen"
                    );
                }
                DemuxSeekResult::Superseded => {
                    session.reset_to(session.id(), nsecs_to_seconds(previous_session_start_nsecs));
                    pipeline.current_start_position_nsecs = previous_pipeline_start_nsecs;
                    pipeline
                        .video_decode_pipeline
                        .finish_hevc_same_hardware_recovery_terminal();
                    tracing::debug!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        "cancelled cached safe-IDR rebuild for a newer seek"
                    );
                }
                DemuxSeekResult::Requested => {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(
                            "cache-only safe-IDR rebuild unexpectedly requested a low-level seek",
                        );
                }
            }
            Ok(true)
        }
        HevcDecodeRecoveryAction::RequestSoftwareFallback => {
            fallback_to_software_after_same_hardware_recovery(
                session,
                control,
                demux_cache,
                pipeline,
                vo_queue,
                event_tx,
                emit_playback_buffered_events,
            )
        }
        HevcDecodeRecoveryAction::FailExplicitly => {
            let error = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_terminal_error(now)
                .unwrap_or_else(|| "ForceVulkan 同硬解恢复失败，但缺少终态诊断".to_string());
            pipeline
                .video_decode_pipeline
                .finish_hevc_same_hardware_recovery_terminal();
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fallback_to_software_after_same_hardware_recovery(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    let now = Instant::now();
    let target_nsecs = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_recovery_target()
        .ok_or_else(|| "software fallback requested without a same-hardware target".to_string())?;
    let terminal_error = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_recovery_terminal_error(now)
        .unwrap_or_else(|| "bounded same-Vulkan recovery exhausted".to_string());
    if pipeline.video_decode_pipeline.requested_hardware_mode()
        != super::super::HardwareDecodeMode::Auto
    {
        return Err(format!(
            "software fallback invariant violation for {:?}: {terminal_error}",
            pipeline.video_decode_pipeline.requested_hardware_mode()
        ));
    }

    let reopened = pipeline
        .video_decode_pipeline
        .reopen_software_decoder(pipeline.video_stream)?;
    if !reopened {
        return Err(format!(
            "HEVC hardware recovery requested software fallback but decoder was already software: {terminal_error}"
        ));
    }
    let discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
    let generation = pipeline.advance_playback_generation();
    pipeline
        .video_frame_prepare_worker
        .flush_generation(generation);
    pipeline.output_scheduler.begin_decode_recovery(
        pipeline.active_recovery_transaction_id(),
        target_nsecs,
        DecodeRecoverySource::SoftwareFallback,
        control,
        session.id(),
    );
    pipeline
        .video_decode_recovery
        .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
    pipeline.dovi_pipeline.reset();
    let playback_video_info = playback_video_info_from_worker(
        pipeline.video_stream,
        pipeline.video_decode_pipeline.info(),
    );
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(playback_video_info),
    ));
    let replay_packets = pipeline
        .video_decode_pipeline
        .requeue_hevc_hw_replay_journal(
            &mut pipeline.playback_generation,
            target_nsecs,
            session.id(),
        )?;
    pipeline
        .video_decode_pipeline
        .finish_hevc_same_hardware_recovery_terminal();
    tracing::warn!(
        session_id = ?session.id(),
        target_nsecs,
        discarded_vo_frames,
        replay_packets,
        terminal_error,
        "Auto mode opened the software decoder after HEVC hardware recovery failure"
    );
    if replay_packets > 0 {
        pipeline
            .video_decode_recovery
            .begin_verified_replay_from_safe_anchor(pipeline.video_stream.codec_id, target_nsecs);
        pipeline
            .output_scheduler
            .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
        pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
        return Ok(true);
    }

    let position_seconds = nsecs_to_seconds(target_nsecs);
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek: true,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: Some(pipeline.active_recovery_transaction_id()),
        low_level_seek_reason: Some("hevc_hardware_recovery_fallback"),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
        control,
        event_tx,
    })?;
    pipeline.output_scheduler.begin_decode_recovery(
        pipeline.active_recovery_transaction_id(),
        target_nsecs,
        DecodeRecoverySource::LowLevelSeek,
        control,
        session.id(),
    );
    pipeline
        .output_scheduler
        .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
    tracing::warn!(
        session_id = ?session.id(),
        target_nsecs,
        seek_generation,
        ?demux_seek_result,
        "software fallback had no replay journal and performed one exact low-level seek"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn drain_video_decode_results_before_watchdog(
    session_id: PlaybackSessionId,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    decode_pipeline: &mut DecodePipelineService,
    frame_presented: &AtomicBool,
) -> std::result::Result<bool, String> {
    let Some(deadline) = pipeline
        .video_decode_pipeline
        .hevc_startup_stall_watchdog_deadline()
    else {
        return Ok(false);
    };
    if Instant::now() < deadline {
        return Ok(false);
    }

    let before = pipeline.video_decode_pipeline.snapshot();
    let max_passes = before
        .command_queue_capacity
        .saturating_add(before.queue_capacity)
        .saturating_add(1)
        .max(1);
    let mut made_progress = false;
    let mut passes = 0usize;
    while passes < max_passes {
        passes = passes.saturating_add(1);
        let status = decode_pipeline.service_once(DecodePipelineServiceContext {
            pipeline,
            control,
            session_id,
            event_tx,
            vo_queue,
            frame_presented,
            demux_reader_watermark: || demux_cache.cached_reader_watermark(),
        })?;
        made_progress |= status.made_progress();
        if status.interrupted() || !status.made_progress() {
            break;
        }
    }
    if made_progress {
        pipeline
            .video_decode_pipeline
            .observe_hevc_decode_pipeline_progress(Instant::now());
    }
    let after = pipeline.video_decode_pipeline.snapshot();
    tracing::debug!(
        session_id = ?session_id,
        passes,
        made_progress,
        submitted_sequence = after.submitted_sequence,
        result_produced_sequence_before = before.result_produced_sequence,
        result_produced_sequence = after.result_produced_sequence,
        result_consumed_sequence_before = before.result_consumed_sequence,
        result_consumed_sequence = after.result_consumed_sequence,
        submitted_not_consumed_packets_before = before.submitted_not_consumed_packets,
        submitted_not_consumed_packets = after.submitted_not_consumed_packets,
        completed_packets = after.completed_packets,
        decoded_frames = after.queued_frames,
        oldest_submitted_packet_nsecs = ?after.oldest_submitted_packet_nsecs,
        last_worker_progress_ms = ?after.last_result_produced_at.map(|at| {
            Instant::now().saturating_duration_since(at).as_secs_f64() * 1000.0
        }),
        "drained FFmpeg video worker results before HEVC startup watchdog decision"
    );
    Ok(made_progress
        || before.result_consumed_sequence != after.result_consumed_sequence
        || before.result_produced_sequence != after.result_produced_sequence)
}
