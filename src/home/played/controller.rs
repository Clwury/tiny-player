use super::model::{PlayedCommand, PlayedContent, PlayedRequest, PlayedResponse, PlayedUpdate};
use crate::{
    effects::{RequestScope, RequestSlot, WorkspaceIdentity},
    emby::{MediaItem, MediaItems, UserItemData},
    home::model::{
        LoadState,
        user_data::{UserDataState, apply_media_item_user_data_overrides},
    },
};
use std::collections::HashMap;

/// One played mutation can run in a workspace. begin owns the slot until accept
/// consumes its token. The presentation cancels delivery on release; accepted
/// errors retain the initiating route's notification key. Follow-up reads stay
/// in the same scope and preserve their original ordering and partial failures.
#[derive(Debug)]
pub(crate) struct PlayedController {
    identity: WorkspaceIdentity,
    pending: Option<RequestSlot>,
}

pub(crate) struct PlayedCompletion {
    request: PlayedRequest,
    result: anyhow::Result<PlayedResponse>,
}

impl PlayedController {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            identity,
            pending: None,
        }
    }
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub(crate) fn begin(
        &mut self,
        request: PlayedRequest,
        conflicting_action: bool,
    ) -> Option<PlayedCommand> {
        if conflicting_action || self.has_pending() {
            return None;
        }
        let mut slot = RequestSlot::new(
            RequestScope::PlayedMutation {
                item_id: request.item_id.clone(),
            },
            self.identity.clone(),
        );
        let token = slot.issue();
        self.pending = Some(slot);
        Some(PlayedCommand { request, token })
    }
    pub(crate) fn accept(
        &mut self,
        command: PlayedCommand,
        result: anyhow::Result<PlayedResponse>,
        identity: &WorkspaceIdentity,
    ) -> Option<PlayedCompletion> {
        let slot = self.pending.as_mut()?;
        if !command.token.is_for(identity) || !slot.commit(&command.token) {
            return None;
        }
        self.pending = None;
        Some(PlayedCompletion {
            request: command.request,
            result,
        })
    }
}

impl PlayedCompletion {
    // The effect runner suspends any queued snapshot before the pure reducer
    // changes data. Only a validated successful mutation permits suspension.
    pub(crate) fn succeeded(&self) -> bool {
        self.result.is_ok()
    }

    pub(crate) fn apply(
        self,
        mut content: PlayedContent<'_>,
        data: &mut UserDataState,
    ) -> PlayedUpdate {
        let request = self.request;
        let PlayedResponse {
            data: response,
            parent,
            episodes,
            episode,
        } = match self.result {
            Ok(response) => response,
            Err(error) => {
                return PlayedUpdate {
                    changed: false,
                    notification: request.notification,
                    errors: vec![format!("更新观看状态失败：{error}")],
                };
            }
        };
        let mut errors = Vec::new();
        let mut affected = loaded_user_data(&content, &request);
        affected
            .entry(request.item_id.clone())
            .or_insert_with(|| request.user_data.clone());
        for (id, fallback) in affected {
            let previous = data.effective(&id, fallback.as_ref()).cloned();
            let updated = if id == request.item_id {
                let mut updated = response.clone();
                if let Some(previous) = previous {
                    updated.is_favorite = previous.is_favorite;
                }
                updated
            } else {
                // Fence older responses without inventing Episode progress.
                data.bump(&id);
                continue;
            };
            data.bump(&id);
            data.overrides.insert(id.clone(), updated);
            remove_resume_items(content.resume, &id);
        }
        if request.whole_series {
            data.series_revisions
                .insert(request.item_id.clone(), data.revision);
            apply_series_refresh(&mut content, data, &request, episodes, episode, &mut errors);
        }
        match parent {
            Some(Ok(item)) => {
                if let Some(updated) = item.user_data {
                    data.bump(&item.id);
                    data.overrides.insert(item.id, updated);
                }
            }
            Some(Err(error)) => errors.push(format!("观看状态已更新，刷新整部剧状态失败：{error}")),
            None => {}
        }
        for detail in content.details_mut() {
            if let Some(item) = &mut detail.item
                && let Some(updated) = data.overrides.get(&item.id)
            {
                item.user_data = Some(updated.clone());
            }
            for items in [
                &mut detail.episodes,
                &mut detail.next_up,
                &mut detail.seasons,
            ]
            .into_iter()
            .flatten()
            {
                apply_media_item_user_data_overrides(&mut items.items, &data.overrides);
            }
            if let Some(item) = &mut detail.resume_episode
                && let Some(updated) = data.overrides.get(&item.id)
            {
                item.user_data = Some(updated.clone());
            }
        }
        PlayedUpdate {
            changed: true,
            notification: request.notification,
            errors,
        }
    }
}

