use super::commands::{begin_seek, begin_track_switch};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::playback_reset_service::{
    PlaybackGenerationFlushContext, PlaybackPositionResetKind, PlaybackPositionStateResetContext,
    PlaybackSeekResetContext, service_playback_generation_seek,
    service_playback_position_state_reset, service_playback_seek_reset,
};
use super::track_switch::{TrackSwitchPipelineState, service_track_switch_pipelines};
use super::*;
use crate::player::backend::ffmpeg::worker::PendingTrackSelection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackCommandServiceStatus {
    Idle,
    Continue,
    Stopped,
}

pub(super) struct PlaybackCommandContext<'a> {
    pub(super) source: &'a mut FfmpegPlaybackInput,
    pub(super) session: &'a mut PlaybackSession,
    pub(super) control: &'a Arc<FfmpegControl>,
    pub(super) command_rx: &'a Receiver<FfmpegCommand>,
    pub(super) http_cache: Option<&'a HttpRingCache>,
    pub(super) stream_catalog: &'a StreamCatalog,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) emit_playback_buffered_events: bool,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}

pub(super) fn service_playback_commands(
    mut context: PlaybackCommandContext<'_>,
) -> std::result::Result<PlaybackCommandServiceStatus, String> {
    let drained_commands = drain_playback_commands(context.command_rx, context.control);
    if context.control.should_stop() {
        return Ok(PlaybackCommandServiceStatus::Stopped);
    }

    if let Some(cache_config) = drained_commands.cache_config {
        apply_playback_cache_config(
            context.source,
            context.demux_cache,
            context.http_cache,
            cache_config,
        );
    }

    if let Some(pending_track_selection) = drained_commands.pending_track_selection {
        let position_seconds =
            begin_track_switch(context.session, context.control, &pending_track_selection);
        let switch_result = service_track_selection_command(
            &mut context,
            position_seconds,
            pending_track_selection,
        );
        if context.control.has_pending_seek() {
            return Ok(PlaybackCommandServiceStatus::Continue);
        }
        switch_result?;
        return Ok(PlaybackCommandServiceStatus::Continue);
    }

    if let Some(pending_seek) = drained_commands.pending_seek {
        let position_seconds = begin_seek(context.session, context.control, &pending_seek);
        context.pipeline.current_start_position_nsecs = context.session.start_position_nsecs();
        service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: pending_seek.mode,
            seek_generation: pending_seek.generation,
            session_id: context.session.id(),
            vo_queue: context.vo_queue,
            demux_cache: context.demux_cache,
            pipeline: context.pipeline,
            emit_playback_buffered_events: context.emit_playback_buffered_events,
            control: context.control,
            event_tx: context.event_tx,
        })?;
        return Ok(PlaybackCommandServiceStatus::Continue);
    }

    Ok(PlaybackCommandServiceStatus::Idle)
}

fn apply_playback_cache_config(
    source: &mut FfmpegPlaybackInput,
    demux_cache: &DemuxPacketCache,
    http_cache: Option<&HttpRingCache>,
    cache_config: PlaybackCacheConfig,
) {
    source.cache_config = cache_config.clone();
    let resolved_cache_config =
        cache_config.resolved_for_cacheable_input(should_cache_http_url(&source.url));
    demux_cache.apply_cache_config(resolved_cache_config.clone());
    if let Some(cache) = http_cache {
        cache.apply_cache_config(&resolved_cache_config);
    }
}

fn service_track_selection_command(
    context: &mut PlaybackCommandContext<'_>,
    position_seconds: f64,
    pending_track_selection: PendingTrackSelection,
) -> std::result::Result<(), String> {
    let selected_tracks = pending_track_selection.selected_tracks.clone();
    let demux_audio_stream = select_audio_stream_for_selection_from_catalog(
        &selected_tracks,
        context.stream_catalog,
        false,
    )?;
    let demux_subtitle_stream = select_subtitle_stream_for_selection_from_catalog(
        &selected_tracks,
        context.stream_catalog,
    )?;
    context
        .demux_cache
        .set_selected_streams(demux_audio_stream, demux_subtitle_stream);

    context.pipeline.current_start_position_nsecs = context.session.start_position_nsecs();
    service_playback_generation_seek(PlaybackGenerationFlushContext {
        kind: PlaybackPositionResetKind::TrackSelection,
        position_seconds,
        seek_mode: PlaybackSeekMode::Precise,
        seek_generation: pending_track_selection.generation,
        session_id: context.session.id(),
        vo_queue: context.vo_queue,
        demux_cache: context.demux_cache,
        pipeline: context.pipeline,
        selected_tracks: Some(&selected_tracks),
        control: context.control,
    })?;

    context.pipeline.audio_decode_pipeline = None;
    let track_switch_pipeline_state = service_track_switch_pipelines(
        context.source,
        selected_tracks,
        context.stream_catalog,
        context.pipeline.audio_output.take(),
        Arc::clone(context.control),
        context.pipeline.video_decode_pipeline.info().size,
        context.pipeline.current_start_position_nsecs,
        &mut context.pipeline.subtitle_pipeline,
    )?;
    let TrackSwitchPipelineState {
        audio_stream: next_audio_stream,
        audio_output: next_audio_output,
        audio_decode_pipeline: next_audio_decode_pipeline,
    } = track_switch_pipeline_state;
    context.pipeline.audio_stream = next_audio_stream;
    context.pipeline.audio_output = next_audio_output;
    context.pipeline.audio_decode_pipeline = next_audio_decode_pipeline;

    service_playback_position_state_reset(PlaybackPositionStateResetContext {
        position_seconds,
        session_id: context.session.id(),
        pipeline: context.pipeline,
        emit_playback_buffered_events: context.emit_playback_buffered_events,
        control: context.control,
        event_tx: context.event_tx,
    });

    if pending_track_selection.pause_after_switch {
        context.control.set_user_paused(true);
        context.pipeline.subtitle_pipeline.update_overlay(
            context.pipeline.current_start_position_nsecs,
            context.session.id(),
            context.event_tx,
        );
        let _ = context.event_tx.send(BackendEvent::new(
            context.session.id(),
            BackendEventKind::Pause(true),
        ));
        let _ = context.event_tx.send(BackendEvent::new(
            context.session.id(),
            BackendEventKind::Buffering(false),
        ));
    } else {
        let _ = context.event_tx.send(BackendEvent::new(
            context.session.id(),
            BackendEventKind::Buffering(true),
        ));
    }

    Ok(())
}
