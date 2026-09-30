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
        page.controller.test_state_mut().navigation.select_root(HomeRoot::Search);
        page
    });
    cx.simulate_resize(size(px(900.0), px(600.0)));
    cx.run_until_parked();
    (page, cx)
}

#[gpui::test]
fn query_change_cancels_pending_delivery_before_it_can_notify(cx: &mut TestAppContext) {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    let (page, cx) = search_window(cx);
    let delivered = Arc::new(AtomicBool::new(false));
    let task_delivered = delivered.clone();
    page.update(cx, |page, cx| {
        page.dispatch_search(SearchIntent::InputChanged("old".into()), cx);
        let request = SearchRequestContext {
            user_data_revision: page.controller.user_data_request_revision(),
            request: page
                .controller
                .test_state_mut()
                .search
                .test_state_mut()
                .begin_initial()
                .unwrap(),
        };
        page.search_effect.replace(cx.spawn(async move |page, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            task_delivered.store(true, Ordering::SeqCst);
            page.update(cx, |page, cx| {
                page.finish_search_request(request, Err(anyhow::anyhow!("late error")), cx);
            })
            .ok();
        }));
    });
    cx.run_until_parked();
    let input = page.read_with(cx, |page, _| page.search_input.clone());
    input.update(cx, |input, cx| input.set_value("new", cx));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(!delivered.load(Ordering::SeqCst));
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.test_state().search.test_state().query,
            "new"
        );
        assert_eq!(
            page.controller.test_state().search.test_state().initial,
            LoadState::Idle
        );
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .initial_error
                .is_none()
        );
    });
}

#[gpui::test]
fn mismatched_workspace_cannot_commit_search_data_or_errors(cx: &mut TestAppContext) {
    let (page, cx) = search_window(cx);
    page.update(cx, |page, cx| {
        page.dispatch_search(SearchIntent::InputChanged("query".into()), cx);
        let request = page
            .controller
            .test_state_mut()
            .search
            .test_state_mut()
            .begin_initial()
            .unwrap();
        page.current_server.user_id = Some("another-user".into());
        let context = SearchRequestContext {
            request,
            user_data_revision: page.controller.user_data_request_revision(),
        };
        page.finish_search_request(
            context.clone(),
            Ok(SearchPage::from_response(
                serde_json::from_value(serde_json::json!({
                    "Items": [{"Id": "stale", "Name": "stale", "Type": "Movie"}],
                    "TotalRecordCount": 1,
                }))
                .unwrap(),
            )),
            cx,
        );
        page.finish_search_request(context, Err(anyhow::anyhow!("wrong account")), cx);
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .items
                .is_empty()
        );
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .initial_error
                .is_none()
        );
        assert_eq!(
            page.controller.test_state().search.test_state().initial,
            LoadState::Loading
        );
    });
}

#[gpui::test]
fn history_tag_fills_input_searches_and_clear_preserves_current_search(cx: &mut TestAppContext) {
    let (page, cx) = search_window(cx);
    let input = page.read_with(cx, |page, _| page.search_input.clone());
    input.update(cx, |input, cx| input.set_value("  沙丘  ", cx));
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| {
        page.controller
            .test_state()
            .search
            .test_state()
            .history
            .entries()
            .is_empty()
    }));
    input.update(cx, |_, cx| cx.emit(EditorEvent::Submitted));
    cx.run_until_parked();
    input.update(cx, |input, cx| input.set_value("星际穿越", cx));
    input.update(cx, |_, cx| cx.emit(EditorEvent::Submitted));
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.controller
                .test_state()
                .search
                .test_state()
                .history
                .entries(),
            ["星际穿越", "沙丘"]
        )
    });
    let token = page.read_with(cx, |page, _| {
        page.controller
            .test_state()
            .search
            .test_state()
            .request_token()
    });
    let tag = cx.debug_bounds("search-history-tag-1").unwrap();
    let editor = cx.debug_bounds("editor-input").unwrap();
    assert!(tag.top() >= editor.bottom());
    cx.simulate_click(tag.center(), Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, cx| {
        assert_eq!(page.search_input.read(cx).value().as_ref(), "沙丘");
        assert_eq!(
            page.controller.test_state().search.test_state().query,
            "沙丘"
        );
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .request_token()
                != token
        );
        // Invalid local endpoint fails immediately: Failed confirms submission
        // actually ran, rather than merely resetting the query to Idle.
        assert_eq!(
            page.controller.test_state().search.test_state().initial,
            LoadState::Failed
        );
        assert_eq!(
            page.controller
                .test_state()
                .search
                .test_state()
                .history
                .entries(),
            ["沙丘", "星际穿越"]
        );
    });
    let token = page.read_with(cx, |page, _| {
        page.controller
            .test_state()
            .search
            .test_state()
            .request_token()
    });
    let tag = cx.debug_bounds("search-history-tag-0").unwrap();
    cx.simulate_click(tag.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| {
        page.controller
            .test_state()
            .search
            .test_state()
            .request_token()
            != token
    }));
    let token = page.read_with(cx, |page, _| {
        page.controller
            .test_state()
            .search
            .test_state()
            .request_token()
    });
    let clear = cx.debug_bounds("search-history-clear").unwrap();
    cx.simulate_click(clear.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("search-history").is_none());
    page.read_with(cx, |page, cx| {
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .history
                .entries()
                .is_empty()
        );
        assert_eq!(
            page.controller.test_state().search.test_state().query,
            "沙丘"
        );
        assert_eq!(page.search_input.read(cx).value().as_ref(), "沙丘");
        assert_eq!(
            page.controller
                .test_state()
                .search
                .test_state()
                .request_token(),
            token
        );
    });
}

#[gpui::test]
fn thirty_long_history_tags_wrap_within_a_bounded_header(cx: &mut TestAppContext) {
    let (page, cx) = search_window(cx);
    page.update(cx, |page, cx| {
        for i in 0..35 {
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
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
