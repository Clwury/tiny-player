use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, ensure};
use gpui::{Context, Entity, Window};
use uuid::Uuid;

use crate::{
    emby::EmbyClient,
    server::{AddServerSubmission, CachedServer},
    storage,
    ui::add_server_dialog::AddServerDialogState,
};

use super::{Page, TinyApp};

impl TinyApp {
    pub(super) fn toggle_server_auto_start(
        &mut self,
        server: &CachedServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_server_menu(window, cx);
        if !matches!(self.page, Page::Servers)
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
            || !self
                .cache
                .servers
                .iter()
                .any(|cached| cached.id == server.id)
        {
            return;
        }
        self.cache.auto_start_server_id = (self.cache.auto_start_server_id.as_deref()
            != Some(&server.id))
        .then(|| server.id.clone());
        self.schedule_cache_save("保存自动启动设置失败", cx);
        cx.notify();
    }

    pub(super) fn reorder_server(
        &mut self,
        server_id: &str,
        target_id: &str,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.page, Page::Servers | Page::Home(_))
            || self.selecting_server_id.is_some()
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        let Some(source) = self
            .cache
            .servers
            .iter()
            .position(|server| server.id == server_id)
        else {
            return;
        };
        let Some(target) = self
            .cache
            .servers
            .iter()
            .position(|server| server.id == target_id)
        else {
            return;
        };
        if source == target {
            return;
        }

        // Move the current cached record so a drag never restores stale server data.
        let server = self.cache.servers.remove(source);
        self.cache.servers.insert(target, server);
        self.servers = self.cache.servers.clone();
        self.sync_home_servers(cx);
        self.schedule_cache_save("保存服务器顺序失败", cx);
        cx.notify();
    }

    fn sync_home_servers(&self, cx: &mut Context<Self>) {
        let home = match &self.page {
            Page::Home(home)
            | Page::Playback {
                return_to: home, ..
            } => home,
            Page::Servers => return,
        };
        home.update(cx, |home, cx| home.set_servers(self.servers.clone(), cx));
    }

    pub(super) fn delete_server(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
        let mut cache = self.cache.clone();
        if !storage::delete_server_by_id(&mut cache, &server.id) {
            cx.notify();
            return;
        }

        match storage::save(&cache) {
            Ok(()) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.clear_app_notifications();
            }
            Err(error) => {
                self.push_app_error_notification(format!("删除服务器失败：{error}"), cx);
            }
        }
        cx.notify();
    }

    pub(super) fn finish_save_server(
        &mut self,
        dialog: Entity<AddServerDialogState>,
        result: Result<CachedServer>,
        cx: &mut Context<Self>,
    ) {
        let editing = dialog.read(cx).edit_server_id().is_some();
        let result = result.and_then(|server| {
            let server = if editing {
                let current = self
                    .cache
                    .servers
                    .iter()
                    .find(|current| current.id == server.id)
                    .ok_or_else(|| anyhow!("服务器不存在"))?;
                // Only settings are edited; keep even cache updates received
                // while the dialog was open or its save was pending.
                CachedServer {
                    endpoint: server.endpoint,
                    username: server.username,
                    password: server.password,
                    needs_auth_refresh: true,
                    ..current.clone()
                }
            } else {
                server
            };
            self.save_server(server, editing)
        });
        match result {
            Ok(_) => {
                self.servers = self.cache.servers.clone();
                self.retain_item_count_state();
                self.clear_app_notifications();
                self.clear_server_notifications();
                self.add_server_dialog = None;
                self.cancel_server_selection(cx);
                if editing {
                    self.page = Page::Servers;
                } else {
                    self.sync_home_servers(cx);
                }
            }
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    let action = if editing { "保存" } else { "添加" };
                    dialog.push_error_notification(format!("{action}服务器失败：{error}"), cx);
                });
            }
        }
        cx.notify();
    }

    pub(super) fn save_server(&mut self, server: CachedServer, editing: bool) -> Result<String> {
        // Apply network results to the latest cache so concurrent count refreshes
        // or application settings changes are preserved.
        let mut cache = self.cache.clone();
        let server_id = if editing {
            let id = server.id.clone();
            ensure!(
                storage::update_server_by_id(&mut cache, server),
                "服务器不存在"
            );
            id
        } else {
            storage::upsert_server(&mut cache, server)
        };
        let previous = std::mem::replace(&mut self.cache, cache);
        if let Err(error) = self.save_cache() {
            self.cache = previous;
            return Err(error);
        }
        Ok(server_id)
    }
}

pub(super) fn prepare_server(
    client: &EmbyClient,
    submission: &AddServerSubmission,
    existing: Option<&CachedServer>,
) -> Result<CachedServer> {
    if let Some(existing) = existing {
        return Ok(CachedServer {
            endpoint: submission.endpoint.clone(),
            username: submission.username.clone(),
            password: submission.password.clone(),
            needs_auth_refresh: true,
            ..existing.clone()
        });
    }
    let info = client.public_system_info(submission)?;
    let icon_url = info
        .server_name
        .as_deref()
        .and_then(crate::server::icon::match_icon_url)
        .map(str::to_owned);
    Ok(CachedServer {
        id: Uuid::new_v4().to_string(),
        endpoint: submission.endpoint.clone(),
        username: submission.username.clone(),
        password: submission.password.clone(),
        user_id: None,
        server_id: info.id,
        server_name: info.server_name,
        icon_url,
        icon_is_custom: false,
        access_token: None,
        needs_auth_refresh: false,
        item_counts: None,
        added_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    })
}

pub(super) fn authenticate_server(
    client: &EmbyClient,
    server: &CachedServer,
) -> Result<CachedServer> {
    let submission = AddServerSubmission {
        endpoint: server.endpoint.clone(),
        username: server.username.clone(),
        password: server.password.clone(),
    };
    let session = client.authenticate_by_name(&submission)?;
    let user_id = session
        .user_id()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Emby 认证响应缺少用户 ID"))?;
    ensure!(
        !session.access_token.trim().is_empty(),
        "Emby 认证响应缺少访问令牌"
    );

    // Public metadata fetched on add is reusable. After edits it may describe
    // a different endpoint, so refresh it on entry when authentication omits it.
    let server_name = session.server_name().or_else(|| {
        server
            .server_name
            .clone()
            .filter(|name| !server.needs_auth_refresh && !name.trim().is_empty())
    });
    let info = if server_name.is_none() {
        client.public_system_info(&submission).ok()
    } else {
        None
    };
    let server_name = server_name
        .or_else(|| info.as_ref().and_then(|info| info.server_name.clone()))
        .or_else(|| server.server_name.clone());
    let icon_url = if server.icon_is_custom {
        server.icon_url.clone()
    } else {
        server_name
            .as_deref()
            .and_then(crate::server::icon::match_icon_url)
            .map(str::to_owned)
    };

    Ok(CachedServer {
        user_id: Some(user_id),
        server_id: session
            .server_id()
            .or_else(|| info.and_then(|info| info.id))
            .or_else(|| server.server_id.clone()),
        server_name,
        icon_url,
        access_token: Some(session.access_token),
        needs_auth_refresh: false,
        ..server.clone()
    })
}

#[cfg(test)]
mod tests;
