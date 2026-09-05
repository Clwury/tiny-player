use std::ops::Range;

use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, canvas, deferred, div, point,
    prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{SortOrder, UserItem, UserItemsSort},
    theme,
};

use super::{
    HomeContent,
    carousel::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, home_main_content_width_for_window_width,
    },
    components::{user_episode_card, user_item_card},
    library::{LibraryState, available_library_sorts},
    navigation::HomeRoute,
    paged_items::PagedItemsState,
};

const WORKSPACE_AUTO_LOAD_MIN_THRESHOLD_PX: f32 = 480.0;
const LIBRARY_FIXED_HEADER_HEIGHT_PX: f32 = 64.0;
const LIBRARY_SORT_SELECT_WIDTH_PX: f32 = 232.0;
const LIBRARY_SORT_OPTION_HEIGHT_PX: f32 = 30.0;
const LIBRARY_SORT_ORDERS: [SortOrder; 2] = [SortOrder::Ascending, SortOrder::Descending];
// Movie/series cards are 302px tall at the default rem size (240px artwork,
// 8px total padding, an 8px card gap, and two metadata lines). Keeping the
// measured height here preserves the original flex-grid row spacing exactly.
const USER_ITEM_GRID_ROW_HEIGHT_PX: f32 = 302.0;
const USER_ITEM_GRID_ROW_STEP_PX: f32 = USER_ITEM_GRID_ROW_HEIGHT_PX + HOME_ITEM_CARD_GAP_PX;
const USER_ITEM_GRID_OVERSCAN_ROWS: usize = 1;

#[derive(Clone, Copy)]
enum UserItemGridSource {
    Favorites,
    Search,
}

impl UserItemGridSource {
    fn scroll_id(self) -> &'static str {
        match self {
            Self::Favorites => "favorite-grid-scroll",
            Self::Search => "search-grid-scroll",
        }
    }

    fn card_id_prefix(self) -> &'static str {
        match self {
            Self::Favorites => "favorite-grid-item",
            Self::Search => "search-grid-item",
        }
    }
}

