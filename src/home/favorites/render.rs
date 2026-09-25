use gpui::{
    App, ClickEvent, Context, ElementId, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::ui::radius;
use crate::{emby::VideoItemType, theme};

use super::super::{
    HomeContent, LoadState,
    carousel::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_MAIN_SCROLLBAR_WIDTH_PX,
        carousel_content_width_for, carousel_visible_range_between_for, home_main_content_width,
        max_carousel_scroll_offset_for,
    },
    components::{carousel_button, home_section_more_button, home_section_title},
    render::home_carousel_track,
    workspace_render::UserItemGridSource,
};
use super::{FAVORITE_ITEM_TYPES, FAVORITES_PAGE_LIMIT, favorite_section_title};

impl HomeContent {
    pub(in crate::home) fn render_favorites_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let width = home_main_content_width(window);
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
            .flex()
            .flex_col()
            .gap_8()
            .children(
                FAVORITE_ITEM_TYPES
                    .into_iter()
                    .filter(|item_type| {
                        let state = &self.favorites[*item_type].paged;
                        !state.items.is_empty() || state.initial == LoadState::Failed
                    })
                    .map(|item_type| self.render_favorite_section(item_type, width, cx)),
            )
    }

    fn render_favorite_section(
        &self,
        item_type: VideoItemType,
        width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let state = &self.favorites[item_type].paged;
        let has_items = !state.items.is_empty();
        let title = favorite_section_title(item_type);
        div()
            .id((ElementId::from("favorite-section"), item_type.as_str()))
            .debug_selector(move || format!("favorite-section-{}", item_type.as_str()))
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .mb_3()
                    .child(home_section_title(title, cx))
                    .when(has_items, |this| {
                        this.child(
                            home_section_more_button(
                                ElementId::from(format!("favorite-more-{}", item_type.as_str())),
                                cx,
                            )
                            .debug_selector(move || format!("favorite-more-{}", item_type.as_str()))
                            .on_click(cx.listener(
                                move |page, _, _, cx| page.open_favorite_items(item_type, cx),
                            )),
                        )
                    }),
            )
            .when(has_items, |this| {
                this.child(self.render_favorite_row(item_type, width, cx))
            })
            .when(state.initial == LoadState::Failed, |this| {
                this.child(favorite_action(
                    format!("favorite-retry-{}", item_type.as_str()),
                    "重试",
                    cx,
                    cx.listener(move |page, _, _, cx| page.load_favorites_initial(item_type, cx)),
                ))
            })
    }

    fn render_favorite_row(
        &self,
        item_type: VideoItemType,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let source = UserItemGridSource::Favorites(item_type);
        let card_width = source.card_width();
        let section = &self.favorites[item_type];
        let count = section.paged.items.len().min(FAVORITES_PAGE_LIMIT as usize);
        let width = viewport_width.min(carousel_content_width_for(
            count,
            card_width,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        ));
        let max_offset = max_carousel_scroll_offset_for(
            count,
            width,
            card_width,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        );
        let carousel = section.carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible = carousel_visible_range_between_for(
            count,
            (previous_offset, offset),
            width,
            card_width,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
            if self.resize_in_progress {
                (0, 0)
            } else {
                (2, 4)
            },
        );
        let controls_visible = carousel.controls_visible(max_offset > 0.0);

        div()
            .id((ElementId::from("favorite-row"), item_type.as_str()))
            .debug_selector(move || format!("favorite-row-{}", item_type.as_str()))
            .relative()
            .w(px(width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(cx.listener(move |page, hovered, _, cx| {
                page.set_favorite_row_hovered(item_type, *hovered, false, cx)
            }))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .when(visible.leading_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible.leading_width)))
                    })
                    .children((visible.start..visible.end).map(|index| {
                        self.render_user_item_grid_card(
                            &section.paged.items[index],
                            index,
                            source,
                            "favorite-row-item",
                            cx,
                        )
                    }))
                    .when(visible.trailing_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible.trailing_width)))
                    })
                    .map(|track| {
                        home_carousel_track(
                            track,
                            (
                                ElementId::from("favorite-row-scroll"),
                                format!("{}-{}", item_type.as_str(), carousel.animation_id()),
                            ),
                            previous_offset,
                            offset,
                        )
                    }),
            )
            .when(max_offset > 0.0, |this| {
                this.child(carousel_button(
                    "favorite-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    cx.listener(move |page, hovered, _, cx| {
                        page.set_favorite_row_hovered(item_type, *hovered, true, cx)
                    }),
                    cx.listener(move |page, _, window, cx| {
                        page.scroll_favorite_row(item_type, -1.0, window, cx)
                    }),
                ))
                .child(carousel_button(
                    "favorite-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    cx.listener(move |page, hovered, _, cx| {
                        page.set_favorite_row_hovered(item_type, *hovered, true, cx)
                    }),
                    cx.listener(move |page, _, window, cx| {
                        page.scroll_favorite_row(item_type, 1.0, window, cx)
                    }),
                ))
            })
    }

    fn set_favorite_row_hovered(
        &mut self,
        item_type: VideoItemType,
        hovered: bool,
        controls: bool,
        cx: &mut Context<Self>,
    ) {
        let carousel = &mut self.favorites[item_type].carousel;
        let changed = if controls {
            carousel.set_controls_hovered(hovered)
        } else {
            carousel.set_hovered(hovered)
        };
        if changed {
            cx.notify();
        }
    }

    fn scroll_favorite_row(
        &mut self,
        item_type: VideoItemType,
        direction: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let width = home_main_content_width(window);
        let card_width = UserItemGridSource::Favorites(item_type).card_width();
        let section = &mut self.favorites[item_type];
        let count = section.paged.items.len().min(FAVORITES_PAGE_LIMIT as usize);
        let max_offset = max_carousel_scroll_offset_for(
            count,
            width,
            card_width,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        );
        let step = (card_width + HOME_ITEM_CARD_PADDING_PX * 2.0 + HOME_ITEM_CARD_GAP_PX)
            * if item_type == VideoItemType::Episode {
                3.0
            } else {
                4.0
            };
        let offset = section.carousel.scroll_offset(max_offset) + direction * step;
        if section.carousel.set_scroll_offset(offset, max_offset) {
            cx.notify();
        }
    }
}

pub(in crate::home) fn favorite_action(
    id: String,
    label: &'static str,
    cx: &Context<HomeContent>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let selector = id.clone();
    div()
        .id(ElementId::from(id))
        .debug_selector(move || selector.clone())
        .flex()
        .flex_none()
        .h(px(28.0))
        .px_2()
        .items_center()
        .justify_center()
        .rounded(radius::CONTROL)
        .text_sm()
        .text_color(theme.foreground)
        .hover(move |style| style.bg(theme.secondary_hover))
        .cursor_pointer()
        .child(label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
}
