use super::*;

#[test]
fn failed_delete_preserves_auto_start_and_pending_counts_until_successful_persistence() {
    let mut controller = controller();
    controller.dispatch(ServerIntent::ToggleAutoStart("one".into()));
    let request = controller.begin_counts("one").unwrap();
    let change = controller.proposed_delete("one").unwrap();
    let result = controller.persist_catalog(change, |catalog| {
        assert!(catalog.servers.iter().all(|server| server.id != "one"));
        assert!(catalog.auto_start_server_id.is_none());
        anyhow::bail!("disk unavailable")
    });
    assert_eq!(result.unwrap_err().to_string(), "disk unavailable");
    assert_eq!(controller.auto_start_server().unwrap().id, "one");
    assert!(controller.counts_loading("one"));
    assert!(matches!(
        controller.finish_counts(
            &request,
            Ok(ItemCounts {
                movie_count: 17,
                ..Default::default()
            })
        ),
        CountResult::Saved
    ));
    controller.reset_counts("one");
    let late_request = controller.begin_counts("one").unwrap();
    let change = controller.proposed_delete("one").unwrap();
    controller.persist_catalog(change, |_| Ok(())).unwrap();
    assert!(controller.server("one").is_none());
    assert!(controller.auto_start_server().is_none());
    assert!(!controller.counts_loading("one"));
    assert!(matches!(
        controller.finish_counts(&late_request, Ok(ItemCounts::default())),
        CountResult::Ignored
    ));
    assert!(controller.proposed_delete("one").is_none());
}

#[test]
fn credential_fence_changes_only_after_edit_persistence_succeeds() {
    let mut controller = controller();
    let ServerCommand::Authenticate(request) =
        controller.dispatch(ServerIntent::SelectServer("two".into()))
    else {
        panic!("authentication required")
    };
    let mut edit = controller.server("two").unwrap().clone();
    edit.password = "new password".into();
    let edit = controller.merge_edited_server(edit).unwrap();
    let (change, id) = controller.proposed_save(edit.clone(), true).unwrap();
    assert_eq!(id, "two");
    assert!(
        controller
            .persist_catalog(change, |catalog| {
                assert_eq!(catalog.servers[1].password, "new password");
                anyhow::bail!("disk unavailable")
            })
            .is_err()
    );
    assert!(controller.server("two").unwrap().password.is_empty());
    assert!(matches!(
        controller.finish_authentication(&request, Ok(request.server.clone())),
        AuthResult::Ready(Ok(_))
    ));
    let ServerCommand::Authenticate(request) =
        controller.dispatch(ServerIntent::SelectServer("two".into()))
    else {
        panic!("authentication required")
    };
    let (change, _) = controller.proposed_save(edit, true).unwrap();
    controller.persist_catalog(change, |_| Ok(())).unwrap();
    assert_eq!(controller.server("two").unwrap().password, "new password");
    assert!(matches!(
        controller.finish_authentication(&request, Ok(request.server.clone())),
        AuthResult::Invalidated
    ));
    assert!(controller.server("two").unwrap().access_token.is_none());
}
