//! Dashboard and carousel composition over feed/card view models.
use gpui::{
    App, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};

use crate::{
    emby::{ResumeItems, UserItems, UserView, UserViews},
    theme,
};

use crate::home::{
    HomeContent,
    carousel::{
        CarouselVisibleRange, HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX,
        HOME_ITEM_CARD_WIDTH_PX, HOME_MAIN_SCROLLBAR_WIDTH_PX, USER_VIEW_CARD_GAP_PX,
        USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX, carousel_content_width,
        carousel_content_width_for, carousel_visible_range_between_for, max_carousel_scroll_offset,
        max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, home_section_more_button, home_section_title, home_section_title_text,
        resume_item_card, user_episode_card, user_item_card, user_view_card,
    },
    item_context_menu::ItemContextMenuSource,
    visible_row::VisibleRow,
};

use crate::home::components::{home_carousel_track, tallest_home_item};

impl HomeContent {
    pub(in crate::home) fn render_main_scrollable_content(
        &self,
        main_content_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let vm = self.controller.feed_view();
        let show_user_views_section = vm.show_views;
        let show_resume_section = vm.show_resume;

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-main-content")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&self.home_scroll_handle)
            .p_6()
            .when(show_user_views_section, |this| {
                this.child(
                    div()
                        .child(
                            div()
                                .mb_3()
                                .text_lg()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.foreground)
                                .child("我的媒体"),
                        )
                        .when_some(vm.views, |this, views| {
                            this.child(self.render_user_views_row(views, main_content_width, cx))
                        })
                        .when(vm.show_empty_views, |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("暂无可浏览的视频媒体库"),
                            )
                        }),
                )
            })
            .when(show_resume_section, |this| {
                this.child(
                    div()
                        .child(
                            home_section_title("继续观看", cx)
                                .mb_3()
                                .when(show_user_views_section, |this| this.mt_8()),
                        )
                        .when_some(vm.resume, |this, items| {
                            this.child(self.render_resume_items_row(items, main_content_width, cx))
                        })
                        .when(vm.missing_episode_series, |this| {
                            this.child(
                                div()
                                    .mt_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("部分单集缺少剧集信息，暂时无法打开"),
                            )
                        }),
                )
            })
            .when_some(vm.views, |this, views| {
                this.children(
                    views.items.iter().map(|view| {
                        self.render_user_view_items_section(view, main_content_width, cx)
                    }),
                )
            })
    }

    pub(in crate::home) fn render_user_views_row(
        &self,
        views: &UserViews,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width(views.items.len()));
        let max_offset = max_carousel_scroll_offset(views.items.len(), viewport_width);
        let carousel = self.user_views_carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            views.items.len(),
            (previous_offset, offset),
            viewport_width,
            USER_VIEW_CARD_WIDTH_PX,
            USER_VIEW_CARD_PADDING_PX,
            USER_VIEW_CARD_GAP_PX,
            self.layout.view_model().carousel_overscan,
        );
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let on_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_user_views_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_user_views_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_user_views_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_user_views_left);
        let scroll_right = cx.listener(Self::scroll_user_views_right);

        div()
            .id("user-views-row")
            .debug_selector(|| "user-views-row".into())
            .relative()
            .group("user-views-row")
            .w(px(viewport_width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(on_hover)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .when(visible_range.leading_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.leading_width)))
                    })
                    .children(
                        views.items[visible_range.start..visible_range.end]
                            .iter()
                            .map(|view| {
                                let image_path = self.image_path_for_primary_image(
                                    &view.id,
                                    view.image_tags
                                        .as_ref()
                                        .and_then(|tags| tags.primary.as_deref()),
                                );
                                let item_id = view.id.clone();
                                let card_name = view.name.clone();
                                let open_view_id = item_id.clone();
                                let on_click = cx.listener(move |page, _, _, cx| {
                                    page.open_library_by_id(&open_view_id, cx);
                                });
                                user_view_card(card_name, image_path, cx)
                                    .id((gpui::ElementId::from("user-view-card"), item_id))
                                    .cursor_pointer()
                                    .on_click(on_click)
                            }),
                    )
                    .when(visible_range.trailing_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.trailing_width)))
                    })
                    .map(|track| {
                        home_carousel_track(
                            track,
                            ("user-views-scroll", carousel.animation_id()),
                            previous_offset,
                            offset,
                        )
                    }),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-views-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    left_controls_hover,
                    scroll_left,
                ))
            })
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-views-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }

    pub(in crate::home) fn render_resume_items_row(
        &self,
        items: &ResumeItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width(items.items.len()));
        let max_offset = max_carousel_scroll_offset(items.items.len(), viewport_width);
        let carousel = self.resume_items_carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            items.items.len(),
            (previous_offset, offset),
            viewport_width,
            USER_VIEW_CARD_WIDTH_PX,
            USER_VIEW_CARD_PADDING_PX,
            USER_VIEW_CARD_GAP_PX,
            self.layout.view_model().carousel_overscan,
        );
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let on_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_resume_items_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_resume_items_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_resume_items_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_resume_items_left);
        let scroll_right = cx.listener(Self::scroll_resume_items_right);

        div()
            .id("resume-items-row")
            .debug_selector(|| "resume-items-row".into())
            .relative()
            .group("resume-items-row")
            .w(px(viewport_width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(on_hover)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .when(visible_range.leading_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.leading_width)))
                    })
                    .children(
                        items.items[visible_range.start..visible_range.end]
                            .iter()
                            .map(|item| {
                                let image_path = item
                                    .image_source()
                                    .and_then(|source| self.image_path_for_resume_image(source));
                                let item_id = item.id.clone();
                                let context_item_id = item_id.clone();
                                let card_item_id = item_id.clone();
                                let pending = self.controller.resume_item_pending(&item_id);
                                let open_context_menu = cx.listener(
                                    move |page: &mut HomeContent, event: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        page.open_item_context_menu(
                                            context_item_id.clone(),
                                            ItemContextMenuSource::Resume,
                                            event.position,
                                            cx,
                                        );
                                    },
                                );
                                let card = resume_item_card(
                                    self.controller.resume_card_vm(item),
                                    image_path,
                                    cx,
                                )
                                .id((
                                    gpui::ElementId::from("resume-item-card"),
                                    card_item_id.clone(),
                                ))
                                .debug_selector(move || format!("resume-item-card-{card_item_id}"))
                                .when(pending, |this| this.opacity(0.62))
                                .on_mouse_down(MouseButton::Right, open_context_menu);

                                let navigable = item.item_type.as_deref() == Some("Movie")
                                    || (item.item_type.as_deref() == Some("Episode")
                                        && item
                                            .series_id
                                            .as_deref()
                                            .is_some_and(|id| !id.trim().is_empty()));
                                if navigable {
                                    let open_item_id = item_id.clone();
                                    let on_click =
                                        cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                            page.open_resume_item_detail_by_id(
                                                open_item_id.clone(),
                                                cx,
                                            );
                                        });
                                    card.cursor_pointer().on_click(on_click)
                                } else {
                                    card
                                }
                            }),
                    )
                    .when(visible_range.trailing_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.trailing_width)))
                    })
                    .map(|track| {
                        home_carousel_track(
                            track,
                            ("resume-items-scroll", carousel.animation_id()),
                            previous_offset,
                            offset,
                        )
                    }),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "resume-items-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    left_controls_hover,
                    scroll_left,
                ))
            })
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "resume-items-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }

    pub(in crate::home) fn render_user_view_items_section(
        &self,
        view: &UserView,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let row = self.controller.latest_row(&view.id);
        let items = row.items;
        let visible = row.visible;
        let title = view.name.to_string();
        let open_view_id = view.id.clone();
        let view_all_action_id =
            gpui::ElementId::from((gpui::ElementId::from("view-all"), view.id.clone()));
        let open_library = cx.listener(move |page, _, _, cx| {
            page.open_library_by_id(&open_view_id, cx);
        });
        div().when(visible, |this| {
            this.mt_8()
                .child(
                    div()
                        .mb_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .child(home_section_title_text(title, cx))
                        .child(
                            home_section_more_button(view_all_action_id, cx)
                                .debug_selector(|| format!("view-all-{}", view.id))
                                .on_click(open_library),
                        ),
                )
                .when_some(
                    items.filter(|items| !items.items.is_empty()),
                    |this, items| {
                        this.child(self.render_visible_user_view_items_row(
                            &view.id,
                            items,
                            viewport_width,
                            cx,
                        ))
                    },
                )
        })
    }

    pub(in crate::home) fn render_visible_user_view_items_row(
        &self,
        view_id: &str,
        items: &UserItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let viewport_width = viewport_width.min(carousel_content_width_for(
            items.items.len(),
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        ));
        let carousel = self
            .latest_carousels
            .get(view_id)
            .copied()
            .unwrap_or_default();
        let max_offset = max_carousel_scroll_offset_for(
            items.items.len(),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        );
        let range = carousel_visible_range_between_for(
            items.items.len(),
            (
                carousel.previous_scroll_offset(max_offset),
                carousel.scroll_offset(max_offset),
            ),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
            self.layout.view_model().carousel_overscan,
        );
        // A carousel's height comes from its tallest card. Measure just that
        // card, without loading artwork, so offscreen rows keep their exact
        // space without constructing their images, labels and event listeners.
        let measurement = div().w(px(viewport_width)).max_w_full().when_some(
            tallest_home_item(&items.items[range.start..range.end]),
            |this, item| {
                this.child(if item.item_type.as_deref() == Some("Episode") {
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
                })
            },
        );
        let content = cx.weak_entity();
        let view_id = view_id.to_owned();
        VisibleRow::new(measurement, move |_: &mut Window, cx: &mut App| {
            content
                .update(cx, |content, cx| {
                    let Some(items) = content.controller.latest_row(&view_id).items else {
                        return div().into_any_element();
                    };
                    content
                        .render_user_view_items_row(&view_id, items, viewport_width, range, cx)
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
        })
    }

    pub(in crate::home) fn render_user_view_items_row(
        &self,
        view_id: &str,
        items: &UserItems,
        viewport_width: f32,
        visible_range: CarouselVisibleRange,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let carousel = self
            .latest_carousels
            .get(view_id)
            .copied()
            .unwrap_or_default();
        let viewport_width = viewport_width.min(carousel_content_width_for(
            items.items.len(),
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        ));
        let max_offset = max_carousel_scroll_offset_for(
            items.items.len(),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        );
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let animation_id = carousel.animation_id();
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let hover_view_id = view_id.to_string();
        let on_hover = cx.listener(move |page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_user_view_items_hovered(&hover_view_id, *hovered, cx);
        });
        let left_hover_view_id = view_id.to_string();
        let left_controls_hover =
            cx.listener(move |page: &mut HomeContent, hovered: &bool, _, cx| {
                page.set_user_view_items_controls_hovered(&left_hover_view_id, *hovered, cx);
            });
        let right_hover_view_id = view_id.to_string();
        let right_controls_hover =
            cx.listener(move |page: &mut HomeContent, hovered: &bool, _, cx| {
                page.set_user_view_items_controls_hovered(&right_hover_view_id, *hovered, cx);
            });
        let left_scroll_view_id = view_id.to_string();
        let scroll_left = cx.listener(move |page: &mut HomeContent, _, window, cx| {
            page.scroll_user_view_items_left(&left_scroll_view_id, window, cx);
        });
        let right_scroll_view_id = view_id.to_string();
        let scroll_right = cx.listener(move |page: &mut HomeContent, _, window, cx| {
            page.scroll_user_view_items_right(&right_scroll_view_id, window, cx);
        });
        let animation_key = gpui::ElementId::from((
            gpui::ElementId::from("user-view-items-scroll"),
            format!("{view_id}-{animation_id}"),
        ));

        div()
            .id((
                gpui::ElementId::from("user-view-items-row"),
                view_id.to_string(),
            ))
            .debug_selector(|| format!("user-view-items-row-{view_id}"))
            .relative()
            .group(format!("user-view-items-row-{view_id}"))
            .w(px(viewport_width))
            .max_w_full()
            .overflow_hidden()
            .on_hover(on_hover)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .when(visible_range.leading_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.leading_width)))
                    })
                    .children(
                        items.items[visible_range.start..visible_range.end]
                            .iter()
                            .map(|item| {
                                let item_id = item.id.clone();
                                let open_item_id = item_id.clone();
                                let context_item_id = item_id.clone();
                                let open_context_menu = cx.listener(
                                    move |page: &mut HomeContent, event: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        page.open_item_context_menu(
                                            context_item_id.clone(),
                                            ItemContextMenuSource::UserItem,
                                            event.position,
                                            cx,
                                        );
                                    },
                                );
                                let on_click =
                                    cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                        page.open_media_detail_by_id(open_item_id.clone(), cx);
                                    });
                                let card = if item.item_type.as_deref() == Some("Episode") {
                                    let image_path = self.image_path_for_episode_user_item(item);
                                    user_episode_card(
                                        self.controller.user_episode_card_vm(item),
                                        image_path,
                                        cx,
                                    )
                                    .id((
                                        gpui::ElementId::from("user-view-episode-card"),
                                        item_id.clone(),
                                    ))
                                } else {
                                    let image_path = self.image_path_for_user_item(item);
                                    user_item_card(
                                        self.controller.user_item_card_vm(item, true),
                                        image_path,
                                        cx,
                                    )
                                    .id((
                                        gpui::ElementId::from("user-view-item-card"),
                                        item_id.clone(),
                                    ))
                                };
                                card.debug_selector(move || {
                                    format!("user-view-item-card-{item_id}")
                                })
                                .cursor_pointer()
                                .on_click(on_click)
                                .on_mouse_down(MouseButton::Right, open_context_menu)
                            }),
                    )
                    .when(visible_range.trailing_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.trailing_width)))
                    })
                    .map(|track| {
                        home_carousel_track(track, animation_key, previous_offset, offset)
                    }),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-view-items-scroll-left",
                    "icons/chevron-left.svg",
                    false,
                    controls_visible,
                    theme,
                    left_controls_hover,
                    scroll_left,
                ))
            })
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "user-view-items-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }
}
