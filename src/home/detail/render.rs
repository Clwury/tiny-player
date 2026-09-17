use gpui::{
    Animation, AnimationExt as _, Context, InteractiveElement, InteractiveText, IntoElement,
    MouseButton, ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, StyledImage,
    StyledText, Transformation, Window, deferred, div, ease_in_out, img, percentage,
    prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{MediaItem, MediaItems, MediaPerson, UserItems},
    theme,
    ui::{
        scrollbar::{SCROLLBAR_WIDTH_PX, Scrollbar},
        tooltip::text_tooltip,
    },
};

use super::super::{
    HomeContent, LoadState,
    carousel::{
        DETAIL_EPISODE_CARD_GAP_PX, DETAIL_EPISODE_CARD_PADDING_PX, DETAIL_EPISODE_CARD_WIDTH_PX,
        DETAIL_PERSON_CARD_GAP_PX, DETAIL_PERSON_CARD_PADDING_PX, DETAIL_PERSON_CARD_WIDTH_PX,
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, carousel_content_width_for,
        carousel_visible_range_between_for, home_main_content_width,
        max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, episode_card, format_community_rating, home_section_title, person_card,
        user_item_card,
    },
};
use super::{SeriesDetailSelectKind, SeriesDetailState};

const DETAIL_SELECT_MAX_VISIBLE_OPTIONS: usize = 5;
const DETAIL_SELECT_OPTION_HEIGHT_PX: f32 = 28.0;
const DETAIL_TWO_LINE_OPTION_HEIGHT_PX: f32 = 48.0;
const DETAIL_SELECT_WIDTH_PX: f32 = 250.0;
const DETAIL_SELECT_TOOLTIP_WIDTH_UNITS: usize = 30;
const SEASON_SELECT_MIN_WIDTH_PX: f32 = 100.0;
const SEASON_SELECT_MAX_WIDTH_PX: f32 = 320.0;
const SEASON_SELECT_HORIZONTAL_PADDING_PX: f32 = 8.0;

pub(super) fn reveal_two_line_option(scroll_handle: &ScrollHandle, option_index: usize) {
    // On first open, GPUI's scroll_to_item has no previous viewport to inspect.
    // Seed the offset from our fixed row heights; layout clamps it to the content.
    let first_visible = option_index.saturating_sub(DETAIL_SELECT_MAX_VISIBLE_OPTIONS - 1);
    scroll_handle.set_offset(gpui::point(
        px(0.0),
        px(-(first_visible as f32) * (DETAIL_TWO_LINE_OPTION_HEIGHT_PX + 4.0)),
    ));
}

#[path = "render/controls.rs"]
mod controls;
#[path = "render/episodes.rs"]
mod episodes;
#[path = "render/hero.rs"]
mod hero;
#[path = "render/people.rs"]
mod people;
#[path = "render/similar.rs"]
mod similar;
#[path = "render/video_metadata.rs"]
mod video_metadata;

fn has_studios(item: &MediaItem) -> bool {
    item.studios
        .as_deref()
        .is_some_and(|studios| studios.iter().any(|studio| studio.name().is_some()))
}

fn detail_tag<T>(label: String, clickable: bool, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background.opacity(0.86))
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .when(clickable, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(theme.secondary_hover))
        })
        .child(label)
}

fn detail_select_box<T>(
    label: &'static str,
    value: String,
    enabled: bool,
    cx: &Context<T>,
) -> gpui::Div {
    detail_select_box_with_width(label, value, enabled, DETAIL_SELECT_WIDTH_PX, cx)
}

fn detail_select_box_with_width<T>(
    label: &'static str,
    value: String,
    enabled: bool,
    width: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let text = StyledText::new(value.clone());
    let text_layout = text.layout().clone();
    let text = InteractiveText::new((gpui::ElementId::from("detail-select-value"), label), text)
        .tooltip(move |_, _, cx| {
            // Compare the rendered text (including any ellipsis) with the original
            // so the tooltip follows the actual available width and font metrics.
            (text_layout.text() != value).then(|| text_tooltip(value.clone(), cx))
        });

    div()
        .flex()
        .h(px(34.0))
        .w(px(width))
        .max_w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(px(8.0))
        .border_1()
        .border_color(if enabled {
            theme.input_border
        } else {
            theme.input_border.opacity(0.62)
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px_3()
        .text_sm()
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(enabled, |this| {
            this.hover(move |style| style.bg(theme.secondary_hover))
        })
        .when(!enabled, |this| this.opacity(0.62))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(text),
                ),
        )
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .size(px(14.0))
                .flex_none()
                .text_color(theme.muted_foreground),
        )
}

