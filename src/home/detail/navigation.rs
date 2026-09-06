use super::*;

use crate::home::navigation::{HomeRoot, HomeRoute};

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;

impl HomeContent {
    pub(in super::super) fn open_media_detail_by_id(
        &mut self,
        item_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.user_item_by_id(&item_id) else {
            return;
        };
        self.open_media_detail(&item, cx);
    }

    pub(in super::super) fn open_media_detail(&mut self, item: &UserItem, cx: &mut Context<Self>) {
        let Some(detail) = SeriesDetailState::from_user_item(item) else {
            return;
        };

        self.open_detail_state(detail, cx);
    }

    pub(in super::super) fn open_resume_item_detail(
        &mut self,
        item: &ResumeItem,
        cx: &mut Context<Self>,
    ) {
        match item.item_type.as_deref() {
            Some("Movie") => {
                let Some(detail) = SeriesDetailState::from_resume_movie(item) else {
                    return;
                };
                self.open_detail_state(detail, cx);
            }
            Some("Episode") => {
                let Some(detail) = SeriesDetailState::from_resume_episode(item) else {
                    if let Some(scope) = self.current_notification_scope() {
                        self.push_error_notification(
                            scope,
                            HOME_RESUME_DETAIL_NOTIFICATION_KEY,
                            "继续观看剧集缺少 SeriesId，无法打开详情",
                            cx,
                        );
                    }
                    cx.notify();
                    return;
                };
                self.open_detail_state(detail, cx);
            }
            _ => {}
        }
    }

    pub(in super::super) fn open_resume_item_detail_by_id(
        &mut self,
        item_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.resume_item_by_id(&item_id) else {
            return;
        };
        self.open_resume_item_detail(&item, cx);
    }

    fn user_item_by_id(&self, item_id: &str) -> Option<UserItem> {
        let current_route_item = match self.navigation.current() {
            HomeRoute::Root(HomeRoot::Favorites) => {
                self.favorites.items.iter().find(|item| item.id == item_id)
            }
            HomeRoute::Root(HomeRoot::Search) => {
                self.search.items.iter().find(|item| item.id == item_id)
            }
            HomeRoute::Library { view_id, .. } => self
                .libraries
                .get(view_id)
                .and_then(|library| library.paged.items.iter().find(|item| item.id == item_id)),
            HomeRoute::Detail { .. } => self
                .series_detail
                .as_ref()
                .and_then(|detail| detail.similar_items.as_ref())
                .and_then(|items| items.items.iter().find(|item| item.id == item_id)),
            HomeRoute::Root(HomeRoot::Home) => None,
        };

        let item = current_route_item
            .or_else(|| {
                self.user_view_items_rows
                    .values()
                    .filter_map(|row| row.items.as_ref())
                    .flat_map(|items| items.items.iter())
                    .find(|item| item.id == item_id)
            })
            .or_else(|| self.favorites.items.iter().find(|item| item.id == item_id))
            .or_else(|| self.search.items.iter().find(|item| item.id == item_id))
            .or_else(|| {
                self.libraries
                    .values()
                    .flat_map(|library| library.paged.items.iter())
                    .find(|item| item.id == item_id)
            })?;

        Some(self.effective_user_item(item).into_owned())
    }

    fn resume_item_by_id(&self, item_id: &str) -> Option<ResumeItem> {
        let item = self
            .resume_items
            .as_ref()?
            .items
            .iter()
            .find(|item| item.id == item_id)?;
        Some(self.effective_resume_item(item).into_owned())
    }

