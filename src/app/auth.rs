use anyhow::Result;
use gpui::{AppContext as _, Context, Window};

use crate::{
    emby::AuthSession,
    home::{HomeEvent, HomePage},
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
        self.open_server_menu = None;
        self.cache_error = None;

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
            self.cache_error = Some("Emby HTTP 客户端不可用".into());
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
                self.cache_error = Some(format!("登录服务器失败：{error}").into());
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
            self.cache_error = Some("登录服务器失败：服务器不存在".into());
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
            self.cache_error = Some("保存登录信息失败：服务器不存在".into());
            self.page = Page::Servers;
            return;
        }

        self.open_home_for_server(home_server, cx);

        self.item_counts_loading.remove(&server_id);
        self.item_counts_failed.remove(&server_id);
        self.item_counts_refreshed.remove(&server_id);

        match storage::save(&self.cache) {
            Ok(()) => {
                self.cache_error = None;
                self.item_counts_failed.remove(&server_id);
                self.load_item_counts_for_server_id(&server_id, cx);
            }
            Err(error) => {
                self.cache_error = Some(format!("保存登录信息失败：{error}").into());
            }
        }
    }

    fn open_home_for_server(&mut self, server: CachedServer, cx: &mut Context<Self>) {
        let Some(client) = self.emby_client.clone() else {
            self.selecting_server_id = None;
            self.cache_error = Some("Emby HTTP 客户端不可用".into());
            self.page = Page::Servers;
            return;
        };

        self.selecting_server_id = None;
        let servers = self.servers.clone();
        let home_page = cx.new(|cx| HomePage::new(server, servers, client, cx));
        cx.subscribe(&home_page, |app: &mut TinyApp, _, event, cx| match event {
            HomeEvent::BackToServers => app.show_servers_page_from_home(cx),
            HomeEvent::SectionChanged => cx.notify(),
        })
        .detach();
        self.page = Page::Home(home_page);
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
