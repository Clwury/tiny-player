use gpui::{
    AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder, px, size,
};

use crate::{
    theme,
    ui::{
        playback_settings_dialog::{PlaybackSettingsDialogState, SettingsChanged},
        titlebar::app_titlebar,
    },
};

use super::{TinyApp, app_window_options, resize::resize_handles, window::window_border};

pub(super) struct SettingsWindow {
    pub(super) settings: Entity<PlaybackSettingsDialogState>,
}

impl TinyApp {
    pub(super) fn open_settings_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window.take()
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            self.settings_window = Some(handle);
            return;
        }

        self.open_server_menu = None;
        self.clear_app_notifications();
        let config = self.cache.playback.clone();
        let settings = cx.new(|cx| PlaybackSettingsDialogState::new(&config, cx));
        cx.subscribe(&settings, |app, settings, _: &SettingsChanged, cx| {
            let settings = settings.read(cx);
            app.cache.playback = settings.playback_config();
            app.cache.color_theme = settings.color_theme();
            app.cache.track_languages = settings.track_languages();
            app.schedule_cache_save("自动保存设置失败", cx);
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(960.0), px(680.0)), cx);
        match cx.open_window(
            app_window_options("设置".into(), bounds, size(px(900.0), px(600.0))),
            |_, cx| cx.new(|_| SettingsWindow { settings }),
        ) {
            Ok(window) => self.settings_window = Some(window),
            Err(error) => {
                self.push_app_error_notification(format!("打开设置窗口失败：{error}"), cx)
            }
        }
        if let Some(error_prefix) = self.pending_cache_save_error_prefix {
            self.schedule_cache_save(error_prefix, cx);
        }
        cx.notify();
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized() && !window.is_fullscreen();
        div()
            .relative()
            .size_full()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(
                        div()
                            .flex_none()
                            .child(app_titlebar(window, cx, "设置".into())),
                    )
                    .child(div().flex_1().min_h_0().child(self.settings.clone())),
            )
            .when(rounded_window, |this| this.children(resize_handles()))
            .when(rounded_window, |this| this.child(window_border(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{player::PlaybackCacheConfig, theme::ColorTheme};
    use gpui::{ScaledPixels, TestAppContext};

    #[gpui::test]
    fn settings_window_bottom_corners_are_not_covered_by_content_backgrounds(
        cx: &mut TestAppContext,
    ) {
        cx.update(theme::init);
        let (_, cx) = cx.add_window_view(|_, cx| SettingsWindow {
            settings: cx
                .new(|cx| PlaybackSettingsDialogState::new(&PlaybackCacheConfig::default(), cx)),
        });
        for selection in [ColorTheme::Mocha, ColorTheme::Latte] {
            cx.update(|_, cx| theme::set(selection, cx));
            for (width, height) in [(900.0, 600.0), (1100.0, 720.0)] {
                cx.simulate_resize(size(px(width), px(height)));
                cx.run_until_parked();
                cx.update(|window, cx| {
                    let theme = theme::get(cx);
                    let scale = window.scale_factor();
                    let radius = ScaledPixels(f32::from(theme.radius_lg) * scale);
                    let mut bottom_left = false;
                    let mut bottom_right = false;
                    let mut sidebar_background = false;
                    assert_ne!(theme.title_bar, theme.background);
                    for quad in window.painted_quads().iter().filter(|quad| {
                        (quad.background == theme.background.into()
                            || quad.background == theme.title_bar.into())
                            && quad.bounds.bottom() == ScaledPixels(height * scale)
                    }) {
                        if quad.bounds.left() == ScaledPixels(0.0) {
                            assert_eq!(quad.corner_radii.bottom_left, radius);
                            bottom_left = true;
                        }
                        if quad.bounds.right() == ScaledPixels(width * scale) {
                            assert_eq!(quad.corner_radii.bottom_right, radius);
                            bottom_right = true;
                        }
                        if quad.background == theme.title_bar.into() {
                            assert_eq!(quad.bounds.left(), ScaledPixels(0.0));
                            assert_eq!(quad.bounds.size.width, ScaledPixels(226.0 * scale));
                            assert_eq!(quad.corner_radii.bottom_right, ScaledPixels(0.0));
                            sidebar_background = true;
                        }
                    }
                    assert!(bottom_left && bottom_right && sidebar_background);
                });
            }
        }
    }
}
