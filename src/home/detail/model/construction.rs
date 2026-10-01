//! Detail construction rules; state remains owned by SeriesDetailModel.
use super::*;

impl SeriesDetailModel {
    pub(crate) fn from_user_item(item: &UserItem) -> Option<Self> {
        match item.item_type.as_deref() {
            Some("Series") => Some(Self::new_series(item)),
            Some("Movie") => Some(Self::new_movie(item)),
            Some("Episode") => Self::from_user_episode(item),
            _ => None,
        }
    }

    pub(super) fn from_user_episode(item: &UserItem) -> Option<Self> {
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

    pub(super) fn new(item: &UserItem, kind: SeriesDetailKind, origin: SeriesDetailOrigin) -> Self {
        Self::new_with_identity(item.id.clone(), item.name.clone(), kind, origin)
    }

    pub(super) fn new_with_identity(
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
            pending_subtitle_choices: HashMap::new(),
            episodes_request_season_id: None,
        }
    }
}