    pub(super) fn open_detail_state(
        &mut self,
        mut detail: SeriesDetailState,
        cx: &mut Context<Self>,
    ) {
        detail.resume_video_version = detail
            .resume_media_item_id()
            .and_then(|id| self.played_video_versions.get(id))
            .cloned();
        self.resume_item_context_menu = None;
        self.clear_all_notifications();
        if let Some(current) = self.series_detail.take() {
            self.detail_history.push(current);
        }
        self.navigation.push_detail(
            detail.series_id.clone(),
            detail.preferred_episode_id.clone(),
        );
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.series_detail = Some(detail);
        self.load_media_detail_effects(cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in super::super) fn close_series_detail(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.navigation.pop() {
            return;
        }
        self.clear_notifications_for_scope(NotificationScope::Detail);
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.series_detail = if matches!(
            self.navigation.current(),
            super::super::navigation::HomeRoute::Detail { .. }
        ) {
            self.detail_history.pop()
        } else {
            self.detail_history.clear();
            None
        };
        if let Some(detail) = self.series_detail.as_mut() {
            detail.reset_in_flight_effects();
            self.load_media_detail_effects(cx);
        }
        if self.navigation.current()
            == &super::super::navigation::HomeRoute::Root(
                super::super::navigation::HomeRoot::Favorites,
            )
        {
            self.enter_favorites_if_needed(cx);
        }
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in super::super) fn close_series_detail_select(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.open_select.take().is_some() {
            cx.notify();
        }
    }

    pub(in super::super) fn play_selected_media(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_notification(NotificationScope::Detail, DETAIL_PLAYBACK_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.playback_loading || detail.video_sources_loading() {
            return;
        }

        let selected = match selected_playback(
            detail,
            &self.current_server,
            PlaybackLanguagePreferences::get(cx),
        ) {
            Ok(selected) => selected,
            Err(error) => {
                let message: SharedString = error.into();
                detail.playback_failed = Some(message.clone());
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_PLAYBACK_NOTIFICATION_KEY,
                    message,
                    cx,
                );
                cx.notify();
                return;
            }
        };

        detail.playback_loading = true;
        detail.playback_failed = None;
        detail.open_select = None;
        cx.notify();

        let server = self.current_server.clone();
        let identity = self.request_identity();
        let generation = self.detail_generation;
        let client = self.emby_client.clone();
        let task_server = server.clone();
        let task_item_id = selected.item_id.clone();
        let task_media_source_id = selected.media_source_id.clone();
        let task = cx.background_spawn(async move {
            let playback_info =
                client.playback_info(&task_server, &task_item_id, &task_media_source_id)?;
            let source = playback_info.direct_stream_source_for(&task_media_source_id)?;
            let direct_stream_url = source.direct_stream_url()?;
            let playback_url = resolve_direct_stream_url(&task_server, direct_stream_url)?;
            let http_headers = client.playback_http_headers(&task_server)?;
            let media_source_id = source
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .unwrap_or(task_media_source_id.as_str())
                .to_string();
            Ok::<_, anyhow::Error>(ResolvedPlayback {
                item_id: source.playback_item_id(&task_item_id).to_string(),
                url: playback_url.to_string(),
                http_headers,
                content_length: source.size,
                media_source_id,
                play_session_id: playback_info.play_session_id,
            })
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_play_selected_media(identity, generation, selected, result, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_play_selected_media(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        selected: SelectedPlayback,
        result: anyhow::Result<ResolvedPlayback>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || !self.selected_playback_still_current(&selected)
        {
            return;
        }

        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        detail.playback_loading = false;
        match result {
            Ok(playback) => {
                detail.playback_failed = None;
                let initial_position_seconds = playback_initial_position_seconds(
                    selected.playback_position_ticks,
                    selected.run_time_ticks,
                );
                cx.emit(HomeContentEvent::OpenPlayback(Box::new(PlaybackRequest {
                    title: selected.title,
                    url: playback.url,
                    http_headers: playback.http_headers,
                    content_length: playback.content_length,
                    audio_tracks: selected.audio_tracks,
                    subtitle_tracks: selected.subtitle_tracks,
                    selected_tracks: selected.selected_tracks,
                    initial_position_seconds,
                    queue: selected.queue,
                    emby: EmbyPlaybackContext {
                        client: self.emby_client.clone(),
                        server: self.current_server.clone(),
                        item_id: playback.item_id,
                        media_source_id: playback.media_source_id,
                        play_session_id: playback
                            .play_session_id
                            .filter(|id| !id.trim().is_empty()),
                        run_time_ticks: selected.run_time_ticks,
                    },
                })));
            }
            Err(error) => {
                let message: SharedString = format!("获取播放地址失败：{error}").into();
                detail.playback_failed = Some(message.clone());
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_PLAYBACK_NOTIFICATION_KEY,
                    message,
                    cx,
                );
            }
        }

        cx.notify();
    }

    pub(super) fn selected_playback_still_current(&self, selected: &SelectedPlayback) -> bool {
        self.series_detail.as_ref().is_some_and(|detail| {
            if detail.series_id.as_str() != selected.detail_id.as_str() {
                return false;
            }
            let item_matches = detail
                .selected_playback_item()
                .is_some_and(|item| item.id.as_str() == selected.list_item_id.as_str());
            let source_matches = detail
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                .is_some_and(|id| id == selected.media_source_id.as_str());

            item_matches && source_matches
        })
    }
}
