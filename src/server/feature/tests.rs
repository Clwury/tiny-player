use std::sync::Mutex;

use super::{gateway::ServerGateway, *};
use crate::{
    config::GlobalConfig,
    emby::{AuthSession, ItemCounts, PublicSystemInfo},
    server::{AddServerSubmission, CachedServer},
    storage::ServerCache,
};
use anyhow::Result;

mod icons;
mod persistence;

fn server(id: &str, authenticated: bool) -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": id, "server_id": "remote", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": id, "password": "", "added_at_unix": 0,
        "server_name": "Cached name", "access_token": authenticated.then_some("token")
    }))
    .unwrap()
}

fn controller() -> ServerController {
    ServerController::new(ServerCatalog {
        servers: vec![
            server("one", true),
            server("two", false),
            server("three", true),
        ],
        auto_start_server_id: None,
    })
}

#[test]
fn sidebar_projection_preserves_catalog_order_and_exact_title_fallback() {
    let mut controller = controller();
    controller.catalog.servers[0].server_name = None;
    controller.catalog.servers[1].server_name = Some(String::new());
    controller.catalog.servers[2].server_name = Some("  ".into());
    controller.catalog.servers[2].icon_url = Some("https://example.com/icon.png".into());
    // An in-progress grid drag must not publish its preview as the saved order.
    controller.begin_reorder("one");
    controller.preview_reorder(2);

    let rows = controller.sidebar_servers();
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["one", "two", "three"]
    );
    assert_eq!(
        rows.iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["example.com", "example.com", "  "]
    );
    assert_eq!(
        rows[2].icon_url.as_deref(),
        Some("https://example.com/icon.png")
    );
}

#[derive(Default)]
struct FakeGateway {
    calls: Mutex<Vec<&'static str>>,
    fail_login: bool,
}
impl ServerGateway for FakeGateway {
    fn matched_icon_url(&self, name: &str) -> Option<String> {
        crate::server::icon::match_icon_url(name).map(str::to_owned)
    }
    fn public_system_info(&self, _: &AddServerSubmission) -> Result<PublicSystemInfo> {
        self.calls.lock().unwrap().push("public");
        Ok(PublicSystemInfo {
            id: Some("remote".into()),
            server_name: Some("Fresh name".into()),
        })
    }
    fn authenticate_by_name(&self, _: &AddServerSubmission) -> Result<AuthSession> {
        self.calls.lock().unwrap().push("auth");
        anyhow::ensure!(!self.fail_login, "rejected");
        Ok(serde_json::from_value(
            serde_json::json!({ "AccessToken": "new-token", "User": {"Id":"user"} }),
        )?)
    }
    fn item_counts(&self, _: &CachedServer) -> Result<ItemCounts> {
        self.calls.lock().unwrap().push("counts");
        Ok(ItemCounts {
            movie_count: 12,
            series_count: 34,
            ..Default::default()
        })
    }
}

#[test]
fn selection_coalesces_and_cancelled_same_server_cannot_finish_new_attempt() {
    let mut controller = controller();
    controller.enter_workspace(Some("one".into()));
    let ServerCommand::Authenticate(old) = controller.select("two") else {
        panic!("authentication required")
    };
    assert!(matches!(controller.select("two"), ServerCommand::Ignored));
    assert_eq!(controller.selecting_server_id(), Some("two"));
    assert!(matches!(controller.select("one"), ServerCommand::Cancelled));
    let ServerCommand::Authenticate(current) = controller.select("two") else {
        panic!("authentication required")
    };
    assert!(matches!(
        controller.finish_authentication(&old, Err(anyhow::anyhow!("old"))),
        AuthResult::Ignored
    ));
    assert_eq!(controller.selecting_server_id(), Some("two"));
    assert!(matches!(
        controller.finish_authentication(&current, Err(anyhow::anyhow!("current"))),
        AuthResult::Ready(Err(_))
    ));
    assert!(controller.selecting_server_id().is_none());
    assert!(matches!(controller.select("three"), ServerCommand::Open(_)));
}

