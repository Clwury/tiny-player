//! Detail data, selections and request acceptance. No executor or view resources.
use std::collections::HashMap;

/// Stable key for an entry's presentation resources. Async validity uses
/// RequestToken; this key survives hide/restore and never replaces that fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DetailId(u64);

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;

use crate::{
    effects::{DetailResource, RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{MediaItem, MediaItems, MediaSource, UserItems},
    home::model::{
        LoadState,
        detail::{DetailChange, SeriesDetailModel},
        user_data::{PendingUserData, UserDataState, apply_media_item_user_data_overrides},
    },
    player::{PlaybackTrack, PlaybackTrackExt, SavedTrackChoice},
};

/// One entry owns its data and independent request slots. Leaving the active
/// route invalidates every slot; restoring the entry retains completed data.
#[derive(Debug)]
pub(crate) struct DetailController {
    id: DetailId,
    pub(crate) state: SeriesDetailModel,
    requests: HashMap<DetailResource, RequestSlot>,
    activation_slot: RequestSlot,
    activation: Option<RequestToken>,
    playback: Option<RequestSlot>,
}

#[derive(Clone, Debug)]
pub(crate) struct DetailRequest {
    pub(crate) token: RequestToken,
    pub(crate) resource: DetailResource,
    pub(crate) item_id: String,
    pub(crate) season_id: Option<String>,
    user_data_revision: u64,
}

pub(crate) enum DetailResponse {
    Item(Box<MediaItem>),
    Similar(UserItems),
    Seasons(MediaItems),
    NextUp(MediaItems),
    Episodes(MediaItems),
    ResumeSources(Vec<MediaSource>),
}

impl DetailResponse {
    fn resource(&self) -> DetailResource {
        match self {
            Self::Item(_) => DetailResource::Item,
            Self::Similar(_) => DetailResource::Similar,
            Self::Seasons(_) => DetailResource::Seasons,
            Self::NextUp(_) => DetailResource::NextUp,
            Self::Episodes(_) => DetailResource::Episodes,
            Self::ResumeSources(_) => DetailResource::ResumeSources,
        }
    }
}

#[derive(Default)]
pub(crate) struct DetailUpdate {
    pub(crate) change: DetailChange,
    pub(crate) title_changed: bool,
    pub(crate) load_episodes: bool,
    pub(crate) error: Option<String>,
    pub(crate) images: bool,
    pub(crate) retry: bool,
    pub(crate) playback_cancelled: bool,
}

pub(crate) enum DetailIntent {
    Season(String),
    Episode(String),
    MediaSource(usize),
    Subtitle(Option<usize>),
}

impl DetailController {
    pub(crate) fn new(state: SeriesDetailModel, identity: WorkspaceIdentity) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let mut activation_slot = RequestSlot::new(
            RequestScope::DetailActivation {
                item_id: state.series_id.clone(),
            },
            identity,
        );
        let activation = Some(activation_slot.issue());
        Self {
            id: DetailId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            state,
            requests: HashMap::new(),
            activation_slot,
            activation,
            playback: None,
        }
    }

    pub(crate) fn view_model(&self) -> &SeriesDetailModel {
        &self.state
    }

    pub(crate) fn id(&self) -> DetailId {
        self.id
    }

    pub(crate) fn from_user_item(
        item: &crate::emby::UserItem,
        identity: WorkspaceIdentity,
    ) -> Option<Self> {
        SeriesDetailModel::from_user_item(item).map(|model| Self::new(model, identity))
    }
    pub(crate) fn from_resume_movie(
        item: &crate::emby::ResumeItem,
        identity: WorkspaceIdentity,
    ) -> Option<Self> {
        SeriesDetailModel::from_resume_movie(item).map(|model| Self::new(model, identity))
    }
    pub(crate) fn from_resume_episode(
        item: &crate::emby::ResumeItem,
        identity: WorkspaceIdentity,
    ) -> Option<Self> {
        SeriesDetailModel::from_resume_episode(item).map(|model| Self::new(model, identity))
    }

    pub(crate) fn deactivate(&mut self) {
        self.requests.clear();
        self.playback = None;
        self.activation = None;
        self.activation_slot.invalidate();
        self.state.reset_in_flight_effects();
    }

    pub(crate) fn activate(&mut self) {
        if self.activation.is_none() {
            self.activation = Some(self.activation_slot.issue());
        }
    }

    pub(crate) fn activation(&self) -> Option<&RequestToken> {
        self.activation.as_ref()
    }

    fn cancel_reset_playback(&mut self) -> bool {
        if !self.state.playback_loading {
            return self.playback.take().is_some();
        }
        false
    }

    pub(crate) fn begin_playback(
        &mut self,
        selected: Result<super::playback::SelectedPlayback, String>,
        identity: WorkspaceIdentity,
    ) -> Result<Option<super::playback::DetailPlaybackCommand>, String> {
        if self.activation.is_none()
            || self.state.playback_loading
            || self.state.video_sources_loading()
        {
            return Ok(None);
        }
        let selected = match selected {
            Ok(selected) => selected,
            Err(error) => {
                self.state.playback_failed = Some(error.clone());
                return Err(error);
            }
        };
        if !self.playback_selection_is_current(&selected) {
            return Ok(None);
        }
        let mut slot = RequestSlot::new(
            RequestScope::DetailPlayback {
                item_id: selected.item_id.clone(),
            },
            identity,
        );
        let token = slot.issue();
        self.playback = Some(slot);
        self.state.playback_loading = true;
        self.state.playback_failed = None;
        Ok(Some(super::playback::DetailPlaybackCommand {
            token,
            selected,
        }))
    }

    pub(crate) fn playback_selection_is_current(
        &self,
        selected: &super::playback::SelectedPlayback,
    ) -> bool {
        self.state.series_id == selected.detail_id
            && self
                .state
                .selected_playback_item()
                .is_some_and(|item| item.id == selected.list_item_id)
            && self
                .state
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                == Some(selected.media_source_id.as_str())
    }

    pub(crate) fn complete_playback(
        &mut self,
        command: super::playback::DetailPlaybackCommand,
        result: anyhow::Result<crate::player::gateway::ResolvedPlayback>,
        identity: &WorkspaceIdentity,
    ) -> Option<super::playback::DetailPlaybackUpdate> {
        if !command.token.is_for(identity)
            || !self.playback_selection_is_current(&command.selected)
            || !self.playback.as_mut()?.commit(&command.token)
        {
            return None;
        }
        self.playback = None;
        self.state.playback_loading = false;
        Some(match result {
            Ok(playback) => {
                self.state.playback_failed = None;
                self.state.pending_subtitle_choices.remove(
                    &crate::player::PlaybackTrackPreferenceKey {
                        item_id: command.selected.item_id.clone(),
                        media_source_id: command.selected.media_source_id.clone(),
                    },
                );
                super::playback::DetailPlaybackUpdate::Open {
                    selected: Box::new(command.selected),
                    playback,
                }
            }
            Err(error) => {
                let message = format!("获取播放地址失败：{error}");
                self.state.playback_failed = Some(message.clone());
                super::playback::DetailPlaybackUpdate::Failed(message)
            }
        })
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: &crate::player::PlaybackStateUpdate,
        user_data: &crate::emby::UserItemData,
    ) -> DetailUpdate {
        let change = self.state.apply_playback_update(update, user_data);
        DetailUpdate {
            change,
            playback_cancelled: self.cancel_reset_playback(),
            ..Default::default()
        }
    }

    pub(crate) fn begin(
        &mut self,
        resource: DetailResource,
        identity: WorkspaceIdentity,
        user_data_revision: u64,
    ) -> Option<DetailRequest> {
        self.activation.as_ref()?;
        let model = &mut self.state;
        let mut item_id = model.series_id.clone();
        let mut season_id = None;
        let allowed = match resource {
            DetailResource::Item => model.effects.item.can_start(),
            DetailResource::Similar => model.effects.similar.can_start(),
            DetailResource::Seasons => model.is_series() && model.effects.seasons.can_start(),
            DetailResource::NextUp => {
                model.should_load_next_up() && model.effects.next_up.can_start()
            }
            DetailResource::ResumeSources => {
                item_id = model.resume_media_item_id()?.to_string();
                model.effects.resume_sources.can_start()
            }
            DetailResource::Episodes => {
                if !model.is_series() {
                    return None;
                }
                season_id = Some(model.selected_season_id.clone()?);
                let already = model.episodes_request_season_id == season_id;
                !(already
                    && (model.effects.episodes.is_loading()
                        || (model.effects.episodes == LoadState::Loaded
                            && model.episodes.is_some())))
            }
        };
        if !allowed {
            return None;
        }
        match resource {
            DetailResource::Item => {
                model.effects.item = LoadState::Loading;
                model.item_failed = None;
            }
            DetailResource::Similar => {
                model.effects.similar = LoadState::Loading;
                model.similar_failed = None;
            }
            DetailResource::Seasons => {
                model.effects.seasons = LoadState::Loading;
                model.seasons_failed = None;
            }
            DetailResource::NextUp => {
                model.effects.next_up = LoadState::Loading;
                model.next_up_failed = None;
            }
            DetailResource::Episodes => {
                model.effects.episodes = LoadState::Loading;
                model.episodes_failed = None;
                model.episodes_request_season_id = season_id.clone();
            }
            DetailResource::ResumeSources => model.effects.resume_sources = LoadState::Loading,
        }
        let mut slot = RequestSlot::new(
            RequestScope::Detail {
                item_id: item_id.clone(),
                resource,
            },
            identity,
        );
        let token = slot.issue();
        self.requests.insert(resource, slot);
        Some(DetailRequest {
            token,
            resource,
            item_id,
            season_id,
            user_data_revision,
        })
    }

    pub(crate) fn complete(
        &mut self,
        request: &DetailRequest,
        result: anyhow::Result<DetailResponse>,
        identity: &WorkspaceIdentity,
        user_data: &mut UserDataState,
        pending: PendingUserData<'_>,
    ) -> Option<DetailUpdate> {
        // No user-data merge, image request or notification before acceptance.
        if !request.token.is_for(identity)
            || result
                .as_ref()
                .is_ok_and(|response| response.resource() != request.resource)
            || (request.resource == DetailResource::ResumeSources
                && self.state.resume_media_item_id() != Some(request.item_id.as_str()))
            || (request.resource != DetailResource::ResumeSources
                && self.state.series_id != request.item_id)
            || (request.resource == DetailResource::Episodes
                && (self.state.selected_season_id != request.season_id
                    || self.state.episodes_request_season_id != request.season_id))
            || !self
                .requests
                .get_mut(&request.resource)?
                .commit(&request.token)
        {
            return None;
        }
        self.requests.remove(&request.resource);
        let model = &mut self.state;
        if request.resource == DetailResource::Episodes
            && !user_data
                .series_response_is_current(Some(&request.item_id), request.user_data_revision)
        {
            // A mutation may already have refreshed the selected season. Only
            // replace an old read if it would otherwise leave the row loading.
            let load_episodes = model.effects.episodes == LoadState::Loading;
            if load_episodes {
                model.effects.episodes = LoadState::Idle;
            }
            return Some(DetailUpdate {
                load_episodes,
                retry: true,
                ..Default::default()
            });
        }
        let mut update = DetailUpdate::default();
        match result {
            Ok(DetailResponse::Item(mut item)) => {
                user_data.absorb(
                    &item.id,
                    item.user_data.as_ref(),
                    request.user_data_revision,
                    pending,
                );
                if let Some(data) = user_data.overrides.get(&item.id) {
                    item.user_data = Some(data.clone());
                }
                model.effects.item = LoadState::Loaded;
                model.item_failed = None;
                update.title_changed = model.title != item.name;
                model.title = item.name.clone();
                model.item = Some(*item);
                if model.is_movie() {
                    update.change = model.sync_media_source_selection();
                }
                update.images = true;
            }
            Ok(DetailResponse::Similar(mut items)) => {
                items.items.retain(|item| {
                    !item.id.trim().is_empty()
                        && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
                });
                items.total_record_count = items.items.len() as u32;
                user_data.absorb_items(&items, request.user_data_revision, pending);
                model.effects.similar = LoadState::Loaded;
                model.similar_failed = None;
                model.similar_items = Some(items);
                update.images = true;
            }
            Ok(DetailResponse::Seasons(seasons)) => {
                model.effects.seasons = LoadState::Loaded;
                model.seasons_failed = None;
                model.seasons = Some(seasons);
                update.change = model.choose_season_if_needed();
                update.load_episodes = true;
            }
            Ok(DetailResponse::NextUp(mut next_up)) => {
                apply_media_item_user_data_overrides(&mut next_up.items, &user_data.overrides);
                model.effects.next_up = LoadState::Loaded;
                model.next_up_failed = None;
                model.next_up = Some(next_up);
                update.change = model.apply_next_up_preference();
                update.load_episodes = true;
            }
            Ok(DetailResponse::Episodes(mut episodes)) => {
                for item in &episodes.items {
                    user_data.absorb(
                        &item.id,
                        item.user_data.as_ref(),
                        request.user_data_revision,
                        pending,
                    );
                }
                apply_media_item_user_data_overrides(&mut episodes.items, &user_data.overrides);
                model.effects.episodes = LoadState::Loaded;
                model.episodes_failed = None;
                model.episodes = Some(episodes);
                update.change = model.choose_episode_from_loaded_episodes();
                if model.should_reveal_selected_episode() {
                    update.change.reveal_episode = model.selected_episode_index();
                }
                update.images = true;
            }
            Ok(DetailResponse::ResumeSources(sources)) => {
                let previous = model
                    .selected_media_source()
                    .and_then(|source| source.id.clone());
                model.effects.resume_sources = LoadState::Loaded;
                model.resume_media_sources = Some(sources);
                if model
                    .selected_media_source()
                    .and_then(|source| source.id.as_ref())
                    != previous.as_ref()
                {
                    model.reset_playback_request();
                }
                update.change = model.sync_media_source_selection();
            }
            Err(error) => {
                let label = match request.resource {
                    DetailResource::Item => "加载媒体详情失败",
                    DetailResource::Similar => "加载相似作品失败",
                    DetailResource::Seasons => "加载剧集季数失败",
                    DetailResource::NextUp => "加载下一剧集失败",
                    DetailResource::Episodes => "加载剧集分集失败",
                    DetailResource::ResumeSources => "加载播放版本失败",
                };
                let message = format!("{label}：{error}");
                match request.resource {
                    DetailResource::Item => {
                        model.effects.item = LoadState::Failed;
                        model.item_failed = Some(message.clone());
                    }
                    DetailResource::Similar => {
                        model.effects.similar = LoadState::Failed;
                        model.similar_failed = Some(message.clone());
                    }
                    DetailResource::Seasons => {
                        model.effects.seasons = LoadState::Failed;
                        model.seasons_failed = Some(message.clone());
                    }
                    DetailResource::NextUp => {
                        model.effects.next_up = LoadState::Failed;
                        model.next_up_failed = Some(message.clone());
                        update.change = model.choose_season_if_needed();
                        update.load_episodes = true;
                    }
                    DetailResource::Episodes => {
                        model.effects.episodes = LoadState::Failed;
                        model.episodes_failed = Some(message.clone());
                    }
                    DetailResource::ResumeSources => {
                        model.effects.resume_sources = LoadState::Failed;
                        update.change = model.sync_media_source_selection();
                    }
                }
                update.error = Some(message);
            }
        }
        if update.change.episodes_reset {
            self.requests.remove(&DetailResource::Episodes);
        }
        update.playback_cancelled = self.cancel_reset_playback();
        Some(update)
    }

    pub(crate) fn dispatch(&mut self, intent: DetailIntent) -> Option<DetailUpdate> {
        let model = &mut self.state;
        let mut update = DetailUpdate::default();
        match intent {
            DetailIntent::Season(id) => {
                if model.selected_season_id.as_deref() != Some(&id) {
                    self.requests.remove(&DetailResource::Episodes);
                    model.selected_season_id = Some(id);
                    model.preferred_episode_id = None;
                    model.clear_preferred_season_hint();
                    update.change = model.reset_episode_selection();
                    update.load_episodes = true;
                }
            }
            DetailIntent::Episode(id) => {
                if !model
                    .episodes
                    .as_ref()
                    .is_some_and(|episodes| episodes.items.iter().any(|item| item.id == id))
                {
                    return None;
                }
                model.preferred_episode_id = Some(id.clone());
                update.change = model.apply_selected_episode(Some(id));
            }
            DetailIntent::MediaSource(index) => update.change = model.select_media_source(index),
            DetailIntent::Subtitle(index) => {
                let key = model.track_preference_key()?;
                let track = match index {
                    Some(index) => Some(model.selected_media_source().and_then(|source| {
                        source
                            .subtitle_streams()
                            .get(index)
                            .and_then(|stream| PlaybackTrack::from_subtitle_stream(stream, index))
                    })?),
                    None => None,
                };
                model
                    .pending_subtitle_choices
                    .insert(key, SavedTrackChoice::from_track(track.as_ref()));
            }
        }
        update.playback_cancelled = self.cancel_reset_playback();
        Some(update)
    }
}
