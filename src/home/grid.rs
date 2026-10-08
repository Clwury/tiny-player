use std::ops::Range;

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, div, point, px,
};

use crate::{
    emby::{UserItem, VideoItemType},
    theme,
};

use crate::home::{
    HomeContent,
    carousel::{
        DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX, DETAIL_EPISODE_CARD_WIDTH_PX, HOME_ITEM_CARD_GAP_PX,
        HOME_ITEM_CARD_IMAGE_HEIGHT_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_GUTTER_PX, home_main_content_width,
    },
    components::{favorite_episode_card, user_episode_card, user_item_card},
    item_context_menu::ItemContextMenuSource,
    navigation::HomeRoute,
};

const WORKSPACE_AUTO_LOAD_MIN_THRESHOLD_PX: f32 = 480.0;
const USER_ITEM_GRID_ROW_HEIGHT_PX: f32 = 302.0;
const USER_ITEM_GRID_ROW_STEP_PX: f32 = USER_ITEM_GRID_ROW_HEIGHT_PX + HOME_ITEM_CARD_GAP_PX;

#[derive(Clone, Copy)]
pub(super) enum UserItemGridSource {
    Favorites(VideoItemType),
    Search,
}

impl UserItemGridSource {
    pub(super) fn card_width(self) -> f32 {
        match self {
            Self::Favorites(VideoItemType::Episode) => DETAIL_EPISODE_CARD_WIDTH_PX,
            _ => HOME_ITEM_CARD_WIDTH_PX,
        }
    }

    fn row_step(self) -> f32 {
        match self {
            // Episode covers are 16:9, with the same padding and two text lines.
            Self::Favorites(VideoItemType::Episode) => {
                USER_ITEM_GRID_ROW_STEP_PX - HOME_ITEM_CARD_IMAGE_HEIGHT_PX
                    + DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX
            }
            _ => USER_ITEM_GRID_ROW_STEP_PX,
        }
    }

    fn scroll_id(self) -> &'static str {
        match self {
            Self::Favorites(_) => "favorite-grid-scroll",
            Self::Search => "search-grid-scroll",
        }
    }

    fn card_id_prefix(self) -> &'static str {
        match self {
            Self::Favorites(_) => "favorite-grid-item",
            Self::Search => "search-grid-item",
        }
    }
}

