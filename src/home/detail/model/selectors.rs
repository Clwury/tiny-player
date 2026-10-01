//! Detail selectors rules; state remains owned by SeriesDetailModel.
use super::*;

impl SeriesDetailModel {
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

    pub(crate) fn hero_episode_index(&self) -> Option<usize> {
        let episode_id = &self.hero_episode()?.id;
        self.episodes
            .as_ref()?
            .items
            .iter()
            .position(|episode| &episode.id == episode_id)
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

    pub(super) fn preferred_season_id(&self) -> Option<String> {
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

    pub(super) fn resume_episode_for_preferred(&self) -> Option<&ResumeItem> {
        let preferred_episode_id = self.preferred_episode_id.as_deref()?;
        self.resume_episode
            .as_ref()
            .filter(|episode| episode.id == preferred_episode_id)
    }

    pub(super) fn preferred_episode(&self) -> Option<&MediaItem> {
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

    pub(super) fn fallback_season_id(&self) -> Option<String> {
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
}
