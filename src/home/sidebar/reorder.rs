use gpui::{
    Context, EntityId, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};

use crate::{
    server::feature::SidebarServer,
    theme,
    ui::{radius, server_icon::server_icon},
};

#[derive(Clone, Debug)]
pub(crate) struct SidebarReorder {
    pub(in crate::home) focus: FocusHandle,
    pub(super) previous_focus: Option<FocusHandle>,
}

#[derive(Clone)]
pub(super) struct DraggedSidebarServer {
    pub(super) owner: EntityId,
    pub(super) server_id: String,
    title: String,
    icon_url: Option<String>,
}

impl DraggedSidebarServer {
    pub(super) fn new(server: &SidebarServer, owner: EntityId) -> Self {
        Self {
            owner,
            server_id: server.id.clone(),
            title: server.title.clone(),
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
