use super::*;
use crate::{
    emby::{MediaGenre, SortOrder},
    home::library::controller::LibrarySource,
    media::ItemSortPreferences,
};

#[test]
fn genre_pages_keep_the_filter_during_shared_sort_changes_and_raw_pagination() {
    let mut home = HomeController::new(identity());
    home.set_items_sort(ItemSortPreferences {
        sort_by: UserItemsSort::PremiereDate,
        sort_order: SortOrder::Descending,
    });
    let genre = MediaGenre {
        name: " Drama ".into(),
        id: Some(" g18 ".into()),
    };
    let old = home.open_genre(&genre).unwrap().1.request.unwrap();
    assert_eq!(old.source, LibrarySource::Genre("id:g18".into()));
    assert_eq!(home.title(), "Drama");
    assert_eq!(old.query.genre_ids, ["g18"]);
    assert!(
        old.query.genres.is_empty()
            && old.query.parent_id.is_none()
            && old.query.person_ids.is_empty()
    );
    assert!(old.query.group_programs_by_series && old.query.recursive);
    assert_eq!(old.query.collapse_box_set_items, Some(false));
    assert_eq!(old.query.sort_by, Some(UserItemsSort::PremiereDate));
    assert_eq!(old.query.sort_order, SortOrder::Descending);
    assert!(home.current_items_sort_is_available(UserItemsSort::DateLastContentAdded));
    let sorted = home
        .dispatch_genre_items(
            "id:g18",
            LibraryIntent::SortBy(UserItemsSort::DateLastContentAdded),
        )
        .unwrap()
        .request
        .unwrap();
    assert_eq!(
        sorted.query.sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert_eq!(
        home.genre_view("id:g18").unwrap().sort_by,
        UserItemsSort::DateLastContentAdded
    );
    assert_eq!(home.items_sort.sort_by, UserItemsSort::DateLastContentAdded);
    assert!(
        home.complete_genre_items(&old, Ok(items("stale", 1)), &identity())
            .is_none()
    );
    let mut first: UserItems = serde_json::from_value(json!({
        "Items": (0..60).map(|index| json!({
            "Id": format!("entry-{index}"), "Name": "Genre result", "Type": if index % 2 == 0 { "Movie" } else { "Series" }, "UserData": {"IsFavorite": true}
        })).collect::<Vec<_>>(), "TotalRecordCount": 120
    })).unwrap();
    first.items[0].item_type = Some("Episode".into());
    first.items[1].item_type = Some("Person".into());
    home.complete_genre_items(&sorted, Ok(first), &identity())
        .unwrap();
    assert_eq!(home.genre_view("id:g18").unwrap().paged.items.len(), 58);
    assert_eq!(
        home.user_item_by_id("entry-3")
            .unwrap()
            .item_type
            .as_deref(),
        Some("Series")
    );
    assert!(
        home.loaded_playback_user_data("entry-3")
            .unwrap()
            .is_favorite
    );
    let more = home
        .dispatch_genre_items("id:g18", LibraryIntent::LoadMore { automatic: true })
        .unwrap()
        .request
        .unwrap();
    assert_eq!(more.query.start_index, 60);
    assert_eq!(more.query.genre_ids, ["g18"]);
    assert_eq!(
        more.query.include_item_types,
        [VideoItemType::Movie, VideoItemType::Series]
    );
    assert_eq!(
        more.query.sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert_eq!(more.query.sort_order, SortOrder::Descending);
    let inherited = home.open_library(&view()).unwrap().1.request.unwrap();
    assert_eq!(inherited.query.sort_by, Some(UserItemsSort::SortName));
    assert_eq!(inherited.query.sort_order, SortOrder::Descending);
    home.set_items_sort(ItemSortPreferences {
        sort_by: UserItemsSort::ProductionYear,
        sort_order: SortOrder::Descending,
    });
    assert!(
        home.complete_genre_items(&more, Ok(items("stale", 1)), &identity())
            .is_none()
    );
    let refreshed = home
        .dispatch_genre_items(
            "id:g18",
            LibraryIntent::Open(vec![VideoItemType::Movie, VideoItemType::Series]),
        )
        .unwrap()
        .request
        .unwrap();
    assert_eq!(refreshed.query.sort_by, Some(UserItemsSort::ProductionYear));
    assert_eq!(home.items_sort.sort_by, UserItemsSort::ProductionYear);
}

#[test]
fn genre_requests_are_isolated_from_other_genres_and_support_legacy_names() {
    let mut home = HomeController::new(identity());
    let first = home
        .open_genre(&MediaGenre {
            name: "Drama".into(),
            id: Some("g18".into()),
        })
        .unwrap()
        .1
        .request
        .unwrap();
    let second = home
        .open_genre(&MediaGenre {
            name: " g18 ".into(),
            id: None,
        })
        .unwrap()
        .1
        .request
        .unwrap();
    assert_eq!(second.source, LibrarySource::Genre("name:g18".into()));
    assert!(second.query.genre_ids.is_empty());
    assert_eq!(second.query.genres, ["g18"]);
    let forged = crate::home::library::controller::LibraryRequest {
        source: second.source.clone(),
        ..first.clone()
    };
    assert!(
        home.complete_genre_items(&forged, Ok(items("wrong genre", 1)), &identity())
            .is_none()
    );
    let foreign = WorkspaceIdentity {
        user_id: Some("other-user".into()),
        ..identity()
    };
    assert!(
        home.complete_genre_items(&second, Ok(items("foreign", 1)), &foreign)
            .is_none()
    );
    home.complete_genre_items(&second, Ok(items("Name result", 1)), &identity())
        .unwrap();
    home.complete_genre_items(&first, Ok(items("ID result", 1)), &identity())
        .unwrap();
    assert_eq!(home.user_item_by_id("movie").unwrap().name, "Name result");
    assert!(
        home.open_genre(&MediaGenre {
            name: " ".into(),
            id: None
        })
        .is_none()
    );
}