impl HomeContent {
    pub(super) fn render_library_scrollable_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let view_id = match self.navigation.current() {
            HomeRoute::Library { view_id, .. } => view_id,
            _ => unreachable!("library renderer requires a Library route"),
        };
        let state = self
            .libraries
            .get(view_id)
            .expect("Library route has cached LibraryState");
        let total = state
            .paged
            .total_record_count
            .map(|total| format!("共 {total} 项"));
        let sort_select = self.render_library_sort_select(view_id, state, cx);
        let back = cx.listener(Self::close_series_detail);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-library-content")
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.close_current_library_sort_menu(cx)),
            )
            .child(
                div()
                    .id("home-library-fixed-header")
                    .flex()
                    .h(px(LIBRARY_FIXED_HEADER_HEIGHT_PX))
                    .flex_none()
                    .items_start()
                    .gap_3()
                    .bg(theme.background)
                    .px_4()
                    .pt_4()
                    .child(library_back_button(back, cx))
                    .child(
                        div()
                            .flex()
                            .h(px(32.0))
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(div().min_w_0().when_some(total, |this, total| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(total),
                                )
                            }))
                            .child(sort_select),
                    ),
            )
            .child(
                div()
                    .id("home-library-scroll-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
                    .track_scroll(&state.paged.scroll_handle)
                    .px_6()
                    .pb_6()
                    .when(
                        state.paged.initial != super::LoadState::Loading
                            && state.paged.initial_error.is_none()
                            && state.paged.items.is_empty(),
                        |this| {
                            this.child(self.render_center_message("该媒体库暂无内容", false, cx))
                        },
                    )
                    .when(!state.paged.items.is_empty(), |this| {
                        this.child(self.render_items_grid(
                            &state.paged.items,
                            "library-grid-item",
                            cx,
                        ))
                        .child(self.render_library_paged_footer(&state.paged, view_id, cx))
                    }),
            )
    }

    fn render_library_sort_select(
        &self,
        view_id: &str,
        state: &LibraryState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let sort_by = state.sort_by;
        let sort_order = state.sort_order;
        let menu_open = state.sort_menu_open;
        let trigger_label = format!(
            "{} · {}",
            library_sort_label(sort_by),
            library_sort_order_label(sort_order)
        );
        let toggle = cx.listener(|page, _, _, cx| page.toggle_current_library_sort_menu(cx));
        let sort_options = available_library_sorts(&state.item_types)
            .enumerate()
            .map(|(index, candidate)| {
                let option_view_id = view_id.to_string();
                let select = cx.listener(move |page, _, _, cx| {
                    page.select_library_sort_by(option_view_id.clone(), candidate, cx);
                });
                library_sort_option(
                    library_sort_label(candidate),
                    candidate == sort_by,
                    (
                        gpui::ElementId::from("library-sort-option"),
                        index.to_string(),
                    ),
                    cx,
                )
                .on_click(select)
            })
            .collect::<Vec<_>>();
        let order_options = LIBRARY_SORT_ORDERS
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                let option_view_id = view_id.to_string();
                let select = cx.listener(move |page, _, _, cx| {
                    page.select_library_sort_order(option_view_id.clone(), candidate, cx);
                });
                library_sort_option(
                    library_sort_order_label(candidate),
                    candidate == sort_order,
                    (
                        gpui::ElementId::from("library-sort-order-option"),
                        index.to_string(),
                    ),
                    cx,
                )
                .on_click(select)
            })
            .collect::<Vec<_>>();
        div()
            .relative()
            .flex_none()
            .child(
                library_sort_trigger(trigger_label, menu_open, cx)
                    .id("library-sort-select")
                    .on_click(toggle),
            )
            .when(menu_open, |this| {
                this.child(
                    deferred(
                        div()
                            .id("library-sort-menu")
                            .absolute()
                            .top(px(40.0))
                            .right_0()
                            .flex()
                            .w(px(LIBRARY_SORT_SELECT_WIDTH_PX))
                            .flex_col()
                            .overflow_hidden()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.input_border_focused)
                            .bg(theme.dialog_background)
                            .shadow_lg()
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(library_sort_group_label("排序方式", cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .children(sort_options),
                            )
                            .child(div().mx_2().h(px(1.0)).bg(theme.input_border))
                            .child(library_sort_group_label("排列顺序", cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .children(order_options),
                            ),
                    )
                    .with_priority(2),
                )
            })
    }

    pub(super) fn render_favorites_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let page = cx.entity().downgrade();
        let scroll_handle = self.favorites.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                let page = page.clone();
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| page.auto_load_more_favorites(cx))
                        .ok();
                });
            },
        )
        .w_full()
        .h(px(1.0))
        .absolute()
        .left_0()
        .right_0()
        .bottom_0();
        let grid = self.render_virtual_items_grid(
            UserItemGridSource::Favorites,
            self.workspace_grid_columns.max(1),
            self.favorites.scroll_handle.clone(),
            &self.favorites.grid_columns,
            f32::from(window.bounds().size.height),
            cx,
        );

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-favorites-content")
            .when(
                self.favorites.initial != super::LoadState::Loading
                    && self.favorites.initial_error.is_none()
                    && self.favorites.items.is_empty(),
                |this| {
                    this.child(div().size_full().p_6().child(self.render_center_message(
                        "暂无收藏的电影或剧集",
                        false,
                        cx,
                    )))
                },
            )
            .when(!self.favorites.items.is_empty(), |this| {
                this.child(div().size_full().p_6().child(grid)).when(
                    self.favorites.can_auto_load_more() && !self.resize_in_progress,
                    |this| this.child(auto_load_observer),
                )
            })
    }

    pub(super) fn render_search_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let page = cx.entity().downgrade();
        let scroll_handle = self.search.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                let page = page.clone();
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| page.auto_load_more_search(cx))
                        .ok();
                });
            },
        )
        .w_full()
        .h(px(1.0))
        .absolute()
        .left_0()
        .right_0()
        .bottom_0();
        let grid = self.render_virtual_items_grid(
            UserItemGridSource::Search,
            self.workspace_grid_columns.max(1),
            self.search.scroll_handle.clone(),
            &self.search.grid_columns,
            f32::from(window.bounds().size.height),
            cx,
        );

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-search-content")
            .flex()
            .flex_col()
            .child(
                div()
                    .id("home-search-fixed-header")
                    .flex_none()
                    .bg(theme.background)
                    .px_6()
                    .pt_6()
                    .pb_5()
                    .child(div().w_full().child(self.search_input.clone())),
            )
            .child(
                div()
                    .id("home-search-scroll-content")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .px_6()
                    .pb_6()
                    .when(
                        !self.search.query.is_empty()
                            && self.search.initial == super::LoadState::Loaded
                            && self.search.items.is_empty()
                            && self.search.exhausted,
                        |this| {
                            this.child(self.render_center_message(
                                "未找到相关电影或剧集",
                                false,
                                cx,
                            ))
                        },
                    )
                    .when(!self.search.items.is_empty(), |this| this.child(grid))
                    .when(
                        self.search.can_load_more() && !self.resize_in_progress,
                        |this| this.child(auto_load_observer),
                    ),
            )
    }

    fn render_items_grid(
        &self,
        items: &[UserItem],
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div().flex().flex_wrap().items_start().gap_4().children(
            items
                .iter()
                .map(|item| self.render_user_item_card(item, id_prefix, cx)),
        )
    }

    fn render_virtual_items_grid(
        &self,
        source: UserItemGridSource,
        columns: usize,
        scroll_handle: ScrollHandle,
        previous_columns: &std::cell::Cell<usize>,
        viewport_height: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let columns = columns.max(1);
        let item_count = match source {
            UserItemGridSource::Favorites => self.favorites.items.len(),
            UserItemGridSource::Search => self.search.items.len(),
        };
        let row_count = user_item_grid_row_count(item_count, columns);
        let tracked_viewport_height = f32::from(scroll_handle.bounds().size.height).max(0.0);
        let scroll_viewport_height = if tracked_viewport_height > 0.0 {
            tracked_viewport_height
        } else {
            viewport_height
        };
        let scroll_top = sync_workspace_grid_scroll(
            &scroll_handle,
            previous_columns,
            columns,
            row_count,
            scroll_viewport_height,
        );
        // The tracked bounds describe the previous frame during a native resize,
        // while `viewport_height` already describes the new window. Use the new
        // height when choosing rows: the helper anticipates GPUI's upcoming scroll
        // clamp on expansion and avoids building the old tall viewport on shrink.
        let overscan_rows = if self.resize_in_progress {
            0
        } else {
            USER_ITEM_GRID_OVERSCAN_ROWS
        };
        let visible_rows =
            user_item_grid_visible_rows(row_count, scroll_top, viewport_height, overscan_rows);
        let total_height = row_count as f32 * USER_ITEM_GRID_ROW_STEP_PX;
        let grid_width = user_item_grid_content_width(columns);

        // The regular scroll container owns only one total-height spacer and the
        // visible fixed-height rows. Unlike GPUI's general-purpose list elements,
        // it performs no probe layout and has no per-row measurement tree to reset
        // when a window resize changes the number of columns.
        div()
            .id(source.scroll_id())
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&scroll_handle)
            .child(
                div()
                    .relative()
                    // Cards and gaps have fixed widths. Keeping the virtual surface
                    // fixed between column thresholds prevents every 1px native
                    // shrink from propagating a new width constraint through every
                    // visible card subtree.
                    .w(px(grid_width))
                    .h(px(total_height))
                    .flex_none()
                    .children(visible_rows.map(|row| {
                        self.render_user_item_grid_row(
                            source,
                            row,
                            columns,
                            grid_width,
                            source.card_id_prefix(),
                            cx,
                        )
                        .absolute()
                        .top(px(row as f32 * USER_ITEM_GRID_ROW_STEP_PX))
                        .left_0()
                    })),
            )
    }

    fn render_user_item_grid_row(
        &self,
        source: UserItemGridSource,
        row: usize,
        columns: usize,
        grid_width: f32,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let items = match source {
            UserItemGridSource::Favorites => &self.favorites.items,
            UserItemGridSource::Search => &self.search.items,
        };
        let start = row.saturating_mul(columns.max(1));
        if start >= items.len() {
            // A request can finish between the render and prepaint passes (for
            // example when a search query is replaced). Keep the uniform row
            // height stable instead of slicing past the newly shortened list.
            return div().w(px(grid_width)).h(px(USER_ITEM_GRID_ROW_STEP_PX));
        }
        let end = (start + columns.max(1)).min(items.len());
        div()
            .w(px(grid_width))
            .h(px(USER_ITEM_GRID_ROW_STEP_PX))
            .flex()
            .flex_none()
            .items_start()
            .gap_4()
            .children(items[start..end].iter().enumerate().map(|(offset, item)| {
                self.render_user_item_grid_card(item, start + offset, source, id_prefix, cx)
            }))
    }

    fn render_user_item_grid_card(
        &self,
        item: &UserItem,
        index: usize,
        source: UserItemGridSource,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let item = self.effective_user_item(item);
        let item_fingerprint = user_item_id_fingerprint(&item.id);
        let open = cx.listener(move |page, _, _, cx| {
            page.open_user_item_grid_index(source, index, item_fingerprint, cx);
        });
        // Keep element identity stable for an item without cloning its ID into
        // every resize-built listener. The fingerprint is also checked by the
        // click handler so a late event from a removed/reordered result cannot
        // open the wrong item.
        let item_id = gpui::ElementId::from((id_prefix, item_fingerprint));
        if item.item_type.as_deref() == Some("Episode") {
            let image_path = self.image_path_for_episode_user_item(&item);
            user_episode_card(&item, image_path, cx)
                .id(item_id)
                .cursor_pointer()
                .on_click(open)
        } else {
            let image_path = self.image_path_for_user_item(&item);
            user_item_card(&item, image_path, cx)
                .id(item_id)
                .cursor_pointer()
                .on_click(open)
        }
    }

    fn open_user_item_grid_index(
        &mut self,
        source: UserItemGridSource,
        index: usize,
        expected_fingerprint: u64,
        cx: &mut Context<Self>,
    ) {
        let item_id = match source {
            UserItemGridSource::Favorites => self.favorites.items.get(index),
            UserItemGridSource::Search => self.search.items.get(index),
        }
        .filter(|item| user_item_id_fingerprint(&item.id) == expected_fingerprint)
        .map(|item| item.id.clone());
        if let Some(item_id) = item_id {
            self.open_media_detail_by_id(item_id, cx);
        }
    }

    fn render_user_item_card(
        &self,
        item: &UserItem,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let item = self.effective_user_item(item);
        let item_id = item.id.clone();
        let open_item_id = item_id.clone();
        let open = cx.listener(move |page, _, _, cx| {
            page.open_media_detail_by_id(open_item_id.clone(), cx);
        });
        if item.item_type.as_deref() == Some("Episode") {
            let image_path = self.image_path_for_episode_user_item(&item);
            user_episode_card(&item, image_path, cx)
                .id((gpui::ElementId::from(id_prefix), item_id))
                .cursor_pointer()
                .on_click(open)
        } else {
            let image_path = self.image_path_for_user_item(&item);
            user_item_card(&item, image_path, cx)
                .id((gpui::ElementId::from(id_prefix), item_id))
                .cursor_pointer()
                .on_click(open)
        }
    }

    fn render_library_paged_footer(
        &self,
        state: &PagedItemsState,
        view_id: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let page = cx.entity().downgrade();
        let auto_load_view_id = view_id.to_string();
        let scroll_handle = state.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                let page = page.clone();
                let view_id = auto_load_view_id.clone();
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| {
                        page.auto_load_more_library(&view_id, cx);
                    })
                    .ok();
                });
            },
        )
        .w_full()
        .h(px(1.0));

        div()
            .id("library-auto-load-footer")
            .mt_5()
            .flex()
            .min_h(px(1.0))
            .flex_col()
            .items_center()
            .gap_2()
            .when(state.can_auto_load_more(), |this| {
                this.child(auto_load_observer)
            })
    }

    fn render_center_message(
        &self,
        message: &'static str,
        error: bool,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = theme::get(cx);
        div()
            .py_6()
            .text_sm()
            .text_color(if error {
                theme.error
            } else {
                theme.muted_foreground
            })
            .child(message)
    }
}

