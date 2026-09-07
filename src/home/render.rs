use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Context, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Render, ScrollHandle, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, anchored, deferred, div, ease_in_out, point,
    prelude::FluentBuilder, px,
};

use crate::{
    app::WINDOW_RESIZE_EDGE_WIDTH_PX,
    emby::{ResumeItems, UserItem, UserItems, UserView, UserViews},
    theme,
    ui::scrollbar::Scrollbar,
};

use super::{
    HomeContent, HomeDashboard, HomePage,
    carousel::{
        CAROUSEL_SCROLL_DURATION, CarouselVisibleRange, HOME_ITEM_CARD_GAP_PX,
        HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX, HOME_MAIN_SCROLLBAR_WIDTH_PX,
        USER_VIEW_CARD_GAP_PX, USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX,
        carousel_content_width, carousel_content_width_for, carousel_visible_range_between_for,
        home_main_content_width, max_carousel_scroll_offset, max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, home_section_title, home_section_title_text, resume_item_card,
        user_episode_card, user_item_card, user_view_card,
    },
    navigation::{HomeRoot, HomeRoute},
    resume_actions::{ResumeItemAction, ResumeItemContextMenu},
    visible_row::VisibleRow,
};

const HOME_ITEM_RENDER_OVERSCAN_BEFORE: usize = 2;
const HOME_ITEM_RENDER_OVERSCAN_AFTER: usize = 4;
const RESIZE_SETTLE_DEBOUNCE: Duration = Duration::from_millis(120);
const WORKSPACE_GRID_ABRUPT_COLUMN_DELTA: usize = 2;

