use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyView, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, StyleRefinement, Styled,
    Window, div, ease_in_out, prelude::FluentBuilder, px,
};

use crate::{
    app::WINDOW_RESIZE_EDGE_WIDTH_PX,
    emby::{ResumeItems, UserItems, UserView, UserViews},
    theme,
    ui::scrollbar::Scrollbar,
};

use super::{
    HomeContent, HomeDashboard, HomePage,
    carousel::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, USER_VIEW_CARD_GAP_PX, USER_VIEW_CARD_PADDING_PX,
        USER_VIEW_CARD_WIDTH_PX, carousel_content_width, carousel_content_width_for,
        carousel_visible_range_between_for, home_main_content_width, max_carousel_scroll_offset,
        max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, home_section_title, home_section_title_text, resume_item_card,
        user_episode_card, user_item_card, user_view_card,
    },
    navigation::{HomeRoot, HomeRoute},
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

        let current = self.navigation.current();
        let is_detail = matches!(current, HomeRoute::Detail { .. });
        let is_library = matches!(current, HomeRoute::Library { .. });
        let is_favorites = current == &HomeRoute::Root(HomeRoot::Favorites);
        let is_search = current == &HomeRoute::Root(HomeRoot::Search);
        let scroll_handle = self.current_scroll_handle();
        let has_authentication_error = self.authentication_error.is_some();

        div()
            .relative()
            .size_full()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .child(AnyView::from(self.home_dashboard.clone()).cached(home_dashboard_cache_style()))
            .when_some(self.authentication_error.clone(), |this, error| {
                this.child(self.render_workspace_layer(
                    self.render_authentication_error(error, cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_detail && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_series_detail_scrollable_content(window, cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_favorites && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_favorites_scrollable_content(cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_search && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_search_scrollable_content(cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_library && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_library_scrollable_content(cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(
                (is_detail || is_library) && !has_authentication_error,
                |this| this.child(self.render_series_detail_back_button(cx)),
            )
            .child(
                Scrollbar::vertical(scroll_handle)
                    .id("home-main-scrollbar")
                    .edge_inset(px(8.0))
                    .right_inset(scrollbar_right_inset),
            )
    }

    fn render_workspace_layer(
        &self,
        content: impl IntoElement,
        rounded_window: bool,
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
            .occlude()
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .child(content)
    }

    fn current_scroll_handle(&self) -> &ScrollHandle {
        match self.navigation.current() {
            HomeRoute::Root(HomeRoot::Home) => &self.home_scroll_handle,
            HomeRoute::Root(HomeRoot::Favorites) => &self.favorites.scroll_handle,
            HomeRoute::Root(HomeRoot::Search) => &self.search.scroll_handle,
            HomeRoute::Library { view_id, .. } => self
                .libraries
                .get(view_id)
                .map(|state| &state.paged.scroll_handle)
                .unwrap_or(&self.home_scroll_handle),
            HomeRoute::Detail { .. } => self
                .series_detail
                .as_ref()
                .map(|detail| &detail.scroll_handle)
                .unwrap_or(&self.home_scroll_handle),
        }
    }

    fn render_authentication_error(
        &self,
        error: gpui::SharedString,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .p_6()
            .flex()
            .flex_col()
            .gap_2()
            .child(home_section_title("需要重新登录", cx))
            .child(div().text_sm().text_color(theme.error).child(error))
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
            .track_scroll(&self.home_scroll_handle)
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
                this.child(self.render_inline_error(
                    error,
                    "retry-home-views",
                    cx.listener(|page, _, _, cx| page.load_user_views_if_needed(cx)),
                    cx,
                ))
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
                            .child("暂无可浏览的视频媒体库"),
                    )
                },
            )
            .child(home_section_title("继续观看", cx).mt_8().mb_3())
            .when_some(self.resume_items_failed.clone(), |this, error| {
                this.child(self.render_inline_error(
                    error,
                    "retry-home-resume",
                    cx.listener(|page, _, _, cx| page.load_resume_items_if_needed(cx)),
                    cx,
                ))
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
                self.resume_items.as_ref().is_some_and(|items| {
                    items.items.iter().any(|item| {
                        item.item_type.as_deref() == Some("Episode")
                            && item
                                .series_id
                                .as_deref()
                                .is_none_or(|id| id.trim().is_empty())
                    })
                }),
                |this| {
                    this.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("部分单集缺少剧集信息，暂时无法打开"),
                    )
                },
            )
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
        let visible_range = carousel_visible_range_between_for(
            views.items.len(),
            (previous_offset, offset),
            viewport_width,
            USER_VIEW_CARD_WIDTH_PX,
            USER_VIEW_CARD_PADDING_PX,
            USER_VIEW_CARD_GAP_PX,
            (
                HOME_ITEM_RENDER_OVERSCAN_BEFORE,
                HOME_ITEM_RENDER_OVERSCAN_AFTER,
            ),
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
                                let view = view.clone();
                                let item_id = view.id.clone();
                                let card_name = view.name.clone();
                                let on_click = cx.listener(move |page, _, _, cx| {
                                    page.open_library_for_view(&view, cx);
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
        let visible_range = carousel_visible_range_between_for(
            items.items.len(),
            (previous_offset, offset),
            viewport_width,
            USER_VIEW_CARD_WIDTH_PX,
            USER_VIEW_CARD_PADDING_PX,
            USER_VIEW_CARD_GAP_PX,
            (
                HOME_ITEM_RENDER_OVERSCAN_BEFORE,
                HOME_ITEM_RENDER_OVERSCAN_AFTER,
            ),
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
                                let item = self.effective_resume_item(item);
                                let image_path = item
                                    .image_source()
                                    .and_then(|source| self.image_path_for_resume_image(source));
                                let item_id = item.id.clone();
                                let card = resume_item_card(&item, image_path, cx)
                                    .id((gpui::ElementId::from("resume-item-card"), item_id));

                                let navigable = item.item_type.as_deref() == Some("Movie")
                                    || (item.item_type.as_deref() == Some("Episode")
                                        && item
                                            .series_id
                                            .as_deref()
                                            .is_some_and(|id| !id.trim().is_empty()));
                                if navigable {
                                    let on_click =
                                        cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                            page.open_resume_item_detail(&item, cx);
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
        let visible = loading || has_failed || items.is_some_and(|items| !items.items.is_empty());
        let title = view.name.to_string();
        let open_view_id = view.id.clone();
        let view_all_action_id =
            gpui::ElementId::from((gpui::ElementId::from("view-all"), view.id.clone()));
        let open_library = cx.listener(move |page, _, _, cx| {
            page.open_library_by_id(&open_view_id, cx);
        });
        let retry_view_id = view.id.clone();
        let retry = cx.listener(move |page, _, _, cx| {
            page.retry_latest_items(&retry_view_id, cx);
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
                            div()
                                .id(view_all_action_id)
                                .flex()
                                .flex_none()
                                .h(px(28.0))
                                .px_2()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .text_sm()
                                .text_color(theme.foreground)
                                .hover(move |style| style.bg(theme.secondary_hover))
                                .cursor_pointer()
                                .child("更多")
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_click(open_library),
                        ),
                )
                .when_some(failed, |this, error| {
                    this.child(self.render_inline_error(
                        error,
                        gpui::ElementId::from((
                            gpui::ElementId::from("retry-latest"),
                            view.id.clone(),
                        )),
                        retry,
                        cx,
                    ))
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
                                let item = self.effective_user_item(item);
                                let item_id = item.id.clone();
                                let open_item = item.clone();
                                let on_click =
                                    cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                        page.open_media_detail(&open_item, cx);
                                    });
                                if item.item_type.as_deref() == Some("Episode") {
                                    let image_path = self.image_path_for_episode_user_item(&item);
                                    user_episode_card(&item, image_path, cx)
                                        .id((
                                            gpui::ElementId::from("user-view-episode-card"),
                                            item_id,
                                        ))
                                        .cursor_pointer()
                                        .on_click(on_click)
                                } else {
                                    let image_path = self.image_path_for_user_item(&item);
                                    user_item_card(&item, image_path, cx)
                                        .id((gpui::ElementId::from("user-view-item-card"), item_id))
                                        .cursor_pointer()
                                        .on_click(on_click)
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
        if std::mem::take(&mut self.defer_home_dashboard_refresh) {
            let home_dashboard = self.home_dashboard.downgrade();
            window.on_next_frame(move |_, cx| {
                home_dashboard.update(cx, |_, cx| cx.notify()).ok();
            });
            window.request_animation_frame();
        }
        self.render_main_content(window, cx)
    }
}

impl Render for HomeDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(home_content) = self.home_content.upgrade() else {
            return div().into_any_element();
        };

        home_content.update(cx, |content, cx| {
            div()
                .relative()
                .size_full()
                .child(content.render_main_scrollable_content(window, cx))
                .into_any_element()
        })
    }
}

fn home_dashboard_cache_style() -> StyleRefinement {
    StyleRefinement::default().absolute().size_full()
}

impl HomePage {
    fn render_content_area(&self, cx: &Context<Self>, rounded_window: bool) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .bg(theme.background)
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .child(self.home_content.clone())
    }
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized() && !window.is_fullscreen();
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
