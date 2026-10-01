use super::HomeController;
use crate::home::cache::HomeSnapshot;
use crate::media::{PlaybackTrackPreferenceKey, SavedTrackChoices};
use std::collections::HashMap;

impl HomeController {
    pub(in crate::home) fn merge_track_preferences(
        &mut self,
        items: &HashMap<String, HashMap<String, SavedTrackChoices>>,
    ) -> bool {
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

    pub(in crate::home) fn track_preferences(
        &self,
    ) -> impl Iterator<Item = (PlaybackTrackPreferenceKey, SavedTrackChoices)> + '_ {
        self.played_video_versions
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
            })
    }

    pub(in crate::home) fn hydrate_snapshot(&mut self, mut snapshot: HomeSnapshot) {
        self.feed.hydrate_sections(&mut snapshot);
        for (item_id, version) in snapshot.played_video_versions {
            match self.played_video_versions.entry(item_id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(version);
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    if current.source_id.is_empty() {
                        current.source_id = version.source_id;
                        current.name = version.name;
                    }
                    for (source_id, saved) in version.track_preferences {
                        current
                            .track_preferences
                            .entry(source_id)
                            .or_default()
                            .fill_missing(&saved);
                    }
                }
            }
        }
    }

    pub(in crate::home) fn snapshot(&self, saved_at_unix: u64) -> HomeSnapshot {
        let latest_items_by_view = self
            .feed
            .state
            .user_view_items_rows
            .iter()
            .filter_map(|(view_id, row)| {
                row.items
                    .as_ref()
                    .cloned()
                    .map(|mut items| {
                        for item in &mut items.items {
                            if let Some(data) = self.user_data.overrides.get(&item.id) {
                                item.user_data = Some(data.clone());
                            }
                        }
                        items
                    })
                    .map(|items| (view_id.clone(), items))
            })
            .collect::<HashMap<_, _>>();

        let mut resume_items = self.feed.state.resume_items.clone();
        if let Some(items) = resume_items.as_mut() {
            for item in &mut items.items {
                if let Some(data) = self.user_data.overrides.get(&item.id) {
                    item.user_data = Some(data.clone());
                }
            }
        }
        let mut snapshot = HomeSnapshot::from_data(
            &self.identity,
            saved_at_unix,
            self.feed.state.user_views.clone(),
            resume_items,
            latest_items_by_view,
        );
        snapshot.played_video_versions = self.played_video_versions.clone();
        snapshot
    }
}
