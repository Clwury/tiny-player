#[cfg(test)]
use super::feed::home_data_section_is_visible;

use gpui::{
    Context, Corners, InteractiveElement, IntoElement, ParentElement, Pixels, Render, ScrollHandle,
    StyleRefinement, Styled, Window, deferred, div, prelude::FluentBuilder, px,
};

use crate::{
    app::{window_content_size, window_corner_radii},
    theme,
    ui::scrollbar::Scrollbar,
};

use super::{
    HomeContent, HomeDashboard, HomePage,
    carousel::{HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX, home_main_content_width},
    components::home_section_title,
    navigation::{HomeRoot, HomeRoute},
};

impl HomeContent {
    fn render_main_content(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let corners = window_corner_radii(window, cx);

        let current = self.controller.route();
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
        let home_has_content = is_home && self.controller.feed_view().has_content;
        let show_main_scrollbar = main_scrollbar_is_visible(
            current,
            home_has_content,
            self.controller.has_favorites(),
            self.controller.search_view().has_results,
        );
        let scroll_handle = self.current_scroll_handle();
        let has_authentication_error = self.authentication_error.is_some();
        let mount_home_dashboard = self
            .layout
            .view_model()
            .dashboard_should_mount(current, has_authentication_error);

        div()
            .relative()
            .size_full()
            .rounded_br(corners.bottom_right)
            .overflow_hidden()
            .when(mount_home_dashboard, |this| {
                this.child(
                    // Match the live content area so padding and right-aligned
                    // controls keep the same inset on every resize frame. GPUI
                    // refreshes cached views on native resize anyway; stepping
                    // this surface in 32px increments only made the gutter jump.
                    self.dashboard
                        .entity
                        .clone()
                        .cached(StyleRefinement::default().absolute().size_full()),
                )
            })
            .when_some(self.authentication_error.clone(), |this, error| {
                this.child(self.render_workspace_layer(
                    self.render_authentication_error(error, cx),
                    corners,
                    cx,
                ))
            })
            .when(is_detail && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_series_detail_scrollable_content(window, cx),
                    corners,
                    cx,
                ))
            })
            .when(is_favorites && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_favorites_scrollable_content(window, cx),
                    corners,
                    cx,
                ))
            })
            .when(is_favorite_items && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_favorite_items_content(window, cx),
                    corners,
                    cx,
                ))
            })
            .when(is_search && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_search_scrollable_content(window, cx),
                    corners,
                    cx,
                ))
            })
            .when(is_library && !has_authentication_error, |this| {
                this.child(self.render_workspace_layer(
                    self.render_library_scrollable_content(cx),
                    corners,
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
                        .edge_inset(px(8.0)),
                )
            })
            .when_some(self.item_context_menu.clone(), |this, menu| {
                this.child(deferred(self.render_item_context_menu(menu, cx)).with_priority(2))
            })
            .when(
                !has_authentication_error && self.has_visible_notifications(),
                |this| this.child(deferred(self.render_notification_layer(cx)).with_priority(3)),
            )
            .when(is_detail && !has_authentication_error, |this| {
                this.children(
                    self.render_movie_overview_overlay(window, cx)
                        .map(|overlay| deferred(overlay).with_priority(4)),
                )
            })
    }

    fn render_workspace_layer(
        &self,
        content: impl IntoElement,
        corners: Corners<Pixels>,
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
            .rounded_br(corners.bottom_right)
            .overflow_hidden()
            .child(content)
    }

    fn current_scroll_handle(&self) -> &ScrollHandle {
        match self.controller.route() {
            HomeRoute::Root(HomeRoot::Home) => &self.home_scroll_handle,
            HomeRoute::Root(HomeRoot::Favorites) => &self.favorites_presentation.scroll_handle,
            HomeRoute::FavoriteItems { item_type } => {
                &self.favorites_presentation[*item_type]
                    .presentation
                    .scroll_handle
            }
            HomeRoute::Root(HomeRoot::Search) => &self.search_presentation.grid.scroll_handle,
            HomeRoute::Library { view_id, .. } => self
                .library_resources
                .get(view_id)
                .map(|state| &state.presentation.grid.scroll_handle)
                .unwrap_or(&self.home_scroll_handle),
            HomeRoute::Detail { .. } => self
                .detail_view()
                .map(|detail| &detail.presentation.scroll_handle)
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
            .px(px(HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX))
            .py_6()
            .flex()
            .flex_col()
            .gap_2()
            .child(home_section_title("需要重新登录", cx))
            .child(div().text_sm().text_color(theme.error).child(error))
    }
}

impl Render for HomeContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let measured_grid_columns = super::grid::responsive_user_item_grid_columns(
            super::grid::user_item_grid_columns(window, self.controller.route()),
        );
        let frame = self.layout.prepare_frame(
            window_size_key(window),
            measured_grid_columns,
            self.controller.route(),
            cx.background_executor().now(),
        );

        // Bounds updates bypass cached view frames. Track the drag as one burst so
        // hidden dashboard work and automatic pagination stay out of the interactive
        // resize path until the window has been still for a short interval.
        if frame.resized {
            self.favorites_presentation.sync_previous_offsets();
            // Finish Home carousel motion before changing its viewport. Keeping
            // the previous animation range would also build cards that are no
            // longer visible for every subsequent resize frame.
            self.user_views_carousel.sync_previous_offset();
            self.resume_items_carousel.sync_previous_offset();
            for carousel in self.latest_carousels.values_mut() {
                carousel.sync_previous_offset();
            }
        }
        if let Some(token) = frame.start_settle {
            self.schedule_resize_settle(token, window, cx);
        }
        self.layout.frame_prepared();
        self.render_main_content(window, cx)
    }
}

impl Render for HomeDashboard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
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
    let size = window_content_size(window);
    (
        f32::from(size.width).round().max(0.0) as u32,
        f32::from(size.height).round().max(0.0) as u32,
    )
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
    fn render_content_area(&self, corners: Corners<Pixels>) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .rounded_br(corners.bottom_right)
            .overflow_hidden()
            .child(self.home_content.clone())
    }
}

impl Render for HomePage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sidebar_reorder.is_some() && !cx.has_active_drag() {
            self.finish_sidebar_reorder(false, window, cx);
        }
        let corners = window_corner_radii(window, cx);
        let props = super::sidebar::SidebarProps {
            view: self.sidebar.view_model(),
            active_root: self.home_content.read(cx).root(),
            corners,
            owner: cx.entity_id(),
            scroll: self.sidebar_scroll_handle.clone(),
            reorder_focus: self
                .sidebar_reorder
                .as_ref()
                .map(|reorder| reorder.focus.clone()),
        };

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .overflow_hidden()
            .child(super::sidebar::render_sidebar(
                props,
                self.sidebar_listener(cx),
                cx,
            ))
            .child(self.render_content_area(corners))
    }
}

#[cfg(test)]
mod resize_tests;

#[cfg(test)]
mod resume_menu_tests;

#[cfg(test)]
mod tests;
