use super::playback_pipeline_state::PlaybackPipelineState;
use std::sync::mpsc::Sender;

use crate::player::{
    backend::{BackendEvent, BackendEventKind, PlaybackSeekMode},
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::demux_cache::DemuxSeekResult;
use super::{BufferedReporter, DemuxPacketCache, FfmpegControl, reset_playback_timeline_state};

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

pub(super) struct PlaybackSeekResetContext<'a> {
    pub(super) position_seconds: f64,
    pub(super) seek_mode: PlaybackSeekMode,
    pub(super) seek_generation: u64,
    pub(super) session_id: PlaybackSessionId,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) emit_playback_buffered_events: bool,
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
    let demux_seek_result = context.demux_cache.seek(
        context.position_seconds,
        context.seek_mode,
        context.session_id,
        context.seek_generation,
    );
    tracing::debug!(
        session_id = ?context.session_id,
        reset_kind = context.kind.as_str(),
        position_seconds = context.position_seconds,
        seek_mode = ?context.seek_mode,
        seek_generation = context.seek_generation,
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

pub(super) fn service_playback_generation_seek(
    context: PlaybackGenerationFlushContext<'_>,
) -> std::result::Result<(), String> {
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
    Ok(())
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
) -> std::result::Result<(), String> {
    let PlaybackSeekResetContext {
        position_seconds,
        seek_mode,
        seek_generation,
        session_id,
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        control,
        event_tx,
    } = context;
    service_playback_generation_seek(PlaybackGenerationFlushContext {
        kind: PlaybackPositionResetKind::Seek,
        position_seconds,
        seek_mode,
        seek_generation,
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
    let _ = event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::Buffering(true),
    ));
    Ok(())
}
