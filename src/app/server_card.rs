use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, AppContext as _, Context, ElementId, EntityId, FocusHandle,
    Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels,
    Point, Render, SharedString, StatefulInteractiveElement, Styled, Transformation, Window,
    anchored, div, percentage, point, prelude::FluentBuilder, px, svg,
};

use crate::ui::radius;
use crate::{emby::ItemCounts, server::CachedServer, theme, ui::server_icon::server_icon};

use super::TinyApp;

pub(super) const SERVER_CARD_WIDTH_PX: f32 = 190.0;
pub(super) const SERVER_CARD_HEIGHT_PX: f32 = 96.0;
const SERVER_CARD_LOADER_ANIMATION_MS: u64 = 1800;

#[derive(Clone)]
pub(super) struct ServerContextMenu {
    pub(super) server_id: String,
    pub(super) position: Point<Pixels>,
    pub(super) focus: FocusHandle,
    pub(super) previous_focus: Option<FocusHandle>,
}

pub(super) struct ServerCardActions<Select, OpenMenu> {
    pub(super) on_select: Select,
    pub(super) on_context_menu: OpenMenu,
}

pub(super) struct ServerCardState {
    pub(super) loading: bool,
    pub(super) can_reorder: bool,
    pub(super) placeholder: bool,
    pub(super) auto_start: bool,
}

#[derive(Clone)]
pub(super) struct DraggedServer {
    pub(super) owner: EntityId,
    server_id: String,
    title: String,
    icon_url: Option<String>,
    counts: Option<ItemCounts>,
    auto_start: bool,
}

impl Render for DraggedServer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .w(px(SERVER_CARD_WIDTH_PX))
            .h(px(SERVER_CARD_HEIGHT_PX))
            .rounded(radius::CARD)
            .border_1()
            .border_color(theme.accent)
            .bg(theme.dialog_background)
            .shadow_lg()
            .opacity(0.9)
            .child(server_card_content(
                self.title.clone(),
                self.icon_url.as_deref(),
                self.counts.clone(),
                None,
                self.auto_start,
                cx,
            ))
    }
}

pub(super) fn add_server_card(
    cx: &Context<TinyApp>,
    on_add: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("add-server-card")
        .flex()
        .flex_none()
        .w(px(SERVER_CARD_WIDTH_PX))
        .h(px(SERVER_CARD_HEIGHT_PX))
        .items_center()
        .justify_center()
        .rounded(radius::CARD)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(move |style| {
            style
                .bg(theme.secondary_hover)
                .border_color(theme.input_border_focused)
                .text_color(theme.foreground)
        })
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(24.0))
                .text_color(theme.foreground),
        )
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_add(event, window, cx);
        })
}

pub(super) fn server_card<Select, OpenMenu>(
    server: CachedServer,
    counts: Option<ItemCounts>,
    state: ServerCardState,
    cx: &Context<TinyApp>,
    actions: ServerCardActions<Select, OpenMenu>,
) -> impl IntoElement
where
    Select: Fn(&CachedServer, &mut Window, &mut App) + 'static,
    OpenMenu: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let theme = theme::get(cx);
    let ServerCardState {
        loading,
        can_reorder,
        placeholder,
        auto_start,
    } = state;
    let ServerCardActions {
        on_select,
        on_context_menu,
    } = actions;
    let title = server
        .server_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&server.endpoint.address)
        .to_string();
    let selected_server = server.clone();
    let selector = format!("server-card-{}", server.id);
    let loader_server_id = loading.then(|| server.id.clone());
    let card_id = (ElementId::from("server-card"), server.id.clone());
    let owner = cx.entity_id();
    let app = cx.weak_entity();
    let drag = DraggedServer {
        owner,
        server_id: server.id.clone(),
        title: title.clone(),
        icon_url: server.icon_url.clone(),
        counts: counts.clone(),
        auto_start,
    };

    div()
        .id(card_id)
        .debug_selector(move || selector.clone())
        .relative()
        .flex_none()
        .w(px(SERVER_CARD_WIDTH_PX))
        .h(px(SERVER_CARD_HEIGHT_PX))
        .rounded(radius::CARD)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .when(placeholder, |this| {
            this.bg(theme.element_selected)
                .border_color(theme.accent)
                .opacity(0.3)
        })
        .cursor_default()
        .when(!loading, |this| this.cursor_pointer())
        .hover(move |style| {
            style
                .bg(theme.secondary_hover)
                .border_color(theme.input_border_focused)
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_select(&selected_server, window, cx);
        })
        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
            cx.stop_propagation();
            if !loading && !cx.has_active_drag() {
                on_context_menu(event, window, cx);
            }
        })
        .when(can_reorder, |this| {
            this.on_drag(drag, move |drag, _, window, cx| {
                app.update(cx, |app, cx| {
                    app.begin_server_reorder(&drag.server_id, window, cx)
                })
                .ok();
                // GPUI installs the active drag after this callback returns.
                window.defer(cx, |window, cx| {
                    cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
                });
                cx.new(|_| drag.clone())
            })
        })
        .child(server_card_content(
            title,
            server.icon_url.as_deref(),
            counts,
            loader_server_id,
            auto_start,
            cx,
        ))
}