#[test]
fn authentication_effect_preserves_metadata_policy_and_credential_fence() {
    let mut controller = controller();
    let gateway = FakeGateway::default();
    let ServerCommand::Authenticate(request) = controller.select("two") else {
        panic!("authentication required")
    };
    let result = effect::authenticate_server(&gateway, &request.server).unwrap();
    assert_eq!(*gateway.calls.lock().unwrap(), ["auth"]);
    assert_eq!(result.server_name.as_deref(), Some("Cached name"));
    controller.catalog.servers[1].password = "changed".into();
    assert!(matches!(
        controller.finish_authentication(&request, Ok(result)),
        AuthResult::Invalidated
    ));
    assert!(controller.catalog.servers[1].access_token.is_none());
    assert_eq!(controller.catalog.servers[1].password, "changed");

    controller.catalog.servers[1].needs_auth_refresh = true;
    let ServerCommand::Authenticate(request) = controller.select("two") else {
        panic!("authentication required")
    };
    let result = effect::authenticate_server(&gateway, &request.server).unwrap();
    assert_eq!(result.server_name.as_deref(), Some("Fresh name"));
    assert_eq!(*gateway.calls.lock().unwrap(), ["auth", "auth", "public"]);
    assert!(matches!(
        controller.finish_authentication(&request, Ok(result)),
        AuthResult::Ready(Ok(_))
    ));
}

#[test]
fn edited_or_cancelled_submission_never_commits_a_late_result() {
    let mut controller = controller();
    let submission = AddServerSubmission {
        endpoint: controller.catalog.servers[0].endpoint.clone(),
        username: "edited".into(),
        password: "".into(),
    };
    let old = controller
        .prepare_submission(submission.clone(), Some("one"))
        .unwrap();
    controller.cancel_submission();
    let current = controller
        .prepare_submission(submission, Some("one"))
        .unwrap();
    assert!(!controller.finish_submission(&old));
    let gateway = FakeGateway::default();
    let edited =
        effect::prepare_server(&gateway, &current.submission, current.existing.as_ref()).unwrap();
    assert!(
        gateway.calls.lock().unwrap().is_empty(),
        "editing defers login and metadata refresh"
    );
    assert_eq!(edited.id, "one");
    assert!(edited.needs_auth_refresh);
    assert!(controller.finish_submission(&current));
    assert!(!controller.finish_submission(&current));
}

#[test]
fn count_retry_rejects_old_errors_without_clearing_current_loading() {
    let mut controller = controller();
    let gateway = FakeGateway::default();
    let old = controller.begin_counts("one").unwrap();
    assert!(controller.begin_counts("one").is_none());
    controller.reset_counts("one");
    let current = controller.begin_counts("one").unwrap();
    assert!(matches!(
        controller.finish_counts(&old, Err(anyhow::anyhow!("late"))),
        CountResult::Ignored
    ));
    assert!(controller.counts_loading("one"));
    assert!(controller.counts_failed.is_empty());
    let counts = gateway.item_counts(&current.server).unwrap();
    assert!(matches!(
        controller.finish_counts(&current, Ok(counts)),
        CountResult::Saved
    ));
    assert!(!controller.counts_loading("one"));
    assert_eq!(
        controller.card("one").unwrap().counts.unwrap().movie_count,
        12
    );
    assert_eq!(
        controller.catalog.servers[0]
            .item_counts
            .as_ref()
            .unwrap()
            .series_count,
        34
    );
    assert!(controller.begin_counts("one").is_none());
    assert!(matches!(
        controller.finish_counts(&current, Err(anyhow::anyhow!("duplicate"))),
        CountResult::Ignored
    ));
}

#[test]
fn deletion_and_account_changes_invalidate_count_results() {
    let mut controller = controller();
    let request = controller.begin_counts("one").unwrap();
    let deleted = controller.proposed_delete("one").unwrap();
    assert_eq!(
        controller.catalog.servers.len(),
        3,
        "candidate is not a committed mutation"
    );
    controller.persist_catalog(deleted, |_| Ok(())).unwrap();
    assert!(matches!(
        controller.finish_counts(&request, Ok(ItemCounts::default())),
        CountResult::Ignored
    ));
    assert!(!controller.counts_loading("one"));
    let request = controller.begin_counts("three").unwrap();
    controller.catalog.servers[1].user_id = Some("different".into());
    assert!(matches!(
        controller.finish_counts(&request, Ok(ItemCounts::default())),
        CountResult::Ignored
    ));
    assert!(controller.counts_loading("three"));
}

#[test]
fn failed_count_refresh_keeps_display_data_and_requires_explicit_retry() {
    let mut controller = controller();
    controller.catalog.servers[0].item_counts = Some(crate::server::CachedItemCounts {
        movie_count: 3,
        series_count: 4,
    });
    let mut controller = ServerController::new(controller.catalog);
    let request = controller.begin_counts("one").unwrap();
    assert!(matches!(
        controller.finish_counts(&request, Err(anyhow::anyhow!("offline"))),
        CountResult::Failed
    ));
    assert_eq!(
        controller.card("one").unwrap().counts.unwrap().movie_count,
        3
    );
    assert!(controller.begin_counts("one").is_none());
    controller.reset_counts("one");
    assert!(controller.begin_counts("one").is_some());
}

