use super::*;
use crate::{
    emby::{
        ResumeItems, UserItem, UserItemData, UserItems, UserItemsSort, UserView, VideoItemType,
    },
    home::{
        cache::HomeSnapshot,
        detail::controller::DetailController,
        feed::UserViewItemsRow,
        library::controller::LibraryIntent,
        model::{LoadState, navigation::HomeRoot},
    },
    player::{PlaybackStateUpdate, PlaybackStopResult, SavedTrackChoice, SavedTrackChoices},
};
use serde_json::json;

mod boundaries;
mod cards;
mod detail_controls;
mod navigation;

fn identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        local_server_id: "local".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some("user".into()),
    }
}

fn item(id: &str, name: &str, position: u64) -> UserItem {
    serde_json::from_value(json!({
        "Id": id, "Name": name, "Type": "Movie",
        "UserData": {"PlaybackPositionTicks": position, "IsFavorite": true}
    }))
    .unwrap()
}

fn items(name: &str, position: u64) -> UserItems {
    UserItems {
        items: vec![item("movie", name, position)],
        total_record_count: 1,
    }
}

fn resume() -> ResumeItems {
    serde_json::from_value(json!({
        "Items": [
            {"Id":"movie", "Name":"Resume", "Type":"Movie", "UserData":{"PlaybackPositionTicks":20}},
            {"Id":"physical", "Name":"Physical", "Type":"Movie"},
            {"Id":"other", "Name":"Other", "Type":"Movie"}
        ], "TotalRecordCount":9
    })).unwrap()
}

fn view() -> UserView {
    serde_json::from_value(json!({"Id":"library", "Name":"Movies", "CollectionType":"movies"}))
        .unwrap()
}

fn playback() -> PlaybackStateUpdate {
    PlaybackStateUpdate {
        item_id: "physical".into(),
        list_item_id: "movie".into(),
        media_source_id: "source".into(),
        media_source_name: Some("2160p".into()),
        series_id: None,
        season_id: None,
        position_ticks: 123_456_789,
        run_time_ticks: Some(987_654_321),
        ended: false,
        failed: false,
        selected_item_id: None,
        stop_completion: None,
    }
}

fn track(index: usize) -> SavedTrackChoice {
    SavedTrackChoice::Track {
        stream_index: index,
        label: "Saved".into(),
        codec: None,
        is_external: false,
    }
}

#[test]
fn lookup_prioritizes_the_visible_list_and_playback_uses_the_original_data_order() {
    let mut home = HomeController::new(identity());
    home.feed.state.user_view_items_rows.insert(
        "latest".into(),
        UserViewItemsRow {
            items: Some(items("Latest", 30)),
            loading: false,
        },
    );
    home.feed.state.resume_items = Some(resume());
    home.search.test_state_mut().items = items("Search", 60).items;
    home.favorites.test_state_mut(VideoItemType::Movie).items = items("Favorite", 50).items;
    home.open_library(&view()).unwrap();
    home.libraries
        .get_mut("library")
        .unwrap()
        .test_paged_mut()
        .items = items("Library", 40).items;
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Library");
    home.navigation.select_root(HomeRoot::Search);
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Search");
    home.navigation.select_root(HomeRoot::Favorites);
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Favorite");
    home.navigation.select_root(HomeRoot::Home);
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Latest");

    let mut detail =
        DetailController::from_user_item(&item("movie", "Detail", 10), identity()).unwrap();
    detail.state.item = Some(
        serde_json::from_value(json!({
            "Id":"movie", "Name":"Detail", "Type":"Movie", "UserData":{"PlaybackPositionTicks":10}
        }))
        .unwrap(),
    );
    detail.state.similar_items = Some(items("Similar", 70));
    home.navigation
        .open_detail(detail, &home.played_video_versions);
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Similar");
    home.user_data.overrides.insert(
        "movie".into(),
        UserItemData {
            playback_position_ticks: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(1)
    );
    assert!(!home.user_item_by_id("movie").unwrap().is_favorite());
    home.user_data.overrides.clear();
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(10)
    );
    home.navigation.select_root(HomeRoot::Home);
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(20)
    );
    home.feed.state.resume_items = None;
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(30)
    );
    home.feed.state.user_view_items_rows.clear();
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(40)
    );
    home.libraries.clear();
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(50)
    );
    home.favorites
        .test_state_mut(VideoItemType::Movie)
        .items
        .clear();
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(60)
    );
    home.search.test_state_mut().items.clear();
    assert!(home.loaded_playback_user_data("movie").is_none());
}

