use super::*;
use crate::effects::DetailResource;
use crate::home::{
    controller::{NavigationIntent, OpenDetailIntent},
    detail::{
        controller::{DetailIntent, DetailResponse},
        playback::{DetailPlaybackUpdate, SelectedPlayback},
    },
};

fn response(id: &str) -> DetailResponse {
    DetailResponse::Item(Box::new(
        serde_json::from_value(json!({
            "Id": id, "Name": "Loaded", "Type": "Movie",
            "MediaSources": [{"Id":"one"}, {"Id":"two"}]
        }))
        .unwrap(),
    ))
}

fn selected(source: &str) -> SelectedPlayback {
    SelectedPlayback {
        detail_id: "movie".into(),
        list_item_id: "movie".into(),
        item_id: "movie".into(),
        media_source_id: source.into(),
        title: "Movie".into(),
        audio_tracks: Vec::new(),
        subtitle_tracks: Vec::new(),
        selected_tracks: Default::default(),
        remember_subtitle_on_start: false,
        run_time_ticks: None,
        playback_position_ticks: None,
        queue: crate::player::PlaybackQueue::new(Vec::new(), 0),
    }
}

#[test]
fn invalid_resume_intent_keeps_the_active_route_and_detail_request() {
    let mut home = HomeController::new(identity());
    home.dispatch_open_detail(
        OpenDetailIntent::Item(&item("movie", "Movie", 0)),
        identity(),
    )
    .unwrap()
    .unwrap();
    let id = home.detail_view().unwrap().id;
    let route = home.route().clone();
    let request = home.begin_detail(DetailResource::Item, identity()).unwrap();
    let incomplete =
        serde_json::from_value(json!({"Id":"episode", "Name":"Episode", "Type":"Episode"}))
            .unwrap();
    assert_eq!(
        home.dispatch_open_detail(OpenDetailIntent::Resume(&incomplete), identity())
            .unwrap_err(),
        "继续观看剧集缺少 SeriesId，无法打开详情"
    );
    assert_eq!(home.detail_view().unwrap().id, id);
    assert_eq!(home.route(), &route);
    assert!(
        home.complete_detail(&request, Ok(response("movie")), &identity())
            .is_some()
    );
    assert_eq!(home.title(), "Loaded");
}

#[test]
fn back_reuses_the_detail_model_but_never_reuses_retired_request_tokens() {
    let mut home = HomeController::new(identity());
    home.dispatch_navigation(NavigationIntent::Root(HomeRoot::Favorites));
    home.dispatch_open_detail(
        OpenDetailIntent::Item(&item("first", "First", 0)),
        identity(),
    )
    .unwrap()
    .unwrap();
    let first_id = home.detail_view().unwrap().id;
    let old = home.begin_detail(DetailResource::Item, identity()).unwrap();
    let change = home
        .dispatch_open_detail(
            OpenDetailIntent::Item(&item("second", "Second", 0)),
            identity(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(change.hidden, Some(first_id));
    assert!(
        home.complete_detail(&old, Ok(response("first")), &identity())
            .is_none()
    );
    let back = home.dispatch_navigation(NavigationIntent::Back);
    assert_eq!(back.removed.len(), 1);
    assert_eq!(home.detail_view().unwrap().id, first_id);
    assert_eq!(home.root(), HomeRoot::Favorites);
    assert!(
        home.complete_detail(&old, Ok(response("first")), &identity())
            .is_none()
    );
    let current = home.begin_detail(DetailResource::Item, identity()).unwrap();
    assert_ne!(old.token, current.token);
    assert!(
        home.complete_detail(&current, Ok(response("first")), &identity())
            .is_some()
    );
    let exit = home.dispatch_navigation(NavigationIntent::Root(HomeRoot::Search));
    assert_eq!(exit.removed.len(), 1);
    assert!(home.detail_view().is_none());
    assert!(
        home.complete_detail(&current, Ok(response("first")), &identity())
            .is_none()
    );
}

#[test]
fn selection_intent_cancels_prepared_playback_and_current_account_still_gates_completion() {
    let mut home = HomeController::new(identity());
    home.dispatch_open_detail(
        OpenDetailIntent::Item(&item("movie", "Movie", 0)),
        identity(),
    )
    .unwrap()
    .unwrap();
    let request = home.begin_detail(DetailResource::Item, identity()).unwrap();
    home.complete_detail(&request, Ok(response("movie")), &identity())
        .unwrap();
    let old = home
        .begin_detail_playback(Ok(selected("one")), identity())
        .unwrap()
        .unwrap();
    assert!(home.detail_view().unwrap().model.playback_loading);
    let change = home.dispatch_detail(DetailIntent::MediaSource(1)).unwrap();
    assert!(change.playback_cancelled);
    assert!(!home.detail_view().unwrap().model.playback_loading);
    assert!(
        home.complete_detail_playback(old, Err(anyhow::anyhow!("stale")), &identity())
            .is_none()
    );
    assert!(home.detail_view().unwrap().model.playback_failed.is_none());
    let current = home
        .begin_detail_playback(Ok(selected("two")), identity())
        .unwrap()
        .unwrap();
    let mut other = identity();
    other.user_id = Some("other".into());
    assert!(
        home.complete_detail_playback(
            current.clone(),
            Err(anyhow::anyhow!("wrong account")),
            &other
        )
        .is_none()
    );
    assert!(home.detail_view().unwrap().model.playback_loading);
    assert!(
        matches!(home.complete_detail_playback(current, Err(anyhow::anyhow!("offline")), &identity()), Some(DetailPlaybackUpdate::Failed(message)) if message == "获取播放地址失败：offline")
    );
    assert!(!home.detail_view().unwrap().model.playback_loading);
}
