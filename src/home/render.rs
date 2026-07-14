use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyView, Context, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, StyleRefinement, Styled, Window, div, ease_in_out,
    prelude::FluentBuilder, px,
};

use crate::{
    app::WINDOW_RESIZE_EDGE_WIDTH_PX,
    emby::{ResumeItems, UserItems, UserView, UserViews},
    theme,
    ui::scrollbar::Scrollbar,
};

use super::{
    HomeContent, HomePage, HomeSection,
    carousel::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, carousel_content_width, carousel_content_width_for,
        carousel_visible_range_between_for, home_main_content_width, max_carousel_scroll_offset,
        max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, home_section_title, home_section_title_text, resume_item_card,
        section_placeholder, user_item_card, user_view_card,
    },
};

const HOME_ITEM_RENDER_OVERSCAN_BEFORE: usize = 2;
const HOME_ITEM_RENDER_OVERSCAN_AFTER: usize = 4;

impl HomeContent {
    fn render_main_content(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized() && !window.is_fullscreen();
        let scrollbar_right_inset = if rounded_window {
            px(WINDOW_RESIZE_EDGE_WIDTH_PX)
        } else {
            px(0.0)
        };

        div()
            .relative()
            .size_full()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .when(self.series_detail.is_some(), |this| {
                this.child(self.render_series_detail_scrollable_content(window, cx))
            })
            .when(self.series_detail.is_none(), |this| {
                this.child(self.render_main_scrollable_content(window, cx))
            })
            .when(self.series_detail.is_some(), |this| {
                this.child(self.render_series_detail_back_button(cx))
            })
            .child(
                Scrollbar::vertical(&self.main_scroll_handle)
                    .id("home-main-scrollbar")
                    .edge_inset(px(8.0))
                    .right_inset(scrollbar_right_inset),
            )
    }