fn server_card_content(
    title: String,
    icon_url: Option<&str>,
    counts: Option<ItemCounts>,
    loading_server_id: Option<String>,
    auto_start: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .relative()
        .flex()
        .flex_col()
        .size_full()
        .justify_between()
        .p_3()
        .child(
            div()
                .flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .text_base()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(server_icon(icon_url, 28.0))
                .child(div().flex_1().min_w_0().text_ellipsis().child(title)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(20.0))
                .gap_2()
                .child(div().flex().min_w_0().when_some(counts, |this, counts| {
                    this.child(server_counts_row(counts, cx))
                }))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .when_some(loading_server_id, |this, server_id| {
                            this.child(loader_icon(server_id, cx))
                        })
                        .when(auto_start, |this| {
                            this.child(
                                div()
                                    .id("server-auto-start")
                                    .debug_selector(|| "server-auto-start".into())
                                    .aria_label("自动启动")
                                    .flex()
                                    .flex_none()
                                    .size(px(16.0))
                                    .child(
                                        svg()
                                            .path("icons/zap.svg")
                                            .size_full()
                                            .text_color(theme.accent),
                                    ),
                            )
                        }),
                ),
        )
}

fn server_counts_row(counts: ItemCounts, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(server_count_item(
            "icons/film.svg",
            counts.movie_count,
            theme.muted_foreground,
        ))
        .child(server_count_item(
            "icons/tv.svg",
            counts.series_count,
            theme.muted_foreground,
        ))
}

fn server_count_item(icon: &'static str, value: u32, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(svg().path(icon).size(px(13.0)).text_color(color))
        .child(format!("{value}"))
}

fn loader_icon(server_id: String, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    let animation_id = SharedString::from(format!("server-card-loader-{server_id}"));

    svg()
        .path("icons/loader.svg")
        .size(px(16.0))
        .overflow_hidden()
        .text_color(theme.muted_foreground)
        .with_animation(
            animation_id,
            Animation::new(Duration::from_millis(SERVER_CARD_LOADER_ANIMATION_MS)).repeat(),
            |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
        )
}

