use std::sync::Mutex;

use crate::{
    effects::WorkspaceIdentity,
    emby::{ResumeItems, UserItem, UserItems, UserItemsQuery, UserViews, VideoItemType},
    home::{cache::HomeSnapshot, gateway::HomeGateway, model::LoadState},
    persistence::AppPersistence,
    server::CachedServer,
    storage::ServerCache,
};

use super::*;

fn identity(user: &str) -> WorkspaceIdentity {
    WorkspaceIdentity {
        local_server_id: "local".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some(user.into()),
    }
}

fn server() -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "local", "server_id": "remote", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap()
}

fn views(count: usize) -> UserViews {
    serde_json::from_value(serde_json::json!({ "Items": (0..count).map(|index| {
        serde_json::json!({"Id": format!("view-{index}"), "Name": "TV", "CollectionType": "tvshows"})
    }).collect::<Vec<_>>(), "TotalRecordCount": count })).unwrap()
}

fn items() -> Vec<UserItem> {
    serde_json::from_value(serde_json::json!([
        { "Id": "series", "Name": "item", "Type": "Series" },
        { "Id": "episode", "Name": "item", "Type": "Episode", "SeriesId": "series" },
        { "Id": "orphan", "Name": "item", "Type": "Episode" },
        { "Id": "movie", "Name": "item", "Type": "Movie" },
        { "Id": "audio", "Name": "item", "Type": "Audio" },
        { "Id": " ", "Name": "item", "Type": "Series" }
    ]))
    .unwrap()
}

fn cached() -> Box<HomeSnapshot> {
    serde_json::from_value(serde_json::json!({
        "version": 2, "server_id": "local", "remote_server_id": "remote", "user_id": "user", "saved_at_unix": 1,
        "user_views": {"saved_at_unix": 1, "data": views(1)},
        "resume_items": {"saved_at_unix": 1, "data": {"Items": [{"Id":"cached-movie", "Name": "item", "Type":"Movie"}],"TotalRecordCount":1}},
        "latest_items_by_view": {"view-0": {"saved_at_unix": 1, "data": {"Items":items(),"TotalRecordCount":20}}}
    })).unwrap()
}

#[derive(Default)]
struct FakeGateway {
    latest_calls: Mutex<Vec<(String, Vec<VideoItemType>, u32)>>,
}

impl HomeGateway for FakeGateway {
    fn similar_items(&self, _: &str) -> anyhow::Result<crate::emby::UserItems> {
        panic!("unexpected similar request")
    }
    fn show_seasons(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected seasons request")
    }
    fn show_next_up(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected next-up request")
    }
    fn playback_media_sources(&self, _: &str) -> anyhow::Result<Vec<crate::emby::MediaSource>> {
        panic!("unexpected sources request")
    }

    fn set_played(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected played mutation")
    }
    fn media_item(&self, _: &str) -> anyhow::Result<crate::emby::MediaItem> {
        panic!("unexpected media item request")
    }
    fn show_episodes(&self, _: &str, _: Option<&str>) -> anyhow::Result<crate::emby::MediaItems> {
        panic!("unexpected episodes request")
    }

    fn mark_item_played(&self, _: &str) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected resume mutation")
    }
    fn hide_item_from_resume(&self, _: &str) -> anyhow::Result<()> {
        panic!("unexpected resume mutation")
    }

    fn set_favorite(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
        panic!("unexpected favorite mutation")
    }
    fn user_views(&self) -> anyhow::Result<UserViews> {
        Ok(views(6))
    }
    fn resume_items(&self) -> anyhow::Result<ResumeItems> {
        anyhow::bail!("offline")
    }
    fn latest_items(
        &self,
        id: &str,
        types: &[VideoItemType],
        limit: u32,
    ) -> anyhow::Result<Vec<UserItem>> {
        self.latest_calls
            .lock()
            .unwrap()
            .push((id.into(), types.to_vec(), limit));
        Ok(items())
    }
    fn user_items(&self, _: &UserItemsQuery) -> anyhow::Result<UserItems> {
        panic!("wrong endpoint")
    }
    fn search_items(&self, _: &str, _: u32, _: u32) -> anyhow::Result<UserItems> {
        panic!("wrong endpoint")
    }
}

