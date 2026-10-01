mod adapter;
pub(in crate::home) mod controller;
pub(super) mod reorder;

use super::{
    carousel::HOME_SIDEBAR_WIDTH_PX,
    navigation::HomeRoot,
    sidebar::controller::{SidebarRow, SidebarViewModel},
};
use crate::{
    app_metadata::APP_NAME,
    theme,
    ui::{radius, server_icon::server_icon},
};
use gpui::{
    Animation, AnimationExt as _, App, AppContext as _, ClickEvent, Corners, DragMoveEvent,
    EntityId, FocusHandle, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Transformation, Window, div,
    percentage, prelude::FluentBuilder, px, svg,
};
use reorder::DraggedSidebarServer;
use std::{rc::Rc, time::Duration};

pub(super) enum SidebarViewIntent {
    Back,
    Add,
    Settings,
    Root { root: HomeRoot, refresh: bool },
    Select(String),
    BeginReorder(String),
    PreviewReorder(usize),
    FinishReorder(bool),
}
pub(super) type SidebarListener = Rc<dyn Fn(SidebarViewIntent, &mut Window, &mut App)>;
pub(super) struct SidebarProps<'a> {
    pub(super) view: SidebarViewModel<'a>,
    pub(super) active_root: HomeRoot,
    pub(super) corners: Corners<Pixels>,
    pub(super) owner: EntityId,
    pub(super) scroll: ScrollHandle,
    pub(super) reorder_focus: Option<FocusHandle>,
}

pub(super) fn render_sidebar(
    props: SidebarProps<'_>,
    on_intent: SidebarListener,
    cx: &App,
) -> gpui::AnyElement {
    let theme = theme::get(cx);
    let SidebarProps {
        view,
        active_root,
        corners,
        owner,
        scroll,
        reorder_focus,
    } = props;
    let back = on_intent.clone();
    let settings = on_intent.clone();
    div()
        .flex()
        .h_full()
        .w(px(HOME_SIDEBAR_WIDTH_PX))
        .flex_col()
        .border_r_1()
        .border_color(theme.title_bar_border)
        .bg(theme.panel_background)
        .rounded_bl(corners.bottom_left)
        .overflow_hidden()
        .p_3()
        .child(render_title_row(cx, move |_, window, cx| {
            back(SidebarViewIntent::Back, window, cx)
        }))
        .child(div().h(px(12.0)).flex_none())
        .gap_1()
        .children(
            [
                (HomeRoot::Home, "home-section", "icons/home.svg"),
                (HomeRoot::Favorites, "favorites-section", "icons/heart.svg"),
                (HomeRoot::Search, "search-section", "icons/search.svg"),
            ]
            .map(|(root, id, icon)| {
                let on_intent = on_intent.clone();
                sidebar_nav_item(
                    id,
                    icon,
                    root.title(),
                    active_root == root,
                    cx,
                    move |event, window, cx| {
                        on_intent(
                            SidebarViewIntent::Root {
                                root,
                                refresh: root == HomeRoot::Home && event.click_count() == 2,
                            },
                            window,
                            cx,
                        );
                    },
                )
            }),
        )
        .child(
            div()
                .my_3()
                .h(px(1.0))
                .flex_none()
                .bg(theme.title_bar_border),
        )
        .child(
            div()
                .flex()
                .min_h_0()
                .flex_1()
                .flex_col()
                .gap_1()
                .child(render_server_list(
                    view.rows,
                    owner,
                    scroll,
                    reorder_focus,
                    on_intent.clone(),
                    cx,
                ))
                .child(add_server_item(on_intent, cx)),
        )
        .child(
            div()
                .my_3()
                .h(px(1.0))
                .flex_none()
                .bg(theme.title_bar_border),
        )
        .child(user_row(
            view.username.to_owned(),
            cx,
            move |_, window, cx| settings(SidebarViewIntent::Settings, window, cx),
        ))
        .into_any_element()
}

fn render_server_list(
    rows: Vec<SidebarRow<'_>>,
    owner: EntityId,
    scroll: ScrollHandle,
    focus: Option<FocusHandle>,
    on_intent: SidebarListener,
    cx: &App,
) -> gpui::AnyElement {
    let on_key = on_intent.clone();
    div()
        .id("sidebar-servers")
        .debug_selector(|| "sidebar-servers".into())
        .flex()
        .min_h_0()
        .flex_shrink_1()
        .flex_col()
        .gap_1()
        .overflow_y_scroll()
        .scrollbar_width(px(0.0))
        .track_scroll(&scroll)
        .when_some(focus, |this, focus| {
            this.track_focus(&focus)
                .on_key_down(move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_active_drag(window);
                        on_key(SidebarViewIntent::FinishReorder(false), window, cx);
                        cx.stop_propagation();
                    }
                })
        })
        .children(rows.into_iter().enumerate().map(|(index, row)| {
            let on_move = on_intent.clone();
            let on_drop = on_intent.clone();
            let scroll = scroll.clone();
            div()
                .id((
                    gpui::ElementId::from("sidebar-server-slot"),
                    row.server.id.clone(),
                ))
                .flex_none()
                .on_drag_move(
                    move |event: &DragMoveEvent<DraggedSidebarServer>, window, cx| {
                        if event.drag(cx).owner == owner
                            && event.bounds.contains(&event.event.position)
                            && scroll.bounds().contains(&event.event.position)
                        {
                            on_move(SidebarViewIntent::PreviewReorder(index), window, cx);
                        }
                    },
                )
                .on_drop(move |drag: &DraggedSidebarServer, window, cx| {
                    if drag.owner == owner {
                        on_drop(SidebarViewIntent::PreviewReorder(index), window, cx);
                        on_drop(SidebarViewIntent::FinishReorder(true), window, cx);
                    }
                })
                .child(server_list_item(row, owner, on_intent.clone(), cx))
        }))
        .into_any_element()
}

