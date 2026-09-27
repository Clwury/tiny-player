use super::*;

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let position = cx.debug_bounds(selector).unwrap().center();
    cx.simulate_click(position, Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn search_history_persists_across_server_switches_reopening_and_clear(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    cx.update(crate::ui::editor::Editor::bind_keys);
    let (app, cx) = sidebar_window(cx);
    app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
    click(cx, "search-section");
    cx.simulate_input("沙丘");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    app.update(cx, |app, cx| {
        assert_eq!(app.cache.search_history.entries(), ["沙丘"]);
        app.flush_scheduled_cache_save(cx);
    });
    assert_eq!(
        crate::storage::load_or_init_from(&path)
            .unwrap()
            .search_history
            .entries(),
        ["沙丘"]
    );

    click(cx, "sidebar-server-second");
    click(cx, "search-section");
    assert!(cx.debug_bounds("search-history-tag-0").is_some());
    cx.simulate_input("星际穿越");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    app.update(cx, |app, cx| {
        assert_eq!(app.cache.search_history.entries(), ["星际穿越", "沙丘"]);
        app.flush_scheduled_cache_save(cx);
        app.cache = crate::storage::load_or_init_from(&path).unwrap();
        app.open_home_for_server(app.servers[0].clone(), cx);
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, "search-section");
    assert!(cx.debug_bounds("search-history-tag-1").is_some());
    click(cx, "search-history-tag-1");
    app.read_with(cx, |app, _| {
        assert_eq!(app.cache.search_history.entries(), ["沙丘", "星际穿越"])
    });
    click(cx, "search-history-clear");
    app.update(cx, |app, cx| app.flush_scheduled_cache_save(cx));
    assert!(
        crate::storage::load_or_init_from(&path)
            .unwrap()
            .search_history
            .entries()
            .is_empty()
    );
    click(cx, "sidebar-server-second");
    click(cx, "search-section");
    assert!(cx.debug_bounds("search-history").is_none());
}