fn season_popup_menu_trigger<T>(
    value: String,
    enabled: bool,
    width: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let text = StyledText::new(value.clone());
    let text_layout = text.layout().clone();
    let text = InteractiveText::new("series-detail-season-value", text).tooltip(move |_, _, cx| {
        (text_layout.text() != value).then(|| text_tooltip(value.clone(), cx))
    });

    div()
        .flex()
        .h(px(32.0))
        .w(px(width))
        .max_w_full()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .border_1()
        .border_color(if enabled {
            theme.input_border
        } else {
            theme.input_border.opacity(0.62)
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px(px(SEASON_SELECT_HORIZONTAL_PADDING_PX))
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(enabled, |this| {
            this.hover(move |style| style.bg(theme.secondary_hover))
        })
        .when(!enabled, |this| this.opacity(0.62))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .debug_selector(|| "series-detail-season-value".into())
                .flex_1()
                .min_w_0()
                .text_center()
                .truncate()
                .child(text),
        )
}

fn detail_select_menu<T, I, E>(
    id: &'static str,
    option_count: usize,
    width: f32,
    option_height: f32,
    scroll_handle: &ScrollHandle,
    cx: &Context<T>,
    children: I,
) -> impl IntoElement
where
    I: IntoIterator<Item = E>,
    E: IntoElement,
{
    let theme = theme::get(cx);
    let scrollable = detail_select_menu_is_scrollable(option_count);
    let content_scroll_handle = scroll_handle.clone();
    let scrollbar_scroll_handle = scroll_handle.clone();

    div()
        .id(id)
        .debug_selector(move || id.into())
        .absolute()
        .top(px(40.0))
        .left_0()
        .flex()
        .flex_col()
        .w(px(width))
        .max_w_full()
        .when(scrollable, |this| {
            this.h(px(option_height * DETAIL_SELECT_MAX_VISIBLE_OPTIONS as f32
                + 4.0 * 4.0
                + 6.0 * 2.0))
        })
        .overflow_hidden()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border_focused)
        .bg(theme.dialog_background)
        .shadow_lg()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .id((gpui::ElementId::from(id), "content"))
                .flex()
                .flex_col()
                .gap_1()
                .p(px(4.0))
                .when(scrollable, |this| {
                    this.size_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .scrollbar_width(px(SCROLLBAR_WIDTH_PX))
                        .track_scroll(&content_scroll_handle)
                })
                .children(children),
        )
        .when(scrollable, |this| {
            this.child(
                Scrollbar::vertical(&scrollbar_scroll_handle)
                    .id((gpui::ElementId::from(id), "scrollbar"))
                    .edge_inset(px(4.0)),
            )
        })
}

fn detail_select_option<T>(
    label: String,
    selected: bool,
    id: impl Into<gpui::ElementId>,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let show_tooltip = detail_select_label_needs_tooltip(&label);
    let tooltip_label = label.clone();

    detail_select_option_container(selected, id, cx)
        .when(show_tooltip, |this| {
            this.tooltip(move |_, cx| text_tooltip(tooltip_label.clone(), cx))
        })
        .child(div().flex_1().min_w_0().truncate().child(label))
}

