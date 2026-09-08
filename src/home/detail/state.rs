use gpui::{ScrollHandle, point, px};

use crate::emby::{MediaItem, MediaItems, MediaSource, ResumeItem, UserItem, UserItems};

use super::super::{LoadState, video_version::VideoVersion};

const EMBY_TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Clone, Debug, Default)]
pub(crate) struct SeriesDetailEffects {
    pub(crate) item: LoadState,
    pub(crate) seasons: LoadState,
    pub(crate) next_up: LoadState,
    pub(crate) episodes: LoadState,
    pub(crate) similar: LoadState,
    pub(crate) resume_sources: LoadState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeriesDetailSelectKind {
    Season,
    MediaSource,
    Subtitle,
    Actions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeriesDetailKind {
    Series,
    Movie,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeriesDetailOrigin {
    UserView,
    Resume,
}

#[derive(Clone, Debug)]
pub(crate) struct SeriesDetailState {
    kind: SeriesDetailKind,
    origin: SeriesDetailOrigin,
    pub(crate) series_id: String,
    pub(crate) title: String,
    pub(crate) effects: SeriesDetailEffects,
    pub(crate) item: Option<MediaItem>,
    pub(crate) item_failed: Option<gpui::SharedString>,
    pub(crate) seasons: Option<MediaItems>,
    pub(crate) seasons_failed: Option<gpui::SharedString>,
    pub(crate) next_up: Option<MediaItems>,
    pub(crate) next_up_failed: Option<gpui::SharedString>,
    pub(super) resume_episode: Option<ResumeItem>,
    // Keep the original playable item separate from its series/episode group.
    resume_media_item_id: Option<String>,
    pub(crate) resume_media_sources: Option<Vec<MediaSource>>,
    pub(crate) resume_video_version: Option<VideoVersion>,
    pub(crate) episodes: Option<MediaItems>,
    pub(crate) episodes_failed: Option<gpui::SharedString>,
    pub(crate) episode_selection_warning: Option<gpui::SharedString>,
    pub(crate) similar_items: Option<UserItems>,
    pub(crate) similar_failed: Option<gpui::SharedString>,
    pub(crate) playback_loading: bool,
    pub(crate) playback_failed: Option<gpui::SharedString>,
    pub(crate) selected_season_id: Option<String>,
    pub(crate) selected_episode_id: Option<String>,
    pub(crate) preferred_episode_id: Option<String>,
    preferred_season_id_hint: Option<String>,
    pub(crate) selected_media_source_index: Option<usize>,
    manual_video_version: Option<VideoVersion>,
    /// Explicit selection only; automatic choices follow the current language preference.
    pub(crate) selected_subtitle_index: Option<usize>,
    pub(crate) open_select: Option<SeriesDetailSelectKind>,
    pub(crate) action_menu_focus: Option<gpui::FocusHandle>,
    pub(crate) scroll_handle: ScrollHandle,
    pub(crate) season_scroll_handle: ScrollHandle,
    pub(crate) media_source_scroll_handle: ScrollHandle,
    pub(crate) subtitle_scroll_handle: ScrollHandle,
    pub(crate) episodes_request_season_id: Option<String>,
    pub(crate) episodes_carousel: super::super::carousel::CarouselState,
    pub(crate) people_carousel: super::super::carousel::CarouselState,
    pub(crate) similar_carousel: super::super::carousel::CarouselState,
}

impl SeriesDetailState {
    pub(crate) fn from_user_item(item: &UserItem) -> Option<Self> {
        match item.item_type.as_deref() {
            Some("Series") => Some(Self::new_series(item)),
            Some("Movie") => Some(Self::new_movie(item)),
            Some("Episode") => Self::from_user_episode(item),
            _ => None,
        }
    }

    fn from_user_episode(item: &UserItem) -> Option<Self> {
        let series_id = item
            .series_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())?
            .to_string();
        let episode_id = item.id.trim();
        if episode_id.is_empty() {
            return None;
        }
        let title = item
            .series_name
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&item.name)
            .to_string();
        let resume = ResumeItem {
            id: item.id.clone(),
            name: item.name.clone(),
            item_type: Some("Episode".to_string()),
            parent_id: item.parent_id.clone(),
            series_name: item.series_name.clone(),
            series_id: Some(series_id.clone()),
            parent_index_number: item.parent_index_number,
            index_number: item.index_number,
            production_year: item.production_year,
            image_tags: item.image_tags.clone(),
            backdrop_image_tags: item.backdrop_image_tags.clone(),
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
            user_data: item.user_data.clone(),
        };
        let mut detail = Self::new_with_identity(
            series_id,
            title,
            SeriesDetailKind::Series,
            SeriesDetailOrigin::Resume,
        );
        detail.selected_episode_id = Some(episode_id.to_string());
        detail.preferred_episode_id = Some(episode_id.to_string());
        detail.preferred_season_id_hint = item
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToString::to_string);
        detail.resume_episode = Some(resume);
        Some(detail)
    }

    pub(crate) fn new_series(item: &UserItem) -> Self {
        Self::new(item, SeriesDetailKind::Series, SeriesDetailOrigin::UserView)
    }

    pub(crate) fn new_movie(item: &UserItem) -> Self {
        Self::new(item, SeriesDetailKind::Movie, SeriesDetailOrigin::UserView)
    }

