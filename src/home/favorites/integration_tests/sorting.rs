use super::*;
use crate::{
    emby::{
        MediaItem, MediaItems, MediaSource, ResumeItems, SortOrder, UserItemsQuery, UserItemsSort,
        UserViews, VideoItemType,
    },
    home::{favorites::controller::FavoritesIntent, gateway::HomeGateway},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(super) struct SortGateway {
    pub(super) queries: Mutex<Vec<UserItemsQuery>>,
    pub(super) person_queries: Mutex<Vec<UserItemsQuery>>,
    pub(super) favorites: Mutex<Vec<(String, bool)>>,
    pub(super) fail_favorite: AtomicBool,
    removed_people: Mutex<std::collections::HashSet<String>>,
}

impl HomeGateway for SortGateway {
    fn persons(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
        self.person_queries.lock().unwrap().push(query.clone());
        let removed = self.removed_people.lock().unwrap();
        Ok(UserItems {
            items: (0..90)
                .map(|index| item(FavoriteItemType::Person, index as usize))
                .filter(|item| !removed.contains(&item.id))
                .skip(query.start_index as usize)
                .take(query.limit as usize)
                .collect(),
            total_record_count: 90 - removed.len() as u32,
        })
    }
    fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
        self.queries.lock().unwrap().push(query.clone());
        Ok(UserItems {
            items: (query.start_index..query.start_index + query.limit)
                .map(|index| item(query.include_item_types[0].into(), index as usize))
                .collect(),
            total_record_count: 90,
        })
    }
    fn similar_items(&self, _: &str) -> anyhow::Result<UserItems> {
        panic!("unexpected similar items")
    }
    fn show_seasons(&self, _: &str) -> anyhow::Result<MediaItems> {
        panic!("unexpected seasons")
    }
    fn show_next_up(&self, _: &str) -> anyhow::Result<MediaItems> {
        panic!("unexpected next up")
    }
    fn playback_media_sources(&self, _: &str) -> anyhow::Result<Vec<MediaSource>> {
        panic!("unexpected sources")
    }
    fn set_played(&self, _: &str, _: bool) -> anyhow::Result<UserItemData> {
        panic!("unexpected played")
    }
    fn media_item(&self, id: &str) -> anyhow::Result<MediaItem> {
        assert!(id.starts_with("Person-"));
        Ok(serde_json::from_value(serde_json::json!({
            "Id": id, "Name": "Person", "Type": "Person", "UserData": { "IsFavorite": true }
        }))
        .unwrap())
    }
    fn show_episodes(&self, _: &str, _: Option<&str>) -> anyhow::Result<MediaItems> {
        panic!("unexpected episodes")
    }
    fn mark_item_played(&self, _: &str) -> anyhow::Result<UserItemData> {
        panic!("unexpected mark played")
    }
    fn hide_item_from_resume(&self, _: &str) -> anyhow::Result<()> {
        panic!("unexpected hide")
    }
    fn set_favorite(&self, id: &str, favorite: bool) -> anyhow::Result<UserItemData> {
        self.favorites.lock().unwrap().push((id.into(), favorite));
        anyhow::ensure!(
            !self.fail_favorite.load(Ordering::SeqCst),
            "favorite failed"
        );
        if favorite {
            self.removed_people.lock().unwrap().remove(id);
        } else {
            self.removed_people.lock().unwrap().insert(id.into());
        }
        Ok(UserItemData {
            is_favorite: favorite,
            ..Default::default()
        })
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

pub(super) fn click(selector: &'static str, cx: &mut VisualTestContext) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn favorite_more_sort_menu_updates_queries_resets_scroll_and_shares_category_choices(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let gateway = Arc::new(SortGateway::default());
    content.update(cx, |page, _| page.ports.browsing = gateway.clone());
    cx.simulate_resize(size(px(900.0), px(1000.0)));
    cx.run_until_parked();
    click("favorite-more-Movie", cx);
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(section.sort_by, UserItemsSort::SortName);
        assert_eq!(section.sort_order, SortOrder::Ascending);
    });
    let sort = cx.debug_bounds("library-sort-select").unwrap();
    let count = cx.debug_bounds("favorite-items-count").unwrap();
    assert!(sort.left() > count.right());
    assert!(sort.right() <= px(900.0 - HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX));
    content.update(cx, |page, cx| {
        page.favorites_presentation[FavoriteItemType::Movie]
            .presentation
            .scroll_handle
            .set_offset(point(px(0.0), px(-300.0)));
        cx.notify();
    });
    cx.run_until_parked();
    let saved_offset = content.read_with(cx, |page, _| {
        page.favorites_presentation[FavoriteItemType::Movie]
            .presentation
            .scroll_handle
            .offset()
    });
    click("library-sort-select", cx);
    assert!(cx.debug_bounds("library-sort-option-9").is_some());
    assert!(cx.debug_bounds("library-sort-option-10").is_none());
    click("library-sort-option-0", cx);
    content.read_with(cx, |page, _| {
        assert!(!page.favorites_presentation[FavoriteItemType::Movie].sort_menu_open);
        assert_eq!(
            page.favorites_presentation[FavoriteItemType::Movie]
                .presentation
                .scroll_handle
                .offset(),
            saved_offset
        );
    });
    assert!(gateway.queries.lock().unwrap().is_empty());
    click("library-sort-select", cx);
    click("library-sort-option-1", cx);
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(section.sort_by, UserItemsSort::DateCreated);
        assert_eq!(section.sort_order, SortOrder::Ascending);
        assert_eq!(section.paged.items.len(), 30);
        assert_eq!(
            page.favorites_presentation[FavoriteItemType::Movie]
                .presentation
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0))
        );
    });
    click("library-sort-select", cx);
    click("library-sort-order-option-1", cx);
    content.update(cx, |page, cx| {
        page.load_more_favorites(FavoriteItemType::Movie, cx)
    });
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(section.sort_order, SortOrder::Descending);
        assert_eq!(section.paged.items.len(), 60);
    });
    click("library-sort-select", cx);
    assert!(cx.debug_bounds("library-sort-menu").is_some());
    click("favorite-items-count", cx);
    assert!(cx.debug_bounds("library-sort-menu").is_none());
    let before_return = gateway.queries.lock().unwrap().len();
    click("home-library-back-button", cx);
    content.read_with(cx, |page, _| {
        for kind in FAVORITE_ITEM_TYPES {
            let row = page.controller.favorite_overview_section(kind);
            assert_eq!(row.sort_by, UserItemsSort::DateCreated);
            assert_eq!(row.sort_order, SortOrder::Descending);
            assert_eq!(row.paged.items.len(), 30);
            assert!(!row.paged.dirty);
            for (index, item) in row.paged.items.iter().enumerate() {
                assert_eq!(item.id, format!("{}-{index}", kind.as_str()));
            }
        }
    });
    assert_eq!(gateway.queries.lock().unwrap().len(), before_return);
    assert!(gateway.person_queries.lock().unwrap().is_empty());
    click("favorite-more-Series", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller
                .favorite_section(FavoriteItemType::Series)
                .sort_by,
            UserItemsSort::DateCreated
        );
    });
    click("library-sort-select", cx);
    assert!(cx.debug_bounds("library-sort-option-10").is_some());
    click("library-sort-option-7", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller
                .favorite_section(FavoriteItemType::Series)
                .sort_by,
            UserItemsSort::DateLastContentAdded
        );
        assert_eq!(
            page.controller
                .favorite_section(FavoriteItemType::Episode)
                .sort_by,
            UserItemsSort::SortName
        );
    });
    click("home-library-back-button", cx);
    click("favorite-more-Movie", cx);
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(section.sort_by, UserItemsSort::SortName);
        assert_eq!(section.sort_order, SortOrder::Descending);
        assert!(!page.favorites_presentation[FavoriteItemType::Movie].sort_menu_open);
    });
    content.update(cx, |page, cx| {
        page.dispatch_favorites(
            FavoriteItemType::Movie,
            FavoritesIntent::SortBy(UserItemsSort::DateLastContentAdded),
            cx,
        );
        assert_eq!(
            crate::media::ItemSortPreferences::get(cx).sort_by,
            UserItemsSort::DateLastContentAdded
        );
    });
    cx.run_until_parked();
    let queries = gateway.queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 6);
    assert!(queries.iter().all(|query| query.is_favorite == Some(true)
        && query.recursive
        && query.parent_id.is_none()
        && query.secondary_sort_by.is_empty()));
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(queries[0].sort_order, SortOrder::Ascending);
    assert_eq!(queries[1].sort_order, SortOrder::Descending);
    assert_eq!(queries[2].start_index, 30);
    assert_eq!(queries[2].sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(queries[2].sort_order, SortOrder::Descending);
    for query in &queries[3..4] {
        assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
        assert_eq!(query.sort_order, SortOrder::Descending);
        assert_eq!(query.start_index, 0);
    }
    assert_eq!(
        queries[4].sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert_eq!(queries[5].sort_by, Some(UserItemsSort::SortName));
    for query in &queries[4..] {
        assert_eq!(query.sort_order, SortOrder::Descending);
        assert_eq!(query.start_index, 0);
    }
    click("home-library-back-button", cx);
    click("favorite-more-Series", cx);
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Series);
        assert_eq!(section.sort_by, UserItemsSort::DateLastContentAdded);
        assert_eq!(section.sort_order, SortOrder::Descending);
    });
    assert_eq!(gateway.queries.lock().unwrap().len(), 6);
}

