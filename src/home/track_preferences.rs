use gpui::App;

use crate::player::{PlaybackTrackPreferenceKey, PlaybackTrackPreferences};

use super::HomeContent;

impl HomeContent {
    pub(super) fn sync_track_preferences(&mut self, cx: &App) -> bool {
        let Some(items) = cx
            .try_global::<PlaybackTrackPreferences>()
            .and_then(|preferences| preferences.for_server(&self.current_server))
        else {
            return false;
        };
        let mut changed = false;
        for (item_id, sources) in items {
            let version = self
                .played_video_versions
                .entry(item_id.clone())
                .or_default();
            for (source_id, saved) in sources {
                let current = version
                    .track_preferences
                    .entry(source_id.clone())
                    .or_default();
                // Explicit in-memory choices take priority over a late snapshot load.
                let mut merged = saved.clone();
                merged.fill_missing(current);
                if *current != merged {
                    *current = merged;
                    changed = true;
                }
            }
        }
        changed
    }

    pub(super) fn restore_track_preferences(&self, cx: &mut App) {
        let entries = self
            .played_video_versions
            .iter()
            .flat_map(|(item_id, version)| {
                version
                    .track_preferences
                    .iter()
                    .map(move |(source_id, saved)| {
                        (
                            PlaybackTrackPreferenceKey {
                                item_id: item_id.clone(),
                                media_source_id: source_id.clone(),
                            },
                            saved.clone(),
                        )
                    })
            });
        PlaybackTrackPreferences::restore(&self.current_server, entries, cx);
    }
}

#[cfg(test)]
#[path = "track_preferences_tests.rs"]
mod tests;
