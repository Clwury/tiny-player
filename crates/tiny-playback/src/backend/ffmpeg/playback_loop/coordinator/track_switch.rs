use std::sync::Arc;

use crate::{
    PlaybackTrackSelection,
    backend::{BackendEvent, BackendEventKind},
    render_host::RenderSize,
};

use super::coordinator_commands::PlaybackCommandContext;

use super::{
    AudioDecodePipeline, AudioOutput, FfmpegControl, FfmpegPlaybackInput, StreamCatalog,
    StreamInfo, SubtitlePipeline, open_audio_decoder,
    select_audio_stream_for_selection_from_catalog,
    select_subtitle_stream_for_selection_from_catalog,
};

pub(super) fn service_audio_disable_command(
    context: &mut PlaybackCommandContext<'_>,
) -> std::result::Result<(), String> {
    let subtitle_stream = select_subtitle_stream_for_selection_from_catalog(
        &context.source.selected_tracks,
        context.stream_catalog,
    )?;
    context.source.selected_tracks.audio_stream_index = None;
    // Confirm selection in this same session, including when Off was queued
    // before the initial resolved-track event reached the UI.
    let _ = context.event_tx.send(BackendEvent::new(
        context.session.id(),
        BackendEventKind::PlaybackTracksChanged {
            audio: context
                .stream_catalog
                .tracks(ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_AUDIO),
            subtitles: context
                .stream_catalog
                .tracks(ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_SUBTITLE),
            selected: context.source.selected_tracks.clone(),
        },
    ));
    if context.pipeline.audio_stream.is_none() && context.pipeline.audio_output.is_none() {
        return Ok(());
    }

    // Anchor to the output clock, never the decoder's prefetched timestamp or
    // the UI's older position report. Keep video, subtitles, and reader heads.
    let timeline_nsecs = context
        .pipeline
        .audio_output
        .as_ref()
        .and_then(|output| output.clock_handle().played_timeline_nsecs())
        .or_else(|| {
            context
                .vo_queue
                .snapshot()
                .last_presentation
                .map(|frame| frame.timeline_nsecs)
        })
        .unwrap_or_else(|| context.pipeline.scheduler.current_timeline_nsecs());
    context.pipeline.output_scheduler.disable_audio(
        &mut context.pipeline.scheduler,
        timeline_nsecs,
        context.control,
    );
    context.pipeline.clear_audio_realign_transaction();
    context.pipeline.audio_decode_pipeline = None;
    context.pipeline.audio_output = None;
    context.pipeline.audio_stream = None;
    context.pipeline.buffered_reporter.disable_audio();
    context
        .demux_cache
        .set_selected_streams(None, subtitle_stream);
    context.control.finish_seek_audio_pause();
    let session_id = context.session.id();
    let _ = context.event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::PlaybackAudioInfoChanged(None),
    ));
    if !context.pipeline.output_scheduler.restart_pending() {
        context.demux_cache.clear_cache_pause_for_decoded_resume();
        let _ = context.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::Buffering(false),
        ));
    }
    tracing::debug!(
        ?session_id,
        timeline_nsecs,
        queued_video_frames = context
            .pipeline
            .output_scheduler
            .scheduled_video_queue
            .len(),
        "disabled FFmpeg audio without seeking or resetting video and subtitles"
    );
    Ok(())
}

pub(super) struct TrackSwitchPipelineState {
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) audio_output: Option<AudioOutput>,
    pub(super) audio_decode_pipeline: Option<AudioDecodePipeline>,
}

