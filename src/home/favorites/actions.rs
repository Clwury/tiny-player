use std::collections::HashMap;

use super::controller::FavoritesController;
use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{UserItem, UserItemData, VideoItemType},
    home::model::{
        notification::ActionNotification,
        user_data::{PendingFavorites, UserDataState},
    },
};

/// Owns pending favorite writes and rollback snapshots for one workspace.
/// Toggle/complete are the only mutation paths. Each active item owns a token;
/// completion removes it, and dropping the workspace drops all pending state.
/// Presentation holds delivery handles. Notification keys are captured from
/// the originating route, so navigation during a write preserves error scope.
#[derive(Debug)]
pub(crate) struct FavoriteActions {
    identity: WorkspaceIdentity,
    pending: HashMap<String, FavoriteOperation>,
}

#[derive(Debug)]
struct FavoriteOperation {
    slot: RequestSlot,
    previous_override: Option<UserItemData>,
    removed: Option<(VideoItemType, usize, UserItem)>,
    notification: ActionNotification,
}

pub(crate) struct ToggleFavorite {
    pub(crate) item_id: String,
    pub(crate) fallback: Option<UserItemData>,
    pub(crate) in_favorites: bool,
    pub(crate) notification: ActionNotification,
}

#[derive(Clone)]
pub(crate) struct FavoriteCommand {
    pub(crate) item_id: String,
    pub(crate) desired: bool,
    pub(crate) token: RequestToken,
}

pub(crate) struct FavoriteUpdate {
    pub(crate) failure: Option<(ActionNotification, String)>,
}

impl FavoriteActions {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            identity,
            pending: HashMap::new(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub(crate) fn is_pending(&self, item_id: &str) -> bool {
        self.pending.contains_key(item_id)
    }

    pub(crate) fn toggle(
        &mut self,
        intent: ToggleFavorite,
        other_action_pending: bool,
        data: &mut UserDataState,
        favorites: &mut FavoritesController,
    ) -> Option<FavoriteCommand> {
        if intent.item_id.trim().is_empty() || other_action_pending || self.has_pending() {
            return None;
        }
        let mut optimistic = data
            .effective(&intent.item_id, intent.fallback.as_ref())
            .cloned()
            .unwrap_or_default();
        let desired = !optimistic.is_favorite;
        optimistic.is_favorite = desired;
        data.bump(&intent.item_id);
        let previous_override = data.overrides.insert(intent.item_id.clone(), optimistic);
        let removed = (intent.in_favorites && !desired)
            .then(|| favorites.remove_item(&intent.item_id))
            .flatten();
        favorites.mark_dirty();
        let mut slot = RequestSlot::new(
            RequestScope::FavoriteMutation {
                item_id: intent.item_id.clone(),
            },
            self.identity.clone(),
        );
        let token = slot.issue();
        self.pending.insert(
            intent.item_id.clone(),
            FavoriteOperation {
                slot,
                previous_override,
                removed,
                notification: intent.notification,
            },
        );
        Some(FavoriteCommand {
            item_id: intent.item_id,
            desired,
            token,
        })
    }

    pub(crate) fn complete(
        &mut self,
        command: &FavoriteCommand,
        result: anyhow::Result<UserItemData>,
        identity: &WorkspaceIdentity,
        data: &mut UserDataState,
        favorites: &mut FavoritesController,
    ) -> Option<FavoriteUpdate> {
        let operation = self.pending.get_mut(&command.item_id)?;
        if !command.token.is_for(identity) || !operation.slot.commit(&command.token) {
            return None;
        }
        let operation = self.pending.remove(&command.item_id)?;
        data.bump(&command.item_id);
        let failure = match result {
            Ok(response) => {
                data.overrides.insert(command.item_id.clone(), response);
                None
            }
            Err(error) => {
                match operation.previous_override {
                    Some(previous) => {
                        data.overrides.insert(command.item_id.clone(), previous);
                    }
                    None => {
                        data.overrides.remove(&command.item_id);
                    }
                }
                if let Some((kind, index, item)) = operation.removed {
                    favorites.restore_item(kind, index, item);
                }
                Some((operation.notification, error.to_string()))
            }
        };
        favorites.mark_dirty();
        Some(FavoriteUpdate { failure })
    }
}

impl PendingFavorites for FavoriteActions {
    fn contains(&self, item_id: &str) -> bool {
        self.is_pending(item_id)
    }
}

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;
