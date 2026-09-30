use super::*;

fn controller() -> SearchController {
    SearchController::new(WorkspaceIdentity::default())
}
fn submit(controller: &mut SearchController, query: &str) -> SearchRequestContext {
    controller
        .dispatch(SearchIntent::Submit(query.into()), 7)
        .request
        .unwrap()
}
fn page(id: &str, raw: u32) -> SearchPage {
    SearchPage {
        items: if id.is_empty() {
            Vec::new()
        } else {
            vec![
                serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Movie"}))
                    .unwrap(),
            ]
        },
        total_record_count: 90,
        raw_item_count: raw,
    }
}

#[test]
fn input_submission_and_history_clear_preserve_the_active_request_contract() {
    let mut controller = controller();
    let input = controller.dispatch(SearchIntent::InputChanged("  query  ".into()), 3);
    assert!(input.reset_presentation && input.notify && !input.history_changed);
    assert!(input.request.is_none() && controller.view_model().history.entries().is_empty());
    let submitted = controller.dispatch(SearchIntent::Submit(" query ".into()), 4);
    assert!(submitted.reset_presentation && submitted.history_changed && submitted.notify);
    let request = submitted.request.unwrap();
    assert_eq!(request.request.query, "query");
    assert_eq!(request.user_data_revision, 4);
    let duplicate = controller.dispatch(SearchIntent::Submit("query".into()), 5);
    assert!(!duplicate.reset_presentation && !duplicate.notify && duplicate.request.is_none());
    let whitespace = controller.dispatch(SearchIntent::InputChanged("  query ".into()), 6);
    assert!(!whitespace.reset_presentation && !whitespace.notify);
    let cleared = controller.dispatch(SearchIntent::ClearHistory, 6);
    assert!(cleared.history_changed && !cleared.reset_presentation);
    assert!(
        controller
            .complete(
                &request,
                Ok(page("first", 30)),
                &WorkspaceIdentity::default()
            )
            .is_some()
    );
    assert!(
        controller.view_model().has_results && controller.view_model().history.entries().is_empty()
    );
}

#[test]
fn query_replacement_and_clear_suppress_all_old_completion_side_effects() {
    let mut controller = controller();
    let old = submit(&mut controller, "old");
    let current = submit(&mut controller, "new");
    for response in [Ok(page("old", 30)), Err(anyhow::anyhow!("old error"))] {
        assert!(
            controller
                .complete(&old, response, &WorkspaceIdentity::default())
                .is_none()
        );
    }
    assert!(controller.view_model().items.is_empty());
    assert_eq!(controller.state.initial, LoadState::Loading);
    let reset = controller.dispatch(SearchIntent::Submit("  ".into()), 9);
    assert!(reset.reset_presentation && reset.notify && !reset.history_changed);
    assert!(
        controller
            .complete(&current, Ok(page("new", 30)), &WorkspaceIdentity::default())
            .is_none()
    );
    assert!(!controller.view_model().has_results && !controller.view_model().empty_results);
    assert_eq!(controller.view_model().history.entries(), ["new", "old"]);
}

#[test]
fn filtered_empty_page_can_load_more_and_retry_keeps_raw_offset_and_prior_data() {
    let mut controller = controller();
    let first = submit(&mut controller, "query");
    let update = controller
        .complete(&first, Ok(page("", 30)), &WorkspaceIdentity::default())
        .unwrap();
    assert!(update.initial && update.received.unwrap().items.is_empty());
    assert!(controller.view_model().can_load_more && !controller.view_model().empty_results);
    let more = controller
        .dispatch(SearchIntent::LoadMore, 8)
        .request
        .unwrap();
    assert_eq!(more.request.start_index, 30);
    let failure = controller
        .complete(
            &more,
            Err(anyhow::anyhow!("offline")),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(!failure.initial && failure.received.is_none());
    assert_eq!(failure.error.as_deref(), Some("offline"));
    let retry = controller
        .dispatch(SearchIntent::LoadMore, 9)
        .request
        .unwrap();
    assert_eq!(retry.request.start_index, 30);
    assert!(
        controller
            .complete(
                &more,
                Err(anyhow::anyhow!("late")),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
    controller
        .complete(&retry, Ok(page("last", 1)), &WorkspaceIdentity::default())
        .unwrap();
    assert_eq!(controller.view_model().items[0].id, "last");
    assert!(!controller.view_model().can_load_more && controller.view_model().has_results);
}

#[test]
fn foreign_workspace_is_rejected_before_results_or_errors_can_be_consumed() {
    let mut controller = controller();
    let request = submit(&mut controller, "query");
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..Default::default()
    };
    assert!(
        controller
            .complete(&request, Ok(page("foreign", 30)), &foreign)
            .is_none()
    );
    assert!(
        controller
            .complete(&request, Err(anyhow::anyhow!("foreign")), &foreign)
            .is_none()
    );
    assert!(controller.state.initial_error.is_none());
    let response = controller
        .complete(&request, Ok(page("", 0)), &WorkspaceIdentity::default())
        .unwrap();
    assert!(response.received.is_some() && response.error.is_none());
    assert!(controller.view_model().empty_results);
    assert!(
        controller
            .complete(
                &request,
                Err(anyhow::anyhow!("duplicate")),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
}
