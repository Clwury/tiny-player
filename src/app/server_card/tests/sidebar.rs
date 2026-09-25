use super::*;

fn sidebar_window(cx: &mut TestAppContext) -> (Entity<TinyApp>, &mut VisualTestContext) {
    let mut cache = server_cache_with_ids(&["first", "second", "third"]);
    cache.auto_start_server_id = Some("first".into());
    let (app, cx) = servers_window_with_cache(cx, cache);
    cx.simulate_resize(size(px(1100.0), px(600.0)));
    cx.run_until_parked();
    (app, cx)
}

fn home_id(app: &Entity<TinyApp>, cx: &VisualTestContext) -> EntityId {
    app.read_with(cx, |app, _| match &app.page {
        Page::Home(home) => home.entity_id(),
        _ => panic!("sidebar actions must preserve the Home page"),
    })
}

fn assert_saved_order(app: &Entity<TinyApp>, cx: &VisualTestContext, expected: &[&str]) {
    app.read_with(cx, |app, _| {
        for servers in [&app.servers, &app.cache.servers] {
            assert_eq!(
                servers
                    .iter()
                    .map(|server| server.id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        assert!(app.selecting_server_id.is_none());
    });
}

#[gpui::test]
fn adding_from_sidebar_keeps_home_and_appends_to_both_server_lists(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let (app, cx) = sidebar_window(cx);
    app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
    let home = home_id(&app, cx);
    let third = cx.debug_bounds("sidebar-server-third").unwrap();
    let add = cx.debug_bounds("sidebar-add-server").unwrap();
    assert_eq!(add.top() - third.bottom(), px(4.0));
    cx.simulate_click(add.center(), Modifiers::default());
    cx.run_until_parked();
    let dialog = app.read_with(cx, |app, cx| {
        let dialog = app.add_server_dialog.clone().expect("shared add dialog");
        assert!(dialog.read(cx).edit_server_id().is_none());
        dialog
    });
    app.update(cx, |app, cx| {
        app.finish_save_server(dialog.clone(), Err(anyhow::anyhow!("test failure")), cx);
        assert!(app.add_server_dialog.is_some());
    });
    assert_eq!(home_id(&app, cx), home);
    assert_saved_order(&app, cx, &["first", "second", "third"]);

    let server = server_cache_with_ids(&["fourth"]).servers.remove(0);
    app.update(cx, |app, cx| app.finish_save_server(dialog, Ok(server), cx));
    cx.run_until_parked();
    assert_eq!(home_id(&app, cx), home);
    assert!(app.read_with(cx, |app, _| app.add_server_dialog.is_none()));
    assert_saved_order(&app, cx, &["first", "second", "third", "fourth"]);
    let fourth = cx.debug_bounds("sidebar-server-fourth").unwrap();
    assert_eq!(fourth.top() - third.bottom(), px(4.0));
    assert_eq!(
        cx.debug_bounds("sidebar-add-server").unwrap().top() - fourth.bottom(),
        px(4.0)
    );
    let saved = storage::load_or_init_from(&path).unwrap();
    assert_eq!(saved.servers.last().unwrap().id, "fourth");
    app.update(cx, |app, cx| app.show_servers_page_from_home(cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-card-fourth").is_some());
}

#[gpui::test]
fn sidebar_drag_previews_then_saves_order_without_switching_servers(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let (app, cx) = sidebar_window(cx);
    app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
    let home = home_id(&app, cx);
    let first = cx.debug_bounds("sidebar-server-first").unwrap();
    let third = cx.debug_bounds("sidebar-server-third").unwrap();
    begin_drag(cx, first.center());
    assert!(cx.update(|_, cx| cx.has_active_drag()));
    for offset in [0.0, 1.0, 2.0] {
        cx.simulate_mouse_move(
            third.center() + point(px(offset), px(0.0)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("sidebar-server-first").unwrap().origin,
            third.origin
        );
        assert_saved_order(&app, cx, &["first", "second", "third"]);
        assert_eq!(home_id(&app, cx), home);
    }
    assert!(!path.exists());
    // The drag must move the current cached record, including updates made mid-drag.
    app.update(cx, |app, _| {
        app.cache.servers[0].server_name = Some("Refreshed".into());
        app.cache.servers[0].access_token = Some("refreshed-token".into());
    });
    drop_at(cx, third.center());
    assert_saved_order(&app, cx, &["second", "third", "first"]);
    assert_eq!(home_id(&app, cx), home);
    assert_eq!(
        cx.debug_bounds("sidebar-server-second").unwrap().origin,
        first.origin
    );
    // Also move an inactive server upward without activating it.
    let third = cx.debug_bounds("sidebar-server-third").unwrap();
    begin_drag(cx, third.center());
    drop_at(cx, first.center());
    assert_saved_order(&app, cx, &["third", "second", "first"]);
    assert_eq!(home_id(&app, cx), home);
    cx.executor().advance_clock(Duration::from_millis(350));
    cx.run_until_parked();
    let saved = storage::load_or_init_from(&path).unwrap();
    assert_eq!(
        saved
            .servers
            .iter()
            .map(|server| server.id.as_str())
            .collect::<Vec<_>>(),
        ["third", "second", "first"]
    );
    assert_eq!(
        saved.servers[2].access_token.as_deref(),
        Some("refreshed-token")
    );
    assert_eq!(saved.auto_start_server_id.as_deref(), Some("first"));
    app.update(cx, |app, cx| app.show_servers_page_from_home(cx));
    cx.run_until_parked();
    assert_order(&app, cx, &["third", "second", "first"]);
}

#[gpui::test]
fn cancelling_sidebar_drag_or_dropping_on_add_does_not_save_or_open_dialog(
    cx: &mut TestAppContext,
) {
    let (app, cx) = sidebar_window(cx);
    let home = home_id(&app, cx);
    let first = cx.debug_bounds("sidebar-server-first").unwrap();
    let third = cx.debug_bounds("sidebar-server-third").unwrap();
    for cancel in ["escape", "outside", "add"] {
        begin_drag(cx, first.center());
        cx.simulate_mouse_move(
            third.center(),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("sidebar-server-first").unwrap().origin,
            third.origin
        );
        match cancel {
            "escape" => {
                cx.simulate_keystrokes("escape");
                cx.run_until_parked();
                drop_at(cx, third.center());
            }
            "add" => {
                let add = cx.debug_bounds("sidebar-add-server").unwrap();
                drop_at(cx, add.center());
            }
            _ => drop_at(cx, point(px(600.0), px(400.0))),
        }
        assert_saved_order(&app, cx, &["first", "second", "third"]);
        assert_eq!(home_id(&app, cx), home);
        assert_eq!(
            cx.debug_bounds("sidebar-server-first").unwrap().origin,
            first.origin
        );
        app.read_with(cx, |app, _| {
            assert!(app.pending_cache_save_error_prefix.is_none());
            assert!(app.add_server_dialog.is_none());
        });
    }
}

#[gpui::test]
fn sidebar_can_scroll_during_drag_and_drop_on_a_previously_hidden_server(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let ids = (0..20)
        .map(|index| format!("server-{index}"))
        .collect::<Vec<_>>();
    let mut cache = server_cache_with_ids(&ids.iter().map(String::as_str).collect::<Vec<_>>());
    cache.auto_start_server_id = Some(ids[0].clone());
    let (app, cx) = servers_window_with_cache(cx, cache);
    app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
    cx.simulate_resize(size(px(1100.0), px(500.0)));
    cx.run_until_parked();
    let home = home_id(&app, cx);
    let list = cx.debug_bounds("sidebar-servers").unwrap();
    let first = cx.debug_bounds("sidebar-server-server-0").unwrap();
    let add = cx.debug_bounds("sidebar-add-server").unwrap();
    assert!(cx.debug_bounds("sidebar-server-server-19").unwrap().top() > list.bottom());
    begin_drag(cx, first.center());
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: first.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-2000.0))),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
    assert!(cx.update(|_, cx| cx.has_active_drag()));
    assert_eq!(cx.debug_bounds("sidebar-add-server").unwrap(), add);
    let last = cx.debug_bounds("sidebar-server-server-19").unwrap();
    assert!(list.contains(&last.center()));
    drop_at(cx, last.center());
    assert_eq!(home_id(&app, cx), home);
    assert_eq!(cx.debug_bounds("sidebar-add-server").unwrap(), add);
    assert_eq!(
        cx.debug_bounds("sidebar-server-server-0").unwrap().origin,
        last.origin
    );
    let mut expected = ids.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    expected.push(&ids[0]);
    assert_saved_order(&app, cx, &expected);
    cx.executor().advance_clock(Duration::from_millis(350));
    cx.run_until_parked();
    assert_eq!(
        storage::load_or_init_from(&path)
            .unwrap()
            .servers
            .last()
            .unwrap()
            .id,
        ids[0]
    );
}
