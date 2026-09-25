use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, WindowControlArea, div, img,
    prelude::FluentBuilder, px,
};

#[cfg(not(target_os = "windows"))]
use gpui::svg;
#[cfg(target_os = "windows")]
use gpui::{rgb, white};

use crate::{app::window_corner_radii, app_metadata::APP_ICON_ASSET_PATH, theme};

pub(crate) const APP_TITLEBAR_HEIGHT_PX: f32 = 35.0;

pub fn app_titlebar(window: &Window, cx: &App, title: SharedString) -> impl IntoElement {
    let theme = theme::get(cx);
    let corners = window_corner_radii(window, cx);

    div()
        .id("titlebar")
        .debug_selector(|| "app-titlebar".into())
        .relative()
        .flex()
        .h(px(APP_TITLEBAR_HEIGHT_PX))
        .w_full()
        .items_center()
        .justify_center()
        .border_b_1()
        .border_color(theme.title_bar_border)
        .bg(theme.title_bar)
        .rounded_tl(corners.top_left)
        .rounded_tr(corners.top_right)
        .cursor_default()
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
        .right_0()
        .top_0()
        .bottom_0()
        .flex()
        .items_center()
        // Match Zed's LinuxWindowControls, including padding around the group.
        .gap_3()
        .px_3()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(window_control_button(
            "minimize",
            "icons/window-control-minimize.svg",
            WindowControlArea::Min,
            window.is_minimizable(),
            cx,
            |window, _| window.minimize_window(),
        ))
        .child(window_control_button(
            "maximize",
            if window.is_maximized() {
                "icons/window-control-restore.svg"
            } else {
                "icons/window-control-maximize.svg"
            },
            WindowControlArea::Max,
            window.is_resizable(),
            cx,
            |window, _| window.zoom_window(),
        ))
        .child(window_control_button(
            "close",
            "icons/window-control-close.svg",
            WindowControlArea::Close,
            true,
            cx,
            |window, _| window.remove_window(),
        ))
}

#[cfg(not(target_os = "windows"))]
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    control_area: WindowControlArea,
    enabled: bool,
    cx: &App,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_base(id, icon_path.into(), enabled, cx)
        .window_control_area(control_area)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            if enabled {
                action(window, cx);
            }
        })
}

#[cfg(not(target_os = "windows"))]
fn button_base(
    id: &'static str,
    icon_path: SharedString,
    enabled: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .group(id)
        .debug_selector(move || format!("window-control-{id}"))
        .flex()
        .flex_none()
        .size_5()
        .items_center()
        .justify_center()
        // Window controls use Zed's circular 20px style, independently of buttons.
        .rounded_2xl()
        .text_color(theme.foreground)
        .cursor_default()
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(theme.secondary_hover))
                .active(move |style| style.bg(theme.secondary_hover))
        })
        .child(
            svg()
                .path(icon_path)
                .size_4()
                .flex_none()
                .text_color(if enabled {
                    theme.foreground
                } else {
                    theme.muted_foreground
                })
                .when(enabled, |this| {
                    this.group_hover(id, move |style| style.text_color(theme.muted_foreground))
                }),
        )
}

#[cfg(target_os = "windows")]
const WINDOWS_CAPTION_BUTTON_WIDTH_PX: f32 = 36.0;

#[cfg(target_os = "windows")]
fn windows_caption_font(cx: &App) -> &'static str {
    static FONT: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        // Match Zed's Windows 11 icons, falling back on older Windows installs.
        if cx
            .text_system()
            .all_font_names()
            .iter()
            .any(|name| name == "Segoe Fluent Icons")
        {
            "Segoe Fluent Icons"
        } else {
            "Segoe MDL2 Assets"
        }
    })
}

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
    let (hover, pressed, foreground, pressed_foreground) = if area == WindowControlArea::Close {
        let close: gpui::Hsla = rgb(0xe81120).into();
        (close, close.opacity(0.8), white(), white().opacity(0.8))
    } else {
        (
            theme.secondary_hover,
            theme.element_selected_hover,
            theme.foreground,
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
        .font_family(windows_caption_font(cx))
        .text_size(px(10.0))
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .cursor_default()
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(hover).text_color(foreground))
                .active(move |style| style.bg(pressed).text_color(pressed_foreground))
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
                            rgb(0xe81120).into()
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

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use gpui::{Context, Modifiers, Render, TestAppContext, point};

    struct ControlWindow {
        enabled: bool,
        activations: Rc<Cell<usize>>,
        titlebar_drags: Rc<Cell<usize>>,
    }

    impl Render for ControlWindow {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let activations = self.activations.clone();
            let titlebar_drags = self.titlebar_drags.clone();
            div()
                .size_full()
                .on_mouse_move(move |event, _, _| {
                    if event.dragging() {
                        titlebar_drags.set(titlebar_drags.get() + 1);
                    }
                })
                .child(window_control_button(
                    "test",
                    "icons/window-control-maximize.svg",
                    WindowControlArea::Max,
                    self.enabled,
                    cx,
                    move |_, _| activations.set(activations.get() + 1),
                ))
        }
    }

    #[gpui::test]
    fn caption_clicks_activate_on_release_without_dragging_the_titlebar(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let activations = Rc::new(Cell::new(0));
        let titlebar_drags = Rc::new(Cell::new(0));
        let (root, cx) = cx.add_window_view(|_, _| ControlWindow {
            enabled: true,
            activations: activations.clone(),
            titlebar_drags: titlebar_drags.clone(),
        });
        let bounds = cx.debug_bounds("window-control-test").unwrap();
        let position = bounds.center();
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        assert_eq!(
            activations.get(),
            0,
            "press must leave time for active feedback"
        );
        cx.simulate_mouse_move(position, Some(MouseButton::Left), Modifiers::default());
        assert_eq!(titlebar_drags.get(), 0);
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
        assert_eq!(activations.get(), 1);

        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        let outside = bounds.bottom_right() + point(px(20.0), px(20.0));
        cx.simulate_mouse_move(outside, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(outside, MouseButton::Left, Modifiers::default());
        assert_eq!(
            activations.get(),
            1,
            "releasing outside must cancel the action"
        );

        root.update(cx, |root, cx| {
            root.enabled = false;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_click(position, Modifiers::default());
        assert_eq!(activations.get(), 1, "disabled controls must not activate");
    }
}
