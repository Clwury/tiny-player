use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, Styled, Window,
    WindowControlArea, div, prelude::FluentBuilder, px, svg,
};

use crate::theme;

pub fn app_titlebar(window: &Window, cx: &App, title: SharedString) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("titlebar")
        .relative()
        .flex()
        .h(px(35.0))
        .w_full()
        .items_center()
        .justify_center()
        .border_b_1()
        .border_color(theme.title_bar_border)
        .bg(theme.title_bar)
        .when(!window.is_maximized() && !window.is_fullscreen(), |this| {
            this.rounded_tl(theme.radius_lg).rounded_tr(theme.radius_lg)
        })
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            if event.click_count == 2 {
                window.zoom_window();
            }
        })
        .on_mouse_move(|event, window, _| {
            if event.dragging() {
                window.start_window_move();
            }
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
        .child(
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
                )),
        )
}

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

fn button_base(id: &'static str, icon_path: SharedString, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
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
