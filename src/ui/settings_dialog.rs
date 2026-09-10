//! Build-aware settings presentation over the same persisted application preferences.

use gpui::{AppContext as _, Context, Entity, EventEmitter, IntoElement, Render, Window};

use crate::{
    player::{PlaybackCacheConfig, PlaybackLanguagePreferences},
    theme::ColorTheme,
};

use super::{
    playback_settings_dialog::PlaybackSettingsDialogState,
    user_settings_dialog::UserSettingsDialogState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsDialogMode {
    Development,
    User,
}

impl SettingsDialogMode {
    pub(crate) fn from_env() -> Self {
        Self::resolve(
            cfg!(debug_assertions),
            std::env::var("TINY_DEV_SETTINGS").ok().as_deref(),
        )
    }

    fn resolve(debug_build: bool, override_value: Option<&str>) -> Self {
        match override_value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("1" | "true" | "on") => Self::Development,
            Some("0" | "false" | "off") => Self::User,
            _ if debug_build => Self::Development,
            _ => Self::User,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Development => "开发设置",
            Self::User => "设置",
        }
    }
}

pub(crate) struct SettingsChanged;

enum SettingsView {
    Development(Entity<PlaybackSettingsDialogState>),
    User(Entity<UserSettingsDialogState>),
}

pub(crate) struct SettingsDialogState {
    view: SettingsView,
}

impl EventEmitter<SettingsChanged> for SettingsDialogState {}

impl SettingsDialogState {
    pub(crate) fn new(
        config: &PlaybackCacheConfig,
        mode: SettingsDialogMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let view = match mode {
            SettingsDialogMode::Development => {
                let dialog = cx.new(|cx| PlaybackSettingsDialogState::new(config, cx));
                cx.subscribe(&dialog, |_, _, _: &SettingsChanged, cx| {
                    cx.emit(SettingsChanged)
                })
                .detach();
                SettingsView::Development(dialog)
            }
            SettingsDialogMode::User => {
                let dialog = cx.new(|cx| UserSettingsDialogState::new(config, cx));
                cx.subscribe(&dialog, |_, _, _: &SettingsChanged, cx| {
                    cx.emit(SettingsChanged)
                })
                .detach();
                SettingsView::User(dialog)
            }
        };
        Self { view }
    }

    pub(crate) fn mode(&self) -> SettingsDialogMode {
        match self.view {
            SettingsView::Development(_) => SettingsDialogMode::Development,
            SettingsView::User(_) => SettingsDialogMode::User,
        }
    }

    pub(crate) fn playback_config(&self, cx: &gpui::App) -> PlaybackCacheConfig {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).playback_config(),
            SettingsView::User(dialog) => dialog.read(cx).playback_config(),
        }
    }

    pub(crate) fn color_theme(&self, cx: &gpui::App) -> ColorTheme {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).color_theme(),
            SettingsView::User(dialog) => dialog.read(cx).color_theme(),
        }
    }

    pub(crate) fn track_languages(&self, cx: &gpui::App) -> PlaybackLanguagePreferences {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).track_languages(),
            SettingsView::User(dialog) => dialog.read(cx).track_languages(),
        }
    }
}

impl Render for SettingsDialogState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        match &self.view {
            SettingsView::Development(dialog) => dialog.clone().into_any_element(),
            SettingsView::User(dialog) => dialog.clone().into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsDialogMode::{self, Development, User};

    #[test]
    fn settings_follow_the_build_profile_without_an_override() {
        for value in [None, Some(""), Some(" "), Some("invalid")] {
            assert_eq!(SettingsDialogMode::resolve(true, value), Development);
            assert_eq!(SettingsDialogMode::resolve(false, value), User);
        }
    }

    #[test]
    fn environment_can_select_either_dialog_in_both_build_profiles() {
        for debug_build in [true, false] {
            for value in ["1", "true", "on", " TRUE ", "On"] {
                assert_eq!(
                    SettingsDialogMode::resolve(debug_build, Some(value)),
                    Development
                );
            }
            for value in ["0", "false", "off", " FALSE ", "Off"] {
                assert_eq!(SettingsDialogMode::resolve(debug_build, Some(value)), User);
            }
        }
    }
}
