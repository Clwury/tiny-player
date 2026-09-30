//! Sidebar display snapshot and temporary reorder state. The server feature
//! remains the authoritative catalog and validates/persists emitted ID commands.
use crate::server::feature::SidebarServer;

pub(in crate::home) enum SidebarIntent {
    ServersChanged(Vec<SidebarServer>),
    SelectingChanged(Option<String>),
    Select(String),
    BeginReorder(String),
    PreviewReorder(usize),
    FinishReorder(bool),
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::home) enum SidebarCommand {
    None,
    Changed,
    Select(String),
    Reorder {
        server_id: String,
        target_id: String,
    },
}

#[derive(Clone, Debug)]
struct Reorder {
    server_id: String,
    target_index: usize,
}

// Owned by HomePage. Catalog/selection snapshots and user intents are the only
// writes; finish/cancel clears preview, and workspace release drops all state.
#[derive(Clone, Debug)]
pub(in crate::home) struct SidebarController {
    current_server_id: String,
    username: String,
    servers: Vec<SidebarServer>,
    selecting_server_id: Option<String>,
    reorder: Option<Reorder>,
}

#[derive(Clone, Copy)]
pub(in crate::home) struct SidebarRow<'a> {
    pub(in crate::home) server: &'a SidebarServer,
    pub(in crate::home) active: bool,
    pub(in crate::home) loading: bool,
    pub(in crate::home) placeholder: bool,
    pub(in crate::home) can_reorder: bool,
}

pub(in crate::home) struct SidebarViewModel<'a> {
    pub(in crate::home) username: &'a str,
    pub(in crate::home) rows: Vec<SidebarRow<'a>>,
}

impl SidebarController {
    pub(in crate::home) fn new(
        current_server_id: String,
        username: String,
        servers: Vec<SidebarServer>,
    ) -> Self {
        Self {
            current_server_id,
            username,
            servers,
            selecting_server_id: None,
            reorder: None,
        }
    }

    #[cfg(test)]
    pub(in crate::home) fn current_server_id(&self) -> &str {
        &self.current_server_id
    }

    pub(in crate::home) fn dispatch(&mut self, intent: SidebarIntent) -> SidebarCommand {
        match intent {
            SidebarIntent::ServersChanged(servers) => {
                if self.servers == servers {
                    return SidebarCommand::None;
                }
                self.servers = servers;
            }
            SidebarIntent::SelectingChanged(id) => {
                if self.selecting_server_id == id {
                    return SidebarCommand::None;
                }
                self.selecting_server_id = id;
            }
            SidebarIntent::Select(id) => {
                return if (id != self.current_server_id || self.selecting_server_id.is_some())
                    && self.servers.iter().any(|server| server.id == id)
                {
                    SidebarCommand::Select(id)
                } else {
                    SidebarCommand::None
                };
            }
            SidebarIntent::BeginReorder(id) => {
                let Some(target_index) = self.servers.iter().position(|server| server.id == id)
                else {
                    return SidebarCommand::None;
                };
                self.reorder = Some(Reorder {
                    server_id: id,
                    target_index,
                });
            }
            SidebarIntent::PreviewReorder(index) => {
                let Some(reorder) = &mut self.reorder else {
                    return SidebarCommand::None;
                };
                if index >= self.servers.len() || reorder.target_index == index {
                    return SidebarCommand::None;
                }
                reorder.target_index = index;
            }
            SidebarIntent::FinishReorder(commit) => {
                let Some(reorder) = self.reorder.take() else {
                    return SidebarCommand::None;
                };
                if commit
                    && let Some(target) = self.servers.get(reorder.target_index)
                    && target.id != reorder.server_id
                {
                    return SidebarCommand::Reorder {
                        server_id: reorder.server_id,
                        target_id: target.id.clone(),
                    };
                }
            }
        }
        SidebarCommand::Changed
    }

    pub(in crate::home) fn view_model(&self) -> SidebarViewModel<'_> {
        let mut indices: Vec<_> = (0..self.servers.len()).collect();
        if let Some(reorder) = &self.reorder
            && let Some(source) = self
                .servers
                .iter()
                .position(|server| server.id == reorder.server_id)
        {
            let target = reorder.target_index.min(indices.len() - 1);
            let index = indices.remove(source);
            indices.insert(target, index);
        }
        SidebarViewModel {
            username: &self.username,
            rows: indices
                .into_iter()
                .map(|index| {
                    let server = &self.servers[index];
                    SidebarRow {
                        server,
                        active: server.id == self.current_server_id,
                        loading: self.selecting_server_id.as_deref() == Some(server.id.as_str()),
                        placeholder: self
                            .reorder
                            .as_ref()
                            .is_some_and(|reorder| reorder.server_id == server.id),
                        can_reorder: self.selecting_server_id.is_none(),
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
