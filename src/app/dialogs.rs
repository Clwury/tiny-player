use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, Pixels, Point, Window};

use crate::{server::CachedServer, server::view::form::AddServerDialogState};

use super::TinyApp;
use crate::server::feature::effect::prepare_server;
use crate::server::{
    feature::{ServerCommand, ServerIntent},
    view::ServerContextMenu,
};

impl TinyApp {
    pub(super) fn show_add_server_dialog(&mut self, cx: &mut Context<Self>) {
        if self.server_icon_picker.is_some() {
            return;
        }
        if matches!(self.shell.page(), super::Page::Home(_)) {
            self.cancel_server_selection(cx);
        }
        self.clear_server_menu();
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
        self.server_effects.save.cancel();
        self.server_feature.cancel_submission();
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

    pub(super) fn clear_server_menu(&mut self) {
        self.open_server_menu = None;
        self.server_feature.dispatch(ServerIntent::CloseMenu);
    }

    pub(super) fn dismiss_server_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.server_feature.dispatch(ServerIntent::CloseMenu);
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
        let request = match self
            .server_feature
            .prepare_submission(submission, dialog.read(cx).edit_server_id().as_deref())
        {
            Ok(request) => request,
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.push_error_notification(error.to_string(), cx);
                });
                return;
            }
        };
        let submission = request.submission.clone();
        let existing = request.existing.clone();
        let task =
            cx.background_spawn(
                async move { prepare_server(&client, &submission, existing.as_ref()) },
            );
        self.server_effects
            .save
            .replace(cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| {
                    if app.add_server_dialog.as_ref() != Some(&dialog)
                        || !app.server_feature.finish_submission(&request)
                    {
                        return;
                    }
                    app.finish_save_server(dialog, result, cx);
                })
                .ok();
            }));
    }

    pub(super) fn open_server_context_menu(
        &mut self,
        server_id: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.server_icon_picker.is_some() || self.add_server_dialog.is_some() {
            return;
        }
        if !matches!(
            self.server_feature
                .dispatch(ServerIntent::OpenMenu(server_id.into())),
            ServerCommand::PreviewChanged
        ) {
            return;
        }
        let (focus, previous_focus) = if let Some(menu) = self.open_server_menu.take() {
            (menu.focus, menu.previous_focus)
        } else {
            (cx.focus_handle(), window.focused(cx))
        };
        focus.focus(window, cx);
        self.open_server_menu = Some(ServerContextMenu {
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
        if self.server_icon_picker.is_some() {
            return;
        }
        self.clear_server_menu();
        self.clear_server_notifications();
        if let Some(props) = self.server_feature.edit_form(&server.id) {
            self.server_effects.save.cancel();
            self.server_feature.cancel_submission();
            self.add_server_dialog = Some(cx.new(|cx| AddServerDialogState::from_props(props, cx)));
        }
        cx.notify();
    }
}
