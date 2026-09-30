use super::{
    catalog::{self, ServerCatalog},
    model::*,
};
use crate::{
    effects::{RequestScope, RequestSlot},
    emby::ItemCounts,
    server::{CachedItemCounts, CachedServer},
};
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};

/// Server feature owns catalog, selection, count requests/results and reorder.
/// UI intents and domain completions enter here; a successful save commits a
/// candidate catalog. The shell retains global configuration and GPUI resources.
#[derive(Debug)]
pub(crate) struct ServerController {
    pub(super) catalog: ServerCatalog,
    pub(super) counts: HashMap<String, ItemCounts>,
    pub(super) counts_failed: HashSet<String>,
    pub(super) counts_refreshed: HashSet<String>,
    pub(super) selection: SelectionState,
    pub(super) selected: Option<String>,
    pub(super) reorder: Option<ReorderState>,
    count_requests: HashMap<String, RequestSlot>,
    save_request: Option<RequestSlot>,
    pub(super) icon_picker: Option<super::icons::IconPickerState>,
    pub(super) menu_server_id: Option<String>,
}

/// A synchronous save candidate built only by the controller. Persistence sees
/// an immutable catalog; failed saves drop the candidate without changing state.
/// No asynchronous work may intervene between proposing and persisting it.
pub(crate) struct CatalogChange {
    catalog: ServerCatalog,
}

impl CatalogChange {
    #[cfg(test)]
    pub(super) fn catalog(&self) -> &ServerCatalog {
        &self.catalog
    }
}

impl ServerController {
    pub(crate) fn dispatch(&mut self, intent: ServerIntent) -> ServerCommand {
        let (changed, catalog) = match intent {
            ServerIntent::SelectServer(id) => return self.select(&id),
            ServerIntent::CancelSelection => {
                self.cancel_selection();
                return ServerCommand::Cancelled;
            }
            ServerIntent::ToggleAutoStart(id) => (self.toggle_auto_start(&id), true),
            ServerIntent::ReorderServer {
                server_id,
                target_id,
            } => (self.move_server(&server_id, &target_id), true),
            ServerIntent::BeginReorder(id) => (self.begin_reorder(&id), false),
            ServerIntent::ReorderPreview(index) => (self.preview_reorder(index), false),
            ServerIntent::CommitReorder(commit) => (self.finish_reorder(commit), true),
            ServerIntent::OpenIconPicker(id) => (self.open_icon_picker(&id), false),
            ServerIntent::CloseIconPicker => (self.icon_picker.take().is_some(), false),
            ServerIntent::SearchIcons(query) => (self.search_icons(query), false),
            ServerIntent::OpenMenu(id) => {
                if self.icon_picker.is_some()
                    || self.selecting_server_id() == Some(&id)
                    || !self.catalog.servers.iter().any(|server| server.id == id)
                {
                    return ServerCommand::Ignored;
                }
                self.menu_server_id = Some(id);
                (true, false)
            }
            ServerIntent::CloseMenu => (self.menu_server_id.take().is_some(), false),
            ServerIntent::SelectIcon(index) => {
                return self
                    .begin_icon_download(index)
                    .map_or(ServerCommand::Ignored, |request| {
                        ServerCommand::DownloadIcon(Box::new(request))
                    });
            }
        };
        if !changed {
            ServerCommand::Ignored
        } else if catalog {
            ServerCommand::CatalogChanged
        } else {
            ServerCommand::PreviewChanged
        }
    }
    pub(crate) fn new(catalog: ServerCatalog) -> Self {
        let counts = catalog
            .servers
            .iter()
            .filter_map(|server| {
                server
                    .item_counts
                    .as_ref()
                    .map(|counts| (server.id.clone(), ItemCounts::from(counts)))
            })
            .collect();
        Self {
            catalog,
            counts,
            counts_failed: HashSet::new(),
            counts_refreshed: HashSet::new(),
            selection: SelectionState::Idle,
            selected: None,
            reorder: None,
            count_requests: HashMap::new(),
            save_request: None,
            icon_picker: None,
            menu_server_id: None,
        }
    }

    pub(crate) fn selecting_server_id(&self) -> Option<&str> {
        match &self.selection {
            SelectionState::Authenticating { server_id, .. } => Some(server_id),
            _ => None,
        }
    }

    pub(crate) fn cancel_selection(&mut self) {
        self.selection = SelectionState::Idle;
    }
    pub(crate) fn selection_failed(&mut self) {
        self.selection = SelectionState::Failed;
    }
    pub(crate) fn enter_workspace(&mut self, id: Option<String>) {
        self.cancel_selection();
        self.selected = id;
    }

