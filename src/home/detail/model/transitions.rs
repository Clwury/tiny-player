//! Detail transitions rules; state remains owned by SeriesDetailModel.
use super::*;

impl SeriesDetailModel {
    pub(crate) fn choose_season_if_needed(&mut self) -> DetailChange {
        let current_valid = self.selected_season_id.as_deref().is_some_and(|season_id| {
            self.seasons
                .as_ref()
                .is_none_or(|seasons| seasons.items.iter().any(|season| season.id == season_id))
        });
        if current_valid {
            return DetailChange::default();
        }
        let season_id = self
            .preferred_season_id()
            .or_else(|| self.fallback_season_id());
        if self.selected_season_id == season_id {
            return DetailChange::default();
        }
        self.selected_season_id = season_id;
        self.reset_episode_selection()
    }

    pub(crate) fn apply_next_up_preference(&mut self) -> DetailChange {
        let Some((next_up_episode_id, next_up_season_id)) = self
            .next_up_episode()
            .map(|episode| (episode.id.clone(), episode.season_id.clone()))
        else {
            return self.choose_season_if_needed();
        };
        self.preferred_episode_id = Some(next_up_episode_id);
        if next_up_season_id.is_some() && self.selected_season_id != next_up_season_id {
            self.selected_season_id = next_up_season_id;
            return self.reset_episode_selection();
        }
        self.choose_season_if_needed()
    }

    pub(crate) fn choose_episode_from_loaded_episodes(&mut self) -> DetailChange {
        let preferred = self.preferred_episode().map(|episode| episode.id.clone());
        let preferred_missing = self.preferred_episode_id.is_some() && preferred.is_none();
        self.episode_selection_warning =
            preferred_missing.then(|| "原单集已不可用，已选择当前可播放单集".into());
        let selected =
            preferred.or_else(|| self.selected_episode().map(|episode| episode.id.clone()));
        self.apply_selected_episode(selected)
    }

    pub(crate) fn apply_selected_episode(&mut self, episode_id: Option<String>) -> DetailChange {
        let episode_changed = self.selected_episode_id != episode_id;
        if episode_changed {
            self.selected_episode_id = episode_id;
            self.selected_media_source_index = None;
            self.manual_video_version = None;
            self.reset_playback_request();
        }
        let mut change = self.sync_media_source_selection();
        change.episode_changed = episode_changed;
        change
    }

    pub(crate) fn reset_episode_selection(&mut self) -> DetailChange {
        self.episodes = None;
        self.episodes_failed = None;
        self.episode_selection_warning = None;
        self.effects.episodes = LoadState::Idle;
        self.episodes_request_season_id = None;
        self.selected_episode_id = None;
        self.selected_media_source_index = None;
        self.manual_video_version = None;
        self.reset_playback_request();
        DetailChange {
            episodes_reset: true,
            ..Default::default()
        }
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

    pub(crate) fn sync_media_source_selection(&mut self) -> DetailChange {
        let selected_media_source_index = self.selected_media_source_index();
        self.selected_media_source_index = selected_media_source_index;
        if selected_media_source_index.is_none() {
            return DetailChange {
                selection_unavailable: true,
                ..Default::default()
            };
        }
        let subtitle_count = self
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        DetailChange {
            subtitles_unavailable: subtitle_count == 0,
            ..Default::default()
        }
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: &crate::player::PlaybackStateUpdate,
        user_data: &crate::emby::UserItemData,
    ) -> DetailChange {
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
            return DetailChange::default();
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
            return DetailChange::default();
        }

        self.preferred_episode_id = Some(selected_item_id.clone());
        let mut change = self.apply_selected_episode(Some(selected_item_id.clone()));
        change.reveal_episode = self.selected_episode_index();
        change
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
