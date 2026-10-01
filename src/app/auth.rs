use crate::effects::RequestToken;
use crate::server::feature::{
    AuthRequest, AuthResult, ServerCommand, ServerIntent, effect::authenticate_server,
};
use anyhow::Result;
#[cfg(test)]
use gpui::Entity;
use gpui::{AppContext as _, Context};

use crate::{
    home::{HomeEvent, HomePage},
    player::PlaybackRequest,
    server::CachedServer,
};

use super::{Page, TinyApp, intent::AppIntent};

impl TinyApp {
    fn sync_server_selection(&mut self, cx: &mut Context<Self>) {
        let server_id = self.server_feature.selecting_server_id().map(str::to_owned);
        if let Page::Home(home) = self.shell.page() {
            home.update(cx, |home, cx| home.set_selecting_server(server_id, cx));
        }
        cx.notify();
    }

    pub(super) fn cancel_server_selection(&mut self, cx: &mut Context<Self>) {
        self.server_effects.auth.cancel();
        self.server_feature.dispatch(ServerIntent::CancelSelection);
        self.sync_server_selection(cx);
    }

    fn server_selection_error(
        &mut self,
        message: impl Into<gpui::SharedString>,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.shell.page(), Page::Home(_)) {
            self.push_app_error_notification(message, cx);
        } else {
            self.push_server_error_notification(message, cx);
        }
    }

    pub(super) fn show_servers_page_from_home(&mut self, cx: &mut Context<Self>) {
        self.clear_server_menu();
        self.cancel_server_selection(cx);
        self.server_feature.enter_workspace(None);
        self.shell.show_servers();
        cx.notify();
    }

    pub(super) fn begin_select_server(&mut self, server: &CachedServer, cx: &mut Context<Self>) {
        if self.server_icon_picker.is_some() {
            return;
        }
        let command = self
            .server_feature
            .dispatch(ServerIntent::SelectServer(server.id.clone()));
        if matches!(command, ServerCommand::Ignored) {
            return;
        }
        if matches!(command, ServerCommand::Cancelled) {
            self.cancel_server_selection(cx);
            return;
        }
        self.clear_server_menu();
        self.clear_app_notifications();
        self.clear_server_notifications();
        self.server_effects.auth.cancel();
        match command {
            ServerCommand::Open(server) => {
                let id = server.id.clone();
                self.open_home_for_server(*server, cx);
                self.load_item_counts_for_server_id(&id, cx);
                cx.notify();
            }
            ServerCommand::Authenticate(request) => {
                let Some(client) = self.emby_client.clone() else {
                    self.server_feature.selection_failed();
                    self.sync_server_selection(cx);
                    self.server_selection_error("Emby HTTP 客户端不可用", cx);
                    return;
                };
                self.sync_server_selection(cx);
                if !matches!(self.shell.page(), Page::Home(_)) {
                    self.shell.show_servers();
                }
                let task_server = request.server.clone();
                let task =
                    cx.background_spawn(async move { authenticate_server(&client, &task_server) });
                self.server_effects
                    .auth
                    .replace(cx.spawn(async move |app, cx| {
                        let result = task.await;
                        app.update(cx, |app, cx| {
                            app.finish_select_server(request.server, request.token, result, cx)
                        })
                        .ok();
                    }));
            }
            ServerCommand::Ignored
            | ServerCommand::Cancelled
            | ServerCommand::CatalogChanged
            | ServerCommand::DownloadIcon(_)
            | ServerCommand::PreviewChanged => unreachable!(),
        }
    }

    #[cfg(test)]
    fn begin_authentication_token(&mut self, server: &CachedServer) -> RequestToken {
        self.server_feature
            .begin_authentication(server.clone())
            .token
    }

    fn finish_select_server(
        &mut self,
        requested: CachedServer,
        token: RequestToken,
        result: Result<CachedServer>,
        cx: &mut Context<Self>,
    ) {
        let result = match self.server_feature.finish_authentication(
            &AuthRequest {
                server: requested,
                token,
            },
            result,
        ) {
            AuthResult::Ignored => return,
            AuthResult::Invalidated => {
                self.sync_server_selection(cx);
                return;
            }
            AuthResult::Ready(result) => result.map(|server| *server),
        };
        self.sync_server_selection(cx);
        let result = result.and_then(|server| {
            self.save_server(server.clone(), true)?;
            Ok(server)
        });
        match result {
            Ok(server) => {
                self.refresh_saved_server_counts(&server.id, cx);
                self.open_home_for_server(server, cx);
            }
            Err(error) => {
                self.server_feature.selection_failed();
                self.server_selection_error(format!("登录服务器失败：{error}"), cx);
            }
        }
        cx.notify();
    }

    fn open_home_for_server(&mut self, server: CachedServer, cx: &mut Context<Self>) {
        let Some(client) = self.emby_client.clone() else {
            self.cancel_server_selection(cx);
            self.server_selection_error("Emby HTTP 客户端不可用", cx);
            return;
        };

        self.server_feature.enter_workspace(Some(server.id.clone()));
        self.clear_server_notifications();
        let servers = self.server_feature.sidebar_servers();
        let identity = server.workspace_identity();
        let ports = crate::home::HomePorts::new(
            std::sync::Arc::new(crate::home::adapter::EmbyHomeGateway {
                client: client.clone(),
                server: server.clone(),
            }),
            std::sync::Arc::new(crate::player::adapter::EmbyPlaybackGateway {
                client: client.clone(),
                server: server.clone(),
            }),
        );
        let home_page = cx.new(|cx| HomePage::with_ports(server, servers, client, ports, cx));
        home_page.update(cx, |home, cx| {
            home.set_search_history(self.cache.search_history.clone(), cx);
        });
        let token = self.shell.mount_home(home_page.clone(), identity);
        let source = token.clone();
        let subscription = cx.subscribe(
            &home_page,
            move |app: &mut TinyApp, _, event: &HomeEvent, cx| {
                app.dispatch_app_intent(
                    AppIntent::Home {
                        source: source.clone(),
                        event: event.clone(),
                    },
                    cx,
                );
            },
        );
        self.shell.subscribe_home(&token, subscription);
    }

    pub(super) fn open_playback_page(&mut self, request: PlaybackRequest, cx: &mut Context<Self>) {
        if self.shell.home().is_none() {
            return;
        }
        // Playback on the retained page takes precedence over an in-flight switch.
        self.cancel_server_selection(cx);
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
        self.mount_playback_page(playback_page, cx);
    }

    pub(super) fn mount_playback_page(
        &mut self,
        playback_page: gpui::Entity<crate::player::PlaybackPage>,
        cx: &mut Context<Self>,
    ) {
        let token = self
            .shell
            .mount_playback(playback_page.clone())
            .expect("playback retains mounted Home");
        let source = token.clone();
        let subscription = cx.subscribe(
            &playback_page,
            move |app: &mut TinyApp, _, event: &crate::player::PlaybackEvent, cx| {
                app.dispatch_app_intent(
                    AppIntent::Playback {
                        source: source.clone(),
                        event: event.clone(),
                    },
                    cx,
                );
            },
        );
        self.shell.subscribe_playback(&token, subscription);
        cx.notify();
    }

    pub(super) fn return_to_playback_origin(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = self.shell.page() else {
            return;
        };
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
        self.shell.return_home();
        cx.notify();
    }

    pub(super) fn update_playback_origin(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = self.shell.page() else {
            return;
        };
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
    }

    pub(super) fn replace_playback_page(
        &mut self,
        request: PlaybackRequest,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let Page::Playback { return_to, .. } = self.shell.page() else {
            return;
        };
        let return_to = return_to.clone();
        return_to.update(cx, |page, cx| {
            page.apply_playback_update(update, cx);
        });
        self.open_playback_page(request, cx);
    }
}