#[gpui::test]
fn global_sort_changes_keep_favorite_rows_and_row_refresh_uses_original_sort(
    cx: &mut TestAppContext,
) {
    use crate::media::ItemSortPreferences;
    let (content, cx) = favorites_window(cx);
    let gateway = Arc::new(SortGateway::default());
    content.update(cx, |page, _| page.ports.browsing = gateway.clone());
    cx.update(|_, cx| {
        ItemSortPreferences {
            sort_by: UserItemsSort::PremiereDate,
            sort_order: SortOrder::Descending,
        }
        .apply(cx)
    });
    cx.run_until_parked();
    assert!(gateway.queries.lock().unwrap().is_empty());
    assert!(gateway.person_queries.lock().unwrap().is_empty());
    content.read_with(cx, |page, _| {
        for kind in FAVORITE_ITEM_TYPES {
            let row = page.controller.favorite_overview_section(kind);
            assert_eq!(row.sort_by, UserItemsSort::DateCreated);
            assert_eq!(row.sort_order, SortOrder::Descending);
            assert_eq!(row.paged.items.len(), 30);
            assert!(!row.paged.dirty);
        }
    });
    click("favorite-more-Movie", cx);
    click("home-library-back-button", cx);
    assert_eq!(gateway.queries.lock().unwrap().len(), 1);
    content.update(cx, |page, cx| {
        page.load_favorites_initial(FavoriteItemType::Movie, cx)
    });
    cx.run_until_parked();
    let queries = gateway.queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::PremiereDate));
    assert!(queries[0].secondary_sort_by.is_empty());
    assert_eq!(queries[0].sort_order, SortOrder::Descending);
    assert_eq!(queries[1].sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(
        queries[1].secondary_sort_by,
        [UserItemsSort::DateLastContentAdded, UserItemsSort::SortName]
    );
    assert_eq!(queries[1].sort_order, SortOrder::Descending);
    click("favorite-more-Movie", cx);
    content.read_with(cx, |page, _| {
        let vm = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(vm.sort_by, UserItemsSort::PremiereDate);
        assert_eq!(vm.sort_order, SortOrder::Descending);
        assert_eq!(vm.paged.items.len(), 30);
    });
    assert_eq!(gateway.queries.lock().unwrap().len(), 2);
}