fn remove_resume_items(resume: &mut Option<crate::emby::ResumeItems>, id: &str) {
    if let Some(resume) = resume {
        let old_len = resume.items.len();
        resume.items.retain(|item| item.id != id);
        resume.total_record_count = resume
            .total_record_count
            .saturating_sub((old_len - resume.items.len()) as u32);
    }
}

fn apply_series_refresh(
    content: &mut PlayedContent<'_>,
    data: &mut UserDataState,
    request: &PlayedRequest,
    episodes: Option<anyhow::Result<MediaItems>>,
    episode: Option<anyhow::Result<MediaItem>>,
    errors: &mut Vec<String>,
) {
    let mut refresh_errors = Vec::new();
    let episodes = match episodes {
        Some(Ok(items)) => Some(items),
        Some(Err(error)) => {
            refresh_errors.push(format!("分集列表：{error}"));
            None
        }
        None => None,
    };
    let episode = match episode {
        Some(Ok(item)) => Some(item),
        Some(Err(error)) => {
            refresh_errors.push(format!("当前分集详情：{error}"));
            None
        }
        None => None,
    };
    // Items is the last response and takes precedence for the selected Episode.
    for item in episodes
        .iter()
        .flat_map(|items| &items.items)
        .chain(episode.iter())
    {
        data.bump(&item.id);
        if let Some(updated) = &item.user_data {
            data.overrides.insert(item.id.clone(), updated.clone());
        } else {
            data.overrides.remove(&item.id);
        }
        if item
            .user_data
            .as_ref()
            .is_some_and(|data| data.played || data.playback_position_ticks == Some(0))
        {
            remove_resume_items(content.resume, &item.id);
        }
    }
    if request.detail_activation.is_some()
        && content.detail_activation == request.detail_activation
        && let Some(detail) = content.current.as_deref_mut().filter(|detail| {
            detail.series_id == request.item_id && detail.selected_season_id == request.season_id
        })
    {
        if let Some(items) = episodes {
            detail.episodes = Some(items);
            detail.effects.episodes = LoadState::Loaded;
            detail.episodes_failed = None;
            detail.episodes_request_season_id = request.season_id.clone();
        }
        if let Some(item) = episode
            && let Some(current) = detail
                .episodes
                .as_mut()
                .and_then(|items| items.items.iter_mut().find(|entry| entry.id == item.id))
        {
            *current = item;
        }
    }
    if !refresh_errors.is_empty() {
        errors.push(format!(
            "观看状态已更新，刷新失败（{}）",
            refresh_errors.join("；")
        ));
    }
}

fn loaded_user_data(
    content: &PlayedContent<'_>,
    request: &PlayedRequest,
) -> HashMap<String, Option<UserItemData>> {
    let mut items = HashMap::new();
    for detail in content.details() {
        for item in detail
            .item
            .iter()
            .chain(detail.episodes.iter().flat_map(|items| &items.items))
            .chain(detail.next_up.iter().flat_map(|items| &items.items))
            .chain(detail.seasons.iter().flat_map(|items| &items.items))
        {
            if item.id == request.item_id
                || (request.whole_series && detail.series_id == request.item_id)
            {
                items.insert(item.id.clone(), item.user_data.clone());
            }
        }
        if let Some(item) = &detail.resume_episode
            && (item.id == request.item_id
                || (request.whole_series && detail.series_id == request.item_id))
        {
            items.insert(item.id.clone(), item.user_data.clone());
        }
    }
    for item in &content.items {
        if item.id == request.item_id
            || (request.whole_series && item.series_id.as_deref() == Some(&request.item_id))
        {
            items
                .entry(item.id.clone())
                .or_insert_with(|| item.user_data.clone());
        }
    }
    for item in content.resume.iter().flat_map(|items| &items.items) {
        if item.id == request.item_id
            || (request.whole_series && item.series_id.as_deref() == Some(&request.item_id))
        {
            items
                .entry(item.id.clone())
                .or_insert_with(|| item.user_data.clone());
        }
    }
    items
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
