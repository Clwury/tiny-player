use gpui::{
    App, BorderStyle, Bounds, Context, Corners, Decorations, Edges, Hsla, IntoElement,
    ParentElement, Pixels, SharedString, Size, Styled, Tiling, TitlebarOptions, Window,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, canvas, div, point,
    px, quad,
};

use crate::{
    app_metadata::{APP_ID, APP_NAME},
    theme,
    ui::paint::{device_bounds, device_mask},
};

use super::{Page, TinyApp};

#[cfg(target_os = "windows")]
pub(super) mod windows;

const WINDOW_BORDER_WIDTH: Pixels = px(1.0);

/// Linux can fall back to system decorations even when we requested Client.
/// Windows and macOS retain their existing native/custom titlebar integration.
pub(crate) fn window_uses_system_decorations(window: &Window) -> bool {
    cfg!(target_os = "linux") && matches!(window.window_decorations(), Decorations::Server)
}

pub(super) fn sync_window_decorations(window: &mut Window, title: SharedString, cx: &mut App) {
    let last_title = window.use_keyed_state("window-decoration-title", cx, |window, _| {
        // Wayland decoration negotiation signals an appearance change. GPUI
        // notifies observers but does not itself invalidate the rendered root.
        window
            .observe_window_appearance(|window, _| window.refresh())
            .detach();
        None::<SharedString>
    });
    if last_title.read(cx).as_ref() != Some(&title) {
        window.set_window_title(&title);
        last_title.update(cx, |last_title, _| *last_title = Some(title));
    }
}

/// Only client-decorated, floating windows need an application-drawn frame.
pub(crate) fn window_has_rounded_corners(window: &Window) -> bool {
    !cfg!(target_os = "windows")
        && !window_uses_system_decorations(window)
        && !window.is_maximized()
        && !window.is_fullscreen()
}

pub(crate) fn window_corner_radii(window: &Window, cx: &App) -> Corners<Pixels> {
    // Content sits inside the frame. Keep the inner arcs concentric with the
    // outer arcs so child backgrounds cannot repaint their antialiased edge.
    window_frame_corner_radii(window, cx)
        .map(|radius| (*radius - window_border_width(window)).max(px(0.0)))
}

fn window_border_width(window: &Window) -> Pixels {
    // Match GPUI's stroke snapping at fractional display scales so layout,
    // border paint and the inner corner radii agree on the same device pixels.
    window
        .pixel_snap(WINDOW_BORDER_WIDTH)
        .max(px(1.0 / window.scale_factor()))
}

fn window_frame_corner_radii(window: &Window, cx: &App) -> Corners<Pixels> {
    if !window_has_rounded_corners(window) {
        return Corners::default();
    }

    if cfg!(target_os = "linux") {
        match window.window_decorations() {
            Decorations::Client { tiling } => linux_window_corner_radii(tiling),
            Decorations::Server => Corners::default(),
        }
    } else {
        Corners::all(theme::get(cx).radius_lg)
    }
}

fn linux_window_corner_radii(tiling: Tiling) -> Corners<Pixels> {
    // Match Zed's CLIENT_SIDE_DECORATION_ROUNDING and rounded_client_corners:
    // a corner is rounded only when neither adjacent edge is tiled.
    let radius = px(10.0);
    Corners {
        top_left: if tiling.top || tiling.left {
            px(0.0)
        } else {
            radius
        },
        top_right: if tiling.top || tiling.right {
            px(0.0)
        } else {
            radius
        },
        bottom_left: if tiling.bottom || tiling.left {
            px(0.0)
        } else {
            radius
        },
        bottom_right: if tiling.bottom || tiling.right {
            px(0.0)
        } else {
            radius
        },
    }
}

pub(crate) trait WindowCornersExt: Styled {
    fn rounded_window_corners(self, corners: Corners<Pixels>) -> Self {
        self.rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
    }
}

impl<T: Styled> WindowCornersExt for T {}

fn window_frame_borders(window: &Window) -> Edges<Pixels> {
    if !window_has_rounded_corners(window) {
        return Edges::default();
    }
    let tiling = match window.window_decorations() {
        Decorations::Client { tiling } if cfg!(target_os = "linux") => tiling,
        _ => Tiling::default(),
    };
    untiled_border_widths(tiling, window_border_width(window))
}

fn untiled_border_widths(tiling: Tiling, border_width: Pixels) -> Edges<Pixels> {
    let width = |tiled| if tiled { px(0.0) } else { border_width };
    Edges {
        top: width(tiling.top),
        right: width(tiling.right),
        bottom: width(tiling.bottom),
        left: width(tiling.left),
    }
}

#[derive(Clone, Copy)]
pub(super) struct WindowFrameColors {
    pub background: Hsla,
    pub top: Hsla,
    pub bottom_left: Hsla,
}

