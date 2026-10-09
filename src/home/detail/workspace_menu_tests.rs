use super::*;
use crate::home::navigation::HomeRoute;

fn workspace_menu_window<'a>(
    cx: &'a mut TestAppContext,
    server: &MockEmby,
    root: HomeRoot,
) -> (Entity<HomeContent>, &'a mut VisualTestContext) {
    let (page, cx) = home_menu_window(cx, server);
    page.update(cx, |page, cx| {
        page.controller
            .test_state_mut()
            .navigation
            .select_root(root);
        if root == HomeRoot::Favorites {
            page.enter_favorites_if_needed(cx);
        } else {
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
                .query = "test".into();
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
                .items = page
                .controller
                .test_state_mut()
                .feed
                .state
                .user_view_items_rows["videos"]
                .items
                .as_ref()
                .unwrap()
                .items
                .iter()
                .filter(|item| item.item_type.as_deref() != Some("Episode"))
                .cloned()
                .collect();
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
                .initial = LoadState::Loaded;
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
                .exhausted = true;
            page.controller
                .test_state_mut()
                .search
                .test_state_mut()
                .total_record_count = Some(2);
        }
        cx.notify();
    });
    cx.run_until_parked();
    (page, cx)
}

#[gpui::test]
fn favorite_row_menus_mark_movies_episodes_and_series_played_without_navigation(
    cx: &mut TestAppContext,
) {
    let server = MockEmby::with_favorites(true);
    let (page, cx) = workspace_menu_window(cx, &server, HomeRoot::Favorites);
    for (id, selector) in [
        ("movie-1", "favorite-row-item-movie-1"),
        ("episode-1", "favorite-row-item-episode-1"),
        ("series-1", "favorite-row-item-series-1"),
    ] {
        right_click(cx, selector);
        assert!(
            cx.debug_bounds("user-view-item-favorite-取消收藏")
                .is_some()
        );
        assert!(cx.debug_bounds("resume-item-hide-from-resume").is_none());
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.controller.route(),
                &HomeRoute::Root(HomeRoot::Favorites)
            );
            assert_eq!(page.item_context_menu.as_ref().unwrap().item_id, id);
        });
        click(cx, "user-view-item-mark-played");
        page.read_with(cx, |page, _| {
            assert!(page.controller.test_state().user_data.overrides[id].played);
            assert!(page.controller.test_state().user_data.overrides[id].is_favorite);
            assert_eq!(page.controller.test_state().favorites.items().count(), 4);
            assert!(!page.has_visible_notifications());
        });
    }
    page.read_with(cx, |page, _| {
        assert!(
            page.controller
                .test_state()
                .feed
                .state
                .resume_items
                .as_ref()
                .unwrap()
                .items
                .is_empty()
        )
    });
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/PlayedItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/episode-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn favorite_grid_menu_rolls_back_failed_removal_and_retargets_after_success(
    cx: &mut TestAppContext,
) {
    let server = MockEmby::with_favorites(true);
    let (page, cx) = workspace_menu_window(cx, &server, HomeRoot::Favorites);
    click(cx, "favorite-more-Episode");
    right_click(cx, "favorite-grid-item-episode-1");
    server.fail_next.store(true, Ordering::SeqCst);
    click(cx, "user-view-item-favorite");
    page.read_with(cx, |page, _| {
        let items = &page
            .controller
            .favorite_section(VideoItemType::Episode.into())
            .paged;
        assert_eq!(
            items
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["episode-1", "episode-2"]
        );
        assert_eq!(items.total_record_count, Some(2));
        assert!(
            page.controller
                .user_item_by_id("episode-1")
                .unwrap()
                .is_favorite()
        );
        assert!(page.has_visible_notifications());
    });
    for (id, selector, remaining) in [
        ("episode-1", "favorite-grid-item-episode-1", 1),
        ("episode-2", "favorite-grid-item-episode-2", 0),
    ] {
        right_click(cx, selector);
        assert!(
            cx.debug_bounds("user-view-item-favorite-取消收藏")
                .is_some()
        );
        click(cx, "user-view-item-favorite");
        assert!(cx.debug_bounds(selector).is_none());
        page.read_with(cx, |page, _| {
            assert!(!page.controller.test_state().user_data.overrides[id].is_favorite);
            let items = &page
                .controller
                .favorite_section(VideoItemType::Episode.into())
                .paged;
            assert_eq!(items.items.len(), remaining);
            assert_eq!(items.total_record_count, Some(remaining as u32));
            assert_eq!(
                page.controller
                    .favorite_overview_section(VideoItemType::Movie.into())
                    .paged
                    .items
                    .len(),
                1
            );
            assert!(!page.has_visible_notifications());
            assert!(page.item_context_menu.is_none());
        });
    }
    assert_eq!(
        server.mutations(),
        [
            "DELETE /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/episode-2 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn search_menus_toggle_favorites_and_mark_played_without_removing_results(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = workspace_menu_window(cx, &server, HomeRoot::Search);
    for (id, selector) in [
        ("movie-1", "search-grid-item-movie-1"),
        ("series-1", "search-grid-item-series-1"),
    ] {
        for (favorite, label) in [
            (true, "user-view-item-favorite-收藏"),
            (false, "user-view-item-favorite-取消收藏"),
        ] {
            right_click(cx, selector);
            assert!(cx.debug_bounds(label).is_some());
            click(cx, "user-view-item-favorite");
            page.read_with(cx, |page, _| {
                assert_eq!(
                    page.controller.test_state().user_data.overrides[id].is_favorite,
                    favorite
                );
                assert_eq!(
                    page.controller.test_state().search.test_state().items.len(),
                    2
                );
                assert_eq!(
                    page.controller
                        .test_state()
                        .search
                        .test_state()
                        .total_record_count,
                    Some(2)
                );
                assert_eq!(page.controller.route(), &HomeRoute::Root(HomeRoot::Search));
            });
        }
        right_click(cx, selector);
        click(cx, "user-view-item-mark-played");
        page.read_with(cx, |page, _| {
            assert!(page.controller.test_state().user_data.overrides[id].played);
            assert!(!page.controller.test_state().user_data.overrides[id].is_favorite);
            assert_eq!(
                page.controller.test_state().search.test_state().items.len(),
                2
            );
            assert!(!page.has_visible_notifications());
        });
    }
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/FavoriteItems/movie-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/FavoriteItems/series-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/series-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn search_menu_reports_failures_and_closes_when_the_query_changes(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = workspace_menu_window(cx, &server, HomeRoot::Search);
    for option in ["user-view-item-favorite", "user-view-item-mark-played"] {
        right_click(cx, "search-grid-item-movie-1");
        server.fail_next.store(true, Ordering::SeqCst);
        click(cx, option);
        page.update(cx, |page, _| {
            let data = page
                .controller
                .user_item_by_id("movie-1")
                .unwrap()
                .user_data
                .unwrap();
            assert!(!data.is_favorite && !data.played);
            assert!(page.has_visible_notifications());
            assert_eq!(
                page.controller.test_state().search.test_state().items.len(),
                2
            );
            page.clear_all_notifications();
        });
    }
    right_click(cx, "search-grid-item-movie-1");
    let input = page.read_with(cx, |page, _| page.search_input.clone());
    input.update(cx, |input, cx| input.set_value("new query", cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("user-view-item-context-menu").is_none());
    page.read_with(cx, |page, _| {
        assert!(page.item_context_menu.is_none());
        assert!(
            page.controller
                .test_state()
                .search
                .test_state()
                .items
                .is_empty()
        );
        assert_eq!(
            page.controller.test_state().search.test_state().query,
            "new query"
        );
    });
    assert_eq!(server.mutations().len(), 2);
}
