use crate::home::components::workspace_back_button;

use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, Styled, Window, canvas, div,
    prelude::FluentBuilder, px,
};

use crate::theme;

use crate::home::{
    HomeContent,
    favorites::{favorite_section_title, view::favorite_action},
    navigation::HomeRoute,
};

use crate::home::grid::*;

impl HomeContent {
    pub(in crate::home) fn render_favorite_items_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let HomeRoute::FavoriteItems { item_type } = self.controller.route() else {
            unreachable!("favorite category renderer requires a FavoriteItems route");
        };
        let item_type = *item_type;
        let theme = theme::get(cx);
        let section = &self.favorites_presentation[item_type];
        let state = self.controller.favorite_section(item_type).paged;
        let presentation = &section.presentation;
        let page = cx.entity().downgrade();
        let scroll_handle = presentation.scroll_handle.clone();
        let observer = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| page.auto_load_more_favorites(item_type, cx))
                        .ok();
                });
            },
        )
        .absolute()
        .bottom_0()
        .left_0()
        .w_full()
        .h(px(1.0));
        let grid = self.render_virtual_items_grid(
            UserItemGridSource::Favorites(item_type),
            self.layout.view_model().grid_columns,
            presentation.scroll_handle.clone(),
            &presentation.grid_columns,
            f32::from(window.bounds().size.height),
            cx,
        );
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-favorite-items-content")
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .pt_3()
                    .pb_4()
                    .child(workspace_back_button(
                        cx.listener(Self::close_series_detail),
                        cx,
                    ))
                    .when(!state.items.is_empty(), |this| {
                        this.child(
                            div()
                                .debug_selector(|| "favorite-items-title".to_string())
                                .text_lg()
                                .text_color(theme.foreground)
                                .child(favorite_section_title(item_type)),
                        )
                        .when_some(
                            state.total_record_count,
                            |this, total| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("共 {total} 项")),
                                )
                            },
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .px_6()
                    .pb_6()
                    .when(!state.items.is_empty(), |this| this.child(grid))
                    .when(
                        state.can_auto_load_more() && self.layout.view_model().auto_paginate,
                        |this| this.child(observer),
                    ),
            )
            .when(state.initial == crate::home::LoadState::Failed, |this| {
                this.child(favorite_action(
                    "favorite-items-retry".into(),
                    "重试",
                    cx,
                    cx.listener(move |page, _, _, cx| page.load_favorites_initial(item_type, cx)),
                ))
            })
            .when(state.load_more == crate::home::LoadState::Failed, |this| {
                this.child(favorite_action(
                    "favorite-items-load-more-retry".into(),
                    "重试加载更多",
                    cx,
                    cx.listener(move |page, _, _, cx| page.load_more_favorites(item_type, cx)),
                ))
            })
    }
}
