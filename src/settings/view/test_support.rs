//! Construction fixtures for settings presentation tests.
use crate::{media::PlaybackLanguagePreferences, settings::SettingsSnapshot, theme};

pub(crate) fn settings_snapshot(
    config: &tiny_playback::PlaybackCacheConfig,
    cx: &gpui::App,
) -> SettingsSnapshot {
    SettingsSnapshot {
        playback: config.clone(),
        color_theme: theme::get(cx).selection,
        track_languages: PlaybackLanguagePreferences::get(cx),
    }
}
