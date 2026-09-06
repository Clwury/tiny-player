use std::time::Duration;

use gpui::Context;

use crate::storage;

use super::TinyApp;

const CACHE_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);

impl TinyApp {
    pub(super) fn schedule_cache_save(
        &mut self,
        error_prefix: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.pending_cache_save_error_prefix = Some(error_prefix);
        self.last_cache_save_activity = Some(cx.background_executor().now());

        // Keep one debounce task alive for the whole burst of changes. Window
        // resize events can arrive once per frame; creating and cancelling a
        // timer for every pixel change still allocates work on the foreground
        // executor even though only the final size needs to be persisted.
        if self.cache_save_task_active {
            return;
        }

        self.cache_save_task_active = true;
        self.cache_save_task = cx.spawn(async move |app, cx| {
            cx.background_executor().timer(CACHE_SAVE_DEBOUNCE).await;
            loop {
                let remaining = app
                    .update(cx, |app, cx| {
                        app.last_cache_save_activity
                            .map(|last| {
                                CACHE_SAVE_DEBOUNCE.saturating_sub(
                                    cx.background_executor()
                                        .now()
                                        .saturating_duration_since(last),
                                )
                            })
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if remaining.is_zero() {
                    app.update(cx, |app, cx| {
                        app.cache_save_task_active = false;
                        app.last_cache_save_activity = None;
                        app.flush_scheduled_cache_save(cx);
                    })
                    .ok();
                    break;
                }
                cx.background_executor().timer(remaining).await;
            }
        });
    }

    pub(super) fn flush_scheduled_cache_save(&mut self, cx: &mut Context<Self>) {
        let Some(error_prefix) = self.pending_cache_save_error_prefix.take() else {
            return;
        };
        // The small settings file is written once per burst on the app thread.
        // Read the latest state here; overlapping background snapshots could
        // otherwise overwrite a newer edit or race on the same temporary file.
        if let Err(error) = self.save_cache() {
            self.pending_cache_save_error_prefix = Some(error_prefix);
            self.push_app_error_notification(format!("{error_prefix}：{error}"), cx);
        }
    }

    pub(super) fn save_pending_cache_on_release(&mut self) {
        if self.pending_cache_save_error_prefix.take().is_some()
            && let Err(error) = self.save_cache()
        {
            tracing::error!(%error, "failed to save pending application settings on close");
        }
    }

    fn save_cache(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(path) = &self.cache_save_path {
            return storage::save_to(&self.cache, path);
        }
        storage::save(&self.cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        storage::ServerCache,
        theme::{self, ColorTheme},
        ui::editor::Editor,
    };
    use gpui::{AppContext as _, Modifiers, TestAppContext, VisualTestContext, px, size};

    #[gpui::test]
    fn settings_dialog_changes_reach_disk_and_survive_reopening(cx: &mut TestAppContext) {
        use crate::player::{PlaybackLanguagePreferences, TrackLanguage};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        cx.update(|cx| {
            theme::init(cx);
            Editor::bind_keys(cx);
        });
        let (app, cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.cache_save_path = Some(path.clone());
            app.window_persistence_enabled = false;
            app.open_playback_settings_dialog(cx);
            app
        });
        cx.simulate_resize(size(px(1100.0), px(720.0)));
        cx.run_until_parked();
        let click = |cx: &mut VisualTestContext, selector| {
            let position = cx.debug_bounds(selector).unwrap().center();
            cx.simulate_click(position, Modifiers::default());
            cx.run_until_parked();
            for _ in 0..2 {
                cx.update(|window, cx| window.simulate_next_frame(cx));
                cx.run_until_parked();
            }
        };
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
        click(cx, "close-playback-settings");
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(saved.color_theme, ColorTheme::Latte);
        assert_eq!(saved.playback.cache_secs, 42.5);
        assert_eq!(saved.track_languages, languages);
        assert!(app.read_with(cx, |app, _| app.playback_settings_dialog.is_none()));

        app.update(cx, |app, cx| app.open_playback_settings_dialog(cx));
        assert_eq!(
            app.read_with(cx, |app, cx| {
                app.playback_settings_dialog
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .track_languages()
            }),
            languages
        );
        assert_eq!(
            app.read_with(cx, |app, cx| {
                app.playback_settings_dialog
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .color_theme()
            }),
            ColorTheme::Latte
        );
        assert_eq!(
            app.read_with(cx, |app, cx| {
                app.playback_settings_dialog
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .playback_config()
                    .cache_secs
            }),
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
            assert!(app.pending_cache_save_error_prefix.is_some());
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
        assert!(app.read_with(cx, |app, _| app.pending_cache_save_error_prefix.is_none()));
    }
}
