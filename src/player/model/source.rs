//! Media metadata and selected tracks owned by the playback session. Backend
//! results update this model; UI dropdown visibility stays in presentation.
use crate::player::PlaybackTrackPreferenceKey;
use tiny_playback::{
    PlaybackAudioInfo, PlaybackFileInfo, PlaybackTrack, PlaybackTrackSelection, PlaybackVideoInfo,
};

pub(in crate::player) struct PlaybackSourceState {
    pub(in crate::player) source_protocol: Option<String>,
    pub(in crate::player) source_url: String,
    pub(in crate::player) content_length: Option<u64>,
    pub(in crate::player) playback_file_info: Option<PlaybackFileInfo>,
    pub(in crate::player) playback_info: Option<PlaybackVideoInfo>,
    pub(in crate::player) playback_audio_info: Option<PlaybackAudioInfo>,
    pub(in crate::player) tracks: PlaybackTrackState,
    pub(in crate::player) track_preference_key: PlaybackTrackPreferenceKey,
    pub(in crate::player) remember_subtitle_on_start: bool,
}

impl PlaybackSourceState {
    pub(in crate::player) fn apply_tracks(
        &mut self,
        audio: Vec<PlaybackTrack>,
        mut subtitles: Vec<PlaybackTrack>,
        selected: PlaybackTrackSelection,
    ) {
        // A backend fallback must not overwrite an explicit detail preference.
        if selected.subtitle_stream_index != self.tracks.selected_subtitle_stream_index {
            self.remember_subtitle_on_start = false;
        }
        subtitles.extend(
            self.tracks
                .subtitles
                .iter()
                .filter(|track| track.is_external)
                .cloned(),
        );
        self.tracks = PlaybackTrackState::new(audio, subtitles, selected);
    }
    pub(in crate::player) fn clear_metadata(&mut self) {
        self.playback_file_info = None;
        self.playback_info = None;
        self.playback_audio_info = None;
    }
}

pub(in crate::player) struct PlaybackTrackState {
    pub(in crate::player) audio: Vec<PlaybackTrack>,
    pub(in crate::player) subtitles: Vec<PlaybackTrack>,
    pub(in crate::player) selected_audio_stream_index: Option<usize>,
    pub(in crate::player) selected_subtitle_stream_index: Option<usize>,
}

impl PlaybackTrackState {
    pub(in crate::player) fn new(
        audio: Vec<PlaybackTrack>,
        subtitles: Vec<PlaybackTrack>,
        selected: PlaybackTrackSelection,
    ) -> Self {
        Self {
            audio,
            subtitles,
            selected_audio_stream_index: selected.audio_stream_index,
            selected_subtitle_stream_index: selected.subtitle_stream_index,
        }
    }
}

pub(in crate::player) fn playback_protocol(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|url| url.scheme().trim().to_ascii_lowercase())
        .filter(|protocol| !protocol.is_empty())
}
