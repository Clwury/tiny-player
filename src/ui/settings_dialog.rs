//! Build-aware settings presentation over the same persisted application preferences.

use gpui::{AppContext as _, Context, Entity, EventEmitter, IntoElement, Render, Window};

use crate::player::PlaybackCacheConfig;
#[cfg(test)]
use crate::{player::PlaybackLanguagePreferences, theme::ColorTheme};

use super::{
    playback_settings_dialog::PlaybackSettingsDialogState,
    user_settings_dialog::UserSettingsDialogState,
};

pub(crate) use crate::settings::SettingsMode as SettingsDialogMode;

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

    pub(crate) fn snapshot(&self, cx: &gpui::App) -> crate::settings::SettingsSnapshot {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).snapshot(),
            SettingsView::User(dialog) => dialog.read(cx).snapshot(),
        }
    }

    #[cfg(test)]
    pub(crate) fn playback_config(&self, cx: &gpui::App) -> PlaybackCacheConfig {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).playback_config(),
            SettingsView::User(dialog) => dialog.read(cx).playback_config(),
        }
    }

    #[cfg(test)]
    pub(crate) fn color_theme(&self, cx: &gpui::App) -> ColorTheme {
        match &self.view {
            SettingsView::Development(dialog) => dialog.read(cx).color_theme(),
            SettingsView::User(dialog) => dialog.read(cx).color_theme(),
        }
    }

    #[cfg(test)]
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
