use super::*;

fn servers(ids: &[&str]) -> Vec<SidebarServer> {
    ids.iter()
        .map(|id| SidebarServer {
            id: (*id).into(),
            title: format!("{id} title"),
            icon_url: None,
        })
        .collect()
}
fn controller() -> SidebarController {
    SidebarController::new(
        "first".into(),
        "viewer".into(),
        servers(&["first", "second", "third"]),
    )
}
fn order(controller: &SidebarController) -> Vec<&str> {
    controller
        .view_model()
        .rows
        .iter()
        .map(|row| row.server.id.as_str())
        .collect()
}

#[test]
fn selection_requires_a_known_server_and_allows_current_server_to_cancel_pending_login() {
    let mut sidebar = controller();
    assert_eq!(
        sidebar.dispatch(SidebarIntent::Select("first".into())),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::Select("unknown".into())),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::Select("second".into())),
        SidebarCommand::Select("second".into())
    );
    sidebar.dispatch(SidebarIntent::SelectingChanged(Some("second".into())));
    assert_eq!(
        sidebar.dispatch(SidebarIntent::Select("first".into())),
        SidebarCommand::Select("first".into())
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::Select("unknown".into())),
        SidebarCommand::None
    );
    assert_eq!(sidebar.current_server_id(), "first");
}

#[test]
fn preview_borrows_display_records_and_commit_uses_original_catalog_target() {
    let mut sidebar = controller();
    assert_eq!(
        sidebar.dispatch(SidebarIntent::BeginReorder("first".into())),
        SidebarCommand::Changed
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::PreviewReorder(2)),
        SidebarCommand::Changed
    );
    let view = sidebar.view_model();
    assert_eq!(view.username, "viewer");
    assert_eq!(order(&sidebar), ["second", "third", "first"]);
    assert!(view.rows[2].active && view.rows[2].placeholder);
    assert!(std::ptr::eq(view.rows[2].server, &sidebar.servers[0]));
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(true)),
        SidebarCommand::Reorder {
            server_id: "first".into(),
            target_id: "third".into()
        }
    );
    assert_eq!(order(&sidebar), ["first", "second", "third"]);
    assert!(sidebar.view_model().rows.iter().all(|row| !row.placeholder));
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(true)),
        SidebarCommand::None
    );
}

#[test]
fn invalid_preview_and_cancel_preserve_catalog_without_emitting_reorder() {
    let mut sidebar = controller();
    assert_eq!(
        sidebar.dispatch(SidebarIntent::BeginReorder("unknown".into())),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::PreviewReorder(1)),
        SidebarCommand::None
    );
    sidebar.dispatch(SidebarIntent::BeginReorder("third".into()));
    assert_eq!(
        sidebar.dispatch(SidebarIntent::PreviewReorder(2)),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::PreviewReorder(3)),
        SidebarCommand::None
    );
    sidebar.dispatch(SidebarIntent::PreviewReorder(0));
    assert_eq!(order(&sidebar), ["third", "first", "second"]);
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(false)),
        SidebarCommand::Changed
    );
    assert_eq!(order(&sidebar), ["first", "second", "third"]);
    sidebar.dispatch(SidebarIntent::BeginReorder("second".into()));
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(true)),
        SidebarCommand::Changed
    );
}

#[test]
fn snapshot_updates_preserve_preview_clamping_and_out_of_bounds_commit_rules() {
    let mut sidebar = controller();
    sidebar.dispatch(SidebarIntent::BeginReorder("first".into()));
    sidebar.dispatch(SidebarIntent::PreviewReorder(2));
    sidebar.dispatch(SidebarIntent::ServersChanged(servers(&["first", "second"])));
    assert_eq!(order(&sidebar), ["second", "first"]);
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(true)),
        SidebarCommand::Changed
    );
    sidebar.dispatch(SidebarIntent::BeginReorder("first".into()));
    sidebar.dispatch(SidebarIntent::ServersChanged(vec![]));
    assert!(sidebar.view_model().rows.is_empty());
    assert_eq!(
        sidebar.dispatch(SidebarIntent::FinishReorder(true)),
        SidebarCommand::Changed
    );
}

#[test]
fn selection_snapshot_controls_loading_and_reorder_without_changing_active_server() {
    let mut sidebar = controller();
    assert_eq!(
        sidebar.dispatch(SidebarIntent::SelectingChanged(None)),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::ServersChanged(servers(&[
            "first", "second", "third"
        ]))),
        SidebarCommand::None
    );
    assert_eq!(
        sidebar.dispatch(SidebarIntent::SelectingChanged(Some("third".into()))),
        SidebarCommand::Changed
    );
    let view = sidebar.view_model();
    assert!(view.rows[0].active && !view.rows[0].loading);
    assert!(view.rows[2].loading && !view.rows[2].active);
    assert!(view.rows.iter().all(|row| !row.can_reorder));
    sidebar.dispatch(SidebarIntent::SelectingChanged(None));
    assert!(
        sidebar
            .view_model()
            .rows
            .iter()
            .all(|row| row.can_reorder && !row.loading)
    );
}
