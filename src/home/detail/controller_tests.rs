use super::*;
use crate::{
    emby::{ResumeItem, UserItem},
    home::{
        detail::effect::run_detail,
        gateway::test_support::{Call, FakeMutations, Reply},
    },
};
use serde_json::json;
use std::collections::HashSet;

fn series() -> DetailController {
    let item: UserItem =
        serde_json::from_value(json!({"Id":"series", "Name":"Series", "Type":"Series"})).unwrap();
    DetailController::new(SeriesDetailModel::new_series(&item), Default::default())
}
fn resume() -> DetailController {
    let item: ResumeItem = serde_json::from_value(json!({"Id":"episode", "Name":"Episode", "Type":"Episode", "SeriesId":"series", "ParentId":"season"})).unwrap();
    DetailController::new(
        SeriesDetailModel::from_resume_episode(&item).unwrap(),
        Default::default(),
    )
}
fn empty_items() -> MediaItems {
    serde_json::from_value(json!({"Items":[], "TotalRecordCount":0})).unwrap()
}
fn episodes(id: &str) -> MediaItems {
    serde_json::from_value(json!({"Items":[{"Id":id, "Name":"Episode", "Type":"Episode", "SeasonId":"season", "UserData":{"Played":false, "IsFavorite":false}, "MediaSources":[{"Id":"source"}]}], "TotalRecordCount":1})).unwrap()
}
fn complete(
    controller: &mut DetailController,
    request: &DetailRequest,
    result: anyhow::Result<DetailResponse>,
    data: &mut UserDataState,
) -> Option<DetailUpdate> {
    controller.complete(
        request,
        result,
        &WorkspaceIdentity::default(),
        data,
        PendingUserData {
            played: false,
            favorites: &HashSet::new(),
        },
    )
}

#[test]
fn hidden_same_item_and_other_workspace_cannot_commit_data_or_errors() {
    let mut old = series();
    let request = old
        .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
        .unwrap();
    assert!(
        old.begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
            .is_none()
    );
    let mut current = series();
    let current_request = current
        .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
        .unwrap();
    let mut data = UserDataState::default();
    let item: MediaItem = serde_json::from_value(
        json!({"Id":"series","Name":"Late", "UserData":{"IsFavorite":true}}),
    )
    .unwrap();
    assert!(
        complete(
            &mut current,
            &request,
            Ok(DetailResponse::Item(Box::new(item))),
            &mut data
        )
        .is_none()
    );
    assert!(data.overrides.is_empty());
    assert_eq!(current.view_model().title, "Series");
    let other = WorkspaceIdentity {
        local_server_id: "other".into(),
        ..Default::default()
    };
    assert!(
        current
            .complete(
                &current_request,
                Err(anyhow::anyhow!("foreign")),
                &other,
                &mut data,
                PendingUserData {
                    played: false,
                    favorites: &HashSet::new()
                }
            )
            .is_none()
    );
    old.deactivate();
    assert_eq!(old.view_model().effects.item, LoadState::Idle);
    assert!(
        complete(
            &mut old,
            &request,
            Err(anyhow::anyhow!("hidden")),
            &mut data
        )
        .is_none()
    );
    old.activate();
    let restored = old
        .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
        .unwrap();
    let failure = complete(
        &mut old,
        &restored,
        Err(anyhow::anyhow!("offline")),
        &mut data,
    )
    .unwrap();
    assert_eq!(failure.error.as_deref(), Some("加载媒体详情失败：offline"));
    assert!(!failure.images);
    let retry = old
        .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
        .unwrap();
    assert!(
        complete(
            &mut old,
            &restored,
            Err(anyhow::anyhow!("duplicate")),
            &mut data
        )
        .is_none()
    );
    assert_eq!(old.view_model().effects.item, LoadState::Loading);
    assert!(
        complete(
            &mut old,
            &retry,
            Ok(DetailResponse::Episodes(empty_items())),
            &mut data
        )
        .is_none()
    );
    assert!(
        complete(
            &mut old,
            &retry,
            Err(anyhow::anyhow!("retry failure")),
            &mut data
        )
        .is_some()
    );
}

#[test]
fn returning_to_the_same_season_rejects_its_previous_request_and_duplicate_completion() {
    let mut controller = series();
    controller
        .dispatch(DetailIntent::Season("season".into()))
        .unwrap();
    let old = controller
        .begin(DetailResource::Episodes, WorkspaceIdentity::default(), 0)
        .unwrap();
    controller
        .dispatch(DetailIntent::Season("other".into()))
        .unwrap();
    controller
        .dispatch(DetailIntent::Season("season".into()))
        .unwrap();
    let current = controller
        .begin(DetailResource::Episodes, WorkspaceIdentity::default(), 0)
        .unwrap();
    let mut data = UserDataState::default();
    assert!(
        complete(
            &mut controller,
            &old,
            Ok(DetailResponse::Episodes(episodes("stale"))),
            &mut data
        )
        .is_none()
    );
    assert!(data.overrides.is_empty());
    let update = complete(
        &mut controller,
        &current,
        Ok(DetailResponse::Episodes(episodes("current"))),
        &mut data,
    )
    .unwrap();
    assert!(update.images);
    assert_eq!(
        controller.view_model().selected_episode_id.as_deref(),
        Some("current")
    );
    assert!(
        complete(
            &mut controller,
            &current,
            Err(anyhow::anyhow!("duplicate")),
            &mut data
        )
        .is_none()
    );
    assert_eq!(controller.view_model().effects.episodes, LoadState::Loaded);
}

