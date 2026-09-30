//! App-facing compatibility events enter through mount validation before the
//! shell coordinates routes, overlays, persistence or another feature.
use super::TinyApp;
use crate::{effects::RequestToken, home::HomeEvent, player::PlaybackEvent};
use gpui::Context;

pub(super) enum AppIntent {
    Home {
        source: RequestToken,
        event: HomeEvent,
    },
    Playback {
        source: RequestToken,
        event: PlaybackEvent,
    },
}

impl TinyApp {
    pub(super) fn dispatch_app_intent(&mut self, intent: AppIntent, cx: &mut Context<Self>) {
        match intent {
            AppIntent::Home { source, event } => {
                if !self.shell.accepts_home(&source) {
                    return;
                }
                event.trace();
                match event {
                    HomeEvent::BackToServers => self.show_servers_page_from_home(cx),
                    HomeEvent::AddServer => self.show_add_server_dialog(cx),
                    HomeEvent::ReorderServer {
                        server_id,
                        target_id,
                    } => self.reorder_server(&server_id, &target_id, cx),
                    HomeEvent::SwitchServer(id) => {
                        if let Some(server) = self.server_feature.server(&id).cloned() {
                            self.begin_select_server(&server, cx);
                        }
                    }
                    HomeEvent::SectionChanged | HomeEvent::TitleChanged => cx.notify(),
                    HomeEvent::SearchHistoryChanged(history) => {
                        self.cache.search_history = history;
                        self.schedule_config_save(
                            crate::persistence::DirtyKey::SearchHistory,
                            "保存搜索历史失败",
                            cx,
                        );
                    }
                    HomeEvent::OpenSettings => self.open_settings_window(cx),
                    HomeEvent::OpenPlayback(request) => self.open_playback_page(*request, cx),
                }
            }
            AppIntent::Playback { source, event } => {
                if !self.shell.accepts_playback(&source) {
                    return;
                }
                event.trace();
                match event {
                    PlaybackEvent::VolumeChanged { settings } => {
                        self.update_playback_volume(settings, cx)
                    }
                    PlaybackEvent::Update { update } => self.update_playback_origin(update, cx),
                    PlaybackEvent::Back { update } => self.return_to_playback_origin(update, cx),
                    PlaybackEvent::Replace { request, update } => {
                        self.replace_playback_page(*request, update, cx)
                    }
                }
            }
        }
    }
}
