use std::collections::HashMap;

use crate::emby::UserItemData;

use super::*;

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub(in crate::home) struct PlayedRequest {
    item_id: String,
    series_id: Option<String>,
    whole_series: bool,
    played: bool,
    season_id: Option<String>,
    episode_id: Option<String>,
    detail_generation: u64,
}

struct PlayedResponse {
    data: UserItemData,
    parent: Option<anyhow::Result<MediaItem>>,
    episodes: Option<anyhow::Result<MediaItems>>,
    episode: Option<anyhow::Result<MediaItem>>,
}

impl HomeContent {
    pub(in crate::home) fn series_user_data_response_is_current(
        &self,
        series_id: Option<&str>,
        request_revision: u64,
    ) -> bool {
        series_id
            .and_then(|id| self.series_user_data_revisions.get(id))
            .is_none_or(|revision| *revision <= request_revision)
    }

    pub(in crate::home) fn detail_user_data_pending(&self) -> bool {
        self.played_request.is_some()
            || !self.favorite_requests.is_empty()
            || !self.resume_item_requests.is_empty()
    }

    pub(in crate::home) fn toggle_detail_actions_menu(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self
            .series_detail
            .as_mut()
            .filter(|detail| detail.is_series())
        else {
            return;
        };
        if detail.open_select == Some(SeriesDetailSelectKind::Actions) {
            detail.open_select = None;
            window.blur(cx);
        } else {
            let focus = detail
                .action_menu_focus
                .get_or_insert_with(|| cx.focus_handle());
            focus.focus(window, cx);
            detail.open_select = Some(SeriesDetailSelectKind::Actions);
        }
        cx.notify();
    }

    pub(in crate::home) fn toggle_series_favorite(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self
            .series_detail
            .as_mut()
            .filter(|detail| detail.is_series())
        else {
            return;
        };
        detail.open_select = None;
        let id = detail.series_id.clone();
        let fallback = detail.item.as_ref().and_then(|item| item.user_data.clone());
        self.toggle_item_favorite(id, fallback, cx);
    }

