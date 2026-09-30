use super::*;
use crate::emby::UserItem;

fn controller(item_type: VideoItemType) -> LibraryController {
    LibraryController::new(
        vec![item_type],
        "library".into(),
        WorkspaceIdentity::default(),
    )
}

fn open(controller: &mut LibraryController, item_type: VideoItemType) -> LibraryRequest {
    controller
        .dispatch(LibraryIntent::Open(vec![item_type]), 7)
        .request
        .unwrap()
}

fn item(id: &str, item_type: &str) -> UserItem {
    serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": item_type})).unwrap()
}

fn page(items: Vec<UserItem>) -> UserItems {
    UserItems {
        items,
        total_record_count: 180,
    }
}

#[test]
fn sort_replacement_suppresses_old_results_and_errors_before_followups() {
    let mut controller = controller(VideoItemType::Movie);
    let old = open(&mut controller, VideoItemType::Movie);
    let changed = controller.dispatch(LibraryIntent::SortBy(UserItemsSort::DateCreated), 8);
    assert!(changed.cancel && changed.close_menu && changed.notify);
    let current = changed.request.unwrap();
    assert_eq!(current.user_data_revision, 8);
    for result in [
        Ok(page(vec![item("old", "Movie")])),
        Err(anyhow::anyhow!("old failure")),
    ] {
        assert!(
            controller
                .complete(&old, result, &WorkspaceIdentity::default())
                .is_none()
        );
    }
    assert!(controller.view_model().paged.items.is_empty());
    assert!(controller.view_model().paged.initial_error.is_none());
    let update = controller
        .complete(
            &current,
            Ok(page(vec![item("current", "Movie")])),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(update.failure.is_none());
    assert_eq!(update.received.unwrap().items[0].id, "current");
    assert_eq!(update.images.unwrap().items[0].id, "current");
    assert!(
        controller
            .complete(
                &current,
                Err(anyhow::anyhow!("duplicate")),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
}

#[test]
fn raw_page_count_survives_filtering_deduplication_and_manual_retry() {
    let mut controller = controller(VideoItemType::Movie);
    let first = open(&mut controller, VideoItemType::Movie);
    let mut items = vec![item("duplicate", "Movie"); 2];
    items.extend((2..PAGED_ITEMS_LIMIT).map(|index| item(&index.to_string(), "Episode")));
    let update = controller
        .complete(&first, Ok(page(items)), &WorkspaceIdentity::default())
        .unwrap();
    // Both accepted copies remain available to user-data revision merging even
    // though the grid keeps only one card and advances by the unfiltered count.
    assert_eq!(update.received.unwrap().items.len(), 2);
    assert_eq!(update.images.unwrap().items.len(), 1);
    assert_eq!(controller.view_model().paged.items.len(), 1);
    let more = controller
        .dispatch(LibraryIntent::LoadMore { automatic: true }, 8)
        .request
        .unwrap();
    assert_eq!(more.start_index, PAGED_ITEMS_LIMIT);
    assert!(
        controller
            .dispatch(LibraryIntent::LoadMore { automatic: true }, 8)
            .request
            .is_none()
    );
    let update = controller
        .complete(
            &more,
            Err(anyhow::anyhow!("offline")),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(update.received.is_none() && update.images.is_none());
    assert!(matches!(update.failure, Some(LibraryFailure::More(error)) if error == "offline"));
    assert!(
        controller
            .dispatch(LibraryIntent::LoadMore { automatic: true }, 9)
            .request
            .is_none()
    );
    let retry = controller
        .dispatch(LibraryIntent::LoadMore { automatic: false }, 9)
        .request
        .unwrap();
    assert_eq!(retry.start_index, PAGED_ITEMS_LIMIT);
    assert!(
        controller
            .complete(
                &more,
                Ok(page(vec![item("late", "Movie")])),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
    controller
        .complete(
            &retry,
            Ok(page(vec![item("last", "Movie")])),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert_eq!(
        controller
            .view_model()
            .paged
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["duplicate", "last"]
    );
    assert!(controller.view_model().paged.exhausted);
}

#[test]
fn reopening_preserves_loaded_data_and_resets_an_unavailable_sort() {
    let mut controller = controller(VideoItemType::Series);
    let first = open(&mut controller, VideoItemType::Series);
    assert!(
        controller
            .dispatch(LibraryIntent::Open(vec![VideoItemType::Series]), 8)
            .request
            .is_none()
    );
    controller
        .complete(
            &first,
            Ok(page(vec![item("series", "Series")])),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(
        controller
            .dispatch(LibraryIntent::Open(vec![VideoItemType::Series]), 8)
            .request
            .is_none()
    );
    assert_eq!(controller.view_model().paged.items[0].id, "series");
    let old = controller
        .dispatch(
            LibraryIntent::SortBy(UserItemsSort::DateLastContentAdded),
            9,
        )
        .request
        .unwrap();
    let reopened = controller.dispatch(LibraryIntent::Open(vec![VideoItemType::Movie]), 10);
    assert!(reopened.cancel && reopened.close_menu);
    let current = reopened.request.unwrap();
    assert_eq!(current.query.include_item_types, [VideoItemType::Movie]);
    assert_eq!(current.query.sort_by, Some(UserItemsSort::SortName));
    assert!(
        controller
            .complete(
                &old,
                Err(anyhow::anyhow!("late")),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
    let ignored = controller.dispatch(
        LibraryIntent::SortBy(UserItemsSort::DateLastContentAdded),
        11,
    );
    assert!(!ignored.notify && !ignored.close_menu && ignored.request.is_none());
    controller
        .complete(
            &current,
            Ok(page(vec![item("movie", "Movie")])),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert_eq!(controller.view_model().paged.items[0].id, "movie");
}

#[test]
fn workspace_and_library_tokens_are_isolated_and_failed_initial_can_retry() {
    let mut controller = controller(VideoItemType::Movie);
    let first = open(&mut controller, VideoItemType::Movie);
    let foreign = WorkspaceIdentity {
        user_id: Some("foreign".into()),
        ..Default::default()
    };
    assert!(
        controller
            .complete(&first, Err(anyhow::anyhow!("foreign")), &foreign)
            .is_none()
    );
    let mut other = LibraryController::new(
        vec![VideoItemType::Movie],
        "other".into(),
        WorkspaceIdentity::default(),
    );
    let other_request = open(&mut other, VideoItemType::Movie);
    assert!(
        controller
            .complete(
                &other_request,
                Ok(page(vec![item("other", "Movie")])),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
    let update = controller
        .complete(
            &first,
            Err(anyhow::anyhow!("offline")),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(matches!(update.failure, Some(LibraryFailure::Initial(error)) if error == "offline"));
    assert!(!controller.view_model().empty);
    let retry = open(&mut controller, VideoItemType::Movie);
    assert_eq!(retry.start_index, 0);
    controller
        .complete(
            &retry,
            Ok(page(vec![
                item("", "Movie"),
                item("  ", "Movie"),
                item("unsupported", "Episode"),
            ])),
            &WorkspaceIdentity::default(),
        )
        .unwrap();
    assert!(controller.view_model().empty);
    assert_eq!(controller.view_model().paged.next_start_index, 3);
}
