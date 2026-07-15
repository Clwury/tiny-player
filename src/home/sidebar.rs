use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, svg,
};

use crate::{server::CachedServer, theme};

use super::{HomePage, carousel::HOME_SIDEBAR_WIDTH_PX, navigation::HomeRoot};

impl HomePage {
    pub(super) fn render_sidebar(
        &self,
        cx: &Context<Self>,
        rounded_window: bool,
        on_back: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        on_home: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_favorites: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_search: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let username = self.current_server.username.clone();
        let active_root = self.home_content.read(cx).root();

        div()
            .flex()
            .h_full()
            .w(px(HOME_SIDEBAR_WIDTH_PX))
            .flex_col()
            .border_r_1()
            .border_color(theme.title_bar_border)
            .bg(theme.title_bar)
            .when(rounded_window, |this| {
                this.rounded_bl(theme.radius_lg).overflow_hidden()
            })
            .p_3()
            .child(self.render_title_row(cx, on_back))
            .child(div().h(px(12.0)).flex_none())
            .gap_1()
            .child(sidebar_nav_item(
                "home-section",
                "icons/home.svg",
                HomeRoot::Home.title(),
                active_root == HomeRoot::Home,
                cx,
                on_home,
            ))
            .child(sidebar_nav_item(
                "favorites-section",
                "icons/heart.svg",
                HomeRoot::Favorites.title(),
                active_root == HomeRoot::Favorites,
                cx,
                on_favorites,
            ))
            .child(sidebar_nav_item(
                "search-section",
                "icons/search.svg",
                HomeRoot::Search.title(),
                active_root == HomeRoot::Search,
                cx,
                on_search,
            ))
            .child(div().my_3().h(px(1.0)).bg(theme.title_bar_border))
            .child(div().flex().min_h_0().flex_1().flex_col().gap_1().children(
                self.servers.iter().map(|server| {
                    server_list_item(
                        server_title(server),
                        server.id == self.current_server.id,
                        cx,
                    )
                }),
            ))
            .child(user_row(username, cx))
    }

    fn render_title_row(
        &self,
        cx: &Context<HomePage>,
        on_back: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .relative()
            .flex()
            .h(px(36.0))
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("home-back")
                    .absolute()
                    .left_0()
                    .flex()
                    .size(px(32.0))
                    .items_center()
                    .justify_center()
                    .rounded_md()
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
                    .on_click(on_back),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child("Tiny"),
            )
    }
}

fn sidebar_nav_item(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    active: bool,
    cx: &Context<HomePage>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let color = if active {
        theme.foreground
    } else {
        theme.muted_foreground
    };

    div()
        .id(id)
        .flex()
        .h(px(34.0))
        .items_center()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(color)
        .when(active, |this| this.bg(theme.secondary_hover))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(svg().path(icon).size(px(16.0)).text_color(color))
        .child(label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx);
        })
}

fn server_list_item(title: String, active: bool, cx: &Context<HomePage>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(30.0))
        .items_center()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(active, |this| this.bg(theme.secondary_hover))
        .child(title)
}

fn user_row(username: String, cx: &Context<HomePage>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .mt_3()
        .flex()
        .h(px(38.0))
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .child(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    svg()
                        .path("icons/user.svg")
                        .size(px(24.0))
                        .text_color(theme.foreground),
                )
                .child(div().truncate().child(username)),
        )
        .child(
            svg()
                .path("icons/setting.svg")
                .size(px(17.0))
                .text_color(theme.muted_foreground),
        )
}

fn server_title(server: &CachedServer) -> String {
    server
        .server_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&server.endpoint.address)
        .to_string()
}
