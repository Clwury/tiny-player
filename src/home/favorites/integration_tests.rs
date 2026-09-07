use gpui::{
    AppContext as _, Context, Entity, IntoElement, Modifiers, ParentElement, Render, Styled,
    TestAppContext, VisualTestContext, Window, div, point, px, size,
};

use super::super::carousel::HOME_SIDEBAR_WIDTH_PX;
use super::*;
use crate::{emby::EmbyClient, server::CachedServer, theme};

struct FavoritesWindow {
    content: Entity<HomeContent>,
}

impl Render for FavoritesWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().relative().size_full().child(
            div()
                .absolute()
                .left(px(HOME_SIDEBAR_WIDTH_PX))
                .top_0()
                .bottom_0()
                .right_0()
                .child(self.content.clone()),
        )
    }
}

fn item(item_type: VideoItemType, index: usize) -> UserItem {
    serde_json::from_value(serde_json::json!({
        "Id": format!("{}-{index}", item_type.as_str()), "Name": format!("Item {index}"),
        "Type": item_type.as_str(), "SeriesId": "series-parent", "SeriesName": "Series",
        "ParentIndexNumber": 1, "IndexNumber": index + 1, "ProductionYear": 2024,
        "UserData": { "IsFavorite": true }
    }))
    .unwrap()
}

fn favorites_window(cx: &mut TestAppContext) -> (Entity<HomeContent>, &mut VisualTestContext) {
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| {
        let server: CachedServer = serde_json::from_value(serde_json::json!({
            "id": "favorites-test", "endpoint": { "protocol": "Https", "address": "example.com", "port": 443, "path": "" },
            "username": "test", "password": "", "user_id": "test", "added_at_unix": 0
        })).unwrap();
        let content = cx.new(|cx| {
            let mut content = HomeContent::new(server, EmbyClient::new("favorites-test".into()).unwrap(), cx);
            content.navigation.select_root(HomeRoot::Favorites);
            for item_type in FAVORITE_ITEM_TYPES {
                let state = &mut content.favorites[item_type].paged;
                state.items = (0..30).map(|index| item(item_type, index)).collect();
                state.initial = LoadState::Loaded;
                state.total_record_count = Some(30);
                state.next_start_index = 30;
                state.exhausted = true;
            }
            content
        });
        FavoritesWindow { content }
    });
    let content = root.read_with(cx, |root, _| root.content.clone());
    cx.simulate_resize(size(px(1100.0), px(1000.0)));
    cx.run_until_parked();
    (content, cx)
}

#[gpui::test]
fn favorites_shows_three_rows_and_more_preserves_category_scroll(cx: &mut TestAppContext) {
    let (content, cx) = favorites_window(cx);
    let movie = cx.debug_bounds("favorite-row-Movie").unwrap();
    let series = cx.debug_bounds("favorite-row-Series").unwrap();
    let episode = cx.debug_bounds("favorite-row-Episode").unwrap();
    assert!(movie.bottom() < series.top());
    assert!(series.bottom() < episode.top());
    assert_eq!(movie.left(), series.left());
    assert_eq!(movie.right(), series.right());
    assert_eq!(series.right(), episode.right());
    assert!(episode.size.height < movie.size.height);
    assert!(
        episode.size.height <= px(186.0),
        "episode cards must fit the paged grid row: {episode:?}"
    );

    let more = cx.debug_bounds("favorite-more-Series").unwrap();
    cx.simulate_click(more.center(), Modifiers::default());
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        assert_eq!(
            page.navigation.current(),
            &HomeRoute::FavoriteItems {
                item_type: VideoItemType::Series
            }
        );
        page.favorites[VideoItemType::Series]
            .paged
            .scroll_handle
            .set_offset(point(px(0.0), px(-320.0)));
        cx.notify();
    });
    cx.run_until_parked();
    let saved_offset = content.read_with(cx, |page, _| {
        page.favorites[VideoItemType::Series]
            .paged
            .scroll_handle
            .offset()
    });
    let back = cx.debug_bounds("home-library-back-button").unwrap();
    cx.simulate_click(back.center(), Modifiers::default());
    cx.run_until_parked();
    content.update(cx, |page, cx| {
        assert_eq!(
            page.navigation.current(),
            &HomeRoute::Root(HomeRoot::Favorites)
        );
        page.open_favorite_items(VideoItemType::Series, cx);
        page.navigation.push_detail("Series-5".into(), None);
        assert!(page.navigation.pop());
        assert_eq!(
            page.navigation.current(),
            &HomeRoute::FavoriteItems {
                item_type: VideoItemType::Series
            }
        );
        assert!(page.navigation.pop());
        page.open_favorite_items(VideoItemType::Movie, cx);
        assert_eq!(
            page.favorites[VideoItemType::Movie]
                .paged
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0))
        );
        assert!(page.navigation.pop());
        page.open_favorite_items(VideoItemType::Series, cx);
    });
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.favorites[VideoItemType::Series]
                .paged
                .scroll_handle
                .offset(),
            saved_offset
        );
    });
}

