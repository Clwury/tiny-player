use super::*;

/// Handle failures discovered after avcodec_open2, including first-frame format
/// negotiation, VA surface mapping and rendering. Corrupt packets and normal
/// queue pressure have already been handled by the decoder/recovery services.
#[allow(clippy::too_many_arguments)]
pub(super) fn fallback_after_hardware_error(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    error: &str,
) -> Result<bool, String> {
    if !pipeline
        .video_decode_pipeline
        .can_fallback_after_hardware_error(error)
    {
        return Ok(false);
    }
    if control.should_interrupt() {
        return Ok(true);
    }
    let target_nsecs = if pipeline
        .output_scheduler
        .snapshot()
        .first_video_frame_pending
    {
        pipeline.current_start_position_nsecs
    } else {
        pipeline
            .audio_output
            .as_ref()
            .and_then(|audio| audio.snapshot().ok())
            .map(|snapshot| snapshot.played_timeline_nsecs)
            .unwrap_or_else(|| pipeline.scheduler.current_timeline_nsecs())
            .max(pipeline.current_start_position_nsecs)
    };
    let old_backend = pipeline
        .video_decode_pipeline
        .info()
        .hardware_decode_backend;
    let generation = pipeline.advance_playback_generation();
    pipeline
        .output_scheduler
        .release_vulkan_frames_for_resource_pressure(control, session.id());
    vo_queue.discard_pending_frames(session.id());
    pipeline
        .video_frame_prepare_worker
        .restart_after_resource_pressure(generation)?;
    pipeline
        .video_decode_pipeline
        .reopen_next_auto_decoder(pipeline.video_stream)?;
    let info = playback_video_info_from_worker(
        pipeline.video_stream,
        pipeline.video_decode_pipeline.info(),
    );
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(info),
    ));

    // Use the existing generation-fenced seek reset to replay compressed input
    // from a safe boundary and realign audio/subtitles without changing settings.
    let position_seconds = nsecs_to_seconds(target_nsecs);
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek: true,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: None,
        low_level_seek_reason: Some("auto_hardware_decode_fallback"),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
        control,
        event_tx,
    })?;
    vo_queue.set_auto_hardware_fallback(
        session.id(),
        pipeline.video_decode_pipeline.info().hardware_accelerated,
    );
    if let Some(device) = pipeline.video_decode_pipeline.info().vulkan_device.clone() {
        vo_queue.request_vulkan_prewarm(session.id(), device);
    }
    tracing::warn!(
        ?old_backend,
        backend = ?pipeline.video_decode_pipeline.info().hardware_decode_backend,
        target_nsecs,
        %error,
        "Auto decode advanced to the next candidate and reset playback input"
    );
    Ok(true)
}
