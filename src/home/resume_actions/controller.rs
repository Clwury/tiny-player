use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{ResumeItems, UserItemData},
    home::model::user_data::UserDataState,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeItemAction {
    MarkPlayed,
    HideFromResume,
}

impl ResumeItemAction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MarkPlayed => "标记为已观看",
            Self::HideFromResume => "从继续观看中移除",
        }
    }
}

pub(crate) enum ResumeItemActionResponse {
    MarkedPlayed(UserItemData),
    HiddenFromResume,
}

/// Workspace-owned pending resume mutations. Each item has an independent
/// request scope, allowing different cards to run concurrently. Results commit
/// once; workspace release drops slots and the page cancels their handles.
/// Errors use the existing home:resume-action notification key.
#[derive(Debug)]
pub(crate) struct ResumeActions {
    identity: WorkspaceIdentity,
    pending: HashMap<String, RequestSlot>,
}

#[derive(Clone)]
pub(crate) struct ResumeCommand {
    pub(crate) item_id: String,
    pub(crate) action: ResumeItemAction,
    pub(crate) token: RequestToken,
}

pub(crate) enum ResumeUpdate {
    Changed,
    Failed(String),
}

impl ResumeActions {
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

    pub(crate) fn begin(
        &mut self,
        item_id: String,
        action: ResumeItemAction,
        items: &Option<ResumeItems>,
        conflicting_action: bool,
    ) -> Option<ResumeCommand> {
        if item_id.trim().is_empty()
            || conflicting_action
            || self.is_pending(&item_id)
            || items
                .as_ref()
                .is_none_or(|items| !items.items.iter().any(|item| item.id == item_id))
        {
            return None;
        }
        let mut slot = RequestSlot::new(
            RequestScope::ResumeMutation {
                item_id: item_id.clone(),
            },
            self.identity.clone(),
        );
        let token = slot.issue();
        self.pending.insert(item_id.clone(), slot);
        Some(ResumeCommand {
            item_id,
            action,
            token,
        })
    }

    pub(crate) fn complete(
        &mut self,
        command: &ResumeCommand,
        result: anyhow::Result<ResumeItemActionResponse>,
        identity: &WorkspaceIdentity,
        items: &mut Option<ResumeItems>,
        user_data: &mut UserDataState,
    ) -> Option<ResumeUpdate> {
        let slot = self.pending.get_mut(&command.item_id)?;
        if !command.token.is_for(identity) || !slot.commit(&command.token) {
            return None;
        }
        self.pending.remove(&command.item_id);
        match result {
            Ok(response) => {
                if let ResumeItemActionResponse::MarkedPlayed(data) = response {
                    let fallback = items
                        .as_ref()
                        .and_then(|items| {
                            items.items.iter().find(|item| item.id == command.item_id)
                        })
                        .and_then(|item| item.user_data.as_ref());
                    let previous = user_data.effective(&command.item_id, fallback).cloned();
                    let data = user_data_after_mark_played(previous, data);
                    user_data.bump(&command.item_id);
                    user_data.overrides.insert(command.item_id.clone(), data);
                }
                remove_resume_item(items, &command.item_id);
                Some(ResumeUpdate::Changed)
            }
            Err(error) => Some(ResumeUpdate::Failed(format!(
                "{}失败：{error}",
                command.action.label()
            ))),
        }
    }
}

fn remove_resume_item(items: &mut Option<ResumeItems>, item_id: &str) -> bool {
    let Some(items) = items.as_mut() else {
        return false;
    };
    let Some(index) = items.items.iter().position(|item| item.id == item_id) else {
        return false;
    };
    items.items.remove(index);
    items.total_record_count = items.total_record_count.saturating_sub(1);
    true
}

