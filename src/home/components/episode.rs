//! Home episode presentation components.
use super::*;

pub(in crate::home) fn user_episode_card<T>(
    view: UserEpisodeCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let image = compact_episode_card_image(image_path, view.played_fraction, cx);
    user_episode_card_with_image(
        view,
        image,
        HOME_ITEM_CARD_WIDTH_PX,
        HOME_ITEM_CARD_PADDING_PX,
        cx,
    )
}

pub(in crate::home) fn favorite_episode_card<T>(
    view: UserEpisodeCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let image = episode_card_image(image_path, view.played_fraction, cx);
    user_episode_card_with_image(
        view,
        image,
        DETAIL_EPISODE_CARD_WIDTH_PX,
        DETAIL_EPISODE_CARD_PADDING_PX,
        cx,
    )
}

fn compact_episode_card_image<T>(
    image_path: Option<Arc<Path>>,
    played_fraction: Option<f32>,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();
    div()
        .relative()
        .w(px(HOME_ITEM_CARD_WIDTH_PX))
        .h(px(HOME_ITEM_CARD_WIDTH_PX * 9.0 / 16.0))
        .rounded(radius::CARD)
        .overflow_hidden()
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(
                img(path)
                    .w_full()
                    .h_full()
                    .rounded(radius::CARD)
                    .object_fit(gpui::ObjectFit::Cover),
            )
        })
        .when(!has_image, |this| {
            this.flex().items_center().justify_center().child(
                svg()
                    .path("icons/clapperboard.svg")
                    .size(px(32.0))
                    .text_color(theme.muted_foreground),
            )
        })
        .when_some(played_fraction, |this, fraction| {
            this.child(image_progress_bar(HOME_ITEM_CARD_WIDTH_PX, fraction, cx))
        })
}

fn user_episode_card_with_image<T>(
    view: UserEpisodeCardVm,
    image: impl IntoElement,
    width: f32,
    padding: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(padding))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            div()
                .relative()
                .child(image)
                .when(view.played, |this| this.child(episode_played_badge(cx))),
        )
        .child(
            div()
                .w(px(width))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_center()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(view.title),
                )
                .child(
                    div()
                        .truncate()
                        .text_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(view.subtitle),
                ),
        )
}

pub(in crate::home) fn episode_card<T>(
    view: EpisodeCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let label = view.label;
    let selected = view.selected;

    div()
        .relative()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(DETAIL_EPISODE_CARD_PADDING_PX))
        .when(selected, |this| this.bg(theme.element_selected))
        .hover(move |style| {
            style.bg(if selected {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .child(
            div()
                .relative()
                .child(episode_card_image(image_path, view.played_fraction, cx))
                .when(view.played, |this| this.child(episode_played_badge(cx))),
        )
        .child(
            div()
                .w(px(DETAIL_EPISODE_CARD_WIDTH_PX))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .id("episode-card-label")
                        .truncate()
                        .text_sm()
                        .font_weight(if selected {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        })
                        .text_color(if selected {
                            theme.accent_text
                        } else {
                            theme.foreground
                        })
                        .child(label.clone())
                        .tooltip(move |_, cx| text_tooltip(label.clone(), cx)),
                )
                .when_some(view.overview, |this, overview| {
                    this.child(
                        div()
                            .id("episode-card-overview")
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .text_ellipsis()
                            .line_clamp(3)
                            .child(overview.clone())
                            .tooltip(move |_, cx| text_tooltip(overview.clone(), cx)),
                    )
                }),
        )
}

fn episode_played_badge(cx: &gpui::App) -> impl IntoElement {
    let theme = theme::media_overlay(cx);
    div()
        .id("episode-played-badge")
        .debug_selector(|| "episode-watched".into())
        .absolute()
        .top(px(6.0))
        .right(px(6.0))
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .aria_label("已观看")
        .tooltip(|_, cx| text_tooltip("已观看", cx))
        .child(
            svg()
                .path("icons/circle-check-filled.svg")
                .size(px(18.0))
                .text_color(theme.foreground),
        )
}

pub(super) fn episode_card_image<T>(
    image_path: Option<Arc<Path>>,
    played_fraction: Option<f32>,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(DETAIL_EPISODE_CARD_WIDTH_PX))
        .h(px(DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded(radius::CARD)
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                DETAIL_EPISODE_CARD_WIDTH_PX,
                DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex().items_center().justify_center().child(
                svg()
                    .path("icons/clapperboard.svg")
                    .size(px(32.0))
                    .text_color(theme.muted_foreground),
            )
        })
        .when_some(played_fraction, |this, fraction| {
            this.child(cover_image_progress_bar(fraction, cx))
        })
}
