use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Styled, Window,
    deferred, div, prelude::FluentBuilder,
};

use crate::{
    server::view::{self as server_view, ServerGridProps},
    theme,
    ui::titlebar::app_titlebar,
};

use super::{
    Page, TinyApp,
    shell::AppRoute,
    window::{
        WindowCornersExt, WindowFrameColors, sync_window_decorations, window_corner_radii,
        window_frame, window_has_rounded_corners, window_uses_system_decorations,
    },
};

impl TinyApp {
    fn render_content(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let close_menu = cx.listener(Self::close_server_menu);
        let home_page = match self.shell.page() {
            Page::Home(page) => Some(page.clone()),
            Page::Servers | Page::Playback { .. } => None,
        };
        let playback_page = match self.shell.page() {
            Page::Playback { page, .. } => Some(page.clone()),
            Page::Servers | Page::Home(_) => None,
        };

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_down(MouseButton::Left, close_menu)
            .on_mouse_down(MouseButton::Right, cx.listener(Self::close_server_menu))
            .when(matches!(self.shell.page(), Page::Servers), |this| {
                this.child(self.render_servers_page(cx))
            })
            .when_some(home_page, |this, page| this.child(page))
            .when_some(playback_page, |this, page| this.child(page))
            .when_some(self.open_server_menu.clone(), |this, menu| {
                if let Some(vm) = self.server_feature.menu() {
                    this.child(
                        deferred(server_view::server_card_menu(
                            vm,
                            menu,
                            self.server_view_listener(cx),
                            cx,
                        ))
                        .with_priority(2),
                    )
                } else {
                    this
                }
            })
            .when(
                matches!(self.shell.page(), Page::Home(_)) && self.has_app_notifications(),
                |this| {
                    this.child(deferred(self.render_app_notification_layer(cx)).with_priority(4))
                },
            )
            .when(
                matches!(self.shell.page(), Page::Servers) && self.has_server_page_notifications(),
                |this| {
                    this.child(
                        deferred(self.render_server_page_notification_layer(cx)).with_priority(4),
                    )
                },
            )
    }

    fn render_servers_page(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = self.server_feature.cards();
        self.server_card_positions
            .retain(|id, _| cards.iter().any(|card| &card.server_id == id));
        let cards = cards
            .into_iter()
            .map(|card| {
                let position = self
                    .server_card_positions
                    .entry(card.server_id.clone())
                    .or_default()
                    .clone();
                (card, position)
            })
            .collect();
        server_view::server_page(
            ServerGridProps {
                cards,
                owner: cx.entity_id(),
                can_reorder: self.server_feature.selecting_server_id().is_none()
                    && self.add_server_dialog.is_none()
                    && self.server_icon_picker.is_none(),
                reorder_focus: self
                    .server_reorder
                    .as_ref()
                    .map(|reorder| reorder.focus.clone()),
            },
            self.server_view_listener(cx),
            cx,
        )
    }
}

impl Render for TinyApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "windows")]
        super::window::windows::sync_window_theme(window, cx);
        self.observe_window_bounds_once(window, cx);
        if self.server_reorder.is_some() && !cx.has_active_drag() {
            self.finish_server_reorder(false, window, cx);
        }

        let title = self.title(cx);
        sync_window_decorations(window, title.clone(), cx);
        let theme = theme::get(cx);
        let system_decorations = window_uses_system_decorations(window);
        let playback_fullscreen =
            window.is_fullscreen() && matches!(self.shell.route(), AppRoute::Playback(_));
        let background = if matches!(self.shell.route(), AppRoute::Playback(_)) {
            gpui::black()
        } else {
            theme.background
        };
        let frame_colors = WindowFrameColors {
            background,
            top: if !playback_fullscreen && !system_decorations {
                theme.title_bar
            } else {
                background
            },
            bottom_left: if matches!(self.shell.page(), Page::Home(_)) {
                theme.panel_background
            } else {
                background
            },
        };
        let rounded_window = window_has_rounded_corners(window);
        let corners = window_corner_radii(window, cx);
        let close_dialog = cx.listener(Self::close_add_server_dialog);
        let close_menu = cx.listener(Self::close_server_menu);
        let submit_dialog = cx.listener(Self::submit_add_server_dialog);
        let dialog = self.add_server_dialog.clone();
        let icon_picker = self.render_server_icon_picker(window, cx);
        let modal_open = dialog.is_some() || icon_picker.is_some();

        let content = div()
            .relative()
            .size_full()
            .when(rounded_window, |this| {
                this.rounded_window_corners(corners).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .when(rounded_window, |this| {
                        this.rounded_window_corners(corners).overflow_hidden()
                    })
                    .when(!playback_fullscreen && !system_decorations, |this| {
                        this.child(
                            div()
                                .on_mouse_down(MouseButton::Left, close_menu)
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(Self::close_server_menu),
                                )
                                .child(app_titlebar(window, cx, title)),
                        )
                    })
                    .child(self.render_content(cx)),
            )
            .when_some(dialog, |this, dialog| {
                this.child(dialog.read(cx).render_layer(
                    dialog.clone(),
                    corners,
                    close_dialog,
                    submit_dialog,
                    cx,
                ))
            })
            .when_some(icon_picker, |this, picker| this.child(picker));

        window_frame(content, frame_colors, !modal_open, window, cx)
    }
}
