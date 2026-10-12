use super::*;

impl PlaybackSettingsDialogState {
    pub(super) fn general_items(&self, dialog: Entity<Self>) -> [SettingItem; 1] {
        [SettingItem::new(
            SettingsCategory::General,
            "界面",
            "语言",
            "应用界面语言",
            "app_language interface language locale 简体中文 繁體中文 English",
            app_language_selector(
                self.dropdown.clone(),
                self.controller.view_model().app_language,
                move |language, cx| {
                    dialog.update(cx, |dialog, cx| dialog.select_app_language(language, cx))
                },
            ),
        )]
    }
}
