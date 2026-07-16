use std::{fs, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, Asset, ClickEvent, Context, ImageCacheError, InteractiveElement, IntoElement, MouseButton,
    ParentElement, RenderImage, StatefulInteractiveElement, Styled, StyledImage, Window, div, img,
    prelude::FluentBuilder, px, svg,
};
use image::{Frame, imageops::FilterType};

use crate::{
    emby::{MediaItem, MediaPerson, ResumeItem, UserItem},
    theme,
};

use super::carousel::{
    DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX, DETAIL_EPISODE_CARD_PADDING_PX,
    DETAIL_EPISODE_CARD_WIDTH_PX, DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
    DETAIL_PERSON_CARD_IMAGE_WIDTH_PX, DETAIL_PERSON_CARD_PADDING_PX, DETAIL_PERSON_CARD_WIDTH_PX,
    HOME_ITEM_CARD_IMAGE_HEIGHT_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
    USER_VIEW_CARD_IMAGE_HEIGHT_PX, USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX,
};

const IMAGE_PROGRESS_BAR_HEIGHT_PX: f32 = 4.0;
const IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX: f32 = 8.0;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CoverImageSource {
    path: PathBuf,
    width: u32,
    height: u32,
}

enum CoverImageAsset {}

impl Asset for CoverImageAsset {
    type Source = CoverImageSource;
    type Output = std::result::Result<Arc<RenderImage>, ImageCacheError>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        async move { load_cover_image(source).map_err(|error| ImageCacheError::Other(Arc::new(error))) }
    }
}

pub(super) fn cover_img(path: PathBuf, width: f32, height: f32) -> impl IntoElement {
    let source = CoverImageSource {
        path,
        width: width as u32,
        height: height as u32,
    };

    img(move |window: &mut Window, cx: &mut App| window.use_asset::<CoverImageAsset>(&source, cx))
        .w(px(width))
        .h(px(height))
        .rounded_lg()
}

fn load_cover_image(source: CoverImageSource) -> Result<Arc<RenderImage>> {
    let bytes = fs::read(&source.path)
        .with_context(|| format!("读取图片缓存失败：{}", source.path.display()))?;
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("解析图片缓存失败：{}", source.path.display()))?
        .to_rgba8();
    let (source_width, source_height) = image.dimensions();
    let (x, y, crop_width, crop_height) =
        cover_crop_bounds(source_width, source_height, source.width, source.height)?;
    let cropped = image::imageops::crop_imm(&image, x, y, crop_width, crop_height).to_image();
    let mut resized =
        image::imageops::resize(&cropped, source.width, source.height, FilterType::Lanczos3);

    for pixel in resized.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    Ok(Arc::new(RenderImage::new([Frame::new(resized)])))
}

