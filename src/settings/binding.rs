//! GPUI global binding for shared media language preferences.
use crate::media::PlaybackLanguagePreferences;
use gpui::{App, Global};

impl Global for PlaybackLanguagePreferences {}

impl PlaybackLanguagePreferences {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn apply(self, cx: &mut App) {
        if cx.try_global::<Self>() != Some(&self) {
            cx.set_global(self);
            cx.refresh_windows();
        }
    }
}

mod track_preferences;
pub(crate) use track_preferences::PlaybackTrackPreferences;
