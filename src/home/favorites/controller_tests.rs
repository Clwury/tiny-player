use super::*;
use crate::emby::UserItemData;
use std::collections::HashSet;

struct Harness {
    controller: FavoritesController,
    data: UserDataState,
    pending: HashSet<String>,
}

impl Harness {
    fn new() -> Self {
        Self {
            controller: FavoritesController::new(WorkspaceIdentity::default()),
            data: UserDataState::default(),
            pending: HashSet::new(),
        }
    }

    fn dispatch(&mut self, item_type: VideoItemType, intent: FavoritesIntent) -> FavoritesRequest {
        self.controller
            .dispatch(item_type, intent, self.data.revision)
            .unwrap()
    }

    fn complete(
        &mut self,
        request: &FavoritesRequest,
        result: anyhow::Result<UserItems>,
    ) -> Option<FavoritesUpdate> {
        self.controller.complete(
            request,
            result,
            &WorkspaceIdentity::default(),
            &mut self.data,
            PendingUserData {
                played: false,
                favorites: &self.pending,
            },
        )
    }
}

fn item(item_type: VideoItemType, id: &str, favorite: bool) -> UserItem {
    serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": item_type.as_str(), "UserData": {"IsFavorite": favorite}})).unwrap()
}

fn page(item_type: VideoItemType) -> UserItems {
    UserItems {
        items: (0..30)
            .map(|index| item(item_type, &format!("{}-{index}", item_type.as_str()), true))
            .collect(),
        total_record_count: 90,
    }
}

#[test]
fn categories_commit_independently_and_do_not_share_request_tokens() {
    let mut h = Harness::new();
    let movie = h.dispatch(VideoItemType::Movie, FavoritesIntent::Enter);
    let series = h.dispatch(VideoItemType::Series, FavoritesIntent::Enter);
    assert!(
        h.controller
            .dispatch(VideoItemType::Series, FavoritesIntent::Enter, 0)
            .is_none()
    );
    let forged = FavoritesRequest {
        item_type: VideoItemType::Series,
        ..movie.clone()
    };
    assert!(
        h.complete(&forged, Ok(page(VideoItemType::Movie)))
            .is_none()
    );
    assert!(h.data.overrides.is_empty());
    let update = h
        .complete(&series, Ok(page(VideoItemType::Series)))
        .unwrap();
    assert_eq!(update.images.unwrap().items.len(), 30);
    assert_eq!(
        h.controller.view_model(VideoItemType::Movie).paged.initial,
        LoadState::Loading
    );
    h.complete(&movie, Err(anyhow::anyhow!("offline"))).unwrap();
    assert!(h.controller.view_model(VideoItemType::Movie).can_retry);
    assert_eq!(
        h.controller
            .view_model(VideoItemType::Series)
            .paged
            .items
            .len(),
        30
    );
    assert!(
        h.complete(&series, Err(anyhow::anyhow!("duplicate")))
            .is_none()
    );
    assert!(
        h.controller
            .view_model(VideoItemType::Series)
            .paged
            .initial_error
            .is_none()
    );
}

#[test]
fn dirty_pages_and_foreign_workspaces_cannot_mutate_overrides_or_emit_followups() {
    let mut h = Harness::new();
    let old = h.dispatch(VideoItemType::Episode, FavoritesIntent::Enter);
    let foreign = WorkspaceIdentity {
        user_id: Some("foreign".into()),
        ..Default::default()
    };
    assert!(
        h.controller
            .complete(
                &old,
                Ok(page(VideoItemType::Episode)),
                &foreign,
                &mut h.data,
                PendingUserData {
                    played: false,
                    favorites: &h.pending
                }
            )
            .is_none()
    );
    h.controller.mark_dirty();
    for result in [
        Ok(page(VideoItemType::Episode)),
        Err(anyhow::anyhow!("stale")),
    ] {
        assert!(h.complete(&old, result).is_none());
    }
    assert!(h.data.overrides.is_empty());
    let current = h.dispatch(VideoItemType::Episode, FavoritesIntent::Enter);
    h.complete(&current, Ok(page(VideoItemType::Episode)))
        .unwrap();
    let (kind, index, removed) = h.controller.remove_item("Episode-5").unwrap();
    h.controller.mark_dirty();
    assert!(
        h.controller
            .dispatch(kind, FavoritesIntent::LoadMore { automatic: false }, 0)
            .is_none()
    );
    h.controller.restore_item(kind, index, removed);
    let vm = h.controller.view_model(kind);
    assert_eq!(vm.paged.items[5].id, "Episode-5");
    assert_eq!(vm.paged.items.len(), 30);
    assert_eq!(vm.paged.total_record_count, Some(90));
    assert!(vm.paged.dirty);
}

