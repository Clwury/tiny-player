use super::*;
use crate::{
    emby::{
        MediaItems, MediaSource, ResumeItems, SortOrder, UserItem, UserItemData, UserItemsQuery,
        UserItemsSort, UserViews, VideoItemType,
    },
    home::{gateway::HomeGateway, model::LoadState},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct PersonGateway {
    queries: Mutex<Vec<UserItemsQuery>>,
    favorites: Mutex<Vec<(String, bool)>>,
    fail_favorite: AtomicBool,
}

impl HomeGateway for PersonGateway {
    fn media_item(&self, id: &str) -> anyhow::Result<MediaItem> {
        Ok(serde_json::from_value(serde_json::json!({
            "Id": id, "Name": "Person", "Type": if id == "person" { "Person" } else { "Movie" },
            "UserData": {"IsFavorite": true}
        }))
        .unwrap())
    }
    fn persons(&self, _: &crate::emby::UserItemsQuery) -> anyhow::Result<crate::emby::UserItems> {
        Ok(crate::emby::UserItems {
            items: Vec::new(),
            total_record_count: 0,
        })
    }
    fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
        self.queries.lock().unwrap().push(query.clone());
        Ok(serde_json::from_value(serde_json::json!({
            "Items": [{"Id":"movie", "Name":"Movie", "Type":"Movie"}, {"Id":"series", "Name":"Series", "Type":"Series"}],
            "TotalRecordCount": 2
        })).unwrap())
    }
    fn set_favorite(&self, id: &str, favorite: bool) -> anyhow::Result<UserItemData> {
        self.favorites
            .lock()
            .unwrap()
            .push((id.to_string(), favorite));
        anyhow::ensure!(
            !self.fail_favorite.load(Ordering::SeqCst),
            "favorite failed"
        );
        Ok(serde_json::from_value(serde_json::json!({"IsFavorite": favorite})).unwrap())
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

#[gpui::test]
fn person_page_uses_injected_gateway_and_reuses_scroll_with_favorite_rollback(
    cx: &mut gpui::TestAppContext,
) {
    let content = crate::home::test_support::content(cx);
    let gateway = Arc::new(PersonGateway::default());
    let person: MediaPerson =
        serde_json::from_value(serde_json::json!({"Id":"person", "Name":"Person"})).unwrap();
    content.update(cx, |page, cx| {
        page.ports.browsing = gateway.clone();
        page.open_person_page(&person, cx);
    });
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        let vm = page.controller.person_view("person").unwrap();
        assert!(vm.loaded);
        assert_eq!(vm.items.sort_by, UserItemsSort::SortName);
        assert_eq!(vm.items.sort_order, SortOrder::Ascending);
        assert_eq!(vm.items.paged.initial, LoadState::Loaded);
        assert_eq!(vm.items.paged.items.len(), 2);
        assert_eq!(
            page.controller
                .user_item_by_id("series")
                .unwrap()
                .item_type
                .as_deref(),
            Some("Series")
        );
        page.toggle_person_favorite(cx);
    });
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        let vm = page.controller.person_view("person").unwrap();
        assert!(
            !page
                .controller
                .effective_user_data("person", vm.user_data)
                .unwrap()
                .is_favorite
        );
        gateway.fail_favorite.store(true, Ordering::SeqCst);
        page.toggle_person_favorite(cx);
    });
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        let vm = page.controller.person_view("person").unwrap();
        assert!(
            !page
                .controller
                .effective_user_data("person", vm.user_data)
                .unwrap()
                .is_favorite
        );
        assert!(page.has_visible_notifications());
        page.toggle_current_items_sort_menu(cx);
        assert!(
            page.person_resources["person"]
                .items
                .presentation
                .sort_menu_open
        );
        page.dispatch_items_source(
            &LibrarySource::Person("person".into()),
            LibraryIntent::SortBy(UserItemsSort::ProductionYear),
            cx,
        );
        assert!(
            !page.person_resources["person"]
                .items
                .presentation
                .sort_menu_open
        );
    });
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        page.dispatch_items_source(
            &LibrarySource::Person("person".into()),
            LibraryIntent::SortOrder(SortOrder::Descending),
            cx,
        );
    });
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        let offset = gpui::point(gpui::px(0.0), gpui::px(-400.0));
        page.person_resources["person"]
            .items
            .presentation
            .grid
            .scroll_handle
            .set_offset(offset);
        page.open_person_page(&person, cx);
        assert_eq!(
            page.person_resources["person"]
                .items
                .presentation
                .grid
                .scroll_handle
                .offset(),
            offset
        );
        assert_eq!(
            page.controller
                .person_view("person")
                .unwrap()
                .items
                .sort_order,
            SortOrder::Descending
        );
    });
    let queries = gateway.queries.lock().unwrap();
    assert_eq!(queries.len(), 3);
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::SortName));
    assert_eq!(queries[0].sort_order, SortOrder::Ascending);
    assert!(queries.iter().all(|query| query.person_ids == ["person"]
        && query.parent_id.is_none()
        && query.include_item_types == [VideoItemType::Movie, VideoItemType::Series]
        && query.recursive));
    assert_eq!(queries[2].sort_by, Some(UserItemsSort::ProductionYear));
    assert_eq!(queries[2].sort_order, SortOrder::Descending);
    assert_eq!(
        *gateway.favorites.lock().unwrap(),
        [("person".into(), false), ("person".into(), true)]
    );
}

