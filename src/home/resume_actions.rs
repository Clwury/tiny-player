use gpui::{AppContext as _, Context, MouseDownEvent, Pixels, Point, Window};

use crate::emby::{ResumeItems, UserItemData};

use super::{HomeContent, WorkspaceIdentity};

#[derive(Clone, Debug)]
pub(super) struct ResumeItemContextMenu {
    pub(super) item_id: String,
    pub(super) position: Point<Pixels>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResumeItemAction {
    MarkPlayed,
    HideFromResume,
}

impl ResumeItemAction {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::MarkPlayed => "标记为已观看",
            Self::HideFromResume => "从继续观看中移除",
        }
    }
}

enum ResumeItemActionResponse {
    MarkedPlayed(UserItemData),
    HiddenFromResume,
}

impl HomeContent {
    pub(super) fn open_resume_item_context_menu(
        &mut self,
        item_id: String,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if item_id.trim().is_empty()
            || self
                .resume_items
                .as_ref()
                .is_none_or(|items| !items.items.iter().any(|item| item.id == item_id))
        {
            return;
        }
        self.resume_item_context_menu = Some(ResumeItemContextMenu { item_id, position });
        cx.notify();
    }

    pub(super) fn close_resume_item_context_menu(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.resume_item_context_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn start_resume_item_action(
        &mut self,
        item_id: String,
        action: ResumeItemAction,
        cx: &mut Context<Self>,
    ) {
        if item_id.trim().is_empty()
            || self.resume_item_requests.contains(&item_id)
            || self
                .resume_items
                .as_ref()
                .is_none_or(|items| !items.items.iter().any(|item| item.id == item_id))
        {
            return;
        }

        self.resume_item_context_menu = None;
        self.resume_action_failed = None;
        self.resume_item_requests.insert(item_id.clone());
        cx.notify();

        let identity = self.request_identity();
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let task_item_id = item_id.clone();
        let task = cx.background_spawn(async move {
            match action {
                ResumeItemAction::MarkPlayed => client
                    .mark_item_played(&server, &task_item_id)
                    .map(ResumeItemActionResponse::MarkedPlayed),
                ResumeItemAction::HideFromResume => client
                    .hide_item_from_resume(&server, &task_item_id)
                    .map(|_| ResumeItemActionResponse::HiddenFromResume),
            }
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_resume_item_action(identity, item_id, action, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_resume_item_action(
        &mut self,
        identity: WorkspaceIdentity,
        item_id: String,
        action: ResumeItemAction,
        result: anyhow::Result<ResumeItemActionResponse>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        self.resume_item_requests.remove(&item_id);
        match result {
            Ok(response) => {
                if let ResumeItemActionResponse::MarkedPlayed(data) = response {
                    let fallback = self
                        .resume_items
                        .as_ref()
                        .and_then(|items| items.items.iter().find(|item| item.id == item_id))
                        .and_then(|item| item.user_data.as_ref());
                    let previous = self.effective_user_data(&item_id, fallback).cloned();
                    let data = user_data_after_mark_played(previous, data);
                    self.bump_user_data_revision(&item_id);
                    self.user_data_overrides.insert(item_id.clone(), data);
                }
                remove_resume_item(&mut self.resume_items, &item_id);
                self.resume_action_failed = None;
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                self.resume_action_failed = Some(format!("{}失败：{error}", action.label()).into());
            }
        }
        cx.notify();
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
    response
}

#[cfg(test)]
mod tests {
    use super::*;

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
