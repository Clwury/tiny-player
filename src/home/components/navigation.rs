//! Home navigation presentation components.
use super::*;

pub(in crate::home) fn home_section_title<T>(title: &'static str, cx: &Context<T>) -> gpui::Div {
    home_section_title_text(title, cx)
}

pub(in crate::home) fn home_section_title_text<T>(
    title: impl Into<gpui::SharedString>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(title.into())
}

pub(in crate::home) fn home_section_more_button(
    id: gpui::ElementId,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label("更多")
        .tooltip(|_, cx| text_tooltip("更多", cx))
        .flex()
        .flex_none()
        .size(px(28.0))
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            svg()
                .path("icons/ellipsis.svg")
                .size(px(18.0))
                .text_color(theme.foreground),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

pub(in crate::home) fn carousel_button(
    id: &'static str,
    icon_path: &'static str,
    align_right: bool,
    visible: bool,
    theme: &theme::TinyTheme,
    on_hover: impl Fn(&bool, &mut Window, &mut App) + 'static,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let foreground = theme.foreground;
    let background = theme.dialog_background;
    let hover = theme.secondary_hover;
    let pressed = hover.blend(foreground.opacity(0.08));

    div()
        .id((gpui::ElementId::from(id), "overlay"))
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(48.0))
        .flex()
        .items_center()
        .justify_center()
        .when(!align_right, |this| this.left_0())
        .when(align_right, |this| this.right_0())
        .occlude()
        .cursor_default()
        .on_hover(on_hover)
        .child(
            div()
                .id((gpui::ElementId::from(id), "button"))
                .flex()
                .size(px(32.0))
                .items_center()
                .justify_center()
                .rounded(radius::CONTROL)
                .bg(background.opacity(0.96))
                .shadow_sm()
                .opacity(if visible { 1.0 } else { 0.0 })
                .when(visible, |this| this.cursor_pointer())
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
                .child(svg().path(icon_path).size(px(18.0)).text_color(foreground))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    if visible {
                        on_click(event, window, cx);
                    }
                }),
        )
}

pub(in crate::home) fn workspace_back_button(
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("home-library-back-button")
        .debug_selector(|| "home-library-back-button".into())
        .flex()
        .size(px(32.0))
        // Match the sidebar's 32px button centered in a 36px title row.
        .my(px(2.0))
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .occlude()
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            svg()
                .path("icons/chevron-left.svg")
                .size(px(18.0))
                .text_color(theme.foreground),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(on_click)
}

pub(in crate::home) fn home_carousel_track(
    track: gpui::Div,
    animation_id: impl Into<gpui::ElementId>,
    previous_offset: f32,
    offset: f32,
) -> gpui::AnyElement {
    if previous_offset == offset {
        // Stationary rows need no animation state or extra frame requests,
        // including rows that reappear after being outside the viewport.
        track.ml(px(-offset)).into_any_element()
    } else {
        track
            .with_animation(
                animation_id,
                Animation::new(CAROUSEL_SCROLL_DURATION).with_easing(ease_in_out),
                move |track, delta| {
                    track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                },
            )
            .into_any_element()
    }
}

pub(in crate::home) fn tallest_home_item(items: &[UserItem]) -> Option<&UserItem> {
    items.iter().max_by_key(|item| {
        (
            item.item_type.as_deref() != Some("Episode"),
            item.production_year.is_some(),
        )
    })
}
