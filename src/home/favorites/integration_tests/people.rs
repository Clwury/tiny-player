use super::sorting::{SortGateway, click};
use super::*;
use crate::emby::{SortOrder, UserItemsSort, VideoItemType};
use std::sync::{Arc, atomic::Ordering};

fn scroll_to_people(content: &Entity<HomeContent>, cx: &mut VisualTestContext) {
    content.update(cx, |page, cx| {
        page.favorites_presentation
            .scroll_handle
            .set_offset(point(px(0.0), px(-2000.0)));
        cx.notify();
    });
    cx.run_until_parked();
}

#[gpui::test]
fn favorite_people_row_uses_name_only_portraits_and_opens_person_with_return_position(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let gateway = Arc::new(SortGateway::default());
    content.update(cx, |page, _| page.ports.browsing = gateway.clone());
    scroll_to_people(&content, cx);
    let card = cx.debug_bounds("favorite-row-item-Person-0").unwrap();
    assert_eq!(card.size.width, px(148.0));
    assert_eq!(card.size.height, px(248.5));
    let next = cx.debug_bounds("favorite-row-item-Person-1").unwrap();
    assert_eq!(next.left() - card.right(), px(16.0));
    let offset = content.read_with(cx, |page, _| {
        page.favorites_presentation.scroll_handle.offset()
    });
    click("favorite-row-item-Person-0", cx);
    content.read_with(cx, |page, _| {
        assert!(matches!(page.controller.route(), HomeRoute::Person { person_id, .. } if person_id == "Person-0"));
        let person = page.controller.person_view("Person-0").unwrap();
        assert!(person.loaded);
        assert_eq!(person.items.sort_by, UserItemsSort::SortName);
        assert_eq!(person.items.sort_order, SortOrder::Ascending);
    });
    assert!(cx.debug_bounds("person-favorite-button").is_some());
    click("home-library-back-button", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.route(),
            &HomeRoute::Root(HomeRoot::Favorites)
        );
        assert_eq!(page.favorites_presentation.scroll_handle.offset(), offset);
    });
    assert!(cx.debug_bounds("favorite-more-Person").is_some());
    let queries = gateway.queries.lock().unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].person_ids, ["Person-0"]);
    assert_eq!(
        queries[0].include_item_types,
        [VideoItemType::Movie, VideoItemType::Series]
    );
    assert!(gateway.person_queries.lock().unwrap().is_empty());
}

