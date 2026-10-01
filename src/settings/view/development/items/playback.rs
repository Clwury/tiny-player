//! Development settings playback rows in their original display order.
use super::*;

impl PlaybackSettingsDialogState {
    pub(super) fn playback_items(&self, dialog: Entity<Self>, cx: &App) -> [SettingItem; 5] {
        use SettingsCategory::*;
        [
            SettingItem::new(
                Appearance,
                "主题",
                "颜色主题",
                "切换后立即应用，自动保存你的选择。",
                "color_theme appearance catppuccin latte frappe macchiato mocha",
                selector_row(
                    ("color-theme-dropdown", "颜色主题"),
                    self.dropdown.clone(),
                    ColorTheme::ALL.map(|selection| (selection.id(), selection.name(), selection)),
                    self.controller.view_model().color_theme,
                    {
                        let dialog = dialog.clone();
                        move |selection, cx| {
                            dialog.update(cx, |dialog, cx| dialog.select_color_theme(selection, cx))
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "首选语言",
                "音轨语言",
                "播放时优先选择此语言；Default 或无匹配时使用默认音轨。",
                "audio preferred language default 音频 语言",
                track_language_selector(
                    ("audio-language-dropdown", "音轨语言"),
                    self.dropdown.clone(),
                    self.controller.view_model().track_languages.audio,
                    {
                        let dialog = dialog.clone();
                        move |language, cx| {
                            dialog.update(cx, |dialog, cx| {
                                dialog.select_track_language(language, true, cx)
                            })
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "首选语言",
                "字幕语言",
                "播放时优先选择此语言；Default 或无匹配时使用默认字幕，可在详情页手动选择。",
                "subtitle preferred language default 字幕 语言",
                track_language_selector(
                    ("subtitle-language-dropdown", "字幕语言"),
                    self.dropdown.clone(),
                    self.controller.view_model().track_languages.subtitle,
                    {
                        let dialog = dialog.clone();
                        move |language, cx| {
                            dialog.update(cx, |dialog, cx| {
                                dialog.select_track_language(language, false, cx)
                            })
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "视频解码",
                "硬件解码",
                HARDWARE_DECODE_DESCRIPTION,
                "hardware_decode hwdec vulkan 硬解 软解",
                hardware_decode_selector(
                    self.dropdown.clone(),
                    self.controller.view_model().config.hardware_decode,
                    {
                        let dialog = dialog.clone();
                        move |mode, cx| {
                            dialog.update(cx, |dialog, cx| dialog.select_hardware_decode(mode, cx))
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "流畅度",
                "解码器追赶丢帧",
                "视频落后时跳过部分解码以追赶音频，可能降低画面连贯性。默认关闭。",
                "decoder_framedrop framedrop 解码 丢帧",
                toggle(
                    &dialog,
                    cx,
                    ToggleSetting::DecoderFramedrop,
                    "解码器追赶丢帧",
                    self.controller.view_model().config.decoder_framedrop,
                ),
            ),
        ]
    }
}
