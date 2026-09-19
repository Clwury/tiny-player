use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window,
    WindowControlArea, div, img, prelude::FluentBuilder, px,
};

#[cfg(not(target_os = "windows"))]
use gpui::svg;
#[cfg(target_os = "windows")]
use gpui::{StatefulInteractiveElement, rgb, white};

use crate::{app::window_has_rounded_corners, app_metadata::APP_ICON_ASSET_PATH, theme};

pub(crate) const APP_TITLEBAR_HEIGHT_PX: f32 = 35.0;

pub fn app_titlebar(window: &Window, cx: &App, title: SharedString) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("titlebar")
        .relative()
        .flex()
        .h(px(APP_TITLEBAR_HEIGHT_PX))
        .w_full()
        .items_center()
        .justify_center()
        .border_b_1()
        .border_color(theme.title_bar_border)
        .bg(theme.title_bar)
        .when(window_has_rounded_corners(window), |this| {
            this.rounded_tl(theme.radius_lg).rounded_tr(theme.radius_lg)
        })
        .when(!cfg!(target_os = "windows"), |this| {
            // Windows handles non-client dragging and double clicks in GPUI's
            // platform backend, including restoring a maximized window.
            this.on_mouse_down(MouseButton::Left, |event, window, _| {
                if event.click_count == 2 {
                    window.zoom_window();
                }
            })
            .on_mouse_move(|event, window, _| {
                if event.dragging() {
                    window.start_window_move();
                }
            })
        })
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .right_0()
                .window_control_area(WindowControlArea::Drag),
        )
        .child(
            div()
                .absolute()
                .left_2()
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .child(img(APP_ICON_ASSET_PATH).size(px(20.0))),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(title),
        )
        .child(window_controls(window, cx))
}

#[cfg(not(target_os = "windows"))]
fn window_controls(window: &Window, cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .right_2()
        .top_0()
        .bottom_0()
        .flex()
        .items_center()
        .gap_1()
        .child(window_control_button(
            "minimize",
            "icons/window-minimize.svg",
            WindowControlArea::Min,
            cx,
            |window, _| window.minimize_window(),
        ))
        .child(window_control_button(
            "maximize",
            if window.is_maximized() {
                "icons/window-restore.svg"
            } else {
                "icons/window-maximize.svg"
            },
            WindowControlArea::Max,
            cx,
            |window, _| window.zoom_window(),
        ))
        .child(window_control_button(
            "close",
            "icons/window-close.svg",
            WindowControlArea::Close,
            cx,
            |window, _| window.remove_window(),
        ))
}

#[cfg(not(target_os = "windows"))]
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    control_area: WindowControlArea,
    cx: &App,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_base(id, icon_path.into(), cx)
        .window_control_area(control_area)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
            action(window, cx);
        })
}

#[cfg(not(target_os = "windows"))]
fn button_base(id: &'static str, icon_path: SharedString, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .debug_selector(move || format!("window-control-{id}"))
        .flex()
        .size(px(24.0))
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme.foreground)
        .hover(move |style| style.rounded_full().bg(theme.secondary_hover))
        .child(
            svg()
                .path(icon_path)
                .size(px(14.0))
                .text_color(theme.foreground),
        )
}

#[cfg(target_os = "windows")]
const WINDOWS_CAPTION_BUTTON_WIDTH_PX: f32 = 36.0;

#[cfg(target_os = "windows")]
fn window_controls(window: &Window, cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .right_0()
        .top_0()
        .bottom_0()
        .flex()
        .child(windows_caption_button(
            "minimize",
            "\u{e921}",
            WindowControlArea::Min,
            window.is_minimizable(),
            cx,
        ))
        .child(windows_caption_button(
            "maximize",
            if window.is_maximized() {
                "\u{e923}"
            } else {
                "\u{e922}"
            },
            WindowControlArea::Max,
            window.is_resizable(),
            cx,
        ))
        .child(windows_caption_button(
            "close",
            "\u{e8bb}",
            WindowControlArea::Close,
            true,
            cx,
        ))
}

#[cfg(target_os = "windows")]
fn windows_caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    enabled: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let (hover, pressed, foreground) = if area == WindowControlArea::Close {
        (rgb(0xe81123).into(), rgb(0xc50f1f).into(), white())
    } else {
        (
            theme.secondary_hover,
            theme.element_selected_hover,
            theme.foreground,
        )
    };
    div()
        .id(id)
        .debug_selector(move || format!("window-control-{id}"))
        .flex()
        .flex_none()
        .w(px(WINDOWS_CAPTION_BUTTON_WIDTH_PX))
        .h_full()
        .items_center()
        .justify_center()
        .font_family("Segoe MDL2 Assets")
        .text_size(px(10.0))
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(enabled, |this| {
            this.hover(move |style| style.bg(hover).text_color(foreground))
                .active(move |style| style.bg(pressed).text_color(foreground))
        })
        // Occlude the drag hitbox below. Let GPUI handle native non-client
        // clicks on release, as well as the Windows 11 snap-layout flyout.
        .occlude()
        .window_control_area(area)
        .child(glyph)
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use crate::theme::ColorTheme;
    use gpui::{Context, Corners, Modifiers, Render, ScaledPixels, TestAppContext, point, size};

    struct TitlebarWindow;

    impl Render for TitlebarWindow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(app_titlebar(window, cx, "Tiny Player".into()))
        }
    }

    #[gpui::test]
    fn windows_caption_buttons_fill_the_right_edge_and_follow_theme(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let (_, cx) = cx.add_window_view(|_, _| TitlebarWindow);
        for selection in ColorTheme::ALL {
            cx.update(|_, cx| theme::set(selection, cx));
            for width in [900.0, 1100.0] {
                cx.simulate_resize(size(px(width), px(600.0)));
                cx.run_until_parked();
                for (index, selector) in [
                    "window-control-minimize",
                    "window-control-maximize",
                    "window-control-close",
                ]
                .into_iter()
                .enumerate()
                {
                    let bounds = cx.debug_bounds(selector).unwrap();
                    assert_eq!(
                        bounds.left(),
                        px(width - (3 - index) as f32 * WINDOWS_CAPTION_BUTTON_WIDTH_PX)
                    );
                    assert_eq!(bounds.top(), px(0.0));
                    assert_eq!(
                        bounds.size,
                        size(
                            px(WINDOWS_CAPTION_BUTTON_WIDTH_PX),
                            px(APP_TITLEBAR_HEIGHT_PX - 1.0)
                        )
                    );

                    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
                    cx.run_until_parked();
                    cx.update(|window, cx| {
                        let expected = if selector == "window-control-close" {
                            rgb(0xe81123).into()
                        } else {
                            theme::get(cx).secondary_hover
                        };
                        let quad = window
                            .painted_quads()
                            .into_iter()
                            .find(|quad| {
                                quad.bounds == bounds.scale(window.scale_factor())
                                    && quad.background == expected.into()
                            })
                            .expect("caption hover should fill a rectangular button");
                        assert_eq!(quad.corner_radii, Corners::all(ScaledPixels(0.0)));
                    });
                }
                cx.simulate_mouse_move(point(px(400.0), px(100.0)), None, Modifiers::default());
            }
        }
    }
}