    pub(in crate::home) fn toggle_detail_played(
        &mut self,
        whole_series: bool,
        cx: &mut Context<Self>,
    ) {
        if self.detail_user_data_pending() {
            return;
        }
        let Some(detail) = self.series_detail.as_ref() else {
            return;
        };
        if whole_series && !detail.is_series() {
            return;
        }
        let item = if whole_series {
            detail.item.as_ref()
        } else {
            detail.selected_playback_item()
        };
        let Some(item) = item else {
            return;
        };
        let request = PlayedRequest {
            item_id: item.id.clone(),
            series_id: detail.is_series().then(|| detail.series_id.clone()),
            whole_series,
            played: !self
                .effective_user_data(&item.id, item.user_data.as_ref())
                .is_some_and(|data| data.played),
            season_id: detail.selected_season_id.clone(),
            episode_id: detail.selected_episode().map(|episode| episode.id.clone()),
            detail_generation: self.detail_generation,
        };
        self.played_request = Some(request.clone());
        if let Some(detail) = self.series_detail.as_mut() {
            detail.open_select = None;
        }
        self.clear_notification(NotificationScope::Detail, "detail:played");
        cx.notify();

        let identity = self.request_identity();
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let task_request = request.clone();
        let task = cx.background_spawn(async move {
            let data = client.set_played(&server, &task_request.item_id, task_request.played)?;
            // An Episode changes the Series aggregate; read that state from the server.
            let parent = if !task_request.whole_series {
                task_request
                    .series_id
                    .as_ref()
                    .map(|id| client.media_item(&server, id))
            } else {
                None
            };
            // Refresh both endpoints after the mutation, even if one refresh fails.
            let episodes = task_request.whole_series.then(|| {
                client.show_episodes(
                    &server,
                    &task_request.item_id,
                    task_request.season_id.as_deref(),
                )
            });
            let episode = if task_request.whole_series {
                task_request
                    .episode_id
                    .as_deref()
                    .map(|id| client.media_item(&server, id))
            } else {
                None
            };
            Ok(PlayedResponse {
                data,
                parent,
                episodes,
                episode,
            })
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_detail_played(identity, request, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_detail_played(
        &mut self,
        identity: WorkspaceIdentity,
        request: PlayedRequest,
        result: anyhow::Result<PlayedResponse>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        self.played_request = None;
        let PlayedResponse {
            data: response,
            parent,
            episodes,
            episode,
        } = match result {
            Ok(response) => response,
            Err(error) => {
                self.push_error_notification(
                    NotificationScope::Detail,
                    "detail:played",
                    format!("更新观看状态失败：{error}"),
                    cx,
                );
                cx.notify();
                return;
            }
        };
        self.invalidate_pending_home_snapshot_save();
        let mut affected = self.loaded_detail_user_data(&request);
        affected.entry(request.item_id.clone()).or_default();
        for (id, fallback) in affected {
            let previous = self.effective_user_data(&id, fallback.as_ref()).cloned();
            let data = if id == request.item_id {
                let mut data = response.clone();
                if let Some(previous) = previous {
                    data.is_favorite = previous.is_favorite;
                }
                data
            } else {
                // Fence off older responses without inventing Episode progress.
                self.bump_user_data_revision(&id);
                continue;
            };
            self.bump_user_data_revision(&id);
            self.user_data_overrides.insert(id.clone(), data);
            if let Some(resume) = &mut self.resume_items {
                let old_len = resume.items.len();
                resume.items.retain(|item| item.id != id);
                resume.total_record_count = resume
                    .total_record_count
                    .saturating_sub((old_len - resume.items.len()) as u32);
            }
        }
        if request.whole_series {
            self.series_user_data_revisions
                .insert(request.item_id.clone(), self.user_data_revision);
            self.apply_series_played_refresh(&request, episodes, episode, cx);
        }
        match parent {
            Some(Ok(item)) => {
                if let Some(data) = item.user_data {
                    self.bump_user_data_revision(&item.id);
                    self.user_data_overrides.insert(item.id, data);
                }
            }
            Some(Err(error)) => {
                self.push_error_notification(
                    NotificationScope::Detail,
                    "detail:played",
                    format!("观看状态已更新，刷新整部剧状态失败：{error}"),
                    cx,
                );
            }
            None => {}
        }
        for detail in self
            .series_detail
            .iter_mut()
            .chain(self.detail_history.iter_mut())
        {
            if let Some(item) = &mut detail.item
                && let Some(data) = self.user_data_overrides.get(&item.id)
            {
                item.user_data = Some(data.clone());
            }
            for items in [
                &mut detail.episodes,
                &mut detail.next_up,
                &mut detail.seasons,
            ]
            .into_iter()
            .flatten()
            {
                apply_media_item_user_data_overrides(&mut items.items, &self.user_data_overrides);
            }
            if let Some(item) = &mut detail.resume_episode
                && let Some(data) = self.user_data_overrides.get(&item.id)
            {
                item.user_data = Some(data.clone());
            }
        }
        self.favorites.mark_dirty();
        self.schedule_home_snapshot_save(cx);
        cx.notify();
    }

    fn apply_series_played_refresh(
        &mut self,
        request: &PlayedRequest,
        episodes: Option<anyhow::Result<MediaItems>>,
        episode: Option<anyhow::Result<MediaItem>>,
        cx: &mut Context<Self>,
    ) {
        let mut errors = Vec::new();
        let episodes = match episodes {
            Some(Ok(items)) => Some(items),
            Some(Err(error)) => {
                errors.push(format!("分集列表：{error}"));
                None
            }
            None => None,
        };
        let episode = match episode {
            Some(Ok(item)) => Some(item),
            Some(Err(error)) => {
                errors.push(format!("当前分集详情：{error}"));
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
            self.bump_user_data_revision(&item.id);
            if let Some(data) = &item.user_data {
                self.user_data_overrides
                    .insert(item.id.clone(), data.clone());
            } else {
                self.user_data_overrides.remove(&item.id);
            }
            if let Some(resume) = &mut self.resume_items {
                let old_len = resume.items.len();
                if item
                    .user_data
                    .as_ref()
                    .is_some_and(|data| data.played || data.playback_position_ticks == Some(0))
                {
                    resume.items.retain(|entry| entry.id != item.id);
                }
                resume.total_record_count = resume
                    .total_record_count
                    .saturating_sub((old_len - resume.items.len()) as u32);
            }
        }
        if self.detail_generation == request.detail_generation
            && let Some(detail) = self.series_detail.as_mut().filter(|detail| {
                detail.series_id == request.item_id
                    && detail.selected_season_id == request.season_id
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
        if !errors.is_empty() {
            self.push_error_notification(
                NotificationScope::Detail,
                "detail:played",
                format!("观看状态已更新，刷新失败（{}）", errors.join("；")),
                cx,
            );
        }
    }

    fn loaded_detail_user_data(
        &self,
        request: &PlayedRequest,
    ) -> HashMap<String, Option<UserItemData>> {
        let mut items = HashMap::new();
        for detail in self.series_detail.iter().chain(self.detail_history.iter()) {
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
        for item in self
            .favorites
            .items()
            .chain(self.search.items.iter())
            .chain(
                self.libraries
                    .values()
                    .flat_map(|library| &library.paged.items),
            )
            .chain(
                self.user_view_items_rows
                    .values()
                    .flat_map(|row| row.items.iter().flat_map(|items| &items.items)),
            )
        {
            if item.id == request.item_id
                || (request.whole_series && item.series_id.as_deref() == Some(&request.item_id))
            {
                items
                    .entry(item.id.clone())
                    .or_insert_with(|| item.user_data.clone());
            }
        }
        for item in self.resume_items.iter().flat_map(|items| &items.items) {
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
}