impl HomeContent {
    fn schedule_resize_settle(&mut self, window: &Window, cx: &mut Context<Self>) {
        // Keep one debounce task alive for the whole drag. Bounds notifications
        // can arrive once per display frame; replacing a timer for every event
        // would add another stream of foreground task allocations to the resize
        // path.
        if self.resize_settle_task_active {
            return;
        }
        self.resize_settle_task_active = true;
        self.resize_settle_task = cx.spawn_in(window, async move |page, cx| {
            // Let the current render/effect cycle finish before borrowing the
            // entity from the async context. The first timer also provides the
            // normal debounce window for a resize drag.
            cx.background_executor().timer(RESIZE_SETTLE_DEBOUNCE).await;
            loop {
                let remaining = page
                    .update(cx, |page, _| {
                        page.last_resize_activity
                            .map(|last| RESIZE_SETTLE_DEBOUNCE.saturating_sub(last.elapsed()))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if remaining.is_zero() {
                    let settled = page
                        .update(cx, |page, cx| {
                            page.resize_in_progress = false;
                            page.last_resize_activity = None;
                            page.resize_settle_task_active = false;
                            page.defer_workspace_grid_contraction = false;
                            let generation = page.resize_generation;
                            let warm_dashboard = page.defer_home_dashboard_warmup
                                && page.navigation.current() != &HomeRoute::Root(HomeRoot::Home)
                                && page.authentication_error.is_none();
                            if !warm_dashboard {
                                page.defer_home_dashboard_warmup = false;
                            }
                            // First present the final workspace grid. Warming the
                            // hidden Home cache is deliberately kept out of this
                            // same frame below.
                            cx.notify();
                            (generation, warm_dashboard)
                        })
                        .ok();

                    if let Some((generation, true)) = settled {
                        let page = page.clone();
                        // Next-frame callbacks run before their frame is drawn.
                        // Nest once so the settled workspace is guaranteed one
                        // presentation before the hidden dashboard is rebuilt.
                        cx.on_next_frame(move |window, _| {
                            window.on_next_frame(move |_, cx| {
                                page.update(cx, |page, cx| {
                                    // A newer resize owns its own warmup frame.
                                    if page.resize_generation == generation
                                        && !page.resize_in_progress
                                        && page.defer_home_dashboard_warmup
                                    {
                                        page.defer_home_dashboard_warmup = false;
                                        cx.notify();
                                    }
                                })
                                .ok();
                            });
                        });
                    }
                    break;
                }
                cx.background_executor().timer(remaining).await;
            }
        });
    }

    fn render_main_content(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let rounded_window = !window.is_maximized() && !window.is_fullscreen();
        let scrollbar_right_inset = if rounded_window {
            px(WINDOW_RESIZE_EDGE_WIDTH_PX)
        } else {
            px(0.0)
        };

        let current = self.navigation.current();
        let is_home = current == &HomeRoute::Root(HomeRoot::Home);
        let is_detail = matches!(current, HomeRoute::Detail { .. });
        let is_library = matches!(current, HomeRoute::Library { .. });
        let is_favorites = current == &HomeRoute::Root(HomeRoot::Favorites);
        let is_favorite_items = matches!(current, HomeRoute::FavoriteItems { .. });
        let is_search = current == &HomeRoute::Root(HomeRoot::Search);
        // Keep the cached dashboard layer mounted behind opaque workspaces so a
        // route transition back to Home can reuse its last layout and paint scene.
        // Drop it during non-Home resize bursts, where changed bounds would otherwise
        // force the hidden dashboard to rebuild on every pointer update.
        let home_has_content = is_home
            && (self
                .user_views
                .as_ref()
                .is_some_and(|views| !views.items.is_empty())
                || self
                    .resume_items
                    .as_ref()
                    .is_some_and(|items| !items.items.is_empty())
                || self.user_view_items_rows.values().any(|row| {
                    row.items
                        .as_ref()
                        .is_some_and(|items| !items.items.is_empty())
                }));
        let show_main_scrollbar = main_scrollbar_is_visible(
            current,
            home_has_content,
            self.favorites.has_items(),
            !self.search.query.is_empty() && !self.search.items.is_empty(),
        );
        let scroll_handle = self.current_scroll_handle();
        let has_authentication_error = self.authentication_error.is_some();
        let mount_home_dashboard = home_dashboard_should_mount(
            current,
            has_authentication_error,
            self.resize_in_progress,
            self.defer_home_dashboard_warmup,
        );

        div()
            .relative()
            .size_full()
            .bg(theme.background)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::close_resume_item_context_menu),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::close_resume_item_context_menu),
            )
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .when(mount_home_dashboard, |this| {
                this.child(
                    // Match the live content area so padding and right-aligned
                    // controls keep the same inset on every resize frame. GPUI
                    // refreshes cached views on native resize anyway; stepping
                    // this surface in 32px increments only made the gutter jump.
                    self.home_dashboard
                        .clone()
                        .cached(StyleRefinement::default().absolute().size_full()),
                )
            })
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
                    self.render_favorites_scrollable_content(window, cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_favorite_items && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_favorite_items_content(window, cx),
                    rounded_window,
                    cx,
                ))
            })
            .when(is_search && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_search_scrollable_content(window, cx),
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
            .when(is_detail && !has_authentication_error, |this| {
                this.child(self.render_series_detail_back_button(cx))
            })
            .when(show_main_scrollbar, |this| {
                this.child(
                    Scrollbar::vertical(scroll_handle)
                        .id("home-main-scrollbar")
                        .edge_inset(px(8.0))
                        .right_inset(scrollbar_right_inset),
                )
            })
            .when_some(self.resume_item_context_menu.clone(), |this, menu| {
                this.child(
                    deferred(self.render_resume_item_context_menu(menu, cx)).with_priority(2),
                )
            })
            .when(
                !has_authentication_error && self.has_visible_notifications(),
                |this| this.child(deferred(self.render_notification_layer(cx)).with_priority(3)),
            )
    }

    fn render_resume_item_context_menu(
        &self,
        menu: ResumeItemContextMenu,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let pending = self.resume_item_requests.contains(&menu.item_id);
        let mark_item_id = menu.item_id.clone();
        let hide_item_id = menu.item_id;
        let mark_played = cx.listener(move |page: &mut HomeContent, _: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
            page.start_resume_item_action(mark_item_id.clone(), ResumeItemAction::MarkPlayed, cx);
        });
        let hide_from_resume =
            cx.listener(move |page: &mut HomeContent, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                page.start_resume_item_action(
                    hide_item_id.clone(),
                    ResumeItemAction::HideFromResume,
                    cx,
                );
            });

        anchored()
            .position(menu.position)
            .offset(point(px(4.0), px(4.0)))
            .snap_to_window_with_margin(px(8.0))
            .child(
                div()
                    .id("resume-item-context-menu")
                    .occlude()
                    .flex()
                    .flex_col()
                    .min_w(px(176.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border_focused)
                    .bg(theme.dialog_background)
                    .shadow_lg()
                    .p(px(4.0))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .child(resume_item_context_menu_option(
                        ResumeItemAction::MarkPlayed.label(),
                        false,
                        pending,
                        mark_played,
                        cx,
                    ))
                    .child(resume_item_context_menu_option(
                        ResumeItemAction::HideFromResume.label(),
                        true,
                        pending,
                        hide_from_resume,
                        cx,
                    )),
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
            HomeRoute::FavoriteItems { item_type } => {
                &self.favorites[*item_type].paged.scroll_handle
            }
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
        main_content_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let show_user_views_section = home_data_section_is_visible(
            self.user_views.is_some(),
            self.user_views
                .as_ref()
                .is_some_and(|views| !views.items.is_empty()),
            self.home_effects.user_views.is_loading(),
            self.user_views_failed.is_some(),
        );
        let show_resume_section = self
            .resume_items
            .as_ref()
            .is_some_and(|items| !items.items.is_empty());

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
                        ),
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
                        ),
                )
            })
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
            home_carousel_overscan(self.resize_in_progress),
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
            home_carousel_overscan(self.resize_in_progress),
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
                                let item = self.effective_resume_item(item);
                                let image_path = item
                                    .image_source()
                                    .and_then(|source| self.image_path_for_resume_image(source));
                                let item_id = item.id.clone();
                                let context_item_id = item_id.clone();
                                let card_item_id = item_id.clone();
                                let pending = self.resume_item_requests.contains(&item_id);
                                let open_context_menu = cx.listener(
                                    move |page: &mut HomeContent, event: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        page.open_resume_item_context_menu(
                                            context_item_id.clone(),
                                            event.position,
                                            cx,
                                        );
                                    },
                                );
                                let card = resume_item_card(&item, image_path, cx)
                                    .id((gpui::ElementId::from("resume-item-card"), card_item_id))
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

    fn render_user_view_items_section(
        &self,
        view: &UserView,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let row = self.user_view_items_rows.get(&view.id);
        let items = row.and_then(|row| row.items.as_ref());
        let visible = items.is_some_and(|items| !items.items.is_empty());
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
                            div()
                                .id(view_all_action_id)
                                .debug_selector(|| format!("view-all-{}", view.id))
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

    fn render_visible_user_view_items_row(
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
            .user_view_items_rows
            .get(view_id)
            .map(|row| row.carousel)
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
            home_carousel_overscan(self.resize_in_progress),
        );
        // A carousel's height comes from its tallest card. Measure just that
        // card, without loading artwork, so offscreen rows keep their exact
        // space without constructing their images, labels and event listeners.
        let measurement = div().w(px(viewport_width)).max_w_full().when_some(
            tallest_home_item(&items.items[range.start..range.end]),
            |this, item| {
                this.child(if item.item_type.as_deref() == Some("Episode") {
                    user_episode_card(item, None, cx)
                } else {
                    user_item_card(item, None, cx)
                })
            },
        );
        let content = cx.weak_entity();
        let view_id = view_id.to_owned();
        VisibleRow::new(measurement, move |_: &mut Window, cx: &mut App| {
            content
                .update(cx, |content, cx| {
                    let Some(items) = content
                        .user_view_items_rows
                        .get(&view_id)
                        .and_then(|row| row.items.as_ref())
                    else {
                        return div().into_any_element();
                    };
                    content
                        .render_user_view_items_row(&view_id, items, viewport_width, range, cx)
                        .into_any_element()
                })
                .unwrap_or_else(|_| div().into_any_element())
        })
    }

    fn render_user_view_items_row(
        &self,
        view_id: &str,
        items: &UserItems,
        viewport_width: f32,
        visible_range: CarouselVisibleRange,
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
                                let item = self.effective_user_item(item);
                                let item_id = item.id.clone();
                                let open_item_id = item_id.clone();
                                let on_click =
                                    cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                        page.open_media_detail_by_id(open_item_id.clone(), cx);
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

fn resume_item_context_menu_option(
    label: &'static str,
    destructive: bool,
    disabled: bool,
    on_mouse_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    cx: &Context<HomeContent>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .rounded(px(6.0))
        .px_2()
        .text_sm()
        .text_color(if destructive {
            theme.error
        } else {
            theme.foreground
        })
        .when(disabled, |this| this.cursor_default().opacity(0.52))
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(theme.secondary_hover))
                .on_mouse_down(MouseButton::Left, on_mouse_down)
        })
        .child(label)
}