fn user_item_id_fingerprint(item_id: &str) -> u64 {
    // FNV-1a is sufficient for an element key here and avoids allocating a
    // temporary `String` on every visible card. The click path validates the
    // same fingerprint against the current item before navigating.
    item_id.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    })
}

fn sync_workspace_grid_scroll(
    scroll_handle: &ScrollHandle,
    previous_columns: &std::cell::Cell<usize>,
    columns: usize,
    row_count: usize,
    viewport_height: f32,
) -> f32 {
    let columns = columns.max(1);
    let old_columns = previous_columns.get().max(1);
    let offset = scroll_handle.offset();
    let old_scroll_top = (-f32::from(offset.y)).max(0.0);
    let scroll_top = remap_workspace_grid_scroll_top(
        old_scroll_top,
        old_columns,
        columns,
        row_count,
        viewport_height,
    );
    previous_columns.set(columns);

    if (old_scroll_top - scroll_top).abs() >= 0.5 {
        scroll_handle.set_offset(point(offset.x, px(-scroll_top)));
    }

    scroll_top
}

fn remap_workspace_grid_scroll_top(
    old_scroll_top: f32,
    old_columns: usize,
    columns: usize,
    row_count: usize,
    viewport_height: f32,
) -> f32 {
    if row_count == 0 {
        return 0.0;
    }

    let old_columns = old_columns.max(1);
    let columns = columns.max(1);
    let old_scroll_top = if old_scroll_top.is_finite() {
        old_scroll_top.max(0.0)
    } else {
        0.0
    };
    let old_row = (old_scroll_top / USER_ITEM_GRID_ROW_STEP_PX).floor() as usize;
    let offset_in_row = old_scroll_top - old_row as f32 * USER_ITEM_GRID_ROW_STEP_PX;
    let first_visible_item = old_row.saturating_mul(old_columns);
    let new_row = (first_visible_item / columns).min(row_count - 1);
    let target = if old_columns == columns {
        old_scroll_top
    } else {
        new_row as f32 * USER_ITEM_GRID_ROW_STEP_PX + offset_in_row
    };
    let viewport_height = if viewport_height.is_finite() {
        viewport_height.max(0.0)
    } else {
        0.0
    };
    let max_scroll = (row_count as f32 * USER_ITEM_GRID_ROW_STEP_PX - viewport_height).max(0.0);
    target.clamp(0.0, max_scroll)
}

