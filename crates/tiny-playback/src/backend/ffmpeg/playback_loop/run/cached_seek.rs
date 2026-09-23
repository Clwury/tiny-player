use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn service_hevc_startup_stall_watchdog_due_if_needed(
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
    missing_recovery_request_tracker: &mut MissingRecoveryRequestTracker,
    checkpoint: &'static str,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    let drained_progress = drain_video_decode_results_before_watchdog(
        session.id(),
        control,
        demux_cache,
        pipeline,
        vo_queue,
        event_tx,
        decode_pipeline,
        frame_presented,
    )?;
    if drained_progress {
        return Ok(true);
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
    if let PlaybackTickStatus::RecoveryPending(request) = tick_status {
        if service_cached_seek_recovery_fallback_if_needed(
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
            Some(request),
        )? {
            log_recovery_request_miss_summary(missing_recovery_request_tracker, session.id());
            return Ok(true);
        }
        wait_after_missing_recovery_request(
            pipeline,
            playback_wait,
            missing_recovery_request_tracker,
            request,
            session.id(),
            checkpoint,
        );
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_cached_seek_recovery_fallback_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    requested_recovery: Option<PlaybackRecoveryRequest>,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    let (cached_fallback, hevc_fallback, requested_recovery) =
        match take_next_recovery_fallback(pipeline, session.id(), requested_recovery) {
            RecoveryFallbackArbitration::CachedSeek(fallback) => (Some(fallback), None, None),
            RecoveryFallbackArbitration::HevcDecodeChain { request, fallback } => {
                (None, Some(fallback), request)
            }
            RecoveryFallbackArbitration::MissingRequested(_)
            | RecoveryFallbackArbitration::None => {
                return Ok(false);
            }
        };
    if let Some(fallback) = cached_fallback {
        let transaction_id = pipeline.active_recovery_transaction_id();
        let cleared_landing = pipeline
            .video_decode_pipeline
            .clear_hevc_low_level_seek_recovery();
        let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
        control.set_cache_paused(false);
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id,
            recovery_scope = pipeline.video_decode_recovery.recovery_scope().as_str(),
            target_nsecs = fallback.target_nsecs,
            fallback_source = "cached_seek_watchdog",
            fallback_reason = fallback.reason.as_str(),
            actual_anchor_nsecs = ?cleared_landing.map(|landing| landing.anchor_nsecs),
            actual_anchor_kind = ?cleared_landing.map(|landing| landing.anchor_kind.as_str()),
            arbitration_outcome = "fallback_consumed",
            fallback_consumed = true,
            fallback_cleared = cleared_landing.is_some(),
            "consumed atomic cached-seek recovery fallback request"
        );
        if pipeline
            .video_decode_pipeline
            .hevc_same_hardware_recovery_target()
            .is_some()
        {
            let action = pipeline
                .video_decode_pipeline
                .request_hevc_same_hardware_recovery(
                    cached_seek_fallback_as_hevc(fallback),
                    Instant::now(),
                );
            pipeline.video_decode_recovery.reset();
            if action == HevcDecodeRecoveryAction::None {
                pipeline.rearm_cached_seek_recovery_watchdog(
                    fallback.target_nsecs,
                    fallback.cached_seek,
                    session.id(),
                );
            }
            tracing::warn!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                cached_action = fallback.action.as_str(),
                recovery_action = action.as_str(),
                "continued active same-Vulkan transaction before cached-seek escalation"
            );
            return Ok(true);
        }
        match fallback.action {
            CachedSeekRecoveryFallbackAction::RecoveryExhausted => {
                return Err(format!(
                    "HEVC cached seek recovery exhausted at {:.3}s after soft recovery, bounded decoder recovery and low-level seek",
                    position_seconds
                ));
            }
            CachedSeekRecoveryFallbackAction::SoftRecover => {
                let requeued_probe_packets =
                    pipeline.soft_recover_cached_seek_hevc_decode_chain(session.id())?;
                pipeline.rearm_cached_seek_recovery_watchdog(
                    fallback.target_nsecs,
                    fallback.cached_seek,
                    session.id(),
                );
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
            CachedSeekRecoveryFallbackAction::RecoverHardware => {
                let action = pipeline
                    .video_decode_pipeline
                    .request_hevc_same_hardware_recovery(
                        cached_seek_fallback_as_hevc(fallback),
                        Instant::now(),
                    );
                if action == HevcDecodeRecoveryAction::None
                    && pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_target()
                        .is_none()
                {
                    return Err(format!(
                        "cached seek requested hardware recovery at {:.3}s without an active hardware decoder",
                        position_seconds
                    ));
                }
                pipeline.video_decode_recovery.reset();
                if action == HevcDecodeRecoveryAction::None {
                    pipeline.rearm_cached_seek_recovery_watchdog(
                        fallback.target_nsecs,
                        fallback.cached_seek,
                        session.id(),
                    );
                }
                tracing::warn!(
                    session_id = ?session.id(),
                    position_seconds,
                    target_nsecs = fallback.target_nsecs,
                    reason = fallback.reason.as_str(),
                    recovery_action = action.as_str(),
                    "routed cached-seek fallback into bounded same-Vulkan recovery"
                );
                return Ok(true);
            }
            CachedSeekRecoveryFallbackAction::LowLevelSeek => {}
        }
        let demux_watermark = demux_cache.cached_reader_watermark();
        let low_level_seek_required = matches!(
            fallback.action,
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        let failed_cra_cached_seek = fallback
            .cached_seek
            .filter(|info| info.uses_cra_anchor() && low_level_seek_required);
        if let Some(info) = failed_cra_cached_seek {
            demux_cache.exclude_failed_cached_seek_range(info, fallback.reason.as_str());
            tracing::warn!(
                session_id = ?session.id(),
                range_id = info.range_id,
                anchor_packet_id = info.anchor_packet_id,
                anchor_kind = info.anchor_kind.as_str(),
                anchor_nsecs = info.anchor_nsecs,
                target_nsecs = info.target_nsecs,
                preroll_nsecs = info.preroll_nsecs,
                reason = fallback.reason.as_str(),
                cached_seek_succeeded = false,
                low_level_fallback = true,
                "CRA cached seek recovery failed; performing its single low-level fallback"
            );
        }
        if !low_level_seek_required
            && !demux_reader_unusable_for_hevc_low_level_seek(demux_watermark)
        {
            pipeline.rearm_cached_seek_recovery_watchdog(
                fallback.target_nsecs,
                fallback.cached_seek,
                session.id(),
            );
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
        let buffering_policy = if failed_cra_cached_seek.is_some() {
            PlaybackSeekBufferingPolicy::PreserveVisibleFrame
        } else {
            internal_recovery_seek_buffering_policy(pipeline.output_scheduler.snapshot())
        };
        let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: crate::backend::PlaybackSeekMode::Precise,
            seek_generation,
            force_low_level_seek: true,
            cache_only: false,
            require_safe_cached_anchor: false,
            preserve_hevc_same_hardware_recovery: false,
            recovery_transaction_id: Some(transaction_id),
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
        pipeline.output_scheduler.begin_decode_recovery(
            transaction_id,
            fallback.target_nsecs,
            DecodeRecoverySource::LowLevelSeek,
            control,
            session.id(),
        );
        pipeline
            .output_scheduler
            .mark_decode_recovery_replaying(transaction_id);
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
    let fallback = hevc_fallback.expect("recovery arbitration selected a HEVC fallback");
    if let Some(request) = requested_recovery {
        let source_matches = matches!(
            request.source,
            PlaybackRecoverySource::HevcDecodeChain(reason) if reason == fallback.reason
        );
        if request.transaction_id != pipeline.active_recovery_transaction_id()
            || request.target_nsecs != fallback.target_nsecs
            || !source_matches
        {
            tracing::error!(
                session_id = ?session.id(),
                requested_transaction_id = request.transaction_id,
                active_transaction_id = pipeline.active_recovery_transaction_id(),
                requested_source = request.source.as_str(),
                requested_target_nsecs = request.target_nsecs,
                fallback_target_nsecs = fallback.target_nsecs,
                fallback_reason = fallback.reason.as_str(),
                arbitration_outcome = "request_mismatch_consumed_safely",
                "HEVC recovery request changed before atomic fallback consumption"
            );
        }
    }
    let transaction_id = pipeline.active_recovery_transaction_id();
    let cleared_landing = pipeline
        .video_decode_pipeline
        .clear_hevc_low_level_seek_recovery();
    let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
    control.set_cache_paused(false);
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id,
        recovery_scope = ?pipeline.video_decode_recovery.recovery_scope().as_str(),
        target_nsecs = fallback.target_nsecs,
        fallback_source = "hevc_decode_chain",
        fallback_reason = fallback.reason.as_str(),
        actual_anchor_nsecs = ?cleared_landing.map(|landing| landing.anchor_nsecs),
        actual_anchor_kind = ?cleared_landing.map(|landing| landing.anchor_kind.as_str()),
        arbitration_outcome = "fallback_consumed",
        fallback_consumed = true,
        fallback_cleared = cleared_landing.is_some(),
        "consumed atomic HEVC recovery fallback request"
    );

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
        && hevc_decode_chain_fallback_requests_same_hardware_recovery(fallback.reason)
    {
        let action = pipeline
            .video_decode_pipeline
            .request_hevc_same_hardware_recovery(fallback, Instant::now());
        pipeline.video_decode_recovery.reset();
        if action == HevcDecodeRecoveryAction::RequestSoftwareFallback {
            tracing::warn!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                reason = fallback.reason.as_str(),
                recovery_action = action.as_str(),
                "routed exhausted bounded HEVC hardware recovery to software fallback"
            );
            return fallback_to_software_after_same_hardware_recovery(
                session,
                control,
                demux_cache,
                pipeline,
                vo_queue,
                event_tx,
                emit_playback_buffered_events,
            );
        }
        if action == HevcDecodeRecoveryAction::None {
            pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        }
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            recovery_action = action.as_str(),
            "routed HEVC hardware fallback into bounded same-Vulkan recovery"
        );
        return Ok(true);
    }

    if let Some(info) = pipeline.active_cra_cached_seek() {
        let position_seconds = nsecs_to_seconds(info.target_nsecs);
        pipeline.clear_cached_seek_recovery_watchdog();
        demux_cache.exclude_failed_cached_seek_range(info, fallback.reason.as_str());
        let seek_generation = control.request_seek();
        session.reset_to(session.id(), position_seconds);
        pipeline.current_start_position_nsecs = session.start_position_nsecs();
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            anchor_nsecs = info.anchor_nsecs,
            target_nsecs = info.target_nsecs,
            preroll_nsecs = info.preroll_nsecs,
            reason = fallback.reason.as_str(),
            seek_generation,
            cached_seek_succeeded = false,
            low_level_fallback = true,
            preserve_visible_frame = true,
            "CRA cached seek decode chain failed; performing its single low-level fallback"
        );
        let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: crate::backend::PlaybackSeekMode::Precise,
            seek_generation,
            force_low_level_seek: true,
            cache_only: false,
            require_safe_cached_anchor: false,
            preserve_hevc_same_hardware_recovery: false,
            recovery_transaction_id: Some(transaction_id),
            low_level_seek_reason: Some(fallback.reason.as_str()),
            session_id: session.id(),
            vo_queue,
            demux_cache,
            pipeline,
            emit_playback_buffered_events,
            buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
            control,
            event_tx,
        })?;
        pipeline
            .video_decode_pipeline
            .remember_hevc_recovery_low_level_seek_target(info.target_nsecs);
        tracing::debug!(
            session_id = ?session.id(),
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            target_nsecs = info.target_nsecs,
            seek_generation,
            ?demux_seek_result,
            "completed CRA cached seek decode-error low-level fallback transaction"
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

    // Hardware decoders have already been routed through the bounded
    // same-Vulkan transaction above. Reaching this point means the pipeline is
    // software-decoded and only its existing low-level seek policy applies.
    let requeued_probe_packets = 0usize;
    let software_reopened_without_replay = false;
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
        software_reopened_without_replay,
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
    if !software_reopened_without_replay
        && !force_low_level_from_loop
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
        seek_mode: crate::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: Some(transaction_id),
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
    pipeline.output_scheduler.begin_decode_recovery(
        transaction_id,
        fallback.target_nsecs,
        DecodeRecoverySource::LowLevelSeek,
        control,
        session.id(),
    );
    pipeline
        .output_scheduler
        .mark_decode_recovery_replaying(transaction_id);
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

pub(super) fn hevc_decode_chain_fallback_requests_same_hardware_recovery(
    reason: HevcDecodeChainFallbackReason,
) -> bool {
    matches!(
        reason,
        HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            | HevcDecodeChainFallbackReason::StartupInFlightStall
            | HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
            | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
            | HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill
    )
}

pub(super) fn cached_seek_fallback_as_hevc(
    fallback: CachedSeekRecoveryFallback,
) -> HevcDecodeChainFallback {
    HevcDecodeChainFallback {
        target_nsecs: fallback.target_nsecs,
        reason: match fallback.reason {
            CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout => {
                HevcDecodeChainFallbackReason::StartupInFlightStall
            }
            CachedSeekRecoveryFallbackReason::VideoPacketLimit => {
                HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            }
        },
    }
}

pub(super) fn hevc_decode_chain_fallback_requires_boundary_reset(
    reason: HevcDecodeChainFallbackReason,
) -> bool {
    reason.requires_boundary_reset()
}

pub(super) fn demux_reader_unusable_for_hevc_low_level_seek(
    watermark: DemuxReaderWatermark,
) -> bool {
    let video_forward_empty = watermark.video_forward_nsecs.unwrap_or_default() == 0;
    let selected_forward_empty = watermark.selected_min_forward_nsecs.unwrap_or_default() == 0;
    watermark.video_underrun && video_forward_empty && selected_forward_empty
}

pub(super) fn demux_reader_healthy_for_hevc_low_level_seek_suppression(
    watermark: DemuxReaderWatermark,
) -> bool {
    let video_forward_nsecs = watermark
        .video_forward_nsecs
        .or(watermark.selected_min_forward_nsecs)
        .unwrap_or_default();
    !watermark.video_underrun
        && video_forward_nsecs >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
}

pub(super) fn hevc_decode_chain_fallback_should_suppress_low_level_seek(
    reason: HevcDecodeChainFallbackReason,
    target_nsecs: u64,
    probe_packets: usize,
    demux_watermark: DemuxReaderWatermark,
    startup_or_post_seek: bool,
    software_reopened_without_replay: bool,
) -> bool {
    !software_reopened_without_replay
        && matches!(
            reason,
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
                | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
        )
        && target_nsecs == 0
        && probe_packets == 0
        && startup_or_post_seek
        && demux_reader_healthy_for_hevc_low_level_seek_suppression(demux_watermark)
}
