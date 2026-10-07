//! Application information shared by both settings presentations.
use crate::{
    app_metadata::{APP_ICON_ASSET_PATH, APP_NAME},
    theme,
};
use gpui::{
    App, InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, div, img, px,
    relative,
};

pub(super) fn render_about(cx: &App) -> gpui::Div {
    let theme = theme::get(cx);
    div()
        .flex()
        .flex_col()
        .size_full()
        .min_w_0()
        .items_center()
        .justify_center()
        .py_8()
        .child(
            div()
                .id("settings-about-content")
                .debug_selector(|| "settings-about-content".into())
                .flex()
                .flex_col()
                .flex_none()
                .items_center()
                .w_full()
                .max_w(px(420.0))
                .text_center()
                .child(
                    div()
                        .id("settings-about-icon")
                        .debug_selector(|| "settings-about-icon".into())
                        .size(px(72.0))
                        .mb_4()
                        .child(img(APP_ICON_ASSET_PATH).size_full()),
                )
                .child(
                    div()
                        .id("settings-about-name")
                        .debug_selector(|| "settings-about-name".into())
                        .text_2xl()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(APP_NAME),
                )
                .child(
                    div()
                        .id("settings-about-description")
                        .debug_selector(|| "settings-about-description".into())
                        .mt_2()
                        .text_sm()
                        .line_height(relative(1.5))
                        .text_color(theme.muted_foreground)
                        .child("原生 Emby 桌面客户端"),
                )
                .child(
                    div()
                        .id("settings-about-version")
                        .debug_selector(|| "settings-about-version".into())
                        .mt_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("版本 {}", env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    div()
                        .id("settings-about-github")
                        .debug_selector(|| "settings-about-github".into())
                        .role(gpui::Role::Link)
                        .aria_label("GitHub 项目链接")
                        .aria_description(env!("CARGO_PKG_REPOSITORY"))
                        .mt_6()
                        .text_sm()
                        .cursor_pointer()
                        .text_color(theme.accent_text)
                        .hover(|style| style.text_color(theme.foreground))
                        .on_click(|_, _, cx| cx.open_url(env!("CARGO_PKG_REPOSITORY")))
                        .child(env!("CARGO_PKG_REPOSITORY")),
                ),
        )
}
