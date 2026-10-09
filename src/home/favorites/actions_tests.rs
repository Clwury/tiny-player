use super::*;
use crate::home::model::{LoadState, notification::NotificationScope};

fn intent(id: &str, in_favorites: bool, fallback: Option<UserItemData>) -> ToggleFavorite {
    ToggleFavorite {
        item_id: id.into(),
        fallback,
        in_favorites,
        notification: ActionNotification {
            scope: NotificationScope::Favorites,
            key: format!("favorites:favorite:{id}"),
        },
    }
}

#[test]
fn pending_actions_and_invalid_targets_do_not_mutate_data_or_revisions() {
    let mut actions = FavoriteActions::new(WorkspaceIdentity::default());
    let mut data = UserDataState::default();
    let mut favorites = FavoritesController::new(WorkspaceIdentity::default());
    assert!(
        actions
            .toggle(intent(" ", false, None), false, &mut data, &mut favorites)
            .is_none()
    );
    assert!(
        actions
            .toggle(
                intent("movie", false, None),
                true,
                &mut data,
                &mut favorites
            )
            .is_none()
    );
    assert_eq!(data.revision, 0);
    assert!(!actions.has_pending() && data.overrides.is_empty());
    let command = actions
        .toggle(
            intent(
                "movie",
                false,
                Some(UserItemData {
                    played: true,
                    playback_position_ticks: Some(42),
                    ..Default::default()
                }),
            ),
            false,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    assert!(command.desired && actions.is_pending("movie"));
    assert!(data.overrides["movie"].is_favorite && data.overrides["movie"].played);
    assert_eq!(data.overrides["movie"].playback_position_ticks, Some(42));
    assert!(
        actions
            .toggle(
                intent("other", false, None),
                false,
                &mut data,
                &mut favorites
            )
            .is_none()
    );
    assert_eq!(data.revision, 1);
    assert_eq!(data.overrides.len(), 1);
}

#[test]
fn failed_removal_restores_the_exact_override_category_position_and_total() {
    let identity = WorkspaceIdentity::default();
    let mut actions = FavoriteActions::new(identity.clone());
    let mut data = UserDataState::default();
    let mut favorites = FavoritesController::new(identity.clone());
    let original = UserItemData {
        is_favorite: true,
        played: true,
        playback_position_ticks: Some(420),
        ..Default::default()
    };
    data.overrides.insert("episode".into(), original.clone());
    let section = favorites.test_state_mut(FavoriteItemType::Episode);
    section.items = ["before", "episode", "after"]
        .into_iter()
        .map(|id| {
            serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Episode"}))
                .unwrap()
        })
        .collect();
    section.initial = LoadState::Loaded;
    section.total_record_count = Some(3);
    let row = favorites.test_overview_state_mut(FavoriteItemType::Episode);
    row.items = ["episode", "before", "after"]
        .into_iter()
        .map(|id| {
            serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Episode"}))
                .unwrap()
        })
        .collect();
    row.initial = LoadState::Loaded;
    row.total_record_count = Some(3);
    let command = actions
        .toggle(
            intent("episode", true, None),
            false,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    assert!(!command.desired && !data.overrides["episode"].is_favorite);
    assert_eq!(
        favorites
            .view_model(FavoriteItemType::Episode)
            .paged
            .total_record_count,
        Some(2)
    );
    assert_eq!(
        favorites.view_model(FavoriteItemType::Episode).paged.items[1].id,
        "after"
    );
    let row = favorites.overview_view_model(FavoriteItemType::Episode);
    assert_eq!(row.paged.items[0].id, "before");
    assert_eq!(row.paged.total_record_count, Some(2));
    let update = actions
        .complete(
            &command,
            Err(anyhow::anyhow!("offline")),
            &identity,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    let (notification, error) = update.failure.unwrap();
    assert_eq!(notification.scope, NotificationScope::Favorites);
    assert_eq!(notification.key, "favorites:favorite:episode");
    assert_eq!(error, "offline");
    assert_eq!(data.overrides["episode"], original);
    let section = favorites.view_model(FavoriteItemType::Episode);
    assert_eq!(section.paged.items[1].id, "episode");
    assert_eq!(section.paged.total_record_count, Some(3));
    assert!(section.paged.dirty);
    let row = favorites.overview_view_model(FavoriteItemType::Episode);
    assert_eq!(row.paged.items[0].id, "episode");
    assert_eq!(row.paged.items[1].id, "before");
    assert_eq!(row.paged.total_record_count, Some(3));
    assert!(row.paged.dirty);
    assert_eq!(data.revision, 2);
    assert!(!actions.has_pending());
}

#[test]
fn foreign_and_duplicate_results_cannot_finish_or_rollback_a_new_mutation() {
    let identity = WorkspaceIdentity::default();
    let mut actions = FavoriteActions::new(identity.clone());
    let mut data = UserDataState::default();
    let mut favorites = FavoritesController::new(identity.clone());
    let first = actions
        .toggle(
            intent("movie", false, None),
            false,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..Default::default()
    };
    assert!(
        actions
            .complete(
                &first,
                Err(anyhow::anyhow!("foreign")),
                &foreign,
                &mut data,
                &mut favorites
            )
            .is_none()
    );
    assert!(actions.has_pending() && data.overrides["movie"].is_favorite);
    assert_eq!(data.revision, 1);
    actions
        .complete(
            &first,
            Err(anyhow::anyhow!("failure")),
            &identity,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    assert!(data.overrides.is_empty());
    let second = actions
        .toggle(
            intent("movie", false, None),
            false,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    assert!(
        actions
            .complete(
                &first,
                Err(anyhow::anyhow!("duplicate")),
                &identity,
                &mut data,
                &mut favorites
            )
            .is_none()
    );
    assert!(actions.is_pending("movie"));
    assert_eq!(data.revision, 3);
    let response = UserItemData {
        is_favorite: true,
        playback_position_ticks: Some(99),
        ..Default::default()
    };
    actions
        .complete(
            &second,
            Ok(response.clone()),
            &identity,
            &mut data,
            &mut favorites,
        )
        .unwrap();
    assert_eq!(data.overrides["movie"], response);
    assert_eq!(data.revision, 4);
    assert!(
        actions
            .complete(
                &second,
                Err(anyhow::anyhow!("duplicate")),
                &identity,
                &mut data,
                &mut favorites
            )
            .is_none()
    );
    assert_eq!(data.revision, 4);
    assert!(!actions.has_pending());
}
