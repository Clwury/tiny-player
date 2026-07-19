use super::playback_pipeline_state::PlaybackPipelineState;
use std::sync::mpsc::Sender;

use crate::player::{
    backend::{BackendEvent, BackendEventKind, PlaybackSeekMode},
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::demux_cache::DemuxSeekResult;
use super::{
    BufferedReporter, DemuxPacketCache, FfmpegControl, preroll_seek_position_seconds,
    reset_playback_timeline_state, seconds_to_nsecs,
};
use ffmpeg_sys_next as ffi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackPositionResetKind {
    Seek,
    TrackSelection,
}

impl PlaybackPositionResetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Seek => "seek",
            Self::TrackSelection => "track_selection",
        }
    }
}

pub(super) struct PlaybackGenerationFlushContext<'a> {
    pub(super) kind: PlaybackPositionResetKind,
    pub(super) position_seconds: f64,
    pub(super) seek_mode: PlaybackSeekMode,
    pub(super) seek_generation: u64,
    pub(super) force_low_level_seek: bool,
    pub(super) cache_only: bool,
    pub(super) low_level_seek_reason: Option<&'static str>,
    pub(super) session_id: PlaybackSessionId,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) selected_tracks: Option<&'a crate::player::PlaybackTrackSelection>,
    pub(super) control: &'a FfmpegControl,
}

pub(super) struct PlaybackPositionStateResetContext<'a> {
    pub(super) position_seconds: f64,
    pub(super) session_id: PlaybackSessionId,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) emit_playback_buffered_events: bool,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackSeekBufferingPolicy {
    Emit,
    PreserveVisibleFrame,
}

impl PlaybackSeekBufferingPolicy {
    fn emits_buffering_start(self) -> bool {
        matches!(self, Self::Emit)
    }
}

pub(super) struct PlaybackSeekResetContext<'a> {
    pub(super) position_seconds: f64,
    pub(super) seek_mode: PlaybackSeekMode,
    pub(super) seek_generation: u64,
    pub(super) force_low_level_seek: bool,
    pub(super) cache_only: bool,
    pub(super) recovery_transaction_id: Option<u64>,
    pub(super) low_level_seek_reason: Option<&'static str>,
    pub(super) session_id: PlaybackSessionId,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) emit_playback_buffered_events: bool,
    pub(super) buffering_policy: PlaybackSeekBufferingPolicy,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}

pub(super) struct PlaybackGenerationFlushResult {
    pub(super) generation: u64,
    pub(super) demux_seek_result: DemuxSeekResult,
}

fn flush_playback_generation_for_position_reset(
    context: PlaybackGenerationFlushContext<'_>,
) -> std::result::Result<PlaybackGenerationFlushResult, String> {
    let generation = context.pipeline.advance_playback_generation();
    context.vo_queue.begin_session(context.session_id);
    context.pipeline.flush_playback_generation(generation)?;
    if let Some(audio_output) = context.pipeline.audio_output.as_ref() {
        audio_output.reset_clock(context.pipeline.current_start_position_nsecs);
    }
    context.pipeline.output_scheduler.reset(context.control);
    let demux_seek_result = if context.force_low_level_seek {
        context.demux_cache.seek_low_level(
            context.position_seconds,
            context.session_id,
            context.seek_generation,
            context
                .low_level_seek_reason
                .unwrap_or("forced_low_level_seek"),
        )
    } else if context.cache_only {
        context.demux_cache.seek_cached_only(
            context.position_seconds,
            context.seek_mode,
            context.session_id,
            context.seek_generation,
        )
    } else {
        context.demux_cache.seek(
            context.position_seconds,
            context.seek_mode,
            context.session_id,
            context.seek_generation,
        )
    };
    tracing::debug!(
        session_id = ?context.session_id,
        reset_kind = context.kind.as_str(),
        position_seconds = context.position_seconds,
        seek_mode = ?context.seek_mode,
        seek_generation = context.seek_generation,
        force_low_level_seek = context.force_low_level_seek,
        cache_only = context.cache_only,
        low_level_seek_reason = ?context.low_level_seek_reason,
        playback_generation = generation,
        current_start_position_nsecs = context.pipeline.current_start_position_nsecs,
        ?demux_seek_result,
        selected_tracks = ?context.selected_tracks,
        "handling FFmpeg playback generation flush transaction"
    );
    context.control.finish_seek(context.seek_generation);
    Ok(PlaybackGenerationFlushResult {
        generation,
        demux_seek_result,
    })
}

