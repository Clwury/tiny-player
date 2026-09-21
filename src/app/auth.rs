use anyhow::Result;
use gpui::{AppContext as _, Context, Entity, Task, Window};

use crate::{
    home::{HomeEvent, HomePage},
    player::{PlaybackEvent, PlaybackRequest},
    server::CachedServer,
};

use super::{Page, TinyApp, server_cache::authenticate_server};

impl TinyApp {
    pub(super) fn show_servers_page_from_home(&mut self, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        self.selecting_server_id = None;
        self.select_server_task = Task::ready(());
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
        // Cards and sidebar items can hold snapshots from before a server edit.
        let Some(server) = self
            .servers
            .iter()
            .find(|current| current.id == server.id)
            .cloned()
        else {
            return;
        };
        if self.selecting_server_id.as_deref() == Some(&server.id) {
            return;
        }
        self.open_server_menu = None;
        self.clear_app_notifications();
        self.clear_server_notifications();

        let server_id = server.id.clone();
        if server.can_reuse_auth() {
            self.selecting_server_id = None;
            self.select_server_task = Task::ready(());
            self.open_home_for_server(server.clone(), cx);
            self.load_item_counts_for_server_id(&server_id, cx);
            cx.notify();
            return;
        }

        let Some(client) = self.emby_client.clone() else {
            self.push_server_error_notification("Emby HTTP 客户端不可用", cx);
            cx.notify();
            return;
        };
        self.selecting_server_id = Some(server_id);
        self.page = Page::Servers;
        cx.notify();

        let request = server.clone();
        let task = cx.background_spawn(async move { authenticate_server(&client, &request) });
        self.select_server_task = cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| app.finish_select_server(server, result, cx))
                .ok();
        });
    }

    fn finish_select_server(
        &mut self,
        requested: CachedServer,
        result: Result<CachedServer>,
        cx: &mut Context<Self>,
    ) {
        if self.selecting_server_id.as_deref() != Some(&requested.id) {
            return;
        }
        self.selecting_server_id = None;
        // An edit or deletion while the request was running invalidates its result.
        if !self.servers.iter().any(|current| {
            current.id == requested.id
                && current.endpoint == requested.endpoint
                && current.username == requested.username
                && current.password == requested.password
                && current.needs_auth_refresh == requested.needs_auth_refresh
        }) {
            cx.notify();
            return;
        }

        let result = result.and_then(|server| {
            self.save_server(server.clone(), true)?;
            Ok(server)
        });
        match result {
            Ok(server) => {
                self.servers = self.cache.servers.clone();
                self.refresh_saved_server_counts(&server.id, cx);
                self.open_home_for_server(server, cx);
            }
            Err(error) => {
                self.push_server_error_notification(format!("登录服务器失败：{error}"), cx);
                self.page = Page::Servers;
            }
        }
        cx.notify();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage::ServerCache, theme};
    use gpui::{Modifiers, TestAppContext, px, size};

    #[gpui::test]
    fn late_authentication_cannot_overwrite_edited_credentials(cx: &mut TestAppContext) {
        cx.update(theme::init);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let requested: CachedServer = serde_json::from_value(serde_json::json!({
            "id": "server", "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "user", "password": "old-password", "added_at_unix": 0
        })).unwrap();
        let edited = CachedServer {
            password: "edited-password".into(),
            ..requested.clone()
        };
        let app = cx.new(|cx| {
            let mut cache = ServerCache::empty();
            cache.servers.push(edited);
            let mut app = TinyApp::new(cache, None, cx);
            app.cache_save_path = Some(path.clone());
            app
        });
        app.update(cx, |app, cx| {
            app.selecting_server_id = Some(requested.id.clone());
            let response = CachedServer {
                user_id: Some("user-id".into()),
                access_token: Some("old-login-token".into()),
                ..requested.clone()
            };
            app.finish_select_server(requested, Ok(response), cx);
            assert!(matches!(app.page, Page::Servers));
            assert!(app.selecting_server_id.is_none());
            assert_eq!(app.servers[0].password, "edited-password");
            assert!(app.servers[0].access_token.is_none());
            assert!(app.cache.servers[0].access_token.is_none());
        });
        assert!(!path.exists(), "a stale response must not write the cache");
    }

    #[gpui::test]
    fn sidebar_switches_with_cached_auth_and_authenticates_without_it(cx: &mut TestAppContext) {
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
