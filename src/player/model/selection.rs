//! Deterministic source and stream selection, independent of GPUI and IO.
use crate::player::PlaybackTrackExt;
use tiny_playback::{PlaybackTrack, PlaybackTrackSelection};

pub(crate) fn preferred_playback_media_source(
    sources: &[crate::emby::MediaSource],
) -> Option<&crate::emby::MediaSource> {
    let valid = |source: &&crate::emby::MediaSource| {
        source.id.as_deref().is_some_and(|id| !id.trim().is_empty())
    };
    sources
        .iter()
        .filter(valid)
        .find(|source| source.is_default_source())
        .or_else(|| {
            sources
                .iter()
                .filter(valid)
                .find(|source| source.has_default_video_stream())
        })
        .or_else(|| sources.iter().find(valid))
}

pub(crate) fn playback_audio_tracks_for_source(
    source: &crate::emby::MediaSource,
) -> Vec<PlaybackTrack> {
    source
        .audio_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| PlaybackTrack::from_audio_stream(stream, index))
        .collect()
}

pub(crate) fn preferred_playback_track_selection(
    source: &crate::emby::MediaSource,
    subtitle_tracks: &[PlaybackTrack],
    languages: crate::player::PlaybackLanguagePreferences,
) -> PlaybackTrackSelection {
    let default_audio_stream_index = source.audio_streams().into_iter().find_map(|stream| {
        stream
            .index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|_| stream.is_default.unwrap_or(false))
    });
    let audio_stream_index = languages
        .audio
        .matching_audio_stream_index(source)
        .or(default_audio_stream_index)
        .or_else(|| {
            source
                .audio_streams()
                .into_iter()
                .find_map(|stream| stream.index.and_then(|index| usize::try_from(index).ok()))
        });
    let selected_subtitle = languages
        .subtitle
        .preferred_subtitle_stream_position(source)
        .and_then(|position| {
            playback_subtitle_track_at_position(source, subtitle_tracks, position)
        });

    let mut selection = PlaybackTrackSelection {
        audio_stream_index,
        default_audio_stream_index,
        ..Default::default()
    };
    selection.set_subtitle_track(selected_subtitle);
    selection
}

pub(crate) fn playback_subtitle_track_at_position<'a>(
    source: &crate::emby::MediaSource,
    subtitle_tracks: &'a [PlaybackTrack],
    position: usize,
) -> Option<&'a PlaybackTrack> {
    let stream_index = source
        .subtitle_streams()
        .get(position)?
        .index
        .and_then(|index| usize::try_from(index).ok())?;
    subtitle_tracks
        .iter()
        .find(|track| track.stream_index == stream_index)
}