pub(super) fn window_frame(
    content: impl IntoElement,
    colors: WindowFrameColors,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let borders = window_frame_borders(window);
    let corners = window_frame_corner_radii(window, cx);
    window_frame_surface(content, borders, corners, colors, cx)
}

fn window_frame_surface(
    content: impl IntoElement,
    borders: Edges<Pixels>,
    corners: Corners<Pixels>,
    colors: WindowFrameColors,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let border_color = theme.window_border;
    // Like Zed's client_side_decorations, reserve space for the border in
    // layout. Div paints its fill and border separately; paint both in the
    // same quad to avoid repeatedly blending the outer arc's coverage.
    div()
        .relative()
        .size_full()
        .border_t(borders.top)
        .border_r(borders.right)
        .border_b(borders.bottom)
        .border_l(borders.left)
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    // An absolutely positioned child fills the padding box;
                    // extend back across the layout-only border to the frame.
                    let bounds = Bounds::from_corners(
                        point(bounds.left() - borders.left, bounds.top() - borders.top),
                        point(
                            bounds.right() + borders.right,
                            bounds.bottom() + borders.bottom,
                        ),
                    );
                    // Each corner is backed by its actual surface color. The
                    // masks partition device pixels, so fill and border still
                    // share exactly one outer-arc coverage calculation.
                    let scale = window.scale_factor();
                    for (region, background) in frame_background_regions(
                        device_bounds(window, bounds),
                        corners.map(|radius| f32::from(*radius) * scale),
                        colors,
                    ) {
                        window.with_content_mask(Some(device_mask(region, scale)), |window| {
                            window.paint_quad(quad(
                                bounds,
                                corners,
                                background,
                                borders,
                                border_color,
                                BorderStyle::Solid,
                            ));
                        });
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .child(content)
}

fn frame_background_regions(
    bounds: Bounds<i32>,
    corners: Corners<f32>,
    colors: WindowFrameColors,
) -> Vec<(Bounds<i32>, Hsla)> {
    let mut regions = Vec::with_capacity(4);
    let mut remaining = bounds;
    let top_height = corners.top_left.max(corners.top_right).ceil() as i32;
    if colors.top != colors.background && top_height > 0 {
        let bottom = (bounds.top() + top_height).min(bounds.bottom());
        regions.push((
            Bounds::from_corners(bounds.origin, point(bounds.right(), bottom)),
            colors.top,
        ));
        remaining = Bounds::from_corners(point(bounds.left(), bottom), bounds.bottom_right());
    }
    let left_size = corners.bottom_left.ceil() as i32;
    if colors.bottom_left != colors.background && left_size > 0 {
        let top = (bounds.bottom() - left_size).max(remaining.top());
        let right = (bounds.left() + left_size).min(bounds.right());
        regions.push((
            Bounds::from_corners(remaining.origin, point(bounds.right(), top)),
            colors.background,
        ));
        regions.push((
            Bounds::from_corners(point(bounds.left(), top), point(right, bounds.bottom())),
            colors.bottom_left,
        ));
        remaining = Bounds::from_corners(point(right, top), bounds.bottom_right());
    }
    regions.push((remaining, colors.background));
    regions.retain(|(bounds, _)| !bounds.is_empty());
    regions
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
    fn linux_window_corners_follow_zeds_radius_and_tiled_edges() {
        let rounded = px(10.0);
        let square = px(0.0);
        assert_eq!(
            linux_window_corner_radii(Tiling::default()),
            Corners::all(rounded)
        );
        assert_eq!(
            linux_window_corner_radii(Tiling::tiled()),
            Corners::all(square)
        );
        for ([top, left, right, bottom], expected) in [
            (
                [true, false, false, false],
                [square, square, rounded, rounded],
            ),
            (
                [false, false, false, true],
                [rounded, rounded, square, square],
            ),
            (
                [false, true, false, false],
                [square, rounded, square, rounded],
            ),
            (
                [false, false, true, false],
                [rounded, square, rounded, square],
            ),
            (
                [true, true, false, false],
                [square, square, square, rounded],
            ),
            ([false, true, true, false], [square; 4]),
            ([true, false, false, true], [square; 4]),
        ] {
            let tiling = Tiling {
                top,
                left,
                right,
                bottom,
            };
            let corners = linux_window_corner_radii(tiling);
            assert_eq!(
                [
                    corners.top_left,
                    corners.top_right,
                    corners.bottom_left,
                    corners.bottom_right
                ],
                expected,
                "{tiling:?}",
            );
        }
    }

    #[test]
    fn tiled_edges_do_not_reserve_space_for_a_window_border() {
        for bits in 0..16 {
            let tiling = Tiling {
                top: bits & 1 != 0,
                right: bits & 2 != 0,
                bottom: bits & 4 != 0,
                left: bits & 8 != 0,
            };
            let borders = untiled_border_widths(tiling, px(1.0));
            for (tiled, width) in [
                (tiling.top, borders.top),
                (tiling.right, borders.right),
                (tiling.bottom, borders.bottom),
                (tiling.left, borders.left),
            ] {
                assert_eq!(width, px(if tiled { 0.0 } else { 1.0 }));
            }
        }
    }

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
            if visible {
                // Different corner colors may partition the frame, but each
                // device pixel must retain a single fill/border calculation.
                assert_eq!(
                    quads.iter().filter(|quad| quad.bounds == bounds).count(),
                    borders.len()
                );
                for (index, border) in borders.iter().enumerate() {
                    for other in &borders[index + 1..] {
                        assert!(
                            border
                                .content_mask
                                .bounds
                                .intersect(&other.content_mask.bounds)
                                .is_empty()
                        );
                    }
                }
                let area: f32 = borders
                    .iter()
                    .map(|border| {
                        let mask = border.content_mask.bounds;
                        mask.size.width.0 * mask.size.height.0
                    })
                    .sum();
                assert_eq!(area, width * height * scale * scale);
                let content_bounds = Bounds::new(
                    point(px(1.0), px(1.0)),
                    size(px(width - 2.0), px(height - 2.0)),
                )
                .scale(scale);
                for quad in quads.iter().filter(|quad| quad.bounds != bounds) {
                    let painted = quad.bounds.intersect(&quad.content_mask.bounds);
                    assert_eq!(painted.intersect(&content_bounds), painted);
                }
            }
            for border in &borders {
                assert_eq!(border.border_color, theme.window_border);
                assert_eq!(border.border_widths, Edges::all(ScaledPixels(scale)));
                assert_eq!(
                    border.corner_radii,
                    Corners::all(ScaledPixels(if cfg!(target_os = "linux") {
                        10.0 * scale
                    } else {
                        f32::from(theme.radius_lg) * scale
                    }))
                );
                let mask = border.content_mask.bounds;
                assert!(mask.left() >= bounds.left() && mask.right() <= bounds.right());
                assert!(mask.top() >= bounds.top() && mask.bottom() <= bounds.bottom());
            }
            if visible {
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
                    // TestWindow reports Server, including on Linux.
                    let client_frame = !cfg!(any(target_os = "windows", target_os = "linux"));
                    assert_border(cx, width, height, client_frame);
                    cx.update(|window, _| {
                        window.toggle_fullscreen();
                        window.refresh();
                    });
                    assert_border(cx, width, height, false);
                    cx.update(|window, _| {
                        window.toggle_fullscreen();
                        window.refresh();
                    });
                    assert_border(cx, width, height, client_frame);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn server_decorations_remove_client_chrome_from_main_and_settings(cx: &mut TestAppContext) {
        use crate::ui::settings_dialog::SettingsDialogMode;
        use gpui::{Modifiers, MouseButton};

        cx.update(theme::init);
        let (app, main_cx) = cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(ServerCache::empty(), None, cx);
            app.window_persistence_enabled = false;
            app
        });
        app.update(main_cx, |app, cx| {
            app.open_settings_window_with_mode(SettingsDialogMode::User, cx)
        });
        let settings = app.read_with(main_cx, |app, _| app.settings_window.unwrap());
        let mut settings_cx = VisualTestContext::from_window(settings.into(), &main_cx.cx);
        for cx in [main_cx, &mut settings_cx] {
            cx.simulate_resize(size(px(960.0), px(680.0)));
            for fullscreen in [false, true, false] {
                cx.update(|window, _| {
                    if window.is_fullscreen() != fullscreen {
                        window.toggle_fullscreen();
                        window.refresh();
                    }
                });
                cx.run_until_parked();
                cx.update(|window, cx| {
                    // TestWindow reports the actual Server mode regardless of
                    // the Client preference passed to open_window.
                    assert_eq!(window.window_decorations(), Decorations::Server);
                    assert!(!window_has_rounded_corners(window));
                    assert_eq!(window_corner_radii(window, cx), Corners::default());
                    assert_eq!(window_frame_borders(window), Edges::default());
                    let bounds = Bounds::new(point(px(0.0), px(0.0)), window.viewport_size())
                        .scale(window.scale_factor());
                    let fills: Vec<_> = window
                        .painted_quads()
                        .into_iter()
                        .filter(|quad| quad.bounds == bounds && !quad.background.is_transparent())
                        .collect();
                    assert!(!fills.is_empty());
                    for fill in fills {
                        assert_eq!(fill.corner_radii, Corners::default());
                        assert_eq!(fill.border_widths, Edges::default());
                    }
                });
                for selector in [
                    "app-titlebar",
                    "window-control-minimize",
                    "window-control-maximize",
                    "window-control-close",
                    "window-resize-Top",
                    "window-resize-Right",
                    "window-resize-Bottom",
                    "window-resize-Left",
                    "window-resize-TopLeft",
                    "window-resize-TopRight",
                    "window-resize-BottomLeft",
                    "window-resize-BottomRight",
                ] {
                    assert!(cx.debug_bounds(selector).is_none(), "{selector}");
                }
                if let Some(panel) = cx.debug_bounds("user-settings-panel") {
                    assert_eq!(
                        panel,
                        Bounds::new(point(px(0.0), px(0.0)), size(px(960.0), px(680.0)))
                    );
                }
                // A leaked resize hot zone would call the unsupported native
                // resize method in TestWindow and panic on these presses.
                for position in [point(px(1.0), px(1.0)), point(px(959.0), px(679.0))] {
                    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
                    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn client_frame_still_paints_the_outer_arc_once(cx: &mut TestAppContext) {
        use gpui::Render;

        struct ClientFrame;
        impl Render for ClientFrame {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                // TestWindow cannot negotiate Client mode; exercise the same
                // frame surface with the client geometry explicitly supplied.
                window_frame_surface(
                    div()
                        .size_full()
                        .bg(theme::get(cx).background)
                        .rounded(px(9.0)),
                    Edges::all(px(1.0)),
                    linux_window_corner_radii(Tiling::default()),
                    WindowFrameColors {
                        background: theme::get(cx).background,
                        top: theme::get(cx).background,
                        bottom_left: theme::get(cx).background,
                    },
                    cx,
                )
            }
        }
        cx.update(theme::init);
        let (_, cx) = cx.add_window_view(|_, _| ClientFrame);
        for selection in ColorTheme::ALL {
            cx.update(|_, cx| theme::set(selection, cx));
            for (width, height) in [(900.0, 600.0), (1100.0, 720.0)] {
                cx.simulate_resize(size(px(width), px(height)));
                assert_border(cx, width, height, true);
            }
        }
    }

    #[gpui::test]
    fn frame_corner_colors_partition_device_pixels_at_fractional_scales(cx: &mut TestAppContext) {
        use gpui::{Render, black};

        struct ColoredFrame {
            playback: bool,
        }
        impl Render for ColoredFrame {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let theme = theme::get(cx);
                window_frame_surface(
                    div().size_full(),
                    Edges::all(window_border_width(window)),
                    Corners::all(px(10.0)),
                    WindowFrameColors {
                        background: if self.playback {
                            black()
                        } else {
                            theme.background
                        },
                        top: theme.title_bar,
                        bottom_left: if self.playback {
                            black()
                        } else {
                            theme.panel_background
                        },
                    },
                    cx,
                )
            }
        }
        cx.update(theme::init);
        let (root, cx) = cx.add_window_view(|_, _| ColoredFrame { playback: false });
        for selection in ColorTheme::ALL {
            cx.update(|_, cx| theme::set(selection, cx));
            for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
                for playback in [false, true] {
                    root.update(cx, |root, cx| {
                        root.playback = playback;
                        cx.notify();
                    });
                    cx.simulate_resize(size(px(101.0), px(79.0)));
                    cx.update(|window, _| {
                        window.set_scale_factor(scale);
                        window.refresh();
                    });
                    cx.run_until_parked();
                    cx.update(|window, cx| {
                        let theme = theme::get(cx);
                        let body = if playback { black() } else { theme.background };
                        let left = if playback {
                            black()
                        } else {
                            theme.panel_background
                        };
                        let bounds = device_bounds(
                            window,
                            Bounds::new(point(px(0.0), px(0.0)), window.viewport_size()),
                        );
                        let quads = window.painted_quads();
                        for y in bounds.top()..bounds.bottom() {
                            for x in bounds.left()..bounds.right() {
                                let position = point(
                                    ScaledPixels(x as f32 + 0.5),
                                    ScaledPixels(y as f32 + 0.5),
                                );
                                let covering: Vec<_> = quads
                                    .iter()
                                    .filter(|quad| quad.content_mask.bounds.contains(&position))
                                    .collect();
                                assert_eq!(covering.len(), 1, "scale={scale}, pixel=({x}, {y})");
                                let radius = (10.0_f32 * scale).ceil() as i32;
                                let color = if y < radius {
                                    theme.title_bar
                                } else if x < radius && y >= bounds.bottom() - radius {
                                    left
                                } else {
                                    body
                                };
                                assert_eq!(covering[0].background, color.into());
                            }
                        }
                    });
                }
            }
        }
    }
}
