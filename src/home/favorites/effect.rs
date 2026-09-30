use super::controller::{FavoritesRequest, favorite_query};
use crate::{emby::UserItems, home::gateway::HomeGateway};

pub(super) fn run_favorite_mutation(
    gateway: &impl HomeGateway,
    command: &super::actions::FavoriteCommand,
) -> anyhow::Result<crate::emby::UserItemData> {
    gateway.set_favorite(&command.item_id, command.desired)
}

pub(super) fn run_favorites(
    gateway: &impl HomeGateway,
    request: &FavoritesRequest,
) -> anyhow::Result<UserItems> {
    gateway.user_items(&favorite_query(request.item_type, request.start_index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        effects::WorkspaceIdentity,
        emby::{SortOrder, UserItemsQuery, UserItemsSort, VideoItemType},
        home::{
            favorites::controller::{FavoritesController, FavoritesIntent},
            model::user_data::{PendingUserData, UserDataState},
        },
    };
    use std::{collections::HashSet, sync::Mutex};

    #[derive(Default)]
    struct FakeGateway {
        calls: Mutex<Vec<UserItemsQuery>>,
        mutations: Mutex<Vec<(String, bool)>>,
        fail_mutation: bool,
    }

    impl HomeGateway for FakeGateway {
        fn similar_items(&self, _: &str) -> anyhow::Result<crate::emby::UserItems> {
            panic!("unexpected similar request")
        }
        fn show_seasons(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
            panic!("unexpected seasons request")
        }
        fn show_next_up(&self, _: &str) -> anyhow::Result<crate::emby::MediaItems> {
            panic!("unexpected next-up request")
        }
        fn playback_media_sources(&self, _: &str) -> anyhow::Result<Vec<crate::emby::MediaSource>> {
            panic!("unexpected sources request")
        }

        fn set_played(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
            panic!("unexpected played mutation")
        }
        fn media_item(&self, _: &str) -> anyhow::Result<crate::emby::MediaItem> {
            panic!("unexpected media item request")
        }
        fn show_episodes(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<crate::emby::MediaItems> {
            panic!("unexpected episodes request")
        }

        fn mark_item_played(&self, _: &str) -> anyhow::Result<crate::emby::UserItemData> {
            panic!("unexpected resume mutation")
        }
        fn hide_item_from_resume(&self, _: &str) -> anyhow::Result<()> {
            panic!("unexpected resume mutation")
        }

        fn set_favorite(
            &self,
            id: &str,
            desired: bool,
        ) -> anyhow::Result<crate::emby::UserItemData> {
            self.mutations.lock().unwrap().push((id.into(), desired));
            if self.fail_mutation {
                anyhow::bail!("offline");
            }
            Ok(crate::emby::UserItemData {
                is_favorite: desired,
                ..Default::default()
            })
        }
        fn user_views(&self) -> anyhow::Result<crate::emby::UserViews> {
            panic!("unexpected endpoint")
        }
        fn resume_items(&self) -> anyhow::Result<crate::emby::ResumeItems> {
            panic!("unexpected endpoint")
        }
        fn latest_items(
            &self,
            _: &str,
            _: &[VideoItemType],
            _: u32,
        ) -> anyhow::Result<Vec<crate::emby::UserItem>> {
            panic!("unexpected endpoint")
        }
        fn search_items(&self, _: &str, _: u32, _: u32) -> anyhow::Result<UserItems> {
            panic!("favorites must use the user items endpoint")
        }
        fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
            self.calls.lock().unwrap().push(query.clone());
            let kind = query.include_item_types[0].as_str();
            Ok(serde_json::from_value(serde_json::json!({
                "Items": [{"Id": kind, "Name": kind, "Type": kind, "UserData": {"IsFavorite": true}}], "TotalRecordCount": 1
            })).unwrap())
        }
    }

    #[test]
    fn favorite_gateway_failure_rolls_back_then_retry_uses_the_same_desired_value() {
        use crate::home::{
            favorites::actions::{FavoriteActions, ToggleFavorite},
            model::notification::{ActionNotification, NotificationScope},
        };
        let identity = WorkspaceIdentity::default();
        let mut actions = FavoriteActions::new(identity.clone());
        let mut favorites = FavoritesController::new(identity.clone());
        let mut data = UserDataState::default();
        let mut gateway = FakeGateway {
            fail_mutation: true,
            ..Default::default()
        };
        for failed in [true, false] {
            gateway.fail_mutation = failed;
            let command = actions
                .toggle(
                    ToggleFavorite {
                        item_id: "movie".into(),
                        fallback: None,
                        in_favorites: false,
                        notification: ActionNotification {
                            scope: NotificationScope::Home,
                            key: "home:favorite:movie".into(),
                        },
                    },
                    false,
                    &mut data,
                    &mut favorites,
                )
                .unwrap();
            let update = actions
                .complete(
                    &command,
                    run_favorite_mutation(&gateway, &command),
                    &identity,
                    &mut data,
                    &mut favorites,
                )
                .unwrap();
            assert_eq!(update.failure.is_some(), failed);
            assert_eq!(data.overrides.contains_key("movie"), !failed);
            assert!(!actions.has_pending());
        }
        assert_eq!(
            *gateway.mutations.lock().unwrap(),
            [("movie".into(), true), ("movie".into(), true)]
        );
        assert!(data.overrides["movie"].is_favorite);
        assert!(gateway.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn refresh_cancels_delayed_gateway_results_without_changing_favorite_query_metadata() {
        let identity = WorkspaceIdentity::default();
        let mut controller = FavoritesController::new(identity.clone());
        let mut data = UserDataState::default();
        let favorites = HashSet::new();
        let pending = PendingUserData {
            played: false,
            favorites: &favorites,
        };
        let gateway = FakeGateway::default();
        let kind = VideoItemType::Episode;
        let old = controller
            .dispatch(kind, FavoritesIntent::Enter, 0)
            .unwrap();
        let delayed = run_favorites(&gateway, &old);
        controller.mark_dirty();
        let current = controller
            .dispatch(kind, FavoritesIntent::Refresh, 1)
            .unwrap();
        assert!(
            controller
                .complete(&old, delayed, &identity, &mut data, pending)
                .is_none()
        );
        assert!(data.overrides.is_empty());
        let update = controller
            .complete(
                &current,
                run_favorites(&gateway, &current),
                &identity,
                &mut data,
                pending,
            )
            .unwrap();
        assert_eq!(update.images.unwrap().items[0].id, "Episode");
        assert!(data.overrides["Episode"].is_favorite);
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for query in calls.iter() {
            assert_eq!(query.include_item_types, [kind]);
            assert_eq!(query.is_favorite, Some(true));
            assert!(query.recursive);
            assert_eq!(query.parent_id, None);
            assert_eq!(query.start_index, 0);
            assert_eq!(query.limit, 30);
            assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
            assert_eq!(query.sort_order, SortOrder::Descending);
            assert_eq!(
                query.fields.as_deref(),
                Some(
                    "BasicSyncInfo,CommunityRating,ProductionYear,EndDate,Container,ParentId,SeriesId,SeriesName,ParentIndexNumber,IndexNumber,UserData"
                )
            );
        }
    }
}