fn cover_crop_bounds(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<(u32, u32, u32, u32)> {
    if source_width == 0 || source_height == 0 || target_width == 0 || target_height == 0 {
        return Err(anyhow!("图片裁剪尺寸无效"));
    }

    let source_ratio = source_width as f64 / source_height as f64;
    let target_ratio = target_width as f64 / target_height as f64;

    if source_ratio > target_ratio {
        let crop_width = ((source_height as f64 * target_ratio).round() as u32)
            .max(1)
            .min(source_width);
        let x = (source_width - crop_width) / 2;
        Ok((x, 0, crop_width, source_height))
    } else {
        let crop_height = ((source_width as f64 / target_ratio).round() as u32)
            .max(1)
            .min(source_height);
        let y = (source_height - crop_height) / 2;
        Ok((0, y, source_width, crop_height))
    }
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
    let border = theme.input_border;
    let hover = theme.secondary_hover;

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
        .on_hover(on_hover)
        .child(
            div()
                .id((gpui::ElementId::from(id), "button"))
                .flex()
                .size(px(34.0))
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(border)
                .bg(background.opacity(0.88))
                .shadow_lg()
                .opacity(if visible { 1.0 } else { 0.0 })
                .hover(move |style| style.bg(hover))
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
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded_lg()
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

fn user_view_card_image<T>(image_path: Option<PathBuf>, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .when_some(image_path, |this, path| {
            this.child(img(path).w(px(USER_VIEW_CARD_WIDTH_PX)).rounded_lg())
        })
        .when(!has_image, |this| {
            this.flex()
                .w(px(USER_VIEW_CARD_WIDTH_PX))
                .h(px(USER_VIEW_CARD_IMAGE_HEIGHT_PX))
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("暂无图片")
        })
}

fn resume_item_card_image<T>(
    image_path: Option<PathBuf>,
    played_fraction: Option<f32>,
    is_favorite: bool,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(USER_VIEW_CARD_WIDTH_PX))
        .rounded_lg()
        .overflow_hidden()
        .when_some(image_path, |this, path| {
            this.child(img(path).w(px(USER_VIEW_CARD_WIDTH_PX)).rounded_lg())
        })
        .when(!has_image, |this| {
            this.flex()
                .h(px(USER_VIEW_CARD_IMAGE_HEIGHT_PX))
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("暂无图片")
        })
        .when_some(played_fraction, |this, fraction| {
            this.child(image_progress_bar(USER_VIEW_CARD_WIDTH_PX, fraction, cx))
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
                            .path("icons/heart.svg")
                            .size(px(14.0))
                            .text_color(theme.error),
                    ),
            )
        })
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
    item: &ResumeItem,
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let (title, subtitle) = resume_item_card_text(item);
    let played_fraction = item
        .played_percentage()
        .map(|percentage| (percentage / 100.0) as f32);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .p(px(USER_VIEW_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(resume_item_card_image(
            image_path,
            played_fraction,
            item.is_favorite(),
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
                        .child(title),
                )
                .when_some(subtitle, |this, subtitle| {
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
    item: &UserItem,
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .p(px(HOME_ITEM_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(user_item_card_image(item, image_path, cx))
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
                        .child(item.name.clone()),
                )
                .when_some(item.production_year, |this, year| {
                    this.child(
                        div()
                            .truncate()
                            .text_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(year.to_string()),
                    )
                }),
        )
}

pub(super) fn user_episode_card<T>(
    item: &UserItem,
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();
    let played_fraction = item
        .user_data
        .as_ref()
        .and_then(|data| data.played_percentage)
        .filter(|percentage| percentage.is_finite())
        .map(|percentage| (percentage.clamp(0.0, 100.0) / 100.0) as f32);
    let episode_number = match (item.parent_index_number, item.index_number) {
        (Some(season), Some(episode)) => Some(format!("S{season:02}E{episode:02}")),
        (None, Some(episode)) => Some(format!("E{episode:02}")),
        _ => None,
    };
    let title = item
        .series_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&item.name)
        .to_string();
    let subtitle = match episode_number {
        Some(number) if title != item.name => format!("{number} · {}", item.name),
        Some(number) => number,
        None if title != item.name => item.name.clone(),
        None => "单集".to_string(),
    };

    div()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .p(px(HOME_ITEM_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            div()
                .relative()
                .w(px(HOME_ITEM_CARD_WIDTH_PX))
                .h(px(HOME_ITEM_CARD_WIDTH_PX * 9.0 / 16.0))
                .rounded_lg()
                .overflow_hidden()
                .bg(theme.input_background)
                .when_some(image_path, |this, path| {
                    this.child(
                        img(path)
                            .w_full()
                            .h_full()
                            .object_fit(gpui::ObjectFit::Cover),
                    )
                })
                .when(!has_image, |this| {
                    this.flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("暂无图片")
                })
                .when_some(played_fraction, |this, fraction| {
                    this.child(image_progress_bar(HOME_ITEM_CARD_WIDTH_PX, fraction, cx))
                }),
        )
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
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(subtitle),
                ),
        )
}

pub(super) fn episode_card<T>(
    episode: &MediaItem,
    image_path: Option<PathBuf>,
    selected: bool,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let played_fraction = if selected {
        episode
            .played_percentage()
            .map(|percentage| (percentage / 100.0) as f32)
    } else {
        None
    };

    div()
        .relative()
        .flex()
        .flex_none()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .p(px(DETAIL_EPISODE_CARD_PADDING_PX))
        .when(selected, |this| {
            this.bg(theme.secondary_hover.opacity(0.45))
        })
        .hover(move |style| style.bg(theme.secondary_hover))
        .when(selected, |this| {
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.input_border_focused),
            )
        })
        .child(episode_card_image(image_path, played_fraction, cx))
        .child(
            div()
                .w(px(DETAIL_EPISODE_CARD_WIDTH_PX))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(episode.episode_card_label()),
                )
                .when_some(
                    non_empty_string(episode.overview.as_deref()),
                    |this, overview| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .text_ellipsis()
                                .line_clamp(3)
                                .child(overview),
                        )
                    },
                ),
        )
}

fn episode_card_image<T>(
    image_path: Option<PathBuf>,
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
        .rounded_lg()
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                DETAIL_EPISODE_CARD_WIDTH_PX,
                DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("暂无图片")
        })
        .when_some(played_fraction, |this, fraction| {
            this.child(image_progress_bar(
                DETAIL_EPISODE_CARD_WIDTH_PX,
                fraction,
                cx,
            ))
        })
}

