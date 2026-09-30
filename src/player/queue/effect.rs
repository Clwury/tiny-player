//! Blocking source resolution through the playback port. No GPUI or account IO
//! reaches the controller; the result is committed before page event assembly.
use super::model::ResolvedQueuePlayback;
use crate::player::{
    PlaybackLanguagePreferences, PlaybackTrackPreferenceKey, SavedTrackChoices,
    gateway::PlaybackGateway, model::queue::PlaybackQueue, playback_audio_tracks_for_source,
    playback_initial_position_seconds, preferred_playback_media_source,
    preferred_playback_track_selection,
};
use anyhow::{Result, anyhow};

pub(in crate::player) fn preference_key(
    queue: &PlaybackQueue,
) -> Option<PlaybackTrackPreferenceKey> {
    let item = queue.current()?;
    let source = preferred_playback_media_source(&item.media_sources)?;
    Some(PlaybackTrackPreferenceKey {
        item_id: source.playback_item_id(&item.item_id).into(),
        media_source_id: source.id.clone()?,
    })
}

pub(in crate::player) fn resolve(
    gateway: &dyn PlaybackGateway,
    queue: &PlaybackQueue,
    languages: PlaybackLanguagePreferences,
    saved_tracks: SavedTrackChoices,
) -> Result<ResolvedQueuePlayback> {
    let item = queue.current().ok_or_else(|| anyhow!("目标单集不存在"))?;
    let source = preferred_playback_media_source(&item.media_sources)
        .ok_or_else(|| anyhow!("目标单集没有可用视频源"))?;
    let selected_media_source_id = source
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("目标单集视频源缺少 ID"))?;
    let requested_item_id = source.playback_item_id(&item.item_id);
    let mut resolved = gateway.resolve_source(requested_item_id, selected_media_source_id)?;
    let audio_tracks = playback_audio_tracks_for_source(source);
    let subtitle_tracks =
        gateway.subtitle_tracks(source, &resolved.item_id, &resolved.media_source_id);
    let mut selected_tracks =
        preferred_playback_track_selection(source, &subtitle_tracks, languages);
    saved_tracks.apply(&audio_tracks, &subtitle_tracks, &mut selected_tracks);
    resolved.play_session_id = resolved.play_session_id.filter(|id| !id.trim().is_empty());
    Ok(ResolvedQueuePlayback {
        source: resolved,
        title: item.title.clone(),
        audio_tracks,
        subtitle_tracks,
        selected_tracks,
        track_preference_key: PlaybackTrackPreferenceKey {
            item_id: requested_item_id.into(),
            media_source_id: selected_media_source_id.into(),
        },
        initial_position_seconds: playback_initial_position_seconds(
            item.playback_position_ticks,
            item.run_time_ticks,
        ),
        run_time_ticks: item.run_time_ticks,
    })
}