    pub(crate) fn select(&mut self, id: &str) -> ServerCommand {
        if self.icon_picker.is_some() {
            return ServerCommand::Ignored;
        }
        if self.selected.as_deref() == Some(id) {
            self.cancel_selection();
            return ServerCommand::Cancelled;
        }
        let Some(server) = self
            .catalog
            .servers
            .iter()
            .find(|server| server.id == id)
            .cloned()
        else {
            return ServerCommand::Ignored;
        };
        if self.selecting_server_id() == Some(id) {
            return ServerCommand::Ignored;
        }
        self.cancel_selection();
        if server.can_reuse_auth() {
            return ServerCommand::Open(Box::new(server));
        }
        ServerCommand::Authenticate(Box::new(self.begin_authentication(server)))
    }

    pub(crate) fn begin_authentication(&mut self, server: CachedServer) -> AuthRequest {
        let mut slot = RequestSlot::new(RequestScope::ServerAuth, server.workspace_identity());
        let token = slot.issue();
        self.selection = SelectionState::Authenticating {
            server_id: server.id.clone(),
            slot: Box::new(slot),
        };
        AuthRequest { server, token }
    }

    pub(crate) fn finish_authentication(
        &mut self,
        request: &AuthRequest,
        result: Result<CachedServer>,
    ) -> AuthResult {
        let SelectionState::Authenticating { server_id, slot } = &mut self.selection else {
            return AuthResult::Ignored;
        };
        if server_id != &request.server.id || !request.token.is_current(slot) {
            return AuthResult::Ignored;
        }
        let valid = self.catalog.servers.iter().any(|current| {
            current.id == request.server.id
                && request.token.is_for(&current.workspace_identity())
                && same_credentials(current, &request.server)
                && current.needs_auth_refresh == request.server.needs_auth_refresh
        });
        slot.commit(&request.token);
        self.selection = if result.is_err() && valid {
            SelectionState::Failed
        } else {
            SelectionState::Idle
        };
        if valid {
            AuthResult::Ready(result.map(Box::new))
        } else {
            AuthResult::Invalidated
        }
    }

    pub(crate) fn proposed_save(
        &self,
        server: CachedServer,
        editing: bool,
    ) -> Result<(CatalogChange, String)> {
        let mut catalog = self.catalog.clone();
        let id = if editing {
            let id = server.id.clone();
            catalog::update_server_by_id(&mut catalog, server)?;
            id
        } else {
            catalog::upsert_server(&mut catalog, server)
        };
        Ok((CatalogChange { catalog }, id))
    }

    pub(crate) fn prepare_submission(
        &mut self,
        submission: crate::server::AddServerSubmission,
        editing: Option<&str>,
    ) -> Result<SaveRequest> {
        let existing = editing
            .map(|id| {
                self.catalog
                    .servers
                    .iter()
                    .find(|server| server.id == id)
                    .cloned()
                    .ok_or_else(|| anyhow!("服务器不存在"))
            })
            .transpose()?;
        let identity = existing
            .as_ref()
            .map(CachedServer::workspace_identity)
            .unwrap_or_default();
        let mut slot = RequestSlot::new(RequestScope::ServerSave, identity);
        let token = slot.issue();
        self.save_request = Some(slot);
        Ok(SaveRequest {
            submission,
            existing,
            token,
        })
    }

    pub(crate) fn cancel_submission(&mut self) {
        self.save_request = None;
    }

    pub(crate) fn finish_submission(&mut self, request: &SaveRequest) -> bool {
        self.save_request
            .as_mut()
            .is_some_and(|slot| slot.commit(&request.token))
    }

    pub(crate) fn merge_edited_server(&self, server: CachedServer) -> Result<CachedServer> {
        let current = self
            .catalog
            .servers
            .iter()
            .find(|current| current.id == server.id)
            .ok_or_else(|| anyhow!("服务器不存在"))?;
        let display = self
            .catalog
            .servers
            .iter()
            .find(|candidate| {
                candidate.id != server.id
                    && candidate.endpoint == server.endpoint
                    && candidate.username == server.username
            })
            .unwrap_or(current);
        Ok(CachedServer {
            endpoint: server.endpoint,
            username: server.username,
            password: server.password,
            server_id: display.server_id.clone(),
            server_name: display.server_name.clone(),
            icon_url: display.icon_url.clone(),
            icon_is_custom: display.icon_is_custom,
            needs_auth_refresh: true,
            ..current.clone()
        })
    }

    pub(crate) fn proposed_delete(&self, id: &str) -> Option<CatalogChange> {
        let mut catalog = self.catalog.clone();
        catalog::delete_server_by_id(&mut catalog, id).then_some(CatalogChange { catalog })
    }

    pub(crate) fn persist_catalog(
        &mut self,
        change: CatalogChange,
        persist: impl FnOnce(&ServerCatalog) -> Result<()>,
    ) -> Result<()> {
        persist(&change.catalog)?;
        self.catalog = change.catalog;
        self.retain_counts();
        Ok(())
    }

