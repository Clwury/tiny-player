use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, Pixels, Point, Window};

use crate::{server::CachedServer, ui::add_server_dialog::AddServerDialogState};

use super::{TinyApp, server_cache::prepare_server, server_card::ServerContextMenu};

impl TinyApp {
    pub(super) fn open_add_server_dialog(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
        self.clear_server_notifications();
        if self.add_server_dialog.is_none() {
            self.add_server_dialog = Some(cx.new(AddServerDialogState::new));
        }
        cx.notify();
    }

    pub(super) fn close_add_server_dialog(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_server_dialog = None;
        cx.notify();
    }

    pub(super) fn close_server_menu(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_server_menu(window, cx);
    }

    pub(super) fn dismiss_server_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = self.open_server_menu.take() {
            if menu.focus.is_focused(window) {
                if let Some(focus) = menu.previous_focus {
                    focus.focus(window, cx);
                } else {
                    window.blur(cx);
                }
            }
            cx.notify();
        }
    }

    pub(super) fn submit_add_server_dialog(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.add_server_dialog.clone() else {
            return;
        };

        let Some(submission) = dialog.update(cx, |dialog, cx| dialog.submit(cx)) else {
            return;
        };

        dialog.update(cx, |dialog, cx| dialog.set_submitting(true, cx));

        let Some(client) = self.emby_client.clone() else {
            dialog.update(cx, |dialog, cx| {
                dialog.set_submitting(false, cx);
                dialog.push_error_notification("Emby HTTP 客户端不可用", cx);
            });
            return;
        };
        let existing = if let Some(server_id) = dialog.read(cx).edit_server_id() {
            let Some(server) = self.servers.iter().find(|server| server.id == server_id) else {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.push_error_notification("服务器不存在", cx);
                });
                return;
            };
            Some(server.clone())
        } else {
            None
        };
        let task =
            cx.background_spawn(
                async move { prepare_server(&client, &submission, existing.as_ref()) },
            );

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| app.finish_save_server(dialog, result, cx))
                .ok();
        })
        .detach();
    }

    pub(super) fn open_server_context_menu(
        &mut self,
        server_id: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting_server_id.as_deref() == Some(server_id)
            || !self.servers.iter().any(|server| server.id == server_id)
        {
            return;
        }
        let (focus, previous_focus) = if let Some(menu) = self.open_server_menu.take() {
            (menu.focus, menu.previous_focus)
        } else {
            (cx.focus_handle(), window.focused(cx))
        };
        focus.focus(window, cx);
        self.open_server_menu = Some(ServerContextMenu {
            server_id: server_id.to_owned(),
            position,
            focus,
            previous_focus,
        });
        cx.notify();
    }

    pub(super) fn open_edit_server_dialog(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
        self.clear_server_notifications();
        if let Some(server) = self.servers.iter().find(|cached| cached.id == server.id) {
            let server = server.clone();
            self.add_server_dialog = Some(cx.new(|cx| AddServerDialogState::new_edit(&server, cx)));
        }
        cx.notify();
    }
}