fn hevc_seek_starts_video_bootstrap(
    codec_id: ffi::AVCodecID,
    force_low_level_seek: bool,
    reason: Option<&'static str>,
    demux_seek_result: DemuxSeekResult,
) -> bool {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return false;
    }
    if !force_low_level_seek && !matches!(demux_seek_result, DemuxSeekResult::Requested) {
        return false;
    }
    reason.is_some_and(|reason| {
        matches!(reason, "first_video_frame_timeout" | "video_packet_limit")
            || reason.starts_with("hevc_decode_chain_")
    })
}

pub(super) fn service_playback_generation_seek(
    context: PlaybackGenerationFlushContext<'_>,
) -> std::result::Result<PlaybackGenerationFlushResult, String> {
    let kind = context.kind;
    let session_id = context.session_id;
    let flush_result = flush_playback_generation_for_position_reset(context)?;
    tracing::trace!(
        session_id = ?session_id,
        reset_kind = kind.as_str(),
        playback_generation = flush_result.generation,
        demux_seek_result = ?flush_result.demux_seek_result,
        "completed FFmpeg playback generation seek reset"
    );
    Ok(flush_result)
}

pub(super) fn service_playback_position_state_reset(
    context: PlaybackPositionStateResetContext<'_>,
) {
    let current_start_position_nsecs = context.pipeline.current_start_position_nsecs;
    reset_playback_timeline_state(
        context.pipeline.video_stream,
        context.pipeline.audio_stream,
        context.pipeline.video_frame_duration_nsecs,
        current_start_position_nsecs,
        &mut context.pipeline.video_clock,
        &mut context.pipeline.playback_timeline_origin_nsecs,
        &mut context.pipeline.audio_clock,
        &mut context.pipeline.scheduler,
        context.pipeline.audio_output.as_ref(),
        &mut context.pipeline.dovi_pipeline,
    );
    context.pipeline.dropped_video_frames_before_start_count = 0;
    context.pipeline.dropped_audio_frames_before_start_count = 0;
    context.pipeline.output_scheduler.reset(context.control);
    context
        .pipeline
        .video_decode_recovery
        .reset_for_timeline_start(
            context.pipeline.video_stream.codec_id,
            current_start_position_nsecs,
        );
    context
        .pipeline
        .subtitle_pipeline
        .reset_cues_for_position(current_start_position_nsecs);
    context.pipeline.buffered_reporter = BufferedReporter::new_with_events(
        context.pipeline.audio_output.is_some(),
        context.emit_playback_buffered_events,
    );
    context.pipeline.buffered_reporter.reset_to(
        context.position_seconds,
        context.session_id,
        context.event_tx,
    );
    let _ = context.event_tx.send(BackendEvent::new(
        context.session_id,
        BackendEventKind::PositionChanged(context.position_seconds),
    ));
    let _ = context.event_tx.send(BackendEvent::new(
        context.session_id,
        BackendEventKind::SubtitleChanged(None),
    ));
}

