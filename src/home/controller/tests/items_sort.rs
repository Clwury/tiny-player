use super::*;
use crate::{
    emby::{MediaPerson, SortOrder},
    home::{
        FavoriteItemType,
        favorites::controller::{FAVORITE_ITEM_TYPES, FavoritesIntent},
    },
};

#[test]
fn shared_sort_invalidates_every_cached_scope_and_new_pages_inherit_the_selection() {
    let mut home = HomeController::new(identity());
    let old_library = home.open_library(&view()).unwrap().1.request.unwrap();
    let person: MediaPerson = serde_json::from_value(json!({
        "Id": "person", "Name": "Person"
    }))
    .unwrap();
    let old_person = home.open_person(&person).unwrap().1.items.request.unwrap();
    let old_favorites: Vec<_> = FAVORITE_ITEM_TYPES
        .into_iter()
        .map(|kind| {
            home.dispatch_favorites(kind, FavoritesIntent::Enter)
                .unwrap()
        })
        .collect();
    let sorted_favorite = home
        .dispatch_favorites(
            FavoriteItemType::Person,
            FavoritesIntent::SortBy(UserItemsSort::PremiereDate),
        )
        .unwrap();
    let sorted_library = home
        .dispatch_library("library", LibraryIntent::SortOrder(SortOrder::Descending))
        .unwrap()
        .request
        .unwrap();
    assert!(
        home.complete_library(&old_library, Ok(items("stale", 100)), &identity())
            .is_none()
    );
    assert!(
        home.complete_person_items(&old_person, Ok(items("stale", 100)), &identity())
            .is_none()
    );
    for stale in old_favorites
        .iter()
        .chain(std::iter::once(&sorted_favorite))
    {
        assert!(
            home.complete_favorites(stale, Ok(items("stale", 100)), &identity())
                .is_none()
        );
    }
    assert!(home.user_data.overrides.is_empty());
    assert_eq!(
        sorted_library.query.sort_by,
        Some(UserItemsSort::PremiereDate)
    );
    assert_eq!(sorted_library.query.sort_order, SortOrder::Descending);
    for kind in FAVORITE_ITEM_TYPES {
        let vm = home.favorite_section(kind);
        assert_eq!(vm.sort_by, UserItemsSort::PremiereDate);
        assert_eq!(vm.sort_order, SortOrder::Descending);
        assert!(vm.paged.items.is_empty());
        let refreshed = home
            .dispatch_favorites(kind, FavoritesIntent::Enter)
            .unwrap();
        assert_eq!(refreshed.sort_by, UserItemsSort::PremiereDate);
        assert_eq!(refreshed.sort_order, SortOrder::Descending);
        assert_eq!(refreshed.start_index, 0);
    }
    let current_person = home.enter_person("person").unwrap().items.request.unwrap();
    assert_eq!(
        current_person.query.sort_by,
        Some(UserItemsSort::PremiereDate)
    );
    assert_eq!(current_person.query.sort_order, SortOrder::Descending);
    let new_view = serde_json::from_value(json!({
        "Id": "another-library", "Name": "Series", "CollectionType": "tvshows"
    }))
    .unwrap();
    let new_library = home.open_library(&new_view).unwrap().1.request.unwrap();
    assert_eq!(new_library.query.sort_by, Some(UserItemsSort::PremiereDate));
    assert_eq!(new_library.query.sort_order, SortOrder::Descending);
    let new_person =
        serde_json::from_value(json!({"Id": "another-person", "Name": "Another"})).unwrap();
    let new_person_items = home
        .open_person(&new_person)
        .unwrap()
        .1
        .items
        .request
        .unwrap();
    assert_eq!(
        new_person_items.query.sort_by,
        Some(UserItemsSort::PremiereDate)
    );
    assert_eq!(new_person_items.query.sort_order, SortOrder::Descending);
    let unchanged = home
        .dispatch_person_items(
            "another-person",
            LibraryIntent::SortBy(UserItemsSort::PremiereDate),
        )
        .unwrap();
    assert!(unchanged.close_menu);
    assert!(unchanged.request.is_none());
    assert!(
        home.person_view("another-person")
            .unwrap()
            .items
            .paged
            .accepts_initial(&new_person_items.token)
    );
}

#[test]
fn series_only_shared_sort_falls_back_without_overwriting_the_saved_choice() {
    let mut home = HomeController::new(identity());
    let preferences = crate::media::ItemSortPreferences {
        sort_by: UserItemsSort::DateLastContentAdded,
        sort_order: SortOrder::Descending,
    };
    home.set_items_sort(preferences);
    let movie = home.open_library(&view()).unwrap().1.request.unwrap();
    assert_eq!(movie.query.sort_by, Some(UserItemsSort::SortName));
    assert_eq!(movie.query.sort_order, SortOrder::Descending);
    assert!(!home.current_items_sort_is_available(UserItemsSort::DateLastContentAdded));
    let ignored = home
        .dispatch_library(
            "library",
            LibraryIntent::SortBy(UserItemsSort::DateLastContentAdded),
        )
        .unwrap();
    assert!(ignored.request.is_none());
    assert!(
        home.library_view("library")
            .unwrap()
            .paged
            .accepts_initial(&movie.token)
    );

    let person = serde_json::from_value(json!({"Id": "person", "Name": "Person"})).unwrap();
    let person_items = home.open_person(&person).unwrap().1.items.request.unwrap();
    assert_eq!(person_items.query.sort_by, Some(UserItemsSort::SortName));
    assert_eq!(person_items.query.sort_order, SortOrder::Descending);
    assert!(!home.current_items_sort_is_available(UserItemsSort::DateLastContentAdded));
    assert!(
        home.dispatch_person_items(
            "person",
            LibraryIntent::SortBy(UserItemsSort::DateLastContentAdded)
        )
        .unwrap()
        .request
        .is_none()
    );
    assert!(
        home.person_view("person")
            .unwrap()
            .items
            .paged
            .accepts_initial(&person_items.token)
    );

    for kind in FAVORITE_ITEM_TYPES {
        home.navigation.push_favorite_items(kind);
        let is_series = kind == FavoriteItemType::Series;
        assert_eq!(
            home.current_items_sort_is_available(UserItemsSort::DateLastContentAdded),
            is_series
        );
        let request = home
            .dispatch_favorites(kind, FavoritesIntent::Enter)
            .unwrap();
        assert_eq!(
            request.sort_by,
            if is_series {
                UserItemsSort::DateLastContentAdded
            } else {
                UserItemsSort::SortName
            }
        );
        assert_eq!(request.sort_order, SortOrder::Descending);
        assert!(
            home.dispatch_favorites(
                kind,
                FavoritesIntent::SortBy(UserItemsSort::DateLastContentAdded)
            )
            .is_none()
        );
        assert!(
            home.favorite_section(kind)
                .paged
                .accepts_initial(&request.token)
        );
        assert_eq!(home.items_sort, preferences);
    }

    let series_view = serde_json::from_value(json!({
        "Id": "series-library", "Name": "Series", "CollectionType": "tvshows"
    }))
    .unwrap();
    let series = home.open_library(&series_view).unwrap().1.request.unwrap();
    assert_eq!(
        series.query.sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert_eq!(series.query.sort_order, SortOrder::Descending);
    assert!(home.current_items_sort_is_available(UserItemsSort::DateLastContentAdded));
    assert_eq!(home.items_sort, preferences);
}