struct MemoryPersistence;
impl AppPersistence for MemoryPersistence {
    fn load_settings(&self) -> anyhow::Result<ServerCache> {
        panic!("feed does not read settings")
    }
    fn save_settings(&self, _: &ServerCache) -> anyhow::Result<()> {
        panic!("feed does not write settings")
    }
    fn load_home(&self, _: &CachedServer) -> anyhow::Result<Option<HomeSnapshot>> {
        Ok(Some(*cached()))
    }
    fn save_home(&self, _: &CachedServer, _: &HomeSnapshot) -> anyhow::Result<()> {
        panic!("controller does not write files")
    }
}

#[test]
fn cache_first_intent_effect_result_flow_preserves_data_on_network_failure() {
    let mut controller = FeedController::new(identity("user"));
    let gateway = FakeGateway::default();
    let request = controller.dispatch(FeedIntent::LoadSnapshot).pop().unwrap();
    assert!(controller.dispatch(FeedIntent::LoadSnapshot).is_empty());
    let response = effect::run_feed(&gateway, &MemoryPersistence, &server(), &request);
    assert!(matches!(
        controller.complete(&request, response),
        FeedUpdate::Snapshot(Some(_))
    ));
    let vm = controller.view_model();
    assert!(vm.show_views && vm.show_resume && vm.has_content);
    assert_eq!(vm.resume.unwrap().items[0].id, "cached-movie");
    let request = controller.dispatch(FeedIntent::LoadResume).pop().unwrap();
    let response = effect::run_feed(&gateway, &MemoryPersistence, &server(), &request);
    assert!(
        matches!(controller.complete(&request, response), FeedUpdate::Failed { message, .. } if message == "刷新失败：offline")
    );
    assert!(controller.view_model().show_resume);
    assert_eq!(controller.state.resume_load, LoadState::Failed);
    assert!(controller.dispatch(FeedIntent::LoadResume).len() == 1);
}

#[test]
fn latest_effect_preserves_endpoint_parameters_filtering_and_four_job_limit() {
    let mut controller = FeedController::new(identity("user"));
    let gateway = FakeGateway::default();
    let request = controller.dispatch(FeedIntent::LoadViews).pop().unwrap();
    let response = effect::run_feed(&gateway, &MemoryPersistence, &server(), &request);
    assert!(matches!(
        controller.complete(&request, response),
        FeedUpdate::Views
    ));
    let first = controller.dispatch(FeedIntent::LoadLatest);
    assert_eq!(first.len(), 4);
    assert!(controller.dispatch(FeedIntent::LoadLatest).is_empty());
    assert!(controller.pump_latest().is_empty());
    for request in &first {
        let response = effect::run_feed(&gateway, &MemoryPersistence, &server(), request);
        assert!(matches!(
            controller.complete(request, response),
            FeedUpdate::Latest(_)
        ));
    }
    let next = controller.pump_latest();
    assert_eq!(next.len(), 2);
    let row = controller.state.user_view_items_rows["view-0"]
        .items
        .as_ref()
        .unwrap();
    assert_eq!(
        row.items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["series", "episode"]
    );
    assert_eq!(row.total_record_count, 2);
    assert_eq!(
        gateway.latest_calls.lock().unwrap()[0],
        (
            "view-0".into(),
            vec![VideoItemType::Series, VideoItemType::Episode],
            30
        )
    );
}

