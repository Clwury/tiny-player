//! Reusable boolean switch.
use crate::theme;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, div,
    px,
};

pub(crate) fn toggle_switch(
    label: &'static str,
    selected: bool,
    on_toggle: impl Fn(&mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .id(label)
        .role(gpui::Role::Switch)
        .aria_label(label)
        .aria_toggled(if selected {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .debug_selector(move || format!("settings-toggle-{label}"))
        .group("settings-toggle")
        .flex()
        .items_center()
        .p(px(3.0))
        .cursor_pointer()
        .child(
            div()
                .id("switch-track")
                .flex()
                .items_center()
                .w(px(32.0))
                .h(px(20.0))
                .px(px(2.0))
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    theme.input_border_focused
                } else {
                    theme.input_border
                })
                .bg(if selected {
                    theme.accent
                } else {
                    theme.input_background
                })
                .group_hover("settings-toggle", |style| {
                    style
                        .bg(if selected {
                            theme.accent_hover
                        } else {
                            theme.secondary_hover
                        })
                        .border_color(theme.accent)
                })
                .when(selected, |this| this.justify_end())
                .child(div().size(px(12.0)).rounded_full().bg(if selected {
                    theme.accent_foreground
                } else {
                    theme.muted_foreground
                })),
        )
        .on_click(move |_, _, cx| on_toggle(cx))
}
