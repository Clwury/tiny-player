use super::{Page, TinyApp};
#[cfg(test)]
use crate::server::CachedServer;
use crate::server::feature::{ServerCommand, ServerIntent};
use gpui::{Context, FocusHandle, Window};

pub(super) struct ServerReorder {
    pub(super) focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

impl TinyApp {
    pub(super) fn begin_server_reorder(
        &mut self,
        server_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.shell.page(), Page::Servers)
            || self.server_feature.selecting_server_id().is_some()
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        if !matches!(
            self.server_feature
                .dispatch(ServerIntent::BeginReorder(server_id.into())),
            ServerCommand::PreviewChanged
        ) {
            return;
        }
        self.dismiss_server_menu(window, cx);
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        self.server_reorder = Some(ServerReorder {
            focus,
            previous_focus,
        });
        cx.notify();
    }

    pub(super) fn preview_server_reorder(&mut self, index: usize, cx: &mut Context<Self>) {
        if matches!(
            self.server_feature
                .dispatch(ServerIntent::ReorderPreview(index)),
            ServerCommand::PreviewChanged
        ) {
            cx.notify();
        }
    }

    #[cfg(test)]
    pub(super) fn preview_servers(&self) -> Vec<CachedServer> {
        self.server_feature.preview_servers()
    }

    pub(super) fn finish_server_reorder(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = self.server_reorder.take() else {
            return;
        };
        let commit = commit
            && matches!(self.shell.page(), Page::Servers | Page::Home(_))
            && self.server_feature.selecting_server_id().is_none()
            && self.add_server_dialog.is_none()
            && self.server_icon_picker.is_none();
        if matches!(
            self.server_feature
                .dispatch(ServerIntent::CommitReorder(commit)),
            ServerCommand::CatalogChanged
        ) {
            self.sync_home_servers(cx);
            self.schedule_cache_save("保存服务器顺序失败", cx);
        }
        if reorder.focus.is_focused(window) {
            if let Some(focus) = reorder.previous_focus {
                focus.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
        cx.notify();
    }
}
