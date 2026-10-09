use crate::home::{
    gateway::HomeGateway,
    search::model::{SEARCH_LIMIT, SearchPage, SearchRequest},
};

pub(super) fn run_search(
    gateway: &(impl HomeGateway + ?Sized),
    request: &SearchRequest,
) -> anyhow::Result<SearchPage> {
    gateway
        .search_items(&request.query, request.start_index, SEARCH_LIMIT)
        .map(SearchPage::from_response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        effects::WorkspaceIdentity,
        emby::UserItems,
        home::{
            model::LoadState,
            search::controller::{SearchController, SearchIntent},
        },
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeGateway {
        calls: Mutex<Vec<(String, u32, u32)>>,
        failed: bool,
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
            _: &[crate::emby::VideoItemType],
            _: u32,
        ) -> anyhow::Result<Vec<crate::emby::UserItem>> {
            panic!("unexpected endpoint")
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
        fn user_items(&self, _: &crate::emby::UserItemsQuery) -> anyhow::Result<UserItems> {
            panic!("search must use the search endpoint");
        }
        fn search_items(
            &self,
            query: &str,
            start_index: u32,
            limit: u32,
        ) -> anyhow::Result<UserItems> {
            self.calls
                .lock()
                .unwrap()
                .push((query.into(), start_index, limit));
            if self.failed {
                anyhow::bail!("offline");
            }
            Ok(serde_json::from_value(serde_json::json!({
                "Items": [{"Id": query, "Name": query, "Type": "Movie"}],
                "TotalRecordCount": 1
            }))
            .unwrap())
        }
    }

    #[test]
    fn reordered_gateway_responses_only_commit_the_current_search() {
        let gateway = FakeGateway::default();
        let identity = WorkspaceIdentity::default();
        let mut controller = SearchController::new(identity.clone());
        let old = controller
            .dispatch(SearchIntent::Submit("old".into()), 1)
            .request
            .unwrap();
        let delayed = run_search(&gateway, &old.request);
        let current = controller
            .dispatch(SearchIntent::Submit("new".into()), 2)
            .request
            .unwrap();
        assert!(
            controller
                .complete(&current, run_search(&gateway, &current.request), &identity)
                .is_some()
        );
        assert!(controller.complete(&old, delayed, &identity).is_none());
        assert_eq!(controller.view_model().items[0].id, "new");
        assert_eq!(
            *gateway.calls.lock().unwrap(),
            vec![("old".into(), 0, 30), ("new".into(), 0, 30)]
        );
    }

    #[test]
    fn cancelled_failure_cannot_overwrite_new_query_or_create_error_state() {
        let gateway = FakeGateway {
            failed: true,
            ..Default::default()
        };
        let identity = WorkspaceIdentity::default();
        let mut controller = SearchController::new(identity.clone());
        let old = controller
            .dispatch(SearchIntent::Submit("old".into()), 1)
            .request
            .unwrap();
        let delayed_failure = run_search(&gateway, &old.request);
        controller.dispatch(SearchIntent::InputChanged("new".into()), 2);
        assert!(
            controller
                .complete(&old, delayed_failure, &identity)
                .is_none()
        );
        assert_eq!(controller.test_state().initial, LoadState::Idle);
        assert!(controller.test_state().initial_error.is_none());
        let current = controller
            .dispatch(SearchIntent::Submit("new".into()), 2)
            .request
            .unwrap();
        let update = controller
            .complete(&current, run_search(&gateway, &current.request), &identity)
            .unwrap();
        assert_eq!(controller.test_state().initial, LoadState::Failed);
        assert_eq!(update.error.as_deref(), Some("offline"));
    }
}
