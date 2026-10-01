use gpui::App;

use crate::{media::SavedTrackChoices, settings::binding::PlaybackTrackPreferences};

use super::HomeContent;

/// GPUI preference adapter. The model merges the detail-local draft purely;
/// resource bindings and read models never look up application globals.
pub(super) fn detail_track_choices(
    model: &super::detail::model::SeriesDetailModel,
    server: &crate::server::CachedServer,
    cx: &App,
) -> SavedTrackChoices {
    let saved = model
        .track_preference_key()
        .map(|key| PlaybackTrackPreferences::get(server, &key, cx))
        .unwrap_or_default();
    model.selected_track_choices(saved)
}

impl HomeContent {
    pub(super) fn sync_track_preferences(&mut self, cx: &App) -> bool {
        let Some(items) = cx
            .try_global::<PlaybackTrackPreferences>()
            .and_then(|preferences| preferences.for_server(&self.current_server))
        else {
            return false;
        };
        self.controller.merge_track_preferences(items)
    }

    pub(super) fn restore_track_preferences(&self, cx: &mut App) {
        let entries = self.controller.track_preferences();
        PlaybackTrackPreferences::restore(&self.current_server, entries, cx);
    }
}

#[cfg(test)]
mod tests;
