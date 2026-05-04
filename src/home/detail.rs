use std::path::PathBuf;

use gpui::{AppContext as _, ClickEvent, Context, Window, point, px};

use crate::emby::{
    EmbyImageRequest, EmbyImageType, ImageQuality, MediaItem, MediaItems, MediaSource, UserItem,
};

use super::{HomeContent, LoadState, SeriesDetailSelectKind, SeriesDetailState};

const SERIES_BACKDROP_IMAGE_MAX_WIDTH: u32 = 3000;
const SERIES_EPISODE_IMAGE_MAX_WIDTH: u32 = 640;

impl SeriesDetailState {
    fn new(item: &UserItem) -> Self {
        Self {
            series_id: item.id.clone(),
            title: item.name.clone(),
            effects: Default::default(),
            item: None,
            item_failed: None,
            seasons: None,
            seasons_failed: None,
            next_up: None,
            next_up_failed: None,
            episodes: None,
            episodes_failed: None,
            selected_season_id: None,
            selected_episode_id: None,
            preferred_episode_id: None,
            selected_media_source_index: None,
            selected_subtitle_index: None,
            open_select: None,
            episodes_request_season_id: None,
            episodes_carousel: Default::default(),
        }
    }

    pub(super) fn next_up_episode(&self) -> Option<&MediaItem> {
        self.next_up.as_ref().and_then(|items| items.items.first())
    }

    fn preferred_season_id(&self) -> Option<String> {
        self.next_up_episode()
            .and_then(|episode| episode.season_id.clone())
    }

    fn fallback_season_id(&self) -> Option<String> {
        self.seasons
            .as_ref()
            .and_then(|seasons| seasons.items.first())
            .map(|season| season.id.clone())
    }

    pub(super) fn selected_season(&self) -> Option<&MediaItem> {
        let selected = self.selected_season_id.as_deref();
        self.seasons.as_ref().and_then(|seasons| {
            selected
                .and_then(|season_id| seasons.items.iter().find(|season| season.id == season_id))
                .or_else(|| seasons.items.first())
        })
    }

    pub(super) fn selected_episode(&self) -> Option<&MediaItem> {
        let episodes = self.episodes.as_ref()?;
        self.selected_episode_id
            .as_deref()
            .and_then(|episode_id| {
                episodes
                    .items
                    .iter()
                    .find(|episode| episode.id == episode_id)
            })
            .or_else(|| episodes.items.first())
    }

    pub(super) fn selected_media_source(&self) -> Option<&MediaSource> {
        let episode = self.selected_episode()?;
        let sources = episode.media_sources.as_deref()?;
        let index = self.selected_media_source_index()?;
        sources.get(index)
    }

