mod images;
mod render;
mod state;

pub(crate) use state::{SeriesDetailSelectKind, SeriesDetailState};

use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, SharedString, Window, point, px};

use crate::{
    emby::{
        MediaItem, MediaItems, ResumeItem, UserItem, UserItems, playback::resolve_direct_stream_url,
    },
    player::{PlaybackRequest, PlaybackTrack, PlaybackTrackSelection},
    server::CachedServer,
};

use super::{
    HomeContent, HomeContentEvent, LoadState, WorkspaceIdentity,
    carousel::DETAIL_EPISODE_CARD_STEP_PX,
};

struct SelectedPlayback {
    detail_id: String,
    item_id: String,
    media_source_id: String,
    title: SharedString,
    audio_tracks: Vec<PlaybackTrack>,
    subtitle_tracks: Vec<PlaybackTrack>,
    selected_tracks: PlaybackTrackSelection,
}

struct ResolvedPlayback {
    url: String,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
}

impl HomeContent {
    pub(super) fn open_media_detail(&mut self, item: &UserItem, cx: &mut Context<Self>) {
        let Some(detail) = SeriesDetailState::from_user_item(item) else {
            return;
        };

        self.open_detail_state(detail, cx);
    }

    pub(super) fn open_resume_item_detail(&mut self, item: &ResumeItem, cx: &mut Context<Self>) {
        match item.item_type.as_deref() {
            Some("Movie") => {
                let Some(detail) = SeriesDetailState::from_resume_movie(item) else {
                    return;
                };
                self.open_detail_state(detail, cx);
            }
            Some("Episode") => {
                let Some(detail) = SeriesDetailState::from_resume_episode(item) else {
                    self.resume_detail_failed =
                        Some("继续观看剧集缺少 SeriesId，无法打开详情".into());
                    cx.notify();
                    return;
                };
                self.open_detail_state(detail, cx);
            }
            _ => {}
        }
    }

