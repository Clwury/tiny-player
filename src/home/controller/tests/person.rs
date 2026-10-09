use super::*;
use crate::{
    emby::{MediaItem, MediaPerson},
    home::model::notification::{ActionNotification, NotificationScope},
};

fn person() -> MediaPerson {
    serde_json::from_value(json!({"Id":"person", "Name":"Person"})).unwrap()
}

#[test]
fn empty_person_ids_do_not_change_the_route() {
    let mut home = HomeController::new(identity());
    let invalid = serde_json::from_value(json!({"Id":"   ", "Name":"Unknown"})).unwrap();
    assert!(home.open_person(&invalid).is_none());
    assert_eq!(
        home.route(),
        &crate::home::model::navigation::HomeRoute::Root(HomeRoot::Home)
    );
    assert!(home.persons.is_empty());
}

#[test]
fn delayed_person_metadata_cannot_overwrite_a_newer_favorite_mutation() {
    let identity = identity();
    let mut home = HomeController::new(identity.clone());
    let (_, transition) = home.open_person(&person()).unwrap();
    let metadata = transition.metadata.unwrap();
    let command = home
        .dispatch_favorite(FavoriteIntent {
            item_id: "person".into(),
            fallback: Some(UserItemData::default()),
            notification: ActionNotification {
                scope: NotificationScope::Person,
                key: "person:person:favorite".into(),
            },
        })
        .unwrap();
    assert!(command.desired);
    let favorited = serde_json::from_value(json!({"IsFavorite":true})).unwrap();
    assert!(
        home.complete_favorite(&command, Ok(favorited), &identity)
            .unwrap()
            .failure
            .is_none()
    );
    let stale: MediaItem = serde_json::from_value(
        json!({"Id":"person", "Name":"Person", "Type":"Person", "UserData":{"IsFavorite":false}}),
    )
    .unwrap();
    assert!(
        home.complete_person(&metadata, Ok(stale), &identity)
            .unwrap()
            .is_ok()
    );
    let vm = home.person_view("person").unwrap();
    assert!(vm.loaded);
    assert!(!vm.user_data.unwrap().is_favorite);
    assert!(
        home.effective_user_data("person", vm.user_data)
            .unwrap()
            .is_favorite
    );
}

#[test]
fn delayed_person_items_apply_newer_movie_user_data_and_cannot_complete_as_a_library() {
    let identity = identity();
    let mut home = HomeController::new(identity.clone());
    let (_, transition) = home.open_person(&person()).unwrap();
    let request = transition.items.request.unwrap();
    home.user_data.overrides.insert(
        "movie".into(),
        serde_json::from_value(json!({"IsFavorite":true})).unwrap(),
    );
    home.user_data.bump("movie");
    let result: UserItems = serde_json::from_value(json!({"Items":[{"Id":"movie","Name":"Movie","Type":"Movie", "UserData":{"IsFavorite":false}}],"TotalRecordCount":1})).unwrap();
    assert!(
        home.complete_library(&request, Ok(result.clone()), &identity)
            .is_none()
    );
    assert!(
        home.complete_person_items(&request, Ok(result), &identity)
            .is_some()
    );
    assert!(
        home.user_item_by_id("movie")
            .unwrap()
            .user_data
            .unwrap()
            .is_favorite
    );
}
