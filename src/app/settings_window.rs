use gpui::{
    AppContext as _, Bounds, Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder, px, size,
};

use crate::{
    theme,
    ui::{
        settings_dialog::{SettingsChanged, SettingsDialogMode, SettingsDialogState},
        titlebar::app_titlebar,
    },
};

use super::{
    TinyApp, app_window_options,
    resize::resize_handles,
    window::{
        WindowCornersExt, WindowFrameColors, sync_window_decorations, window_corner_radii,
        window_frame, window_has_rounded_corners, window_uses_system_decorations,
    },
};

pub(super) struct SettingsWindow {
    pub(super) settings: Entity<SettingsDialogState>,
}

impl TinyApp {
    pub(super) fn open_settings_window(&mut self, cx: &mut Context<Self>) {
        self.open_settings_window_with_mode(SettingsDialogMode::from_env(), cx);
    }

    pub(super) fn open_settings_window_with_mode(
        &mut self,
        mode: SettingsDialogMode,
        cx: &mut Context<Self>,
    ) {
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
        let settings = cx.new(|cx| SettingsDialogState::new(&config, mode, cx));
        cx.subscribe(&settings, |app, settings, _: &SettingsChanged, cx| {
            let settings = settings.read(cx);
            app.cache.playback = settings.playback_config(cx);
            app.cache.color_theme = settings.color_theme(cx);
            app.cache.track_languages = settings.track_languages(cx);
            if let super::Page::Playback { page, .. } = &app.page {
                let config = app.cache.playback.clone();
                if let Err(error) = page.update(cx, |page, _| page.apply_playback_config(config)) {
                    app.push_app_error_notification(format!("应用播放设置失败：{error}"), cx);
                }
            }
            app.schedule_cache_save("自动保存设置失败", cx);
        })
        .detach();
        let initial_size = size(px(960.0), px(680.0));
        let minimum_size = size(px(900.0), px(600.0));
        let bounds = Bounds::centered(None, initial_size, cx);
        match cx.open_window(
            app_window_options(mode.title().into(), bounds, minimum_size),
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
        #[cfg(target_os = "windows")]
        super::window::windows::sync_window_theme(window, cx);
        let title = self.settings.read(cx).mode().title();
        sync_window_decorations(window, title.into(), cx);
        let theme = theme::get(cx);
        let rounded_window = window_has_rounded_corners(window);
        let corners = window_corner_radii(window, cx);
        let system_decorations = window_uses_system_decorations(window);
        let content = div()
            .relative()
            .size_full()
            .when(rounded_window, |this| {
                this.rounded_window_corners(corners).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .when(!system_decorations, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .child(app_titlebar(window, cx, title.into())),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(self.settings.clone())),
            )
            .when(rounded_window, |this| this.children(resize_handles()));

        window_frame(
            content,
            WindowFrameColors {
                background: theme.background,
                top: theme.title_bar,
                bottom_left: theme.panel_background,
            },
            window,
            cx,
        )
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
        let (root, cx) = cx.add_window_view(|_, cx| SettingsWindow {
            settings: cx.new(|cx| {
                SettingsDialogState::new(
                    &PlaybackCacheConfig::default(),
                    SettingsDialogMode::Development,
                    cx,
                )
            }),
        });
        for mode in [SettingsDialogMode::Development, SettingsDialogMode::User] {
            root.update(cx, |window, cx| {
                window.settings = cx
                    .new(|cx| SettingsDialogState::new(&PlaybackCacheConfig::default(), mode, cx));
                cx.notify();
            });
            for selection in ColorTheme::ALL {
                cx.update(|_, cx| theme::set(selection, cx));
                for (width, height) in [(900.0, 600.0), (1100.0, 720.0)] {
                    cx.simulate_resize(size(px(width), px(height)));
                    cx.run_until_parked();
                    cx.update(|window, cx| {
                        let theme = theme::get(cx);
                        let scale = window.scale_factor();
                        // GPUI's TestWindow reports Server decorations.
                        let inset = if cfg!(any(target_os = "windows", target_os = "linux")) {
                            0.0
                        } else {
                            1.0
                        };
                        let radius = ScaledPixels(
                            if cfg!(any(target_os = "windows", target_os = "linux")) {
                                0.0
                            } else {
                                (f32::from(theme.radius_lg) - inset).max(0.0) * scale
                            },
                        );
                        let mut bottom_left = false;
                        let mut sidebar_background = false;
                        assert_ne!(theme.title_bar, theme.background);
                        // The frame now owns the page background; there is no
                        // second full-size content quad at the inner arc.
                        let frame_bounds =
                            Bounds::new(gpui::point(px(0.0), px(0.0)), size(px(width), px(height)))
                                .scale(scale);
                        assert!(window.painted_quads().iter().any(|quad| {
                            quad.bounds == frame_bounds
                                && quad.background == theme.background.into()
                        }));
                        for quad in window.painted_quads().iter().filter(|quad| {
                            (quad.background == theme.background.into()
                                || quad.background == theme.title_bar.into())
                                && quad.bounds.bottom() == ScaledPixels((height - inset) * scale)
                        }) {
                            if quad.bounds.left() == ScaledPixels(inset * scale) {
                                assert_eq!(quad.corner_radii.bottom_left, radius);
                                bottom_left = true;
                            }
                            if quad.bounds.right() == ScaledPixels((width - inset) * scale) {
                                assert_eq!(quad.corner_radii.bottom_right, radius);
                            }
                            if quad.background == theme.title_bar.into() {
                                assert_eq!(quad.bounds.left(), ScaledPixels(inset * scale));
                                assert_eq!(quad.bounds.size.width, ScaledPixels(226.0 * scale));
                                assert_eq!(quad.corner_radii.bottom_right, ScaledPixels(0.0));
                                sidebar_background = true;
                            }
                        }
                        assert!(bottom_left && sidebar_background);
                    });
                }
            }
        }
    }
}
