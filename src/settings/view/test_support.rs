//! Fixtures and layout assertions for settings presentation tests.
use crate::{media::PlaybackLanguagePreferences, settings::SettingsSnapshot, theme};
use gpui::{Bounds, Pixels, VisualTestContext, px, size};

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

pub(crate) fn assert_about_centered(viewport: Bounds<Pixels>, cx: &mut VisualTestContext) {
    let content = cx.debug_bounds("settings-about-content").unwrap();
    assert!((content.center().x - viewport.center().x).abs() <= px(1.0));
    assert!((content.center().y - viewport.center().y).abs() <= px(1.0));
    assert!(viewport.contains(&content.origin));
    assert!(viewport.contains(&content.bottom_right()));
    for selector in [
        "settings-about-icon",
        "settings-about-name",
        "settings-about-description",
        "settings-about-version",
        "settings-about-github",
    ] {
        let bounds = cx.debug_bounds(selector).expect(selector);
        assert!(
            (bounds.center().x - viewport.center().x).abs() <= px(1.0),
            "{selector}"
        );
    }
    assert_eq!(
        cx.debug_bounds("settings-about-icon").unwrap().size,
        size(px(72.0), px(72.0))
    );
}