    pub(super) fn selected_media_source_index(&self) -> Option<usize> {
        let source_count = self
            .selected_episode()
            .and_then(|episode| episode.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if source_count == 0 {
            return None;
        }

        Some(
            self.selected_media_source_index
                .filter(|index| *index < source_count)
                .unwrap_or(0),
        )
    }

    pub(super) fn selected_subtitle_index(&self) -> Option<usize> {
        let subtitle_count = self
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if subtitle_count == 0 {
            return None;
        }

        Some(
            self.selected_subtitle_index
                .filter(|index| *index < subtitle_count)
                .unwrap_or(0),
        )
    }

    pub(super) fn selected_media_source_label(&self) -> String {
        let Some(episode) = self.selected_episode() else {
            return "暂无视频源".to_string();
        };
        let Some(sources) = episode.media_sources.as_deref() else {
            return "暂无视频源".to_string();
        };
        let Some(index) = self.selected_media_source_index() else {
            return "暂无视频源".to_string();
        };
        sources
            .get(index)
            .map(|source| source.name_label(index))
            .unwrap_or_else(|| "暂无视频源".to_string())
    }

    pub(super) fn selected_subtitle_label(&self) -> String {
        let Some(source) = self.selected_media_source() else {
            return "无字幕".to_string();
        };
        let subtitles = source.subtitle_streams();
        let Some(index) = self.selected_subtitle_index() else {
            return "无字幕".to_string();
        };
        subtitles
            .get(index)
            .map(|stream| stream.display_title_label(index))
            .unwrap_or_else(|| "无字幕".to_string())
    }

    fn choose_season_if_needed(&mut self) -> bool {
        let current_valid = self.selected_season_id.as_deref().is_some_and(|season_id| {
            self.seasons
                .as_ref()
                .is_none_or(|seasons| seasons.items.iter().any(|season| season.id == season_id))
        });
        if current_valid {
            return false;
        }

        let season_id = self
            .preferred_season_id()
            .or_else(|| self.fallback_season_id());
        if self.selected_season_id == season_id {
            return false;
        }

        self.selected_season_id = season_id;
        self.reset_episode_selection();
        true
    }

    fn apply_next_up_preference(&mut self) -> bool {
        let Some((next_up_episode_id, next_up_season_id)) = self
            .next_up_episode()
            .map(|episode| (episode.id.clone(), episode.season_id.clone()))
        else {
            return self.choose_season_if_needed();
        };

        self.preferred_episode_id = Some(next_up_episode_id);
        if next_up_season_id.is_some() && self.selected_season_id != next_up_season_id {
            self.selected_season_id = next_up_season_id;
            self.reset_episode_selection();
            return true;
        }

        self.choose_season_if_needed()
    }

    fn choose_episode_from_loaded_episodes(&mut self) {
        let episode_ids = self
            .episodes
            .as_ref()
            .map(|episodes| {
                episodes
                    .items
                    .iter()
                    .map(|episode| episode.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let selected = self
            .preferred_episode_id
            .as_ref()
            .filter(|episode_id| episode_ids.iter().any(|id| id == *episode_id))
            .cloned()
            .or_else(|| {
                self.selected_episode_id
                    .as_ref()
                    .filter(|episode_id| episode_ids.iter().any(|id| id == *episode_id))
                    .cloned()
            })
            .or_else(|| episode_ids.first().cloned());

        self.apply_selected_episode(selected);
    }

    fn apply_selected_episode(&mut self, episode_id: Option<String>) {
        if self.selected_episode_id != episode_id {
            self.selected_episode_id = episode_id;
            self.selected_media_source_index = None;
            self.selected_subtitle_index = None;
            self.open_select = None;
        }
        self.sync_media_source_selection();
    }

    fn reset_episode_selection(&mut self) {
        self.episodes = None;
        self.episodes_failed = None;
        self.effects.episodes = LoadState::Idle;
        self.episodes_request_season_id = None;
        self.selected_episode_id = None;
        self.selected_media_source_index = None;
        self.selected_subtitle_index = None;
        self.open_select = None;
        self.episodes_carousel = Default::default();
    }

    fn sync_media_source_selection(&mut self) {
        let source_count = self
            .selected_episode()
            .and_then(|episode| episode.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if source_count == 0 {
            self.selected_media_source_index = None;
            self.selected_subtitle_index = None;
            self.open_select = None;
            return;
        }

        if self
            .selected_media_source_index
            .is_none_or(|index| index >= source_count)
        {
            self.selected_media_source_index = Some(0);
        }

        let subtitle_count = self
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if subtitle_count == 0 {
            self.selected_subtitle_index = None;
            if self.open_select == Some(SeriesDetailSelectKind::Subtitle) {
                self.open_select = None;
            }
        } else if self
            .selected_subtitle_index
            .is_none_or(|index| index >= subtitle_count)
        {
            self.selected_subtitle_index = Some(0);
        }
    }
}

impl HomeContent {
    pub(super) fn open_series_detail(&mut self, item: &UserItem, cx: &mut Context<Self>) {
        if item.item_type.as_deref() != Some("Series") {
            return;
        }

        self.series_detail = Some(SeriesDetailState::new(item));
        self.main_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.main_scrollbar_drag = None;
        self.load_series_detail_effects(cx);
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
        cx.notify();
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
                    detail.title = item.name.clone();
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

    fn ensure_series_media_item_images(&mut self, item: &MediaItem, cx: &mut Context<Self>) {
        if let Some(request) = series_backdrop_image_request(item) {
            self.ensure_image(request, cx);
        }
        if let Some(request) = series_logo_image_request(item) {
            self.ensure_image(request, cx);
        }
    }

    fn ensure_series_episode_images(&mut self, episodes: &MediaItems, cx: &mut Context<Self>) {
        for episode in &episodes.items {
            if let Some(request) = episode_primary_image_request(episode) {
                self.ensure_image(request, cx);
            }
        }
    }

    pub(super) fn image_path_for_series_backdrop(&self, item: &MediaItem) -> Option<PathBuf> {
        let request = series_backdrop_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_series_logo(&self, item: &MediaItem) -> Option<PathBuf> {
        let request = series_logo_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_episode_primary(&self, episode: &MediaItem) -> Option<PathBuf> {
        let request = episode_primary_image_request(episode)?;
        self.image_path_for_request(&request)
    }
}

fn series_backdrop_image_request(item: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(item.id.clone(), EmbyImageType::Backdrop)
            .with_tag(Some(item.backdrop_image_tag()?.to_string()))
            .with_max_width(SERIES_BACKDROP_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT),
    )
}

fn series_logo_image_request(item: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(item.id.clone(), EmbyImageType::Logo)
            .with_tag(Some(item.logo_image_tag()?.to_string()))
            .with_quality(ImageQuality::DEFAULT),
    )
}

fn episode_primary_image_request(episode: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(episode.id.clone(), EmbyImageType::Primary)
            .with_tag(Some(episode.primary_image_tag()?.to_string()))
            .with_max_width(SERIES_EPISODE_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT),
    )
}

#[cfg(test)]
mod tests {
    use crate::emby::MediaItems;

    use super::*;

    fn media_item(id: &str, name: &str) -> MediaItem {
        MediaItem {
            id: id.to_string(),
            name: name.to_string(),
            item_type: Some("Episode".to_string()),
            server_id: None,
            production_year: None,
            premiere_date: None,
            run_time_ticks: None,
            index_number: None,
            parent_index_number: None,
            is_folder: None,
            community_rating: None,
            official_rating: None,
            genres: None,
            overview: None,
            series_name: None,
            series_id: None,
            season_id: None,
            season_name: None,
            media_type: None,
            image_tags: None,
            backdrop_image_tags: None,
            parent_logo_item_id: None,
            parent_logo_image_tag: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
            series_primary_image_tag: None,
            media_sources: None,
        }
    }

    #[test]
    fn chooses_next_up_episode_when_loaded_episodes_include_it() {
        let mut detail = SeriesDetailState {
            series_id: "series-1".to_string(),
            title: "Series".to_string(),
            effects: Default::default(),
            item: None,
            item_failed: None,
            seasons: None,
            seasons_failed: None,
            next_up: Some(MediaItems {
                items: vec![media_item("episode-2", "第二集")],
                total_record_count: 2,
            }),
            next_up_failed: None,
            episodes: Some(MediaItems {
                items: vec![
                    media_item("episode-1", "第一集"),
                    media_item("episode-2", "第二集"),
                ],
                total_record_count: 2,
            }),
            episodes_failed: None,
            selected_season_id: None,
            selected_episode_id: None,
            preferred_episode_id: Some("episode-2".to_string()),
            selected_media_source_index: None,
            selected_subtitle_index: None,
            open_select: None,
            episodes_request_season_id: None,
            episodes_carousel: Default::default(),
        };

        detail.choose_episode_from_loaded_episodes();

        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
    }
}
