use super::controller::LibraryRequest;
use crate::{emby::UserItems, home::gateway::HomeGateway};

pub(in crate::home) fn run_library(
    gateway: &(impl HomeGateway + ?Sized),
    request: &LibraryRequest,
) -> anyhow::Result<UserItems> {
    gateway.user_items(&request.query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        effects::WorkspaceIdentity,
        emby::{SortOrder, UserItemsQuery, UserItemsSort, VideoItemType},
        home::library::controller::{LibraryController, LibraryIntent},
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeGateway {
        calls: Mutex<Vec<UserItemsQuery>>,
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

        fn set_favorite(&self, _: &str, _: bool) -> anyhow::Result<crate::emby::UserItemData> {
            panic!("unexpected favorite mutation")
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
            panic!("library must use the user items endpoint")
        }
        fn persons(
            &self,
            _: &crate::emby::UserItemsQuery,
        ) -> anyhow::Result<crate::emby::UserItems> {
            Ok(crate::emby::UserItems {
                items: Vec::new(),
                total_record_count: 0,
            })
        }
        fn user_items(&self, query: &UserItemsQuery) -> anyhow::Result<UserItems> {
            self.calls.lock().unwrap().push(query.clone());
            Ok(serde_json::from_value(serde_json::json!({
                "Items": [{"Id": format!("{:?}", query.sort_order), "Name": "Movie", "Type": "Movie"}],
                "TotalRecordCount": 1
            }))
            .unwrap())
        }
    }

    #[test]
    fn reordered_gateway_responses_only_commit_the_current_library_sort() {
        let identity = WorkspaceIdentity::default();
        let gateway = FakeGateway::default();
        let mut controller = LibraryController::new(
            vec![VideoItemType::Movie],
            "library".into(),
            identity.clone(),
        );
        let old = controller
            .dispatch(LibraryIntent::Open(vec![VideoItemType::Movie]), 3)
            .request
            .unwrap();
        let delayed = run_library(&gateway, &old);
        let current = controller
            .dispatch(LibraryIntent::SortOrder(SortOrder::Descending), 4)
            .request
            .unwrap();
        assert!(
            controller
                .complete(&current, run_library(&gateway, &current), &identity)
                .is_some()
        );
        assert!(controller.complete(&old, delayed, &identity).is_none());
        assert_eq!(controller.view_model().paged.items[0].id, "Descending");
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for query in calls.iter() {
            assert_eq!(query.parent_id.as_deref(), Some("library"));
            assert_eq!(query.include_item_types, [VideoItemType::Movie]);
            assert!(query.recursive);
            assert_eq!(query.start_index, 0);
            assert_eq!(query.limit, 60);
            assert_eq!(query.sort_by, Some(UserItemsSort::SortName));
        }
        assert_eq!(calls[0].sort_order, SortOrder::Ascending);
        assert_eq!(calls[1].sort_order, SortOrder::Descending);
    }
}