#[test]
fn overlay_reconciliation_precedes_favorite_filtering_and_raw_paging() {
    let mut h = Harness::new();
    let request = h.dispatch(VideoItemType::Movie, FavoritesIntent::Enter);
    h.data.bump("recently-removed");
    h.data
        .overrides
        .insert("recently-removed".into(), UserItemData::default());
    h.pending.insert("pending-removal".into());
    h.data
        .overrides
        .insert("pending-removal".into(), UserItemData::default());
    let mut response = page(VideoItemType::Movie);
    response.items[0] = item(VideoItemType::Movie, "recently-removed", true);
    response.items[1] = item(VideoItemType::Movie, "pending-removal", true);
    response.items[2] = item(VideoItemType::Movie, "duplicate", false);
    response.items[3] = item(VideoItemType::Movie, "duplicate", true);
    response.items[4] = item(VideoItemType::Series, "foreign-category", true);
    response.items[5] = item(VideoItemType::Movie, " ", true);
    let update = h.complete(&request, Ok(response)).unwrap();
    let images = update.images.unwrap();
    assert_eq!(images.items.len(), 26);
    assert_eq!(
        images
            .items
            .iter()
            .filter(|item| item.id == "duplicate")
            .count(),
        2
    );
    assert!(h.data.overrides["duplicate"].is_favorite);
    assert!(!h.data.overrides["recently-removed"].is_favorite);
    assert!(!h.data.overrides["pending-removal"].is_favorite);
    assert!(!h.data.overrides.contains_key("foreign-category"));
    let vm = h.controller.view_model(VideoItemType::Movie);
    assert_eq!(vm.paged.items.len(), 25);
    assert_eq!(vm.paged.next_start_index, 30);
    let more = h.dispatch(
        VideoItemType::Movie,
        FavoritesIntent::LoadMore { automatic: true },
    );
    assert_eq!(more.start_index, 30);
    assert_eq!(more.user_data_revision, 1);
}

#[test]
fn refresh_failure_retains_data_and_pagination_retry_keeps_the_failed_offset() {
    let mut h = Harness::new();
    let kind = VideoItemType::Series;
    let first = h.dispatch(kind, FavoritesIntent::Enter);
    h.complete(&first, Ok(page(kind))).unwrap();
    let refresh = h.dispatch(kind, FavoritesIntent::Refresh);
    let failed = h
        .complete(&refresh, Err(anyhow::anyhow!("refresh offline")))
        .unwrap();
    assert!(failed.images.is_none());
    assert_eq!(
        failed.failure,
        Some(("refresh", "刷新失败：refresh offline".into()))
    );
    assert_eq!(h.controller.view_model(kind).paged.items.len(), 30);
    assert_eq!(h.controller.view_model(kind).paged.next_start_index, 30);
    let more = h.dispatch(kind, FavoritesIntent::LoadMore { automatic: true });
    let failed = h
        .complete(&more, Err(anyhow::anyhow!("more offline")))
        .unwrap();
    assert_eq!(failed.failure, Some(("load-more", "more offline".into())));
    assert!(
        h.controller
            .dispatch(kind, FavoritesIntent::LoadMore { automatic: true }, 0)
            .is_none()
    );
    let retry = h.dispatch(kind, FavoritesIntent::LoadMore { automatic: false });
    assert_eq!(retry.start_index, more.start_index);
    assert!(h.complete(&more, Ok(page(kind))).is_none());
    let response = UserItems {
        items: vec![item(kind, "last", true)],
        total_record_count: 31,
    };
    h.complete(&retry, Ok(response)).unwrap();
    assert_eq!(h.controller.view_model(kind).paged.items.len(), 31);
    assert!(h.controller.view_model(kind).paged.exhausted);
}
