use gpui::{
    AppContext as _, IntoElement, ParentElement, Styled, TestAppContext, div, point, px, size,
};

use crate::{emby::UserItem, theme};

use super::{HomeRoot, HomeRoute, home_data_section_is_visible, main_scrollbar_is_visible};
use crate::home::{
    carousel::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        carousel_visible_range_between_for,
    },
    components::{tallest_home_item, user_episode_card, user_item_card},
};

use crate::home::model::layout::home_carousel_overscan;

#[test]
fn resize_carousels_keep_partial_cards_without_building_overscan() {
    let range = |resizing| {
        carousel_visible_range_between_for(
            100,
            (1_840.0, 1_840.0),
            800.0,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
            home_carousel_overscan(resizing),
        )
    };
    let resizing = range(true);
    assert_eq!(resizing.start..resizing.end, 10..15);
    let settled = range(false);
    assert_eq!(settled.start..settled.end, 8..19);
}

#[gpui::test]
fn representative_card_matches_full_carousel_height(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let cx = cx.add_empty_window();
    let movie = |year| {
        serde_json::from_value::<UserItem>(serde_json::json!({
            "Id": "movie", "Name": "Movie", "Type": "Movie", "ProductionYear": year
        }))
        .unwrap()
    };
    let episode = serde_json::from_value::<UserItem>(serde_json::json!({
        "Id": "episode", "Name": "Episode", "Type": "Episode", "SeriesName": "Series"
    }))
    .unwrap();

    for items in [
        vec![movie(None)],
        vec![movie(Some(2024))],
        vec![episode.clone()],
        vec![episode.clone(), movie(None)],
        vec![movie(None), episode, movie(Some(2024))],
    ] {
        cx.draw(
            point(px(0.0), px(0.0)),
            size(px(800.0), px(600.0)),
            |window, cx| {
                cx.new(|cx| {
                    let card = |item: &UserItem| {
                        if item.item_type.as_deref() == Some("Episode") {
                            user_episode_card(
                                crate::home::model::cards::UserEpisodeCardVm::new(
                                    item,
                                    item.user_data.as_ref(),
                                ),
                                None,
                                cx,
                            )
                        } else {
                            user_item_card(
                                crate::home::model::cards::UserItemCardVm::new(
                                    item,
                                    item.user_data.as_ref(),
                                    true,
                                ),
                                None,
                                cx,
                            )
                        }
                    };
                    let mut measurement = div()
                        .w(px(800.0))
                        .child(card(tallest_home_item(&items).unwrap()))
                        .into_any_element();
                    let mut full_row = div()
                        .w(px(800.0))
                        .flex()
                        .gap_4()
                        .children(items.iter().map(card))
                        .into_any_element();
                    let space = size(px(800.0).into(), gpui::AvailableSpace::MinContent);
                    assert_eq!(
                        measurement.layout_as_root(space, window, cx).height,
                        full_row.layout_as_root(space, window, cx).height,
                    );
                });
                div()
            },
        );
    }
}

#[test]
fn empty_home_section_is_hidden_while_loading_or_after_failure() {
    assert!(!home_data_section_is_visible(false, false, true, false));
    assert!(!home_data_section_is_visible(true, false, true, false));
    assert!(!home_data_section_is_visible(true, false, false, true));
}

#[test]
fn home_section_shows_cached_items_or_a_confirmed_empty_state() {
    assert!(home_data_section_is_visible(true, true, true, false));
    assert!(home_data_section_is_visible(true, false, false, false));
}

#[test]
fn empty_root_pages_hide_the_main_scrollbar() {
    assert!(!main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Home),
        false,
        false,
        false,
    ));
    assert!(!main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Favorites),
        false,
        false,
        false,
    ));
    assert!(!main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Search),
        false,
        false,
        false,
    ));
}

#[test]
fn populated_root_pages_show_the_main_scrollbar() {
    assert!(main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Home),
        true,
        false,
        false,
    ));
    assert!(main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Favorites),
        false,
        true,
        false,
    ));
    assert!(main_scrollbar_is_visible(
        &HomeRoute::Root(HomeRoot::Search),
        false,
        false,
        true,
    ));
}
