use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, rgb, svg,
};

use crate::{app_metadata::APP_NAME, server::CachedServer, theme};

use super::{HomePage, carousel::HOME_SIDEBAR_WIDTH_PX, navigation::HomeRoot};

impl HomePage {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_sidebar(
        &self,
        cx: &Context<Self>,
        rounded_window: bool,
        on_back: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        on_home: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_favorites: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_search: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_settings: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
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
            .bg(theme.panel_background)
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
            .child(
                div()
                    .my_3()
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.title_bar_border),
            )
            .child(
                div()
                    .id("sidebar-servers")
                    .debug_selector(|| "sidebar-servers".to_string())
                    .flex()
                    .min_h_0()
                    .flex_1()
                    .flex_col()
                    .gap_1()
                    .overflow_y_scroll()
                    .scrollbar_width(px(0.0))
                    .children(self.servers.iter().map(|server| {
                        let server_id = server.id.clone();
                        server_list_item(
                            server,
                            server.id == self.current_server.id,
                            cx,
                            cx.listener(move |page, _, _, cx| page.switch_server(&server_id, cx)),
                        )
                    })),
            )
            .child(user_row(username, cx, on_settings))
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
            .flex_none()
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
                    .child(APP_NAME),
            )
    }
}

fn sidebar_nav_item(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    active: bool,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id(id)
        .cursor_pointer()
        .debug_selector(move || id.to_string())
        .flex()
        .flex_none()
        .h(px(34.0))
        .items_center()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .when(active, |this| {
            this.bg(theme.element_selected)
                .text_color(theme.accent_text)
        })
        .hover(move |style| {
            style.bg(if active {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .child(svg().path(icon).size(px(16.0)).text_color(if active {
            theme.accent_text
        } else {
            theme.foreground
        }))
        .child(label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx);
        })
}

fn server_list_item(
    server: &CachedServer,
    active: bool,
    cx: &Context<HomePage>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let server_id = server.id.clone();

    div()
        .id((gpui::ElementId::from("sidebar-server"), server_id.clone()))
        .debug_selector(move || format!("sidebar-server-{server_id}"))
        .flex()
        .flex_none()
        .h(px(36.0))
        .min_w_0()
        .items_center()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .when(active, |this| {
            this.bg(theme.element_selected)
                .text_color(theme.accent_text)
        })
        .hover(move |style| {
            style.bg(if active {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .cursor_pointer()
        .child(
            svg()
                .path("icons/emby.svg")
                .size(px(18.0))
                .flex_none()
                .text_color(rgb(0x53b34c)),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .child(server_title(server)),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx);
        })
}

fn user_row(
    username: String,
    cx: &Context<HomePage>,
    on_settings: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .debug_selector(|| "sidebar-user".to_string())
        .mt_3()
        .flex()
        .flex_none()
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
            div()
                .id("open-playback-settings")
                .flex()
                .size(px(30.0))
                .items_center()
                .justify_center()
                .rounded_md()
                .hover(move |style| style.bg(theme.secondary_hover))
                .child(
                    svg()
                        .path("icons/setting.svg")
                        .size(px(18.0))
                        .text_color(theme.foreground),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    on_settings(event, window, cx);
                }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emby::EmbyClient;
    use gpui::{Modifiers, Render, TestAppContext, point, size};

    struct SidebarHover;

    impl Render for SidebarHover {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(theme::get(cx).panel_background)
                .flex()
                .flex_col()
                .p_3()
                .children([false, true].map(|selected| {
                    sidebar_nav_item(
                        if selected { "selected" } else { "idle" },
                        "icons/home.svg",
                        "Home",
                        selected,
                        cx,
                        |_, _, _| {},
                    )
                }))
        }
    }

    #[gpui::test]
    fn latte_sidebar_items_change_fill_when_hovered(cx: &mut TestAppContext) {
        cx.update(|cx| theme::set(theme::ColorTheme::Latte, cx));
        let (_, cx) = cx.add_window_view(|_, _| SidebarHover);
        cx.run_until_parked();
        for selector in ["idle", "selected"] {
            cx.simulate_mouse_move(point(px(400.0), px(400.0)), None, Modifiers::default());
            cx.run_until_parked();
            let hover = cx.update(|_, cx| {
                let theme = theme::get(cx);
                if selector == "selected" {
                    theme.element_selected_hover
                } else {
                    theme.secondary_hover
                }
            });
            assert!(cx.update(|window, _| {
                window
                    .painted_quads()
                    .iter()
                    .all(|quad| quad.background != hover.into())
            }));
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
            cx.run_until_parked();
            assert!(cx.update(|window, _| {
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| quad.background == hover.into())
            }));
        }
    }

    #[gpui::test]
    fn sidebar_server_rows_keep_their_height_when_the_list_overflows(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let (_, cx) = cx.add_window_view(|_, cx| {
            let servers = (0..20)
                .map(|index| {
                    serde_json::from_value(serde_json::json!({
                        "id": format!("server-{index}"),
                        "server_name": "A long server name that should truncate within the sidebar",
                        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
                        "username": "test", "password": "", "added_at_unix": 0
                    }))
                    .unwrap()
                })
                .collect::<Vec<CachedServer>>();
            // Missing user IDs prevent background effects from starting.
            HomePage::new(servers[0].clone(), servers, EmbyClient::new("test".into()).unwrap(), cx)
        });
        for height in [900.0, 500.0, 720.0] {
            cx.simulate_resize(size(px(1100.0), px(height)));
            cx.run_until_parked();
            let first = cx.debug_bounds("sidebar-server-server-0").unwrap();
            let second = cx.debug_bounds("sidebar-server-server-1").unwrap();
            let list = cx.debug_bounds("sidebar-servers").unwrap();
            let user = cx.debug_bounds("sidebar-user").unwrap();
            assert_eq!(first.size.height, px(36.0));
            assert_eq!(second.top() - first.bottom(), px(4.0));
            assert!(first.left() >= list.left());
            assert!(first.right() <= list.right());
            assert!(list.bottom() < user.top());
            assert_eq!(user.size.height, px(38.0));
            assert!(user.bottom() <= px(height));
        }
    }
}
