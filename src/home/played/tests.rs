use super::*;
use crate::emby::{ResumeItems, UserItem};
use crate::home::{
    gateway::test_support::{Call, FakeMutations, Reply},
    model::{
        detail::SeriesDetailModel,
        notification::{ActionNotification, NotificationScope},
    },
    played::effect::run_played,
};
use serde_json::json;

fn request(whole_series: bool) -> PlayedRequest {
    PlayedRequest {
        item_id: if whole_series { "series" } else { "episode" }.into(),
        series_id: Some("series".into()),
        whole_series,
        played: true,
        season_id: Some("season".into()),
        episode_id: Some("episode".into()),
        detail_activation: None,
        user_data: None,
        notification: ActionNotification {
            scope: NotificationScope::Detail,
            key: "detail:played".into(),
        },
    }
}

fn episode(id: &str, played: bool, percentage: f64) -> MediaItem {
    serde_json::from_value(json!({"Id": id, "Name": id, "Type": "Episode", "SeriesId": "series", "UserData": {"Played": played, "PlayedPercentage": percentage, "PlaybackPositionTicks": 50}})).unwrap()
}

fn detail_model() -> SeriesDetailModel {
    let item: UserItem =
        serde_json::from_value(json!({"Id": "series", "Name": "Series", "Type": "Series"}))
            .unwrap();
    let mut detail = SeriesDetailModel::new_series(&item);
    detail.selected_season_id = Some("season".into());
    detail.selected_episode_id = Some("episode".into());
    detail.item = Some(serde_json::from_value(json!({"Id": "series", "Name": "Series", "Type": "Series", "UserData": {"IsFavorite": true}})).unwrap());
    detail.episodes = Some(MediaItems {
        items: vec![
            episode("episode", false, 33.0),
            episode("other", false, 37.5),
        ],
        total_record_count: 2,
    });
    detail
}

fn empty_content(resume: &mut Option<ResumeItems>) -> PlayedContent<'_> {
    PlayedContent {
        current: None,
        history: Vec::new(),
        items: Vec::new(),
        resume,
        detail_activation: None,
    }
}

#[test]
fn failed_played_write_skips_followups_and_duplicate_result_cannot_finish_a_retry() {
    let identity = WorkspaceIdentity::default();
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..Default::default()
    };
    let mut controller = PlayedController::new(identity.clone());
    assert!(controller.begin(request(true), true).is_none());
    let first = controller.begin(request(true), false).unwrap();
    assert!(controller.begin(request(false), false).is_none());
    assert!(
        controller
            .accept(first.clone(), Err(anyhow::anyhow!("foreign")), &foreign)
            .is_none()
    );
    assert!(controller.has_pending());
    let gateway = FakeMutations::new(vec![Reply::UserData(Err(anyhow::anyhow!("offline")))]);
    let accepted = controller
        .accept(first.clone(), run_played(&gateway, &first), &identity)
        .unwrap();
    assert!(!accepted.succeeded() && !controller.has_pending());
    let mut resume = None;
    let mut data = UserDataState::default();
    let update = accepted.apply(empty_content(&mut resume), &mut data);
    assert!(!update.changed);
    assert_eq!(update.notification.key, "detail:played");
    assert_eq!(update.errors, ["更新观看状态失败：offline"]);
    assert!(data.overrides.is_empty() && data.revision == 0);
    assert_eq!(
        *gateway.calls.lock().unwrap(),
        [Call::SetPlayed("series".into(), true)]
    );
    let retry = controller.begin(request(true), false).unwrap();
    assert!(
        controller
            .accept(first, Err(anyhow::anyhow!("duplicate")), &identity)
            .is_none()
    );
    assert!(controller.has_pending());
    controller
        .accept(
            retry.clone(),
            Err(anyhow::anyhow!("failed again")),
            &identity,
        )
        .unwrap();
    assert!(
        controller
            .accept(retry, Err(anyhow::anyhow!("duplicate")), &identity)
            .is_none()
    );
}

#[test]
fn whole_series_keeps_unrefreshed_episode_progress_and_still_fetches_selected_item_after_list_failure()
 {
    let identity = WorkspaceIdentity::default();
    let mut controller = PlayedController::new(identity.clone());
    let detail_controller =
        crate::home::detail::controller::DetailController::new(detail_model(), identity.clone());
    let activation = detail_controller.activation().cloned();
    let mut request = request(true);
    request.detail_activation = activation.clone();
    let command = controller.begin(request, false).unwrap();
    let gateway = FakeMutations::new(vec![
        Reply::UserData(Ok(UserItemData {
            played: true,
            ..Default::default()
        })),
        Reply::Episodes(Err(anyhow::anyhow!("list unavailable"))),
        Reply::item(Ok(episode("episode", true, 18.0))),
    ]);
    let completion = controller
        .accept(command.clone(), run_played(&gateway, &command), &identity)
        .unwrap();
    assert!(completion.succeeded());
    let mut detail = detail_model();
    let mut data = UserDataState::default();
    let mut resume = None;
    let update = completion.apply(
        PlayedContent {
            current: Some(&mut detail),
            detail_activation: activation,
            ..empty_content(&mut resume)
        },
        &mut data,
    );
    assert!(update.changed);
    assert_eq!(
        update.errors,
        ["观看状态已更新，刷新失败（分集列表：list unavailable）"]
    );
    assert!(data.overrides["series"].played && data.overrides["series"].is_favorite);
    assert_eq!(data.overrides["episode"].played_percentage, Some(18.0));
    assert!(!data.overrides.contains_key("other"));
    assert!(data.item_revisions.contains_key("other"));
    assert!(data.series_revisions.contains_key("series"));
    let episodes = &detail.episodes.as_ref().unwrap().items;
    assert_eq!(
        episodes[1].user_data.as_ref().unwrap().played_percentage,
        Some(37.5)
    );
    assert!(!episodes[1].user_data.as_ref().unwrap().played);
    assert_eq!(
        *gateway.calls.lock().unwrap(),
        [
            Call::SetPlayed("series".into(), true),
            Call::Episodes("series".into(), Some("season".into())),
            Call::Item("episode".into())
        ]
    );
}

