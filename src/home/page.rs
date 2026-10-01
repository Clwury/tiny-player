//! Mounted Home page and app-facing event binding.
use super::*;

impl HomePage {
    #[cfg(test)]
    pub(crate) fn new(
        current_server: CachedServer,
        servers: Vec<crate::server::SidebarServer>,
        emby_client: EmbyClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let ports = super::test_support::ports(&current_server, &emby_client);
        Self::with_ports(current_server, servers, emby_client, ports, cx)
    }

    pub(crate) fn with_ports(
        current_server: CachedServer,
        servers: Vec<crate::server::SidebarServer>,
        emby_client: EmbyClient,
        ports: HomePorts,
        cx: &mut Context<Self>,
    ) -> Self {
        let home_content =
            cx.new(|cx| HomeContent::with_ports(current_server.clone(), emby_client, ports, cx));
        cx.subscribe(
            &home_content,
            |_: &mut HomePage, _, event, cx| match event {
                HomeContentEvent::TitleChanged => cx.emit(HomeEvent::TitleChanged),
                HomeContentEvent::SearchHistoryChanged(history) => {
                    cx.emit(HomeEvent::SearchHistoryChanged(history.clone()))
                }
                HomeContentEvent::OpenPlayback(request) => {
                    cx.emit(HomeEvent::OpenPlayback(request.clone()))
                }
            },
        )
        .detach();
        let mut page = Self {
            sidebar: sidebar::controller::SidebarController::new(
                current_server.id,
                current_server.username,
                servers,
            ),
            sidebar_scroll_handle: ScrollHandle::new(),
            sidebar_reorder: None,
            home_content,
        };
        page.start_effects(cx);
        page
    }

    pub fn title(&self, cx: &App) -> SharedString {
        self.home_content.read(cx).title()
    }

    pub(crate) fn set_servers(
        &mut self,
        servers: Vec<crate::server::SidebarServer>,
        cx: &mut Context<Self>,
    ) {
        self.apply_sidebar_intent(
            sidebar::controller::SidebarIntent::ServersChanged(servers),
            cx,
        );
    }

    pub(crate) fn set_search_history(&mut self, history: SearchHistory, cx: &mut Context<Self>) {
        self.home_content.update(cx, |content, cx| {
            content.controller.restore_search_history(history);
            cx.notify();
        });
    }

    #[cfg(test)]
    pub(crate) fn current_server_id(&self) -> &str {
        self.sidebar.current_server_id()
    }

    pub(crate) fn set_selecting_server(
        &mut self,
        server_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_sidebar_intent(
            sidebar::controller::SidebarIntent::SelectingChanged(server_id),
            cx,
        );
    }

    pub(crate) fn start_effects(&mut self, cx: &mut Context<Self>) {
        self.home_content
            .update(cx, |content, cx| content.start_effects(cx));
    }

    pub(super) fn set_active_section(
        &mut self,
        section: HomeRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self
            .home_content
            .update(cx, |content, cx| content.select_root(section, window, cx));
        if changed {
            cx.emit(HomeEvent::SectionChanged);
            cx.notify();
        }
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        self.home_content.update(cx, |content, cx| {
            content.apply_playback_update(update, cx);
        });
    }
}