#[gpui::test]
fn favorite_people_more_reuses_sorting_paginates_and_returns_from_person_without_reset(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let gateway = Arc::new(SortGateway::default());
    content.update(cx, |page, _| page.ports.browsing = gateway.clone());
    scroll_to_people(&content, cx);
    click("favorite-more-Person", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.route(),
            &HomeRoute::FavoriteItems {
                item_type: FavoriteItemType::Person
            }
        );
        assert_eq!(page.controller.title(), "演职人员");
        let section = page.controller.favorite_section(FavoriteItemType::Person);
        assert_eq!(section.sort_by, UserItemsSort::SortName);
        assert_eq!(section.sort_order, SortOrder::Ascending);
    });
    let columns = content.read_with(cx, |page, _| page.layout.view_model().grid_columns);
    assert_eq!(columns, 5);
    let first = cx.debug_bounds("favorite-grid-item-Person-0").unwrap();
    let second_row = cx.debug_bounds("favorite-grid-item-Person-5").unwrap();
    assert_eq!(first.size, size(px(148.0), px(248.5)));
    assert_eq!(second_row.top() - first.top(), px(266.0));
    assert!(second_row.top() > first.bottom());
    let sort = cx.debug_bounds("library-sort-select").unwrap();
    assert!(sort.left() > cx.debug_bounds("favorite-items-count").unwrap().right());
    content.update(cx, |page, cx| {
        page.favorites_presentation[FavoriteItemType::Person]
            .presentation
            .scroll_handle
            .set_offset(point(px(0.0), px(-150.0)));
        cx.notify();
    });
    cx.run_until_parked();
    click("library-sort-select", cx);
    click("library-sort-option-1", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.favorites_presentation[FavoriteItemType::Person]
                .presentation
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0)),
        )
    });
    click("library-sort-select", cx);
    click("library-sort-order-option-1", cx);
    content.update(cx, |page, cx| {
        page.load_more_favorites(FavoriteItemType::Person, cx)
    });
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        let section = page.controller.favorite_section(FavoriteItemType::Person);
        assert_eq!(section.paged.items.len(), 60);
        assert_eq!(section.paged.next_start_index, 60);
        assert_eq!(section.sort_by, UserItemsSort::DateCreated);
        assert_eq!(section.sort_order, SortOrder::Descending);
        let movies = page.controller.favorite_section(FavoriteItemType::Movie);
        assert_eq!(movies.sort_by, UserItemsSort::DateCreated);
        assert_eq!(movies.sort_order, SortOrder::Descending);
        assert!(movies.paged.items.is_empty());
        assert!(movies.paged.dirty);
    });
    content.update(cx, |page, cx| {
        page.favorites_presentation[FavoriteItemType::Person]
            .presentation
            .scroll_handle
            .set_offset(point(px(0.0), px(-120.0)));
        cx.notify();
    });
    cx.run_until_parked();
    let offset = content.read_with(cx, |page, _| {
        page.favorites_presentation[FavoriteItemType::Person]
            .presentation
            .scroll_handle
            .offset()
    });
    click("favorite-grid-item-Person-0", cx);
    click("home-library-back-button", cx);
    content.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.route(),
            &HomeRoute::FavoriteItems {
                item_type: FavoriteItemType::Person
            }
        );
        assert_eq!(
            page.favorites_presentation[FavoriteItemType::Person]
                .presentation
                .scroll_handle
                .offset(),
            offset
        );
        assert_eq!(
            page.controller
                .favorite_section(FavoriteItemType::Person)
                .paged
                .items
                .len(),
            60
        );
    });
    let queries = gateway.person_queries.lock().unwrap();
    assert_eq!(queries.len(), 3);
    assert!(
        queries
            .iter()
            .all(|query| query.include_item_types.is_empty()
                && query.is_favorite == Some(true)
                && query.person_ids.is_empty()
                && query.parent_id.is_none()
                && query.limit == 30)
    );
    assert_eq!(queries[0].sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(queries[0].sort_order, SortOrder::Ascending);
    assert_eq!(queries[1].sort_order, SortOrder::Descending);
    assert_eq!(queries[2].start_index, 30);
    assert_eq!(queries[2].sort_order, SortOrder::Descending);
}

#[gpui::test]
fn favorite_people_context_menu_only_offers_favorite_and_restores_failed_removal(
    cx: &mut TestAppContext,
) {
    let (content, cx) = favorites_window(cx);
    let gateway = Arc::new(SortGateway::default());
    gateway.fail_favorite.store(true, Ordering::SeqCst);
    content.update(cx, |page, _| page.ports.browsing = gateway.clone());
    scroll_to_people(&content, cx);
    click("favorite-more-Person", cx);
    for failed in [true, false] {
        gateway.fail_favorite.store(failed, Ordering::SeqCst);
        let position = cx
            .debug_bounds("favorite-grid-item-Person-0")
            .unwrap()
            .center();
        cx.simulate_mouse_down(position, gpui::MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(position, gpui::MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("user-view-item-favorite").is_some());
        assert!(cx.debug_bounds("user-view-item-mark-played").is_none());
        click("user-view-item-favorite", cx);
        content.read_with(cx, |page, _| {
            let items = &page
                .controller
                .favorite_section(FavoriteItemType::Person)
                .paged
                .items;
            assert_eq!(items.iter().any(|item| item.id == "Person-0"), failed);
            if failed {
                assert_eq!(items[0].id, "Person-0");
            }
            assert!(!page.controller.user_data_pending());
            assert!(matches!(
                page.controller.route(),
                HomeRoute::FavoriteItems {
                    item_type: FavoriteItemType::Person
                }
            ));
        });
    }
    assert!(cx.debug_bounds("favorite-grid-item-Person-0").is_none());
    assert_eq!(
        *gateway.favorites.lock().unwrap(),
        [("Person-0".into(), false), ("Person-0".into(), false)]
    );
}