impl HomeContent {
    pub(in crate::home) fn render_items_grid(
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
    pub(in crate::home) fn render_virtual_items_grid(
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
            UserItemGridSource::Favorites(item_type) => self
                .controller
                .favorite_section(item_type)
                .paged
                .items
                .len(),
            UserItemGridSource::Search => self.controller.search_view().items.len(),
        };
        let row_count = user_item_grid_row_count(item_count, columns);
        let row_step = source.row_step();
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
            row_step,
        );
        // The tracked bounds describe the previous frame during a native resize,
        // while `viewport_height` already describes the new window. Use the new
        // height when choosing rows: the helper anticipates GPUI's upcoming scroll
        // clamp on expansion and avoids building the old tall viewport on shrink.
        let overscan_rows = self.layout.view_model().grid_overscan_rows;
        let visible_rows = user_item_grid_visible_rows(
            row_count,
            scroll_top,
            viewport_height,
            overscan_rows,
            row_step,
        );
        let total_height = row_count as f32 * row_step;
        let grid_width = user_item_grid_content_width(columns, source.card_width());

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
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_GUTTER_PX))
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
                        .top(px(row as f32 * row_step))
                        .left_0()
                    })),
            )
    }
    pub(in crate::home) fn render_user_item_grid_row(
        &self,
        source: UserItemGridSource,
        row: usize,
        columns: usize,
        grid_width: f32,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let items = match source {
            UserItemGridSource::Favorites(item_type) => {
                &self.controller.favorite_section(item_type).paged.items
            }
            UserItemGridSource::Search => self.controller.search_view().items,
        };
        let start = row.saturating_mul(columns.max(1));
        if start >= items.len() {
            // A request can finish between the render and prepaint passes (for
            // example when a search query is replaced). Keep the uniform row
            // height stable instead of slicing past the newly shortened list.
            return div().w(px(grid_width)).h(px(source.row_step()));
        }
        let end = (start + columns.max(1)).min(items.len());
        div()
            .w(px(grid_width))
            .h(px(source.row_step()))
            .flex()
            .flex_none()
            .items_start()
            .gap_4()
            .children(items[start..end].iter().enumerate().map(|(offset, item)| {
                self.render_user_item_grid_card(item, start + offset, source, id_prefix, cx)
            }))
    }
    pub(in crate::home) fn render_user_item_grid_card(
        &self,
        item: &UserItem,
        index: usize,
        source: UserItemGridSource,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let item_fingerprint = user_item_id_fingerprint(&item.id);
        let open = cx.listener(move |page, _, _, cx| {
            page.open_user_item_grid_index(source, index, item_fingerprint, cx);
        });
        let open_context_menu = cx.listener(move |page, event: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            if let Some(item_id) = page.user_item_grid_id(source, index, item_fingerprint) {
                page.open_item_context_menu(
                    item_id,
                    ItemContextMenuSource::UserItem,
                    event.position,
                    cx,
                );
            }
        });
        // Keep element identity stable for an item without cloning its ID into
        // every resize-built listener. The fingerprint is also checked by the
        // click handler so a late event from a removed/reordered result cannot
        // open the wrong item.
        let item_id = gpui::ElementId::from((id_prefix, item_fingerprint));
        let card = if item.item_type.as_deref() == Some("Episode") {
            let card = if matches!(source, UserItemGridSource::Favorites(_)) {
                favorite_episode_card(
                    self.controller.user_episode_card_vm(item),
                    self.image_path_for_favorite_episode(item),
                    cx,
                )
            } else {
                user_episode_card(
                    self.controller.user_episode_card_vm(item),
                    self.image_path_for_episode_user_item(item),
                    cx,
                )
            };
            card.id(item_id)
        } else {
            let image_path = self.image_path_for_user_item(item);
            user_item_card(
                self.controller
                    .user_item_card_vm(item, !matches!(source, UserItemGridSource::Favorites(_))),
                image_path,
                cx,
            )
            .id(item_id)
        };
        card.debug_selector(|| format!("{id_prefix}-{}", item.id))
            .cursor_pointer()
            .on_click(open)
            .on_mouse_down(MouseButton::Right, open_context_menu)
    }
    pub(in crate::home) fn open_user_item_grid_index(
        &mut self,
        source: UserItemGridSource,
        index: usize,
        expected_fingerprint: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(item_id) = self.user_item_grid_id(source, index, expected_fingerprint) {
            self.open_media_detail_by_id(item_id, cx);
        }
    }
    pub(in crate::home) fn user_item_grid_id(
        &self,
        source: UserItemGridSource,
        index: usize,
        expected_fingerprint: u64,
    ) -> Option<String> {
        match source {
            UserItemGridSource::Favorites(item_type) => self
                .controller
                .favorite_section(item_type)
                .paged
                .items
                .get(index),
            UserItemGridSource::Search => self.controller.search_view().items.get(index),
        }
        .filter(|item| user_item_id_fingerprint(&item.id) == expected_fingerprint)
        .map(|item| item.id.clone())
    }
    pub(in crate::home) fn render_user_item_card(
        &self,
        item: &UserItem,
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let item_id = item.id.clone();
        let open_item_id = item_id.clone();
        let context_item_id = item_id.clone();
        let open_context_menu = cx.listener(move |page, event: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            page.open_item_context_menu(
                context_item_id.clone(),
                ItemContextMenuSource::UserItem,
                event.position,
                cx,
            );
        });
        let open = cx.listener(move |page, _, _, cx| {
            page.open_media_detail_by_id(open_item_id.clone(), cx);
        });
        let card = if item.item_type.as_deref() == Some("Episode") {
            let image_path = self.image_path_for_episode_user_item(item);
            user_episode_card(self.controller.user_episode_card_vm(item), image_path, cx)
                .id((gpui::ElementId::from(id_prefix), item_id.clone()))
        } else {
            let image_path = self.image_path_for_user_item(item);
            user_item_card(
                self.controller.user_item_card_vm(item, true),
                image_path,
                cx,
            )
            .id((gpui::ElementId::from(id_prefix), item_id.clone()))
        };
        card.debug_selector(move || format!("{id_prefix}-{item_id}"))
            .cursor_pointer()
            .on_click(open)
            .on_mouse_down(MouseButton::Right, open_context_menu)
    }
    pub(in crate::home) fn render_center_message(
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

pub(in crate::home) fn user_item_id_fingerprint(item_id: &str) -> u64 {
    // FNV-1a is sufficient for an element key here and avoids allocating a
    // temporary `String` on every visible card. The click path validates the
    // same fingerprint against the current item before navigating.
    item_id.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        hash.wrapping_mul(0x100000001b3) ^ u64::from(byte)
    })
}

