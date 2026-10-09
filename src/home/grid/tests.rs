use crate::emby::{SortOrder, UserItemsSort};
use crate::home::library::view::{library_sort_label, library_sort_order_label};

use super::*;

#[test]
fn workspace_auto_loads_within_one_viewport_of_the_bottom() {
    assert!(!workspace_scroll_position_is_near_end(
        500.0, 2_000.0, 800.0
    ));
    assert!(workspace_scroll_position_is_near_end(
        1_200.0, 2_000.0, 800.0
    ));
    assert!(workspace_scroll_position_is_near_end(
        1_800.0, 2_000.0, 300.0
    ));
}

#[test]
fn workspace_auto_loads_again_when_content_does_not_fill_the_viewport() {
    assert!(workspace_scroll_position_is_near_end(0.0, 0.0, 800.0));
}

#[test]
fn library_sort_controls_use_requested_labels() {
    assert_eq!(library_sort_label(UserItemsSort::SortName), "名称");
    assert_eq!(
        library_sort_label(UserItemsSort::DateLastContentAdded),
        "最后一集添加日期"
    );
    assert_eq!(library_sort_label(UserItemsSort::OfficialRating), "分级");
    assert_eq!(library_sort_order_label(SortOrder::Ascending), "升序");
    assert_eq!(library_sort_order_label(SortOrder::Descending), "降序");
}

#[test]
fn user_item_grid_columns_fit_the_available_width() {
    assert_eq!(
        user_item_grid_columns_for_width(719.0, HOME_ITEM_CARD_WIDTH_PX),
        3
    );
    assert_eq!(
        user_item_grid_columns_for_width(720.0, HOME_ITEM_CARD_WIDTH_PX),
        4
    );
    assert_eq!(
        user_item_grid_columns_for_width(1.0, HOME_ITEM_CARD_WIDTH_PX),
        1
    );
}

#[test]
fn uniform_grid_row_count_handles_empty_and_partial_rows() {
    assert_eq!(user_item_grid_row_count(0, 4), 0);
    assert_eq!(user_item_grid_row_count(1, 4), 1);
    assert_eq!(user_item_grid_row_count(9, 4), 3);
    assert_eq!(user_item_grid_row_count(9, 0), 9);
}

#[test]
fn virtual_grid_surface_width_changes_only_at_column_thresholds() {
    assert_eq!(
        user_item_grid_content_width(0, HOME_ITEM_CARD_WIDTH_PX),
        168.0
    );
    assert_eq!(
        user_item_grid_content_width(1, HOME_ITEM_CARD_WIDTH_PX),
        168.0
    );
    assert_eq!(
        user_item_grid_content_width(3, HOME_ITEM_CARD_WIDTH_PX),
        536.0
    );
}

#[test]
fn grid_column_thresholds_apply_immediately_in_both_directions() {
    assert_eq!(responsive_user_item_grid_columns(0), 1);
    assert_eq!(responsive_user_item_grid_columns(3), 3);
    assert_eq!(responsive_user_item_grid_columns(7), 7);
}

#[test]
fn grid_column_change_preserves_the_first_visible_card() {
    let old_scroll_top = USER_ITEM_GRID_ROW_STEP_PX * 2.0 + 42.0;
    let remapped = remap_workspace_grid_scroll_top(
        old_scroll_top,
        3,
        4,
        20,
        600.0,
        USER_ITEM_GRID_ROW_STEP_PX,
    );

    // Old row 2 starts with item 6, which belongs to new row 1.
    assert_eq!(remapped, USER_ITEM_GRID_ROW_STEP_PX + 42.0);
}

#[test]
fn unchanged_grid_keeps_and_clamps_pixel_scroll_position() {
    assert_eq!(
        remap_workspace_grid_scroll_top(653.0, 4, 4, 20, 600.0, USER_ITEM_GRID_ROW_STEP_PX),
        653.0
    );
    assert_eq!(
        remap_workspace_grid_scroll_top(653.0, 4, 4, 3, 800.0, USER_ITEM_GRID_ROW_STEP_PX),
        154.0
    );
    assert_eq!(
        remap_workspace_grid_scroll_top(f32::NAN, 4, 4, 3, 800.0, USER_ITEM_GRID_ROW_STEP_PX),
        0.0
    );
}

#[test]
fn virtual_grid_only_builds_visible_rows_with_small_overscan() {
    let scroll_top = USER_ITEM_GRID_ROW_STEP_PX * 10.0 + 20.0;
    let visible =
        user_item_grid_visible_rows(100, scroll_top, 640.0, 1, USER_ITEM_GRID_ROW_STEP_PX);

    assert_eq!(visible, 9..14);
    assert_eq!(
        user_item_grid_visible_rows(0, 0.0, 640.0, 1, USER_ITEM_GRID_ROW_STEP_PX),
        0..0
    );
    assert!(visible.len() <= 5);
}

#[test]
fn virtual_grid_drops_overscan_from_the_resize_hot_path() {
    let scroll_top = USER_ITEM_GRID_ROW_STEP_PX * 10.0 + 20.0;

    assert_eq!(
        user_item_grid_visible_rows(100, scroll_top, 640.0, 0, USER_ITEM_GRID_ROW_STEP_PX),
        10..13
    );
}

#[test]
fn virtual_grid_anticipates_scroll_clamp_on_height_expansion() {
    let visible = user_item_grid_visible_rows(
        20,
        USER_ITEM_GRID_ROW_STEP_PX * 15.0,
        USER_ITEM_GRID_ROW_STEP_PX * 10.0,
        1,
        USER_ITEM_GRID_ROW_STEP_PX,
    );

    // Expanding at the bottom clamps the top row from 15 to 10. Include
    // that new viewport plus only the normal one-row overscan.
    assert_eq!(visible, 9..20);
}

#[test]
fn episode_favorites_grid_uses_landscape_row_height_for_scrolling_and_resize() {
    let step = UserItemGridSource::Favorites(FavoriteItemType::Episode).row_step();
    assert_eq!(step, 202.0);
    assert_eq!(
        user_item_grid_visible_rows(100, 10.0 * step, 600.0, 1, step),
        9..14
    );
    assert_eq!(
        remap_workspace_grid_scroll_top(2.0 * step + 20.0, 3, 4, 20, 600.0, step),
        step + 20.0
    );
}