#[test]
fn refresh_retires_old_latest_jobs_without_losing_permits_or_committing_errors() {
    let mut controller = FeedController::new(identity("user"));
    controller.state.user_views = Some(views(6));
    let old = controller.dispatch(FeedIntent::LoadLatest);
    assert_eq!(old.len(), 4);
    let refreshed = controller.dispatch(FeedIntent::Refresh);
    let views_request = refreshed
        .iter()
        .find(|request| request.kind == FeedRequestKind::Views)
        .unwrap();
    controller.complete(views_request, FeedResponse::Views(Ok(views(6))));
    assert!(controller.dispatch(FeedIntent::LoadLatest).is_empty());
    assert!(matches!(
        controller.complete(
            &old[0],
            FeedResponse::Latest(Err(anyhow::anyhow!("old failure")))
        ),
        FeedUpdate::Ignored
    ));
    assert!(controller.state.user_view_items_rows.is_empty());
    let next = controller.pump_latest();
    assert_eq!(next.len(), 1);
    assert_ne!(old[0].token, next[0].token);
    assert!(matches!(
        controller.complete(&old[0], FeedResponse::Latest(Ok(items()))),
        FeedUpdate::Ignored
    ));
    assert!(
        controller.pump_latest().is_empty(),
        "duplicate completion cannot release another permit"
    );
    assert!(controller.state.user_view_items_rows["view-0"].loading);
}

#[test]
fn stale_snapshot_retry_and_cross_account_results_cannot_mutate_current_feed() {
    let mut controller = FeedController::new(identity("user"));
    let snapshot = controller.dispatch(FeedIntent::LoadSnapshot).pop().unwrap();
    let network = controller.dispatch(FeedIntent::Refresh);
    assert!(matches!(
        controller.complete(&snapshot, FeedResponse::Snapshot(Ok(Some(cached())))),
        FeedUpdate::Ignored
    ));
    assert!(controller.state.user_views.is_none());
    let views_request = network
        .iter()
        .find(|request| request.kind == FeedRequestKind::Views)
        .unwrap();
    controller.complete(
        views_request,
        FeedResponse::Views(Err(anyhow::anyhow!("failed"))),
    );
    let retry = controller.dispatch(FeedIntent::LoadViews).pop().unwrap();
    assert!(matches!(
        controller.complete(views_request, FeedResponse::Views(Ok(views(1)))),
        FeedUpdate::Ignored
    ));
    assert_eq!(controller.state.views_load, LoadState::Loading);
    let mut other = FeedController::new(identity("other"));
    other.dispatch(FeedIntent::LoadViews);
    assert!(matches!(
        other.complete(&retry, FeedResponse::Views(Ok(views(1)))),
        FeedUpdate::Ignored
    ));
    assert!(other.state.user_views.is_none());
    assert!(matches!(
        controller.complete(&retry, FeedResponse::Views(Ok(views(1)))),
        FeedUpdate::Views
    ));
}

#[test]
fn playback_refresh_coalesces_resume_load_and_does_not_invalidate_other_scopes() {
    let mut controller = FeedController::new(identity("user"));
    let views = controller.dispatch(FeedIntent::LoadViews).pop().unwrap();
    let resume = controller
        .dispatch(FeedIntent::RefreshResume)
        .pop()
        .unwrap();
    assert!(controller.dispatch(FeedIntent::RefreshResume).is_empty());
    controller.complete(
        &resume,
        FeedResponse::Resume(Ok(serde_json::from_value(
            serde_json::json!({"Items":[],"TotalRecordCount":0}),
        )
        .unwrap())),
    );
    let next = controller
        .dispatch(FeedIntent::RefreshResume)
        .pop()
        .unwrap();
    assert_ne!(next.token, resume.token);
    assert!(controller.accepts(&views));
    assert!(matches!(
        controller.complete(&resume, FeedResponse::Resume(Err(anyhow::anyhow!("old")))),
        FeedUpdate::Ignored
    ));
}

#[test]
fn hydration_filters_invalid_content_and_never_overwrites_network_data() {
    let mut controller = FeedController::new(identity("user"));
    controller.state.user_views = Some(views(2));
    controller.hydrate_sections(&mut cached());
    assert_eq!(controller.state.user_views.as_ref().unwrap().items.len(), 2);
    let items = controller.state.user_view_items_rows["view-0"]
        .items
        .as_ref()
        .unwrap();
    assert_eq!(
        items
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["series", "episode", "movie"]
    );
    assert_eq!(items.total_record_count, 3);
    controller.state.user_views = Some(views(0));
    controller.state.resume_items = None;
    let vm = controller.view_model();
    assert!(vm.show_views && vm.show_empty_views && !vm.show_resume);
    controller.dispatch(FeedIntent::LoadViews);
    assert!(!controller.view_model().show_views);
}
