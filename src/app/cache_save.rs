use gpui::Context;

use crate::persistence::{DirtyKey, FilePersistence};

use super::TinyApp;

#[cfg(test)]
use crate::{persistence::SETTINGS_DEBOUNCE as CACHE_SAVE_DEBOUNCE, storage};
#[cfg(test)]
use std::time::Duration;

impl TinyApp {
    pub(super) fn update_items_sort(
        &mut self,
        preferences: crate::media::ItemSortPreferences,
        cx: &mut Context<Self>,
    ) {
        if self.cache.items_sort == preferences {
            return;
        }
        self.cache.items_sort = preferences;
        self.schedule_cache_save("保存排序设置失败", cx);
    }

    pub(super) fn update_playback_volume(
        &mut self,
        settings: crate::player::PlaybackVolumeSettings,
        cx: &mut Context<Self>,
    ) {
        let settings = settings.normalized();
        if self.cache.playback_volume == settings {
            return;
        }
        self.cache.playback_volume = settings;
        self.schedule_cache_save("保存音量失败", cx);
    }

    pub(super) fn schedule_cache_save(
        &mut self,
        error_prefix: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.schedule_config_save(DirtyKey::Settings, error_prefix, cx);
    }

    pub(super) fn schedule_config_save(
        &mut self,
        key: DirtyKey,
        error_prefix: &'static str,
        cx: &mut Context<Self>,
    ) {
        let app = cx.weak_entity();
        self.persistence.schedule_settings(
            key,
            self.cache_snapshot(),
            self.persistence_adapter(),
            error_prefix,
            move |message, cx| {
                app.update(cx, |app, cx| app.push_app_error_notification(message, cx))
                    .ok();
            },
            cx,
        );
    }

    #[cfg(test)]
    pub(super) fn flush_scheduled_cache_save(&mut self, cx: &mut Context<Self>) {
        if let Some(message) = self.persistence.flush(&DirtyKey::Settings, cx) {
            self.push_app_error_notification(message, cx);
        }
    }

    pub(super) fn flush_persistence(&mut self, cx: &mut Context<Self>) -> gpui::Task<()> {
        let (error, task) = self.persistence.flush_all(cx);
        if let Some(message) = error {
            self.push_app_error_notification(message, cx);
        }
        task
    }

    pub(super) fn save_pending_cache_on_release(&mut self, cx: &mut gpui::App) {
        let (error, task) = self.persistence.flush_all(cx);
        if error.is_some() {
            tracing::error!("failed to save pending application settings on close");
        }
        task.detach();
    }

    pub(super) fn cache_snapshot(&self) -> crate::storage::ServerCache {
        self.cache.snapshot(self.server_feature.catalog())
    }

    #[cfg(test)]
    pub(super) fn save_cache(&self) -> anyhow::Result<()> {
        self.persistence
            .save_settings_now(self.cache_snapshot(), self.persistence_adapter())
    }

