use gpui::{
    App, BorderStyle, Bounds, Context, IntoElement, Pixels, SharedString, Size, Styled,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowOptions, canvas, deferred, outline,
};

use crate::{
    app_metadata::{APP_ID, APP_NAME},
    theme,
};

use super::{Page, TinyApp};

#[cfg(target_os = "windows")]
pub(super) mod windows;

/// Windows owns the outer frame; other platforms retain our rounded client decorations.
pub(crate) fn window_has_rounded_corners(window: &Window) -> bool {
    !cfg!(target_os = "windows") && !window.is_maximized() && !window.is_fullscreen()
}

pub(super) fn window_border(cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    let radius = theme.radius_lg;
    let color = theme.window_border;
    // Like Zed's client-side decorations, use a 1px theme border. Paint above
    // child backgrounds without changing content bounds or adding a hitbox.
    deferred(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.paint_quad(outline(bounds, color, BorderStyle::Solid).corner_radii(radius));
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
    .with_priority(10)
}

pub(crate) fn app_window_options(
    title: SharedString,
    bounds: Bounds<Pixels>,
    minimum_size: Size<Pixels>,
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(minimum_size),
        window_decorations: Some(if cfg!(target_os = "windows") {
            WindowDecorations::Server
        } else {
            WindowDecorations::Client
        }),
        window_background: if cfg!(target_os = "windows") {
            WindowBackgroundAppearance::Opaque
        } else {
            WindowBackgroundAppearance::Transparent
        },
        titlebar: Some(TitlebarOptions {
            title: Some(title),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        app_id: Some(APP_ID.to_string()),
        ..Default::default()
    }
}

impl TinyApp {
    pub(super) fn observe_window_bounds_once(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_bounds_observed {
            return;
        }

        cx.observe_window_bounds(window, |app, window, cx| {
            app.save_window_size(window, cx);
        })
        .detach();
        self.window_bounds_observed = true;
    }

    pub(super) fn title(&self, cx: &Context<Self>) -> SharedString {
        match &self.page {
            Page::Servers => APP_NAME.into(),
            Page::Home(page) => page.read(cx).title(cx),
            Page::Playback { page, .. } => page.read(cx).title(),
        }
    }

    fn save_window_size(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.window_persistence_enabled || window.is_maximized() || window.is_fullscreen() {
            return;
        }

        let size = window.window_bounds().get_bounds().size;
        let width = f32::from(size.width).round();
        let height = f32::from(size.height).round();
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return;
        }

        let width = width as u32;
        let height = height as u32;
        if !self.cache.set_window_size(width, height) {
            return;
        }

        self.schedule_cache_save("保存窗口大小失败", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage::ServerCache, theme::ColorTheme};
    use gpui::{Corners, Edges, ScaledPixels, TestAppContext, VisualTestContext, point, px, size};

    #[test]
    fn window_options_use_platform_decorations_with_a_themed_titlebar() {
        let bounds = Bounds::new(point(px(100.0), px(80.0)), size(px(1100.0), px(720.0)));
        let minimum = size(px(900.0), px(600.0));
        let options = app_window_options(APP_NAME.into(), bounds, minimum);
        assert_eq!(options.window_min_size, Some(minimum));
        assert_eq!(options.window_bounds, Some(WindowBounds::Windowed(bounds)));
        let titlebar = options.titlebar.unwrap();
        assert_eq!(titlebar.title.as_deref(), Some(APP_NAME));
        assert!(titlebar.appears_transparent);
        if cfg!(target_os = "windows") {
            assert_eq!(options.window_decorations, Some(WindowDecorations::Server));
            assert_eq!(
                options.window_background,
                WindowBackgroundAppearance::Opaque
            );
        } else {
            assert_eq!(options.window_decorations, Some(WindowDecorations::Client));
            assert_eq!(
                options.window_background,
                WindowBackgroundAppearance::Transparent
            );
        }
    }

    fn assert_border(cx: &mut VisualTestContext, width: f32, height: f32, visible: bool) {
        cx.run_until_parked();
        cx.update(|window, cx| {
            let theme = theme::get(cx);
            let scale = window.scale_factor();
            let bounds = Bounds::new(
                point(ScaledPixels(0.0), ScaledPixels(0.0)),
                size(ScaledPixels(width * scale), ScaledPixels(height * scale)),
            );
            let quads = window.painted_quads();
            let borders = quads
                .iter()
                .filter(|quad| quad.bounds == bounds && quad.border_widths.top > ScaledPixels(0.0))
                .collect::<Vec<_>>();
            assert_eq!(!borders.is_empty(), visible);
            for border in &borders {
                assert_eq!(border.border_color, theme.window_border);
                assert_eq!(border.border_widths, Edges::all(ScaledPixels(scale)));
                assert_eq!(
                    border.corner_radii,
                    Corners::all(ScaledPixels(f32::from(theme.radius_lg) * scale))
                );
                let mask = border.content_mask.bounds;
                assert!(mask.left() >= bounds.left() && mask.right() <= bounds.right());
                assert!(mask.top() >= bounds.top() && mask.bottom() <= bounds.bottom());
            }
            if visible {
                // GPUI can split an outline into several quads to skip its transparent interior.
                for point in [
                    point(bounds.center().x, bounds.top() + ScaledPixels(0.5)),
                    point(bounds.center().x, bounds.bottom() - ScaledPixels(0.5)),
                    point(bounds.left() + ScaledPixels(0.5), bounds.center().y),
                    point(bounds.right() - ScaledPixels(0.5), bounds.center().y),
                ] {
                    assert!(
                        borders
                            .iter()
                            .any(|quad| quad.content_mask.bounds.contains(&point))
                    );
                }
            }
        });
    }

    #[gpui::test]
    fn main_and_settings_window_borders_follow_theme_and_fullscreen_state(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let (app, main_cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.window_persistence_enabled = false;
            app
        });
        app.update(main_cx, |app, cx| app.open_settings_window(cx));
        let settings = app.read_with(main_cx, |app, _| app.settings_window.unwrap());
        let mut settings_cx = VisualTestContext::from_window(settings.into(), &main_cx.cx);
        for cx in [main_cx, &mut settings_cx] {
            for selection in ColorTheme::ALL {
                cx.update(|_, cx| theme::set(selection, cx));
                for (width, height) in [(900.0, 600.0), (1100.0, 720.0)] {
                    cx.simulate_resize(size(px(width), px(height)));
                    assert_border(cx, width, height, !cfg!(target_os = "windows"));
                    cx.update(|window, _| {
                        window.toggle_fullscreen();
                        window.refresh();
                    });
                    assert_border(cx, width, height, false);
                    cx.update(|window, _| {
                        window.toggle_fullscreen();
                        window.refresh();
                    });
                    assert_border(cx, width, height, !cfg!(target_os = "windows"));
                }
            }
        }
    }
}
