mod images;
mod render;
mod state;

pub(crate) use state::{SeriesDetailSelectKind, SeriesDetailState};

use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, SharedString, Window, point, px};

use crate::{
    emby::{MediaItem, MediaItems, UserItem, playback::resolve_direct_stream_url},
    player::{PlaybackRequest, PlaybackTrack, PlaybackTrackSelection},
    server::CachedServer,
};

use super::{HomeContent, HomeContentEvent, LoadState};

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

        self.series_detail = Some(detail);
        self.main_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.main_scrollbar_drag = None;
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
                page.finish_play_selected_media(selected, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_play_selected_media(
        &mut self,
        selected: SelectedPlayback,
        result: anyhow::Result<ResolvedPlayback>,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_playback_still_current(&selected) {
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
        if self
            .series_detail
            .as_ref()
            .is_some_and(SeriesDetailState::is_series)
        {
            self.load_series_seasons_if_needed(cx);
            self.load_series_next_up_if_needed(cx);
            self.load_series_episodes_if_needed(cx);
        }
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

    fn load_series_seasons_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.is_series() || !detail.effects.seasons.can_start() {
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
        if !detail.is_series() || !detail.effects.next_up.can_start() {
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
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
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
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if index >= source_count {
            return;
        }

        detail.selected_media_source_index = Some(index);
        detail.selected_subtitle_index = None;
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
    let subtitle_tracks = playback_subtitle_tracks(source, server);
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
) -> Vec<PlaybackTrack> {
    source
        .subtitle_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let stream_index = usize::try_from(stream.index?).ok()?;
            let external_url = stream.delivery_url.as_deref().and_then(|delivery_url| {
                resolve_direct_stream_url(server, delivery_url)
                    .map(|url| url.to_string())
                    .ok()
            });
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

fn playback_track_selection(
    detail: &SeriesDetailState,
    source: &crate::emby::MediaSource,
    subtitle_tracks: &[PlaybackTrack],
) -> PlaybackTrackSelection {
    let audio_stream_index = source
        .audio_streams()
        .into_iter()
        .find_map(|stream| {
            stream
                .index
                .and_then(|index| usize::try_from(index).ok())
                .filter(|_| stream.is_default.unwrap_or(false))
        })
        .or_else(|| {
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
        ..Default::default()
    };
    selection.set_subtitle_track(selected_subtitle);
    selection
}