fn user_item_grid_visible_rows(
    row_count: usize,
    scroll_top: f32,
    viewport_height: f32,
    overscan_rows: usize,
) -> Range<usize> {
    if row_count == 0 {
        return 0..0;
    }

    let scroll_top = if scroll_top.is_finite() {
        scroll_top.max(0.0)
    } else {
        0.0
    };
    let viewport_height = if viewport_height.is_finite() {
        viewport_height.max(0.0)
    } else {
        0.0
    };
    // During a height expansion the scroll handle still contains the previous
    // frame's offset. GPUI clamps it later in prepaint; anticipate that clamp so
    // the newly exposed rows are present in this same frame. The full window
    // height is a conservative upper bound for the page viewport, so this may
    // build one extra row but cannot omit a visible one.
    let content_height = row_count as f32 * USER_ITEM_GRID_ROW_STEP_PX;
    let scroll_top = scroll_top.min((content_height - viewport_height).max(0.0));
    let first = (scroll_top / USER_ITEM_GRID_ROW_STEP_PX).floor() as usize;
    let last = ((scroll_top + viewport_height) / USER_ITEM_GRID_ROW_STEP_PX).ceil() as usize;

    first.saturating_sub(overscan_rows).min(row_count)
        ..last.saturating_add(overscan_rows).min(row_count)
}

