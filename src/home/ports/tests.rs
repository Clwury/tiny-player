//! Page-level regression for composition-root gateway injection.
use super::*;
use crate::{
    emby::{UserItems, UserItemsQuery, UserItemsSort, VideoItemType},
    home::{HomeContent, LoadState},
};
use gpui::AppContext as _;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct BrowsingGateway {
    views: AtomicUsize,
    queries: Mutex<Vec<UserItemsQuery>>,
}

impl HomeGateway for BrowsingGateway {
    fn similar_items(&self, _: &str) -> anyhow::Result<crate::emby::UserItems> {
        panic!("unexpected similar request")
    }
    fn show_seasons(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected seasons request")
    }
    fn show_next_up(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected next-up request")
    }
    fn playback_media_sources(&self, _: &str) -> anyhow::Result<Vec<crate::emby::MediaSource>> {
        panic!("unexpected sources request")
    }

    fn set_played(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected played mutation")
    }
    fn media_item(&self, _: &str) -> anyhow::Result<crate::emby::MediaItem> {
        panic!("unexpected media item request")
    }
    fn show_episodes(&self, _: &str, _: Option<&str>) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected episodes request")
    }

    fn mark_item_played(&self, _: &str) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected resume mutation")
    }
    fn hide_item_from_resume(&self, _: &str) -> anyhow::Result<()> {
        panic!("unexpected resume mutation")
    }

    fn set_favorite(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected favorite mutation")
    }
    fn user_views(&self) -> anyhow::Result<crate::emby::UserViews> {
        self.views.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::from_value(serde_json::json!({"Items":[], "TotalRecordCount":0})).unwrap())
    }
    fn resume_items(&self) -> anyhow::Result<crate::emby::ResumeItems> {
        panic!("unexpected endpoint")
    }
    fn latest_items(
        &self,
        _: &str,
        _: &[VideoItemType],
        _: u32,
    ) -> anyhow::Result<Vec<crate::emby::UserItem>> {
        panic!("unexpected endpoint")
    }
    fn search_items(&self, _: &str, _: u32, _: u32) -> anyhow::Result<UserItems> {
        panic!("library must use the user items endpoint")
    }
    fn persons(&self, _: &crate::emby::UserItemsQuery) -> anyhow::Result<crate::emby::UserItems> {
        Ok(crate::emby::UserItems {
            items: Vec::new(),
            total_record_count: 0,
        })
    }
    fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
        self.queries.lock().unwrap().push(query.clone());
        anyhow::ensure!(
            query.sort_by == Some(UserItemsSort::SortName),
            "injected sort failure"
        );
        Ok(serde_json::from_value(serde_json::json!({"Items":[{"Id":"injected", "Name":"Injected movie", "Type":"Movie"}], "TotalRecordCount":1})).unwrap())
    }
}
#[gpui::test]
fn feed_and_library_use_injected_browsing_for_success_and_failure(cx: &mut gpui::TestAppContext) {
    let server = serde_json::from_value(serde_json::json!({
        "id":"injected", "server_id":"remote", "user_id":"user",
        "endpoint":{"protocol":"Https", "address":"example.invalid", "port":443, "path":""},
        "username":"test", "password":"", "added_at_unix":0
    }))
    .unwrap();
    let client = crate::emby::EmbyClient::new("test".into()).unwrap();
    let playback = crate::home::test_support::ports(&server, &client).playback;
    let browsing = Arc::new(BrowsingGateway::default());
    let content = cx.new(|cx| {
        HomeContent::with_ports(
            server,
            client,
            HomePorts::new(browsing.clone(), playback),
            cx,
        )
    });
    content.update(cx, |page, cx| page.load_user_views_if_needed(cx));
    cx.run_until_parked();
    assert_eq!(browsing.views.load(Ordering::SeqCst), 1);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.test_state().feed.state.views_load,
            LoadState::Loaded
        );
        assert!(!page.has_visible_notifications());
    });
    let view = serde_json::from_value(
        serde_json::json!({"Id":"library", "Name":"Movies", "CollectionType":"movies"}),
    )
    .unwrap();
    content.update(cx, |page, cx| page.open_library_for_view(&view, cx));
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        let vm = page.controller.library_view("library").unwrap();
        assert_eq!(vm.paged.initial, LoadState::Loaded);
        assert_eq!(vm.paged.items[0].id, "injected");
        assert!(!page.has_visible_notifications());
    });
    content.update(cx, |page, cx| {
        page.select_library_sort_by("library".into(), UserItemsSort::CriticRating, cx)
    });
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller
                .library_view("library")
                .unwrap()
                .paged
                .initial,
            LoadState::Failed
        );
        assert!(page.has_visible_notifications());
    });
    let queries = browsing.queries.lock().unwrap();
    assert_eq!(queries.len(), 2);
    assert!(
        queries
            .iter()
            .all(|query| query.parent_id.as_deref() == Some("library"))
    );
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::SortName));
    assert_eq!(queries[1].sort_by, Some(UserItemsSort::CriticRating));
}
