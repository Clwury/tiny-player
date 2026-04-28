use anyhow::Result;
use gpui::{AppContext as _, Context, Window};

use crate::{
    emby::{AuthSession, EmbyClient},
    home::{HomeEvent, HomePage},
    server::{AddServerSubmission, CachedServer},
    storage,
};

use super::{Page, TinyApp};

impl TinyApp {
    pub(super) fn show_servers_page_from_home(&mut self, cx: &mut Context<Self>) {
        self.open_server_menu = None;
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
        let device_id = self.cache.device_id.clone();
        let server_id = server.id.clone();
        let submission = AddServerSubmission {
            endpoint: server.endpoint.clone(),
            username: server.username.clone(),
            password: server.password.clone(),
        };
        let task = cx.background_spawn(async move {
            let client = EmbyClient::new(device_id)?;
            client.authenticate_by_name(&submission)
        });

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
        match result {
            Ok(session) => {
                self.finish_authenticated_server(server_id.clone(), session, cx);
            }
            Err(error) => {
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

        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.user_id = user_id.clone();
            server.access_token = Some(access_token.clone());
        }

        if let Some(server) = self.servers.iter().find(|server| server.id == server_id) {
            let home_page = HomePage::new(
                server.clone(),
                self.servers.clone(),
                self.cache.device_id.clone(),
            );
            let home_page = cx.new(|_| home_page);
            cx.subscribe(&home_page, |app: &mut TinyApp, _, event, cx| match event {
                HomeEvent::BackToServers => app.show_servers_page_from_home(cx),
                HomeEvent::SectionChanged => cx.notify(),
            })
            .detach();
            self.page = Page::Home(home_page);
        }

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
            self.cache_error = Some("保存登录信息失败：服务器不存在".into());
            return;
        }

        self.item_counts_loading.remove(&server_id);
        self.item_counts_failed.remove(&server_id);
        self.item_counts_refreshed.remove(&server_id);

        match storage::save(&self.cache) {
            Ok(()) => {
                self.cache_error = None;
                self.item_counts_failed.remove(&server_id);
            }
            Err(error) => {
                self.cache_error = Some(format!("保存登录信息失败：{error}").into());
            }
        }
    }
}