    fn open_detail_state(&mut self, detail: SeriesDetailState, cx: &mut Context<Self>) {
        self.resume_detail_failed = None;
        self.favorite_failures.remove(&detail.series_id);
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

    pub(super) fn close_series_detail(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.navigation.pop() {
            return;
        }
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.series_detail = if matches!(
            self.navigation.current(),
            super::navigation::HomeRoute::Detail { .. }
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
            == &super::navigation::HomeRoute::Root(super::navigation::HomeRoot::Favorites)
        {
            self.enter_favorites_if_needed(cx);
        }
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn close_series_detail_select(
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

    pub(super) fn play_selected_media(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.playback_loading {
            return;
        }

        let selected = match selected_playback(detail, &self.current_server) {
            Ok(selected) => selected,
            Err(error) => {
                detail.playback_failed = Some(error.into());
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
            Ok::<_, anyhow::Error>(ResolvedPlayback {
                url: playback_url.to_string(),
                http_headers,
                content_length: source.size,
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

    fn finish_play_selected_media(
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
                cx.emit(HomeContentEvent::OpenPlayback(Box::new(PlaybackRequest {
                    title: selected.title,
                    url: playback.url,
                    http_headers: playback.http_headers,
                    content_length: playback.content_length,
                    audio_tracks: selected.audio_tracks,
                    subtitle_tracks: selected.subtitle_tracks,
                    selected_tracks: selected.selected_tracks,
                })));
            }
            Err(error) => {
                detail.playback_failed = Some(format!("获取播放地址失败：{error}").into());
            }
        }

        cx.notify();
    }

    fn selected_playback_still_current(&self, selected: &SelectedPlayback) -> bool {
        self.series_detail.as_ref().is_some_and(|detail| {
            if detail.series_id.as_str() != selected.detail_id.as_str() {
                return false;
            }
            let item_matches = detail
                .selected_playback_item()
                .is_some_and(|item| item.id.as_str() == selected.item_id.as_str());
            let source_matches = detail
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                .is_some_and(|id| id == selected.media_source_id.as_str());

            item_matches && source_matches
        })
    }

    fn load_media_detail_effects(&mut self, cx: &mut Context<Self>) {
        self.load_series_media_item_if_needed(cx);
        self.load_similar_items_if_needed(cx);
        if self
            .series_detail
            .as_ref()
            .is_some_and(SeriesDetailState::is_series)
        {
            self.load_series_seasons_if_needed(cx);
            if self
                .series_detail
                .as_ref()
                .is_some_and(SeriesDetailState::should_load_next_up)
            {
                self.load_series_next_up_if_needed(cx);
            }
            self.load_series_episodes_if_needed(cx);
        }
    }

    fn load_series_media_item_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let generation = self.detail_generation;
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.item.can_start() {
            return;
        }

        detail.effects.item = LoadState::Loading;
        detail.item_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task = cx.background_spawn(async move { client.media_item(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_media_item(
                    identity,
                    user_data_revision,
                    generation,
                    series_id,
                    result,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_media_item(
        &mut self,
        identity: WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItem>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || self
                .series_detail
                .as_ref()
                .is_none_or(|detail| detail.series_id.as_str() != series_id.as_str())
        {
            return;
        }

        match result {
            Ok(item) => {
                self.ensure_series_media_item_images(&item, cx);
                self.absorb_user_data(&item.id, item.user_data.as_ref(), user_data_revision);
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str() {
                        return;
                    }

                    detail.effects.item = LoadState::Loaded;
                    detail.item_failed = None;
                    if detail.title != item.name {
                        detail.title = item.name.clone();
                        cx.emit(HomeContentEvent::TitleChanged);
                    }
                    detail.item = Some(item);
                    if detail.is_movie() {
                        detail.sync_media_source_selection();
                    }
                }
            }
            Err(error) => {
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str() {
                        return;
                    }

                    detail.effects.item = LoadState::Failed;
                    detail.item_failed = Some(format!("加载媒体详情失败：{error}").into());
                }
            }
        }

        cx.notify();
    }

    fn load_similar_items_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let generation = self.detail_generation;
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.similar.can_start() {
            return;
        }

        detail.effects.similar = LoadState::Loading;
        detail.similar_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let item_id = detail.series_id.clone();
        let task_item_id = item_id.clone();
        let task = cx.background_spawn(async move { client.similar_items(&server, &task_item_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_similar_items(
                    identity,
                    user_data_revision,
                    generation,
                    item_id,
                    result,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    fn finish_similar_items(
        &mut self,
        identity: WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        item_id: String,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || self
                .series_detail
                .as_ref()
                .is_none_or(|detail| detail.series_id.as_str() != item_id.as_str())
        {
            return;
        }

        match result {
            Ok(mut items) => {
                items.items.retain(|item| {
                    !item.id.trim().is_empty()
                        && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
                });
                items.total_record_count = items.items.len() as u32;
                self.absorb_user_items_user_data(&items, user_data_revision);
                self.ensure_user_items_images(&items, cx);
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != item_id.as_str() {
                        return;
                    }

                    detail.effects.similar = LoadState::Loaded;
                    detail.similar_failed = None;
                    detail.similar_items = Some(items);
                }
            }
            Err(error) => {
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != item_id.as_str() {
                        return;
                    }

                    detail.effects.similar = LoadState::Failed;
                    detail.similar_failed = Some(format!("加载相似作品失败：{error}").into());
                }
            }
        }

        cx.notify();
    }

    fn load_series_seasons_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let generation = self.detail_generation;
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.is_series() || !detail.effects.seasons.can_start() {
            return;
        }

        detail.effects.seasons = LoadState::Loading;
        detail.seasons_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_seasons(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_seasons(identity, generation, series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_seasons(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || !self.matches_request_identity(&identity) {
            return;
        }
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.series_id.as_str() != series_id.as_str() {
            return;
        }

        match result {
            Ok(seasons) => {
                detail.effects.seasons = LoadState::Loaded;
                detail.seasons_failed = None;
                detail.seasons = Some(seasons);
                detail.choose_season_if_needed();
                self.load_series_episodes_if_needed(cx);
            }
            Err(error) => {
                detail.effects.seasons = LoadState::Failed;
                detail.seasons_failed = Some(format!("加载剧集季数失败：{error}").into());
            }
        }

        cx.notify();
    }

    fn load_series_next_up_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let generation = self.detail_generation;
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.should_load_next_up() || !detail.effects.next_up.can_start() {
            return;
        }

        detail.effects.next_up = LoadState::Loading;
        detail.next_up_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_next_up(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_next_up(identity, generation, series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_next_up(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || !self.matches_request_identity(&identity) {
            return;
        }
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.series_id.as_str() != series_id.as_str() {
            return;
        }

        match result {
            Ok(next_up) => {
                detail.effects.next_up = LoadState::Loaded;
                detail.next_up_failed = None;
                detail.next_up = Some(next_up);
                detail.apply_next_up_preference();
                self.load_series_episodes_if_needed(cx);
            }
            Err(error) => {
                detail.effects.next_up = LoadState::Failed;
                detail.next_up_failed = Some(format!("加载下一剧集失败：{error}").into());
                detail.choose_season_if_needed();
                self.load_series_episodes_if_needed(cx);
            }
        }

        cx.notify();
    }

    fn load_series_episodes_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let generation = self.detail_generation;
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.is_series() {
            return;
        }
        let Some(season_id) = detail.selected_season_id.clone() else {
            return;
        };
        let already_requested =
            detail.episodes_request_season_id.as_deref() == Some(season_id.as_str());
        if already_requested && detail.effects.episodes.is_loading() {
            return;
        }
        if already_requested
            && detail.effects.episodes == LoadState::Loaded
            && detail.episodes.is_some()
        {
            return;
        }

        detail.effects.episodes = LoadState::Loading;
        detail.episodes_failed = None;
        detail.episodes_request_season_id = Some(season_id.clone());
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task_season_id = season_id.clone();
        let task = cx.background_spawn(async move {
            client.show_episodes(&server, &task_series_id, Some(&task_season_id))
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_episodes(identity, generation, series_id, season_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_episodes(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        series_id: String,
        season_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || self.series_detail.as_ref().is_none_or(|detail| {
                detail.series_id.as_str() != series_id.as_str()
                    || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                    || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
            })
        {
            return;
        }

        match result {
            Ok(episodes) => {
                self.ensure_series_episode_images(&episodes, cx);
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str()
                        || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                        || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
                    {
                        return;
                    }

                    detail.effects.episodes = LoadState::Loaded;
                    detail.episodes_failed = None;
                    detail.episodes = Some(episodes);
                    detail.choose_episode_from_loaded_episodes();
                    if detail.should_reveal_selected_episode()
                        && let Some(index) = detail.selected_episode_index()
                    {
                        detail.episodes_carousel.set_scroll_offset(
                            index as f32 * DETAIL_EPISODE_CARD_STEP_PX,
                            f32::INFINITY,
                        );
                        detail.episodes_carousel.sync_previous_offset();
                    }
                }
            }
            Err(error) => {
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str()
                        || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                        || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
                    {
                        return;
                    }

                    detail.effects.episodes = LoadState::Failed;
                    detail.episodes_failed = Some(format!("加载剧集分集失败：{error}").into());
                }
            }
        }

        cx.notify();
    }

    pub(super) fn select_series_season(&mut self, season_id: String, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.selected_season_id.as_deref() == Some(season_id.as_str()) {
            detail.open_select = None;
            cx.notify();
            return;
        }

        detail.selected_season_id = Some(season_id);
        detail.preferred_episode_id = None;
        detail.clear_preferred_season_hint();
        detail.open_select = None;
        detail.reset_episode_selection();
        self.load_series_episodes_if_needed(cx);
        cx.notify();
    }

    pub(super) fn toggle_series_season_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let Some(seasons) = detail.seasons.as_ref() else {
            return;
        };
        if seasons.items.is_empty() {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::Season);
        if opening {
            let selected_index = detail
                .selected_season_id
                .as_deref()
                .and_then(|selected_id| {
                    seasons
                        .items
                        .iter()
                        .position(|season| season.id == selected_id)
                })
                .unwrap_or(0);
            detail.season_scroll_handle.scroll_to_item(selected_index);
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::Season);
        cx.notify();
    }

    pub(super) fn select_series_episode(&mut self, episode_id: String, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let exists = detail.episodes.as_ref().is_some_and(|episodes| {
            episodes
                .items
                .iter()
                .any(|episode| episode.id == episode_id)
        });
        if !exists {
            return;
        }

        detail.preferred_episode_id = Some(episode_id.clone());
        detail.apply_selected_episode(Some(episode_id));
        detail.open_select = None;
        cx.notify();
    }

    pub(super) fn toggle_series_media_source_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let source_count = detail
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if source_count == 0 {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::MediaSource);
        if opening {
            detail
                .media_source_scroll_handle
                .scroll_to_item(detail.selected_media_source_index().unwrap_or(0));
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::MediaSource);
        cx.notify();
    }

    pub(super) fn toggle_series_subtitle_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let subtitle_count = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if subtitle_count == 0 {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::Subtitle);
        if opening {
            detail
                .subtitle_scroll_handle
                .scroll_to_item(detail.selected_subtitle_index().unwrap_or(0));
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::Subtitle);
        cx.notify();
    }

    pub(super) fn select_series_media_source(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let source_count = detail
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if index >= source_count {
            return;
        }

        detail.selected_media_source_index = Some(index);
        detail.selected_subtitle_index = None;
        detail
            .subtitle_scroll_handle
            .set_offset(point(px(0.0), px(0.0)));
        detail.open_select = None;
        detail.reset_playback_request();
        detail.sync_media_source_selection();
        cx.notify();
    }

    pub(super) fn select_series_subtitle(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let subtitle_count = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if index >= subtitle_count {
            return;
        }

        detail.selected_subtitle_index = Some(index);
        detail.open_select = None;
        cx.notify();
    }
}

fn selected_playback(
    detail: &SeriesDetailState,
    server: &CachedServer,
) -> Result<SelectedPlayback, String> {
    let item = detail
        .selected_playback_item()
        .ok_or_else(|| "请选择要播放的媒体".to_string())?;
    let source = detail
        .selected_media_source()
        .ok_or_else(|| "请选择视频源".to_string())?;
    let media_source_id = source
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "所选视频源缺少 ID，无法获取播放地址".to_string())?
        .to_string();
    let title = if detail.is_movie() {
        item.name.clone()
    } else {
        let series_name = detail
            .item
            .as_ref()
            .map(|item| item.name.clone())
            .unwrap_or_else(|| detail.title.clone());
        format!("{series_name} {}", item.episode_label())
    };

    let audio_tracks = playback_audio_tracks(source);
    let subtitle_tracks = playback_subtitle_tracks(source, server, &item.id, &media_source_id);
    let selected_tracks = playback_track_selection(detail, source, &subtitle_tracks);

    Ok(SelectedPlayback {
        detail_id: detail.series_id.clone(),
        item_id: item.id.clone(),
        media_source_id,
        title: title.into(),
        audio_tracks,
        subtitle_tracks,
        selected_tracks,
    })
}

fn playback_audio_tracks(source: &crate::emby::MediaSource) -> Vec<PlaybackTrack> {
    source
        .audio_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let stream_index = usize::try_from(stream.index?).ok()?;
            Some(PlaybackTrack::new(
                stream_index,
                stream.audio_label(index),
                stream.is_external.unwrap_or(false),
            ))
        })
        .collect()
}

fn playback_subtitle_tracks(
    source: &crate::emby::MediaSource,
    server: &CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Vec<PlaybackTrack> {
    source
        .subtitle_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let stream_index = usize::try_from(stream.index?).ok()?;
            let external_url =
                playback_subtitle_external_url(stream, server, item_id, media_source_id);
            Some(
                PlaybackTrack::new(
                    stream_index,
                    stream.display_title_label(index),
                    stream.is_external.unwrap_or(false),
                )
                .with_external_url(external_url)
                .with_codec(stream.codec.clone()),
            )
        })
        .collect()
}

fn playback_subtitle_external_url(
    stream: &crate::emby::MediaStream,
    server: &CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    if !is_external_subtitle_stream(stream) {
        return None;
    }

    let delivery_url = stream
        .delivery_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| fallback_external_subtitle_delivery_url(stream, item_id, media_source_id))?;
    let mut url = resolve_direct_stream_url(server, &delivery_url).ok()?;
    if !url.query_pairs().any(|(name, _)| name == "api_key")
        && let Some(access_token) = server
            .access_token
            .as_deref()
            .filter(|token| !token.is_empty())
    {
        url.query_pairs_mut().append_pair("api_key", access_token);
    }
    Some(url.to_string())
}

fn is_external_subtitle_stream(stream: &crate::emby::MediaStream) -> bool {
    stream.is_external.unwrap_or(false)
        || stream
            .delivery_method
            .as_deref()
            .is_some_and(|method| method.eq_ignore_ascii_case("External"))
}

fn fallback_external_subtitle_delivery_url(
    stream: &crate::emby::MediaStream,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    let stream_index = stream.index?;
    let extension = external_subtitle_extension(stream.codec.as_deref())?;
    Some(format!(
        "/Videos/{item_id}/{media_source_id}/Subtitles/{stream_index}/0/Stream.{extension}"
    ))
}

fn external_subtitle_extension(codec: Option<&str>) -> Option<&'static str> {
    match codec?.trim().to_ascii_lowercase().as_str() {
        "ass" => Some("ass"),
        "ssa" => Some("ssa"),
        "subrip" | "srt" => Some("srt"),
        "vtt" | "webvtt" => Some("vtt"),
        "sub" => Some("sub"),
        _ => None,
    }
}

fn playback_track_selection(
    detail: &SeriesDetailState,
    source: &crate::emby::MediaSource,
    subtitle_tracks: &[PlaybackTrack],
) -> PlaybackTrackSelection {
    let default_audio_stream_index = source.audio_streams().into_iter().find_map(|stream| {
        stream
            .index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|_| stream.is_default.unwrap_or(false))
    });
    let audio_stream_index = default_audio_stream_index.or_else(|| {
        source
            .audio_streams()
            .into_iter()
            .find_map(|stream| stream.index.and_then(|index| usize::try_from(index).ok()))
    });
    let selected_subtitle = detail
        .selected_subtitle_index()
        .and_then(|index| subtitle_tracks.get(index));

    let mut selection = PlaybackTrackSelection {
        audio_stream_index,
        default_audio_stream_index,
        ..Default::default()
    };
    selection.set_subtitle_track(selected_subtitle);
    selection
}

#[cfg(test)]
mod tests {
    use crate::{
        emby::{MediaSource, MediaStream},
        server::{Protocol, ServerEndpoint},
    };

    use super::*;

    fn server() -> CachedServer {
        CachedServer {
            id: "server-detail-test".to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: "/emby".to_string(),
            },
            username: "luv".to_string(),
            password: "secret".to_string(),
            user_id: Some("user-1".to_string()),
            server_id: Some("server-1".to_string()),
            server_name: Some("Home".to_string()),
            access_token: Some("token".to_string()),
            item_counts: None,
            added_at_unix: 123,
        }
    }

    #[test]
    fn playback_subtitle_tracks_resolve_external_ass_delivery_url() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(3),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("(ASS)".to_string()),
                title: Some("chs&eng".to_string()),
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: Some(
                    "/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
                        .to_string(),
                ),
                delivery_method: Some("External".to_string()),
                is_external: Some(true),
                is_default: Some(false),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 3);
        assert!(tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("ass"));
        assert_eq!(
            tracks[0].external_url.as_deref(),
            Some(
                "https://example.com/emby/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
            )
        );
    }

    #[test]
    fn playback_subtitle_tracks_build_external_ass_url_when_delivery_url_missing() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(3),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("(ASS)".to_string()),
                title: Some("chs&eng".to_string()),
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: Some("External".to_string()),
                is_external: Some(true),
                is_default: Some(false),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].is_external);
        assert_eq!(
            tracks[0].external_url.as_deref(),
            Some(
                "https://example.com/emby/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
            )
        );
    }

    #[test]
    fn playback_subtitle_tracks_keep_internal_ass_on_embedded_stream() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(2),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("Chinese Simplified (默认 ASS)".to_string()),
                title: Some("Simplified Chinese (简体中文)".to_string()),
                language: Some("chi".to_string()),
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: Some("Embed".to_string()),
                is_external: Some(false),
                is_default: Some(true),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 2);
        assert!(!tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("ass"));
        assert_eq!(tracks[0].external_url, None);
    }

    #[test]
    fn playback_subtitle_tracks_keep_internal_subrip_on_embedded_stream() {
        let source = MediaSource {
            id: Some("mediasource_824061".to_string()),
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(2),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("Chinese Simplified (默认 SUBRIP)".to_string()),
                title: Some("Chinese Simplified".to_string()),
                language: Some("chi".to_string()),
                codec: Some("subrip".to_string()),
                delivery_url: None,
                delivery_method: Some("Embed".to_string()),
                is_external: Some(false),
                is_default: Some(true),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "824061", "mediasource_824061");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 2);
        assert!(!tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("subrip"));
        assert_eq!(tracks[0].external_url, None);
    }
}
