use std::sync::{
    Arc,
    mpsc::{Receiver, Sender},
};
use std::time::Duration;

use ffmpeg_sys_next as ffi;

use crate::player::{
    PlaybackTrackSelection,
    backend::{BackendEvent, BackendEventKind, PlaybackCacheConfig, PlaybackSeekMode},
    render_host::VideoOutputQueue,
};

use super::commands::{begin_seek, begin_track_switch};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::playback_reset_service::{
    PlaybackGenerationFlushContext, PlaybackPositionResetKind, PlaybackPositionStateResetContext,
    PlaybackSeekBufferingPolicy, PlaybackSeekResetContext, service_playback_generation_seek,
    service_playback_position_state_reset, service_playback_seek_reset,
};
use super::track_switch::{TrackSwitchPipelineState, service_track_switch_pipelines};
use super::{
    DemuxPacketCache, FfmpegCommand, FfmpegControl, FfmpegPlaybackInput, HttpRingCache,
    PlaybackSession, StreamCatalog, coalesce_playback_seek_commands, drain_playback_commands,
    playback_audio_info_from_stream, select_audio_stream_for_selection_from_catalog,
    select_subtitle_stream_for_selection_from_catalog, should_cache_http_url,
};
use crate::player::backend::ffmpeg::worker::{PendingSeek, PendingTrackSelection};

const HEVC_CONTINUOUS_SEEK_COALESCE_QUIET_PERIOD: Duration = Duration::from_millis(180);
const SUBTITLE_TRACK_CHANGE_LOW_LEVEL_SEEK_REASON: &str = "internal_subtitle_track_change";

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

fn pending_seek_is_latest_generation(pending_seek: &PendingSeek, control: &FfmpegControl) -> bool {
    pending_seek.generation == control.seek_generation()
}

fn log_superseded_seek(
    pending_seek: &PendingSeek,
    control: &FfmpegControl,
    checkpoint: &'static str,
) {
    tracing::debug!(
        session_id = ?pending_seek.session_id,
        position_seconds = pending_seek.position_seconds,
        seek_mode = ?pending_seek.mode,
        seek_generation = pending_seek.generation,
        latest_seek_generation = control.seek_generation(),
        checkpoint,
        "skipping superseded FFmpeg seek before playback reset"
    );
}

pub(super) fn service_playback_commands(
    mut context: PlaybackCommandContext<'_>,
) -> std::result::Result<PlaybackCommandServiceStatus, String> {
    let mut drained_commands = drain_playback_commands(context.command_rx, context.control);
    if context.pipeline.video_stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        && drained_commands.pending_seek.is_some()
    {
        let initial_seek_generation = drained_commands
            .pending_seek
            .map(|pending| pending.generation);
        drained_commands = coalesce_playback_seek_commands(
            context.command_rx,
            context.control,
            drained_commands,
            HEVC_CONTINUOUS_SEEK_COALESCE_QUIET_PERIOD,
        );
        let final_seek = drained_commands.pending_seek;
        if final_seek.map(|pending| pending.generation) != initial_seek_generation {
            tracing::debug!(
                initial_seek_generation,
                final_seek_generation = final_seek.map(|pending| pending.generation),
                final_position_seconds = final_seek.map(|pending| pending.position_seconds),
                quiet_period_ms = HEVC_CONTINUOUS_SEEK_COALESCE_QUIET_PERIOD.as_secs_f64() * 1000.0,
                "coalesced continuous HEVC seek commands before playback reset"
            );
        }
    }
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
        context
            .pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_recovery_transaction();
        context.pipeline.clear_cached_seek_recovery_watchdog();
        let position_seconds =
            begin_track_switch(context.session, context.control, &pending_track_selection);
        context.pipeline.clear_audio_realign_transaction();
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
        if !pending_seek_is_latest_generation(&pending_seek, context.control) {
            log_superseded_seek(&pending_seek, context.control, "before_begin_seek");
            return Ok(PlaybackCommandServiceStatus::Continue);
        }
        context
            .pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_recovery_transaction();
        context.pipeline.clear_cached_seek_recovery_watchdog();
        let position_seconds = begin_seek(context.session, context.control, &pending_seek);
        context.pipeline.current_start_position_nsecs = context.session.start_position_nsecs();
        context.pipeline.clear_audio_realign_transaction();
        if !pending_seek_is_latest_generation(&pending_seek, context.control) {
            log_superseded_seek(&pending_seek, context.control, "before_seek_reset");
            return Ok(PlaybackCommandServiceStatus::Continue);
        }
        service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: pending_seek.mode,
            seek_generation: pending_seek.generation,
            force_low_level_seek: false,
            cache_only: false,
            recovery_transaction_id: None,
            low_level_seek_reason: None,
            session_id: context.session.id(),
            vo_queue: context.vo_queue,
            demux_cache: context.demux_cache,
            pipeline: context.pipeline,
            emit_playback_buffered_events: context.emit_playback_buffered_events,
            buffering_policy: PlaybackSeekBufferingPolicy::Emit,
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
    let low_level_seek_reason =
        track_selection_low_level_seek_reason(&context.source.selected_tracks, &selected_tracks);
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
        force_low_level_seek: low_level_seek_reason.is_some(),
        cache_only: false,
        low_level_seek_reason,
        session_id: context.session.id(),
        vo_queue: context.vo_queue,
        demux_cache: context.demux_cache,
        pipeline: context.pipeline,
        selected_tracks: Some(&selected_tracks),
        control: context.control,
    })?;
    context.pipeline.clear_cached_seek_recovery_watchdog();

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
    let playback_audio_info = playback_audio_info_from_stream(
        context.pipeline.audio_stream,
        context.pipeline.audio_output.as_ref(),
    );
    let _ = context.event_tx.send(BackendEvent::new(
        context.session.id(),
        BackendEventKind::PlaybackAudioInfoChanged(playback_audio_info),
    ));

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