pub(super) fn user_item_grid_columns(window: &Window) -> usize {
    user_item_grid_columns_for_window_width(f32::from(window.bounds().size.width))
}

pub(super) fn user_item_grid_columns_for_window_width(window_width: f32) -> usize {
    user_item_grid_columns_for_width(home_main_content_width_for_window_width(window_width))
}

pub(super) fn responsive_user_item_grid_columns(measured: usize) -> usize {
    // Card positions only change when a full card-width threshold is crossed.
    // Applying that meaningful change immediately keeps expansion responsive;
    // fixed-height visible-row virtualization keeps each threshold cheap.
    measured.max(1)
}

fn user_item_grid_columns_for_width(available_width: f32) -> usize {
    let card_outer_width = HOME_ITEM_CARD_WIDTH_PX + HOME_ITEM_CARD_PADDING_PX * 2.0;
    ((available_width.max(0.0) + HOME_ITEM_CARD_GAP_PX)
        / (card_outer_width + HOME_ITEM_CARD_GAP_PX))
        .floor()
        .max(1.0) as usize
}

fn user_item_grid_row_count(item_count: usize, columns: usize) -> usize {
    item_count.div_ceil(columns.max(1))
}

fn user_item_grid_content_width(columns: usize) -> f32 {
    let columns = columns.max(1);
    let card_outer_width = HOME_ITEM_CARD_WIDTH_PX + HOME_ITEM_CARD_PADDING_PX * 2.0;
    columns as f32 * card_outer_width + columns.saturating_sub(1) as f32 * HOME_ITEM_CARD_GAP_PX
}