    pub(crate) fn toggle_auto_start(&mut self, id: &str) -> bool {
        if !self.catalog.servers.iter().any(|server| server.id == id) {
            return false;
        }
        self.catalog.auto_start_server_id =
            (self.catalog.auto_start_server_id.as_deref() != Some(id)).then(|| id.to_owned());
        true
    }

    pub(crate) fn move_server(&mut self, id: &str, target_id: &str) -> bool {
        let Some(from) = self
            .catalog
            .servers
            .iter()
            .position(|server| server.id == id)
        else {
            return false;
        };
        let Some(to) = self
            .catalog
            .servers
            .iter()
            .position(|server| server.id == target_id)
        else {
            return false;
        };
        if from == to {
            return false;
        }
        let server = self.catalog.servers.remove(from);
        self.catalog.servers.insert(to, server);
        true
    }

    pub(crate) fn begin_reorder(&mut self, id: &str) -> bool {
        let Some(target_index) = self
            .catalog
            .servers
            .iter()
            .position(|server| server.id == id)
        else {
            return false;
        };
        self.reorder = Some(ReorderState {
            server_id: id.into(),
            target_index,
        });
        true
    }
    pub(crate) fn preview_reorder(&mut self, index: usize) -> bool {
        if let Some(reorder) = &mut self.reorder
            && index < self.catalog.servers.len()
            && reorder.target_index != index
        {
            reorder.target_index = index;
            return true;
        }
        false
    }
    pub(crate) fn finish_reorder(&mut self, commit: bool) -> bool {
        let Some(reorder) = self.reorder.take() else {
            return false;
        };
        if commit && let Some(target) = self.catalog.servers.get(reorder.target_index) {
            let target_id = target.id.clone();
            return self.move_server(&reorder.server_id, &target_id);
        }
        false
    }

    pub(crate) fn reset_counts(&mut self, id: &str) {
        self.count_requests.remove(id);
        self.counts_failed.remove(id);
        self.counts_refreshed.remove(id);
    }
    pub(crate) fn counts_loading(&self, id: &str) -> bool {
        self.count_requests.contains_key(id)
    }
    pub(crate) fn begin_counts(&mut self, id: &str) -> Option<CountRequest> {
        if self.counts_loading(id)
            || self.counts_failed.contains(id)
            || self.counts_refreshed.contains(id)
        {
            return None;
        }
        let server = self
            .catalog
            .servers
            .iter()
            .find(|server| server.id == id && server.can_reuse_auth())?
            .clone();
        let mut slot = RequestSlot::new(
            RequestScope::ServerCounts {
                server_id: id.into(),
            },
            server.workspace_identity(),
        );
        let token = slot.issue();
        self.count_requests.insert(id.into(), slot);
        Some(CountRequest { server, token })
    }
    pub(crate) fn finish_counts(
        &mut self,
        request: &CountRequest,
        result: Result<ItemCounts>,
    ) -> CountResult {
        let id = &request.server.id;
        let Some(slot) = self.count_requests.get_mut(id) else {
            return CountResult::Ignored;
        };
        if !request.token.is_current(slot)
            || !self.catalog.servers.iter().any(|current| {
                current.id == *id
                    && current.can_reuse_auth()
                    && request.token.is_for(&current.workspace_identity())
                    && same_credentials(current, &request.server)
                    && current.user_id == request.server.user_id
                    && current.access_token == request.server.access_token
            })
        {
            return CountResult::Ignored;
        }
        slot.commit(&request.token);
        self.count_requests.remove(id);
        match result {
            Ok(counts) => {
                self.counts_failed.remove(id);
                self.counts_refreshed.insert(id.clone());
                if let Some(server) = self
                    .catalog
                    .servers
                    .iter_mut()
                    .find(|server| server.id == *id)
                {
                    server.item_counts = Some(CachedItemCounts {
                        movie_count: counts.movie_count,
                        series_count: counts.series_count,
                    });
                }
                self.counts.insert(id.clone(), counts);
                CountResult::Saved
            }
            Err(_) => {
                self.counts_failed.insert(id.clone());
                CountResult::Failed
            }
        }
    }
    pub(crate) fn retain_counts(&mut self) {
        self.counts
            .retain(|id, _| self.catalog.servers.iter().any(|server| server.id == *id));
        let ids = self
            .catalog
            .servers
            .iter()
            .filter(|server| server.can_reuse_auth())
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();
        self.count_requests.retain(|id, _| ids.contains(id));
        self.counts_failed.retain(|id| ids.contains(id));
        self.counts_refreshed.retain(|id| ids.contains(id));
    }

    #[cfg(test)]
    pub(crate) fn authentication_token(&self) -> Option<crate::effects::RequestToken> {
        if let SelectionState::Authenticating { slot, .. } = &self.selection {
            slot.latest()
        } else {
            None
        }
    }
}

fn same_credentials(current: &CachedServer, requested: &CachedServer) -> bool {
    current.endpoint == requested.endpoint
        && current.username == requested.username
        && current.password == requested.password
}
