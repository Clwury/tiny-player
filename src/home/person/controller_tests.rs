use super::*;
use crate::{
    emby::{SortOrder, UserItem, UserItems, UserItemsSort},
    home::library::controller::{LibraryFailure, LibrarySource, available_library_sorts},
};

fn person(id: &str, favorite: bool) -> MediaItem {
    serde_json::from_value(serde_json::json!({
        "Id": id, "Name": "Person", "Type": "Person", "UserData": {"IsFavorite": favorite}
    }))
    .unwrap()
}

#[test]
fn metadata_retries_failures_and_rejects_other_people_accounts_and_duplicate_results() {
    let identity = WorkspaceIdentity::default();
    let mut controller = PersonController::new("person".into(), identity.clone());
    let request = controller.enter(7).metadata.unwrap();
    assert!(controller.enter(8).metadata.is_none());
    let other = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..identity.clone()
    };
    assert!(
        controller
            .complete(&request, Ok(person("person", true)), &other)
            .is_none()
    );
    assert!(
        controller
            .complete(&request, Ok(person("wrong", true)), &identity)
            .unwrap()
            .is_err()
    );
    assert!(!controller.view_model().loaded);
    let retry = controller.enter(9).metadata.unwrap();
    assert_eq!(retry.user_data_revision, 9);
    assert!(
        controller
            .complete(&request, Ok(person("person", true)), &identity)
            .is_none()
    );
    assert!(
        controller
            .complete(&retry, Ok(person("person", true)), &identity)
            .unwrap()
            .is_ok()
    );
    assert!(controller.view_model().user_data.unwrap().is_favorite);
    assert!(controller.view_model().loaded);
    assert!(controller.enter(10).metadata.is_none());
    assert!(
        controller
            .complete(&retry, Ok(person("person", false)), &identity)
            .is_none()
    );
}

#[test]
fn person_filmography_keeps_filter_across_sort_changes_and_raw_pagination() {
    let identity = WorkspaceIdentity::default();
    let mut controller = PersonController::new("person".into(), identity.clone());
    let old = controller.enter(0).items.request.unwrap();
    assert_eq!(old.source, LibrarySource::Person("person".into()));
    assert!(old.query.parent_id.is_none());
    assert_eq!(old.query.person_ids, ["person"]);
    assert_eq!(
        old.query.include_item_types,
        [VideoItemType::Movie, VideoItemType::Series]
    );
    assert!(old.query.recursive);
    assert_eq!(old.query.sort_by, Some(UserItemsSort::SortName));
    assert_eq!(old.query.sort_order, SortOrder::Ascending);
    assert!(
        !available_library_sorts(crate::media::ItemSortOptions::for_item_types(
            &old.query.include_item_types
        ))
        .any(|sort| sort == UserItemsSort::DateLastContentAdded)
    );
    let sorted = controller
        .items
        .dispatch(LibraryIntent::SortBy(UserItemsSort::DateCreated), 1)
        .request
        .unwrap();
    assert!(
        controller
            .items
            .complete(&old, Err(anyhow::anyhow!("late error")), &identity)
            .is_none()
    );
    let descending = controller
        .items
        .dispatch(LibraryIntent::SortOrder(SortOrder::Descending), 2)
        .request
        .unwrap();
    assert!(
        controller
            .items
            .complete(
                &sorted,
                Ok(UserItems {
                    items: vec![],
                    total_record_count: 0
                }),
                &identity
            )
            .is_none()
    );
    let items: Vec<UserItem> = (0..60).map(|index| serde_json::from_value(serde_json::json!({
        "Id": format!("item-{index}"), "Name": "Title", "Type": if index == 0 { "Movie" } else if index == 1 { "Series" } else { "Episode" }
    })).unwrap()).collect();
    let update = controller
        .items
        .complete(
            &descending,
            Ok(UserItems {
                items,
                total_record_count: 61,
            }),
            &identity,
        )
        .unwrap();
    assert!(update.failure.is_none());
    assert_eq!(controller.view_model().items.paged.items.len(), 2);
    let more = controller
        .items
        .dispatch(LibraryIntent::LoadMore { automatic: true }, 3)
        .request
        .unwrap();
    assert_eq!(more.query.person_ids, ["person"]);
    assert!(more.query.parent_id.is_none());
    assert_eq!(more.query.start_index, 60);
    assert_eq!(more.query.sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(more.query.sort_order, SortOrder::Descending);
    let failed = controller
        .items
        .complete(&more, Err(anyhow::anyhow!("offline")), &identity)
        .unwrap();
    assert!(matches!(failed.failure, Some(LibraryFailure::More(_))));
    assert!(
        controller
            .items
            .dispatch(LibraryIntent::LoadMore { automatic: true }, 4)
            .request
            .is_none()
    );
    let retry = controller
        .items
        .dispatch(LibraryIntent::LoadMore { automatic: false }, 4)
        .request
        .unwrap();
    assert_eq!(retry.query.start_index, 60);
}
