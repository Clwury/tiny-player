use std::{collections::HashMap, path::Path, time::Duration};

use gpui::{AppContext as _, Entity, TestAppContext};

use crate::{
    emby::EmbyClient,
    player::{
        PlaybackStateUpdate, PlaybackTrack, PlaybackTrackKind, PlaybackTrackPreferenceKey,
        PlaybackTrackPreferences, SavedTrackChoice, SavedTrackChoices,
    },
    server::CachedServer,
};

use super::super::{HomeContent, cache, video_version::VideoVersion};

fn server() -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "local", "server_id": "remote", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap()
}

fn key(source_id: &str) -> PlaybackTrackPreferenceKey {
    PlaybackTrackPreferenceKey {
        item_id: "episode".into(),
        media_source_id: source_id.into(),
    }
}

fn home(cx: &mut TestAppContext, path: &Path) -> Entity<HomeContent> {
    let page = cx.new(|cx| {
        let mut page = HomeContent::new(server(), EmbyClient::new("test".into()).unwrap(), cx);
        page.snapshot_save_path = Some(path.to_path_buf());
        page
    });
    // GPUI registers global observers at the end of the update cycle.
    cx.run_until_parked();
    page
}

#[gpui::test]
fn track_choices_save_under_video_versions_and_restore_after_reopening(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let audio = PlaybackTrack::new(1, "Japanese", false).with_codec(Some("flac".into()));
    let subtitle = PlaybackTrack::new(9, "Chinese Simplified (ASS)", true)
        .with_codec(Some("ass".into()))
        .with_external_url(Some(
            "https://example.com/subtitle?api_key=private-token".into(),
        ));
    cx.update(|cx| {
        for (source, kind, track) in [
            ("1080", PlaybackTrackKind::Audio, Some(&audio)),
            ("1080", PlaybackTrackKind::Subtitle, None),
            ("2160", PlaybackTrackKind::Subtitle, Some(&subtitle)),
        ] {
            PlaybackTrackPreferences::remember(&server(), &[key(source)], kind, track, cx);
        }
    });
    cx.run_until_parked();
    assert!(!path.exists());
    cx.executor().advance_clock(Duration::from_millis(450));
    cx.run_until_parked();
    let bytes = std::fs::read(&path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["played_video_versions"]["episode"]["track_preferences"]["1080"]["audio"]["stream_index"],
        1
    );
    assert_eq!(
        json["played_video_versions"]["episode"]["track_preferences"]["1080"]["subtitle"]["mode"],
        "off"
    );
    assert!(json.get("track_preferences").is_none());
    assert!(!String::from_utf8(bytes).unwrap().contains("private-token"));

    cx.update(|_| drop(page));
    cx.run_until_parked();
    cx.update(|cx| cx.set_global(PlaybackTrackPreferences::default()));
    let reopened = home(cx, &path);
    let snapshot = cache::load_snapshot_from(&path, &server())
        .unwrap()
        .unwrap();
    reopened.update(cx, |page, cx| page.hydrate_home_snapshot(snapshot, cx));
    cx.update(|cx| {
        let saved = PlaybackTrackPreferences::get(&server(), &key("1080"), cx);
        assert_eq!(
            saved.audio,
            Some(SavedTrackChoice::from_track(Some(&audio)))
        );
        assert_eq!(saved.subtitle, Some(SavedTrackChoice::Off));
        let saved = PlaybackTrackPreferences::get(&server(), &key("2160"), cx);
        assert_eq!(saved.audio, None);
        assert_eq!(
            saved.subtitle,
            Some(SavedTrackChoice::from_track(Some(&subtitle)))
        );
        assert_eq!(
            PlaybackTrackPreferences::get(&server(), &key("new-version"), cx),
            SavedTrackChoices::default()
        );
    });
}

#[gpui::test]
fn close_flushes_pending_track_choices_after_an_older_save(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let audio = PlaybackTrack::new(1, "Japanese", false);
    cx.update(|cx| {
        PlaybackTrackPreferences::remember(
            &server(),
            &[key("source")],
            PlaybackTrackKind::Audio,
            Some(&audio),
            cx,
        );
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(450));
    cx.run_until_parked();
    assert!(path.exists());
    // Close before the observer or debounce for the latest selection has run.
    cx.update(|cx| {
        PlaybackTrackPreferences::remember(
            &server(),
            &[key("source")],
            PlaybackTrackKind::Audio,
            None,
            cx,
        );
    });
    cx.update(|_| drop(page));
    cx.run_until_parked();
    let snapshot = cache::load_snapshot_from(&path, &server())
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.played_video_versions["episode"].track_preferences["source"].audio,
        Some(SavedTrackChoice::Off)
    );
}

#[gpui::test]
fn late_snapshot_preserves_new_choices_and_playback_updates_keep_all_versions(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let mut snapshot = cache::HomeSnapshot::new(&server(), None, None, HashMap::new());
    let old_subtitle = PlaybackTrack::new(10, "Chinese Simplified (ASS)", false);
    snapshot.played_video_versions.insert(
        "episode".into(),
        VideoVersion {
            source_id: "1080".into(),
            name: Some("1080p".into()),
            track_preferences: HashMap::from([
                (
                    "1080".into(),
                    SavedTrackChoices {
                        audio: Some(SavedTrackChoice::Off),
                        subtitle: Some(SavedTrackChoice::from_track(Some(&old_subtitle))),
                    },
                ),
                (
                    "2160".into(),
                    SavedTrackChoices {
                        subtitle: Some(SavedTrackChoice::Off),
                        ..Default::default()
                    },
                ),
            ]),
        },
    );
    page.update(cx, |page, cx| {
        PlaybackTrackPreferences::remember(
            &server(),
            &[key("1080")],
            PlaybackTrackKind::Subtitle,
            None,
            cx,
        );
        page.hydrate_home_snapshot(snapshot, cx);
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        let saved = PlaybackTrackPreferences::get(&server(), &key("1080"), cx);
        assert_eq!(saved.audio, Some(SavedTrackChoice::Off));
        assert_eq!(saved.subtitle, Some(SavedTrackChoice::Off));
        assert_eq!(
            page.played_video_versions["episode"].track_preferences["1080"],
            saved
        );
        let choices = page.played_video_versions["episode"]
            .track_preferences
            .clone();
        page.apply_playback_update(
            PlaybackStateUpdate {
                item_id: "episode".into(),
                list_item_id: "episode".into(),
                media_source_id: "2160".into(),
                media_source_name: Some("2160p".into()),
                series_id: None,
                season_id: None,
                position_ticks: 100,
                run_time_ticks: Some(1_000),
                ended: false,
                failed: false,
                selected_item_id: None,
                stop_completion: None,
            },
            cx,
        );
        assert_eq!(page.played_video_versions["episode"].source_id, "2160");
        assert_eq!(
            page.played_video_versions["episode"].track_preferences,
            choices
        );
    });
}

#[gpui::test]
fn another_account_track_change_does_not_schedule_this_snapshot(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let other = CachedServer {
        user_id: Some("other".into()),
        ..server()
    };
    cx.update(|cx| {
        PlaybackTrackPreferences::remember(
            &other,
            &[key("source")],
            PlaybackTrackKind::Subtitle,
            None,
            cx,
        );
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert!(page.played_video_versions.is_empty());
        assert!(!page.snapshot_save_pending);
    });
    cx.update(|_| drop(page));
    cx.run_until_parked();
    assert!(!path.exists());
}
