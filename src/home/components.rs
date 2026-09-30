use std::{path::Path, sync::Arc};

use gpui::{
    App, Bounds, ClickEvent, ContentMask, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, StyledImage, Window, canvas, div, fill, img,
    point, prelude::FluentBuilder, px, size, svg,
};

use super::model::cards::{
    EpisodeCardVm, PersonCardVm, PosterBadgesVm, ResumeCardVm, UserEpisodeCardVm, UserItemCardVm,
};
use crate::{
    images::cover::{CoverImageAsset, CoverImageRequest},
    ui::radius,
};
use crate::{theme, ui::tooltip::text_tooltip};

use super::carousel::{
    DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX, DETAIL_EPISODE_CARD_PADDING_PX,
    DETAIL_EPISODE_CARD_WIDTH_PX, DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
    DETAIL_PERSON_CARD_IMAGE_WIDTH_PX, DETAIL_PERSON_CARD_PADDING_PX, DETAIL_PERSON_CARD_WIDTH_PX,
    HOME_ITEM_CARD_IMAGE_HEIGHT_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
    USER_VIEW_CARD_IMAGE_HEIGHT_PX, USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX,
};

const IMAGE_PROGRESS_BAR_HEIGHT_PX: f32 = 4.0;
const IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX: f32 = 8.0;

pub(super) fn cover_img(path: Arc<Path>, width: f32, height: f32) -> impl IntoElement {
    let source = CoverImageRequest {
        path,
        width: width as u32,
        height: height as u32,
    };

    img(move |window: &mut Window, cx: &mut App| window.use_asset::<CoverImageAsset>(&source, cx))
        .w(px(width))
        .h(px(height))
        .rounded(radius::CARD)
}

pub(super) fn home_section_title<T>(title: &'static str, cx: &Context<T>) -> gpui::Div {
    home_section_title_text(title, cx)
}

