//! Episode row presentation. The page binds the selected-row action and image
//! path; the component has no session or backend access.
use super::EPISODE_ROW_HEIGHT_PX;
use crate::{
    player::model::episode_card::EpisodeCardVm,
    theme,
    ui::{radius, tooltip::text_tooltip},
};
use gpui::{
    App, FontWeight, InteractiveElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Window, div, img, prelude::FluentBuilder, px,
    svg,
};
use std::{path::Path, sync::Arc};

pub(super) fn episode_card(
    view: EpisodeCardVm<'_>,
    index: usize,
    image_path: Option<Arc<Path>>,
    cx: &App,
    on_select: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let theme = theme::media_overlay(cx);
    let selected = view.selected;
    let label: SharedString = view.label.into();
    let metadata = view.metadata;
    let overview = view.overview;

    div().h(px(EPISODE_ROW_HEIGHT_PX)).pb_2().child(
        div()
            .id((
                gpui::ElementId::from("playback-episode"),
                view.item_id.to_string(),
            ))
            .debug_selector(move || format!("playback-episode-{index}"))
            .size_full()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .rounded(radius::CARD)
            .border_1()
            .border_color(if selected {
                theme.input_border_focused
            } else {
                theme.input_border.opacity(0.32)
            })
            .when(selected, |this| this.bg(theme.element_selected))
            .cursor_pointer()
            .hover(move |style| {
                style.bg(if selected {
                    theme.element_selected_hover
                } else {
                    theme.secondary_hover
                })
            })
            .on_mouse_down(MouseButton::Left, on_select)
            .child(
                div()
                    .debug_selector(move || format!("playback-episode-image-{index}"))
                    .flex_none()
                    .relative()
                    .w(px(144.0))
                    .h(px(81.0))
                    .rounded(radius::CARD)
                    .overflow_hidden()
                    .bg(theme.input_background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(image_path.is_none(), |this| {
                        this.child(
                            svg()
                                .path("icons/clapperboard.svg")
                                .size(px(24.0))
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .when_some(image_path, |this, path| {
                        this.child(
                            img(path)
                                .size_full()
                                .rounded(radius::CARD)
                                .object_fit(gpui::ObjectFit::Cover),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .id("episode-label")
                            .debug_selector(move || format!("playback-episode-label-{index}"))
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(label.clone())
                            .tooltip(move |_, cx| text_tooltip(label.clone(), cx)),
                    )
                    .when_some(metadata, |this, metadata| {
                        this.child(
                            div()
                                .debug_selector(move || {
                                    format!("playback-episode-metadata-{index}")
                                })
                                .truncate()
                                .text_xs()
                                .line_height(px(16.0))
                                .text_color(theme.muted_foreground)
                                .child(metadata),
                        )
                    })
                    .when_some(overview, |this, overview| {
                        this.child(
                            div()
                                .id("episode-overview")
                                .debug_selector(move || {
                                    format!("playback-episode-overview-{index}")
                                })
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .text_ellipsis()
                                .line_clamp(2)
                                .child(overview.clone())
                                .tooltip(move |_, cx| text_tooltip(overview.clone(), cx)),
                        )
                    }),
            ),
    )
}