fn user_data_after_mark_played(
    previous: Option<UserItemData>,
    mut response: UserItemData,
) -> UserItemData {
    if let Some(previous) = previous {
        response.unplayed_item_count = response
            .unplayed_item_count
            .or(previous.unplayed_item_count);
        response.is_favorite |= previous.is_favorite;
    }
    response.playback_position_ticks = Some(0);
    response.played_percentage = Some(100.0);
    response.played = true;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::{
        gateway::test_support::{Call, FakeMutations, Reply},
        resume_actions::effect::run_resume_action,
    };

    #[test]
    fn independent_resume_actions_complete_out_of_order_and_keep_latest_favorite_data() {
        let identity = WorkspaceIdentity::default();
        let mut controller = ResumeActions::new(identity.clone());
        let mut items = resume_items();
        let mut data = UserDataState::default();
        assert!(
            controller
                .begin(
                    "missing".into(),
                    ResumeItemAction::MarkPlayed,
                    &items,
                    false
                )
                .is_none()
        );
        assert!(
            controller
                .begin(
                    "episode-1".into(),
                    ResumeItemAction::MarkPlayed,
                    &items,
                    true
                )
                .is_none()
        );
        let movie = controller
            .begin(
                "movie-1".into(),
                ResumeItemAction::HideFromResume,
                &items,
                false,
            )
            .unwrap();
        assert!(
            controller
                .begin(
                    "movie-1".into(),
                    ResumeItemAction::MarkPlayed,
                    &items,
                    false
                )
                .is_none()
        );
        let episode = controller
            .begin(
                "episode-1".into(),
                ResumeItemAction::MarkPlayed,
                &items,
                false,
            )
            .unwrap();
        data.overrides.insert(
            "episode-1".into(),
            UserItemData {
                is_favorite: true,
                unplayed_item_count: Some(3),
                ..Default::default()
            },
        );
        let gateway = FakeMutations::new(vec![
            Reply::UserData(Ok(UserItemData::default())),
            Reply::Hidden(Ok(())),
        ]);
        assert!(matches!(
            controller.complete(
                &episode,
                run_resume_action(&gateway, &episode),
                &identity,
                &mut items,
                &mut data
            ),
            Some(ResumeUpdate::Changed)
        ));
        assert!(!controller.is_pending("episode-1") && controller.is_pending("movie-1"));
        assert!(data.overrides["episode-1"].is_favorite && data.overrides["episode-1"].played);
        assert_eq!(data.overrides["episode-1"].unplayed_item_count, Some(3));
        assert_eq!(data.overrides["episode-1"].playback_position_ticks, Some(0));
        assert_eq!(data.overrides["episode-1"].played_percentage, Some(100.0));
        assert!(matches!(
            controller.complete(
                &movie,
                run_resume_action(&gateway, &movie),
                &identity,
                &mut items,
                &mut data
            ),
            Some(ResumeUpdate::Changed)
        ));
        assert_eq!(items.as_ref().unwrap().total_record_count, 2);
        assert!(items.as_ref().unwrap().items.is_empty());
        assert_eq!(data.revision, 1);
        assert!(!controller.has_pending());
        assert_eq!(
            *gateway.calls.lock().unwrap(),
            [
                Call::MarkPlayed("episode-1".into()),
                Call::Hide("movie-1".into())
            ]
        );
    }

    #[test]
    fn foreign_and_old_resume_results_cannot_finish_a_retry_or_remove_its_card() {
        let identity = WorkspaceIdentity::default();
        let foreign = WorkspaceIdentity {
            user_id: Some("other".into()),
            ..Default::default()
        };
        let mut controller = ResumeActions::new(identity.clone());
        let mut items = resume_items();
        let mut data = UserDataState::default();
        let first = controller
            .begin(
                "movie-1".into(),
                ResumeItemAction::HideFromResume,
                &items,
                false,
            )
            .unwrap();
        assert!(
            controller
                .complete(
                    &first,
                    Ok(ResumeItemActionResponse::HiddenFromResume),
                    &foreign,
                    &mut items,
                    &mut data
                )
                .is_none()
        );
        assert!(controller.is_pending("movie-1"));
        assert!(
            matches!(controller.complete(&first, Err(anyhow::anyhow!("offline")), &identity, &mut items, &mut data), Some(ResumeUpdate::Failed(message)) if message == "从继续观看中移除失败：offline")
        );
        assert_eq!(items.as_ref().unwrap().total_record_count, 4);
        let retry = controller
            .begin(
                "movie-1".into(),
                ResumeItemAction::HideFromResume,
                &items,
                false,
            )
            .unwrap();
        for result in [
            Ok(ResumeItemActionResponse::HiddenFromResume),
            Err(anyhow::anyhow!("old")),
        ] {
            assert!(
                controller
                    .complete(&first, result, &identity, &mut items, &mut data)
                    .is_none()
            );
        }
        assert!(controller.is_pending("movie-1"));
        assert_eq!(items.as_ref().unwrap().items.len(), 2);
        assert!(matches!(
            controller.complete(
                &retry,
                Ok(ResumeItemActionResponse::HiddenFromResume),
                &identity,
                &mut items,
                &mut data
            ),
            Some(ResumeUpdate::Changed)
        ));
        assert_eq!(items.as_ref().unwrap().total_record_count, 3);
        assert!(data.overrides.is_empty() && data.revision == 0);
        assert!(
            controller
                .complete(
                    &retry,
                    Ok(ResumeItemActionResponse::HiddenFromResume),
                    &identity,
                    &mut items,
                    &mut data
                )
                .is_none()
        );
    }

    fn resume_items() -> Option<ResumeItems> {
        Some(
            serde_json::from_value(serde_json::json!({
                "Items": [
                    { "Id": "episode-1", "Name": "第一集", "Type": "Episode" },
                    { "Id": "movie-1", "Name": "电影", "Type": "Movie" }
                ],
                "TotalRecordCount": 4
            }))
            .unwrap(),
        )
    }

    #[test]
    fn resume_context_menu_uses_requested_action_labels() {
        assert_eq!(ResumeItemAction::MarkPlayed.label(), "标记为已观看");
        assert_eq!(ResumeItemAction::HideFromResume.label(), "从继续观看中移除");
    }

    #[test]
    fn mark_played_preserves_favorite_when_response_omits_user_fields() {
        let data = user_data_after_mark_played(
            Some(UserItemData {
                unplayed_item_count: Some(3),
                played_percentage: Some(45.0),
                playback_position_ticks: Some(450),
                is_favorite: true,
                played: false,
            }),
            UserItemData::default(),
        );

        assert_eq!(data.unplayed_item_count, Some(3));
        assert_eq!(data.played_percentage, Some(100.0));
        assert_eq!(data.playback_position_ticks, Some(0));
        assert!(data.is_favorite);
    }

    #[test]
    fn successful_resume_action_removes_item_and_decrements_server_total() {
        let mut items = resume_items();

        assert!(remove_resume_item(&mut items, "episode-1"));
        let items = items.unwrap();
        assert_eq!(items.total_record_count, 3);
        assert_eq!(items.items.len(), 1);
        assert_eq!(items.items[0].id, "movie-1");
    }

    #[test]
    fn missing_resume_item_does_not_change_server_total() {
        let mut items = resume_items();

        assert!(!remove_resume_item(&mut items, "missing"));
        let items = items.unwrap();
        assert_eq!(items.total_record_count, 4);
        assert_eq!(items.items.len(), 2);
    }
}
