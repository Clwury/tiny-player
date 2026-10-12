//! Stateless playback elements. The page supplies display values and binds
//! intents; components never read the page, session or backend.
use super::playback_volume_percent;
use crate::{
    player::model::track_menu::{TrackMenuIntent, TrackMenuVm},
    theme,
    ui::radius,
};
use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Window, div, point, prelude::FluentBuilder,
    px, relative, rgba, svg,
};
use std::rc::Rc;
use tiny_playback::clamp_playback_volume;

const TRACK_SELECT_MAX_VISIBLE_OPTIONS: usize = 5;
const TRACK_SELECT_OPTION_HEIGHT_PX: f32 = 48.0;
const TRACK_SELECT_OPTION_GAP_PX: f32 = 4.0;
const TRACK_SELECT_MENU_PADDING_PX: f32 = 4.0;
const TRACK_SELECT_MENU_BORDER_PX: f32 = 1.0;
const VOLUME_INDICATOR_BAR_HEIGHT_PX: f32 = 192.0;

pub(super) fn reveal_track_select_option(scroll_handle: &ScrollHandle, option_index: usize) {
    // A newly opened menu has no viewport for scroll_to_item to inspect yet.
    // Keep the first five rows in view, or seed the selected row at the bottom.
    // Layout clamps the offset to the content.
    let first_visible_option = option_index.saturating_sub(TRACK_SELECT_MAX_VISIBLE_OPTIONS - 1);
    scroll_handle.set_offset(point(
        px(0.0),
        px(-(first_visible_option as f32)
            * (TRACK_SELECT_OPTION_HEIGHT_PX + TRACK_SELECT_OPTION_GAP_PX)),
    ));
}

pub(in crate::player::page) fn playback_control_button(
    id: &'static str,
    icon_path: &'static str,
    button_size: Pixels,
    icon_size: Pixels,
    enabled: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::media_overlay(cx);
    let color = if enabled {
        theme.foreground.opacity(0.92)
    } else {
        theme.foreground.opacity(0.52)
    };

    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .size(button_size)
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .text_color(color)
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(theme.foreground.opacity(0.14)))
        })
        .when(!enabled, |this| this.cursor_default().opacity(0.62))
        .child(svg().path(icon_path).size(icon_size).text_color(color))
}

pub(super) fn track_select_menu(
    vm: TrackMenuVm,
    scroll_handle: &ScrollHandle,
    cx: &App,
    on_select: impl Fn(&TrackMenuIntent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::media_overlay(cx);
    let on_select = Rc::new(on_select);
    let select_off = on_select.clone();
    let id = vm.id;
    let option_count = vm.options.len() + 1;
    let visible_options = option_count.min(TRACK_SELECT_MAX_VISIBLE_OPTIONS);
    let height = visible_options as f32 * TRACK_SELECT_OPTION_HEIGHT_PX
        + (visible_options - 1) as f32 * TRACK_SELECT_OPTION_GAP_PX
        + 2.0 * (TRACK_SELECT_MENU_PADDING_PX + TRACK_SELECT_MENU_BORDER_PX);
    let content = div()
        .id("playback-track-menu-content")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .gap(px(TRACK_SELECT_OPTION_GAP_PX))
        .overflow_hidden()
        .when(option_count > TRACK_SELECT_MAX_VISIBLE_OPTIONS, |this| {
            this.overflow_y_scroll()
        })
        .track_scroll(scroll_handle)
        .child(
            track_select_option(
                crate::i18n::tr("关闭", cx),
                "off",
                vm.off_selected,
                "playback-track-off-option".into(),
                cx,
            )
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                select_off(&vm.off_intent, window, cx);
            }),
        );
    let content = vm
        .options
        .into_iter()
        .enumerate()
        .fold(content, |content, (index, option)| {
            let on_select = on_select.clone();
            content.child(
                track_select_option(
                    crate::i18n::tr(option.label, cx),
                    option.metadata,
                    option.selected,
                    format!("playback-track-option-{index}"),
                    cx,
                )
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    on_select(&option.intent, window, cx);
                }),
            )
        });
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .absolute()
        .right_0()
        .bottom(px(32.0))
        .flex()
        .flex_col()
        .min_w(px(190.0))
        .max_w(px(280.0))
        .h(px(height))
        .rounded(radius::SURFACE)
        .border(px(TRACK_SELECT_MENU_BORDER_PX))
        .border_color(theme.input_border.opacity(0.72))
        .bg(rgba(0x000000e6))
        .p(px(TRACK_SELECT_MENU_PADDING_PX))
        .shadow_lg()
        .occlude()
        .cursor_default()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(content)
}

