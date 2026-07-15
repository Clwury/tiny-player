use gpui::{
    App, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, canvas, div, prelude::FluentBuilder,
    px,
};

use crate::{emby::UserItem, theme};

use super::{
    HomeContent,
    carousel::HOME_MAIN_SCROLLBAR_WIDTH_PX,
    components::{user_episode_card, user_item_card},
    navigation::HomeRoute,
    paged_items::PagedItemsState,
};

const LIBRARY_AUTO_LOAD_MIN_THRESHOLD_PX: f32 = 480.0;

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
        let retry_refresh = cx.listener(|page, _, _, cx| page.retry_current_library(cx));
        let retry_initial = cx.listener(|page, _, _, cx| page.retry_current_library(cx));

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-library-content")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&state.paged.scroll_handle)
            .px_6()
            .pt_4()
            .pb_6()
            .child(
                div()
                    .ml_10()
                    .mb_5()
                    .flex()
                    .h(px(32.0))
                    .items_center()
                    .when_some(total, |this, total| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(total),
                        )
                    }),
            )
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
                |this| this.child(self.render_center_message("该媒体库暂无内容", false, cx)),
            )
            .when(!state.paged.items.is_empty(), |this| {
                this.child(self.render_items_grid(&state.paged.items, "library-grid-item", cx))
                    .child(self.render_library_paged_footer(&state.paged, view_id, cx))
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
}