impl Render for HomeContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_size_key = window_size_key(window);
        let previous_window_size = self.last_window_size.replace(window_size_key);
        let window_size_changed = previous_window_size.is_some_and(|size| size != window_size_key);
        let measured_grid_columns = super::workspace_render::responsive_user_item_grid_columns(
            super::workspace_render::user_item_grid_columns(window, self.navigation.current()),
        );
        let current_route = self.navigation.current();
        let virtual_workspace_visible = matches!(
            current_route,
            HomeRoute::FavoriteItems { .. } | HomeRoute::Root(HomeRoot::Search)
        );

        // Bounds updates bypass cached view frames. Track the drag as one burst so
        // hidden dashboard work and automatic pagination stay out of the interactive
        // resize path until the window has been still for a short interval.
        if window_size_changed {
            self.favorites.sync_previous_offsets();
            // Finish Home carousel motion before changing its viewport. Keeping
            // the previous animation range would also build cards that are no
            // longer visible for every subsequent resize frame.
            self.user_views_carousel.sync_previous_offset();
            self.resume_items_carousel.sync_previous_offset();
            for row in self.user_view_items_rows.values_mut() {
                row.carousel.sync_previous_offset();
            }
            self.resize_in_progress = true;
            self.resize_generation = self.resize_generation.wrapping_add(1);
            self.last_resize_activity = Some(std::time::Instant::now());
            self.defer_home_dashboard_warmup = current_route != &HomeRoute::Root(HomeRoot::Home);
            if virtual_workspace_visible
                && workspace_grid_contraction_is_abrupt(
                    self.workspace_grid_columns,
                    measured_grid_columns,
                )
            {
                // Keep the already laid-out rows for the first frame of a large
                // contraction. The viewport clips them immediately, then the
                // final column layout is committed by the settle notification.
                self.defer_workspace_grid_contraction = true;
            }
            self.schedule_resize_settle(window, cx);
        }

        self.workspace_grid_columns = workspace_grid_columns_during_resize(
            self.workspace_grid_columns,
            measured_grid_columns,
            self.defer_workspace_grid_contraction,
        );

        // The dashboard cache stays warm behind idle non-Home workspaces. During a
        // non-Home resize it is omitted and warmed shortly after the final workspace
        // frame, so the two expensive transitions cannot share a presentation. Clear
        // the route-only reuse guard only after this first Home frame is assembled;
        // subsequent data notifications must invalidate the dashboard as usual.
        self.reuse_home_dashboard_until_next_render = false;
        self.render_main_content(window, cx)
    }
}

