use super::*;
use crate::player::model::queue::PlaybackQueueItem;
use crate::server::{CachedServer, Protocol, ServerEndpoint};

#[test]
fn playback_request_debug_redacts_urls_headers_and_credentials() {
    let selected_tracks = PlaybackTrackSelection {
        subtitle_stream_index: Some(3),
        subtitle_external_url: Some(
            "https://example.com/subtitle.ass?api_key=secret-token".to_string(),
        ),
        ..PlaybackTrackSelection::default()
    };
    let request = PlaybackRequest {
        title: "Episode".into(),
        url: "https://example.com/video.mkv?api_key=secret-token".to_string(),
        http_headers: vec![("X-Emby-Token".to_string(), "secret-token".to_string())],
        content_length: Some(100),
        audio_tracks: Vec::new(),
        subtitle_tracks: Vec::new(),
        selected_tracks,
        track_preference_key: crate::player::PlaybackTrackPreferenceKey {
            item_id: "episode-1".into(),
            media_source_id: "source-1".into(),
        },
        initial_position_seconds: 12.0,
        remember_subtitle_on_start: false,
        queue: PlaybackQueue::new(vec![queue_item("episode-1")], 0),
        emby: EmbyPlaybackContext {
            client: crate::emby::EmbyClient::new("device-1".to_string()).unwrap(),
            server: CachedServer {
                id: "local-1".to_string(),
                endpoint: ServerEndpoint {
                    protocol: Protocol::Https,
                    address: "example.com".to_string(),
                    port: 443,
                    path: "/emby".to_string(),
                },
                username: "user".to_string(),
                password: "secret-password".to_string(),
                user_id: Some("user-1".to_string()),
                server_id: Some("server-1".to_string()),
                server_name: Some("Server".to_string()),
                icon_url: None,
                icon_is_custom: false,
                access_token: Some("secret-token".to_string()),
                needs_auth_refresh: false,
                item_counts: None,
                added_at_unix: 1,
            },
            item_id: "episode-1".to_string(),
            media_source_id: "source-1".to_string(),
            play_session_id: Some("session-1".to_string()),
            run_time_ticks: Some(100_000_000),
        },
    };

    let debug = format!("{request:?}");

    assert!(!debug.contains("secret-token"));
    assert!(!debug.contains("secret-password"));
    assert!(!debug.contains("video.mkv"));
    assert!(!debug.contains("subtitle.ass"));
    assert!(!debug.contains("Episode"));
    assert!(!debug.contains("initial_position_seconds"));
    assert!(debug.contains("episode-1"));
    assert!(debug.contains("source-1"));
    assert!(debug.contains("queue_length"));
    assert!(debug.contains("queue_index"));
}

fn queue_item(item_id: &str) -> PlaybackQueueItem {
    PlaybackQueueItem {
        item_id: item_id.to_string(),
        title: item_id.to_string().into(),
        episode_label: item_id.to_string().into(),
        overview: None,
        primary_image_tag: None,
        series_id: Some("series-1".to_string()),
        season_id: Some("season-1".to_string()),
        premiere_date: None,
        run_time_ticks: None,
        playback_position_ticks: None,
        media_sources: Vec::new(),
    }
}