fn library_back_button(
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<HomeContent>,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("home-library-back-button")
        .flex()
        .size(px(32.0))
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_md()
        .occlude()
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            svg()
                .path("icons/chevron-left.svg")
                .size(px(18.0))
                .text_color(theme.foreground),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(on_click)
}

fn library_sort_trigger<T>(label: String, menu_open: bool, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .w(px(LIBRARY_SORT_SELECT_WIDTH_PX))
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(px(8.0))
        .border_1()
        .border_color(if menu_open {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(div().min_w_0().truncate().child(label))
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .size(px(14.0))
                .text_color(theme.muted_foreground),
        )
}

fn library_sort_group_label<T>(label: &'static str, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(28.0))
        .flex_none()
        .items_center()
        .px_3()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(label)
}

fn library_sort_option<T>(
    label: &'static str,
    selected: bool,
    id: impl Into<gpui::ElementId>,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .flex()
        .h(px(LIBRARY_SORT_OPTION_HEIGHT_PX))
        .flex_none()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(px(6.0))
        .px_2()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .bg(if selected {
            theme.secondary_hover
        } else {
            theme.dialog_background
        })
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(div().min_w_0().truncate().child(label))
        .when(selected, |this| {
            this.child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.input_border_focused)
                    .child("✓"),
            )
        })
}

