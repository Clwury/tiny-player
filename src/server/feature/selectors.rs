use super::ServerController;
use crate::emby::ItemCounts;

use crate::server::SidebarServer;

/// Owned, bounded display values suitable for GPUI callbacks. No credentials or
/// endpoint records are copied into cards, drag previews or context menus.
#[derive(Clone)]
pub(crate) struct ServerCardVm {
    pub(crate) server_id: String,
    pub(crate) title: String,
    pub(crate) icon_url: Option<String>,
    pub(crate) counts: Option<ItemCounts>,
    pub(crate) loading: bool,
    pub(crate) auto_start: bool,
    pub(crate) placeholder: bool,
}

pub(crate) struct ServerMenuVm {
    pub(crate) server_id: String,
    pub(crate) auto_start: bool,
}

impl ServerController {
    /// Read-only input for persistence snapshot composition, never a UI mutation path.
    pub(crate) fn catalog(&self) -> &super::ServerCatalog {
        &self.catalog
    }

    pub(crate) fn server(&self, id: &str) -> Option<&crate::server::CachedServer> {
        self.catalog.servers.iter().find(|server| server.id == id)
    }

    pub(crate) fn edit_form(&self, id: &str) -> Option<super::form::ServerFormProps> {
        self.server(id).map(super::form::ServerFormProps::from)
    }

    pub(crate) fn auto_start_server(&self) -> Option<&crate::server::CachedServer> {
        self.server(self.catalog.auto_start_server_id.as_deref()?)
    }

    pub(crate) fn sidebar_servers(&self) -> Vec<SidebarServer> {
        self.catalog
            .servers
            .iter()
            .map(SidebarServer::from)
            .collect()
    }
    pub(crate) fn menu(&self) -> Option<ServerMenuVm> {
        let id = self.menu_server_id.as_ref()?;
        self.catalog
            .servers
            .iter()
            .any(|server| &server.id == id)
            .then(|| ServerMenuVm {
                server_id: id.clone(),
                auto_start: self.catalog.auto_start_server_id.as_ref() == Some(id),
            })
    }

    #[cfg(test)]
    pub(crate) fn card(&self, id: &str) -> Option<ServerCardVm> {
        let server = self.catalog.servers.iter().find(|server| server.id == id)?;
        Some(self.card_for(server))
    }

    fn card_for(&self, server: &crate::server::CachedServer) -> ServerCardVm {
        let id = server.id.as_str();
        ServerCardVm {
            server_id: server.id.clone(),
            title: server
                .server_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&server.endpoint.address)
                .into(),
            icon_url: server.icon_url.clone(),
            counts: self.counts.get(id).cloned(),
            loading: self.selecting_server_id() == Some(id),
            auto_start: self.catalog.auto_start_server_id.as_deref() == Some(id),
            placeholder: self
                .reorder
                .as_ref()
                .is_some_and(|reorder| reorder.server_id == id),
        }
    }

    pub(crate) fn cards(&self) -> Vec<ServerCardVm> {
        self.preview_indices()
            .into_iter()
            .map(|index| self.card_for(&self.catalog.servers[index]))
            .collect()
    }

    fn preview_indices(&self) -> Vec<usize> {
        let mut indices: Vec<_> = (0..self.catalog.servers.len()).collect();
        if let Some(reorder) = &self.reorder
            && let Some(from) = self
                .catalog
                .servers
                .iter()
                .position(|server| server.id == reorder.server_id)
        {
            let target = reorder.target_index.min(indices.len() - 1);
            let index = indices.remove(from);
            indices.insert(target, index);
        }
        indices
    }

    #[cfg(test)]
    pub(crate) fn preview_servers(&self) -> Vec<crate::server::CachedServer> {
        self.preview_indices()
            .into_iter()
            .map(|index| self.catalog.servers[index].clone())
            .collect()
    }
}
