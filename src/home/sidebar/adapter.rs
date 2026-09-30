//! GPUI input/focus adapter. The sidebar component receives no HomePage or server
//! credentials; it emits view intents through this page-owned callback.
use super::{SidebarListener, SidebarViewIntent, reorder::SidebarReorder};
use crate::home::{
    HomeEvent, HomePage,
    model::sidebar::{SidebarCommand, SidebarIntent},
};
use gpui::{Context, Window};

impl HomePage {
    pub(in crate::home) fn sidebar_listener(&self, cx: &Context<Self>) -> SidebarListener {
        let listener = cx.listener(Self::handle_sidebar_intent);
        std::rc::Rc::new(move |intent, window, cx| listener(&intent, window, cx))
    }

    fn handle_sidebar_intent(
        &mut self,
        intent: &SidebarViewIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            SidebarViewIntent::Back => cx.emit(HomeEvent::BackToServers),
            SidebarViewIntent::Add => cx.emit(HomeEvent::AddServer),
            SidebarViewIntent::Settings => cx.emit(HomeEvent::OpenSettings),
            SidebarViewIntent::Root { root, refresh } => {
                self.set_active_section(*root, window, cx);
                if *refresh {
                    self.home_content
                        .update(cx, |content, cx| content.refresh_home_content(cx));
                    window.refresh();
                }
            }
            SidebarViewIntent::Select(id) => {
                self.apply_sidebar_intent(SidebarIntent::Select(id.clone()), cx)
            }
            SidebarViewIntent::PreviewReorder(index) => {
                self.apply_sidebar_intent(SidebarIntent::PreviewReorder(*index), cx)
            }
            SidebarViewIntent::BeginReorder(id) => {
                if self
                    .sidebar
                    .dispatch(SidebarIntent::BeginReorder(id.clone()))
                    == SidebarCommand::Changed
                {
                    let focus = cx.focus_handle();
                    let previous_focus = window.focused(cx);
                    focus.focus(window, cx);
                    self.sidebar_reorder = Some(SidebarReorder {
                        focus,
                        previous_focus,
                    });
                    cx.notify();
                }
            }
            SidebarViewIntent::FinishReorder(commit) => {
                self.finish_sidebar_reorder(*commit, window, cx)
            }
        }
    }

    pub(in crate::home) fn apply_sidebar_intent(
        &mut self,
        intent: SidebarIntent,
        cx: &mut Context<Self>,
    ) {
        match self.sidebar.dispatch(intent) {
            SidebarCommand::None => {}
            SidebarCommand::Changed => cx.notify(),
            SidebarCommand::Select(id) => cx.emit(HomeEvent::SwitchServer(id)),
            SidebarCommand::Reorder {
                server_id,
                target_id,
            } => {
                cx.emit(HomeEvent::ReorderServer {
                    server_id,
                    target_id,
                });
                cx.notify();
            }
        }
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
        self.apply_sidebar_intent(SidebarIntent::FinishReorder(commit), cx);
        if reorder.focus.is_focused(window) {
            if let Some(focus) = reorder.previous_focus {
                focus.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
    }
}
