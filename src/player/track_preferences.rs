use std::collections::HashMap;

use gpui::{App, BorrowAppContext as _, Global};
use serde::{Deserialize, Serialize};

use crate::server::CachedServer;

use super::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection};

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
            track.label.as_ref() == label
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
        // A rescan can renumber streams. Only restore an unambiguous match;
        // never reuse an index that now identifies a different track.
        let mut candidates = tracks.iter().filter(matches);
        let track = candidates.next()?;
        candidates.next().is_none().then_some(Some(track))
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlaybackTrackPreferences {
    servers: HashMap<String, ServerTrackPreferences>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerTrackPreferences {
    remote_server_id: Option<String>,
    user_id: Option<String>,
    items: HashMap<String, HashMap<String, SavedTrackChoices>>,
}

impl ServerTrackPreferences {
    fn matches(&self, server: &CachedServer) -> bool {
        self.remote_server_id == server.server_id && self.user_id == server.user_id
    }
}

impl Global for PlaybackTrackPreferences {}

impl PlaybackTrackPreferences {
    pub(crate) fn for_server(
        &self,
        server: &CachedServer,
    ) -> Option<&HashMap<String, HashMap<String, SavedTrackChoices>>> {
        self.servers
            .get(&server.id)
            .filter(|account| account.matches(server))
            .map(|account| &account.items)
    }

    pub(crate) fn restore(
        server: &CachedServer,
        entries: impl IntoIterator<Item = (PlaybackTrackPreferenceKey, SavedTrackChoices)>,
        cx: &mut App,
    ) {
        let mut preferences = cx.try_global::<Self>().cloned().unwrap_or_default();
        for (key, saved) in entries {
            let mut current = preferences
                .lookup(server, &key)
                .cloned()
                .unwrap_or_default();
            current.fill_missing(&saved);
            for (kind, choice) in [
                (PlaybackTrackKind::Audio, current.audio),
                (PlaybackTrackKind::Subtitle, current.subtitle),
            ] {
                if let Some(choice) = choice {
                    preferences.store(server, &key, kind, choice);
                }
            }
        }
        if cx.try_global::<Self>() != Some(&preferences) {
            cx.set_global(preferences);
        }
    }

    pub(crate) fn get(
        server: &CachedServer,
        key: &PlaybackTrackPreferenceKey,
        cx: &App,
    ) -> SavedTrackChoices {
        cx.try_global::<Self>()
            .and_then(|preferences| preferences.lookup(server, key))
            .cloned()
            .unwrap_or_default()
    }

    fn lookup(
        &self,
        server: &CachedServer,
        key: &PlaybackTrackPreferenceKey,
    ) -> Option<&SavedTrackChoices> {
        self.servers
            .get(&server.id)
            .filter(|account| account.matches(server))?
            .items
            .get(&key.item_id)?
            .get(&key.media_source_id)
    }

    pub(crate) fn remember(
        server: &CachedServer,
        keys: &[PlaybackTrackPreferenceKey],
        kind: PlaybackTrackKind,
        track: Option<&PlaybackTrack>,
        cx: &mut App,
    ) {
        let choice = SavedTrackChoice::from_track(track);
        if keys.iter().all(|key| {
            let saved = Self::get(server, key, cx);
            (match kind {
                PlaybackTrackKind::Audio => saved.audio.as_ref(),
                PlaybackTrackKind::Subtitle => saved.subtitle.as_ref(),
            }) == Some(&choice)
        }) {
            return;
        }
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        cx.update_global::<Self, _>(|preferences, _| {
            for key in keys {
                preferences.store(server, key, kind, choice.clone());
            }
        });
        cx.refresh_windows();
    }

    fn store(
        &mut self,
        server: &CachedServer,
        key: &PlaybackTrackPreferenceKey,
        kind: PlaybackTrackKind,
        choice: SavedTrackChoice,
    ) {
        if key.item_id.trim().is_empty() || key.media_source_id.trim().is_empty() {
            return;
        }
        let account =
            self.servers
                .entry(server.id.clone())
                .or_insert_with(|| ServerTrackPreferences {
                    remote_server_id: server.server_id.clone(),
                    user_id: server.user_id.clone(),
                    items: HashMap::new(),
                });
        if !account.matches(server) {
            *account = ServerTrackPreferences {
                remote_server_id: server.server_id.clone(),
                user_id: server.user_id.clone(),
                items: HashMap::new(),
            };
        }
        let saved = account
            .items
            .entry(key.item_id.clone())
            .or_default()
            .entry(key.media_source_id.clone())
            .or_default();
        match kind {
            PlaybackTrackKind::Audio => saved.audio = Some(choice),
            PlaybackTrackKind::Subtitle => saved.subtitle = Some(choice),
        }
    }
}

#[cfg(test)]
#[path = "track_preferences_tests.rs"]
mod tests;
