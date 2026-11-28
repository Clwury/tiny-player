//! GPUI global binding for shared media preferences.
use crate::media::{
    ItemSortPreferences, PlaybackLanguagePreferences, SubtitleAppearance, SubtitleStyle,
};
use gpui::{App, Global};

impl Global for PlaybackLanguagePreferences {}
impl Global for ItemSortPreferences {}
impl Global for SubtitleStyle {}

impl ItemSortPreferences {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn apply(self, cx: &mut App) {
        if cx.try_global::<Self>() != Some(&self) {
            cx.set_global(self);
        }
    }
}

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

impl SubtitleStyle {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    /// Store normalized preferences in the GPUI global, returning whether the
    /// value changed. Callers that only need the global updated (for example a
    /// playback-owned position) can skip refreshing unrelated windows.
    pub(crate) fn store(self, cx: &mut App) -> bool {
        let style = self.normalized();
        if cx.try_global::<Self>() == Some(&style) {
            return false;
        }
        cx.set_global(style);
        true
    }

    pub(crate) fn apply(self, cx: &mut App) {
        if self.store(cx) {
            cx.refresh_windows();
        }
    }
}

impl SubtitleAppearance {
    /// Apply appearance changes without touching the playback-owned position.
    pub(crate) fn apply_appearance(self, cx: &mut App) {
        SubtitleStyle::get(cx).with_appearance(self).apply(cx);
    }
}

mod track_preferences;
pub(crate) use track_preferences::PlaybackTrackPreferences;

#[cfg(test)]
mod tests {
    use super::SubtitleStyle;
    use crate::media::{SubtitleAppearance, SubtitleTextColor};
    use gpui::TestAppContext;

    #[gpui::test]
    fn appearance_changes_preserve_vertical_position(cx: &mut TestAppContext) {
        cx.update(|cx| {
            SubtitleStyle {
                vertical_offset_fraction: Some(0.02),
                ..Default::default()
            }
            .apply(cx);

            SubtitleAppearance {
                text_color: SubtitleTextColor::Yellow,
                ..Default::default()
            }
            .apply_appearance(cx);

            let style = SubtitleStyle::get(cx);
            assert_eq!(style.appearance.text_color, SubtitleTextColor::Yellow);
            assert_eq!(style.vertical_offset_fraction, Some(0.02));
        });
    }
}