#[test]
fn playback_updates_both_ids_once_preserves_track_choices_and_fences_older_reads() {
    let mut home = HomeController::new(identity());
    home.feed.state.resume_items = Some(resume());
    home.user_data.overrides.insert(
        "movie".into(),
        UserItemData {
            is_favorite: true,
            unplayed_item_count: Some(7),
            ..Default::default()
        },
    );
    let choices = SavedTrackChoices {
        audio: Some(track(6)),
        subtitle: Some(SavedTrackChoice::Off),
    };
    home.played_video_versions.insert(
        "movie".into(),
        VideoVersion {
            source_id: "old".into(),
            track_preferences: HashMap::from([("source".into(), choices.clone())]),
            ..Default::default()
        },
    );
    let mut detail =
        DetailController::from_user_item(&item("movie", "Movie", 0), identity()).unwrap();
    detail.state.item = Some(
        serde_json::from_value(json!({
            "Id":"movie", "Name":"Movie", "Type":"Movie"
        }))
        .unwrap(),
    );
    let id = detail.id();
    home.navigation
        .open_detail(detail, &home.played_video_versions);
    let update = playback();
    assert_eq!(home.apply_playback_update(&update).unwrap().0, id);
    assert_eq!(home.user_data.revision, 1);
    for id in ["movie", "physical"] {
        let data = &home.user_data.overrides[id];
        assert_eq!(data.playback_position_ticks, Some(123_456_789));
        assert!(data.is_favorite);
        assert_eq!(data.unplayed_item_count, Some(7));
        assert_eq!(home.user_data.item_revisions[id], 1);
        assert_eq!(home.played_video_versions[id].source_id, "source");
        assert_eq!(
            home.played_video_versions[id].name.as_deref(),
            Some("2160p")
        );
    }
    assert_eq!(
        home.played_video_versions["movie"].track_preferences["source"],
        choices
    );
    for item in &home.feed.state.resume_items.as_ref().unwrap().items[..2] {
        assert_eq!(
            item.user_data,
            Some(home.user_data.overrides["movie"].clone())
        );
    }
    assert_eq!(
        home.navigation
            .detail()
            .unwrap()
            .view_model()
            .playback_user_data("movie")
            .unwrap(),
        &home.user_data.overrides["movie"]
    );
    home.absorb_user_items_user_data(&items("Stale", 0), 0);
    assert_eq!(
        home.user_data.overrides["movie"].playback_position_ticks,
        Some(123_456_789)
    );
}

#[test]
fn failed_or_empty_playback_keeps_versions_and_completion_removes_both_resume_ids() {
    for (failed, position, source) in [(true, 50, "new"), (false, 0, "new"), (false, 50, "")] {
        let mut home = HomeController::new(identity());
        home.played_video_versions.insert(
            "movie".into(),
            VideoVersion {
                source_id: "old".into(),
                ..Default::default()
            },
        );
        let mut update = playback();
        update.failed = failed;
        update.position_ticks = position;
        update.media_source_id = source.into();
        home.apply_playback_update(&update);
        assert_eq!(home.played_video_versions["movie"].source_id, "old");
        assert!(!home.played_video_versions.contains_key("physical"));
    }
    let mut home = HomeController::new(identity());
    home.feed.state.resume_items = Some(resume());
    let mut update = playback();
    update.ended = true;
    home.apply_playback_update(&update);
    let resume = home.feed.state.resume_items.as_ref().unwrap();
    assert_eq!(resume.items.len(), 1);
    assert_eq!(resume.items[0].id, "other");
    assert_eq!(resume.total_record_count, 9);
    for id in ["movie", "physical"] {
        let data = &home.user_data.overrides[id];
        assert!(data.played);
        assert_eq!(data.playback_position_ticks, Some(0));
        assert_eq!(data.played_percentage, Some(100.0));
    }
}

