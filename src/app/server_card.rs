use super::TinyApp;
use crate::server::view::SERVER_CARD_WIDTH_PX;
use crate::theme;
use gpui::{AppContext as _, EntityId, MouseButton, Pixels, Point, point, px};
use std::time::Duration;

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
            assert!(matches!(app.shell.page(), Page::Servers));
            assert!(app.server_feature.selecting_server_id().is_none());
            let snapshot = app.cache_snapshot();
            for servers in [&app.server_feature.catalog().servers, &snapshot.servers] {
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
                assert!(matches!(app.shell.page(), Page::Servers));
                assert!(app.open_server_menu.is_none());
                assert_eq!(
                    app.server_feature.catalog().auto_start_server_id.as_deref(),
                    expected
                );
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
        let home_id = app.read_with(cx, |app, _| match app.shell.page() {
            Page::Home(home) => home.entity_id(),
            _ => panic!("auto start must open the saved server"),
        });
        // Clicking the already active sidebar server keeps the same Home page.
        let current = cx.debug_bounds("sidebar-server-second").unwrap();
        cx.simulate_click(current.center(), Modifiers::default());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            assert!(matches!(app.shell.page(), Page::Home(home) if home.entity_id() == home_id));
            app.show_servers_page_from_home(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(app.shell.page(), Page::Servers));
            assert_eq!(
                app.server_feature.catalog().auto_start_server_id.as_deref(),
                Some("second")
            );
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
                assert!(matches!(app.shell.page(), Page::Servers));
                assert!(app.server_feature.selecting_server_id().is_none());
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
            assert!(matches!(app.shell.page(), Page::Servers));
            assert!(app.server_feature.selecting_server_id().is_none());
            assert!(app.has_server_page_notifications());
            assert_eq!(
                app.server_feature.catalog().auto_start_server_id.as_deref(),
                Some("first")
            );
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
            app.server_feature.test_catalog_mut().servers[0].server_name =
                Some("Refreshed first".into());
            app.server_feature.test_catalog_mut().servers[0].access_token =
                Some("refreshed-token".into());

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
            app.read_with(cx, |app, _| app.server_feature.menu().unwrap().server_id),
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
        assert!(app.read_with(cx, |app, _| {
            app.persistence.pending_settings_error_prefix().is_none()
        }));

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
            assert!(app.read_with(cx, |app, _| {
                app.persistence.pending_settings_error_prefix().is_none()
            }));
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
            assert!(app.read_with(cx, |app, _| {
                app.persistence.pending_settings_error_prefix().is_none()
            }));
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
            let server = app
                .server_feature
                .catalog()
                .servers
                .iter()
                .find(|server| server.id == "first")
                .unwrap()
                .clone();
            app.server_feature.begin_authentication(server);
            app.reorder_server("first", "second", cx);
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        let second = cx.debug_bounds("server-card-second").unwrap();
        begin_drag(cx, first.center());
        assert!(!cx.update(|_, cx| cx.has_active_drag()));
        drop_at(cx, second.center());
        app.update(cx, |app, _| app.server_feature.cancel_selection());
        assert_order(&app, cx, &["first", "second"]);
        assert!(app.read_with(cx, |app, _| {
            app.persistence.pending_settings_error_prefix().is_none()
        }));
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
                assert!(matches!(app.shell.page(), Page::Servers));
                assert!(app.server_feature.selecting_server_id().is_none());
                let menu = app.open_server_menu.as_ref().unwrap();
                assert_eq!(
                    app.server_feature.menu().unwrap().server_id,
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
            assert!(matches!(app.shell.page(), Page::Servers));
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
            let server = app
                .server_feature
                .catalog()
                .servers
                .iter()
                .find(|server| server.id == "first")
                .unwrap()
                .clone();
            app.server_feature.begin_authentication(server);
            cx.notify();
        });
        cx.run_until_parked();
        let first = cx.debug_bounds("server-card-first").unwrap();
        right_click(cx, first.center());
        assert!(cx.debug_bounds("server-context-menu").is_none());
        app.update(cx, |app, cx| {
            app.server_feature.cancel_selection();
            cx.notify();
        });
        cx.run_until_parked();
        // Click over the card's text/content, not just its empty background.
        cx.simulate_click(
            first.origin + point(px(100.0), px(30.0)),
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| matches!(app.shell.page(), Page::Home(_))));
    }
}