pub(in crate::home) fn sync_workspace_grid_scroll(
    scroll_handle: &ScrollHandle,
    previous_columns: &std::cell::Cell<usize>,
    columns: usize,
    row_count: usize,
    viewport_height: f32,
    row_step: f32,
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
        row_step,
    );
    previous_columns.set(columns);

    if (old_scroll_top - scroll_top).abs() >= 0.5 {
        scroll_handle.set_offset(point(offset.x, px(-scroll_top)));
    }

    scroll_top
}

pub(in crate::home) fn remap_workspace_grid_scroll_top(
    old_scroll_top: f32,
    old_columns: usize,
    columns: usize,
    row_count: usize,
    viewport_height: f32,
    row_step: f32,
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
    let old_row = (old_scroll_top / row_step).floor() as usize;
    let offset_in_row = old_scroll_top - old_row as f32 * row_step;
    let first_visible_item = old_row.saturating_mul(old_columns);
    let new_row = (first_visible_item / columns).min(row_count - 1);
    let target = if old_columns == columns {
        old_scroll_top
    } else {
        new_row as f32 * row_step + offset_in_row
    };
    let viewport_height = if viewport_height.is_finite() {
        viewport_height.max(0.0)
    } else {
        0.0
    };
    let max_scroll = (row_count as f32 * row_step - viewport_height).max(0.0);
    target.clamp(0.0, max_scroll)
}

pub(in crate::home) fn user_item_grid_visible_rows(
    row_count: usize,
    scroll_top: f32,
    viewport_height: f32,
    overscan_rows: usize,
    row_step: f32,
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
    let content_height = row_count as f32 * row_step;
    let scroll_top = scroll_top.min((content_height - viewport_height).max(0.0));
    let first = (scroll_top / row_step).floor() as usize;
    let last = ((scroll_top + viewport_height) / row_step).ceil() as usize;

    first.saturating_sub(overscan_rows).min(row_count)
        ..last.saturating_add(overscan_rows).min(row_count)
}

pub(in crate::home) fn user_item_grid_columns(window: &Window, route: &HomeRoute) -> usize {
    let source = match route {
        HomeRoute::FavoriteItems { item_type } => UserItemGridSource::Favorites(*item_type),
        _ => UserItemGridSource::Search,
    };
    user_item_grid_columns_for_width(home_main_content_width(window), source.card_width())
}

pub(in crate::home) fn responsive_user_item_grid_columns(measured: usize) -> usize {
    // Card positions only change when a full card-width threshold is crossed.
    // Applying that meaningful change immediately keeps expansion responsive;
    // fixed-height visible-row virtualization keeps each threshold cheap.
    measured.max(1)
}

pub(in crate::home) fn user_item_grid_columns_for_width(
    available_width: f32,
    card_width: f32,
) -> usize {
    let card_outer_width = card_width + HOME_ITEM_CARD_PADDING_PX * 2.0;
    ((available_width.max(0.0) + HOME_ITEM_CARD_GAP_PX)
        / (card_outer_width + HOME_ITEM_CARD_GAP_PX))
        .floor()
        .max(1.0) as usize
}

pub(in crate::home) fn user_item_grid_row_count(item_count: usize, columns: usize) -> usize {
    item_count.div_ceil(columns.max(1))
}

pub(in crate::home) fn user_item_grid_content_width(columns: usize, card_width: f32) -> f32 {
    let columns = columns.max(1);
    let card_outer_width = card_width + HOME_ITEM_CARD_PADDING_PX * 2.0;
    columns as f32 * card_outer_width + columns.saturating_sub(1) as f32 * HOME_ITEM_CARD_GAP_PX
}

pub(in crate::home) fn workspace_scroll_is_near_end(scroll_handle: &ScrollHandle) -> bool {
    let scroll_top = -f32::from(scroll_handle.offset().y);
    let max_offset = f32::from(scroll_handle.max_offset().y);
    let viewport_height = f32::from(scroll_handle.bounds().size.height);
    workspace_scroll_position_is_near_end(scroll_top, max_offset, viewport_height)
}

pub(in crate::home) fn workspace_scroll_position_is_near_end(
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
mod tests;