#[test]
fn stop_refresh_accepts_only_latest_success_once_and_preserves_in_flight_detail_reads() {
    let mut home = HomeController::new(identity());
    let mut detail =
        DetailController::from_user_item(&item("movie", "Movie", 0), identity()).unwrap();
    detail.state.effects.item = LoadState::Loaded;
    detail.state.effects.episodes = LoadState::Loading;
    home.navigation
        .open_detail(detail, &home.played_video_versions);
    let old = home.begin_playback_refresh();
    let current = home.begin_playback_refresh();
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..identity()
    };
    assert!(!home.complete_playback_refresh(&old, &identity(), PlaybackStopResult::Succeeded));
    assert!(!home.complete_playback_refresh(&current, &foreign, PlaybackStopResult::Succeeded));
    assert_eq!(
        home.navigation.detail().unwrap().state.effects.item,
        LoadState::Loaded
    );
    assert!(home.complete_playback_refresh(&current, &identity(), PlaybackStopResult::Succeeded));
    assert!(!home.complete_playback_refresh(&current, &identity(), PlaybackStopResult::Succeeded));
    assert_eq!(
        home.navigation.detail().unwrap().state.effects.item,
        LoadState::Idle
    );
    assert_eq!(
        home.navigation.detail().unwrap().state.effects.episodes,
        LoadState::Loading
    );
    let failed = home.begin_playback_refresh();
    assert!(!home.complete_playback_refresh(&failed, &identity(), PlaybackStopResult::Failed));
    assert!(!home.complete_playback_refresh(&failed, &identity(), PlaybackStopResult::Succeeded));
}

#[test]
fn library_result_is_fenced_before_overlay_and_image_effects() {
    let mut home = HomeController::new(identity());
    let (_, first) = home.open_library(&view()).unwrap();
    let first = first.request.unwrap();
    let current = home
        .dispatch_library("library", LibraryIntent::SortBy(UserItemsSort::DateCreated))
        .unwrap()
        .request
        .unwrap();
    assert!(
        home.complete_library(&first, Ok(items("Old", 1)), &identity())
            .is_none()
    );
    assert!(home.user_data.overrides.is_empty());
    home.user_data.bump("movie");
    home.user_data.overrides.insert(
        "movie".into(),
        UserItemData {
            playback_position_ticks: Some(99),
            ..Default::default()
        },
    );
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..identity()
    };
    assert!(
        home.complete_library(&current, Ok(items("Foreign", 2)), &foreign)
            .is_none()
    );
    let accepted = home
        .complete_library(&current, Ok(items("Current", 3)), &identity())
        .unwrap();
    assert_eq!(accepted.images.unwrap().items[0].name, "Current");
    assert_eq!(
        home.user_item_by_id("movie")
            .unwrap()
            .user_data
            .unwrap()
            .playback_position_ticks,
        Some(99)
    );
    assert!(
        home.complete_library(&current, Ok(items("Duplicate", 4)), &identity())
            .is_none()
    );
    assert!(
        home.open_library(&UserView {
            collection_type: Some("music".into()),
            ..view()
        })
        .is_none()
    );
    assert_eq!(home.navigation.title(), "Movies");
}

