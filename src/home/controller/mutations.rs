//! Cross-content writes belong to the workspace owner. Views supply intent and
//! notification context; pending conflicts, target selection, optimistic writes
//! and accepted-result merges are decided together without GPUI or IO.
use super::HomeController;
use crate::{
    emby::UserItemData,
    home::{
        favorites::actions::{FavoriteCommand, FavoriteUpdate, ToggleFavorite},
        model::{
            navigation::HomeRoot,
            notification::{ActionNotification, NotificationScope},
        },
        played::{PlayedCommand, PlayedCompletion, PlayedRequest, PlayedResponse},
        resume_actions::controller::{
            ResumeCommand, ResumeItemAction, ResumeItemActionResponse, ResumeUpdate,
        },
    },
};

pub(in crate::home) struct FavoriteIntent {
    pub(in crate::home) item_id: String,
    pub(in crate::home) fallback: Option<UserItemData>,
    pub(in crate::home) notification: ActionNotification,
}
pub(in crate::home) enum PlayedIntent {
    ToggleDetail {
        whole_series: bool,
    },
    MarkItem {
        item_id: String,
        notification: ActionNotification,
    },
}
impl HomeController {
    pub(super) fn absorb_user_items_user_data(
        &mut self,
        items: &crate::emby::UserItems,
        request_revision: u64,
    ) {
        self.user_data.absorb_items(
            items,
            request_revision,
            crate::home::model::user_data::PendingUserData {
                played: self.played_actions.has_pending(),
                favorites: &self.favorite_actions,
            },
        );
    }

    pub(in crate::home) fn user_data_pending(&self) -> bool {
        self.played_actions.has_pending()
            || self.favorite_actions.has_pending()
            || self.resume_actions.has_pending()
    }
    pub(in crate::home) fn favorite_pending(&self) -> bool {
        self.favorite_actions.has_pending()
    }
    pub(in crate::home) fn resume_item_pending(&self, item_id: &str) -> bool {
        self.resume_actions.is_pending(item_id) || self.favorite_actions.is_pending(item_id)
    }
    pub(in crate::home) fn dispatch_favorite(
        &mut self,
        intent: FavoriteIntent,
    ) -> Option<FavoriteCommand> {
        self.favorite_actions.toggle(
            ToggleFavorite {
                item_id: intent.item_id,
                fallback: intent.fallback,
                in_favorites: self.navigation.root() == HomeRoot::Favorites,
                notification: intent.notification,
            },
            self.played_actions.has_pending() || self.resume_actions.has_pending(),
            &mut self.user_data,
            &mut self.favorites,
        )
    }
    pub(in crate::home) fn complete_favorite(
        &mut self,
        command: &FavoriteCommand,
        result: anyhow::Result<UserItemData>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<FavoriteUpdate> {
        self.favorite_actions.complete(
            command,
            result,
            identity,
            &mut self.user_data,
            &mut self.favorites,
        )
    }
    pub(in crate::home) fn dispatch_played(
        &mut self,
        intent: PlayedIntent,
    ) -> Option<PlayedCommand> {
        if self.user_data_pending() {
            return None;
        }
        let detail_activation = self
            .navigation
            .detail()
            .and_then(|detail| detail.activation().cloned());
        let request = match intent {
            PlayedIntent::ToggleDetail { whole_series } => {
                let detail = self.navigation.detail()?.view_model();
                if whole_series && !detail.is_series() {
                    return None;
                }
                let item = if whole_series {
                    detail.item.as_ref()
                } else {
                    detail.selected_playback_item()
                }?;
                let user_data = self.effective_user_data(&item.id, item.user_data.as_ref());
                PlayedRequest {
                    item_id: item.id.clone(),
                    series_id: detail.is_series().then(|| detail.series_id.clone()),
                    whole_series,
                    played: !user_data.is_some_and(|data| data.played),
                    season_id: detail.selected_season_id.clone(),
                    episode_id: detail.selected_episode().map(|episode| episode.id.clone()),
                    detail_activation,
                    user_data: user_data.cloned(),
                    notification: ActionNotification {
                        scope: NotificationScope::Detail,
                        key: "detail:played".into(),
                    },
                }
            }
            PlayedIntent::MarkItem {
                item_id,
                notification,
            } => {
                let item = self.user_item_by_id(&item_id)?;
                let whole_series = item.item_type.as_deref() == Some("Series");
                PlayedRequest {
                    item_id: item.id,
                    series_id: if whole_series {
                        Some(item_id)
                    } else {
                        item.series_id
                    },
                    whole_series,
                    played: true,
                    season_id: None,
                    episode_id: None,
                    detail_activation,
                    user_data: item.user_data,
                    notification,
                }
            }
        };
        self.played_actions.begin(request, false)
    }
    pub(in crate::home) fn accept_played(
        &mut self,
        command: PlayedCommand,
        result: anyhow::Result<PlayedResponse>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<PlayedCompletion> {
        self.played_actions.accept(command, result, identity)
    }
    pub(in crate::home) fn dispatch_resume_action(
        &mut self,
        item_id: String,
        action: ResumeItemAction,
    ) -> Option<ResumeCommand> {
        let conflict =
            self.played_actions.has_pending() || self.favorite_actions.is_pending(&item_id);
        self.resume_actions
            .begin(item_id, action, &self.feed.state.resume_items, conflict)
    }
    pub(in crate::home) fn complete_resume_action(
        &mut self,
        command: &ResumeCommand,
        result: anyhow::Result<ResumeItemActionResponse>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<ResumeUpdate> {
        self.resume_actions.complete(
            command,
            result,
            identity,
            &mut self.feed.state.resume_items,
            &mut self.user_data,
        )
    }
}