pub(super) fn service_playback_seek_reset(
    context: PlaybackSeekResetContext<'_>,
) -> std::result::Result<DemuxSeekResult, String> {
    let PlaybackSeekResetContext {
        position_seconds,
        seek_mode,
        seek_generation,
        force_low_level_seek: requested_force_low_level_seek,
        cache_only: requested_cache_only,
        recovery_transaction_id: requested_recovery_transaction_id,
        low_level_seek_reason,
        session_id,
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy,
        control,
        event_tx,
    } = context;
    let target_nsecs = seconds_to_nsecs(position_seconds);
    let seek_position_nsecs = seconds_to_nsecs(preroll_seek_position_seconds(
        pipeline.video_stream.codec_id,
        position_seconds,
    ));
    let repeated_cra_low_level_tuple = pipeline.video_stream.codec_id
        == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && pipeline
            .video_decode_pipeline
            .hevc_low_level_seek_would_repeat_cra(target_nsecs, seek_position_nsecs);
    let force_low_level_seek = requested_force_low_level_seek && !repeated_cra_low_level_tuple;
    let cache_only = requested_cache_only || repeated_cra_low_level_tuple;
    let recovery_transaction_id =
        requested_recovery_transaction_id.unwrap_or_else(|| pipeline.begin_recovery_transaction());
    pipeline.continue_recovery_transaction(recovery_transaction_id);
    if repeated_cra_low_level_tuple {
        tracing::warn!(
            ?session_id,
            target_nsecs,
            seek_position_nsecs,
            requested_force_low_level_seek,
            cache_only = true,
            repeat_low_level_seek_suppressed = true,
            "suppressed repeated HEVC low-level seek tuple; allowing cached recovery only"
        );
    }
    let flush_result = service_playback_generation_seek(PlaybackGenerationFlushContext {
        kind: PlaybackPositionResetKind::Seek,
        position_seconds,
        seek_mode,
        seek_generation,
        force_low_level_seek,
        cache_only,
        low_level_seek_reason,
        session_id,
        vo_queue,
        demux_cache,
        pipeline,
        selected_tracks: None,
        control,
    })?;
    service_playback_position_state_reset(PlaybackPositionStateResetContext {
        position_seconds,
        session_id,
        pipeline,
        emit_playback_buffered_events,
        control,
        event_tx,
    });
    if pipeline.video_stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && matches!(flush_result.demux_seek_result, DemuxSeekResult::Requested)
    {
        let reason = low_level_seek_reason.unwrap_or("cached_seek_miss");
        let armed = pipeline
            .video_decode_pipeline
            .begin_hevc_low_level_seek_observation(
                recovery_transaction_id,
                target_nsecs,
                seek_position_nsecs,
                reason,
            );
        tracing::debug!(
            ?session_id,
            transaction_id = recovery_transaction_id,
            recovery_scope = "exact_low_level_seek",
            reason,
            target_nsecs,
            seek_position_nsecs,
            armed,
            "armed first-recovery observation for HEVC low-level seek"
        );
    }
    if let DemuxSeekResult::Cached(info) = flush_result.demux_seek_result
        && matches!(seek_mode, PlaybackSeekMode::Precise)
    {
        pipeline
            .video_decode_recovery
            .enable_hevc_cached_recovery_point(recovery_transaction_id, target_nsecs);
        tracing::debug!(
            ?session_id,
            transaction_id = recovery_transaction_id,
            recovery_scope = "exact_cached_seek",
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            anchor_nsecs = info.anchor_nsecs,
            target_nsecs = info.target_nsecs,
            preroll_nsecs = info.preroll_nsecs,
            exact_output_gate = true,
            "enabled exact HEVC recovery point for closed cached seek transaction"
        );
    }
    if hevc_seek_starts_video_bootstrap(
        pipeline.video_stream.codec_id,
        force_low_level_seek,
        low_level_seek_reason,
        flush_result.demux_seek_result,
    ) {
        pipeline.output_scheduler.begin_video_bootstrap_after_seek(
            session_id,
            low_level_seek_reason.unwrap_or("hevc_low_level_seek_recovery"),
        );
    }
    if let DemuxSeekResult::Cached(info) = flush_result.demux_seek_result
        && !force_low_level_seek
        && (matches!(seek_mode, PlaybackSeekMode::Precise) || info.uses_cra_anchor())
    {
        pipeline.begin_cached_seek_recovery_watchdog_for_hit(info, session_id);
    } else {
        pipeline.clear_cached_seek_recovery_watchdog();
    }
    if buffering_policy.emits_buffering_start() {
        let _ = event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::Buffering(true),
        ));
    } else {
        tracing::debug!(
            session_id = ?session_id,
            position_seconds,
            seek_mode = ?seek_mode,
            force_low_level_seek,
            cache_only,
            low_level_seek_reason,
            "suppressed user-visible buffering for internal playback repair"
        );
    }
    Ok(flush_result.demux_seek_result)
}

#[cfg(test)]
mod tests {
    use super::PlaybackSeekBufferingPolicy;

    #[test]
    fn internal_repair_can_preserve_visible_frame_without_buffering_event() {
        assert!(!PlaybackSeekBufferingPolicy::PreserveVisibleFrame.emits_buffering_start());
        assert!(PlaybackSeekBufferingPolicy::Emit.emits_buffering_start());
    }
}
