use crate::server::feature::{ServerCommand, ServerIntent};
use anyhow::Result;
use gpui::{Context, Entity, Window};

use crate::{server::CachedServer, server::view::form::AddServerDialogState};

use super::{Page, TinyApp};

impl TinyApp {
    pub(super) fn toggle_server_auto_start(
        &mut self,
        server: &CachedServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_server_menu(window, cx);
        if !matches!(self.shell.page(), Page::Servers)
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        if !matches!(
            self.server_feature
                .dispatch(ServerIntent::ToggleAutoStart(server.id.clone())),
            ServerCommand::CatalogChanged
        ) {
            return;
        }
        self.schedule_cache_save("保存自动启动设置失败", cx);
        cx.notify();
    }

    pub(super) fn reorder_server(
        &mut self,
        server_id: &str,
        target_id: &str,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.shell.page(), Page::Servers | Page::Home(_))
            || self.server_feature.selecting_server_id().is_some()
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        if !matches!(
            self.server_feature.dispatch(ServerIntent::ReorderServer {
                server_id: server_id.into(),
                target_id: target_id.into()
            }),
            ServerCommand::CatalogChanged
        ) {
            return;
        }
        self.sync_home_servers(cx);
        self.schedule_cache_save("保存服务器顺序失败", cx);
        cx.notify();
    }

    pub(super) fn sync_home_servers(&self, cx: &mut Context<Self>) {
        let home = match self.shell.page() {
            Page::Home(home)
            | Page::Playback {
                return_to: home, ..
            } => home,
            Page::Servers => return,
        };
        home.update(cx, |home, cx| {
            home.set_servers(self.server_feature.sidebar_servers(), cx)
        });
    }

    pub(super) fn delete_server(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_server_menu();
        let Some(change) = self.server_feature.proposed_delete(&server.id) else {
            cx.notify();
            return;
        };
        let adapter = self.persistence_adapter();
        let config = &self.cache;
        let persistence = &self.persistence;
        match self.server_feature.persist_catalog(change, |catalog| {
            persistence.save_settings_now(config.snapshot(catalog), adapter)
        }) {
            Ok(()) => {
                self.retain_item_count_state();
                self.clear_app_notifications();
            }
            Err(error) => self.push_app_error_notification(format!("删除服务器失败：{error}"), cx),
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
                self.server_feature.merge_edited_server(server)?
            } else {
                server
            };
            self.save_server(server, editing)
        });
        match result {
            Ok(_) => {
                self.retain_item_count_state();
                self.clear_app_notifications();
                self.clear_server_notifications();
                self.add_server_dialog = None;
                self.cancel_server_selection(cx);
                if editing {
                    self.server_feature.enter_workspace(None);
                    self.shell.show_servers();
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
        let (change, server_id) = self.server_feature.proposed_save(server, editing)?;
        let adapter = self.persistence_adapter();
        let config = &self.cache;
        let persistence = &self.persistence;
        self.server_feature.persist_catalog(change, |catalog| {
            persistence.save_settings_now(config.snapshot(catalog), adapter)
        })?;
        Ok(server_id)
    }
}

#[cfg(test)]
use crate::server::{
    AddServerSubmission,
    feature::effect::{authenticate_server, prepare_server},
};
#[cfg(test)]
use crate::{emby::EmbyClient, storage};
#[cfg(test)]
mod tests;