fn render_title_row(
    cx: &App,
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
                .debug_selector(|| "home-back".into())
                .cursor_pointer()
                .absolute()
                .left_0()
                .flex()
                .size(px(32.0))
                .items_center()
                .justify_center()
                .rounded(radius::CONTROL)
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
        .rounded(radius::CONTROL)
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
    row: SidebarRow<'_>,
    owner: EntityId,
    on_intent: SidebarListener,
    cx: &App,
) -> gpui::AnyElement {
    let SidebarRow {
        server,
        active,
        placeholder,
        loading,
        can_reorder,
    } = row;
    let theme = theme::get(cx);
    let server_id = server.id.clone();
    let drag = DraggedSidebarServer::new(server, owner);

    div()
        .id((gpui::ElementId::from("sidebar-server"), server_id.clone()))
        .debug_selector(move || format!("sidebar-server-{server_id}"))
        .flex()
        .flex_none()
        .h(px(36.0))
        .min_w_0()
        .items_center()
        .gap_2()
        .rounded(radius::CONTROL)
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .when(active, |this| {
            this.bg(theme.element_selected)
                .text_color(theme.accent_text)
        })
        .when(placeholder, |this| {
            this.bg(theme.element_selected).opacity(0.3)
        })
        .hover(move |style| {
            style.bg(if active {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .cursor_default()
        .when(!loading, |this| this.cursor_pointer())
        .child(server_icon(server.icon_url.as_deref(), 18.0))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .child(server.title.clone()),
        )
        .when(loading, |this| {
            let id = server.id.clone();
            this.child(
                div()
                    .id("sidebar-server-loading")
                    .debug_selector(move || format!("sidebar-server-loading-{id}"))
                    .aria_label("正在连接服务器")
                    .flex()
                    .flex_none()
                    .size(px(16.0))
                    .child(
                        svg()
                            .path("icons/loader.svg")
                            .size_full()
                            .text_color(theme.muted_foreground)
                            .with_animation(
                                SharedString::from(format!("sidebar-server-loader-{}", server.id)),
                                Animation::new(Duration::from_millis(1800)).repeat(),
                                |icon, delta| {
                                    icon.with_transformation(Transformation::rotate(percentage(
                                        delta,
                                    )))
                                },
                            ),
                    ),
            )
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click({
            let on_intent = on_intent.clone();
            let id = server.id.clone();
            move |_, window, cx| {
                cx.stop_propagation();
                on_intent(SidebarViewIntent::Select(id.clone()), window, cx);
            }
        })
        .when(can_reorder, |this| {
            this.on_drag(drag, move |drag, _, window, cx| {
                on_intent(
                    SidebarViewIntent::BeginReorder(drag.server_id.clone()),
                    window,
                    cx,
                );
                window.defer(cx, |window, cx| {
                    cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
                });
                cx.new(|_| drag.clone())
            })
        })
        .into_any_element()
}

fn add_server_item(on_intent: SidebarListener, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .id("sidebar-add-server")
        .debug_selector(|| "sidebar-add-server".into())
        .aria_label("添加服务器")
        .flex()
        .flex_none()
        .h(px(36.0))
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .bg(theme.dialog_background)
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(16.0))
                .text_color(theme.foreground),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_intent(SidebarViewIntent::Add, window, cx);
        })
}

fn user_row(
    username: String,
    cx: &App,
    on_settings: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .debug_selector(|| "sidebar-user".to_string())
        .flex()
        .flex_none()
        .h(px(38.0))
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(radius::CONTROL)
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
                .cursor_pointer()
                .flex()
                .size(px(30.0))
                .items_center()
                .justify_center()
                .rounded(radius::CONTROL)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{emby::EmbyClient, home::HomePage, server::CachedServer};
    use gpui::{Context, Modifiers, Render, TestAppContext, point, size};

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
            HomePage::new(servers[0].clone(), servers.iter().map(crate::server::SidebarServer::from).collect(), EmbyClient::new("test".into()).unwrap(), cx)
        });
        for height in [900.0, 500.0, 720.0] {
            cx.simulate_resize(size(px(1100.0), px(height)));
            cx.run_until_parked();
            let first = cx.debug_bounds("sidebar-server-server-0").unwrap();
            let second = cx.debug_bounds("sidebar-server-server-1").unwrap();
            let list = cx.debug_bounds("sidebar-servers").unwrap();
            let add = cx.debug_bounds("sidebar-add-server").unwrap();
            let user = cx.debug_bounds("sidebar-user").unwrap();
            assert_eq!(first.size.height, px(36.0));
            assert_eq!(second.top() - first.bottom(), px(4.0));
            assert!(first.left() >= list.left());
            assert!(first.right() <= list.right());
            assert!(list.bottom() < user.top());
            assert_eq!(add.top() - list.bottom(), px(4.0));
            assert_eq!(add.size.height, px(36.0));
            assert!(add.bottom() < user.top());
            assert_eq!(user.size.height, px(38.0));
            assert!(user.bottom() <= px(height));
            cx.simulate_mouse_move(list.center(), None, Modifiers::default());
            cx.simulate_event(gpui::ScrollWheelEvent {
                position: list.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-2000.0))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            assert_eq!(cx.debug_bounds("sidebar-add-server").unwrap(), add);
            assert_eq!(cx.debug_bounds("sidebar-user").unwrap(), user);
            let last = cx.debug_bounds("sidebar-server-server-19").unwrap();
            assert!(last.top() >= list.top());
            assert!(last.bottom() <= list.bottom());
            assert!(cx.debug_bounds("sidebar-server-server-0").unwrap().top() < list.top());
        }
    }
}