fn track_selection_low_level_seek_reason(
    previous: &PlaybackTrackSelection,
    next: &PlaybackTrackSelection,
) -> Option<&'static str> {
    let previous_internal_subtitle = internal_subtitle_stream_index(previous);
    let next_internal_subtitle = internal_subtitle_stream_index(next);

    // The demux cache only indexes the currently selected subtitle stream. A cached seek after
    // selecting another internal stream therefore has no historical packets for the new track and
    // would resume that stream from the demuxer's already-prefetched append position instead.
    (next_internal_subtitle.is_some() && next_internal_subtitle != previous_internal_subtitle)
        .then_some(SUBTITLE_TRACK_CHANGE_LOW_LEVEL_SEEK_REASON)
}

fn internal_subtitle_stream_index(selection: &PlaybackTrackSelection) -> Option<usize> {
    if selection.subtitle_external_url.is_some() {
        None
    } else {
        selection.subtitle_stream_index
    }
}

#[cfg(test)]
mod tests {
    use crate::player::{
        PlaybackTrackSelection, backend::PlaybackSeekMode, render_host::PlaybackSessionId,
    };

    use super::{
        FfmpegControl, HEVC_CONTINUOUS_SEEK_COALESCE_QUIET_PERIOD, PendingSeek,
        SUBTITLE_TRACK_CHANGE_LOW_LEVEL_SEEK_REASON, pending_seek_is_latest_generation,
        track_selection_low_level_seek_reason,
    };

    fn pending_seek(generation: u64, position_seconds: f64) -> PendingSeek {
        PendingSeek {
            session_id: PlaybackSessionId(1),
            position_seconds,
            mode: PlaybackSeekMode::Fast,
            generation,
        }
    }

    #[test]
    fn continuous_seek_generations_only_reset_latest_target() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        let first = pending_seek(control.request_seek(), 75.0);
        assert!(pending_seek_is_latest_generation(&first, &control));

        let second = pending_seek(control.request_seek(), 77.0);
        let latest = pending_seek(control.request_seek(), 79.0);

        assert!(!pending_seek_is_latest_generation(&first, &control));
        assert!(!pending_seek_is_latest_generation(&second, &control));
        assert!(pending_seek_is_latest_generation(&latest, &control));
        assert_eq!(latest.position_seconds, 79.0);
    }

    #[test]
    fn hevc_seek_coalesce_window_covers_keyboard_repeat_interval() {
        assert!(
            HEVC_CONTINUOUS_SEEK_COALESCE_QUIET_PERIOD >= std::time::Duration::from_millis(180)
        );
    }

    #[test]
    fn changing_internal_subtitle_track_forces_low_level_seek() {
        let previous = PlaybackTrackSelection {
            subtitle_stream_index: Some(2),
            subtitle_codec: Some("subrip".to_string()),
            ..PlaybackTrackSelection::default()
        };
        let next = PlaybackTrackSelection {
            subtitle_stream_index: Some(4),
            subtitle_codec: Some("subrip".to_string()),
            ..PlaybackTrackSelection::default()
        };

        assert_eq!(
            track_selection_low_level_seek_reason(&previous, &next),
            Some(SUBTITLE_TRACK_CHANGE_LOW_LEVEL_SEEK_REASON)
        );
    }

    #[test]
    fn enabling_internal_subtitle_track_forces_low_level_seek() {
        let previous = PlaybackTrackSelection::default();
        let next = PlaybackTrackSelection {
            subtitle_stream_index: Some(4),
            subtitle_codec: Some("subrip".to_string()),
            ..PlaybackTrackSelection::default()
        };

        assert_eq!(
            track_selection_low_level_seek_reason(&previous, &next),
            Some(SUBTITLE_TRACK_CHANGE_LOW_LEVEL_SEEK_REASON)
        );
    }

    #[test]
    fn audio_only_or_external_subtitle_changes_keep_cached_seek() {
        let previous = PlaybackTrackSelection {
            audio_stream_index: Some(1),
            subtitle_stream_index: Some(4),
            subtitle_codec: Some("subrip".to_string()),
            ..PlaybackTrackSelection::default()
        };
        let audio_change = PlaybackTrackSelection {
            audio_stream_index: Some(5),
            ..previous.clone()
        };
        let external_subtitle = PlaybackTrackSelection {
            subtitle_stream_index: Some(7),
            subtitle_external_url: Some("https://example.com/subtitle.ass".to_string()),
            subtitle_codec: Some("ass".to_string()),
            ..previous.clone()
        };
        let subtitles_off = PlaybackTrackSelection {
            subtitle_stream_index: None,
            subtitle_external_url: None,
            subtitle_codec: None,
            ..previous.clone()
        };

        assert_eq!(
            track_selection_low_level_seek_reason(&previous, &audio_change),
            None
        );
        assert_eq!(
            track_selection_low_level_seek_reason(&previous, &external_subtitle),
            None
        );
        assert_eq!(
            track_selection_low_level_seek_reason(&previous, &subtitles_off),
            None
        );
    }
}