#[gpui::test]
fn empty_favorite_sections_leave_no_gaps_during_loading_or_after_empty_responses(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let first_row_top = cx.debug_bounds("favorite-row-Movie").unwrap().top();
    content.update(cx, |page, cx| {
        page.favorites[VideoItemType::Movie].paged.items.clear();
        page.favorites[VideoItemType::Movie].paged.initial = LoadState::Loading;
        page.favorites[VideoItemType::Episode].paged.items.clear();
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("favorite-section-Movie").is_none());
    assert!(cx.debug_bounds("favorite-section-Episode").is_none());
    assert_eq!(
        cx.debug_bounds("favorite-row-Series").unwrap().top(),
        first_row_top
    );
    assert!(cx.debug_bounds("favorite-more-Series").is_some());

    for initial in [LoadState::Idle, LoadState::Loading, LoadState::Loaded] {
        content.update(cx, |page, cx| {
            for item_type in FAVORITE_ITEM_TYPES {
                let state = &mut page.favorites[item_type].paged;
                state.items.clear();
                state.initial = initial;
                state.total_record_count = Some(0);
            }
            cx.notify();
        });
        cx.run_until_parked();
        for selector in [
            "favorite-section-Movie",
            "favorite-section-Series",
            "favorite-section-Episode",
        ] {
            assert!(cx.debug_bounds(selector).is_none());
        }
    }

    content.update(cx, |page, cx| {
        let state = &mut page.favorites[VideoItemType::Episode].paged;
        state.items.push(item(VideoItemType::Episode, 0));
        state.total_record_count = Some(1);
        page.favorites[VideoItemType::Movie].paged.initial = LoadState::Failed;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("favorite-row-Episode").is_some());
    assert!(cx.debug_bounds("favorite-more-Episode").is_some());
    assert!(cx.debug_bounds("favorite-retry-Movie").is_some());
    assert!(cx.debug_bounds("favorite-section-Series").is_none());
}

#[gpui::test]
fn empty_favorite_more_page_hides_its_title_and_keeps_back_navigation(cx: &mut TestAppContext) {
    let (content, cx) = favorites_window(cx);
    content.update(cx, |page, cx| {
        page.open_favorite_items(VideoItemType::Series, cx)
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("favorite-items-title").is_some());
    for initial in [LoadState::Loading, LoadState::Loaded] {
        content.update(cx, |page, cx| {
            let state = &mut page.favorites[VideoItemType::Series].paged;
            state.items.clear();
            state.initial = initial;
            state.total_record_count = Some(0);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("favorite-items-title").is_none());
        assert!(cx.debug_bounds("home-library-back-button").is_some());
    }
    let back = cx.debug_bounds("home-library-back-button").unwrap();
    cx.simulate_click(back.center(), Modifiers::default());
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.navigation.current(),
            &HomeRoute::Root(HomeRoot::Favorites)
        );
    });
    assert!(cx.debug_bounds("favorite-section-Series").is_none());
    assert!(cx.debug_bounds("favorite-row-Movie").is_some());
}

#[gpui::test]
fn favorite_episode_cards_keep_detail_dimensions_in_rows_and_responsive_grids(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let first = cx.debug_bounds("favorite-row-item-Episode-0").unwrap();
    let second = cx.debug_bounds("favorite-row-item-Episode-1").unwrap();
    assert_eq!(first.size.width, px(228.0));
    assert_eq!(second.left() - first.right(), px(16.0));
    content.update(cx, |page, cx| {
        page.open_favorite_items(VideoItemType::Episode, cx)
    });
    cx.run_until_parked();

    let card_selectors = [
        "favorite-grid-item-Episode-0",
        "favorite-grid-item-Episode-1",
        "favorite-grid-item-Episode-2",
        "favorite-grid-item-Episode-3",
        "favorite-grid-item-Episode-4",
    ];
    for (width, columns) in [
        (900.0, 2),
        (1100.0, 3),
        (1400.0, 4),
        (1100.0, 3),
        (900.0, 2),
    ] {
        cx.simulate_resize(size(px(width), px(1000.0)));
        cx.run_until_parked();
        let first = cx.debug_bounds("favorite-grid-item-Episode-0").unwrap();
        let last = cx.debug_bounds(card_selectors[columns - 1]).unwrap();
        let next_row = cx.debug_bounds(card_selectors[columns]).unwrap();
        assert_eq!(first.size.width, px(228.0));
        assert!(first.size.height <= px(186.0));
        assert_eq!(last.top(), first.top());
        assert!(last.right() <= px(width - 24.0));
        assert_eq!(next_row.left(), first.left());
        assert_eq!(next_row.top() - first.top(), px(202.0));
        content.read_with(cx, |page, _| {
            assert_eq!(
                page.favorites[VideoItemType::Episode]
                    .paged
                    .grid_columns
                    .get(),
                columns
            )
        });
    }
}

#[gpui::test]
fn downloaded_favorite_episode_primary_is_found_by_the_card_image_lookup(cx: &mut TestAppContext) {
    use crate::{emby::EmbyImageRequest, images::cache::CachedImageKey};
    let (content, cx) = favorites_window(cx);
    content.update(cx, |page, _| {
        let mut episode = item(VideoItemType::Episode, 0);
        episode.image_tags = Some(HashMap::from([
            ("Primary".into(), "episode-primary".into()),
            ("Thumb".into(), "episode-thumb".into()),
        ]));
        episode.backdrop_image_tags = Some(vec!["episode-backdrop".into()]);
        let request = EmbyImageRequest::primary(episode.id.clone(), Some("episode-primary".into()))
            .with_max_width(640);
        let key = CachedImageKey::from_request(&page.current_server, &request).unwrap();
        let path = std::path::PathBuf::from("/tmp/favorite-episode-primary-test.png");
        page.image_loader.finish_job(key, Ok(path.clone()));
        assert_eq!(
            page.image_path_for_favorite_episode(&episode).as_deref(),
            Some(path.as_path())
        );
    });
}

#[gpui::test]
fn category_responses_filter_items_and_advance_by_raw_count_without_mixing_pages(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    content.update(cx, |page, cx| {
        page.favorites = FavoritesState::default();
        let movie_generation = page.favorites[VideoItemType::Movie]
            .paged
            .begin_initial(true)
            .unwrap();
        let series_generation = page.favorites[VideoItemType::Series]
            .paged
            .begin_initial(true)
            .unwrap();
        let mut items = (0..28)
            .map(|index| item(VideoItemType::Series, index))
            .collect::<Vec<_>>();
        items.push(item(VideoItemType::Movie, 0));
        let mut blank = item(VideoItemType::Series, 99);
        blank.id.clear();
        items.push(blank);
        page.user_data_overrides
            .insert("Series-0".into(), UserItemData::default());
        page.user_data_item_revisions.insert("Series-0".into(), 1);
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type: VideoItemType::Series,
                user_data_revision: 0,
                generation: series_generation,
                start_index: 0,
                initial: true,
            },
            Ok(UserItems {
                items,
                total_record_count: 40,
            }),
            cx,
        );
        let series = &page.favorites[VideoItemType::Series].paged;
        assert_eq!(series.items.len(), 27);
        assert_eq!(series.next_start_index, 30);
        assert!(series.can_load_more());
        assert!(
            series
                .items
                .iter()
                .all(|item| item.item_type.as_deref() == Some("Series"))
        );
        assert_eq!(
            page.favorites[VideoItemType::Movie].paged.initial,
            LoadState::Loading
        );

        let (generation, start_index) = page.favorites[VideoItemType::Series]
            .paged
            .begin_load_more()
            .unwrap();
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type: VideoItemType::Series,
                user_data_revision: 0,
                generation,
                start_index,
                initial: false,
            },
            Ok(UserItems {
                items: vec![
                    item(VideoItemType::Series, 27),
                    item(VideoItemType::Series, 30),
                ],
                total_record_count: 32,
            }),
            cx,
        );
        assert_eq!(page.favorites[VideoItemType::Series].paged.items.len(), 28);
        assert_eq!(
            page.favorites[VideoItemType::Series].paged.next_start_index,
            32
        );
        assert!(page.favorites[VideoItemType::Series].paged.exhausted);
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type: VideoItemType::Movie,
                user_data_revision: 0,
                generation: movie_generation,
                start_index: 0,
                initial: true,
            },
            Err(anyhow::anyhow!("request failed")),
            cx,
        );
        assert_eq!(
            page.favorites[VideoItemType::Movie].paged.initial,
            LoadState::Failed
        );
        assert_eq!(page.favorites[VideoItemType::Series].paged.items.len(), 28);
    });
}

