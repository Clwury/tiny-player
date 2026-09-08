use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use gpui::{Context, Entity, Window};
use uuid::Uuid;

use crate::{
    emby::{EmbyClient, PublicSystemInfo},
    server::{AddServerSubmission, CachedServer},
    storage::{self, ServerCache},
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
        if !matches!(self.page, Page::Servers)
            || self.selecting_server_id.is_some()
            || self.add_server_dialog.is_some()
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
        self.schedule_cache_save("保存服务器顺序失败", cx);
        cx.notify();
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

    pub(super) fn finish_add_server(
        &mut self,
        dialog: Entity<AddServerDialogState>,
        result: Result<(ServerCache, CachedServer, AddServerSubmission)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((cache, _, _)) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.clear_app_notifications();
                self.add_server_dialog = None;
                self.page = Page::Servers;
            }
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.push_error_notification(format!("添加服务器失败：{error}"), cx);
                });
            }
        }

        cx.notify();
    }

    pub(super) fn finish_edit_server(
        &mut self,
        dialog: Entity<AddServerDialogState>,
        result: Result<(ServerCache, CachedServer)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((cache, _)) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.clear_app_notifications();
                self.add_server_dialog = None;
            }
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.push_error_notification(format!("保存服务器失败：{error}"), cx);
                });
            }
        }

        cx.notify();
    }
}

pub(super) fn fetch_public_info_and_cache(
    client: EmbyClient,
    mut cache: ServerCache,
    submission: AddServerSubmission,
) -> Result<(ServerCache, CachedServer, AddServerSubmission)> {
    let info = client.public_system_info(&submission)?;
    let server = public_info_to_cached_server(
        Uuid::new_v4().to_string(),
        &submission,
        info,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    );
    storage::upsert_server(&mut cache, server.clone());
    storage::save(&cache)?;

    Ok((cache, server, submission))
}

pub(super) fn fetch_public_info_and_update_cache(
    client: EmbyClient,
    mut cache: ServerCache,
    server_id: String,
    submission: AddServerSubmission,
) -> Result<(ServerCache, CachedServer)> {
    let existing = cache
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .cloned()
        .ok_or_else(|| anyhow!("服务器不存在"))?;
    let info = client.public_system_info(&submission)?;
    let server =
        public_info_to_cached_server(existing.id, &submission, info, existing.added_at_unix);
    if !storage::update_server_by_id(&mut cache, server.clone()) {
        return Err(anyhow!("服务器不存在"));
    }
    storage::save(&cache)?;

    Ok((cache, server))
}

fn public_info_to_cached_server(
    id: String,
    submission: &AddServerSubmission,
    info: PublicSystemInfo,
    added_at_unix: u64,
) -> CachedServer {
    CachedServer {
        id,
        endpoint: submission.endpoint.clone(),
        username: submission.username.clone(),
        password: submission.password.clone(),
        user_id: None,
        server_id: info.id,
        server_name: info.server_name,
        access_token: None,
        item_counts: None,
        added_at_unix,
    }
}