    pub(crate) fn from_resume_movie(item: &ResumeItem) -> Option<Self> {
        if item.item_type.as_deref() != Some("Movie") {
            return None;
        }

        let mut detail = Self::new_with_identity(
            item.id.clone(),
            item.name.clone(),
            SeriesDetailKind::Movie,
            SeriesDetailOrigin::Resume,
        );
        detail.resume_media_item_id = Some(item.id.clone());
        Some(detail)
    }

    pub(crate) fn from_resume_episode(item: &ResumeItem) -> Option<Self> {
        if item.item_type.as_deref() != Some("Episode") {
            return None;
        }

        let series_id = item.series_id.as_deref()?.trim();
        if series_id.is_empty() {
            return None;
        }
        let series_id = series_id.to_string();
        let episode_id = item.id.trim();
        if episode_id.is_empty() {
            return None;
        }
        let episode_id = episode_id.to_string();
        let title = item
            .series_name
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&item.name)
            .to_string();
        let mut detail = Self::new_with_identity(
            series_id,
            title,
            SeriesDetailKind::Series,
            SeriesDetailOrigin::Resume,
        );
        detail.selected_episode_id = Some(episode_id.clone());
        detail.resume_media_item_id = Some(episode_id.clone());
        detail.preferred_episode_id = Some(episode_id);
        detail.preferred_season_id_hint = item
            .parent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToString::to_string);
        detail.resume_episode = Some(item.clone());
        Some(detail)
    }

    fn new(item: &UserItem, kind: SeriesDetailKind, origin: SeriesDetailOrigin) -> Self {
        Self::new_with_identity(item.id.clone(), item.name.clone(), kind, origin)
    }

    fn new_with_identity(
        series_id: String,
        title: String,
        kind: SeriesDetailKind,
        origin: SeriesDetailOrigin,
    ) -> Self {
        Self {
            kind,
            origin,
            series_id,
            title,
            effects: Default::default(),
            item: None,
            item_failed: None,
            seasons: None,
            seasons_failed: None,
            next_up: None,
            next_up_failed: None,
            resume_episode: None,
            resume_media_item_id: None,
            resume_media_sources: None,
            resume_video_version: None,
            episodes: None,
            episodes_failed: None,
            episode_selection_warning: None,
            similar_items: None,
            similar_failed: None,
            playback_loading: false,
            playback_failed: None,
            selected_season_id: None,
            selected_episode_id: None,
            preferred_episode_id: None,
            preferred_season_id_hint: None,
            selected_media_source_index: None,
            manual_video_version: None,
            selected_subtitle_index: None,
            open_select: None,
            action_menu_focus: None,
            scroll_handle: ScrollHandle::new(),
            season_scroll_handle: ScrollHandle::new(),
            media_source_scroll_handle: ScrollHandle::new(),
            subtitle_scroll_handle: ScrollHandle::new(),
            episodes_request_season_id: None,
            episodes_carousel: Default::default(),
            people_carousel: Default::default(),
            similar_carousel: Default::default(),
        }
    }

    pub(crate) fn is_series(&self) -> bool {
        self.kind == SeriesDetailKind::Series
    }

    pub(crate) fn is_movie(&self) -> bool {
        self.kind == SeriesDetailKind::Movie
    }

    pub(crate) fn should_load_next_up(&self) -> bool {
        self.is_series() && self.origin == SeriesDetailOrigin::UserView
    }

    pub(crate) fn opened_from_resume(&self) -> bool {
        self.origin == SeriesDetailOrigin::Resume
    }

    pub(crate) fn should_reveal_selected_episode(&self) -> bool {
        let Some(selected_episode_id) = self.selected_episode_id.as_deref() else {
            return false;
        };

        if self.opened_from_resume() {
            return self
                .preferred_episode()
                .is_some_and(|episode| episode.id == selected_episode_id);
        }

        self.next_up_episode()
            .is_some_and(|episode| episode.id == selected_episode_id)
    }

    pub(crate) fn next_up_episode(&self) -> Option<&MediaItem> {
        self.next_up.as_ref().and_then(|items| items.items.first())
    }

    pub(crate) fn hero_episode(&self) -> Option<&MediaItem> {
        self.selected_episode().or_else(|| self.next_up_episode())
    }

    pub(crate) fn hero_line(&self) -> Option<String> {
        if self.is_movie() {
            None
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
        self.preferred_season_id_hint
            .as_ref()
            .filter(|preferred_id| {
                self.seasons.as_ref().is_some_and(|seasons| {
                    seasons
                        .items
                        .iter()
                        .any(|season| season.id == **preferred_id)
                })
            })
            .cloned()
            .or_else(|| {
                self.resume_episode_for_preferred()
                    .and_then(|episode| episode.parent_index_number)
                    .and_then(|season_number| {
                        self.seasons.as_ref().and_then(|seasons| {
                            seasons
                                .items
                                .iter()
                                .find(|season| season.index_number == Some(season_number))
                                .map(|season| season.id.clone())
                        })
                    })
            })
            .or_else(|| {
                self.next_up_episode()
                    .and_then(|episode| episode.season_id.clone())
            })
    }

    fn resume_episode_for_preferred(&self) -> Option<&ResumeItem> {
        let preferred_episode_id = self.preferred_episode_id.as_deref()?;
        self.resume_episode
            .as_ref()
            .filter(|episode| episode.id == preferred_episode_id)
    }

    fn preferred_episode(&self) -> Option<&MediaItem> {
        let preferred_id = self.preferred_episode_id.as_deref()?;
        let episodes = &self.episodes.as_ref()?.items;
        episodes
            .iter()
            .find(|episode| episode.id == preferred_id)
            .or_else(|| {
                // Alternate versions may be grouped under another episode ID.
                // Resolve that group only for the item opened from Continue Watching.
                if self.resume_media_item_id.as_deref() != Some(preferred_id) {
                    return None;
                }
                episodes.iter().find(|episode| {
                    episode.media_sources.as_ref().is_some_and(|sources| {
                        sources
                            .iter()
                            .any(|source| source.matches_item_id(preferred_id))
                    })
                })
            })
    }

    pub(crate) fn playback_position_seconds(&self) -> Option<u64> {
        Some(self.playback_position_ticks()? / EMBY_TICKS_PER_SECOND)
    }

    pub(crate) fn playback_position_ticks(&self) -> Option<u64> {
        let selected = self.selected_playback_item()?;
        let selected_id = selected.id.as_str();
        self.resume_episode
            .as_ref()
            .filter(|episode| {
                episode.id == selected_id
                    || self
                        .selected_media_source()
                        .is_some_and(|source| source.matches_item_id(&episode.id))
            })
            .and_then(|episode| episode.user_data.as_ref())
            .and_then(|data| data.playback_position_ticks)
            .filter(|ticks| *ticks > 0)
            .or_else(|| {
                self.next_up_episode()
                    .filter(|episode| episode.id == selected_id)
                    .and_then(MediaItem::playback_position_ticks)
            })
            .or_else(|| selected.playback_position_ticks())
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

    pub(crate) fn selected_episode_index(&self) -> Option<usize> {
        let selected_episode_id = self.selected_episode_id.as_deref()?;
        self.episodes
            .as_ref()?
            .items
            .iter()
            .position(|episode| episode.id == selected_episode_id)
    }

    pub(crate) fn playback_user_data(&self, item_id: &str) -> Option<&crate::emby::UserItemData> {
        self.item
            .as_ref()
            .filter(|item| item.id == item_id)
            .and_then(|item| item.user_data.as_ref())
            .or_else(|| {
                self.episodes
                    .as_ref()
                    .and_then(|items| items.items.iter().find(|item| item.id == item_id))
                    .and_then(|item| item.user_data.as_ref())
            })
            .or_else(|| {
                self.next_up
                    .as_ref()
                    .and_then(|items| items.items.iter().find(|item| item.id == item_id))
                    .and_then(|item| item.user_data.as_ref())
            })
            .or_else(|| {
                self.resume_episode
                    .as_ref()
                    .filter(|item| item.id == item_id)
                    .and_then(|item| item.user_data.as_ref())
            })
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: &crate::player::PlaybackStateUpdate,
        user_data: &crate::emby::UserItemData,
    ) {
        for item_id in [&update.item_id, &update.list_item_id] {
            if let Some(item) = self.item.as_mut().filter(|item| item.id == *item_id) {
                item.user_data = Some(user_data.clone());
            }
            if let Some(items) = self.episodes.as_mut() {
                set_media_item_user_data(&mut items.items, item_id, user_data);
            }
            if let Some(items) = self.next_up.as_mut() {
                set_media_item_user_data(&mut items.items, item_id, user_data);
            }
            if let Some(item) = self
                .resume_episode
                .as_mut()
                .filter(|item| item.id == *item_id)
            {
                item.user_data = Some(user_data.clone());
            }
        }

        let Some(selected_item_id) = update.selected_item_id.as_ref() else {
            return;
        };
        if update.series_id.as_deref() != Some(self.series_id.as_str())
            || update.season_id.as_deref() != self.selected_season_id.as_deref()
            || self.episodes.as_ref().is_none_or(|episodes| {
                !episodes
                    .items
                    .iter()
                    .any(|episode| episode.id == *selected_item_id)
            })
        {
            return;
        }

        self.preferred_episode_id = Some(selected_item_id.clone());
        self.apply_selected_episode(Some(selected_item_id.clone()));
        if let Some(index) = self.selected_episode_index() {
            self.episodes_carousel.set_scroll_offset(
                index as f32 * super::super::carousel::DETAIL_EPISODE_CARD_STEP_PX,
                f32::INFINITY,
            );
            self.episodes_carousel.sync_previous_offset();
        }
    }

    pub(crate) fn mark_episodes_for_playback_refresh(&mut self) {
        if self.effects.episodes == LoadState::Loading {
            return;
        }
        self.effects.episodes = LoadState::Idle;
        self.episodes_request_season_id = None;
        self.episodes_failed = None;
    }

    pub(crate) fn mark_item_for_playback_refresh(&mut self) {
        if self.effects.item == LoadState::Loading {
            return;
        }
        self.effects.item = LoadState::Idle;
        self.item_failed = None;
    }

    pub(crate) fn selected_media_source(&self) -> Option<&MediaSource> {
        let sources = self.selected_media_sources()?;
        let index = self.selected_media_source_index()?;
        sources.get(index)
    }

    pub(crate) fn selected_media_source_index(&self) -> Option<usize> {
        let sources = self.selected_media_sources()?;
        self.manual_video_version
            .as_ref()
            .and_then(|version| version.find_source(sources))
            .or_else(|| {
                if self.is_resume_playback_item_selected() {
                    // Like Tsukimi, restore the version matcher or use the first
                    // PlaybackInfo source. Resume.Id identifies an item, not its
                    // previously selected version on servers that group versions.
                    self.resume_video_version
                        .as_ref()
                        .and_then(|version| version.find_source(sources))
                        .or_else(|| (!sources.is_empty()).then_some(0))
                } else {
                    preferred_media_source_index(sources)
                }
            })
    }

    pub(crate) fn resume_media_item_id(&self) -> Option<&str> {
        self.resume_media_item_id.as_deref()
    }

    fn is_resume_playback_item_selected(&self) -> bool {
        let Some(resume_id) = self.resume_media_item_id.as_deref() else {
            return false;
        };
        self.selected_playback_item().is_some_and(|item| {
            self.is_movie()
                || item.id == resume_id
                || item.media_sources.as_ref().is_some_and(|sources| {
                    sources
                        .iter()
                        .any(|source| source.matches_item_id(resume_id))
                })
        })
    }

    pub(crate) fn selected_media_sources(&self) -> Option<&[MediaSource]> {
        if self.is_resume_playback_item_selected()
            && let Some(sources) = self.resume_media_sources.as_deref()
        {
            return Some(sources);
        }
        self.selected_playback_item()?.media_sources.as_deref()
    }

    pub(crate) fn video_sources_loading(&self) -> bool {
        self.is_resume_playback_item_selected() && self.effects.resume_sources == LoadState::Loading
    }

    pub(crate) fn select_media_source(&mut self, index: usize) {
        let Some(source) = self
            .selected_media_sources()
            .and_then(|sources| sources.get(index))
        else {
            return;
        };
        self.manual_video_version = Some(VideoVersion::from_source(source));
        self.selected_media_source_index = Some(index);
        self.selected_subtitle_index = None;
        self.subtitle_scroll_handle
            .set_offset(point(px(0.0), px(0.0)));
        self.open_select = None;
        self.reset_playback_request();
        self.sync_media_source_selection();
    }

    pub(crate) fn selected_subtitle_index(
        &self,
        language: crate::player::TrackLanguage,
    ) -> Option<usize> {
        let source = self.selected_media_source()?;
        let subtitle_count = source.subtitle_streams().len();
        if subtitle_count == 0 {
            return None;
        }

        self.selected_subtitle_index
            .filter(|index| *index < subtitle_count)
            .or_else(|| language.preferred_subtitle_stream_position(source))
    }

    pub(crate) fn selected_media_source_label(&self) -> String {
        if self.video_sources_loading() {
            return "正在加载视频源…".to_string();
        }
        let Some(sources) = self.selected_media_sources() else {
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

    pub(crate) fn selected_subtitle_label(&self, language: crate::player::TrackLanguage) -> String {
        let Some(source) = self.selected_media_source() else {
            return "无字幕".to_string();
        };
        let subtitles = source.subtitle_streams();
        let Some(index) = self.selected_subtitle_index(language) else {
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
        let preferred = self.preferred_episode().map(|episode| episode.id.clone());
        let preferred_missing = self.preferred_episode_id.is_some() && preferred.is_none();
        self.episode_selection_warning =
            preferred_missing.then(|| "原单集已不可用，已选择当前可播放单集".into());

        let selected =
            preferred.or_else(|| self.selected_episode().map(|episode| episode.id.clone()));

        self.apply_selected_episode(selected);
    }

    pub(crate) fn apply_selected_episode(&mut self, episode_id: Option<String>) {
        if self.selected_episode_id != episode_id {
            self.selected_episode_id = episode_id;
            self.selected_media_source_index = None;
            self.manual_video_version = None;
            self.selected_subtitle_index = None;
            self.open_select = None;
            self.reset_select_scroll_offsets();
            self.reset_playback_request();
        }
        self.sync_media_source_selection();
    }

    pub(crate) fn reset_episode_selection(&mut self) {
        self.episodes = None;
        self.episodes_failed = None;
        self.episode_selection_warning = None;
        self.effects.episodes = LoadState::Idle;
        self.episodes_request_season_id = None;
        self.selected_episode_id = None;
        self.selected_media_source_index = None;
        self.manual_video_version = None;
        self.selected_subtitle_index = None;
        self.open_select = None;
        self.reset_select_scroll_offsets();
        self.episodes_carousel = Default::default();
        self.reset_playback_request();
    }

    pub(crate) fn clear_preferred_season_hint(&mut self) {
        self.preferred_season_id_hint = None;
    }

    pub(crate) fn reset_playback_request(&mut self) {
        self.playback_loading = false;
        self.playback_failed = None;
    }

    pub(crate) fn reset_in_flight_effects(&mut self) {
        for state in [
            &mut self.effects.item,
            &mut self.effects.seasons,
            &mut self.effects.next_up,
            &mut self.effects.episodes,
            &mut self.effects.similar,
            &mut self.effects.resume_sources,
        ] {
            if *state == LoadState::Loading {
                *state = LoadState::Idle;
            }
        }
        if self.effects.episodes == LoadState::Idle {
            self.episodes_request_season_id = None;
        }
        self.playback_loading = false;
    }

    fn reset_select_scroll_offsets(&self) {
        let origin = point(px(0.0), px(0.0));
        self.media_source_scroll_handle.set_offset(origin);
        self.subtitle_scroll_handle.set_offset(origin);
    }

    pub(crate) fn sync_media_source_selection(&mut self) {
        let selected_media_source_index = self.selected_media_source_index();
        if selected_media_source_index.is_none() {
            self.selected_media_source_index = None;
            self.selected_subtitle_index = None;
            self.open_select = None;
            return;
        }
        if self.selected_media_source_index != selected_media_source_index
            && self.manual_video_version.is_none()
        {
            self.selected_subtitle_index = None;
        }
        self.selected_media_source_index = selected_media_source_index;
        if let Some(resume_item_id) = self.resume_media_item_id.as_deref() {
            let source = self.selected_media_source();
            tracing::debug!(
                resume_item_id,
                selected_episode_id = ?self.selected_episode_id,
                media_source_id = ?source.and_then(|source| source.id.as_deref()),
                media_source_item_id = ?source.and_then(|source| source.item_id.as_deref()),
                manual = self.manual_video_version.is_some(),
                "resolved resume detail video selection"
            );
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
            .is_some_and(|index| index >= subtitle_count)
        {
            self.selected_subtitle_index = None;
        }
    }
}

fn set_media_item_user_data(
    items: &mut [MediaItem],
    item_id: &str,
    user_data: &crate::emby::UserItemData,
) {
    if let Some(item) = items.iter_mut().find(|item| item.id == item_id) {
        item.user_data = Some(user_data.clone());
    }
}

fn preferred_media_source_index(sources: &[MediaSource]) -> Option<usize> {
    if sources.is_empty() {
        return None;
    }

    sources
        .iter()
        .position(MediaSource::is_default_source)
        .or_else(|| {
            sources
                .iter()
                .position(MediaSource::has_default_video_stream)
        })
        .or(Some(0))
}

#[cfg(test)]
mod tests {
    use crate::emby::{MediaItems, ResumeItem, UserItem, UserItemData};

    use super::*;

    fn user_item(id: &str, name: &str) -> UserItem {
        UserItem {
            id: id.to_string(),
            name: name.to_string(),
            item_type: Some("Series".to_string()),
            media_type: None,
            parent_id: None,
            series_id: None,
            series_name: None,
            index_number: None,
            parent_index_number: None,
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
            people: None,
            studios: None,
            external_urls: None,
            user_data: None,
        }
    }

    fn resume_episode(id: &str, name: &str, series_id: &str, season_number: u32) -> ResumeItem {
        ResumeItem {
            id: id.to_string(),
            name: name.to_string(),
            item_type: Some("Episode".to_string()),
            parent_id: None,
            series_name: Some("示例剧集".to_string()),
            series_id: Some(series_id.to_string()),
            parent_index_number: Some(season_number),
            index_number: None,
            production_year: None,
            image_tags: None,
            backdrop_image_tags: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
            user_data: None,
        }
    }

    #[test]
    fn chooses_next_up_episode_when_loaded_episodes_include_it() {
        let mut detail = SeriesDetailState {
            kind: SeriesDetailKind::Series,
            origin: SeriesDetailOrigin::UserView,
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
            resume_episode: None,
            resume_media_item_id: None,
            resume_media_sources: None,
            resume_video_version: None,
            episodes: Some(MediaItems {
                items: vec![
                    media_item("episode-1", "第一集"),
                    media_item("episode-2", "第二集"),
                ],
                total_record_count: 2,
            }),
            episodes_failed: None,
            episode_selection_warning: None,
            similar_items: None,
            similar_failed: None,
            playback_loading: false,
            playback_failed: None,
            selected_season_id: None,
            selected_episode_id: None,
            preferred_episode_id: Some("episode-2".to_string()),
            preferred_season_id_hint: None,
            selected_media_source_index: None,
            manual_video_version: None,
            selected_subtitle_index: None,
            open_select: None,
            action_menu_focus: None,
            scroll_handle: ScrollHandle::new(),
            season_scroll_handle: ScrollHandle::new(),
            media_source_scroll_handle: ScrollHandle::new(),
            subtitle_scroll_handle: ScrollHandle::new(),
            episodes_request_season_id: None,
            episodes_carousel: Default::default(),
            people_carousel: Default::default(),
            similar_carousel: Default::default(),
        };

        detail.choose_episode_from_loaded_episodes();

        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
        assert!(detail.should_reveal_selected_episode());
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
        assert!(!detail.should_reveal_selected_episode());
    }

    fn media_source(id: &str, name: &str) -> MediaSource {
        MediaSource {
            id: Some(id.to_string()),
            item_id: None,
            name: Some(name.to_string()),
            path: None,
            source_type: None,
            container: None,
            media_streams: None,
            default_subtitle_stream_index: None,
        }
    }

    #[test]
    fn media_selection_uses_default_source_and_default_subtitle_stream_index() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {
                    "Id": "source-1",
                    "ItemId": "movie-1",
                    "Type": "Grouping",
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": false }
                    ]
                },
                {
                    "Id": "source-2",
                    "Type": "Default",
                    "DefaultSubtitleStreamIndex": 6,
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": true },
                        { "Index": 4, "Type": "Subtitle", "DisplayTitle": "英文", "IsDefault": true },
                        { "Index": 6, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": false }
                    ]
                }
            ]))
            .unwrap(),
        );
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert_eq!(detail.selected_media_source_index(), Some(1));
        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(1));
        assert_eq!(
            detail.selected_subtitle_label(Default::default()),
            "简体中文"
        );
    }

    #[test]
    fn resume_movie_restores_its_version_and_preserves_manual_selection() {
        let movie: ResumeItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_resume_movie(&movie).unwrap();
        detail.resume_video_version = Some(VideoVersion {
            source_id: "resumed-source".into(),
            name: Some("Resumed version".into()),
        });
        let mut item = media_item("movie-1", "Movie");
        item.media_sources = Some(serde_json::from_value(serde_json::json!([
            {"Id": "default-source", "Type": "Default"},
            {
                "Id": "resumed-source", "ItemId": "movie-1", "Name": "Resumed version",
                "MediaStreams": [
                    {"Type": "Subtitle", "Index": 3, "DisplayTitle": "简体中文", "IsDefault": true}
                ]
            }
        ])).unwrap());
        detail.item = Some(item);

        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(1));
        assert_eq!(detail.selected_media_source_label(), "Resumed version");
        assert_eq!(
            detail.selected_subtitle_label(Default::default()),
            "简体中文"
        );

        detail.select_media_source(0);
        assert_eq!(detail.selected_media_source_index(), Some(0));
        detail
            .item
            .as_mut()
            .unwrap()
            .media_sources
            .as_mut()
            .unwrap()
            .swap(0, 1);
        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(1));
        assert_eq!(
            detail.selected_media_source().unwrap().id.as_deref(),
            Some("default-source")
        );
    }

    #[test]
    fn remembered_video_overrides_an_automatic_selection_after_reload() {
        let movie: ResumeItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_resume_movie(&movie).unwrap();
        detail.resume_video_version = Some(VideoVersion {
            source_id: "resumed-source".into(),
            name: None,
        });
        let mut item = media_item("movie-1", "Movie");
        item.media_sources = Some(vec![media_source("default-source", "Default")]);
        detail.item = Some(item);
        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(0));

        detail.item.as_mut().unwrap().media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {"Id": "default-source", "ItemId": "other-item", "Type": "Default"},
                {"Id": "resumed-source", "ItemId": "movie-1"}
            ]))
            .unwrap(),
        );
        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(1));
    }

    #[test]
    fn resume_uses_playback_info_order_instead_of_item_id_or_default_type() {
        let movie: ResumeItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_resume_movie(&movie).unwrap();
        let mut item = media_item("movie-1", "Movie");
        item.media_sources = Some(vec![media_source("metadata-source", "Metadata")]);
        detail.resume_media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {"Id": "source-2160p", "ItemId": "other-item", "Name": "2160p", "Type": "Grouping"},
                {"Id": "source-1080p", "ItemId": "movie-1", "Type": "Default"}
            ]))
            .unwrap(),
        );
        detail.item = Some(item);
        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(0));
        assert_eq!(detail.selected_media_source_label(), "2160p");
    }

    #[test]
    fn resume_movie_without_a_remembered_version_uses_the_first_source() {
        let movie: ResumeItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_resume_movie(&movie).unwrap();
        let mut item = media_item("movie-1", "Movie");
        let mut default = media_source("source-default", "Default");
        default.source_type = Some("Default".to_string());
        item.media_sources = Some(vec![media_source("other-version", "Other"), default]);
        detail.item = Some(item);
        detail.sync_media_source_selection();
        assert_eq!(detail.selected_media_source_index(), Some(0));
    }

    #[test]
    fn only_resume_episode_entry_restores_the_remembered_version() {
        let episode = resume_episode("episode-2", "Second", "series-1", 1);
        let user_episode: UserItem =
            serde_json::from_value(serde_json::to_value(&episode).unwrap()).unwrap();
        let resume_detail = SeriesDetailState::from_resume_episode(&episode).unwrap();
        let user_detail = SeriesDetailState::from_user_item(&user_episode).unwrap();
        for (mut detail, expected_index) in [(resume_detail, 1), (user_detail, 0)] {
            detail.resume_video_version = Some(VideoVersion {
                source_id: "mediasource_episode-2".into(),
                name: None,
            });
            let mut item = media_item("episode-2", "Second");
            let mut default = media_source("default-source", "Default");
            default.source_type = Some("Default".to_string());
            let mut resumed_source = media_source("mediasource_episode-2", "Resumed");
            resumed_source.item_id = Some("episode-2".to_string());
            item.media_sources = Some(vec![default.clone(), resumed_source]);
            let mut next = media_item("episode-3", "Third");
            next.media_sources = Some(vec![
                default,
                media_source("mediasource_episode-3", "Other"),
            ]);
            detail.episodes = Some(MediaItems {
                items: vec![item, next],
                total_record_count: 2,
            });
            detail.choose_episode_from_loaded_episodes();
            assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
            assert_eq!(detail.selected_media_source_index(), Some(expected_index));

            detail.preferred_episode_id = Some("episode-3".to_string());
            detail.apply_selected_episode(Some("episode-3".to_string()));
            assert_eq!(detail.selected_media_source_index(), Some(0));
        }
    }

    #[test]
    fn resume_episode_version_selects_its_group_and_keeps_resume_position() {
        let mut episode = resume_episode("version-2", "Second", "series-1", 1);
        episode.user_data = Some(UserItemData {
            playback_position_ticks: Some(9_050_000_000),
            ..Default::default()
        });
        let mut detail = SeriesDetailState::from_resume_episode(&episode).unwrap();
        let mut group = media_item("episode-2", "Second");
        group.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {"Id": "default-source", "Type": "Default"},
                {"Id": "resumed-source", "ItemId": "version-2"}
            ]))
            .unwrap(),
        );
        detail.resume_media_sources = Some(vec![group.media_sources.as_ref().unwrap()[1].clone()]);
        detail.episodes = Some(MediaItems {
            items: vec![media_item("episode-1", "First"), group],
            total_record_count: 2,
        });
        detail.choose_episode_from_loaded_episodes();
        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
        assert_eq!(detail.selected_media_source_index(), Some(0));
        assert!(detail.episode_selection_warning.is_none());
        assert!(detail.should_reveal_selected_episode());
        assert_eq!(detail.playback_position_seconds(), Some(905));
    }

    #[test]
    fn media_selection_falls_back_to_default_stream_flags() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {
                    "Id": "source-1",
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": false }
                    ]
                },
                {
                    "Id": "source-2",
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": true },
                        { "Index": 2, "Type": "Subtitle", "DisplayTitle": "英文强制", "IsDefault": false, "IsForced": true },
                        { "Index": 3, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": true }
                    ]
                }
            ]))
            .unwrap(),
        );
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert_eq!(detail.selected_media_source_index(), Some(1));
        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(1));
    }

    #[test]
    fn media_selection_uses_forced_subtitle_as_the_last_fallback() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {
                    "Id": "source-1",
                    "Type": "Default",
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": true },
                        { "Index": 2, "Type": "Subtitle", "DisplayTitle": "普通字幕", "IsDefault": false, "IsForced": false },
                        { "Index": 3, "Type": "Subtitle", "DisplayTitle": "强制字幕", "IsDefault": false, "IsForced": true }
                    ]
                }
            ]))
            .unwrap(),
        );
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(1));
        assert_eq!(
            detail.selected_subtitle_label(Default::default()),
            "强制字幕"
        );
    }

    #[test]
    fn media_selection_falls_back_to_first_available_subtitle() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {
                    "Id": "source-1",
                    "Type": "Default",
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": true },
                        { "Index": 2, "Type": "Subtitle", "DisplayTitle": "第一字幕", "IsDefault": false, "IsForced": false },
                        { "Index": 3, "Type": "Subtitle", "DisplayTitle": "第二字幕", "IsDefault": false, "IsForced": false }
                    ]
                }
            ]))
            .unwrap(),
        );
        detail.item = Some(item);

        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(0));
        assert_eq!(
            detail.selected_subtitle_label(Default::default()),
            "第一字幕"
        );

        detail.sync_media_source_selection();

        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(0));
        assert_eq!(detail.selected_subtitle_index, None);
    }

    #[test]
    fn negative_default_subtitle_index_continues_to_available_stream_fallbacks() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(
            serde_json::from_value(serde_json::json!([
                {
                    "Id": "source-1",
                    "Type": "Default",
                    "DefaultSubtitleStreamIndex": -1,
                    "MediaStreams": [
                        { "Index": 0, "Type": "Video", "IsDefault": true },
                        { "Index": 2, "Type": "Subtitle", "DisplayTitle": "普通字幕", "IsDefault": false, "IsForced": false },
                        { "Index": 3, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": true, "IsForced": false }
                    ]
                }
            ]))
            .unwrap(),
        );
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert_eq!(detail.selected_media_source_index(), Some(0));
        assert_eq!(detail.selected_subtitle_index(Default::default()), Some(1));
        assert_eq!(
            detail.selected_subtitle_label(Default::default()),
            "简体中文"
        );
    }

    #[test]
    fn movie_detail_uses_item_as_playback_source() {
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut detail = SeriesDetailState::new_movie(&movie);
        let mut item = media_item("movie-1", "电影");
        item.item_type = Some("Movie".to_string());
        item.media_sources = Some(vec![media_source("source-1", "4K HDR")]);
        item.user_data = Some(UserItemData {
            unplayed_item_count: None,
            played_percentage: Some(25.0),
            playback_position_ticks: Some(10_800_000_000),
            is_favorite: false,
            played: false,
        });
        detail.item = Some(item);

        detail.sync_media_source_selection();

        assert!(detail.is_movie());
        assert_eq!(detail.hero_line(), None);
        assert_eq!(detail.selected_media_source_label(), "4K HDR");
        assert_eq!(
            detail.selected_playback_item().map(|item| item.id.as_str()),
            Some("movie-1")
        );
        assert_eq!(detail.playback_position_seconds(), Some(1_080));
    }

    #[test]
    fn from_user_item_accepts_series_movie_and_routable_episode() {
        let series = user_item("series-1", "剧集");
        let mut movie = user_item("movie-1", "电影");
        movie.item_type = Some("Movie".to_string());
        let mut folder = user_item("folder-1", "合集");
        folder.item_type = Some("Folder".to_string());
        let episode: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "episode-1",
            "Name": "第一集",
            "Type": "Episode",
            "SeriesId": "series-1",
            "SeriesName": "剧集",
            "ParentIndexNumber": 1,
            "IndexNumber": 1
        }))
        .unwrap();

        assert!(
            SeriesDetailState::from_user_item(&series).is_some_and(|detail| detail.is_series())
        );
        assert!(SeriesDetailState::from_user_item(&movie).is_some_and(|detail| detail.is_movie()));
        assert!(
            SeriesDetailState::from_user_item(&episode).is_some_and(|detail| {
                detail.is_series() && detail.preferred_episode_id.as_deref() == Some("episode-1")
            })
        );
        assert!(SeriesDetailState::from_user_item(&folder).is_none());
    }

    #[test]
    fn episode_parent_id_selects_the_exact_season_without_an_index_number() {
        let episode: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "episode-2",
            "Name": "第二集",
            "Type": "Episode",
            "SeriesId": "series-1",
            "SeriesName": "剧集",
            "ParentId": "season-2",
            "IndexNumber": 2
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_user_item(&episode).unwrap();
        let mut season_one = media_item("season-1", "第一季");
        season_one.index_number = Some(1);
        let mut season_two = media_item("season-2", "第二季");
        season_two.index_number = Some(2);
        detail.seasons = Some(MediaItems {
            items: vec![season_one, season_two],
            total_record_count: 2,
        });

        detail.choose_season_if_needed();

        assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
    }

    #[test]
    fn resume_episode_parent_id_selects_the_exact_season_without_an_index_number() {
        let mut episode = resume_episode("episode-2", "第二集", "series-1", 2);
        episode.parent_id = Some("season-2".to_string());
        episode.parent_index_number = None;
        let mut detail = SeriesDetailState::from_resume_episode(&episode).unwrap();
        let mut season_one = media_item("season-1", "第一季");
        season_one.index_number = Some(1);
        let mut season_two = media_item("season-2", "第二季");
        season_two.index_number = Some(2);
        detail.seasons = Some(MediaItems {
            items: vec![season_one, season_two],
            total_record_count: 2,
        });

        detail.choose_season_if_needed();

        assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
    }

    #[test]
    fn deleted_preferred_episode_falls_back_and_sets_warning() {
        let episode = resume_episode("deleted-episode", "已删除", "series-1", 1);
        let mut detail = SeriesDetailState::from_resume_episode(&episode).unwrap();
        detail.episodes = Some(MediaItems {
            items: vec![media_item("episode-1", "第一集")],
            total_record_count: 1,
        });

        detail.choose_episode_from_loaded_episodes();

        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-1"));
        assert!(detail.episode_selection_warning.is_some());
    }

    #[test]
    fn resume_episode_entry_defers_media_loading_and_selects_current_episode() {
        let mut episode = resume_episode("episode-2", "第二集", "series-1", 2);
        episode.user_data = Some(UserItemData {
            unplayed_item_count: None,
            played_percentage: Some(50.0),
            playback_position_ticks: Some(9_050_000_000),
            is_favorite: false,
            played: false,
        });

        let mut detail = SeriesDetailState::from_resume_episode(&episode).expect("valid episode");
        let mut season_one = media_item("season-1", "第一季");
        season_one.index_number = Some(1);
        let mut season_two = media_item("season-2", "第二季");
        season_two.index_number = Some(2);
        detail.seasons = Some(MediaItems {
            items: vec![season_one, season_two],
            total_record_count: 2,
        });
        detail.choose_season_if_needed();
        detail.episodes = Some(MediaItems {
            items: vec![
                media_item("episode-1", "第一集"),
                media_item("episode-2", "第二集"),
            ],
            total_record_count: 2,
        });
        detail.choose_episode_from_loaded_episodes();

        assert!(detail.opened_from_resume());
        assert!(!detail.should_load_next_up());
        assert_eq!(detail.effects.item, LoadState::Idle);
        assert!(detail.item.is_none());
        assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
        assert_eq!(detail.selected_episode_index(), Some(1));
        assert!(detail.should_reveal_selected_episode());
        assert_eq!(detail.playback_position_seconds(), Some(905));
    }

    #[test]
    fn next_up_position_drives_user_view_resume_minutes() {
        let mut detail = SeriesDetailState::new_series(&user_item("series-1", "示例剧集"));
        let mut next_up = media_item("episode-2", "第二集");
        next_up.user_data = Some(UserItemData {
            unplayed_item_count: None,
            played_percentage: None,
            playback_position_ticks: Some(12_000_000_000),
            is_favorite: false,
            played: false,
        });
        detail.next_up = Some(MediaItems {
            items: vec![next_up],
            total_record_count: 1,
        });
        detail.episodes = Some(MediaItems {
            items: vec![media_item("episode-2", "第二集")],
            total_record_count: 1,
        });
        detail.selected_episode_id = Some("episode-2".to_string());

        assert!(detail.should_load_next_up());
        assert_eq!(detail.playback_position_seconds(), Some(1_200));
    }

    #[test]
    fn playback_update_selects_replacement_episode_and_updates_closed_item() {
        let mut detail = SeriesDetailState::new_series(&user_item("series-1", "Series"));
        detail.selected_season_id = Some("season-1".to_string());
        detail.episodes = Some(MediaItems {
            items: vec![
                media_item("episode-1", "First"),
                media_item("episode-2", "Second"),
            ],
            total_record_count: 2,
        });
        detail.selected_episode_id = Some("episode-1".to_string());
        let update = crate::player::PlaybackStateUpdate {
            item_id: "episode-1-2160p".to_string(),
            list_item_id: "episode-1".to_string(),
            media_source_id: "source-2160p".to_string(),
            media_source_name: None,
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            position_ticks: 250,
            run_time_ticks: Some(1_000),
            ended: false,
            failed: false,
            selected_item_id: Some("episode-2".to_string()),
            stop_completion: None,
        };
        let user_data = UserItemData {
            playback_position_ticks: Some(250),
            played_percentage: Some(25.0),
            ..UserItemData::default()
        };

        detail.apply_playback_update(&update, &user_data);

        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
        assert_eq!(
            detail
                .episodes
                .as_ref()
                .and_then(|episodes| episodes.items.first())
                .and_then(|episode| episode.playback_position_ticks()),
            Some(250)
        );
    }
}
