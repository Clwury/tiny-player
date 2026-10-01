use super::model::ResolvedQueuePlayback;
use crate::media::gateway::ResolvedPlayback;
use crate::{
    effects::WorkspaceIdentity,
    player::{PlaybackQueue, PlaybackQueueItem, PlaybackTrackPreferenceKey},
};

pub(super) fn identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        local_server_id: "local".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some("user".into()),
    }
}
pub(super) fn queue() -> PlaybackQueue {
    PlaybackQueue::new((0..3).map(|index| PlaybackQueueItem {
        item_id: format!("grouped-{index}"), title: format!("Episode {index}").into(), episode_label: format!("S1E{index}").into(),
        overview: Some("overview".into()), primary_image_tag: None, series_id: Some("series".into()), season_id: Some("season".into()),
        premiere_date: Some("2026-01-01".into()), run_time_ticks: Some(900_000_000), playback_position_ticks: Some(25_000_000),
        media_sources: vec![serde_json::from_value(serde_json::json!({"Id":"selected", "ItemId":"requested-physical", "Type":"Default", "MediaStreams":[
            {"Index":1,"Type":"Audio","Language":"eng"}, {"Index":3,"Type":"Audio","IsDefault":true},
            {"Index":5,"Type":"Subtitle","Language":"eng","Codec":"ass"}
        ]})).unwrap()],
    }).collect(), 1)
}
pub(super) fn resolved() -> ResolvedQueuePlayback {
    ResolvedQueuePlayback {
        source: ResolvedPlayback {
            item_id: "resolved-physical".into(),
            media_source_id: "resolved-source".into(),
            url: "https://example.invalid/video".into(),
            http_headers: vec![("Synthetic".into(), "header".into())],
            content_length: Some(123),
            play_session_id: Some("session".into()),
        },
        title: "Episode".into(),
        audio_tracks: vec![],
        subtitle_tracks: vec![],
        selected_tracks: Default::default(),
        track_preference_key: PlaybackTrackPreferenceKey {
            item_id: "requested-physical".into(),
            media_source_id: "selected".into(),
        },
        initial_position_seconds: 2.5,
        run_time_ticks: Some(900_000_000),
    }
}