pub(super) fn server_card_menu(
    server: CachedServer,
    menu: ServerContextMenu,
    auto_start: bool,
    cx: &Context<TinyApp>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let menu_id = ElementId::from(format!("server-card-menu-{}", server.id));
    let edit_server = server.clone();
    let icon_server = server.clone();
    let auto_start_server = server.clone();
    let delete_server = server;
    let on_edit = cx.listener(TinyApp::open_edit_server_dialog);
    let on_choose_icon = cx.listener(TinyApp::open_server_icon_picker);
    let on_delete = cx.listener(TinyApp::delete_server);
    let on_auto_start = cx.listener(TinyApp::toggle_server_auto_start);

    anchored()
        .position(menu.position)
        .offset(point(px(4.0), px(4.0)))
        .snap_to_window_with_margin(px(8.0))
        .child(
            div()
                .id("server-context-menu")
                .debug_selector(|| "server-context-menu".into())
                .track_focus(&menu.focus)
                .occlude()
                .cursor_default()
                .flex()
                .flex_col()
                .w(px(128.0))
                .rounded(radius::SURFACE)
                .border_1()
                .border_color(theme.context_menu.border)
                .bg(theme.context_menu.background)
                .shadow_lg()
                .p(px(4.0))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(cx.listener(TinyApp::close_server_menu))
                .on_key_down(cx.listener(|app, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        app.dismiss_server_menu(window, cx);
                    }
                }))
                .child(menu_item(
                    (menu_id.clone(), "edit"),
                    "编辑",
                    false,
                    move |window, cx| {
                        on_edit(&edit_server, window, cx);
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id.clone(), "choose-icon"),
                    "选择图标",
                    false,
                    move |window, cx| {
                        on_choose_icon(&icon_server, window, cx);
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id.clone(), "auto-start"),
                    if auto_start {
                        "取消自动启动"
                    } else {
                        "自动启动"
                    },
                    false,
                    move |window, cx| {
                        on_auto_start(&auto_start_server, window, cx);
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id, "delete"),
                    "删除",
                    true,
                    move |window, cx| {
                        on_delete(&delete_server, window, cx);
                    },
                    cx,
                )),
        )
}

fn menu_item(
    id: impl Into<ElementId>,
    label: &'static str,
    destructive: bool,
    action: impl Fn(&mut Window, &mut App) + 'static,
    cx: &Context<TinyApp>,
) -> impl IntoElement {
    let colors = &theme::get(cx).context_menu;
    let hover_background = if destructive {
        colors.destructive_hover_background
    } else {
        colors.hover_background
    };

    div()
        .id(id)
        .debug_selector(move || format!("server-context-menu-{label}"))
        .cursor_pointer()
        .flex()
        .h(px(30.0))
        .items_center()
        .rounded(radius::CONTROL)
        .px_2()
        .text_sm()
        .text_color(if destructive {
            colors.destructive_foreground
        } else {
            colors.foreground
        })
        .hover(move |style| style.bg(hover_background))
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            action(window, cx);
        })
}

#[cfg(test)]
mod tests {
    mod sidebar;