pub(in crate::player::page) fn volume_indicator(volume: f32, cx: &App) -> impl IntoElement {
    let theme = theme::media_overlay(cx);
    let volume = clamp_playback_volume(volume);
    let fill_height = VOLUME_INDICATOR_BAR_HEIGHT_PX * volume;
    let percent = playback_volume_percent(volume);

    div()
        .id("playback-volume-indicator")
        .absolute()
        .right(px(24.0))
        .top(relative(0.5))
        .mt(-px(106.0))
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .child(
            div()
                .relative()
                .w(px(8.0))
                .h(px(VOLUME_INDICATOR_BAR_HEIGHT_PX))
                .overflow_hidden()
                .rounded_full()
                .bg(theme.foreground.opacity(0.24))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(fill_height))
                        .rounded_full()
                        .bg(theme.input_border_focused),
                ),
        )
        .child(
            div()
                .w(px(42.0))
                .text_align(gpui::TextAlign::Center)
                .text_xs()
                .text_color(theme.foreground)
                .child(format!("{percent}%")),
        )
}

pub(super) fn cache_status_popover(segments: Vec<String>, cx: &App) -> impl IntoElement {
    let theme = theme::media_overlay(cx);
    segments.into_iter().fold(
        div()
            .id("playback-cache-status-popover")
            .cursor_default()
            .absolute()
            .right_0()
            .bottom(px(32.0))
            .flex()
            .flex_col()
            .min_w(px(176.0))
            .gap_1()
            .rounded(radius::SURFACE)
            .border_1()
            .border_color(theme.input_border.opacity(0.62))
            .bg(rgba(0x000000dd))
            .p_2()
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            }),
        |this, segment| {
            this.child(
                div()
                    .h(px(22.0))
                    .min_h(px(22.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(radius::CONTROL)
                    .bg(theme.foreground.opacity(0.08))
                    .px_2()
                    .text_xs()
                    .text_color(theme.foreground.opacity(0.9))
                    .child(crate::i18n::tr(segment, cx)),
            )
        },
    )
}

fn track_select_option(
    label: impl Into<SharedString>,
    metadata: impl Into<SharedString>,
    selected: bool,
    id: String,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::media_overlay(cx);
    let label = label.into();
    let metadata = metadata.into();
    let label_id = format!("{id}-label");
    let metadata_id = format!("{id}-metadata");
    let hover_background = if selected {
        theme.input_border_focused.opacity(0.34)
    } else {
        theme.foreground.opacity(0.12)
    };

    div()
        .id(id.clone())
        .debug_selector(move || id.clone())
        .flex()
        .flex_none()
        .h(px(TRACK_SELECT_OPTION_HEIGHT_PX))
        .min_h(px(TRACK_SELECT_OPTION_HEIGHT_PX))
        .items_center()
        .rounded(radius::CONTROL)
        .px_2()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.foreground
        } else {
            theme.foreground.opacity(0.86)
        })
        .bg(if selected {
            theme.input_border_focused.opacity(0.24)
        } else {
            theme.foreground.opacity(0.0)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_background))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.0))
                .child(
                    div()
                        .debug_selector(move || label_id.clone())
                        .truncate()
                        .line_height(px(18.0))
                        .child(label),
                )
                .child(
                    div()
                        .debug_selector(move || metadata_id.clone())
                        .truncate()
                        .text_xs()
                        .line_height(px(14.0))
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(theme.foreground.opacity(if selected { 0.9 } else { 0.72 }))
                        .child(crate::i18n::tr(metadata, cx)),
                ),
        )
}