impl Render for HomeDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(home_content) = self.home_content.upgrade() else {
            return div().into_any_element();
        };

        home_content.update(cx, |content, cx| {
            let main_content_width = home_main_content_width(window);
            div()
                .relative()
                .size_full()
                .child(content.render_main_scrollable_content(main_content_width, cx))
                .into_any_element()
        })
    }
}

fn window_size_key(window: &Window) -> (u32, u32) {
    let size = window.viewport_size();
    (
        f32::from(size.width).round().max(0.0) as u32,
        f32::from(size.height).round().max(0.0) as u32,
    )
}

fn home_dashboard_should_mount(
    route: &HomeRoute,
    has_authentication_error: bool,
    resize_in_progress: bool,
    warmup_deferred: bool,
) -> bool {
    !has_authentication_error
        && (route == &HomeRoute::Root(HomeRoot::Home) || (!resize_in_progress && !warmup_deferred))
}

fn workspace_grid_contraction_is_abrupt(current: usize, measured: usize) -> bool {
    current.max(1).saturating_sub(measured.max(1)) >= WORKSPACE_GRID_ABRUPT_COLUMN_DELTA
}

fn workspace_grid_columns_during_resize(
    current: usize,
    measured: usize,
    defer_contraction: bool,
) -> usize {
    let current = current.max(1);
    let measured = measured.max(1);
    if defer_contraction && measured < current {
        current
    } else {
        // Expansion remains live. Only an abrupt contraction is held until the
        // current resize burst settles.
        measured
    }
}

