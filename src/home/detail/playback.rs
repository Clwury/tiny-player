//! Prepared detail launch and pure queue selection, independent of GPUI.
use crate::{
    effects::RequestToken,
    emby::MediaItem,
    home::detail::model::SeriesDetailModel,
    media::{PlaybackQueue, PlaybackQueueItem, PlaybackTrack, PlaybackTrackSelection},
};

#[derive(Clone)]
pub(crate) struct SelectedPlayback {
    pub(crate) detail_id: String,
    pub(crate) list_item_id: String,
    pub(crate) item_id: String,
    pub(crate) media_source_id: String,
    pub(crate) title: std::sync::Arc<str>,
    pub(crate) audio_tracks: Vec<PlaybackTrack>,
    pub(crate) subtitle_tracks: Vec<PlaybackTrack>,
    pub(crate) selected_tracks: PlaybackTrackSelection,
    pub(crate) remember_subtitle_on_start: bool,
    pub(crate) run_time_ticks: Option<u64>,
    pub(crate) playback_position_ticks: Option<u64>,
    pub(crate) queue: PlaybackQueue,
}

#[derive(Clone)]
pub(crate) struct DetailPlaybackCommand {
    pub(crate) token: RequestToken,
    pub(crate) selected: SelectedPlayback,
}

pub(crate) enum DetailPlaybackUpdate {
    Open {
        selected: Box<SelectedPlayback>,
        playback: crate::media::gateway::ResolvedPlayback,
    },
    Failed(String),
}

pub(super) fn playback_queue(
    detail: &SeriesDetailModel,
    selected_item: &MediaItem,
    selected_title: &str,
) -> PlaybackQueue {
    if detail.is_movie() {
        return PlaybackQueue::new(
            vec![playback_queue_item(
                selected_item,
                selected_title.to_string().into(),
                None,
                None,
            )],
            0,
        );
    }

    let series_name = detail
        .item
        .as_ref()
        .map(|item| item.name.as_str())
        .unwrap_or(detail.title.as_str());
    let selected_season_id = detail.selected_season_id.clone();
    // The response is already scoped to this season and checked when loaded.
    // Grouped versions can carry other physical SeasonIds, so do not filter
    // those episodes out of the playback queue.
    let mut items = detail
        .episodes
        .as_ref()
        .map(|episodes| {
            episodes
                .items
                .iter()
                .filter(|episode| playback_queue_episode_is_valid(episode))
                .map(|episode| {
                    playback_queue_item(
                        episode,
                        format!("{series_name} {}", episode.episode_label()).into(),
                        Some(detail.series_id.clone()),
                        // Playback updates must retain the detail's season context.
                        selected_season_id
                            .clone()
                            .or_else(|| episode.season_id.clone()),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current_index = items
        .iter()
        .position(|item| item.item_id == selected_item.id);
    if let Some(current_index) = current_index {
        return PlaybackQueue::new(items, current_index);
    }

    items.clear();
    items.push(playback_queue_item(
        selected_item,
        selected_title.to_string().into(),
        Some(detail.series_id.clone()),
        selected_season_id.or_else(|| selected_item.season_id.clone()),
    ));
    PlaybackQueue::new(items, 0)
}

fn playback_queue_episode_is_valid(item: &MediaItem) -> bool {
    !item.id.trim().is_empty()
        && item
            .item_type
            .as_deref()
            .is_none_or(|item_type| item_type.eq_ignore_ascii_case("Episode"))
        && item.media_sources.as_ref().is_some_and(|sources| {
            sources
                .iter()
                .any(|source| source.id.as_deref().is_some_and(|id| !id.trim().is_empty()))
        })
}

fn playback_queue_item(
    item: &MediaItem,
    title: std::sync::Arc<str>,
    series_id: Option<String>,
    season_id: Option<String>,
) -> PlaybackQueueItem {
    PlaybackQueueItem {
        item_id: item.id.clone(),
        title,
        episode_label: item.episode_label().into(),
        overview: item.overview.clone(),
        primary_image_tag: item.primary_image_tag().map(str::to_string),
        series_id,
        season_id,
        premiere_date: item.premiere_date.clone(),
        run_time_ticks: item.run_time_ticks,
        playback_position_ticks: item.playback_position_ticks(),
        media_sources: item.media_sources.clone().unwrap_or_default(),
    }
}
