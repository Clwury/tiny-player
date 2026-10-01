pub(crate) mod binding;
mod controller;
pub(crate) mod memory_budget;
mod model;
pub(crate) mod values;
pub(crate) mod view;
pub(crate) use controller::SettingsController;
pub(crate) use model::{
    NumericSetting, SettingDescriptor, SettingValidation, SettingsCategory, SettingsIntent,
    SettingsMode, SettingsSnapshot, ToggleSetting,
};

impl SettingsMode {
    pub(crate) fn from_env() -> Self {
        Self::resolve(
            cfg!(debug_assertions),
            std::env::var("TINY_DEV_SETTINGS").ok().as_deref(),
        )
    }
}

/// Shell adapter. The model submits validated, exact values; the existing
/// application coordinator remains the only writer of the composed document.
pub(crate) struct SettingsPersistenceAdapter;
impl SettingsPersistenceAdapter {
    pub(crate) fn apply(snapshot: SettingsSnapshot, config: &mut crate::config::GlobalConfig) {
        config.playback = snapshot.playback;
        config.color_theme = snapshot.color_theme;
        config.track_languages = snapshot.track_languages;
    }
}
