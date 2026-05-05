mod images;
mod render;
mod state;

pub(crate) use state::{SeriesDetailSelectKind, SeriesDetailState};

use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, Window, point, px};

use crate::emby::{MediaItem, MediaItems, UserItem};

use super::{HomeContent, HomeContentEvent, LoadState};

impl HomeContent {
    pub(super) fn open_series_detail(&mut self, item: &UserItem, cx: &mut Context<Self>) {
        if item.item_type.as_deref() != Some("Series") {
            return;
        }

        self.series_detail = Some(SeriesDetailState::new(item));
        self.main_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.main_scrollbar_drag = None;
        self.load_series_detail_effects(cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn close_series_detail(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.series_detail = None;
        self.main_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.main_scrollbar_drag = None;
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

    fn load_series_detail_effects(&mut self, cx: &mut Context<Self>) {
        self.load_series_media_item_if_needed(cx);
        self.load_series_seasons_if_needed(cx);
        self.load_series_next_up_if_needed(cx);
        self.load_series_episodes_if_needed(cx);
    }

    fn load_series_media_item_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.item.can_start() {
            return;
        }

        detail.effects.item = LoadState::Loading;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task = cx.background_spawn(async move { client.media_item(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_media_item(series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_media_item(
        &mut self,
        series_id: String,
        result: anyhow::Result<MediaItem>,
        cx: &mut Context<Self>,
    ) {
        if self
            .series_detail
            .as_ref()
            .is_none_or(|detail| detail.series_id.as_str() != series_id.as_str())
        {
            return;
        }

        match result {
            Ok(item) => {
                self.ensure_series_media_item_images(&item, cx);
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

    fn load_series_seasons_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.seasons.can_start() {
            return;
        }

        detail.effects.seasons = LoadState::Loading;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_seasons(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_seasons(series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_seasons(
        &mut self,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
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
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.next_up.can_start() {
            return;
        }

        detail.effects.next_up = LoadState::Loading;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_next_up(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_next_up(series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_next_up(
        &mut self,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
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
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
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
                page.finish_series_episodes(series_id, season_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_series_episodes(
        &mut self,
        series_id: String,
        season_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.series_detail.as_ref().is_none_or(|detail| {
            detail.series_id.as_str() != series_id.as_str()
                || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
        }) {
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
            return;
        }

        detail.selected_season_id = Some(season_id);
        detail.preferred_episode_id = None;
        detail.reset_episode_selection();
        self.load_series_episodes_if_needed(cx);
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
            .selected_episode()
            .and_then(|episode| episode.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if source_count == 0 {
            return;
        }

        detail.open_select = if detail.open_select == Some(SeriesDetailSelectKind::MediaSource) {
            None
        } else {
            Some(SeriesDetailSelectKind::MediaSource)
        };
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

        detail.open_select = if detail.open_select == Some(SeriesDetailSelectKind::Subtitle) {
            None
        } else {
            Some(SeriesDetailSelectKind::Subtitle)
        };
        cx.notify();
    }

    pub(super) fn select_series_media_source(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let source_count = detail
            .selected_episode()
            .and_then(|episode| episode.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if index >= source_count {
            return;
        }

        detail.selected_media_source_index = Some(index);
        detail.selected_subtitle_index = None;
        detail.open_select = None;
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
