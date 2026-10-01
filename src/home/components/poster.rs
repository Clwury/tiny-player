//! Home poster presentation components.
use super::*;

pub(in crate::home) fn user_view_card<T>(
    name: String,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(USER_VIEW_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(user_view_card_image(image_path, cx))
        .child(
            div()
                .w(px(USER_VIEW_CARD_WIDTH_PX))
                .truncate()
                .text_center()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(name),
        )
}

fn user_view_card_image<T>(image_path: Option<Arc<Path>>, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .when_some(image_path, |this, path| {
            this.child(
                img(path)
                    .w(px(USER_VIEW_CARD_WIDTH_PX))
                    .rounded(radius::CARD),
            )
        })
        .when(!has_image, |this| {
            this.flex()
                .w(px(USER_VIEW_CARD_WIDTH_PX))
                .h(px(USER_VIEW_CARD_IMAGE_HEIGHT_PX))
                .rounded(radius::CARD)
                .overflow_hidden()
                .bg(theme.input_background)
                .items_center()
                .justify_center()
                .child(
                    svg()
                        .path("icons/clapperboard.svg")
                        .size(px(32.0))
                        .text_color(theme.muted_foreground),
                )
        })
}

pub(super) fn resume_item_card_image<T>(
    image_path: Option<Arc<Path>>,
    played_fraction: Option<f32>,
    is_favorite: bool,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(USER_VIEW_CARD_WIDTH_PX))
        .h(px(USER_VIEW_CARD_IMAGE_HEIGHT_PX))
        .rounded(radius::CARD)
        .overflow_hidden()
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                USER_VIEW_CARD_WIDTH_PX,
                USER_VIEW_CARD_IMAGE_HEIGHT_PX,
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
        .when(is_favorite, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .bottom(px(6.0))
                    .flex()
                    .size(px(24.0))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(theme.dialog_background.opacity(0.86))
                    .child(
                        svg()
                            .path("icons/heart-filled.svg")
                            .size(px(14.0))
                            .text_color(theme.foreground),
                    ),
            )
        })
}

pub(in crate::home) fn resume_item_card<T>(
    view: ResumeCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(USER_VIEW_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(resume_item_card_image(
            image_path,
            view.played_fraction,
            view.favorite,
            cx,
        ))
        .child(
            div()
                .w(px(USER_VIEW_CARD_WIDTH_PX))
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
                .when_some(view.subtitle, |this, subtitle| {
                    this.child(
                        div()
                            .truncate()
                            .text_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(subtitle),
                    )
                }),
        )
}

pub(in crate::home) fn user_item_card<T>(
    view: UserItemCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(HOME_ITEM_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(user_item_card_image(view.badges, image_path, cx))
        .child(
            div()
                .w(px(HOME_ITEM_CARD_WIDTH_PX))
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
                .when_some(view.year, |this, year| {
                    this.child(
                        div()
                            .truncate()
                            .text_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(year),
                    )
                }),
        )
}

fn user_item_card_image<T>(
    badges: PosterBadgesVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();
    let rating = badges.rating;
    let unplayed_count = badges.unplayed_count;
    let has_badges = rating.is_some() || unplayed_count.is_some();
    let is_favorite = badges.favorite;

    div()
        .relative()
        .w(px(HOME_ITEM_CARD_WIDTH_PX))
        .h(px(HOME_ITEM_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded(radius::CARD)
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                HOME_ITEM_CARD_WIDTH_PX,
                HOME_ITEM_CARD_IMAGE_HEIGHT_PX,
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
        .when(has_badges, |this| {
            this.child(
                div()
                    .absolute()
                    .top(px(6.0))
                    .right(px(6.0))
                    .flex()
                    .flex_row()
                    .gap_1()
                    .when_some(rating, |this, rating| {
                        this.child(user_item_badge(rating, cx))
                    })
                    .when_some(unplayed_count, |this, count| {
                        this.child(user_item_badge(count.to_string(), cx))
                    }),
            )
        })
        .when(is_favorite, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .bottom(px(6.0))
                    .flex()
                    .size(px(24.0))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(theme.dialog_background.opacity(0.86))
                    .child(
                        svg()
                            .path("icons/heart-filled.svg")
                            .size(px(14.0))
                            .text_color(theme.foreground),
                    ),
            )
        })
}

fn user_item_badge<T>(text: String, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(20.0))
        .items_center()
        .rounded_full()
        .px_2()
        .bg(theme.dialog_background.opacity(0.86))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .child(text)
}
