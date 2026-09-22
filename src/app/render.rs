use gpui::{
    Context, DragMoveEvent, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Render, Styled, Window, deferred, div, prelude::FluentBuilder,
};

use crate::{theme, ui::titlebar::app_titlebar};

use super::{
    Page, TinyApp,
    resize::resize_handles,
    server_card::{
        DraggedServer, ServerCardActions, ServerCardState, add_server_card, server_card,
        server_card_menu,
    },
    server_reorder::animated_card,
    window::{window_border, window_has_rounded_corners},
};

impl TinyApp {
    fn render_content(
        &mut self,
        _rounded_window: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let close_menu = cx.listener(Self::close_server_menu);
        let home_page = match &self.page {
            Page::Home(page) => Some(page.clone()),
            Page::Servers | Page::Playback { .. } => None,
        };
        let playback_page = match &self.page {
            Page::Playback { page, .. } => Some(page.clone()),
            Page::Servers | Page::Home(_) => None,
        };

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_down(MouseButton::Left, close_menu)
            .on_mouse_down(MouseButton::Right, cx.listener(Self::close_server_menu))
            .when(matches!(self.page, Page::Servers), |this| {
                this.child(self.render_servers_page(cx))
            })
            .when_some(home_page, |this, page| this.child(page))
            .when_some(playback_page, |this, page| this.child(page))
            .when_some(self.open_server_menu.clone(), |this, menu| {
                if let Some(server) = self
                    .servers
                    .iter()
                    .find(|server| server.id == menu.server_id)
                {
                    this.child(
                        deferred(server_card_menu(
                            server.clone(),
                            menu,
                            self.cache.auto_start_server_id.as_deref() == Some(&server.id),
                            cx,
                        ))
                        .with_priority(2),
                    )
                } else {
                    this
                }
            })
            .when(
                matches!(self.page, Page::Home(_)) && self.has_app_notifications(),
                |this| {
                    this.child(deferred(self.render_app_notification_layer(cx)).with_priority(4))
                },
            )
            .when(
                matches!(self.page, Page::Servers) && self.has_server_page_notifications(),
                |this| {
                    this.child(
                        deferred(self.render_server_page_notification_layer(cx)).with_priority(4),
                    )
                },
            )
    }

    fn render_servers_page(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let add_server = cx.listener(Self::open_add_server_dialog);
        let can_reorder = self.selecting_server_id.is_none()
            && self.add_server_dialog.is_none()
            && self.server_icon_picker.is_none();
        let servers = self.preview_servers();
        self.server_card_positions
            .retain(|id, _| self.servers.iter().any(|server| &server.id == id));

        div().relative().flex_1().min_h_0().size_full().p_4().child(
            div()
                .id("server-grid")
                .flex()
                .flex_wrap()
                .gap_3()
                .when_some(self.server_reorder.as_ref(), |this, reorder| {
                    this.track_focus(&reorder.focus).on_key_down(cx.listener(
                        |app, event: &gpui::KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_active_drag(window);
                                app.finish_server_reorder(false, window, cx);
                                cx.stop_propagation();
                            }
                        },
                    ))
                })
                .children(servers.into_iter().enumerate().map(|(index, server)| {
                    let counts = self.item_counts.get(&server.id).cloned();
                    let loading = self.selecting_server_id.as_deref() == Some(server.id.as_str());
                    let auto_start = self.cache.auto_start_server_id.as_deref() == Some(&server.id);
                    let placeholder = self
                        .server_reorder
                        .as_ref()
                        .is_some_and(|reorder| reorder.server_id == server.id);
                    let position = self
                        .server_card_positions
                        .entry(server.id.clone())
                        .or_default()
                        .clone();
                    let select_server = cx.listener(Self::select_server);
                    let server_id = server.id.clone();
                    let slot_id = (gpui::ElementId::from("server-slot"), server.id.clone());
                    let slot_selector = format!("server-slot-{index}");
                    let open_menu = cx.listener(move |app, event: &MouseDownEvent, window, cx| {
                        app.open_server_context_menu(&server_id, event.position, window, cx);
                    });
                    let card = server_card(
                        server,
                        counts,
                        ServerCardState {
                            loading,
                            can_reorder,
                            placeholder,
                            auto_start,
                        },
                        cx,
                        ServerCardActions {
                            on_select: select_server,
                            on_context_menu: open_menu,
                        },
                    );
                    div()
                        .id(slot_id)
                        .debug_selector(move || slot_selector.clone())
                        .flex_none()
                        .when(can_reorder, |this| {
                            this.on_drag_move(cx.listener(
                                move |app, event: &DragMoveEvent<DraggedServer>, _, cx| {
                                    if event.drag(cx).owner == cx.entity_id()
                                        && event.bounds.contains(&event.event.position)
                                    {
                                        app.preview_server_reorder(index, cx);
                                    }
                                },
                            ))
                            .on_drop(cx.listener(
                                move |app, drag: &DraggedServer, window, cx| {
                                    if drag.owner == cx.entity_id() {
                                        app.preview_server_reorder(index, cx);
                                        app.finish_server_reorder(true, window, cx);
                                    }
                                },
                            ))
                        })
                        .child(animated_card(
                            card.into_any_element(),
                            position,
                            index,
                            placeholder,
                        ))
                }))
                .child(add_server_card(cx, add_server)),
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

        let theme = theme::get(cx);
        let title = self.title(cx);
        let playback_fullscreen =
            window.is_fullscreen() && matches!(self.page, Page::Playback { .. });
        let rounded_window = window_has_rounded_corners(window);
        let close_dialog = cx.listener(Self::close_add_server_dialog);
        let close_menu = cx.listener(Self::close_server_menu);
        let submit_dialog = cx.listener(Self::submit_add_server_dialog);
        let dialog = self.add_server_dialog.clone();
        let icon_picker = self.render_server_icon_picker(window, cx);
        let modal_open = dialog.is_some() || icon_picker.is_some();

        div()
            .relative()
            .size_full()
            .when(rounded_window, |this| {
                this.rounded(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .bg(theme.background)
                    .when(rounded_window, |this| {
                        this.rounded(theme.radius_lg).overflow_hidden()
                    })
                    .when(!playback_fullscreen, |this| {
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
                    .child(self.render_content(rounded_window, cx)),
            )
            .when_some(dialog, |this, dialog| {
                this.child(dialog.read(cx).render_layer(
                    dialog.clone(),
                    rounded_window,
                    close_dialog,
                    submit_dialog,
                    cx,
                ))
            })
            .when_some(icon_picker, |this, picker| this.child(picker))
            .when(rounded_window && !modal_open, |this| {
                this.children(resize_handles())
            })
            .when(rounded_window, |this| this.child(window_border(cx)))
    }
}