    use super::*;
    use crate::{
        app::Page,
        storage::{self, ServerCache},
    };
    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, size};

    fn servers_window(cx: &mut TestAppContext) -> (Entity<TinyApp>, &mut VisualTestContext) {
        servers_window_with_ids(cx, &["first", "second"])
    }

    fn servers_window_with_ids<'a>(
        cx: &'a mut TestAppContext,
        ids: &[&str],
    ) -> (Entity<TinyApp>, &'a mut VisualTestContext) {
        servers_window_with_cache(cx, server_cache_with_ids(ids))
    }

    fn server_cache_with_ids(ids: &[&str]) -> ServerCache {
        let mut cache = ServerCache::empty();
        cache.servers = ids
            .iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "id": id, "server_name": id,
                    // Requests fail locally if the left-click test enters the server.
                    "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
                    "username": id, "password": "", "user_id": id,
                    "access_token": "test-token", "added_at_unix": 0
                }))
                .unwrap()
            })
            .collect();
        cache
    }

    fn servers_window_with_cache(
        cx: &mut TestAppContext,
        cache: ServerCache,
    ) -> (Entity<TinyApp>, &mut VisualTestContext) {
        cx.update(theme::init);
        cx.add_window_view(|_, cx| {
            let mut app = TinyApp::new(cache, None, cx);
            app.window_persistence_enabled = false;
            app
        })
    }

    fn right_click(cx: &mut VisualTestContext, position: Point<Pixels>) {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(position, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
    }

    fn begin_drag(cx: &mut VisualTestContext, position: Point<Pixels>) {
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        cx.simulate_mouse_move(
            position + point(px(12.0), px(0.0)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.run_until_parked();
    }

    fn drop_at(cx: &mut VisualTestContext, position: Point<Pixels>) {
        cx.simulate_mouse_move(position, Some(MouseButton::Left), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        assert!(!cx.update(|_, cx| cx.has_active_drag()));
    }

    fn advance_animation(cx: &mut VisualTestContext, duration: Duration) {
        cx.executor().advance_clock(duration);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    fn assert_preview(app: &Entity<TinyApp>, cx: &mut VisualTestContext, expected: &[&str]) {
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.preview_servers()
                    .iter()
                    .map(|server| server.id.as_str())
                    .collect::<Vec<_>>(),
                expected,
            );
        });
    }

    fn assert_order(app: &Entity<TinyApp>, cx: &mut VisualTestContext, expected: &[&str]) {
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Servers));
            assert!(app.selecting_server_id.is_none());
            for servers in [&app.servers, &app.cache.servers] {
                assert_eq!(
                    servers
                        .iter()
                        .map(|server| server.id.as_str())
                        .collect::<Vec<_>>(),
                    expected,
                );
            }
        });
    }

    #[gpui::test]
    fn auto_start_menu_moves_badge_and_saves_only_one_server(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let (app, cx) = servers_window(cx);
        app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
        cx.simulate_resize(size(px(800.0), px(500.0)));
        cx.run_until_parked();
        assert!(cx.debug_bounds("server-auto-start").is_none());
        for (selector, action, expected) in [
            (
                "server-card-first",
                "server-context-menu-自动启动",
                Some("first"),
            ),
            (
                "server-card-second",
                "server-context-menu-自动启动",
                Some("second"),
            ),
            (
                "server-card-second",
                "server-context-menu-取消自动启动",
                None,
            ),
        ] {
            let card = cx.debug_bounds(selector).unwrap();
            right_click(cx, card.center());
            let item = cx.debug_bounds(action).unwrap();
            cx.simulate_click(item.center(), Modifiers::default());
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                assert!(matches!(app.page, Page::Servers));
                assert!(app.open_server_menu.is_none());
                assert_eq!(app.cache.auto_start_server_id.as_deref(), expected);
            });
            let badge = cx.debug_bounds("server-auto-start");
            if expected.is_some() {
                let badge = badge.unwrap();
                assert!(card.contains(&badge.center()));
                assert!(badge.left() > card.center().x && badge.top() > card.center().y);
            } else {
                assert!(badge.is_none());
            }
            cx.executor().advance_clock(Duration::from_millis(350));
            cx.run_until_parked();
            assert_eq!(
                storage::load_or_init_from(&path)
                    .unwrap()
                    .auto_start_server_id
                    .as_deref(),
                expected,
            );
        }
    }

    #[gpui::test]
    fn auto_start_reopens_saved_server_and_allows_returning_to_cards(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = server_cache_with_ids(&["first", "second"]);
        cache.auto_start_server_id = Some("second".into());
        // Ordering does not change the selected startup server.
        cache.servers.reverse();
        storage::save_to(&cache, &path).unwrap();
        let (app, cx) = servers_window_with_cache(cx, storage::load_or_init_from(&path).unwrap());
        cx.simulate_resize(size(px(1100.0), px(720.0)));
        cx.run_until_parked();
        let home_id = app.read_with(cx, |app, _| match &app.page {
            Page::Home(home) => home.entity_id(),
            _ => panic!("auto start must open the saved server"),
        });
        // Clicking the already active sidebar server keeps the same Home page.
        let current = cx.debug_bounds("sidebar-server-second").unwrap();
        cx.simulate_click(current.center(), Modifiers::default());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            assert!(matches!(&app.page, Page::Home(home) if home.entity_id() == home_id));
            app.show_servers_page_from_home(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Servers));
            assert_eq!(app.cache.auto_start_server_id.as_deref(), Some("second"));
        });
        assert!(cx.debug_bounds("server-auto-start").is_some());
    }

    #[gpui::test]
    fn disabled_or_missing_auto_start_server_keeps_the_server_list(cx: &mut TestAppContext) {
        cx.update(theme::init);
        for target in [None, Some("missing")] {
            let mut cache = server_cache_with_ids(&["first"]);
            cache.auto_start_server_id = target.map(str::to_owned);
            let app = cx.new(|cx| TinyApp::new(cache, None, cx));
            app.read_with(cx, |app, _| {
                assert!(matches!(app.page, Page::Servers));
                assert!(app.selecting_server_id.is_none());
            });
        }
    }

    #[gpui::test]
    fn auto_start_login_failure_keeps_cards_available(cx: &mut TestAppContext) {
        let mut cache = server_cache_with_ids(&["first"]);
        cache.auto_start_server_id = Some("first".into());
        cache.servers[0].access_token = None;
        let (app, cx) = servers_window_with_cache(cx, cache);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Servers));
            assert!(app.selecting_server_id.is_none());
            assert!(app.has_server_page_notifications());
            assert_eq!(app.cache.auto_start_server_id.as_deref(), Some("first"));
        });
        let card = cx.debug_bounds("server-card-first").unwrap();
        right_click(cx, card.center());
        assert!(
            cx.debug_bounds("server-context-menu-取消自动启动")
                .is_some()
        );
    }

    #[gpui::test]
    fn dragging_cards_across_rows_saves_order_without_opening_a_server(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let (app, cx) = servers_window_with_ids(cx, &["first", "second", "third"]);
        app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
        cx.simulate_resize(size(px(500.0), px(450.0)));
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        let third = cx.debug_bounds("server-card-third").unwrap();
        assert!(third.top() > first.bottom());
        right_click(cx, first.origin + point(px(10.0), px(80.0)));

        begin_drag(cx, first.center());
        assert!(cx.update(|_, cx| cx.has_active_drag()));
        assert!(app.read_with(cx, |app, _| app.open_server_menu.is_none()));
        assert_order(&app, cx, &["first", "second", "third"]);
        // A refresh during the drag must survive moving the current cached record.
        app.update(cx, |app, cx| {
            app.cache.servers[0].server_name = Some("Refreshed first".into());
            app.cache.servers[0].access_token = Some("refreshed-token".into());
            app.servers = app.cache.servers.clone();
            cx.notify();
        });
        drop_at(cx, third.center());
        assert_order(&app, cx, &["second", "third", "first"]);
        advance_animation(cx, Duration::from_millis(180));

        let third = cx.debug_bounds("server-card-third").unwrap();
        let second = cx.debug_bounds("server-card-second").unwrap();
        begin_drag(cx, third.center());
        drop_at(cx, second.center());
        assert_order(&app, cx, &["third", "second", "first"]);
        cx.executor().advance_clock(Duration::from_millis(350));
        cx.run_until_parked();
        assert!(path.exists());
        let saved = storage::load_or_init_from(&path).unwrap();
        assert_eq!(
            saved
                .servers
                .iter()
                .map(|server| server.id.as_str())
                .collect::<Vec<_>>(),
            ["third", "second", "first"],
        );
        assert_eq!(
            saved.servers[2].server_name.as_deref(),
            Some("Refreshed first")
        );
        assert_eq!(
            saved.servers[2].access_token.as_deref(),
            Some("refreshed-token")
        );

        let card = cx.debug_bounds("server-card-first").unwrap();
        right_click(cx, card.center());
        assert_eq!(
            app.read_with(cx, |app, _| app
                .open_server_menu
                .as_ref()
                .unwrap()
                .server_id
                .clone()),
            "first",
        );
    }

    #[gpui::test]
    fn dragging_previews_order_and_animates_cards_without_saving_until_drop(
        cx: &mut TestAppContext,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let (app, cx) = servers_window_with_ids(cx, &["first", "second", "third"]);
        app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
        cx.simulate_resize(size(px(500.0), px(450.0)));
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        let second = cx.debug_bounds("server-card-second").unwrap();
        let third = cx.debug_bounds("server-card-third").unwrap();
        begin_drag(cx, first.center());
        cx.simulate_mouse_move(
            third.center(),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert_preview(&app, cx, &["second", "third", "first"]);
        assert_order(&app, cx, &["first", "second", "third"]);
        assert_eq!(
            cx.debug_bounds("server-card-first").unwrap().origin,
            third.origin
        );
        assert_eq!(
            cx.debug_bounds("server-card-second").unwrap().origin,
            second.origin
        );

        advance_animation(cx, Duration::from_millis(90));
        let halfway = cx.debug_bounds("server-card-second").unwrap();
        assert!(halfway.left() > first.left() && halfway.left() < second.left());
        // Repeated pointer motion over the same slot must not toggle the order
        // when the displaced cards animate beneath it.
        for offset in [1.0, 2.0, 0.0] {
            cx.simulate_mouse_move(
                third.center() + point(px(offset), px(0.0)),
                Some(MouseButton::Left),
                Modifiers::default(),
            );
            cx.run_until_parked();
            assert_preview(&app, cx, &["second", "third", "first"]);
        }
        advance_animation(cx, Duration::from_millis(400));
        assert_eq!(
            cx.debug_bounds("server-card-second").unwrap().origin,
            first.origin
        );
        assert_eq!(
            cx.debug_bounds("server-card-third").unwrap().origin,
            second.origin
        );
        assert!(!path.exists());
        assert!(app.read_with(cx, |app, _| app.pending_cache_save_error_prefix.is_none()));

        drop_at(cx, third.center());
        assert_order(&app, cx, &["second", "third", "first"]);
        assert!(app.read_with(cx, |app, _| app.server_reorder.is_none()));
        advance_animation(cx, Duration::from_millis(350));
        assert!(path.exists());
    }

    #[gpui::test]
    fn escape_and_outside_drop_restore_preview_without_saving(cx: &mut TestAppContext) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(800.0), px(500.0)));
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        let second = cx.debug_bounds("server-card-second").unwrap();
        for escape in [true, false] {
            begin_drag(cx, first.center());
            cx.simulate_mouse_move(
                second.center(),
                Some(MouseButton::Left),
                Modifiers::default(),
            );
            cx.run_until_parked();
            assert_preview(&app, cx, &["second", "first"]);
            advance_animation(cx, Duration::from_millis(90));
            if escape {
                cx.simulate_keystrokes("escape");
                cx.run_until_parked();
                assert!(!cx.update(|_, cx| cx.has_active_drag()));
            }
            drop_at(cx, point(px(700.0), px(400.0)));
            assert!(app.read_with(cx, |app, _| app.server_reorder.is_none()));
            assert_order(&app, cx, &["first", "second"]);
            assert_preview(&app, cx, &["first", "second"]);
            assert!(app.read_with(cx, |app, _| app.pending_cache_save_error_prefix.is_none()));
            advance_animation(cx, Duration::from_millis(180));
            assert_eq!(
                cx.debug_bounds("server-card-first").unwrap().origin,
                first.origin
            );
            assert_eq!(
                cx.debug_bounds("server-card-second").unwrap().origin,
                second.origin
            );
        }
    }

    #[gpui::test]
    fn dropping_outside_cards_or_on_the_source_keeps_order_and_does_not_navigate(
        cx: &mut TestAppContext,
    ) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(800.0), px(500.0)));
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        for target in [point(px(700.0), px(400.0)), first.center()] {
            begin_drag(cx, first.center());
            assert!(cx.update(|_, cx| cx.has_active_drag()));
            drop_at(cx, target);
            assert_order(&app, cx, &["first", "second"]);
            assert!(app.read_with(cx, |app, _| app.pending_cache_save_error_prefix.is_none()));
        }
    }

    #[gpui::test]
    fn loading_or_missing_servers_do_not_allow_reordering(cx: &mut TestAppContext) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(800.0), px(500.0)));
        app.update(cx, |app, cx| {
            app.reorder_server("missing", "second", cx);
            app.reorder_server("first", "missing", cx);
            app.reorder_server("first", "first", cx);
            app.selecting_server_id = Some("first".into());
            app.reorder_server("first", "second", cx);
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        let second = cx.debug_bounds("server-card-second").unwrap();
        begin_drag(cx, first.center());
        assert!(!cx.update(|_, cx| cx.has_active_drag()));
        drop_at(cx, second.center());
        app.update(cx, |app, _| app.selecting_server_id = None);
        assert_order(&app, cx, &["first", "second"]);
        assert!(app.read_with(cx, |app, _| app.pending_cache_save_error_prefix.is_none()));
    }

    #[gpui::test]
    fn right_click_opens_and_repositions_menu_without_selecting_a_server(cx: &mut TestAppContext) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(800.0), px(500.0)));
        cx.run_until_parked();
        for (selector, offset) in [
            ("server-card-first", point(px(30.0), px(30.0))),
            (
                "server-card-first",
                point(px(SERVER_CARD_WIDTH_PX - 10.0), px(85.0)),
            ),
            ("server-card-second", point(px(100.0), px(30.0))),
        ] {
            let position = cx.debug_bounds(selector).unwrap().origin + offset;
            right_click(cx, position);
            app.read_with(cx, |app, _| {
                assert!(matches!(app.page, Page::Servers));
                assert!(app.selecting_server_id.is_none());
                let menu = app.open_server_menu.as_ref().unwrap();
                assert_eq!(
                    menu.server_id,
                    selector.strip_prefix("server-card-").unwrap()
                );
                assert_eq!(menu.position, position);
            });
            let menu = cx.debug_bounds("server-context-menu").unwrap();
            assert_eq!(menu.origin, position + point(px(4.0), px(4.0)));
            for item in [
                "server-context-menu-编辑",
                "server-context-menu-选择图标",
                "server-context-menu-删除",
            ] {
                let bounds = cx.debug_bounds(item).unwrap();
                assert!(menu.contains(&bounds.center()));
            }
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("server-context-menu").is_none());
        assert!(app.read_with(cx, |app, _| app.open_server_menu.is_none()));

        for button in [MouseButton::Left, MouseButton::Right] {
            let card = cx.debug_bounds("server-card-first").unwrap();
            right_click(cx, card.center());
            let outside = point(px(700.0), px(400.0));
            cx.simulate_mouse_move(outside, None, Modifiers::default());
            cx.simulate_mouse_down(outside, button, Modifiers::default());
            cx.simulate_mouse_up(outside, button, Modifiers::default());
            cx.run_until_parked();
            assert!(app.read_with(cx, |app, _| app.open_server_menu.is_none()));
        }
    }

    #[gpui::test]
    fn context_menu_stays_in_window_and_edits_the_right_clicked_server(cx: &mut TestAppContext) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(500.0), px(200.0)));
        cx.run_until_parked();
        let card = cx.debug_bounds("server-card-second").unwrap();
        right_click(cx, card.bottom_right() - point(px(10.0), px(10.0)));
        let menu = cx.debug_bounds("server-context-menu").unwrap();
        assert!(menu.left() >= px(8.0) && menu.right() <= px(492.0));
        assert!(menu.top() >= px(8.0) && menu.bottom() <= px(192.0));
        let edit = cx.debug_bounds("server-context-menu-编辑").unwrap();
        cx.simulate_click(edit.center(), Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            assert!(matches!(app.page, Page::Servers));
            assert!(app.open_server_menu.is_none());
            assert_eq!(
                app.add_server_dialog
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .edit_server_id()
                    .as_deref(),
                Some("second")
            );
        });
    }

    #[gpui::test]
    fn loading_cards_block_context_menu_and_left_click_still_opens_the_server(
        cx: &mut TestAppContext,
    ) {
        let (app, cx) = servers_window(cx);
        cx.simulate_resize(size(px(800.0), px(500.0)));
        app.update(cx, |app, cx| {
            app.selecting_server_id = Some("first".into());
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        right_click(cx, first.center());
        assert!(cx.debug_bounds("server-context-menu").is_none());
        app.update(cx, |app, cx| {
            app.selecting_server_id = None;
            cx.notify();
        });
        cx.run_until_parked();
        // Click over the card's text/content, not just its empty background.
        cx.simulate_click(
            first.origin + point(px(100.0), px(30.0)),
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| matches!(app.page, Page::Home(_))));
    }
}
