use super::TinyApp;
use crate::server::view::{ServerViewIntent, ServerViewListener};
use gpui::{App, Context, Window};

impl TinyApp {
    pub(super) fn server_view_listener(&self, cx: &Context<Self>) -> ServerViewListener {
        let listener = cx.listener(Self::handle_server_view_intent);
        std::rc::Rc::new(move |intent, window: &mut Window, cx: &mut App| {
            listener(&intent, window, cx)
        })
    }

    /// Shell dispatch for window overlays and existing app-facing callbacks.
    /// Resolve IDs at delivery so events never retain credentials or old data.
    fn handle_server_view_intent(
        &mut self,
        intent: &ServerViewIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            ServerViewIntent::Add => self.show_add_server_dialog(cx),
            ServerViewIntent::OpenMenu {
                server_id,
                position,
            } => self.open_server_context_menu(server_id, *position, window, cx),
            ServerViewIntent::CloseMenu => self.dismiss_server_menu(window, cx),
            ServerViewIntent::BeginReorder(id) => self.begin_server_reorder(id, window, cx),
            ServerViewIntent::PreviewReorder(index) => self.preview_server_reorder(*index, cx),
            ServerViewIntent::CommitReorder(commit) => {
                self.finish_server_reorder(*commit, window, cx)
            }
            ServerViewIntent::Select(id)
            | ServerViewIntent::Edit(id)
            | ServerViewIntent::ChooseIcon(id)
            | ServerViewIntent::Delete(id)
            | ServerViewIntent::ToggleAutoStart(id) => {
                let Some(server) = self.server_feature.server(id).cloned() else {
                    return;
                };
                match intent {
                    ServerViewIntent::Select(_) => self.begin_select_server(&server, cx),
                    ServerViewIntent::Edit(_) => self.open_edit_server_dialog(&server, window, cx),
                    ServerViewIntent::ChooseIcon(_) => {
                        self.open_server_icon_picker(&server, window, cx)
                    }
                    ServerViewIntent::Delete(_) => self.delete_server(&server, window, cx),
                    ServerViewIntent::ToggleAutoStart(_) => {
                        self.toggle_server_auto_start(&server, window, cx)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}