#[gpui::test]
fn person_card_click_opens_filmography_and_controls_work_through_nested_back_navigation(
    cx: &mut gpui::TestAppContext,
) {
    use crate::{home::detail::test_fixture::DetailFixture, theme};
    use gpui::{Modifiers, VisualTestContext, point, px, size};
    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }
    cx.update(|cx| {
        theme::init(cx);
        cx.set_reduce_motion(true);
    });
    let gateway = Arc::new(PersonGateway::default());
    let browsing = gateway.clone();
    let (page, cx) = cx.add_window_view(move |_, cx| {
        let server = serde_json::from_value(serde_json::json!({
            "id":"local", "server_id":"remote", "user_id":"user",
            "endpoint":{"protocol":"Https", "address":"example.com", "port":443, "path":""},
            "username":"test", "password":"", "added_at_unix":0
        })).unwrap();
        let mut page = HomeContent::new(server, crate::emby::EmbyClient::new("test".into()).unwrap(), cx);
        page.ports.browsing = browsing;
        let item = serde_json::from_value(serde_json::json!({"Id":"source", "Name":"Source movie", "Type":"Movie"})).unwrap();
        let mut fixture = DetailFixture::new_movie(&item);
        fixture.controller.state.item = Some(serde_json::from_value(serde_json::json!({
            "Id":"source", "Name":"Source movie", "Type":"Movie", "People":[{"Id":"person", "Name":"Person", "Type":"Actor"}]
        })).unwrap());
        fixture.controller.state.effects.item = LoadState::Loaded;
        fixture.controller.state.effects.similar = LoadState::Loaded;
        page.controller.test_state_mut().navigation.push_detail_route_fixture("source".into(), None);
        page.install_detail_fixture(Some(fixture));
        page
    });
    cx.simulate_resize(size(px(1100.0), px(900.0)));
    cx.run_until_parked();
    let original_id = page.read_with(cx, |page, _| page.controller.detail_view().unwrap().id);
    let original_offset = page.read_with(cx, |page, _| {
        page.detail_view()
            .unwrap()
            .presentation
            .scroll_handle
            .offset()
    });
    click("series-detail-person-card-person-0", cx);
    page.read_with(cx, |page, _| {
        assert!(matches!(page.controller.route(), HomeRoute::Person { person_id, .. } if person_id == "person"));
        assert!(page.controller.detail_view().is_none());
        assert!(page.detail_resources.contains_key(&original_id));
        assert_eq!(page.controller.person_view("person").unwrap().items.paged.items.len(), 2);
    });
    let favorite = cx.debug_bounds("person-favorite-button").unwrap();
    let sort = cx.debug_bounds("library-sort-select").unwrap();
    assert!(favorite.left() > px(550.0));
    assert!(favorite.right() < sort.left());
    assert!(favorite.bottom() < px(64.0));
    click("person-favorite-button", cx);
    assert_eq!(
        *gateway.favorites.lock().unwrap(),
        [("person".into(), false)]
    );
    click("library-sort-select", cx);
    assert!(cx.debug_bounds("library-sort-menu").is_some());
    click("library-sort-option-3", cx);
    assert!(cx.debug_bounds("library-sort-menu").is_none());
    click("library-sort-select", cx);
    click("library-sort-order-option-1", cx);
    page.read_with(cx, |page, _| {
        let vm = page.controller.person_view("person").unwrap();
        assert_eq!(vm.items.sort_by, UserItemsSort::ProductionYear);
        assert_eq!(vm.items.sort_order, SortOrder::Descending);
    });
    click("person-grid-item-movie", cx);
    page.read_with(cx, |page, _| assert!(matches!(page.controller.route(), HomeRoute::Detail { root_item_id, .. } if root_item_id == "movie")));
    click("series-detail-back-button", cx);
    page.read_with(cx, |page, _| {
        assert!(matches!(page.controller.route(), HomeRoute::Person { .. }))
    });
    click("home-library-back-button", cx);
    page.read_with(cx, |page, _| {
        assert_eq!(page.controller.detail_view().unwrap().id, original_id);
        assert_eq!(
            page.detail_view()
                .unwrap()
                .presentation
                .scroll_handle
                .offset(),
            original_offset
        );
        assert_eq!(page.controller.title(), "Source movie");
    });
    // The source's restored resources are still interactive on the next visit.
    click("series-detail-person-card-person-0", cx);
    page.update(cx, |page, _| {
        page.person_resources["person"]
            .items
            .presentation
            .grid
            .scroll_handle
            .set_offset(point(px(0.0), px(0.0)));
    });
    assert_eq!(gateway.queries.lock().unwrap().len(), 3);
}

