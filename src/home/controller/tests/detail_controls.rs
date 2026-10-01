use super::*;
use crate::home::detail::model::SeriesDetailModel;

fn movie() -> SeriesDetailModel {
    let metadata = json!({
        "Id": "movie", "Name": "Movie", "Type": "Movie",
        "UserData": {"PlaybackPositionTicks": 300000000},
        "MediaSources": [
            {"Id": "one", "Name": "One", "MediaStreams": [
                {"Type": "Subtitle", "Index": 3, "Codec": "ass", "Language": "eng"}
            ]},
            {"Id": "two", "Name": "Two", "MediaStreams": [
                {"Type": "Subtitle", "Index": 4, "Codec": "ass", "Language": "eng"}
            ]}
        ]
    });
    let mut detail =
        SeriesDetailModel::from_resume_movie(&serde_json::from_value(metadata.clone()).unwrap())
            .unwrap();
    detail.item = Some(serde_json::from_value(metadata).unwrap());
    detail.select_media_source(0);
    detail
}

#[test]
fn controls_keep_loaded_choices_visible_but_disable_actions_during_source_refresh() {
    let home = HomeController::new(identity());
    let mut detail = movie();
    detail.effects.resume_sources = LoadState::Loading;
    let saved = SavedTrackChoices::default();
    let view = home.detail_controls(&detail, Default::default(), &saved);
    assert_eq!(view.video_label, "正在加载视频源…");
    assert_eq!(view.media_sources.len(), 2);
    assert_eq!(view.subtitle_streams.len(), 1);
    assert!(!view.media_source_select_enabled && !view.subtitle_select_enabled && !view.can_play);
    assert!(view.actions.enabled);
    assert_eq!(view.playback_position_seconds, Some(30));
    assert!(std::ptr::eq(
        view.media_sources.as_ptr(),
        detail
            .item
            .as_ref()
            .unwrap()
            .media_sources
            .as_ref()
            .unwrap()
            .as_ptr()
    ));
    detail.effects.resume_sources = LoadState::Loaded;
    let view = home.detail_controls(&detail, Default::default(), &saved);
    assert!(view.media_source_select_enabled && view.subtitle_select_enabled && view.can_play);
    detail.playback_loading = true;
    let view = home.detail_controls(&detail, Default::default(), &saved);
    assert!(!view.can_play && view.media_source_select_enabled && view.subtitle_select_enabled);
    detail.playback_loading = false;
    detail
        .item
        .as_mut()
        .unwrap()
        .media_sources
        .as_mut()
        .unwrap()[0]
        .id = Some(" ".into());
    assert!(
        !home
            .detail_controls(&detail, Default::default(), &saved)
            .can_play
    );
    detail.item = None;
    let view = home.detail_controls(&detail, Default::default(), &saved);
    assert!(
        !view.can_play
            && !view.actions.enabled
            && !view.media_source_select_enabled
            && !view.subtitle_select_enabled
    );
}

#[test]
fn detail_actions_use_shared_overrides_and_pending_mutations_for_both_targets() {
    let mut home = HomeController::new(identity());
    let mut detail = SeriesDetailModel::new_series(
        &serde_json::from_value(json!({
            "Id":"series", "Name":"Series", "Type":"Series"
        }))
        .unwrap(),
    );
    detail.item = Some(
        serde_json::from_value(json!({
            "Id":"series", "Name":"Series", "Type":"Series", "UserData":{"Played":false}
        }))
        .unwrap(),
    );
    detail.episodes = Some(
        serde_json::from_value(json!({"Items":[{
            "Id":"episode", "Name":"Episode", "Type":"Episode", "UserData":{"IsFavorite":false}
        }]}))
        .unwrap(),
    );
    detail.selected_episode_id = Some("episode".into());
    home.user_data.overrides.insert(
        "series".into(),
        UserItemData {
            played: true,
            ..Default::default()
        },
    );
    home.user_data.overrides.insert(
        "episode".into(),
        UserItemData {
            is_favorite: true,
            ..Default::default()
        },
    );
    let series = home.detail_actions(&detail, true);
    let episode = home.detail_actions(&detail, false);
    assert!(series.played && !series.favorite && series.enabled);
    assert!(episode.favorite && !episode.played && episode.enabled);
    let command = home
        .dispatch_favorite(FavoriteIntent {
            item_id: "episode".into(),
            fallback: None,
            notification: crate::home::model::notification::ActionNotification {
                scope: crate::home::model::notification::NotificationScope::Detail,
                key: "detail:favorite:episode".into(),
            },
        })
        .unwrap();
    assert!(!home.detail_actions(&detail, true).enabled);
    let episode = home.detail_actions(&detail, false);
    assert!(!episode.favorite && !episode.enabled);
    home.complete_favorite(&command, Err(anyhow::anyhow!("rejected")), &identity())
        .unwrap();
    assert!(home.detail_actions(&detail, false).favorite);
    assert!(home.detail_actions(&detail, true).enabled);
}

#[test]
fn subtitle_drafts_override_saved_choices_only_for_the_selected_version() {
    let home = HomeController::new(identity());
    let mut detail = movie();
    let saved = SavedTrackChoices {
        audio: Some(SavedTrackChoice::Off),
        subtitle: None,
    };
    let key = detail.track_preference_key().unwrap();
    detail
        .pending_subtitle_choices
        .insert(key, SavedTrackChoice::Off);
    let choices = detail.selected_track_choices(saved.clone());
    assert_eq!(choices.audio, saved.audio);
    assert_eq!(choices.subtitle, Some(SavedTrackChoice::Off));
    let view = home.detail_controls(&detail, Default::default(), &choices);
    assert_eq!(view.subtitle_label, "Off");
    assert!(view.selected_subtitle_index.is_none());
    detail.select_media_source(1);
    assert_eq!(detail.selected_track_choices(saved.clone()), saved);
    detail.select_media_source(0);
    assert_eq!(detail.selected_track_choices(saved), choices);
    assert!(detail.pending_subtitle_choice().is_some());
}