    fn render_main_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let main_content_width = home_main_content_width(window);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-main-content")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&self.main_scroll_handle)
            .p_6()
            .child(
                div()
                    .mb_3()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child("我的媒体"),
            )
            .when_some(self.user_views_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when(
                self.home_effects.user_views.is_loading() && self.user_views.is_none(),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("加载中…"),
                    )
                },
            )
            .when_some(self.user_views.as_ref(), |this, views| {
                this.child(self.render_user_views_row(views, main_content_width, cx))
            })
            .when(
                !self.home_effects.user_views.is_loading()
                    && self.user_views_failed.is_none()
                    && self
                        .user_views
                        .as_ref()
                        .is_none_or(|views| views.items.is_empty()),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("暂无媒体库"),
                    )
                },
            )
            .child(home_section_title("继续观看", cx).mt_8().mb_3())
            .when_some(self.resume_items_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when_some(self.resume_detail_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when(
                self.home_effects.resume_items.is_loading() && self.resume_items.is_none(),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("加载中…"),
                    )
                },
            )
            .when_some(self.resume_items.as_ref(), |this, items| {
                this.child(self.render_resume_items_row(items, main_content_width, cx))
            })
            .when(
                !self.home_effects.resume_items.is_loading()
                    && self.resume_items_failed.is_none()
                    && self
                        .resume_items
                        .as_ref()
                        .is_none_or(|items| items.items.is_empty()),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("暂无继续观看内容"),
                    )
                },
            )
            .when_some(self.user_views.as_ref(), |this, views| {
                this.children(
                    views.items.iter().map(|view| {
                        self.render_user_view_items_section(view, main_content_width, cx)
                    }),
                )
            })
    }

    fn render_user_views_row(
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
                    .children(views.items.iter().map(|view| {
                        let image_path = self.image_path_for_primary_image(
                            &view.id,
                            view.image_tags
                                .as_ref()
                                .and_then(|tags| tags.primary.as_deref()),
                        );
                        user_view_card(view.name.clone(), image_path, cx)
                    }))
                    .with_animation(
                        ("user-views-scroll", carousel.animation_id()),
                        Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                        move |track, delta| {
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
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

    fn render_resume_items_row(
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
                    .children(items.items.iter().map(|item| {
                        let image_path = item
                            .image_source()
                            .and_then(|source| self.image_path_for_resume_image(source));
                        let item_id = item.id.clone();
                        let card = resume_item_card(item, image_path, cx)
                            .id((gpui::ElementId::from("resume-item-card"), item_id));

                        if matches!(item.item_type.as_deref(), Some("Episode" | "Movie")) {
                            let item = item.clone();
                            let on_click = cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                page.open_resume_item_detail(&item, cx);
                            });
                            card.cursor_pointer().on_click(on_click)
                        } else {
                            card
                        }
                    }))
                    .with_animation(
                        ("resume-items-scroll", carousel.animation_id()),
                        Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                        move |track, delta| {
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
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

    fn render_user_view_items_section(
        &self,
        view: &UserView,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let row = self.user_view_items_rows.get(&view.id);
        let failed = row.and_then(|row| row.failed.clone());
        let has_failed = failed.is_some();
        let loading = row.is_some_and(|row| row.loading);
        let items = row.and_then(|row| row.items.as_ref());
        let show_empty =
            !loading && !has_failed && items.is_some_and(|items| items.items.is_empty());

        div()
            .mt_8()
            .child(home_section_title_text(view.name.clone(), cx).mb_3())
            .when_some(failed, |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when(loading && items.is_none(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("加载中…"),
                )
            })
            .when_some(items, |this, items| {
                this.child(self.render_user_view_items_row(&view.id, items, viewport_width, cx))
            })
            .when(show_empty, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("暂无内容"),
                )
            })
    }

    fn render_user_view_items_row(
        &self,
        view_id: &str,
        items: &UserItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let row = self.user_view_items_rows.get(view_id);
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
        let carousel = row.map(|row| row.carousel).unwrap_or_default();
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            items.items.len(),
            (previous_offset, offset),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
            (
                HOME_ITEM_RENDER_OVERSCAN_BEFORE,
                HOME_ITEM_RENDER_OVERSCAN_AFTER,
            ),
        );
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
                                let image_path = self.image_path_for_user_item(item);
                                let item_id = item.id.clone();
                                let card = user_item_card(item, image_path, cx)
                                    .id((gpui::ElementId::from("user-view-item-card"), item_id));

                                if matches!(item.item_type.as_deref(), Some("Series" | "Movie")) {
                                    let item = item.clone();
                                    let on_click =
                                        cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                            page.open_media_detail(&item, cx);
                                        });
                                    card.on_click(on_click)
                                } else {
                                    card
                                }
                            }),
                    )
                    .when(visible_range.trailing_width > 0.0, |this| {
                        this.child(div().flex_none().w(px(visible_range.trailing_width)))
                    })
                    .with_animation(
                        animation_key,
                        Animation::new(Duration::from_millis(220)).with_easing(ease_in_out),
                        move |track, delta| {
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
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

impl Render for HomeContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_main_content(window, cx)
    }
}

impl HomePage {
    fn render_content_area(&self, cx: &Context<Self>, rounded_window: bool) -> impl IntoElement {
        let theme = theme::get(cx);
        let home_content = AnyView::from(self.home_content.clone())
            .cached(home_content_cache_style(rounded_window, theme.radius_lg));

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .child(home_content)
            .when(self.active_section == HomeSection::Favorites, |this| {
                this.child(self.render_section_layer("收藏", "暂无收藏内容", cx))
            })
            .when(self.active_section == HomeSection::Search, |this| {
                this.child(self.render_section_layer("搜索", "搜索功能暂未实现", cx))
            })
    }

    fn render_section_layer(
        &self,
        title: &'static str,
        message: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .bg(theme.background)
            .p_6()
            .occlude()
            .child(section_placeholder(title, message, cx))
    }
}

fn home_content_cache_style(rounded_window: bool, radius: gpui::Pixels) -> StyleRefinement {
    let style = StyleRefinement::default().absolute().size_full();
    if rounded_window {
        style.rounded_br(radius).overflow_hidden()
    } else {
        style
    }
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized();
        let on_back = cx.listener(Self::back_to_servers);
        let on_home = cx.listener(Self::select_home_section);
        let on_favorites = cx.listener(Self::select_favorites_section);
        let on_search = cx.listener(Self::select_search_section);

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_b(theme.radius_lg).overflow_hidden()
            })
            .child(self.render_sidebar(
                cx,
                rounded_window,
                on_back,
                on_home,
                on_favorites,
                on_search,
            ))
            .child(self.render_content_area(cx, rounded_window))
    }
}