#[gpui::test]
fn shared_sort_synchronizes_open_servers_and_is_inherited_by_new_server_pages(
    cx: &mut gpui::TestAppContext,
) {
    use crate::{emby::UserView, media::ItemSortPreferences};
    use gpui::{AppContext as _, point, px};

    fn content_for_server(id: &str, cx: &mut gpui::TestAppContext) -> gpui::Entity<HomeContent> {
        let server = serde_json::from_value(serde_json::json!({
            "id": id, "server_id": id, "user_id": format!("{id}-user"),
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": id},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        cx.new(|cx| {
            HomeContent::new(
                server,
                crate::emby::EmbyClient::new("test".into()).unwrap(),
                cx,
            )
        })
    }
    let first = content_for_server("first", cx);
    let second = content_for_server("second", cx);
    let first_gateway = Arc::new(PersonGateway::default());
    let second_gateway = Arc::new(PersonGateway::default());
    let person =
        serde_json::from_value(serde_json::json!({"Id": "person", "Name": "Person"})).unwrap();
    let view: UserView = serde_json::from_value(serde_json::json!({
        "Id": "library", "Name": "Movies", "CollectionType": "movies"
    }))
    .unwrap();
    first.update(cx, |page, cx| {
        page.ports.browsing = first_gateway.clone();
        page.open_person_page(&person, cx);
    });
    second.update(cx, |page, cx| {
        page.ports.browsing = second_gateway.clone();
        page.open_library_for_view(&view, cx);
        page.library_resources[&view.id]
            .presentation
            .grid
            .scroll_handle
            .set_offset(point(px(0.0), px(-100.0)));
        page.toggle_current_items_sort_menu(cx);
    });
    cx.run_until_parked();
    first.update(cx, |page, cx| {
        page.dispatch_person_items(
            "person",
            LibraryIntent::SortBy(UserItemsSort::ProductionYear),
            cx,
        )
    });
    cx.run_until_parked();
    second.read_with(cx, |page, _| {
        let vm = page.controller.library_view("library").unwrap();
        assert_eq!(vm.sort_by, UserItemsSort::ProductionYear);
        assert_eq!(vm.sort_order, SortOrder::Ascending);
        assert!(
            !page.library_resources["library"]
                .presentation
                .sort_menu_open
        );
        assert_eq!(
            page.library_resources["library"]
                .presentation
                .grid
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0))
        );
    });
    second.update(cx, |page, cx| {
        page.select_library_sort_order("library".into(), SortOrder::Descending, cx)
    });
    cx.run_until_parked();
    first.read_with(cx, |page, _| {
        let vm = page.controller.person_view("person").unwrap();
        assert_eq!(vm.items.sort_by, UserItemsSort::ProductionYear);
        assert_eq!(vm.items.sort_order, SortOrder::Descending);
        for kind in crate::home::favorites::FAVORITE_ITEM_TYPES {
            let vm = page.controller.favorite_section(kind);
            assert_eq!(vm.sort_by, UserItemsSort::ProductionYear);
            assert_eq!(vm.sort_order, SortOrder::Descending);
        }
    });
    for gateway in [&first_gateway, &second_gateway] {
        let queries = gateway.queries.lock().unwrap();
        assert_eq!(queries.len(), 3);
        assert_eq!(queries[0].sort_by, Some(UserItemsSort::SortName));
        assert_eq!(queries[0].sort_order, SortOrder::Ascending);
        assert_eq!(queries[1].sort_by, Some(UserItemsSort::ProductionYear));
        assert_eq!(queries[1].sort_order, SortOrder::Ascending);
        assert_eq!(queries[2].sort_by, Some(UserItemsSort::ProductionYear));
        assert_eq!(queries[2].sort_order, SortOrder::Descending);
    }
    let third = content_for_server("third", cx);
    let third_gateway = Arc::new(PersonGateway::default());
    third.update(cx, |page, cx| {
        page.ports.browsing = third_gateway.clone();
        page.open_library_for_view(&view, cx);
    });
    cx.run_until_parked();
    let selected = ItemSortPreferences {
        sort_by: UserItemsSort::ProductionYear,
        sort_order: SortOrder::Descending,
    };
    third.read_with(cx, |page, _| {
        let vm = page.controller.library_view("library").unwrap();
        assert_eq!(vm.sort_by, selected.sort_by);
        assert_eq!(vm.sort_order, selected.sort_order);
    });
    let query = third_gateway.queries.lock().unwrap()[0].clone();
    assert_eq!(query.sort_by, Some(selected.sort_by));
    assert_eq!(query.sort_order, selected.sort_order);
    // A listener belonging to a previous route must not overwrite shared settings.
    first.update(cx, |page, cx| {
        page.select_library_sort_by("library".into(), UserItemsSort::Random, cx)
    });
    second.update(cx, |page, cx| {
        page.select_library_sort_order("library".into(), selected.sort_order, cx)
    });
    cx.run_until_parked();
    assert_eq!(cx.update(|cx| ItemSortPreferences::get(cx)), selected);
    assert_eq!(first_gateway.queries.lock().unwrap().len(), 3);
    assert_eq!(second_gateway.queries.lock().unwrap().len(), 3);
}