pub(super) fn service_subtitle_disable_command(context: &mut PlaybackCommandContext<'_>) {
    context.source.selected_tracks.set_subtitle_track(None);
    context
        .pipeline
        .subtitle_pipeline
        .disable(context.session.id(), context.event_tx);
    context
        .demux_cache
        .set_selected_streams(context.pipeline.audio_stream, None);
    let _ = context.event_tx.send(BackendEvent::new(
        context.session.id(),
        BackendEventKind::PlaybackTracksChanged {
            audio: context
                .stream_catalog
                .tracks(ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_AUDIO),
            subtitles: context
                .stream_catalog
                .tracks(ffmpeg_sys_next::AVMediaType::AVMEDIA_TYPE_SUBTITLE),
            selected: context.source.selected_tracks.clone(),
        },
    ));
    tracing::debug!(session_id = ?context.session.id(),
        "disabled FFmpeg subtitles without seeking or resetting audio and video");
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_track_switch_pipelines(
    source: &mut FfmpegPlaybackInput,
    selected_tracks: PlaybackTrackSelection,
    stream_catalog: &StreamCatalog,
    previous_audio_output: Option<AudioOutput>,
    control: Arc<FfmpegControl>,
    video_size: Option<RenderSize>,
    current_start_position_nsecs: u64,
    subtitle_pipeline: &mut SubtitlePipeline,
) -> std::result::Result<TrackSwitchPipelineState, String> {
    let subtitle_changed = source.selected_tracks.subtitle_stream_index
        != selected_tracks.subtitle_stream_index
        || source.selected_tracks.subtitle_external_url != selected_tracks.subtitle_external_url
        || source.selected_tracks.subtitle_codec != selected_tracks.subtitle_codec;
    source.selected_tracks = selected_tracks;

    let audio_stream = select_audio_stream_for_selection_from_catalog(
        &source.selected_tracks,
        stream_catalog,
        false,
    )?;
    let (audio_output, audio_decode_pipeline) = rebuild_audio_pipeline_for_track_switch(
        audio_stream,
        previous_audio_output,
        control,
        current_start_position_nsecs,
    )?;

    if subtitle_changed {
        subtitle_pipeline.switch_tracks(
            source,
            stream_catalog,
            video_size,
            current_start_position_nsecs,
        )?;
    }

    Ok(TrackSwitchPipelineState {
        audio_stream,
        audio_output,
        audio_decode_pipeline,
    })
}

fn rebuild_audio_pipeline_for_track_switch(
    audio_stream: Option<StreamInfo>,
    previous_audio_output: Option<AudioOutput>,
    control: Arc<FfmpegControl>,
    current_start_position_nsecs: u64,
) -> std::result::Result<(Option<AudioOutput>, Option<AudioDecodePipeline>), String> {
    let Some(decoder) = open_audio_decoder(audio_stream, false)? else {
        return Ok((None, None));
    };
    let Some(output) = reuse_or_create_audio_output(previous_audio_output, Arc::clone(&control))
    else {
        return Ok((None, None));
    };

    match AudioDecodePipeline::spawn(decoder, output.sample_rate(), output.channels()) {
        Ok(worker) => {
            output.reset_clock(current_start_position_nsecs);
            Ok((Some(output), Some(worker)))
        }
        Err(error) => {
            tracing::warn!(%error, "FFmpeg audio decode worker initialization failed");
            Ok((None, None))
        }
    }
}

fn reuse_or_create_audio_output(
    previous_audio_output: Option<AudioOutput>,
    control: Arc<FfmpegControl>,
) -> Option<AudioOutput> {
    if previous_audio_output.is_some() {
        return previous_audio_output;
    }
    match AudioOutput::new(control) {
        Ok(output) => Some(output),
        Err(error) => {
            tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::BackendSubtitleCue, render_host::PlaybackSessionId};

    #[test]
    fn audio_only_switch_reuses_loaded_external_subtitles() {
        let session_id = PlaybackSessionId(1);
        let selected_tracks = PlaybackTrackSelection {
            audio_stream_index: Some(1),
            subtitle_stream_index: Some(2),
            subtitle_external_url: Some("invalid://must-not-reload-subtitles".into()),
            subtitle_codec: Some("ass".into()),
            ..Default::default()
        };
        let mut source = FfmpegPlaybackInput {
            session_id,
            url: "file:///fixture.mkv".into(),
            http_headers: Vec::new(),
            content_length: None,
            start_position_seconds: 0.0,
            selected_tracks: selected_tracks.clone(),
            cache_config: Default::default(),
        };
        let cue = BackendSubtitleCue {
            text: "retained subtitle".into(),
            bitmaps: Vec::new(),
            start_nsecs: 0,
            end_nsecs: 10_000_000_000,
        };
        let mut subtitles = SubtitlePipeline::with_external_cues_for_test(vec![cue.clone()]);
        service_track_switch_pipelines(
            &mut source,
            PlaybackTrackSelection {
                audio_stream_index: None,
                ..selected_tracks
            },
            &StreamCatalog::empty_for_test(),
            None,
            Arc::new(FfmpegControl::new(session_id)),
            None,
            5_000_000_000,
            &mut subtitles,
        )
        .expect("audio-only switch must not fetch the subtitle URL again");
        let (tx, rx) = std::sync::mpsc::channel();
        subtitles.reset_cues_for_position(5_000_000_000);
        subtitles.update_overlay(5_000_000_000, session_id, &tx);
        assert!(matches!(rx.try_recv().unwrap().kind,
            BackendEventKind::SubtitleChanged(Some(retained)) if retained == cue));
    }
}