#[gpui::test]
fn dirty_favorites_reject_old_pages_and_rollback_restores_the_original_category(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    content.update(cx, |page, cx| {
        let state = &mut page.favorites[VideoItemType::Episode].paged;
        state.exhausted = false;
        state.total_record_count = Some(60);
        let (generation, start_index) = state.begin_load_more().unwrap();
        let (item_type, index, removed) = page.favorites.remove_item("Episode-5").unwrap();
        assert_eq!(item_type, VideoItemType::Episode);
        page.favorites.mark_dirty();
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type,
                user_data_revision: 0,
                generation,
                start_index,
                initial: false,
            },
            Ok(UserItems {
                items: vec![item(VideoItemType::Episode, 30)],
                total_record_count: 60,
            }),
            cx,
        );
        assert_eq!(page.favorites[item_type].paged.items.len(), 29);
        page.favorites.restore_item(item_type, index, removed);
        assert_eq!(page.favorites[item_type].paged.items[5].id, "Episode-5");
        assert_eq!(page.favorites[item_type].paged.total_record_count, Some(60));
        assert_eq!(page.favorites[VideoItemType::Movie].paged.items.len(), 30);
        assert_eq!(page.favorites[VideoItemType::Series].paged.items.len(), 30);
    });
}

#[gpui::test]
fn failed_category_pagination_retries_the_same_offset_without_discarding_items(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    content.update(cx, |page, cx| {
        let item_type = VideoItemType::Movie;
        let state = &mut page.favorites[item_type].paged;
        state.exhausted = false;
        state.total_record_count = Some(60);
        let (generation, start_index) = state.begin_load_more().unwrap();
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type,
                user_data_revision: 0,
                generation,
                start_index,
                initial: false,
            },
            Err(anyhow::anyhow!("temporary failure")),
            cx,
        );
        let state = &mut page.favorites[item_type].paged;
        assert_eq!(state.items.len(), 30);
        assert!(!state.can_auto_load_more());
        assert_eq!(state.begin_load_more(), Some((generation, 30)));
        page.finish_favorites_page(
            FavoritesRequest {
                identity: page.request_identity(),
                item_type,
                user_data_revision: 0,
                generation,
                start_index: 30,
                initial: false,
            },
            Ok(UserItems {
                items: vec![item(item_type, 30)],
                total_record_count: 31,
            }),
            cx,
        );
        assert_eq!(page.favorites[item_type].paged.items.len(), 31);
        assert!(page.favorites[item_type].paged.exhausted);
        assert_eq!(
            page.favorites[VideoItemType::Series].paged.next_start_index,
            30
        );
    });
}
