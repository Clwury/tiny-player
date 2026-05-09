use crate::emby::{MediaItem, MediaItems, MediaSource, UserItem};

use super::super::LoadState;

#[derive(Clone, Debug, Default)]
pub(crate) struct SeriesDetailEffects {
    pub(crate) item: LoadState,
    pub(crate) seasons: LoadState,
    pub(crate) next_up: LoadState,
    pub(crate) episodes: LoadState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeriesDetailSelectKind {
    MediaSource,
    Subtitle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeriesDetailKind {
    Series,
    Movie,
}

#[derive(Clone, Debug)]
pub(crate) struct SeriesDetailState {
    kind: SeriesDetailKind,
    pub(crate) series_id: String,
    pub(crate) title: String,
    pub(crate) effects: SeriesDetailEffects,
    pub(crate) item: Option<MediaItem>,
    pub(crate) item_failed: Option<gpui::SharedString>,
    pub(crate) seasons: Option<MediaItems>,
    pub(crate) seasons_failed: Option<gpui::SharedString>,
    pub(crate) next_up: Option<MediaItems>,
    pub(crate) next_up_failed: Option<gpui::SharedString>,
    pub(crate) episodes: Option<MediaItems>,
    pub(crate) episodes_failed: Option<gpui::SharedString>,
    pub(crate) playback_loading: bool,
    pub(crate) playback_failed: Option<gpui::SharedString>,
    pub(crate) selected_season_id: Option<String>,
    pub(crate) selected_episode_id: Option<String>,
    pub(crate) preferred_episode_id: Option<String>,
    pub(crate) selected_media_source_index: Option<usize>,
    pub(crate) selected_subtitle_index: Option<usize>,
    pub(crate) open_select: Option<SeriesDetailSelectKind>,
    pub(crate) episodes_request_season_id: Option<String>,
    pub(crate) episodes_carousel: super::super::carousel::CarouselState,
}

impl SeriesDetailState {
    pub(crate) fn from_user_item(item: &UserItem) -> Option<Self> {
        match item.item_type.as_deref() {
            Some("Series") => Some(Self::new_series(item)),
            Some("Movie") => Some(Self::new_movie(item)),
            _ => None,
        }
    }

    pub(crate) fn new_series(item: &UserItem) -> Self {
        Self::new(item, SeriesDetailKind::Series)
    }

    pub(crate) fn new_movie(item: &UserItem) -> Self {
        Self::new(item, SeriesDetailKind::Movie)
    }

    fn new(item: &UserItem, kind: SeriesDetailKind) -> Self {
        Self {
            kind,
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
            playback_loading: false,
            playback_failed: None,
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

    pub(crate) fn is_series(&self) -> bool {
        self.kind == SeriesDetailKind::Series
    }

    pub(crate) fn is_movie(&self) -> bool {
        self.kind == SeriesDetailKind::Movie
    }

    pub(crate) fn next_up_episode(&self) -> Option<&MediaItem> {
        self.next_up.as_ref().and_then(|items| items.items.first())
    }

    pub(crate) fn hero_episode(&self) -> Option<&MediaItem> {
        self.selected_episode().or_else(|| self.next_up_episode())
    }

    pub(crate) fn hero_line(&self) -> Option<String> {
        if self.is_movie() {
            self.item.as_ref().map(|item| item.name.clone())
        } else {
            self.hero_episode().map(MediaItem::episode_label)
        }
    }

    pub(crate) fn selected_playback_item(&self) -> Option<&MediaItem> {
        if self.is_movie() {
            self.item.as_ref()
        } else {
            self.selected_episode()
        }
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

    pub(crate) fn selected_season(&self) -> Option<&MediaItem> {
        let selected = self.selected_season_id.as_deref();
        self.seasons.as_ref().and_then(|seasons| {
            selected
                .and_then(|season_id| seasons.items.iter().find(|season| season.id == season_id))
                .or_else(|| seasons.items.first())
        })
    }

    pub(crate) fn selected_episode(&self) -> Option<&MediaItem> {
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

    pub(crate) fn selected_media_source(&self) -> Option<&MediaSource> {
        let item = self.selected_playback_item()?;
        let sources = item.media_sources.as_deref()?;
        let index = self.selected_media_source_index()?;
        sources.get(index)
    }

    pub(crate) fn selected_media_source_index(&self) -> Option<usize> {
        let source_count = self
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
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

    pub(crate) fn selected_subtitle_index(&self) -> Option<usize> {
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

    pub(crate) fn selected_media_source_label(&self) -> String {
        let Some(item) = self.selected_playback_item() else {
            return "暂无视频源".to_string();
        };
        let Some(sources) = item.media_sources.as_deref() else {
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

    pub(crate) fn selected_subtitle_label(&self) -> String {
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

    pub(crate) fn choose_season_if_needed(&mut self) -> bool {
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

    pub(crate) fn apply_next_up_preference(&mut self) -> bool {
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

    pub(crate) fn choose_episode_from_loaded_episodes(&mut self) {
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

    pub(crate) fn apply_selected_episode(&mut self, episode_id: Option<String>) {
        if self.selected_episode_id != episode_id {
            self.selected_episode_id = episode_id;
            self.selected_media_source_index = None;
            self.selected_subtitle_index = None;
            self.open_select = None;
            self.reset_playback_request();
        }
        self.sync_media_source_selection();
    }

    pub(crate) fn reset_episode_selection(&mut self) {
        self.episodes = None;
        self.episodes_failed = None;
        self.effects.episodes = LoadState::Idle;
        self.episodes_request_season_id = None;
        self.selected_episode_id = None;
        self.selected_media_source_index = None;
        self.selected_subtitle_index = None;
        self.open_select = None;
        self.episodes_carousel = Default::default();
        self.reset_playback_request();
    }

    pub(crate) fn reset_playback_request(&mut self) {
        self.playback_loading = false;
        self.playback_failed = None;
    }

    pub(crate) fn sync_media_source_selection(&mut self) {
        let source_count = self
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
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

#[cfg(test)]
mod tests {
    use crate::emby::{MediaItems, UserItem};

    use super::*;

    fn user_item(id: &str, name: &str) -> UserItem {
        UserItem {
            id: id.to_string(),
            name: name.to_string(),
            item_type: Some("Series".to_string()),
            production_year: None,
            community_rating: None,
            image_tags: None,
            backdrop_image_tags: None,
            user_data: None,
            collection_type: None,
            primary_image_aspect_ratio: None,
            child_count: None,
            container: None,
            can_delete: None,
            provider_ids: None,
        }
    }

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
            kind: SeriesDetailKind::Series,
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
            playback_loading: false,
            playback_failed: None,
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

    #[test]
    fn hero_episode_prefers_selected_episode_over_next_up() {
        let mut detail = SeriesDetailState::new_series(&user_item("series-1", "Series"));
        detail.next_up = Some(MediaItems {
            items: vec![media_item("episode-2", "第二集")],
            total_record_count: 2,
        });
        detail.episodes = Some(MediaItems {
            items: vec![
                media_item("episode-1", "第一集"),
                media_item("episode-2", "第二集"),
            ],
            total_record_count: 2,
        });
        detail.selected_episode_id = Some("episode-1".to_string());

        assert_eq!(
            detail.hero_episode().map(|episode| episode.id.as_str()),
            Some("episode-1")
        );
    }

    fn media_source(id: &str, name: &str) -> MediaSource {
        MediaSource {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            path: None,
            container: None,
            media_streams: None,
        }
    }

    #[test]
    fn movie_detail_uses_item_as_playback_source() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(vec![media_source("source-1", "4K HDR")]);
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert!(detail.is_movie());
        assert_eq!(detail.hero_line().as_deref(), Some("电影"));
        assert_eq!(detail.selected_media_source_label(), "4K HDR");
        assert_eq!(
            detail.selected_playback_item().map(|item| item.id.as_str()),
            Some("movie-1")
        );
    }

    #[test]
    fn from_user_item_accepts_series_and_movie_only() {
        let series = user_item("series-1", "剧集");
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut folder = user_item("folder-1", "合集");
        folder.item_type = Some("Folder".to_string());

        assert!(
            SeriesDetailState::from_user_item(&series).is_some_and(|detail| detail.is_series())
        );
        assert!(SeriesDetailState::from_user_item(&movie).is_some_and(|detail| detail.is_movie()));
        assert!(SeriesDetailState::from_user_item(&folder).is_none());
    }
}
