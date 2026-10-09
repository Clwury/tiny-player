use super::*;
use crate::{
    emby::{
        MediaItem, MediaItems, MediaSource, ResumeItems, SortOrder, UserItem, UserItemData,
        UserItemsQuery, UserItemsSort, UserViews, VideoItemType,
    },
    home::{
        detail::test_fixture::DetailFixture,
        gateway::HomeGateway,
        model::{LoadState, navigation::HomeRoute},
    },
    media::ItemSortPreferences,
    theme,
};
use gpui::{Modifiers, VisualTestContext, point, px, size};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct GenreGateway {
    queries: Mutex<Vec<UserItemsQuery>>,
}

impl HomeGateway for GenreGateway {
    fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
        self.queries.lock().unwrap().push(query.clone());
        Ok(serde_json::from_value(json!({
            "Items": (query.start_index..query.start_index + query.limit).map(|index| json!({
                "Id": format!("entry-{index}"), "Name": format!("Result {index}"), "Type": if index % 2 == 0 { "Movie" } else { "Series" }
            })).collect::<Vec<_>>(), "TotalRecordCount": 120
        })).unwrap())
    }
    fn media_item(&self, id: &str) -> anyhow::Result<MediaItem> {
        Ok(serde_json::from_value(json!({"Id": id, "Name": "Movie", "Type": "Movie"})).unwrap())
    }
    fn similar_items(&self, _: &str) -> anyhow::Result<UserItems> {
        Ok(UserItems {
            items: vec![],
            total_record_count: 0,
        })
    }
    fn playback_media_sources(&self, _: &str) -> anyhow::Result<Vec<MediaSource>> {
        Ok(vec![])
    }
    fn persons(&self, _: &UserItemsQuery) -> anyhow::Result<UserItems> {
        panic!("unexpected persons")
    }
    fn show_seasons(&self, _: &str) -> anyhow::Result<MediaItems> {
        panic!("unexpected seasons")
    }
    fn show_next_up(&self, _: &str) -> anyhow::Result<MediaItems> {
        panic!("unexpected next up")
    }
    fn show_episodes(&self, _: &str, _: Option<&str>) -> anyhow::Result<MediaItems> {
        panic!("unexpected episodes")
    }
    fn set_played(&self, _: &str, _: bool) -> anyhow::Result<UserItemData> {
        panic!("unexpected played")
    }
    fn mark_item_played(&self, _: &str) -> anyhow::Result<UserItemData> {
        panic!("unexpected mark played")
    }
    fn hide_item_from_resume(&self, _: &str) -> anyhow::Result<()> {
        panic!("unexpected hide")
    }
    fn set_favorite(&self, _: &str, _: bool) -> anyhow::Result<UserItemData> {
        panic!("unexpected favorite")
    }
    fn user_views(&self) -> anyhow::Result<UserViews> {
        panic!("unexpected views")
    }
    fn resume_items(&self) -> anyhow::Result<ResumeItems> {
        panic!("unexpected resume")
    }
    fn latest_items(&self, _: &str, _: &[VideoItemType], _: u32) -> anyhow::Result<Vec<UserItem>> {
        panic!("unexpected latest")
    }
    fn search_items(&self, _: &str, _: u32, _: u32) -> anyhow::Result<UserItems> {
        panic!("unexpected search")
    }
}

fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn separate_genre_tags_open_sorted_paged_feeds_and_preserve_nested_detail_navigation(
    cx: &mut gpui::TestAppContext,
) {
    cx.update(|cx| {
        theme::init(cx);
        cx.set_reduce_motion(true);
        ItemSortPreferences {
            sort_by: UserItemsSort::PremiereDate,
            sort_order: SortOrder::Descending,
        }
        .apply(cx);
    });
    let gateway = Arc::new(GenreGateway::default());
    let browsing = gateway.clone();
    let (page, cx) = cx.add_window_view(move |_, cx| {
        let server = serde_json::from_value(json!({
            "id": "local", "server_id": "remote", "user_id": "user",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        let mut page = HomeContent::new(
            server,
            crate::emby::EmbyClient::new("test".into()).unwrap(),
            cx,
        );
        page.ports.browsing = browsing;
        let item = serde_json::from_value(
            json!({"Id": "source", "Name": "Source movie", "Type": "Movie"}),
        )
        .unwrap();
        let mut fixture = DetailFixture::new_movie(&item);
        fixture.controller.state.item = Some(
            serde_json::from_value(json!({
                "Id": "source", "Name": "Source movie", "Type": "Movie",
                "Genres": ["Drama", "动作", "Drama", ""],
                "GenreItems": [{"Name": "Drama", "Id": "g18"}, {"Name": "动作", "Id": 19}]
            }))
            .unwrap(),
        );
        fixture.controller.state.effects.item = LoadState::Loaded;
        fixture.controller.state.effects.similar = LoadState::Loaded;
        page.controller
            .test_state_mut()
            .navigation
            .push_detail_route_fixture("source".into(), None);
        page.install_detail_fixture(Some(fixture));
        page
    });
    cx.simulate_resize(size(px(1100.0), px(900.0)));
    cx.run_until_parked();
    let first_tag = cx.debug_bounds("series-detail-genre-tag-0").unwrap();
    let second_tag = cx.debug_bounds("series-detail-genre-tag-1").unwrap();
    assert!(first_tag.right() < second_tag.left());
    assert!(cx.debug_bounds("series-detail-genre-tag-2").is_none());
    let source_id = page.read_with(cx, |page, _| page.controller.detail_view().unwrap().id);
    let source_offset = page.read_with(cx, |page, _| {
        page.detail_view()
            .unwrap()
            .presentation
            .scroll_handle
            .offset()
    });
    click("series-detail-genre-tag-0", cx);
    page.read_with(cx, |page, _| {
        assert!(matches!(page.controller.route(), HomeRoute::Genre { genre_key, .. } if genre_key == "id:g18"));
        let vm = page.controller.genre_view("id:g18").unwrap();
        assert_eq!(vm.paged.items.len(), 60);
        assert_eq!(vm.sort_by, UserItemsSort::PremiereDate);
        assert_eq!(vm.sort_order, SortOrder::Descending);
        assert!(page.controller.detail_view().is_none());
        assert!(page.detail_resources.contains_key(&source_id));
    });
    let sort_bounds = cx.debug_bounds("library-sort-select").unwrap();
    assert!(sort_bounds.left() > px(550.0));
    assert!(sort_bounds.bottom() < px(64.0));
    page.update(cx, |page, cx| {
        page.genre_resources["id:g18"]
            .presentation
            .grid
            .scroll_handle
            .set_offset(point(px(0.0), px(-300.0)));
        cx.notify();
    });
    cx.run_until_parked();
    click("library-sort-select", cx);
    assert!(cx.debug_bounds("library-sort-option-10").is_some());
    click("library-sort-option-7", cx);
    page.read_with(cx, |page, cx| {
        assert_eq!(
            ItemSortPreferences::get(cx).sort_by,
            UserItemsSort::DateLastContentAdded
        );
        assert_eq!(
            page.controller.genre_view("id:g18").unwrap().sort_by,
            UserItemsSort::DateLastContentAdded
        );
        assert_eq!(
            page.genre_resources["id:g18"]
                .presentation
                .grid
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0))
        );
    });
    click("library-sort-select", cx);
    click("library-sort-order-option-0", cx);
    page.update(cx, |page, cx| {
        page.dispatch_items_source(
            &LibrarySource::Genre("id:g18".into()),
            LibraryIntent::LoadMore { automatic: false },
            cx,
        )
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.controller
                .genre_view("id:g18")
                .unwrap()
                .paged
                .items
                .len(),
            120
        )
    });
    click("genre-grid-item-entry-0", cx);
    page.read_with(cx, |page, _| assert!(matches!(page.controller.route(), HomeRoute::Detail { root_item_id, .. } if root_item_id == "entry-0")));
    click("series-detail-back-button", cx);
    page.read_with(cx, |page, _| {
        assert!(matches!(page.controller.route(), HomeRoute::Genre { .. }))
    });
    click("home-library-back-button", cx);
    page.read_with(cx, |page, _| {
        assert_eq!(page.controller.detail_view().unwrap().id, source_id);
        assert_eq!(
            page.detail_view()
                .unwrap()
                .presentation
                .scroll_handle
                .offset(),
            source_offset
        );
    });
    click("series-detail-genre-tag-1", cx);
    page.read_with(cx, |page, _| {
        let vm = page.controller.genre_view("id:19").unwrap();
        assert_eq!(vm.sort_by, UserItemsSort::DateLastContentAdded);
        assert_eq!(vm.sort_order, SortOrder::Ascending);
    });
    click("home-library-back-button", cx);
    click("series-detail-genre-tag-0", cx);
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.genre_view("id:g18").unwrap().sort_by,
            UserItemsSort::DateLastContentAdded
        )
    });
    let queries = gateway.queries.lock().unwrap();
    assert_eq!(queries.len(), 5);
    for query in queries.iter() {
        assert!(
            query.genres.is_empty() && query.person_ids.is_empty() && query.parent_id.is_none()
        );
        assert_eq!(
            query.include_item_types,
            [VideoItemType::Movie, VideoItemType::Series]
        );
        assert!(query.recursive && query.group_programs_by_series);
        assert_eq!(query.collapse_box_set_items, Some(false));
        assert_eq!(query.limit, 60);
    }
    assert!(queries[..4].iter().all(|query| query.genre_ids == ["g18"]));
    assert_eq!(queries[4].genre_ids, ["19"]);
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::PremiereDate));
    assert_eq!(queries[0].sort_order, SortOrder::Descending);
    assert_eq!(
        queries[1].sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert_eq!(queries[1].sort_order, SortOrder::Descending);
    assert_eq!(queries[2].sort_order, SortOrder::Ascending);
    assert!(
        queries[2..]
            .iter()
            .all(|query| query.sort_by == Some(UserItemsSort::DateLastContentAdded))
    );
    assert_eq!(queries[3].start_index, 60);
}
