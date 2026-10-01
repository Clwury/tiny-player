use std::collections::HashMap;

use gpui::{App, BorrowAppContext as _, Global};

use crate::server::CachedServer;

#[cfg(test)]
use crate::media::PlaybackTrackSelection;
use crate::media::{
    PlaybackTrack, PlaybackTrackKind, PlaybackTrackPreferenceKey, SavedTrackChoice,
    SavedTrackChoices,
};

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
mod tests;