#[test]
fn reorder_commits_current_metadata_and_cancel_only_discards_preview() {
    let mut controller = controller();
    assert!(controller.begin_reorder("one"));
    assert!(controller.preview_reorder(2));
    assert!(controller.card("one").unwrap().placeholder);
    assert_eq!(controller.preview_servers()[2].id, "one");
    assert_eq!(controller.catalog.servers[0].id, "one");
    controller.catalog.servers[0].server_name = Some("Updated".into());
    assert!(controller.finish_reorder(true));
    assert_eq!(
        controller.catalog.servers[2].server_name.as_deref(),
        Some("Updated")
    );
    controller.begin_reorder("one");
    controller.preview_reorder(0);
    assert!(!controller.finish_reorder(false));
    assert_eq!(controller.catalog.servers[2].id, "one");
}

#[test]
fn edit_conflict_preserves_latest_metadata_and_snapshot_assembly_keeps_global_values() {
    let mut controller = controller();
    let mut edit = controller.catalog.servers[0].clone();
    edit.username = controller.catalog.servers[2].username.clone();
    controller.catalog.servers[2].server_name = Some("Destination".into());
    controller.catalog.servers[2].icon_is_custom = true;
    controller.catalog.servers[2].icon_url = Some("icon".into());
    controller.toggle_auto_start("three");
    let edited = controller.merge_edited_server(edit).unwrap();
    assert_eq!(edited.server_name.as_deref(), Some("Destination"));
    assert!(edited.needs_auth_refresh && edited.icon_is_custom);
    let (candidate, id) = controller.proposed_save(edited, true).unwrap();
    assert_eq!(id, "one");
    assert_eq!(
        candidate.catalog().auto_start_server_id.as_deref(),
        Some("one")
    );
    assert_eq!(controller.catalog.servers.len(), 3);
    let (mut global, _) = GlobalConfig::split(ServerCache::empty());
    global.playback.cache_secs = 42.5;
    global.set_window_size(1234, 789);
    let snapshot = global.snapshot(candidate.catalog());
    assert_eq!(snapshot.playback.cache_secs, 42.5);
    assert_eq!(snapshot.window_size().unwrap().width, 1234);
    assert_eq!(snapshot.servers.len(), 2);
    // Rejecting this candidate (persistence failure) leaves the live directory untouched.
    assert_eq!(
        controller.catalog.auto_start_server_id.as_deref(),
        Some("three")
    );
    controller.persist_catalog(candidate, |_| Ok(())).unwrap();
    assert_eq!(controller.catalog.servers.len(), 2);
}

#[test]
fn menu_and_card_selectors_follow_current_catalog_and_selection() {
    let mut controller = controller();
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenMenu("missing".into())),
        ServerCommand::Ignored
    ));
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenMenu("one".into())),
        ServerCommand::PreviewChanged
    ));
    assert!(!controller.menu().unwrap().auto_start);
    controller.dispatch(ServerIntent::ToggleAutoStart("one".into()));
    assert!(controller.menu().unwrap().auto_start);
    controller.catalog.servers[0].server_name = Some(String::new());
    let cards = controller.cards();
    assert_eq!(cards[0].title, "example.com");
    assert!(cards[0].auto_start);
    controller.dispatch(ServerIntent::BeginReorder("one".into()));
    controller.dispatch(ServerIntent::ReorderPreview(2));
    let cards = controller.cards();
    assert_eq!(cards[2].server_id, "one");
    assert!(cards[2].placeholder);
    controller.dispatch(ServerIntent::CommitReorder(false));
    controller.dispatch(ServerIntent::CloseMenu);
    assert!(controller.menu().is_none());
    controller.dispatch(ServerIntent::SelectServer("two".into()));
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenMenu("two".into())),
        ServerCommand::Ignored
    ));
    assert!(matches!(
        controller.dispatch(ServerIntent::OpenMenu("one".into())),
        ServerCommand::PreviewChanged
    ));
    controller
        .persist_catalog(controller.proposed_delete("one").unwrap(), |_| Ok(()))
        .unwrap();
    assert!(controller.menu().is_none());
}
