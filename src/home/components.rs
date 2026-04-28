use std::path::PathBuf;

use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder, px, svg,
};

use crate::{emby::ResumeItem, theme};

use super::{
    HomePage,
    carousel::{
        USER_VIEW_CARD_IMAGE_HEIGHT_PX, USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX,
    },
};

pub(super) fn section_placeholder(
    title: &'static str,
    message: &'static str,
    cx: &Context<HomePage>,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(home_section_title(title, cx))
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(message),
        )
}

pub(super) fn home_section_title(title: &'static str, cx: &Context<HomePage>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .text_lg()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(title)
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

pub(super) fn user_view_card(
    name: String,
    image_path: Option<PathBuf>,
    cx: &Context<HomePage>,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
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

fn user_view_card_image(image_path: Option<PathBuf>, cx: &Context<HomePage>) -> impl IntoElement {
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
        })
}

pub(super) fn resume_item_card(
    item: &ResumeItem,
    image_path: Option<PathBuf>,
    cx: &Context<HomePage>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let (title, subtitle) = resume_item_card_text(item);

    div()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_lg()
        .p(px(USER_VIEW_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(user_view_card_image(image_path, cx))
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
            series_name: None,
            parent_index_number: None,
            index_number: None,
            production_year: None,
            image_tags: None,
            backdrop_image_tags: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
        }
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