fn home_carousel_overscan(resize_in_progress: bool) -> (usize, usize) {
    if resize_in_progress {
        (0, 0)
    } else {
        (
            HOME_ITEM_RENDER_OVERSCAN_BEFORE,
            HOME_ITEM_RENDER_OVERSCAN_AFTER,
        )
    }
}

pub(super) fn home_carousel_track(
    track: gpui::Div,
    animation_id: impl Into<gpui::ElementId>,
    previous_offset: f32,
    offset: f32,
) -> gpui::AnyElement {
    if previous_offset == offset {
        // Stationary rows need no animation state or extra frame requests,
        // including rows that reappear after being outside the viewport.
        track.ml(px(-offset)).into_any_element()
    } else {
        track
            .with_animation(
                animation_id,
                Animation::new(CAROUSEL_SCROLL_DURATION).with_easing(ease_in_out),
                move |track, delta| {
                    track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                },
            )
            .into_any_element()
    }
}

fn tallest_home_item(items: &[UserItem]) -> Option<&UserItem> {
    items.iter().max_by_key(|item| {
        (
            item.item_type.as_deref() != Some("Episode"),
            item.production_year.is_some(),
        )
    })
}

fn home_data_section_is_visible(
    has_response: bool,
    has_items: bool,
    loading: bool,
    failed: bool,
) -> bool {
    has_items || (has_response && !loading && !failed)
}

fn main_scrollbar_is_visible(
    route: &HomeRoute,
    home_has_content: bool,
    favorites_has_content: bool,
    search_has_content: bool,
) -> bool {
    match route {
        HomeRoute::Root(HomeRoot::Home) => home_has_content,
        HomeRoute::Root(HomeRoot::Favorites) => favorites_has_content,
        HomeRoute::Root(HomeRoot::Search) => search_has_content,
        HomeRoute::FavoriteItems { .. } | HomeRoute::Library { .. } | HomeRoute::Detail { .. } => {
            true
        }
    }
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
        let on_settings = cx.listener(Self::open_settings);

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
                on_settings,
            ))
            .child(self.render_content_area(cx, rounded_window))
    }
}