#[test]
fn series_mutation_retries_an_older_list_without_losing_current_user_data() {
    let mut controller = series();
    controller
        .dispatch(DetailIntent::Season("season".into()))
        .unwrap();
    let old = controller
        .begin(DetailResource::Episodes, WorkspaceIdentity::default(), 0)
        .unwrap();
    let mut data = UserDataState::default();
    data.bump("episode");
    data.series_revisions.insert("series".into(), data.revision);
    data.overrides.insert(
        "episode".into(),
        crate::emby::UserItemData {
            played: true,
            is_favorite: true,
            playback_position_ticks: Some(1234567),
            ..Default::default()
        },
    );
    let update = complete(
        &mut controller,
        &old,
        Ok(DetailResponse::Episodes(episodes("episode"))),
        &mut data,
    )
    .unwrap();
    assert!(update.retry && update.load_episodes);
    assert!(!update.images && update.error.is_none());
    assert!(controller.view_model().episodes.is_none());
    let retry = controller
        .begin(
            DetailResource::Episodes,
            WorkspaceIdentity::default(),
            data.revision,
        )
        .unwrap();
    let favorites = HashSet::from(["episode".into()]);
    controller
        .complete(
            &retry,
            Ok(DetailResponse::Episodes(episodes("episode"))),
            &WorkspaceIdentity::default(),
            &mut data,
            PendingUserData {
                played: false,
                favorites: &favorites,
            },
        )
        .unwrap();
    let episode = controller.view_model().selected_episode().unwrap();
    assert!(episode.user_data.as_ref().unwrap().played);
    assert!(episode.user_data.as_ref().unwrap().is_favorite);
    assert_eq!(
        episode.user_data.as_ref().unwrap().playback_position_ticks,
        Some(1234567)
    );
}

#[test]
fn detail_effect_preserves_endpoint_targets_filtering_and_next_up_fallback() {
    let mut controller = series();
    let similar = serde_json::from_value(json!({"Items":[
        {"Id":"movie", "Name":"Movie", "Type":"Movie", "UserData":{"IsFavorite":true}},
        {"Id":"", "Name":"Invalid", "Type":"Movie"},
        {"Id":"episode", "Name":"Episode", "Type":"Episode"}
    ], "TotalRecordCount":50}))
    .unwrap();
    let gateway = FakeMutations::new(vec![
        Reply::item(Ok(serde_json::from_value(
            json!({"Id":"series", "Name":"Updated title"}),
        )
        .unwrap())),
        Reply::Similar(Ok(similar)),
        Reply::Seasons(Ok(serde_json::from_value(
            json!({"Items":[{"Id":"season", "Name":"Season", "Type":"Season", "IndexNumber":1}]}),
        )
        .unwrap())),
        Reply::NextUp(Err(anyhow::anyhow!("offline"))),
        Reply::Episodes(Ok(episodes("episode"))),
        Reply::Sources(Ok(Vec::new())),
    ]);
    let mut data = UserDataState::default();
    for resource in [
        DetailResource::Item,
        DetailResource::Similar,
        DetailResource::Seasons,
        DetailResource::NextUp,
        DetailResource::Episodes,
    ] {
        let request = controller
            .begin(resource, WorkspaceIdentity::default(), 0)
            .unwrap();
        let response = run_detail(&gateway, &request);
        let update = complete(&mut controller, &request, response, &mut data).unwrap();
        if resource == DetailResource::NextUp {
            assert!(update.load_episodes);
            assert_eq!(update.error.as_deref(), Some("加载下一剧集失败：offline"));
        }
    }
    assert_eq!(controller.view_model().title, "Updated title");
    assert_eq!(
        controller
            .view_model()
            .similar_items
            .as_ref()
            .unwrap()
            .total_record_count,
        1
    );
    assert!(data.overrides["movie"].is_favorite);
    let mut resume = resume();
    let request = resume
        .begin(
            DetailResource::ResumeSources,
            WorkspaceIdentity::default(),
            0,
        )
        .unwrap();
    let response = run_detail(&gateway, &request);
    complete(&mut resume, &request, response, &mut data).unwrap();
    assert_eq!(
        *gateway.calls.lock().unwrap(),
        vec![
            Call::Item("series".into()),
            Call::Similar("series".into()),
            Call::Seasons("series".into()),
            Call::NextUp("series".into()),
            Call::Episodes("series".into(), Some("season".into())),
            Call::Sources("episode".into()),
        ]
    );
}
