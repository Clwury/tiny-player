use super::*;
use crate::emby::{MediaSource, MediaStream};
use crate::media::{playback_audio_tracks_for_source, preferred_playback_track_selection};
use crate::server::{CachedServer, Protocol, ServerEndpoint};

#[test]
fn language_preferences_select_actual_stream_indices_and_keep_external_subtitle_metadata() {
    use crate::player::{PlaybackLanguagePreferences, TrackLanguage};
    let source = serde_json::from_value(serde_json::json!({
        "DefaultSubtitleStreamIndex": 5,
        "MediaStreams": [
            {"Index": 1, "Type": "Audio", "Language": "eng", "IsDefault": true},
            {"Type": "Audio", "Language": "jpn", "IsDefault": true},
            {"Index": 4, "Type": "Audio", "Language": "jpn", "DisplayTitle": "Japanese AAC stereo", "Title": "原声音轨", "Codec": "aac"},
            {"Index": 5, "Type": "Subtitle", "Language": "eng"},
            {"Type": "Subtitle", "Language": "chs"},
            {"Index": 9, "Type": "Subtitle", "Language": "zho", "DisplayTitle": "简体中文", "Title": "简体双语字幕", "IsExternal": true, "Codec": "ass"}
        ]
    })).unwrap();
    let tracks =
        playback_subtitle_tracks_for_source(&source, &debug_server(), "episode-1", "source-1");
    let audio = playback_audio_tracks_for_source(&source);
    assert_eq!(audio.len(), 2);
    assert_eq!(audio[0].metadata_label(), "英语");
    assert_eq!(audio[1].label.as_str(), "Japanese AAC stereo");
    assert_eq!(audio[1].language.as_deref(), Some("jpn"));
    assert_eq!(audio[1].title.as_deref(), Some("原声音轨"));
    assert_eq!(audio[1].codec.as_deref(), Some("aac"));
    assert_eq!(audio[1].metadata_label(), "日语 [原声音轨]");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].metadata_label(), "英语");
    assert_eq!(tracks[1].label.as_str(), "简体中文");
    assert_eq!(tracks[1].language.as_deref(), Some("zho"));
    assert_eq!(tracks[1].title.as_deref(), Some("简体双语字幕"));
    assert_eq!(tracks[1].metadata_label(), "中文 [简体双语字幕]");
    let languages = PlaybackLanguagePreferences {
        audio: TrackLanguage::Japanese,
        subtitle: TrackLanguage::ChineseSimplified,
    };
    let selection = preferred_playback_track_selection(&source, &tracks, languages);
    assert_eq!(selection.audio_stream_index, Some(4));
    assert_eq!(selection.default_audio_stream_index, Some(1));
    assert_eq!(selection.subtitle_stream_index, Some(9));
    assert_eq!(selection.subtitle_codec.as_deref(), Some("ass"));
    assert!(
        selection
            .subtitle_external_url
            .as_deref()
            .unwrap()
            .contains("/Subtitles/9/")
    );

    for language in [TrackLanguage::Default, TrackLanguage::Russian] {
        let selection = preferred_playback_track_selection(
            &source,
            &tracks,
            PlaybackLanguagePreferences {
                audio: language,
                subtitle: language,
            },
        );
        assert_eq!(selection.audio_stream_index, Some(1));
        assert_eq!(selection.subtitle_stream_index, Some(5));
    }
}

#[test]
fn default_subtitle_selection_skips_streams_without_indices() {
    let source = MediaSource {
        id: Some("source-1".to_string()),
        item_id: None,
        name: None,
        path: None,
        source_type: None,
        container: None,
        size: None,
        bitrate: None,
        run_time_ticks: None,
        media_streams: Some(vec![
            MediaStream {
                index: None,
                stream_type: Some("Subtitle".to_string()),
                display_title: None,
                title: None,
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: None,
                is_external: None,
                is_default: Some(true),
                is_forced: None,
                is_text_subtitle_stream: None,
                supports_external_stream: None,
                width: None,
                height: None,
                video_range: None,
            },
            MediaStream {
                index: Some(7),
                stream_type: Some("Subtitle".to_string()),
                display_title: None,
                title: None,
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: None,
                is_external: None,
                is_default: None,
                is_forced: Some(true),
                is_text_subtitle_stream: None,
                supports_external_stream: None,
                width: None,
                height: None,
                video_range: None,
            },
        ]),
        default_subtitle_stream_index: None,
    };
    let tracks =
        playback_subtitle_tracks_for_source(&source, &debug_server(), "episode-1", "source-1");

    let selection = preferred_playback_track_selection(&source, &tracks, Default::default());

    assert_eq!(selection.subtitle_stream_index, Some(7));
}

fn debug_server() -> CachedServer {
    CachedServer {
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
    }
}