#[cfg(test)]
mod resize_tests;

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, IntoElement, ParentElement, Styled, TestAppContext, div, point, px, size,
    };

    use crate::{emby::UserItem, theme};

    use super::{
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX, HomeRoot,
        HomeRoute, carousel_visible_range_between_for, home_carousel_overscan,
        home_dashboard_should_mount, home_data_section_is_visible, main_scrollbar_is_visible,
        tallest_home_item, user_episode_card, user_item_card, workspace_grid_columns_during_resize,
        workspace_grid_contraction_is_abrupt,
    };

    #[test]
    fn resize_carousels_keep_partial_cards_without_building_overscan() {
        let range = |resizing| {
            carousel_visible_range_between_for(
                100,
                (1_840.0, 1_840.0),
                800.0,
                HOME_ITEM_CARD_WIDTH_PX,
                HOME_ITEM_CARD_PADDING_PX,
                HOME_ITEM_CARD_GAP_PX,
                home_carousel_overscan(resizing),
            )
        };
        let resizing = range(true);
        assert_eq!(resizing.start..resizing.end, 10..15);
        let settled = range(false);
        assert_eq!(settled.start..settled.end, 8..19);
    }

    #[gpui::test]
    fn representative_card_matches_full_carousel_height(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let cx = cx.add_empty_window();
        let movie = |year| {
            serde_json::from_value::<UserItem>(serde_json::json!({
                "Id": "movie", "Name": "Movie", "Type": "Movie", "ProductionYear": year
            }))
            .unwrap()
        };
        let episode = serde_json::from_value::<UserItem>(serde_json::json!({
            "Id": "episode", "Name": "Episode", "Type": "Episode", "SeriesName": "Series"
        }))
        .unwrap();

        for items in [
            vec![movie(None)],
            vec![movie(Some(2024))],
            vec![episode.clone()],
            vec![episode.clone(), movie(None)],
            vec![movie(None), episode, movie(Some(2024))],
        ] {
            cx.draw(
                point(px(0.0), px(0.0)),
                size(px(800.0), px(600.0)),
                |window, cx| {
                    cx.new(|cx| {
                        let card = |item: &UserItem| {
                            if item.item_type.as_deref() == Some("Episode") {
                                user_episode_card(item, None, cx)
                            } else {
                                user_item_card(item, None, cx)
                            }
                        };
                        let mut measurement = div()
                            .w(px(800.0))
                            .child(card(tallest_home_item(&items).unwrap()))
                            .into_any_element();
                        let mut full_row = div()
                            .w(px(800.0))
                            .flex()
                            .gap_4()
                            .children(items.iter().map(card))
                            .into_any_element();
                        let space = size(px(800.0).into(), gpui::AvailableSpace::MinContent);
                        assert_eq!(
                            measurement.layout_as_root(space, window, cx).height,
                            full_row.layout_as_root(space, window, cx).height,
                        );
                    });
                    div()
                },
            );
        }
    }

    #[test]
    fn home_dashboard_cache_stays_warm_except_during_non_home_resize() {
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            false,
            false,
            false,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            false,
            false,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            true,
            true,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            false,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            true,
            true,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            true,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            false,
            true,
            true,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            true,
            false,
            false,
        ));
    }

    #[test]
    fn only_multi_column_width_contractions_use_the_fast_resize_frame() {
        assert!(workspace_grid_contraction_is_abrupt(7, 5));
        assert!(workspace_grid_contraction_is_abrupt(7, 3));
        assert!(!workspace_grid_contraction_is_abrupt(7, 6));
        assert!(!workspace_grid_contraction_is_abrupt(7, 8));
        assert!(!workspace_grid_contraction_is_abrupt(0, 0));
    }

    #[test]
    fn abrupt_contraction_is_deferred_without_delaying_expansion() {
        assert_eq!(workspace_grid_columns_during_resize(7, 3, true), 7);
        assert_eq!(workspace_grid_columns_during_resize(7, 8, true), 8);
        assert_eq!(workspace_grid_columns_during_resize(7, 3, false), 3);
        assert_eq!(workspace_grid_columns_during_resize(0, 0, true), 1);
    }

    #[test]
    fn empty_home_section_is_hidden_while_loading_or_after_failure() {
        assert!(!home_data_section_is_visible(false, false, true, false));
        assert!(!home_data_section_is_visible(true, false, true, false));
        assert!(!home_data_section_is_visible(true, false, false, true));
    }

    #[test]
    fn home_section_shows_cached_items_or_a_confirmed_empty_state() {
        assert!(home_data_section_is_visible(true, true, true, false));
        assert!(home_data_section_is_visible(true, false, false, false));
    }

    #[test]
    fn empty_root_pages_hide_the_main_scrollbar() {
        assert!(!main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Home),
            false,
            false,
            false,
        ));
        assert!(!main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            false,
            false,
        ));
        assert!(!main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            false,
        ));
    }

    #[test]
    fn populated_root_pages_show_the_main_scrollbar() {
        assert!(main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Home),
            true,
            false,
            false,
        ));
        assert!(main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            true,
            false,
        ));
        assert!(main_scrollbar_is_visible(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            true,
        ));
    }
}