pub(super) fn person_card<T>(
    person: &MediaPerson,
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .items_center()
        .gap_2()
        .rounded_lg()
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
                        .child(person.display_name()),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(person.role_label()),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(person.type_label()),
                ),
        )
}

fn person_card_image<T>(image_path: Option<PathBuf>, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(DETAIL_PERSON_CARD_IMAGE_WIDTH_PX))
        .h(px(DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded_lg()
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                DETAIL_PERSON_CARD_IMAGE_WIDTH_PX,
                DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("暂无图片")
        })
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn user_item_card_image<T>(
    item: &UserItem,
    image_path: Option<PathBuf>,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();
    let rating = item.community_rating.map(format_community_rating);
    let unplayed_count = item.unplayed_count();
    let has_badges = rating.is_some() || unplayed_count.is_some();
    let is_favorite = item.is_favorite();

    div()
        .relative()
        .w(px(HOME_ITEM_CARD_WIDTH_PX))
        .h(px(HOME_ITEM_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded_lg()
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                HOME_ITEM_CARD_WIDTH_PX,
                HOME_ITEM_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("暂无图片")
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
                            .path("icons/heart.svg")
                            .size(px(14.0))
                            .text_color(theme.error),
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

pub(super) fn format_community_rating(rating: f32) -> String {
    let rating = (rating * 10.0).round() / 10.0;
    if rating.fract().abs() < f32::EPSILON {
        format!("{rating:.0}")
    } else {
        format!("{rating:.1}")
    }
}

fn resume_item_card_text(item: &ResumeItem) -> (String, Option<String>) {
    match item.item_type.as_deref() {
        Some("Episode") => {
            let title = item
                .series_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&item.name)
                .to_string();
            let subtitle = match (item.parent_index_number, item.index_number) {
                (Some(season), Some(episode)) => format!("S{season}E{episode}: {}", item.name),
                _ => item.name.clone(),
            };

            (title, Some(subtitle))
        }
        Some("Movie") => (
            item.name.clone(),
            item.production_year.map(|year| year.to_string()),
        ),
        _ => (item.name.clone(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resume_item(item_type: &str, name: &str) -> ResumeItem {
        ResumeItem {
            id: "item-1".to_string(),
            name: name.to_string(),
            item_type: Some(item_type.to_string()),
            parent_id: None,
            series_name: None,
            series_id: None,
            parent_index_number: None,
            index_number: None,
            production_year: None,
            image_tags: None,
            backdrop_image_tags: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
            user_data: None,
        }
    }

    #[test]
    fn formats_community_rating_badge_text() {
        assert_eq!(format_community_rating(8.0), "8");
        assert_eq!(format_community_rating(8.74), "8.7");
        assert_eq!(format_community_rating(8.75), "8.8");
    }

    #[test]
    fn crops_wide_cover_images_horizontally() {
        assert_eq!(
            cover_crop_bounds(400, 213, 160, 213).unwrap(),
            (120, 0, 160, 213)
        );
    }

    #[test]
    fn crops_tall_cover_images_vertically() {
        assert_eq!(
            cover_crop_bounds(160, 400, 160, 213).unwrap(),
            (0, 93, 160, 213)
        );
    }

    #[test]
    fn formats_episode_resume_card_text() {
        let mut item = resume_item("Episode", "第三集");
        item.series_name = Some("示例剧集".to_string());
        item.parent_index_number = Some(1);
        item.index_number = Some(3);

        let (title, subtitle) = resume_item_card_text(&item);

        assert_eq!(title, "示例剧集");
        assert_eq!(subtitle.as_deref(), Some("S1E3: 第三集"));
    }

    #[test]
    fn falls_back_for_incomplete_episode_resume_card_text() {
        let item = resume_item("Episode", "特别篇");

        let (title, subtitle) = resume_item_card_text(&item);

        assert_eq!(title, "特别篇");
        assert_eq!(subtitle.as_deref(), Some("特别篇"));
    }

    #[test]
    fn formats_movie_resume_card_text() {
        let mut item = resume_item("Movie", "示例电影");
        item.production_year = Some(2024);

        let (title, subtitle) = resume_item_card_text(&item);

        assert_eq!(title, "示例电影");
        assert_eq!(subtitle.as_deref(), Some("2024"));
    }

    #[test]
    fn omits_missing_movie_resume_card_subtitle() {
        let item = resume_item("Movie", "无年份电影");

        let (title, subtitle) = resume_item_card_text(&item);

        assert_eq!(title, "无年份电影");
        assert!(subtitle.is_none());
    }
}
