//! Pure detail state and invariants; no GPUI resources or IO.
use std::collections::HashMap;

use crate::emby::{MediaItem, MediaItems, MediaSource, ResumeItem, UserItem, UserItems};
use crate::media::{
    PlaybackTrack, PlaybackTrackExt, PlaybackTrackPreferenceKey, SavedTrackChoice,
    SavedTrackChoices, TrackLanguage, video_version::VideoVersion,
};

use crate::home::model::LoadState;

/// Domain outcomes consumed by presentation after a transition. No GPUI
/// resources or side effects are stored in the business model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DetailChange {
    pub(crate) episode_changed: bool,
    pub(crate) episodes_reset: bool,
    pub(crate) source_selected: bool,
    pub(crate) selection_unavailable: bool,
    pub(crate) subtitles_unavailable: bool,
    pub(crate) reveal_episode: Option<usize>,
}

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
enum SeriesDetailKind {
    Series,
    Movie,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeriesDetailOrigin {
    UserView,
    Resume,
}

/// Owned by DetailController within HomeNavigation. Selection methods and response
/// reducers write this state; target/season changes invalidate the related data.
/// Retained in detail history, dropped when that history or workspace is cleared.
#[derive(Clone, Debug)]
pub(crate) struct SeriesDetailModel {
    kind: SeriesDetailKind,
    origin: SeriesDetailOrigin,
    pub(crate) series_id: String,
    pub(crate) title: String,
    pub(crate) effects: SeriesDetailEffects,
    pub(crate) item: Option<MediaItem>,
    pub(crate) item_failed: Option<String>,
    pub(crate) seasons: Option<MediaItems>,
    pub(crate) seasons_failed: Option<String>,
    pub(crate) next_up: Option<MediaItems>,
    pub(crate) next_up_failed: Option<String>,
    pub(crate) resume_episode: Option<ResumeItem>,
    // Keep the original playable item separate from its series/episode group.
    resume_media_item_id: Option<String>,
    pub(crate) resume_media_sources: Option<Vec<MediaSource>>,
    pub(crate) resume_video_version: Option<VideoVersion>,
    pub(crate) episodes: Option<MediaItems>,
    pub(crate) episodes_failed: Option<String>,
    pub(crate) episode_selection_warning: Option<String>,
    pub(crate) similar_items: Option<UserItems>,
    pub(crate) similar_failed: Option<String>,
    pub(crate) playback_loading: bool,
    pub(crate) playback_failed: Option<String>,
    pub(crate) selected_season_id: Option<String>,
    pub(crate) selected_episode_id: Option<String>,
    pub(crate) preferred_episode_id: Option<String>,
    preferred_season_id_hint: Option<String>,
    pub(crate) selected_media_source_index: Option<usize>,
    manual_video_version: Option<VideoVersion>,
    pub(crate) pending_subtitle_choices: HashMap<PlaybackTrackPreferenceKey, SavedTrackChoice>,
    pub(crate) episodes_request_season_id: Option<String>,
}

mod construction;
mod selection;
mod selectors;
#[cfg(test)]
mod tests;
mod transitions;
