use anyhow::Result;
use gpui::{AppContext as _, Context, Entity, Window};

use crate::{
    emby::AuthSession,
    home::{HomeEvent, HomePage},
    player::{PlaybackEvent, PlaybackRequest},
    server::{AddServerSubmission, CachedServer},
    storage,
};

use super::{Page, TinyApp};

impl TinyApp {
    pub(super) fn show_servers_page_from_home(&mut self, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        self.selecting_server_id = None;
        self.page = Page::Servers;
        cx.notify();
    }

    pub(super) fn select_server(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_select_server(server, cx);
    }

    pub(super) fn begin_select_server(&mut self, server: &CachedServer, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        self.clear_app_notifications();
        self.clear_server_notifications();

        let server_id = server.id.clone();
        if has_cached_auth(server) {
            self.selecting_server_id = None;
            self.open_home_for_server(server.clone(), cx);
            self.load_item_counts_for_server_id(&server_id, cx);
            cx.notify();
            return;
        }

        if self.is_selecting_server(&server_id) {
            cx.notify();
            return;
        }

        let Some(client) = self.emby_client.clone() else {
            self.selecting_server_id = None;
            self.push_server_error_notification("Emby HTTP 客户端不可用", cx);
            cx.notify();
            return;
        };

        self.selecting_server_id = Some(server_id.clone());
        self.page = Page::Servers;
        cx.notify();

        let submission = AddServerSubmission {
            endpoint: server.endpoint.clone(),
            username: server.username.clone(),
            password: server.password.clone(),
        };
        let task = cx.background_spawn(async move { client.authenticate_by_name(&submission) });

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| {
                app.finish_select_server(server_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_select_server(
        &mut self,
        server_id: String,
        result: Result<AuthSession>,
        cx: &mut Context<Self>,
    ) {
        if !self.is_selecting_server(&server_id) {
            return;
        }

        match result {
            Ok(session) => {
                self.finish_authenticated_server(server_id.clone(), session, cx);
            }
            Err(error) => {
                self.selecting_server_id = None;
                self.push_server_error_notification(format!("登录服务器失败：{error}"), cx);
                self.page = Page::Servers;
            }
        }

        cx.notify();
    }

    fn finish_authenticated_server(
        &mut self,
        server_id: String,
        session: AuthSession,
        cx: &mut Context<Self>,
    ) {
        let user_id = session.user_id();
        let access_token = session.access_token;
        let mut updated_cache = false;
        let mut home_server = None;

        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.user_id = user_id.clone();
            server.access_token = Some(access_token.clone());
            home_server = Some(server.clone());
        }

        let Some(home_server) = home_server else {
            self.selecting_server_id = None;
            self.push_server_error_notification("登录服务器失败：服务器不存在", cx);
            self.page = Page::Servers;
            return;
        };

        if let Some(server) = self
            .cache
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.user_id = user_id;
            server.access_token = Some(access_token);
            updated_cache = true;
        }

        if !updated_cache {
            self.selecting_server_id = None;
            self.push_server_error_notification("保存登录信息失败：服务器不存在", cx);
            self.page = Page::Servers;
            return;
        }

        self.open_home_for_server(home_server, cx);

        self.item_counts_loading.remove(&server_id);
        self.item_counts_failed.remove(&server_id);
        self.item_counts_refreshed.remove(&server_id);

        match storage::save(&self.cache) {
            Ok(()) => {
                self.clear_app_notifications();
                self.clear_server_notifications();
                self.item_counts_failed.remove(&server_id);
                self.load_item_counts_for_server_id(&server_id, cx);
            }
            Err(error) => {
                self.push_app_error_notification(format!("保存登录信息失败：{error}"), cx);
            }
        }
    }

    fn open_home_for_server(&mut self, server: CachedServer, cx: &mut Context<Self>) {
        let Some(client) = self.emby_client.clone() else {
            self.selecting_server_id = None;
            self.push_server_error_notification("Emby HTTP 客户端不可用", cx);
            self.page = Page::Servers;
            return;
        };

        self.selecting_server_id = None;
        self.clear_server_notifications();
        let servers = self.servers.clone();
        let home_page = cx.new(|cx| HomePage::new(server, servers, client, cx));
        let playback_return_to = home_page.clone();
        cx.subscribe(
            &home_page,
            move |app: &mut TinyApp, _, event, cx| match event {
                HomeEvent::BackToServers => app.show_servers_page_from_home(cx),
                HomeEvent::SwitchServer(server_id) => {
                    if let Some(server) = app
                        .servers
                        .iter()
                        .find(|server| &server.id == server_id)
                        .cloned()
                    {
                        app.begin_select_server(&server, cx);
                    }
                }
                HomeEvent::SectionChanged | HomeEvent::TitleChanged => cx.notify(),
                HomeEvent::OpenSettings => app.open_settings_window(cx),
                HomeEvent::OpenPlayback(request) => {
                    app.open_playback_page(playback_return_to.clone(), request.as_ref().clone(), cx)
                }
            },
        )
        .detach();
        self.page = Page::Home(home_page);
    }

    fn open_playback_page(
        &mut self,
        return_to: Entity<HomePage>,
        request: PlaybackRequest,
        cx: &mut Context<Self>,
    ) {
        let playback_cache_config = self.cache.playback.clone();
        let playback_volume = self.cache.playback_volume;
        let playback_page = cx.new(|cx| {
            crate::player::PlaybackPage::new_with_settings(
                request,
                playback_cache_config,
                playback_volume,
                cx,
            )
        });
        cx.subscribe(
            &playback_page,
            |app: &mut TinyApp, _, event, cx| match event {
                PlaybackEvent::VolumeChanged { settings } => {
                    app.update_playback_volume(*settings, cx);
                }
                PlaybackEvent::Update { update } => app.update_playback_origin(update.clone(), cx),
                PlaybackEvent::Back { update } => app.return_to_playback_origin(update.clone(), cx),
                PlaybackEvent::Replace { request, update } => {
                    app.replace_playback_page(request.as_ref().clone(), update.clone(), cx)
                }
            },
        )
        .detach();
        self.page = Page::Playback {
            page: playback_page,
            return_to,
        };
        cx.notify();
    }

    fn return_to_playback_origin(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = &self.page else {
            return;
        };
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
        self.page = Page::Home(return_to.clone());
        cx.notify();
    }

    fn update_playback_origin(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = &self.page else {
            return;
        };
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
    }

    fn replace_playback_page(
        &mut self,
        request: PlaybackRequest,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = &self.page else {
            return;
        };
        let return_to = return_to.clone();
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
        self.open_playback_page(return_to, request, cx);
    }

    fn is_selecting_server(&self, server_id: &str) -> bool {
        self.selecting_server_id.as_deref() == Some(server_id)
    }
}

fn has_cached_auth(server: &CachedServer) -> bool {
    server
        .user_id
        .as_deref()
        .is_some_and(|user_id| !user_id.is_empty())
        && server
            .access_token
            .as_deref()
            .is_some_and(|access_token| !access_token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage::ServerCache, theme};
    use gpui::{Modifiers, TestAppContext, px, size};

    #[gpui::test]
    fn sidebar_switches_servers_and_uses_the_existing_login_flow(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let (app, cx) = cx.add_window_view(|_, cx| {
            let mut cache = ServerCache::empty();
            cache.servers = ["first", "second", "login-required"]
                .into_iter()
                .map(|id| {
                    serde_json::from_value(serde_json::json!({
                        "id": id, "server_name": id,
                        // An invalid endpoint makes all requests fail locally, without network access.
                        "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
                        "username": id, "password": "", "user_id": id,
                        "access_token": (id != "login-required").then_some("test-token"),
                        "added_at_unix": 0
                    }))
                    .unwrap()
                })
                .collect();
            let mut app = TinyApp::new(cache, None, cx);
            app.window_persistence_enabled = false;
            app.open_home_for_server(app.servers[0].clone(), cx);
            app
        });
        cx.simulate_resize(size(px(1100.0), px(720.0)));
        cx.run_until_parked();
        let original_home = app.read_with(cx, |app, _| match &app.page {
            Page::Home(home) => home.entity_id(),
            _ => panic!("expected initial home page"),
        });
        let current = cx.debug_bounds("sidebar-server-first").unwrap();
        cx.simulate_click(current.center(), Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(&app.page, Page::Home(home) if home.entity_id() == original_home));
        });

        let mut previous_home = original_home;
        for selector in ["sidebar-server-second", "sidebar-server-first"] {
            let target = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(target.center(), Modifiers::default());
            cx.run_until_parked();
            let switched_home = app.read_with(cx, |app, _| {
                let Page::Home(home) = &app.page else {
                    panic!("cached authentication must switch directly to Home");
                };
                assert!(app.selecting_server_id.is_none());
                home.entity_id()
            });
            assert_ne!(switched_home, previous_home);
            // Clicking the newly active server must keep its current page and state.
            let current = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(current.center(), Modifiers::default());
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                assert!(matches!(&app.page, Page::Home(home) if home.entity_id() == switched_home));
            });
            previous_home = switched_home;
        }

        let target = cx.debug_bounds("sidebar-server-login-required").unwrap();
        cx.simulate_click(target.center(), Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Servers));
            assert!(app.selecting_server_id.is_none());
            assert!(app.has_server_page_notifications());
        });
    }
}