fn library_sort_label(sort_by: UserItemsSort) -> &'static str {
    match sort_by {
        UserItemsSort::SortName => "名称",
        UserItemsSort::DateCreated => "添加日期",
        UserItemsSort::PremiereDate => "发行日期",
        UserItemsSort::ProductionYear => "年份",
        UserItemsSort::CommunityRating => "社区评分",
        UserItemsSort::CriticRating => "影评人评分",
        UserItemsSort::DatePlayed => "播放日期",
        UserItemsSort::DateLastContentAdded => "最后一集添加日期",
        UserItemsSort::PlayCount => "播放次数",
        UserItemsSort::Random => "随机",
        UserItemsSort::OfficialRating => "分级",
    }
}

fn library_sort_order_label(sort_order: SortOrder) -> &'static str {
    match sort_order {
        SortOrder::Ascending => "升序",
        SortOrder::Descending => "降序",
    }
}

fn workspace_scroll_is_near_end(scroll_handle: &ScrollHandle) -> bool {
    let scroll_top = -f32::from(scroll_handle.offset().y);
    let max_offset = f32::from(scroll_handle.max_offset().y);
    let viewport_height = f32::from(scroll_handle.bounds().size.height);
    workspace_scroll_position_is_near_end(scroll_top, max_offset, viewport_height)
}

fn workspace_scroll_position_is_near_end(
    scroll_top: f32,
    max_offset: f32,
    viewport_height: f32,
) -> bool {
    let max_offset = max_offset.max(0.0);
    let scroll_top = scroll_top.clamp(0.0, max_offset);
    let threshold = viewport_height.max(WORKSPACE_AUTO_LOAD_MIN_THRESHOLD_PX);
    max_offset - scroll_top <= threshold
}

#[cfg(test)]
mod tests {
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
        assert_eq!(user_item_grid_columns_for_width(719.0), 3);
        assert_eq!(user_item_grid_columns_for_width(720.0), 4);
        assert_eq!(user_item_grid_columns_for_width(1.0), 1);
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
        assert_eq!(user_item_grid_content_width(0), 168.0);
        assert_eq!(user_item_grid_content_width(1), 168.0);
        assert_eq!(user_item_grid_content_width(3), 536.0);
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
        let remapped = remap_workspace_grid_scroll_top(old_scroll_top, 3, 4, 20, 600.0);

        // Old row 2 starts with item 6, which belongs to new row 1.
        assert_eq!(remapped, USER_ITEM_GRID_ROW_STEP_PX + 42.0);
    }

    #[test]
    fn unchanged_grid_keeps_and_clamps_pixel_scroll_position() {
        assert_eq!(
            remap_workspace_grid_scroll_top(653.0, 4, 4, 20, 600.0),
            653.0
        );
        assert_eq!(
            remap_workspace_grid_scroll_top(653.0, 4, 4, 3, 800.0),
            154.0
        );
        assert_eq!(
            remap_workspace_grid_scroll_top(f32::NAN, 4, 4, 3, 800.0),
            0.0
        );
    }

    #[test]
    fn virtual_grid_only_builds_visible_rows_with_small_overscan() {
        let scroll_top = USER_ITEM_GRID_ROW_STEP_PX * 10.0 + 20.0;
        let visible = user_item_grid_visible_rows(100, scroll_top, 640.0, 1);

        assert_eq!(visible, 9..14);
        assert_eq!(user_item_grid_visible_rows(0, 0.0, 640.0, 1), 0..0);
        assert!(visible.len() <= 5);
    }

    #[test]
    fn virtual_grid_drops_overscan_from_the_resize_hot_path() {
        let scroll_top = USER_ITEM_GRID_ROW_STEP_PX * 10.0 + 20.0;

        assert_eq!(
            user_item_grid_visible_rows(100, scroll_top, 640.0, 0),
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
        );

        // Expanding at the bottom clamps the top row from 15 to 10. Include
        // that new viewport plus only the normal one-row overscan.
        assert_eq!(visible, 9..20);
    }
}