fn detail_select_option_container<T>(
    selected: bool,
    id: impl Into<gpui::ElementId>,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(id)
        .flex()
        .flex_none()
        .h(px(DETAIL_SELECT_OPTION_HEIGHT_PX))
        .items_center()
        .rounded(px(6.0))
        .px_1()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.accent_text
        } else {
            theme.foreground
        })
        .bg(if selected {
            theme.element_selected
        } else {
            theme.dialog_background
        })
        .hover(move |style| {
            style.bg(if selected {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
}

fn detail_select_option_with_subtitle<T>(
    label: String,
    subtitle: Option<&str>,
    selected: bool,
    id: String,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    let subtitle = subtitle.map(str::trim).filter(|title| !title.is_empty());
    let show_tooltip = detail_select_label_needs_tooltip(&label)
        || subtitle.is_some_and(detail_select_label_needs_tooltip);
    let tooltip_label = subtitle.map_or_else(|| label.clone(), |title| format!("{label}\n{title}"));
    let label_id = format!("{id}-label");
    let subtitle_id = format!("{id}-title");

    detail_select_option_container(selected, id.clone(), cx)
        .debug_selector(move || id.clone())
        .h(px(DETAIL_TWO_LINE_OPTION_HEIGHT_PX))
        .px_2()
        .cursor_pointer()
        .when(show_tooltip, |this| {
            this.tooltip(move |_, cx| text_tooltip(tooltip_label.clone(), cx))
        })
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
                .when_some(subtitle, |this, subtitle| {
                    this.child(
                        div()
                            .debug_selector(move || subtitle_id.clone())
                            .truncate()
                            .text_xs()
                            .line_height(px(14.0))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(if selected {
                                theme.accent_text
                            } else {
                                theme.muted_foreground
                            })
                            .child(subtitle.to_string()),
                    )
                }),
        )
}

fn detail_select_label_needs_tooltip(label: &str) -> bool {
    detail_select_label_width_units(label) > DETAIL_SELECT_TOOLTIP_WIDTH_UNITS
}

fn detail_play_button_icon(playback_loading: bool, theme: &theme::TinyTheme) -> impl IntoElement {
    let color = theme.accent_foreground;

    div()
        .flex()
        .size(px(18.0))
        .items_center()
        .justify_center()
        .when(playback_loading, |this| {
            this.child(
                svg()
                    .path("icons/loader.svg")
                    .size(px(18.0))
                    .overflow_hidden()
                    .text_color(color)
                    .with_animation(
                        "series-detail-playback-loader",
                        Animation::new(std::time::Duration::from_millis(1_800)).repeat(),
                        |svg, delta| {
                            svg.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    ),
            )
        })
        .when(!playback_loading, |this| {
            this.child(
                svg()
                    .path("icons/play.svg")
                    .size(px(18.0))
                    .text_color(color),
            )
        })
}

fn detail_play_button_label(playback_seconds: Option<u64>) -> String {
    if let Some(total_seconds) = playback_seconds {
        let hours = total_seconds / 3600;
        let minutes = total_seconds / 60 % 60;
        let seconds = total_seconds % 60;
        if hours > 0 {
            format!("继续 {hours}:{minutes:02}:{seconds:02}")
        } else {
            format!("继续 {minutes}:{seconds:02}")
        }
    } else {
        "播放".to_string()
    }
}

fn detail_select_label_width_units(label: &str) -> usize {
    label
        .chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum::<usize>()
}

fn season_select_width<'a>(labels: impl IntoIterator<Item = &'a str>, window: &Window) -> f32 {
    let mut text_style = window.text_style();
    let font_size = window.rem_size() * 0.875;
    let label_width = labels
        .into_iter()
        .chain(std::iter::once("请选择"))
        .flat_map(str::lines)
        .map(|label| {
            // Measure both the trigger and selected option using the same fonts
            // and size they render with, including fallback glyphs for CJK text.
            [gpui::FontWeight::MEDIUM, gpui::FontWeight::SEMIBOLD]
                .into_iter()
                .map(|weight| {
                    text_style.font_weight = weight;
                    let line = window.text_system().shape_line(
                        label.to_owned().into(),
                        font_size,
                        &[text_style.to_run(label.len())],
                        None,
                    );
                    f32::from(line.width)
                })
                .fold(0.0, f32::max)
        })
        .fold(0.0, f32::max);

    // Account for padding on both sides and the two one-pixel borders.
    (label_width.ceil() + SEASON_SELECT_HORIZONTAL_PADDING_PX * 2.0 + 2.0)
        .clamp(SEASON_SELECT_MIN_WIDTH_PX, SEASON_SELECT_MAX_WIDTH_PX)
}

fn detail_select_menu_is_scrollable(option_count: usize) -> bool {
    option_count > DETAIL_SELECT_MAX_VISIBLE_OPTIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_select_menu_scrolls_only_after_five_options() {
        assert!(!detail_select_menu_is_scrollable(5));
        assert!(detail_select_menu_is_scrollable(6));
    }

    #[test]
    fn detail_select_tooltip_detects_long_ascii_and_cjk_labels() {
        assert!(!detail_select_label_needs_tooltip("English 5.1"));
        assert!(detail_select_label_needs_tooltip(
            "English Dolby Digital Plus 7.1 Atmos Commentary"
        ));
        assert!(detail_select_label_needs_tooltip(
            "简体中文与英文双语特效字幕导演评论版本"
        ));
    }

    #[test]
    fn play_button_label_shows_hours_when_resume_time_reaches_one_hour() {
        assert_eq!(detail_play_button_label(Some(0)), "继续 0:00");
        assert_eq!(detail_play_button_label(Some(905)), "继续 15:05");
        assert_eq!(detail_play_button_label(Some(900)), "继续 15:00");
        assert_eq!(detail_play_button_label(Some(3599)), "继续 59:59");
        assert_eq!(detail_play_button_label(Some(3600)), "继续 1:00:00");
        assert_eq!(detail_play_button_label(Some(3601)), "继续 1:00:01");
        assert_eq!(detail_play_button_label(Some(3905)), "继续 1:05:05");
        assert_eq!(detail_play_button_label(Some(90061)), "继续 25:01:01");
        assert_eq!(detail_play_button_label(None), "播放");
    }

    #[gpui::test]
    fn season_select_width_clamps_short_and_long_labels(cx: &mut gpui::TestAppContext) {
        cx.add_empty_window().update(|window, _| {
            assert_eq!(
                season_select_width(["第一季", "第二季"], window),
                SEASON_SELECT_MIN_WIDTH_PX
            );
            assert_eq!(
                season_select_width(
                    ["This is an exceptionally long season name for testing"],
                    window,
                ),
                SEASON_SELECT_MAX_WIDTH_PX
            );
        });
    }
}
