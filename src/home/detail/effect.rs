use super::controller::{DetailRequest, DetailResponse};
use crate::{effects::DetailResource, home::gateway::HomeGateway};

pub(super) fn run_detail(
    gateway: &impl HomeGateway,
    request: &DetailRequest,
) -> anyhow::Result<DetailResponse> {
    let id = &request.item_id;
    match request.resource {
        DetailResource::Item => gateway
            .media_item(id)
            .map(Box::new)
            .map(DetailResponse::Item),
        DetailResource::Similar => gateway.similar_items(id).map(DetailResponse::Similar),
        DetailResource::Seasons => gateway.show_seasons(id).map(DetailResponse::Seasons),
        DetailResource::NextUp => gateway.show_next_up(id).map(DetailResponse::NextUp),
        DetailResource::Episodes => gateway
            .show_episodes(id, request.season_id.as_deref())
            .map(DetailResponse::Episodes),
        // Query the original playable item, without a MediaSourceId filter.
        DetailResource::ResumeSources => gateway
            .playback_media_sources(id)
            .map(DetailResponse::ResumeSources),
    }
}

use super::playback::{DetailPlaybackCommand, SelectedPlayback, playback_queue};
use crate::{
    home::model::detail::SeriesDetailModel,
    player::{
        PlaybackLanguagePreferences, SavedTrackChoices,
        gateway::{PlaybackGateway, ResolvedPlayback},
    },
};

pub(super) fn run_playback(
    gateway: &impl PlaybackGateway,
    command: &DetailPlaybackCommand,
) -> anyhow::Result<ResolvedPlayback> {
    gateway.resolve_source(&command.selected.item_id, &command.selected.media_source_id)
}

pub(super) fn selected_playback(
    detail: &SeriesDetailModel,
    gateway: &impl PlaybackGateway,
    languages: PlaybackLanguagePreferences,
    saved_tracks: &SavedTrackChoices,
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

    let audio_tracks = crate::player::playback_audio_tracks_for_source(source);
    let item_id = source.playback_item_id(&item.id);
    let subtitle_tracks = gateway.subtitle_tracks(source, item_id, &media_source_id);
    let mut selected_tracks =
        crate::player::preferred_playback_track_selection(source, &subtitle_tracks, languages);
    saved_tracks.apply(&audio_tracks, &subtitle_tracks, &mut selected_tracks);
    let remember_subtitle_on_start = detail
        .pending_subtitle_choice()
        .is_some_and(|choice| choice.resolve(&subtitle_tracks).is_some());
    let playback_position_ticks = detail.playback_position_ticks();
    let mut queue = playback_queue(detail, item, &title);
    if let Some(current) = queue.items.get_mut(queue.current_index) {
        current.playback_position_ticks = playback_position_ticks;
        current.media_sources = detail.selected_media_sources().unwrap_or_default().to_vec();
    }

    Ok(SelectedPlayback {
        detail_id: detail.series_id.clone(),
        list_item_id: item.id.clone(),
        item_id: item_id.to_string(),
        media_source_id,
        title: title.into(),
        audio_tracks,
        subtitle_tracks,
        selected_tracks,
        remember_subtitle_on_start,
        run_time_ticks: item.run_time_ticks,
        playback_position_ticks,
        queue,
    })
}
