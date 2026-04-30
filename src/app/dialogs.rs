use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, Window};

use crate::{server::CachedServer, ui::add_server_dialog::AddServerDialogState};

use super::{
    TinyApp,
    server_cache::{fetch_public_info_and_cache, fetch_public_info_and_update_cache},
};

impl TinyApp {
    pub(super) fn open_add_server_dialog(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
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
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_server_menu.take().is_some() {
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

        dialog.update(cx, |dialog, cx| {
            dialog.clear_form_error(cx);
            dialog.set_submitting(true, cx);
        });

        let Some(client) = self.emby_client.clone() else {
            dialog.update(cx, |dialog, cx| {
                dialog.set_submitting(false, cx);
                dialog.set_form_error("Emby HTTP 客户端不可用", cx);
            });
            return;
        };
        let cache = self.cache.clone();
        if let Some(server_id) = dialog.read(cx).edit_server_id() {
            let task = cx.background_spawn(async move {
                fetch_public_info_and_update_cache(client, cache, server_id, submission)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| app.finish_edit_server(dialog, result, cx))
                    .ok();
            })
            .detach();
        } else {
            let task = cx.background_spawn(async move {
                fetch_public_info_and_cache(client, cache, submission)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| app.finish_add_server(dialog, result, cx))
                    .ok();
            })
            .detach();
        }
    }

    pub(super) fn toggle_server_menu(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = if self.open_server_menu.as_deref() == Some(server.id.as_str()) {
            None
        } else {
            Some(server.id.clone())
        };
        cx.notify();
    }

    pub(super) fn open_edit_server_dialog(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
        if let Some(server) = self.servers.iter().find(|cached| cached.id == server.id) {
            let server = server.clone();
            self.add_server_dialog = Some(cx.new(|cx| AddServerDialogState::new_edit(&server, cx)));
        }
        cx.notify();
    }
}