    pub(super) fn persistence_adapter(
        &self,
    ) -> std::sync::Arc<dyn crate::persistence::AppPersistence> {
        std::sync::Arc::new(FilePersistence {
            #[cfg(test)]
            settings_path: self.cache_save_path.clone(),
            #[cfg(test)]
            home_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::binding::PlaybackTrackPreferences;
    use crate::{
        storage::ServerCache,
        theme::{self, ColorTheme},
        ui::editor::Editor,
    };
    use gpui::{AppContext as _, Modifiers, TestAppContext, VisualTestContext, px, size};

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        if selector == "window-control-close" {
            if cfg!(target_os = "windows")
                || cx.update(|window, _| super::super::window_uses_system_decorations(window))
            {
                // TestPlatform does not emulate native caption/WM close messages.
                cx.update(|window, _| window.remove_window());
            } else {
                let position = cx.debug_bounds(selector).expect(selector).center();
                cx.simulate_mouse_down(position, gpui::MouseButton::Left, Modifiers::default());
            }
            cx.run_until_parked();
            return;
        }
        let position = cx.debug_bounds(selector).expect(selector).center();
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        for _ in 0..2 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    fn track_choices_do_not_write_application_settings(cx: &mut TestAppContext) {
        use crate::player::{PlaybackTrackKind, PlaybackTrackPreferenceKey};

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let server = serde_json::from_value(serde_json::json!({
            "id": "local", "user_id": "user",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        cx.update(theme::init);
        let (app, cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app.window_persistence_enabled = false;
            app
        });
        cx.update(|_, cx| {
            PlaybackTrackPreferences::remember(
                &server,
                &[PlaybackTrackPreferenceKey {
                    item_id: "episode".into(),
                    media_source_id: "source".into(),
                }],
                PlaybackTrackKind::Subtitle,
                None,
                cx,
            );
        });
        cx.run_until_parked();
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert!(!path.exists());
        app.update(cx, |app, _| {
            assert!(app.persistence.pending_settings_error_prefix().is_none());
            app.save_cache().unwrap();
        });
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(json.get("track_preferences").is_none());
        assert!(json.get("track_languages").is_some());
    }

    #[gpui::test]
    fn settings_window_changes_reach_disk_and_survive_reopening(cx: &mut TestAppContext) {
        use crate::player::{PlaybackLanguagePreferences, TrackLanguage};
        use crate::settings::view::SettingsDialogMode;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        cx.update(|cx| {
            theme::init(cx);
            Editor::bind_keys(cx);
        });
        let (app, main_cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app.window_persistence_enabled = false;
            app
        });
        main_cx.simulate_resize(size(px(1100.0), px(720.0)));
        app.update(main_cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::Development, cx)
        });
        let settings_window = app.read_with(main_cx, |app, _| app.settings_window.unwrap());
        let cx = &mut VisualTestContext::from_window(settings_window.into(), &main_cx.cx);
        cx.simulate_resize(size(px(960.0), px(680.0)));
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 2);
        assert!(main_cx.debug_bounds("playback-settings-panel").is_none());
        app.update(main_cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::Development, cx)
        });
        assert_eq!(
            app.read_with(main_cx, |app, _| app.settings_window),
            Some(settings_window)
        );
        assert_eq!(cx.windows().len(), 2);
        assert!(!path.exists());
        click(cx, "settings-category-预读策略");
        click(cx, "cache-secs-input");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("42.5");
        cx.run_until_parked();
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path)
                .unwrap()
                .playback
                .cache_secs,
            42.5
        );

        click(cx, "settings-category-播放");
        click(cx, "settings-toggle-解码器追赶丢帧");
        click(cx, "audio-language-dropdown");
        click(cx, "language-japanese");
        click(cx, "subtitle-language-dropdown");
        click(cx, "language-chinese-simplified");
        let languages = PlaybackLanguagePreferences {
            audio: TrackLanguage::Japanese,
            subtitle: TrackLanguage::ChineseSimplified,
        };
        assert_eq!(
            cx.update(|_, cx| PlaybackLanguagePreferences::get(cx)),
            languages
        );
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().track_languages,
            languages
        );

        click(cx, "settings-category-外观");
        click(cx, "color-theme-dropdown");
        click(cx, "theme-latte");
        assert_eq!(
            cx.update(|_, cx| theme::get(cx).selection),
            ColorTheme::Latte
        );
        click(cx, "window-control-close");
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.color_theme, ColorTheme::Latte);
        assert_eq!(saved.playback.cache_secs, 42.5);
        assert!(saved.playback.decoder_framedrop);
        assert_eq!(saved.track_languages, languages);
        assert!(app.read_with(cx, |app, _| app.settings_window.is_none()));

        app.update(cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::Development, cx)
        });
        assert_eq!(cx.windows().len(), 2);
        let reopened = app.read_with(cx, |app, _| app.settings_window.unwrap());
        assert_ne!(reopened, settings_window);
        reopened
            .read_with(cx, |window, cx| {
                let settings = window.settings.read(cx);
                assert_eq!(settings.track_languages(cx), languages);
                assert_eq!(settings.color_theme(cx), ColorTheme::Latte);
                assert_eq!(settings.playback_config(cx).cache_secs, 42.5);
                assert!(settings.playback_config(cx).decoder_framedrop);
            })
            .unwrap();
        // The window manager's close path also clears the handle and preserves the main window.
        reopened
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.settings_window.is_none()));
        assert_eq!(cx.windows().len(), 1);
    }

    #[gpui::test]
    fn user_settings_autosave_and_reopen_in_either_dialog_without_losing_preferences(
        cx: &mut TestAppContext,
    ) {
        use crate::{
            player::{
                HardwareDecodeMode, PlaybackCacheConfig, PlaybackLanguagePreferences, TrackLanguage,
            },
            settings::view::SettingsDialogMode,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let custom = PlaybackCacheConfig {
            cache_secs: 42.5,
            disk_cache_max_bytes: 5 * 1024 * 1024 * 1024,
            decoder_framedrop: true,
            ..PlaybackCacheConfig::default()
        };
        cx.update(|cx| {
            theme::init(cx);
            Editor::bind_keys(cx);
        });
        let (app, main_cx) = cx.add_window_view(|_, cx| {
            let mut cache = ServerCache::empty();
            cache.playback = custom.clone();
            let mut app = TinyApp::new(cache, None, cx);
            app.cache_save_path = Some(path.clone());
            app.window_persistence_enabled = false;
            app
        });
        app.update(main_cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::User, cx)
        });
        let handle = app.read_with(main_cx, |app, _| app.settings_window.unwrap());
        let cx = &mut VisualTestContext::from_window(handle.into(), &main_cx.cx);
        cx.simulate_resize(size(px(960.0), px(680.0)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("user-settings-panel").is_some());
        assert!(cx.debug_bounds("playback-settings-panel").is_none());
        assert!(!path.exists());
        app.update(cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::User, cx)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.settings_window),
            Some(handle)
        );

        click(cx, "color-theme-dropdown");
        click(cx, "theme-latte");
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(storage::load_or_init_from(&path).unwrap().playback, custom);
        click(cx, "settings-category-播放");
        click(cx, "audio-language-dropdown");
        click(cx, "language-japanese");
        click(cx, "subtitle-language-dropdown");
        click(cx, "language-chinese-simplified");
        click(cx, "hardware-decode-dropdown");
        click(cx, "hardware-decode-off");
        click(cx, "settings-category-内存缓存");
        click(cx, "memory-budget-dropdown");
        click(cx, "memory-budget-128-mib");
        click(cx, "settings-category-磁盘缓存");
        click(cx, "settings-toggle-启用磁盘缓存");
        assert_eq!(
            app.read_with(cx, |app, _| app.cache.playback.disk_cache_max_bytes),
            custom.disk_cache_max_bytes
        );
        click(cx, "disk-cache-input");
        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_input("7");
        cx.run_until_parked();
        click(cx, "window-control-close");
        let languages = PlaybackLanguagePreferences {
            audio: TrackLanguage::Japanese,
            subtitle: TrackLanguage::ChineseSimplified,
        };
        let expected = PlaybackCacheConfig {
            http_cache_max_bytes: 16 * 1024 * 1024,
            demuxer_max_bytes: 75 * 1024 * 1024,
            demuxer_max_back_bytes: 25 * 1024 * 1024,
            total_cache_max_bytes: 128 * 1024 * 1024,
            disk_cache: true,
            disk_cache_max_bytes: 7 * 1024 * 1024 * 1024,
            cache_secs: 42.5,
            decoder_framedrop: true,
            hardware_decode: HardwareDecodeMode::Off,
            ..PlaybackCacheConfig::default()
        };
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.playback, expected);
        assert_eq!(saved.color_theme, ColorTheme::Latte);
        assert_eq!(saved.track_languages, languages);
        assert!(app.read_with(cx, |app, _| app.settings_window.is_none()));

        for mode in [SettingsDialogMode::Development, SettingsDialogMode::User] {
            app.update(cx, |app, cx| app.open_settings_window_with_mode(mode, cx));
            let reopened = app.read_with(cx, |app, _| app.settings_window.unwrap());
            reopened
                .read_with(cx, |window, cx| {
                    let settings = window.settings.read(cx);
                    assert_eq!(settings.mode(), mode);
                    assert_eq!(settings.playback_config(cx), expected);
                    assert_eq!(settings.color_theme(cx), ColorTheme::Latte);
                    assert_eq!(settings.track_languages(cx), languages);
                })
                .unwrap();
            reopened
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().playback,
            expected
        );
        assert_eq!(cx.windows().len(), 1);
    }

    #[gpui::test]
    fn closing_main_window_closes_settings_and_flushes_pending_changes(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        cx.update(theme::init);
        let (app, cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app.window_persistence_enabled = false;
            app
        });
        app.update(cx, |app, cx| {
            app.open_settings_window(cx);
            app.cache.playback.cache_secs = 42.5;
            app.schedule_cache_save("自动保存设置失败", cx);
        });
        assert_eq!(cx.windows().len(), 2);
        assert!(!path.exists());
        drop(app);
        cx.update(|window, _| window.remove_window());
        cx.run_until_parked();
        assert!(cx.windows().is_empty());
        assert_eq!(
            storage::load_or_init_from(&path)
                .unwrap()
                .playback
                .cache_secs,
            42.5
        );
    }

    #[gpui::test]
    fn automatic_save_coalesces_edits_and_close_flushes_the_latest_values(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        app.update(cx, |app, cx| {
            app.cache.playback.cache_secs = 5.0;
            app.schedule_cache_save("保存设置失败", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(200));
        app.update(cx, |app, cx| {
            app.cache.playback.cache_secs = 42.0;
            app.cache.color_theme = ColorTheme::Latte;
            app.schedule_cache_save("保存设置失败", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert!(!path.exists());
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.playback.cache_secs, 42.0);
        assert_eq!(saved.color_theme, ColorTheme::Latte);

        app.update(cx, |app, cx| {
            app.cache.playback.cache_secs = 75.0;
            app.schedule_cache_save("保存设置失败", cx);
            app.flush_scheduled_cache_save(cx);
        });
        assert_eq!(
            storage::load_or_init_from(&path)
                .unwrap()
                .playback
                .cache_secs,
            75.0
        );
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path)
                .unwrap()
                .playback
                .cache_secs,
            75.0
        );
    }

    #[gpui::test]
    fn shared_sort_changes_are_coalesced_flushed_on_close_and_restored_on_restart(
        cx: &mut TestAppContext,
    ) {
        use crate::{
            emby::{SortOrder, UserItemsSort},
            media::ItemSortPreferences,
        };

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        cx.run_until_parked();
        assert!(!path.exists());
        assert!(app.read_with(cx, |app, _| !app.persistence.is_dirty(&DirtyKey::Settings)));

        for sort_by in [UserItemsSort::DateCreated, UserItemsSort::PremiereDate] {
            cx.update(|cx| {
                ItemSortPreferences {
                    sort_by,
                    sort_order: SortOrder::Ascending,
                }
                .apply(cx)
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            assert!(!path.exists());
        }
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().items_sort,
            ItemSortPreferences {
                sort_by: UserItemsSort::PremiereDate,
                sort_order: SortOrder::Ascending,
            }
        );

        let selected = ItemSortPreferences {
            sort_by: UserItemsSort::ProductionYear,
            sort_order: SortOrder::Descending,
        };
        cx.update(|cx| selected.apply(cx));
        cx.run_until_parked();
        assert_eq!(app.read_with(cx, |app, _| app.cache.items_sort), selected);
        drop(app);
        cx.update(|_| {});
        cx.run_until_parked();
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.items_sort, selected);
        cx.update(|cx| ItemSortPreferences::default().apply(cx));
        let reopened = cx.new(|cx| {
            let mut app = TinyApp::new(saved, None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| ItemSortPreferences::get(cx)), selected);
        cx.update(|cx| selected.apply(cx));
        cx.run_until_parked();
        assert!(reopened.read_with(cx, |app, _| !app.persistence.is_dirty(&DirtyKey::Settings)));
    }

    #[gpui::test]
    fn volume_changes_are_coalesced_saved_and_restored_on_restart(cx: &mut TestAppContext) {
        use crate::player::PlaybackVolumeSettings;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        for level in [0.98, 0.96, 0.94] {
            app.update(cx, |app, cx| {
                app.update_playback_volume(
                    PlaybackVolumeSettings {
                        level,
                        unmuted_level: level,
                    },
                    cx,
                );
            });
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            assert!(!path.exists());
        }
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path)
                .unwrap()
                .playback_volume
                .level,
            0.94
        );

        let muted = PlaybackVolumeSettings {
            level: 0.0,
            unmuted_level: 0.94,
        };
        app.update(cx, |app, cx| app.update_playback_volume(muted, cx));
        // Closing before the debounce expires must still save the final setting.
        drop(app);
        cx.update(|_| {});
        cx.run_until_parked();
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.playback_volume, muted);

        let reopened = cx.new(|cx| {
            let mut app = TinyApp::new(saved, None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        assert_eq!(
            reopened.read_with(cx, |app, _| app.cache.playback_volume),
            muted
        );
        reopened.update(cx, |app, cx| app.update_playback_volume(muted, cx));
        assert!(reopened.read_with(cx, |app, _| {
            app.persistence.pending_settings_error_prefix().is_none()
        }));
    }

    #[gpui::test]
    fn pending_settings_are_written_when_the_app_is_released(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app.cache.color_theme = ColorTheme::Macchiato;
            app.schedule_cache_save("保存设置失败", cx);
            app
        });
        assert!(!path.exists());
        drop(app);
        cx.update(|_| {});
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().color_theme,
            ColorTheme::Macchiato
        );
    }

    #[gpui::test]
    fn failed_automatic_save_is_retained_and_retried_on_the_next_change(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let blocked_parent = temp.path().join("file");
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(blocked_parent.join("servers.json"));
            app
        });
        app.update(cx, |app, cx| {
            app.cache.color_theme = ColorTheme::Frappe;
            app.schedule_cache_save("保存设置失败", cx);
            app.flush_scheduled_cache_save(cx);
            assert!(app.persistence.pending_settings_error_prefix().is_some());
            assert!(app.has_app_notifications());
            app.cache_save_path = Some(path.clone());
            app.cache.color_theme = ColorTheme::Latte;
            app.schedule_cache_save("保存设置失败", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(CACHE_SAVE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().color_theme,
            ColorTheme::Latte
        );
        assert!(app.read_with(cx, |app, _| {
            app.persistence.pending_settings_error_prefix().is_none()
        }));
    }

    #[gpui::test]
    fn application_quit_flushes_each_config_dirty_reason_as_one_latest_snapshot(
        cx: &mut TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let app = cx.new(|cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        app.update(cx, |app, cx| {
            app.cache.color_theme = ColorTheme::Latte;
            app.schedule_cache_save("保存设置失败", cx);
            app.cache.set_window_size(1234, 789);
            app.schedule_config_save(DirtyKey::Window, "保存窗口大小失败", cx);
            app.schedule_config_save(DirtyKey::SearchHistory, "保存搜索历史失败", cx);
        });
        assert!(!path.exists());
        cx.quit();
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.color_theme, ColorTheme::Latte);
        assert_eq!(saved.window_size().unwrap().width, 1234);
        app.read_with(cx, |app, _| {
            for key in [
                DirtyKey::Settings,
                DirtyKey::Window,
                DirtyKey::SearchHistory,
            ] {
                assert!(!app.persistence.is_dirty(&key));
            }
        });
    }
}
