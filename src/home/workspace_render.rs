use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, canvas, deferred, div,
    prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{SortOrder, UserItem, UserItemsSort},
    theme,
};

use super::{
    HomeContent,
    carousel::HOME_MAIN_SCROLLBAR_WIDTH_PX,
    components::{user_episode_card, user_item_card},
    library::{LibraryState, available_library_sorts},
    navigation::HomeRoute,
    paged_items::PagedItemsState,
};

const LIBRARY_AUTO_LOAD_MIN_THRESHOLD_PX: f32 = 480.0;
const LIBRARY_FIXED_HEADER_HEIGHT_PX: f32 = 64.0;
const LIBRARY_SORT_SELECT_WIDTH_PX: f32 = 232.0;
const LIBRARY_SORT_OPTION_HEIGHT_PX: f32 = 30.0;
const LIBRARY_SORT_ORDERS: [SortOrder; 2] = [SortOrder::Ascending, SortOrder::Descending];

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
        let retry_refresh = cx.listener(|page, _, _, cx| page.retry_current_library(cx));
        let retry_initial = cx.listener(|page, _, _, cx| page.retry_current_library(cx));

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
                    .when_some(state.paged.refresh_error.clone(), |this, error| {
                        this.child(self.render_inline_error(
                            error,
                            "retry-library-refresh",
                            retry_refresh,
                            cx,
                        ))
                    })
                    .when(
                        state.paged.initial.is_loading() && state.paged.items.is_empty(),
                        |this| this.child(self.render_center_message("加载媒体库中…", false, cx)),
                    )
                    .when_some(
                        state
                            .paged
                            .initial_error
                            .clone()
                            .filter(|_| state.paged.items.is_empty()),
                        |this, error| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_start()
                                    .gap_3()
                                    .child(div().text_sm().text_color(theme.error).child(error))
                                    .child(self.render_text_action(
                                        "retry-library-initial",
                                        "重试",
                                        retry_initial,
                                        cx,
                                    )),
                            )
                        },
                    )
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
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let retry_refresh = cx.listener(|page, _, _, cx| page.retry_favorites(cx));
        let retry_initial = cx.listener(|page, _, _, cx| page.retry_favorites(cx));
        let load_more = cx.listener(|page, _, _, cx| page.load_more_favorites(cx));
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-favorites-content")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&self.favorites.scroll_handle)
            .p_6()
            .when_some(self.favorites.refresh_error.clone(), |this, error| {
                this.child(self.render_inline_error(
                    error,
                    "retry-favorites-refresh",
                    retry_refresh,
                    cx,
                ))
            })
            .when(
                self.favorites.initial.is_loading() && self.favorites.items.is_empty(),
                |this| this.child(self.render_center_message("加载收藏中…", false, cx)),
            )
            .when_some(
                self.favorites
                    .initial_error
                    .clone()
                    .filter(|_| self.favorites.items.is_empty()),
                |this, error| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_3()
                            .child(div().text_sm().text_color(theme.error).child(error))
                            .child(self.render_text_action(
                                "retry-favorites-initial",
                                "重试",
                                retry_initial,
                                cx,
                            )),
                    )
                },
            )
            .when(
                self.favorites.initial != super::LoadState::Loading
                    && self.favorites.initial_error.is_none()
                    && self.favorites.items.is_empty(),
                |this| this.child(self.render_center_message("暂无收藏的电影或剧集", false, cx)),
            )
            .when(!self.favorites.items.is_empty(), |this| {
                this.child(self.render_items_grid(&self.favorites.items, "favorite-grid-item", cx))
                    .child(self.render_paged_footer(
                        &self.favorites,
                        "favorites-load-more",
                        load_more,
                        cx,
                    ))
            })
    }

    pub(super) fn render_search_scrollable_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let retry = cx.listener(|page, _, _, cx| page.retry_search(cx));
        let load_more = cx.listener(|page, _, _, cx| page.load_more_search(cx));
        let load_more_empty = cx.listener(|page, _, _, cx| page.load_more_search(cx));
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-search-content")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&self.search.scroll_handle)
            .p_6()
            .child(div().mb_5().w_full().child(self.search_input.clone()))
            .when(
                !self.search.query.is_empty()
                    && self.search.initial.is_loading()
                    && self.search.items.is_empty(),
                |this| this.child(self.render_center_message("搜索中…", false, cx)),
            )
            .when_some(self.search.initial_error.clone(), |this, error| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_3()
                        .child(div().text_sm().text_color(theme.error).child(error))
                        .child(self.render_text_action("retry-search-initial", "重试", retry, cx)),
                )
            })
            .when(
                !self.search.query.is_empty()
                    && self.search.initial == super::LoadState::Loaded
                    && self.search.items.is_empty()
                    && self.search.exhausted,
                |this| this.child(self.render_center_message("未找到相关电影或剧集", false, cx)),
            )
            .when(
                self.search.items.is_empty() && self.search.can_load_more(),
                |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_2()
                            .when_some(self.search.load_more_error.clone(), |this, error| {
                                this.child(div().text_sm().text_color(theme.error).child(error))
                            })
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("当前页没有可展示的结果"),
                            )
                            .child(self.render_text_action(
                                "search-load-more-empty-action",
                                "加载更多",
                                load_more_empty,
                                cx,
                            )),
                    )
                },
            )
            .when(
                self.search.items.is_empty() && self.search.load_more.is_loading(),
                |this| this.child(self.render_center_message("加载更多中…", false, cx)),
            )
            .when(!self.search.items.is_empty(), |this| {
                this.child(self.render_items_grid(&self.search.items, "search-grid-item", cx))
                    .child(
                        div()
                            .mt_5()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .when_some(self.search.load_more_error.clone(), |this, error| {
                                this.child(div().text_sm().text_color(theme.error).child(error))
                            })
                            .when(self.search.load_more.is_loading(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child("加载更多中…"),
                                )
                            })
                            .when(
                                self.search.can_load_more()
                                    || self.search.load_more == super::LoadState::Failed,
                                |this| {
                                    this.child(self.render_text_action(
                                        "search-load-more-action",
                                        "加载更多",
                                        load_more,
                                        cx,
                                    ))
                                },
                            ),
                    )
            })
    }

    fn render_items_grid(
        &self,
        items: &[UserItem],
        id_prefix: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_4()
            .children(items.iter().map(|item| {
                let item = self.effective_user_item(item);
                let item_id = item.id.clone();
                let open_item = item.clone();
                let open = cx.listener(move |page, _, _, cx| {
                    page.open_media_detail(&open_item, cx);
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
            }))
    }

    fn render_paged_footer(
        &self,
        state: &PagedItemsState,
        id: &'static str,
        load_more: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id(id)
            .mt_5()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .when_some(state.load_more_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when(state.load_more.is_loading(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("加载更多中…"),
                )
            })
            .when(
                state.can_load_more() || state.load_more == super::LoadState::Failed,
                |this| {
                    this.child(self.render_text_action(
                        gpui::ElementId::from((gpui::ElementId::from(id), "action")),
                        "加载更多",
                        load_more,
                        cx,
                    ))
                },
            )
    }

    fn render_library_paged_footer(
        &self,
        state: &PagedItemsState,
        view_id: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let retry = cx.listener(|page, _, _, cx| page.load_more_current_library(cx));
        let page = cx.entity().downgrade();
        let auto_load_view_id = view_id.to_string();
        let scroll_handle = state.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !library_scroll_is_near_end(&scroll_handle) {
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
            .when_some(state.load_more_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when(state.load_more.is_loading(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("加载更多中…"),
                )
            })
            .when(state.load_more == super::LoadState::Failed, |this| {
                this.child(self.render_text_action("library-load-more-retry", "重试", retry, cx))
            })
            .when(state.can_auto_load_more(), |this| {
                this.child(auto_load_observer)
            })
    }

    pub(super) fn render_inline_error(
        &self,
        error: gpui::SharedString,
        retry_id: impl Into<gpui::ElementId>,
        retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = theme::get(cx);
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_sm()
            .child(div().text_color(theme.error).child(error))
            .child(self.render_text_action(retry_id, "重试", retry, cx))
    }

    pub(super) fn render_text_action(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id(id.into())
            .rounded_md()
            .px_2()
            .py_1()
            .text_sm()
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.input_border_focused)
            .hover(move |style| style.bg(theme.secondary_hover))
            .cursor_pointer()
            .child(label)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(on_click)
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

fn library_scroll_is_near_end(scroll_handle: &ScrollHandle) -> bool {
    let scroll_top = -f32::from(scroll_handle.offset().y);
    let max_offset = f32::from(scroll_handle.max_offset().height);
    let viewport_height = f32::from(scroll_handle.bounds().size.height);
    library_scroll_position_is_near_end(scroll_top, max_offset, viewport_height)
}

fn library_scroll_position_is_near_end(
    scroll_top: f32,
    max_offset: f32,
    viewport_height: f32,
) -> bool {
    let max_offset = max_offset.max(0.0);
    let scroll_top = scroll_top.clamp(0.0, max_offset);
    let threshold = viewport_height.max(LIBRARY_AUTO_LOAD_MIN_THRESHOLD_PX);
    max_offset - scroll_top <= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_auto_loads_within_one_viewport_of_the_bottom() {
        assert!(!library_scroll_position_is_near_end(500.0, 2_000.0, 800.0));
        assert!(library_scroll_position_is_near_end(1_200.0, 2_000.0, 800.0));
        assert!(library_scroll_position_is_near_end(1_800.0, 2_000.0, 300.0));
    }

    #[test]
    fn library_auto_loads_again_when_content_does_not_fill_the_viewport() {
        assert!(library_scroll_position_is_near_end(0.0, 0.0, 800.0));
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
}
