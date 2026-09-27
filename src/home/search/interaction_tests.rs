use super::*;
use crate::{
    emby::EmbyClient, home::navigation::HomeRoot, server::CachedServer, theme, ui::editor::Editor,
};
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, size};

fn search_window(cx: &mut TestAppContext) -> (Entity<HomeContent>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init(cx);
        Editor::bind_keys(cx);
    });
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server: CachedServer = serde_json::from_value(serde_json::json!({
            "id": "search-test", "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
            "username": "test", "password": "", "user_id": "test", "access_token": "test", "added_at_unix": 0
        })).unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        page.navigation.select_root(HomeRoot::Search);
        page
    });
    cx.simulate_resize(size(px(900.0), px(600.0)));
    cx.run_until_parked();
    (page, cx)
}

#[gpui::test]
fn history_tag_fills_input_searches_and_clear_preserves_current_search(cx: &mut TestAppContext) {
    let (page, cx) = search_window(cx);
    let input = page.read_with(cx, |page, _| page.search_input.clone());
    input.update(cx, |input, cx| input.set_value("  沙丘  ", cx));
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.search.history.entries().is_empty()));
    input.update(cx, |_, cx| cx.emit(EditorEvent::Submitted));
    cx.run_until_parked();
    input.update(cx, |input, cx| input.set_value("星际穿越", cx));
    input.update(cx, |_, cx| cx.emit(EditorEvent::Submitted));
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(page.search.history.entries(), ["星际穿越", "沙丘"])
    });
    let generation = page.read_with(cx, |page, _| page.search.generation);
    let tag = cx.debug_bounds("search-history-tag-1").unwrap();
    let editor = cx.debug_bounds("editor-input").unwrap();
    assert!(tag.top() >= editor.bottom());
    cx.simulate_click(tag.center(), Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, cx| {
        assert_eq!(page.search_input.read(cx).value().as_ref(), "沙丘");
        assert_eq!(page.search.query, "沙丘");
        assert!(page.search.generation > generation);
        // Invalid local endpoint fails immediately: Failed confirms submission
        // actually ran, rather than merely resetting the query to Idle.
        assert_eq!(page.search.initial, LoadState::Failed);
        assert_eq!(page.search.history.entries(), ["沙丘", "星际穿越"]);
    });
    let generation = page.read_with(cx, |page, _| page.search.generation);
    let tag = cx.debug_bounds("search-history-tag-0").unwrap();
    cx.simulate_click(tag.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.search.generation > generation));
    let generation = page.read_with(cx, |page, _| page.search.generation);
    let clear = cx.debug_bounds("search-history-clear").unwrap();
    cx.simulate_click(clear.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("search-history").is_none());
    page.read_with(cx, |page, cx| {
        assert!(page.search.history.entries().is_empty());
        assert_eq!(page.search.query, "沙丘");
        assert_eq!(page.search_input.read(cx).value().as_ref(), "沙丘");
        assert_eq!(page.search.generation, generation);
    });
}

#[gpui::test]
fn thirty_long_history_tags_wrap_within_a_bounded_header(cx: &mut TestAppContext) {
    let (page, cx) = search_window(cx);
    page.update(cx, |page, cx| {
        for i in 0..35 {
            page.search
                .history
                .record(&format!("{i} {}", "很长的电影名称".repeat(12)));
        }
        cx.notify();
    });
    cx.run_until_parked();
    let tags = cx.debug_bounds("search-history-tags").unwrap();
    assert!(tags.size.height <= px(128.0));
    assert!(cx.debug_bounds("search-history-tag-29").is_some());
    assert!(cx.debug_bounds("search-history-tag-30").is_none());
    for selector in [
        "search-history-tag-0",
        "search-history-tag-1",
        "search-history-tag-29",
    ] {
        let tag = cx.debug_bounds(selector).unwrap();
        assert!(tag.size.width <= px(240.0));
        assert!(tag.left() >= tags.left());
        assert!(tag.right() <= tags.right());
    }
    assert!(
        cx.debug_bounds("home-search-fixed-header")
            .unwrap()
            .size
            .height
            < px(300.0)
    );
    let clear = cx.debug_bounds("search-history-clear").unwrap();
    cx.simulate_click(clear.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("search-history").is_none());
}
