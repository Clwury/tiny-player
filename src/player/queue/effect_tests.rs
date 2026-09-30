use super::{effect, test_support::*};
use crate::player::{
    PlaybackLanguagePreferences, PlaybackTrack, SavedTrackChoice, SavedTrackChoices, TrackLanguage,
    gateway::{PlaybackGateway, PlaybackReport, ResolvedPlayback},
};
use std::sync::Mutex;

#[derive(Default)]
struct FakeGateway {
    requests: Mutex<Vec<(String, String)>>,
    subtitles: Mutex<Vec<(String, String)>>,
    fail: bool,
    blank_session: bool,
}
impl PlaybackGateway for FakeGateway {
    fn report(&self, _: &PlaybackReport) -> anyhow::Result<()> {
        panic!("source resolution cannot report playback")
    }
    fn resolve_source(&self, item: &str, source: &str) -> anyhow::Result<ResolvedPlayback> {
        self.requests
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        anyhow::ensure!(!self.fail, "offline");
        let mut result = resolved().source;
        if self.blank_session {
            result.play_session_id = Some("  ".into());
        }
        Ok(result)
    }
    fn subtitle_tracks(
        &self,
        _: &crate::emby::MediaSource,
        item: &str,
        source: &str,
    ) -> Vec<PlaybackTrack> {
        self.subtitles
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        vec![
            PlaybackTrack::new(5, "English (ASS)", true)
                .with_external_url(Some("https://example.invalid/new-subtitle".into())),
        ]
    }
}

#[test]
fn queue_resolution_preserves_requested_keys_and_resolved_transport_and_tracks() {
    let queue = queue();
    let gateway = FakeGateway::default();
    let key = effect::preference_key(&queue).unwrap();
    assert_eq!(key.item_id, "requested-physical");
    assert_eq!(key.media_source_id, "selected");
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::Track {
            stream_index: 1,
            label: "old label".into(),
            codec: None,
            is_external: false,
        }),
        subtitle: Some(SavedTrackChoice::Off),
    };
    let result = effect::resolve(&gateway, &queue, Default::default(), saved).unwrap();
    assert_eq!(
        *gateway.requests.lock().unwrap(),
        [("requested-physical".into(), "selected".into())]
    );
    assert_eq!(
        *gateway.subtitles.lock().unwrap(),
        [("resolved-physical".into(), "resolved-source".into())]
    );
    assert_eq!(result.source.item_id, "resolved-physical");
    assert_eq!(result.source.media_source_id, "resolved-source");
    assert_eq!(result.source.url, "https://example.invalid/video");
    assert_eq!(result.source.content_length, Some(123));
    assert_eq!(
        result.source.http_headers,
        vec![("Synthetic".into(), "header".into())]
    );
    assert_eq!(result.source.play_session_id.as_deref(), Some("session"));
    assert_eq!(result.track_preference_key, key);
    assert_eq!(result.title.as_ref(), "Episode 1");
    assert_eq!(result.initial_position_seconds, 2.5);
    assert_eq!(result.run_time_ticks, Some(900_000_000));
    assert_eq!(result.selected_tracks.audio_stream_index, Some(1));
    assert_eq!(result.selected_tracks.default_audio_stream_index, Some(3));
    assert_eq!(result.selected_tracks.subtitle_stream_index, None);
    assert_eq!(
        result.subtitle_tracks[0].external_url.as_deref(),
        Some("https://example.invalid/new-subtitle")
    );
}

#[test]
fn language_fallback_invalid_saved_choices_and_ended_positions_follow_existing_policy() {
    let mut queue = queue();
    let gateway = FakeGateway {
        blank_session: true,
        ..Default::default()
    };
    let languages = PlaybackLanguagePreferences {
        audio: TrackLanguage::English,
        subtitle: TrackLanguage::English,
    };
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::Track {
            stream_index: 99,
            label: "missing".into(),
            codec: None,
            is_external: false,
        }),
        subtitle: None,
    };
    for ticks in [Some(900_000_000), Some(u64::MAX), None, Some(0)] {
        queue.items[1].playback_position_ticks = ticks;
        let result = effect::resolve(&gateway, &queue, languages, saved.clone()).unwrap();
        assert_eq!(result.initial_position_seconds, 0.0);
        assert_eq!(result.selected_tracks.audio_stream_index, Some(1));
        assert_eq!(result.selected_tracks.subtitle_stream_index, Some(5));
        assert!(result.source.play_session_id.is_none());
    }
}

#[test]
fn missing_targets_or_valid_sources_fail_before_gateway_io() {
    let gateway = FakeGateway::default();
    let mut queue = queue();
    queue.items[1].media_sources.clear();
    assert!(effect::preference_key(&queue).is_none());
    assert_eq!(
        effect::resolve(&gateway, &queue, Default::default(), Default::default())
            .err()
            .unwrap()
            .to_string(),
        "目标单集没有可用视频源"
    );
    queue.items[1]
        .media_sources
        .push(serde_json::from_value(serde_json::json!({"Id":"  "})).unwrap());
    assert!(effect::resolve(&gateway, &queue, Default::default(), Default::default()).is_err());
    queue.items.clear();
    assert!(effect::resolve(&gateway, &queue, Default::default(), Default::default()).is_err());
    assert!(gateway.requests.lock().unwrap().is_empty());
    assert!(gateway.subtitles.lock().unwrap().is_empty());
}

#[test]
fn resolution_failure_is_returned_without_subtitle_side_effects() {
    let gateway = FakeGateway {
        fail: true,
        ..Default::default()
    };
    assert_eq!(
        effect::resolve(&gateway, &queue(), Default::default(), Default::default())
            .err()
            .unwrap()
            .to_string(),
        "offline"
    );
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
    assert!(gateway.subtitles.lock().unwrap().is_empty());
}
