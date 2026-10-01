use super::{PlaybackTrack, PlaybackTrackSelection};
use serde::{Deserialize, Serialize};

/// Keep the requested source identity even if PlaybackInfo resolves another item ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackTrackPreferenceKey {
    pub item_id: String,
    pub media_source_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum SavedTrackChoice {
    Off,
    Track {
        stream_index: usize,
        label: String,
        codec: Option<String>,
        is_external: bool,
    },
}

impl SavedTrackChoice {
    pub(crate) fn from_track(track: Option<&PlaybackTrack>) -> Self {
        match track {
            Some(track) => Self::Track {
                stream_index: track.stream_index,
                label: track.label.to_string(),
                codec: track.codec.clone(),
                is_external: track.is_external,
            },
            None => Self::Off,
        }
    }

    /// None means the saved track disappeared; Some(None) explicitly disables it.
    pub(crate) fn resolve<'a>(
        &self,
        tracks: &'a [PlaybackTrack],
    ) -> Option<Option<&'a PlaybackTrack>> {
        let Self::Track {
            stream_index,
            label,
            codec,
            is_external,
        } = self
        else {
            return Some(None);
        };
        let matches = |track: &&PlaybackTrack| {
            track.label.as_str() == label
                && track.codec == *codec
                && track.is_external == *is_external
        };
        if let Some(track) = tracks
            .iter()
            .filter(matches)
            .find(|track| track.stream_index == *stream_index)
        {
            return Some(Some(track));
        }
        // A rescan can renumber streams. Prefer an unambiguous metadata match
        // over an index now occupied by another track.
        let mut candidates = tracks.iter().filter(matches);
        if let Some(track) = candidates.next() {
            return candidates.next().is_none().then_some(Some(track));
        }
        // Choices are scoped to an account, item and media source. Embedded
        // tracks use different labels/codecs in Emby and FFmpeg, so a metadata
        // mismatch alone does not invalidate the saved stream index. External
        // subtitle entries can be replaced independently of the media file.
        if *is_external {
            return None;
        }
        tracks
            .iter()
            .find(|track| !track.is_external && track.stream_index == *stream_index)
            .map(Some)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SavedTrackChoices {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) audio: Option<SavedTrackChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subtitle: Option<SavedTrackChoice>,
}

impl SavedTrackChoices {
    pub(crate) fn fill_missing(&mut self, other: &Self) {
        if self.audio.is_none() {
            self.audio = other.audio.clone();
        }
        if self.subtitle.is_none() {
            self.subtitle = other.subtitle.clone();
        }
    }

    pub(crate) fn apply(
        &self,
        audio: &[PlaybackTrack],
        subtitles: &[PlaybackTrack],
        selection: &mut PlaybackTrackSelection,
    ) {
        if let Some(track) = self.audio.as_ref().and_then(|choice| choice.resolve(audio)) {
            selection.audio_stream_index = track.map(|track| track.stream_index);
        }
        if let Some(track) = self
            .subtitle
            .as_ref()
            .and_then(|choice| choice.resolve(subtitles))
        {
            // External URLs are rebuilt from the current source, never persisted.
            selection.set_subtitle_track(track);
        }
    }
}
