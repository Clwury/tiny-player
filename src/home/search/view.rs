use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    Window, canvas, div, prelude::FluentBuilder, px, svg,
};

use crate::theme;
use crate::ui::radius;

use crate::home::HomeContent;

use crate::home::grid::*;

impl HomeContent {
    pub(in crate::home) fn render_search_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let page = cx.entity().downgrade();
        let scroll_handle = self.search_presentation.grid.scroll_handle.clone();
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
            self.layout.view_model().grid_columns,
            self.search_presentation.grid.scroll_handle.clone(),
            &self.search_presentation.grid.grid_columns,
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
                    .debug_selector(|| "home-search-fixed-header".into())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_none()
                    .bg(theme.background)
                    .px_6()
                    .pt_6()
                    .pb_5()
                    .child(div().w_full().child(self.search_input.clone()))
                    .when(
                        !self.controller.search_view().history.entries().is_empty(),
                        |header| header.child(self.render_search_history(cx)),
                    ),
            )
            .child(
                div()
                    .id("home-search-scroll-content")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .px_6()
                    .pb_6()
                    .when(self.controller.search_view().empty_results, |this| {
                        this.child(self.render_center_message("未找到相关电影或剧集", false, cx))
                    })
                    .when(!self.controller.search_view().items.is_empty(), |this| {
                        this.child(grid)
                    })
                    .when(
                        self.controller.search_view().can_load_more
                            && self.layout.view_model().auto_paginate,
                        |this| this.child(auto_load_observer),
                    ),
            )
    }
    pub(in crate::home) fn render_search_history(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id("search-history")
            .debug_selector(|| "search-history".into())
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("搜索历史")
                    .child(
                        div()
                            .id("search-history-clear")
                            .debug_selector(|| "search-history-clear".into())
                            .flex()
                            .size(px(28.0))
                            .items_center()
                            .justify_center()
                            .rounded(radius::CONTROL)
                            .cursor_pointer()
                            .hover(move |style| {
                                style.bg(theme.secondary_hover).text_color(theme.foreground)
                            })
                            .tooltip(|_, cx| crate::ui::tooltip::text_tooltip("清空搜索历史", cx))
                            .child(
                                svg()
                                    .path("icons/trash.svg")
                                    .size(px(16.0))
                                    .text_color(theme.muted_foreground),
                            )
                            .on_click(cx.listener(|page, _, _, cx| {
                                cx.stop_propagation();
                                page.clear_search_history(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("search-history-tags")
                    .debug_selector(|| "search-history-tags".into())
                    .max_h(px(128.0))
                    .overflow_y_scroll()
                    .child(
                        div().flex().flex_wrap().gap_2().children(
                            self.controller
                                .search_view()
                                .history
                                .entries()
                                .iter()
                                .enumerate()
                                .map(|(index, query)| {
                                    let query = query.clone();
                                    let tooltip = query.clone();
                                    div()
                                        .id(("search-history-tag", index))
                                        .debug_selector(move || {
                                            format!("search-history-tag-{index}")
                                        })
                                        .max_w(px(240.0))
                                        .min_w_0()
                                        .px_2()
                                        .py_1()
                                        .rounded(radius::CONTROL)
                                        .bg(theme.secondary_hover)
                                        .text_sm()
                                        .text_color(theme.foreground)
                                        .cursor_pointer()
                                        .hover(move |style| {
                                            style
                                                .bg(theme.element_selected)
                                                .text_color(theme.accent_text)
                                        })
                                        .active(move |style| style.bg(theme.element_selected_hover))
                                        .tooltip(move |_, cx| {
                                            crate::ui::tooltip::text_tooltip(tooltip.clone(), cx)
                                        })
                                        .child(div().text_ellipsis().child(query.clone()))
                                        .on_click(cx.listener(move |page, _, window, cx| {
                                            cx.stop_propagation();
                                            page.search_from_history(&query, window, cx);
                                        }))
                                }),
                        ),
                    ),
            )
    }
}
