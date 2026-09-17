use super::*;

fn server() -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "local", "server_id": "remote", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap()
}

fn key() -> PlaybackTrackPreferenceKey {
    PlaybackTrackPreferenceKey {
        item_id: "episode".into(),
        media_source_id: "source".into(),
    }
}

fn track(index: usize, label: &str) -> PlaybackTrack {
    PlaybackTrack::new(index, label.to_string(), false).with_codec(Some("ass".into()))
}

#[test]
fn saved_choices_exclude_external_urls_and_resolve_current_delivery_urls() {
    let audio = track(1, "Japanese");
    let mut subtitle = track(9, "Chinese Simplified (默认 ASS)");
    subtitle.is_external = true;
    subtitle.external_url = Some("https://example.com/subtitle?api_key=private-token".into());
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::from_track(Some(&audio))),
        subtitle: Some(SavedTrackChoice::from_track(Some(&subtitle))),
    };
    let json = serde_json::to_string(&saved).unwrap();
    assert!(!json.contains("private-token"));
    assert!(!json.contains("external_url"));
    let restored: SavedTrackChoices = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, saved);

    let current_subtitle = subtitle.with_external_url(Some("https://example.com/fresh.ass".into()));
    let mut selected = PlaybackTrackSelection {
        audio_stream_index: Some(5),
        subtitle_stream_index: Some(10),
        ..Default::default()
    };
    restored.apply(&[audio], &[current_subtitle], &mut selected);
    assert_eq!(selected.audio_stream_index, Some(1));
    assert_eq!(selected.subtitle_stream_index, Some(9));
    assert_eq!(
        selected.subtitle_external_url.as_deref(),
        Some("https://example.com/fresh.ass")
    );
}

#[test]
fn explicit_off_survives_reload_and_is_distinct_from_automatic_selection() {
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::Off),
        subtitle: Some(SavedTrackChoice::Off),
    };
    let saved: SavedTrackChoices =
        serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
    let subtitle = track(9, "Chinese").with_external_url(Some("current.ass".into()));
    let mut selected = PlaybackTrackSelection {
        audio_stream_index: Some(5),
        ..Default::default()
    };
    selected.set_subtitle_track(Some(&subtitle));
    let automatic: SavedTrackChoices = serde_json::from_str("{}").unwrap();
    automatic.apply(&[], &[], &mut selected);
    assert_eq!(selected.audio_stream_index, Some(5));
    assert_eq!(selected.subtitle_stream_index, Some(9));
    saved.apply(&[], &[], &mut selected);
    assert_eq!(selected.audio_stream_index, None);
    assert_eq!(selected.subtitle_stream_index, None);
    assert_eq!(selected.subtitle_external_url, None);
    assert_eq!(selected.subtitle_codec, None);
}

#[test]
fn track_preferences_are_isolated_by_account_item_and_media_source() {
    let server = server();
    let key = key();
    let mut preferences = PlaybackTrackPreferences::default();
    preferences.store(
        &server,
        &key,
        PlaybackTrackKind::Subtitle,
        SavedTrackChoice::Off,
    );
    for other in [
        CachedServer {
            id: "other-local".into(),
            ..server.clone()
        },
        CachedServer {
            server_id: Some("other-server".into()),
            ..server.clone()
        },
        CachedServer {
            user_id: Some("other-user".into()),
            ..server.clone()
        },
    ] {
        assert!(preferences.lookup(&other, &key).is_none());
    }
    for other in [
        PlaybackTrackPreferenceKey {
            item_id: "other-episode".into(),
            ..key.clone()
        },
        PlaybackTrackPreferenceKey {
            media_source_id: "other-version".into(),
            ..key.clone()
        },
    ] {
        assert!(preferences.lookup(&server, &other).is_none());
    }
    let saved = preferences.lookup(&server, &key).unwrap();
    assert_eq!(saved.audio, None);
    assert_eq!(saved.subtitle, Some(SavedTrackChoice::Off));
}

#[test]
fn renumbered_track_restores_only_when_its_metadata_matches_unambiguously() {
    let saved = SavedTrackChoice::from_track(Some(&track(9, "Chinese")));
    let tracks = [track(9, "English"), track(12, "Chinese")];
    assert_eq!(saved.resolve(&tracks).flatten().unwrap().stream_index, 12);
    let ambiguous = [track(12, "Chinese"), track(13, "Chinese")];
    assert!(saved.resolve(&ambiguous).is_none());
    let exact = [track(9, "Chinese"), track(13, "Chinese")];
    assert_eq!(saved.resolve(&exact).flatten().unwrap().stream_index, 9);
}

#[test]
fn unavailable_saved_tracks_preserve_current_language_defaults() {
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::from_track(Some(&track(1, "old audio")))),
        subtitle: Some(SavedTrackChoice::from_track(Some(&track(
            9,
            "old subtitle",
        )))),
    };
    let mut selected = PlaybackTrackSelection {
        audio_stream_index: Some(1),
        subtitle_stream_index: Some(10),
        ..Default::default()
    };
    let expected = selected.clone();
    saved.apply(
        &[track(1, "new audio")],
        &[track(9, "new subtitle")],
        &mut selected,
    );
    assert_eq!(selected, expected);
}
