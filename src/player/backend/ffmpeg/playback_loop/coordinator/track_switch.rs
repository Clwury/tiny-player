use super::*;

pub(super) struct TrackSwitchPipelineState {
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) audio_output: Option<AudioOutput>,
    pub(super) audio_decode_pipeline: Option<AudioDecodePipeline>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_track_switch_pipelines(
    source: &mut FfmpegPlaybackInput,
    selected_tracks: crate::player::PlaybackTrackSelection,
    stream_catalog: &StreamCatalog,
    previous_audio_output: Option<AudioOutput>,
    control: Arc<FfmpegControl>,
    video_size: Option<RenderSize>,
    current_start_position_nsecs: u64,
    subtitle_pipeline: &mut SubtitlePipeline,
) -> std::result::Result<TrackSwitchPipelineState, String> {
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

    subtitle_pipeline.switch_tracks(
        source,
        stream_catalog,
        video_size,
        current_start_position_nsecs,
    )?;

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
    let Some(output) = reuse_or_create_audio_output(previous_audio_output, control) else {
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