#[test]
fn late_snapshot_fills_missing_data_without_replacing_live_versions_or_track_choices() {
    let mut home = HomeController::new(identity());
    home.feed.state.user_view_items_rows.insert(
        "library".into(),
        UserViewItemsRow {
            items: Some(items("Live", 9)),
            loading: false,
        },
    );
    home.played_video_versions.insert(
        "movie".into(),
        VideoVersion {
            source_id: "live".into(),
            name: Some("Live".into()),
            track_preferences: HashMap::from([(
                "source".into(),
                SavedTrackChoices {
                    audio: Some(track(9)),
                    subtitle: None,
                },
            )]),
        },
    );
    let mut snapshot = HomeSnapshot::from_data(
        &identity(),
        7,
        None,
        Some(resume()),
        HashMap::from([("library".into(), items("Cached", 1))]),
    );
    snapshot.played_video_versions.insert(
        "movie".into(),
        VideoVersion {
            source_id: "cached".into(),
            name: Some("Cached".into()),
            track_preferences: HashMap::from([(
                "source".into(),
                SavedTrackChoices {
                    audio: Some(track(1)),
                    subtitle: Some(track(2)),
                },
            )]),
        },
    );
    home.hydrate_snapshot(snapshot);
    assert!(home.feed.state.resume_items.is_some());
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Live");
    assert_eq!(home.played_video_versions["movie"].source_id, "live");
    assert_eq!(
        home.played_video_versions["movie"].name.as_deref(),
        Some("Live")
    );
    let choices = &home.played_video_versions["movie"].track_preferences["source"];
    assert_eq!(choices.audio, Some(track(9)));
    assert_eq!(choices.subtitle, Some(track(2)));
    let choices = HashMap::from([(
        "movie".into(),
        HashMap::from([(
            "source".into(),
            SavedTrackChoices {
                audio: None,
                subtitle: Some(SavedTrackChoice::Off),
            },
        )]),
    )]);
    assert!(home.merge_track_preferences(&choices));
    assert!(!home.merge_track_preferences(&choices));
    let exported = home.track_preferences().collect::<Vec<_>>();
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].0.item_id, "movie");
    assert_eq!(exported[0].1.audio, Some(track(9)));
    assert_eq!(exported[0].1.subtitle, Some(SavedTrackChoice::Off));
}

#[test]
fn snapshot_exports_effective_data_with_unchanged_schema_without_mutating_loaded_items() {
    let mut home = HomeController::new(identity());
    home.feed.state.resume_items = Some(resume());
    home.feed.state.user_view_items_rows.insert(
        "library".into(),
        UserViewItemsRow {
            items: Some(items("Latest", 1)),
            loading: false,
        },
    );
    home.user_data.overrides.insert(
        "movie".into(),
        UserItemData {
            playback_position_ticks: Some(123_456_789),
            is_favorite: false,
            ..Default::default()
        },
    );
    home.played_video_versions.insert(
        "movie".into(),
        VideoVersion {
            source_id: "source".into(),
            ..Default::default()
        },
    );
    let snapshot = home.snapshot(42);
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["version"], 2);
    assert_eq!(json["server_id"], "local");
    assert_eq!(json["remote_server_id"], "remote");
    assert_eq!(json["user_id"], "user");
    assert_eq!(json["saved_at_unix"], 42);
    assert_eq!(json["resume_items"]["saved_at_unix"], 42);
    assert_eq!(json["latest_items_by_view"]["library"]["saved_at_unix"], 42);
    assert_eq!(
        snapshot.resume_items.unwrap().data.items[0].user_data,
        Some(home.user_data.overrides["movie"].clone())
    );
    assert_eq!(
        snapshot.latest_items_by_view["library"].data.items[0].user_data,
        Some(home.user_data.overrides["movie"].clone())
    );
    assert_eq!(snapshot.played_video_versions["movie"].source_id, "source");
    assert_eq!(
        home.feed.state.resume_items.as_ref().unwrap().items[0]
            .user_data
            .as_ref()
            .unwrap()
            .playback_position_ticks,
        Some(20)
    );
    assert!(
        home.feed.state.user_view_items_rows["library"]
            .items
            .as_ref()
            .unwrap()
            .items[0]
            .is_favorite()
    );
}