#[test]
fn selected_item_response_wins_and_navigation_fence_preserves_the_new_detail_list() {
    let identity = WorkspaceIdentity::default();
    let mut controller = PlayedController::new(identity.clone());
    let mut detail_controller =
        crate::home::detail::controller::DetailController::new(detail_model(), identity.clone());
    let mut request = request(true);
    request.detail_activation = detail_controller.activation().cloned();
    detail_controller.deactivate();
    detail_controller.activate();
    let activation = detail_controller.activation().cloned();
    assert_ne!(request.detail_activation, activation);
    let command = controller.begin(request, false).unwrap();
    let mut missing = episode("other", false, 9.0);
    missing.user_data = None;
    let gateway = FakeMutations::new(vec![
        Reply::UserData(Ok(UserItemData {
            played: true,
            ..Default::default()
        })),
        Reply::Episodes(Ok(MediaItems {
            items: vec![episode("episode", true, 100.0), missing],
            total_record_count: 2,
        })),
        Reply::item(Ok(episode("episode", false, 18.0))),
    ]);
    let completion = controller
        .accept(command.clone(), run_played(&gateway, &command), &identity)
        .unwrap();
    let mut detail = detail_model();
    detail.episodes = Some(MediaItems {
        items: vec![episode("new", false, 45.0)],
        total_record_count: 1,
    });
    let mut history = detail_model();
    let mut data = UserDataState::default();
    data.overrides.insert(
        "other".into(),
        UserItemData {
            played: true,
            ..Default::default()
        },
    );
    let mut resume = Some(serde_json::from_value(json!({"Items":[{"Id":"episode","Name":"Episode","Type":"Episode","SeriesId":"series"}],"TotalRecordCount":5})).unwrap());
    let update = completion.apply(
        PlayedContent {
            current: Some(&mut detail),
            history: vec![&mut history],
            items: Vec::new(),
            resume: &mut resume,
            detail_activation: activation,
        },
        &mut data,
    );
    assert!(update.changed && update.errors.is_empty());
    assert_eq!(detail.episodes.as_ref().unwrap().items[0].id, "new");
    assert!(!data.overrides["episode"].played);
    assert_eq!(data.overrides["episode"].played_percentage, Some(18.0));
    assert!(!data.overrides.contains_key("other"));
    assert_eq!(
        history.episodes.as_ref().unwrap().items[0]
            .user_data
            .as_ref()
            .unwrap()
            .played_percentage,
        Some(18.0)
    );
    // The earlier server list removed this resume card; the final item read
    // refines data but does not reinsert a card, preserving the original order.
    assert_eq!(resume.as_ref().unwrap().total_record_count, 4);
    assert!(resume.as_ref().unwrap().items.is_empty());
}

#[test]
fn single_episode_preserves_response_progress_and_reports_parent_refresh_failure() {
    let identity = WorkspaceIdentity::default();
    let mut controller = PlayedController::new(identity.clone());
    let command = controller.begin(request(false), false).unwrap();
    let gateway = FakeMutations::new(vec![
        Reply::UserData(Ok(UserItemData {
            played: true,
            playback_position_ticks: Some(42),
            played_percentage: Some(18.0),
            ..Default::default()
        })),
        Reply::item(Err(anyhow::anyhow!("parent unavailable"))),
    ]);
    let completion = controller
        .accept(command.clone(), run_played(&gateway, &command), &identity)
        .unwrap();
    let mut data = UserDataState::default();
    data.overrides.insert(
        "episode".into(),
        UserItemData {
            is_favorite: true,
            ..Default::default()
        },
    );
    let mut resume = None;
    let update = completion.apply(empty_content(&mut resume), &mut data);
    assert!(update.changed && data.overrides["episode"].is_favorite);
    assert_eq!(data.overrides["episode"].playback_position_ticks, Some(42));
    assert_eq!(data.overrides["episode"].played_percentage, Some(18.0));
    assert_eq!(
        update.errors,
        ["观看状态已更新，刷新整部剧状态失败：parent unavailable"]
    );
    assert_eq!(
        *gateway.calls.lock().unwrap(),
        [
            Call::SetPlayed("episode".into(), true),
            Call::Item("series".into())
        ]
    );
}