pub(super) fn home_section_title_text<T>(
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

pub(super) fn home_section_more_button(id: gpui::ElementId, cx: &App) -> gpui::Stateful<gpui::Div> {
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

pub(super) fn carousel_button(
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

pub(super) fn user_view_card<T>(
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

fn resume_item_card_image<T>(
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

fn cover_image_progress_bar<T>(played_fraction: f32, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let track_color = theme.background.opacity(0.72);
    let progress_color = theme.input_border_focused;

    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let radius = radius::CARD.to_pixels(window.rem_size());
            let height = px(IMAGE_PROGRESS_BAR_HEIGHT_PX).min(bounds.size.height);
            let track_bounds = Bounds::new(
                point(bounds.left(), bounds.bottom() - height),
                size(bounds.size.width, height),
            );
            let progress_bounds = Bounds::new(
                track_bounds.origin,
                size(bounds.size.width * played_fraction.clamp(0.0, 1.0), height),
            );

            // GPUI overflow masks are rectangular. Reveal only the bottom strip
            // of full-cover rounded quads so both colors follow the cover corners.
            window.with_content_mask(
                Some(ContentMask {
                    bounds: track_bounds,
                }),
                |window| {
                    window.paint_quad(fill(bounds, track_color).corner_radii(radius));
                },
            );
            window.with_content_mask(
                Some(ContentMask {
                    bounds: progress_bounds,
                }),
                |window| {
                    window.paint_quad(fill(bounds, progress_color).corner_radii(radius));
                },
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn image_progress_bar<T>(image_width: f32, played_fraction: f32, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);
    let track_width = (image_width - IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX * 2.0).max(0.0);

    div()
        .absolute()
        .left(px(IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX))
        .right(px(IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX))
        .bottom_0()
        .h(px(IMAGE_PROGRESS_BAR_HEIGHT_PX))
        .rounded_full()
        .overflow_hidden()
        .bg(theme.background.opacity(0.72))
        .child(
            div()
                .h_full()
                .w(px(track_width * played_fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(theme.input_border_focused),
        )
}

pub(super) fn resume_item_card<T>(
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

pub(super) fn user_item_card<T>(
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

pub(super) fn user_episode_card<T>(
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

pub(super) fn favorite_episode_card<T>(
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

pub(super) fn episode_card<T>(
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

fn episode_card_image<T>(
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

pub(super) fn person_card<T>(
    view: PersonCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .items_center()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(DETAIL_PERSON_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(person_card_image(image_path, cx))
        .child(
            div()
                .w(px(DETAIL_PERSON_CARD_WIDTH_PX))
                .flex()
                .flex_col()
                .gap_1()
                .text_center()
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(view.name),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(view.role),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(view.kind),
                ),
        )
}

fn person_card_image<T>(image_path: Option<Arc<Path>>, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(DETAIL_PERSON_CARD_IMAGE_WIDTH_PX))
        .h(px(DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded(radius::CARD)
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                DETAIL_PERSON_CARD_IMAGE_WIDTH_PX,
                DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex().items_center().justify_center().child(
                svg()
                    .path("icons/circle-user-round.svg")
                    .size(px(32.0))
                    .text_color(theme.muted_foreground),
            )
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn latte_cards_change_fill_over_the_image_and_label(cx: &mut gpui::TestAppContext) {
        use gpui::{Modifiers, Render, point};

        struct CardHover(bool);

        impl Render for CardHover {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let item = serde_json::json!({"Id": "1", "Name": "Movie", "Type": "Movie"});
                let card = if self.0 {
                    episode_card(
                        EpisodeCardVm::new(&serde_json::from_value(item).unwrap(), None, true),
                        None,
                        cx,
                    )
                } else {
                    user_item_card(
                        UserItemCardVm::new(&serde_json::from_value(item).unwrap(), None, true),
                        None,
                        cx,
                    )
                };
                div()
                    .size_full()
                    .bg(theme::get(cx).background)
                    .p_4()
                    .child(card.id("card").debug_selector(|| "card".into()))
            }
        }

        cx.update(|cx| theme::set(theme::ColorTheme::Latte, cx));
        let (view, cx) = cx.add_window_view(|_, _| CardHover(false));
        for selected in [false, true] {
            view.update(cx, |view, cx| {
                view.0 = selected;
                cx.notify();
            });
            cx.run_until_parked();
            cx.simulate_mouse_move(point(px(2.0), px(2.0)), None, Modifiers::default());
            cx.run_until_parked();
            let bounds = cx.debug_bounds("card").unwrap();
            for position in [
                bounds.origin + point(px(20.0), px(20.0)),
                point(bounds.center().x, bounds.bottom() - px(12.0)),
            ] {
                cx.simulate_mouse_move(position, None, Modifiers::default());
                cx.run_until_parked();
                assert!(
                    cx.update(|window, cx| {
                        let theme = theme::get(cx);
                        let expected = if selected {
                            theme.element_selected_hover
                        } else {
                            theme.secondary_hover
                        };
                        window
                            .painted_quads()
                            .iter()
                            .any(|quad| quad.background == expected.into())
                    }),
                    "selected={selected} position={position:?} bounds={bounds:?}"
                );
            }
        }
    }

    #[gpui::test]
    fn resume_progress_matches_cover_corners_and_stays_inside_visible_bounds(
        cx: &mut gpui::TestAppContext,
    ) {
        assert_cover_progress_bounds(cx, false);
    }

    #[gpui::test]
    fn detail_episode_progress_matches_cover_corners_and_stays_inside_visible_bounds(
        cx: &mut gpui::TestAppContext,
    ) {
        assert_cover_progress_bounds(cx, true);
    }

    fn assert_cover_progress_bounds(cx: &mut gpui::TestAppContext, detail_episode: bool) {
        use gpui::{Render, ScaledPixels};

        struct ProgressCard {
            detail_episode: bool,
            fraction: Option<f32>,
            rem_size: f32,
            visible_width: f32,
        }

        impl Render for ProgressCard {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                window.set_rem_size(px(self.rem_size));
                div().size_full().p(px(16.0)).child(
                    div().w(px(self.visible_width)).overflow_hidden().child(
                        if self.detail_episode {
                            episode_card_image(None, self.fraction, cx).into_any_element()
                        } else {
                            resume_item_card_image(None, self.fraction, false, cx)
                                .into_any_element()
                        },
                    ),
                )
            }
        }

        cx.update(theme::init);
        let cover_width = if detail_episode {
            DETAIL_EPISODE_CARD_WIDTH_PX
        } else {
            USER_VIEW_CARD_WIDTH_PX
        };
        let (card, cx) = cx.add_window_view(|_, _| ProgressCard {
            detail_episode,
            fraction: None,
            rem_size: 16.0,
            visible_width: cover_width,
        });

        for rem_size in [16.0, 20.0] {
            for visible_width in [cover_width, 180.0] {
                for fraction in [None, Some(-0.1), Some(0.0), Some(0.5), Some(1.0), Some(1.1)] {
                    card.update(cx, |card, cx| {
                        card.fraction = fraction;
                        card.rem_size = rem_size;
                        card.visible_width = visible_width;
                        cx.notify();
                    });
                    cx.run_until_parked();
                    cx.update(|window, cx| {
                        let theme = theme::get(cx);
                        let quads = window.painted_quads();
                        let cover = quads
                            .iter()
                            .find(|quad| quad.background == theme.input_background.into())
                            .expect("cover background is painted");
                        let track = quads
                            .iter()
                            .find(|quad| quad.background == theme.background.opacity(0.72).into());
                        let progress = quads
                            .iter()
                            .find(|quad| quad.background == theme.input_border_focused.into());

                        assert_eq!(track.is_some(), fraction.is_some());
                        assert_eq!(
                            progress.is_some(),
                            fraction.is_some_and(|value| value > 0.0)
                        );
                        for (quad, width) in track
                            .map(|quad| (quad, visible_width))
                            .into_iter()
                            .chain(progress.map(|quad| {
                                let width = cover_width * fraction.unwrap().clamp(0.0, 1.0);
                                (quad, width.min(visible_width))
                            }))
                        {
                            assert_eq!(quad.bounds, cover.bounds);
                            assert_eq!(quad.corner_radii, cover.corner_radii);
                            let mask = quad.content_mask.bounds;
                            assert_eq!(mask.left(), cover.bounds.left());
                            assert_eq!(mask.bottom(), cover.bounds.bottom());
                            assert_eq!(
                                mask.size.width,
                                ScaledPixels(width * window.scale_factor())
                            );
                            assert_eq!(mask.size.height, ScaledPixels(4.0 * window.scale_factor()));
                        }
                    });
                }
            }
        }
    }
}
