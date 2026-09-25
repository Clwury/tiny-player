use gpui::{
    Context, EntityId, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

use crate::{
    server::CachedServer,
    theme,
    ui::{radius, server_icon::server_icon},
};

use super::{HomeEvent, HomePage, server_title};

#[derive(Clone, Debug)]
pub(crate) struct SidebarReorder {
    pub(super) server_id: String,
    target_index: usize,
    pub(super) focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

#[derive(Clone)]
pub(super) struct DraggedSidebarServer {
    pub(super) owner: EntityId,
    pub(super) server_id: String,
    title: String,
    icon_url: Option<String>,
}

impl DraggedSidebarServer {
    pub(super) fn new(server: &CachedServer, owner: EntityId) -> Self {
        Self {
            owner,
            server_id: server.id.clone(),
            title: server_title(server),
            icon_url: server.icon_url.clone(),
        }
    }
}

impl Render for DraggedSidebarServer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .flex()
            .w(px(super::super::carousel::HOME_SIDEBAR_WIDTH_PX - 24.0))
            .h(px(36.0))
            .items_center()
            .gap_2()
            .px_3()
            .rounded(radius::CONTROL)
            .bg(theme.dialog_background)
            .shadow_lg()
            .opacity(0.9)
            .text_sm()
            .text_color(theme.foreground)
            .child(server_icon(self.icon_url.as_deref(), 18.0))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .child(self.title.clone()),
            )
    }
}

impl HomePage {
    pub(super) fn begin_sidebar_reorder(
        &mut self,
        server_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target_index) = self
            .servers
            .iter()
            .position(|server| server.id == server_id)
        else {
            return;
        };
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        self.sidebar_reorder = Some(SidebarReorder {
            server_id: server_id.into(),
            target_index,
            focus,
            previous_focus,
        });
        cx.notify();
    }

    pub(super) fn preview_sidebar_reorder(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(reorder) = &mut self.sidebar_reorder
            && index < self.servers.len()
            && reorder.target_index != index
        {
            reorder.target_index = index;
            cx.notify();
        }
    }

    pub(super) fn preview_sidebar_servers(&self) -> Vec<CachedServer> {
        let mut servers = self.servers.clone();
        if let Some(reorder) = &self.sidebar_reorder
            && let Some(source) = servers
                .iter()
                .position(|server| server.id == reorder.server_id)
        {
            let target = reorder.target_index.min(servers.len() - 1);
            let server = servers.remove(source);
            servers.insert(target, server);
        }
        servers
    }

    pub(in crate::home) fn finish_sidebar_reorder(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = self.sidebar_reorder.take() else {
            return;
        };
        if commit
            && let Some(target) = self.servers.get(reorder.target_index)
            && target.id != reorder.server_id
        {
            cx.emit(HomeEvent::ReorderServer {
                server_id: reorder.server_id,
                target_id: target.id.clone(),
            });
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