#[cfg(test)]
mod tests {
    mod search_history;
    mod shell_routes;
    mod sidebar_switch;

    use super::*;
    use crate::{storage::ServerCache, theme};
    use gpui::{Modifiers, TestAppContext, VisualTestContext, px, size};

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
            let response = CachedServer {
                user_id: Some("user-id".into()),
                access_token: Some("old-login-token".into()),
                ..requested.clone()
            };
            let token = app.begin_authentication_token(&requested);
            app.finish_select_server(requested, token, Ok(response), cx);
            assert!(matches!(app.shell.page(), Page::Servers));
            assert!(app.server_feature.selecting_server_id().is_none());
            assert_eq!(
                app.server_feature.catalog().servers[0].password,
                "edited-password"
            );
            assert!(
                app.server_feature.catalog().servers[0]
                    .access_token
                    .is_none()
            );
            assert!(
                app.server_feature.catalog().servers[0]
                    .access_token
                    .is_none()
            );
        });
        assert!(!path.exists(), "a stale response must not write the cache");
    }

    fn sidebar_window(cx: &mut TestAppContext) -> (Entity<TinyApp>, &mut VisualTestContext) {
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
            app.open_home_for_server(app.server_feature.catalog().servers[0].clone(), cx);
            app
        });
        cx.simulate_resize(size(px(1100.0), px(720.0)));
        cx.run_until_parked();
        (app, cx)
    }

    #[gpui::test]
    fn sidebar_switches_with_cached_auth_and_authenticates_without_it(cx: &mut TestAppContext) {
        let (app, cx) = sidebar_window(cx);
        let original_home = app.read_with(cx, |app, _| match app.shell.page() {
            Page::Home(home) => home.entity_id(),
            _ => panic!("expected initial home page"),
        });
        let current = cx.debug_bounds("sidebar-server-first").unwrap();
        cx.simulate_click(current.center(), Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                matches!(app.shell.page(), Page::Home(home) if home.entity_id() == original_home)
            );
        });

        let mut previous_home = original_home;
        for selector in ["sidebar-server-second", "sidebar-server-first"] {
            let target = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(target.center(), Modifiers::default());
            cx.run_until_parked();
            let switched_home = app.read_with(cx, |app, _| {
                let Page::Home(home) = app.shell.page() else {
                    panic!("cached authentication must switch directly to Home");
                };
                assert!(app.server_feature.selecting_server_id().is_none());
                home.entity_id()
            });
            assert_ne!(switched_home, previous_home);
            // Clicking the newly active server must keep its current page and state.
            let current = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(current.center(), Modifiers::default());
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                assert!(matches!(app.shell.page(), Page::Home(home) if home.entity_id() == switched_home));
            });
            previous_home = switched_home;
        }

        let target = cx.debug_bounds("sidebar-server-login-required").unwrap();
        cx.simulate_click(target.center(), Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                matches!(app.shell.page(), Page::Home(home) if home.entity_id() == previous_home)
            );
            assert!(app.server_feature.selecting_server_id().is_none());
            assert!(app.has_app_notifications());
        });
    }
}
