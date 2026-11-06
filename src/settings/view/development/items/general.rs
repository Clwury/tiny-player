use super::*;

impl PlaybackSettingsDialogState {
    pub(super) fn general_items(&self, dialog: Entity<Self>, cx: &App) -> [SettingItem; 3] {
        let language_dialog = dialog.clone();
        let logging_enabled = self.controller.view_model().playback_debug_log;
        [
            SettingItem::new(
                SettingsCategory::General,
                "界面",
                "语言",
                "应用界面语言",
                "app_language interface language locale 简体中文 繁體中文 English",
                app_language_selector(
                    self.dropdown.clone(),
                    self.controller.view_model().app_language,
                    move |language, cx| {
                        language_dialog
                            .update(cx, |dialog, cx| dialog.select_app_language(language, cx))
                    },
                ),
            ),
            SettingItem::new(
                SettingsCategory::General,
                "播放诊断",
                PLAYBACK_DEBUG_LOG_LABEL,
                PLAYBACK_DEBUG_LOG_DESCRIPTION,
                "playback debug logging log file privacy 调试 日志 文件 隐私",
                toggle_switch(
                    PLAYBACK_DEBUG_LOG_LABEL,
                    logging_enabled,
                    move |cx| {
                        dialog.update(cx, |dialog, cx| {
                            dialog.dispatch(SettingsIntent::PlaybackDebugLog(!logging_enabled), cx)
                        })
                    },
                    cx,
                ),
            ),
            SettingItem::new(
                SettingsCategory::General,
                "播放诊断",
                "日志文件",
                "每次播放开始时覆盖重写，关闭开关后保留日志。",
                "playback debug logging log file path 日志 文件 路径",
                playback_log_location(cx),
            ),
        ]
    }
}
