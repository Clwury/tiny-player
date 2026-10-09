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

    fn dispatch(
        &mut self,
        item_type: FavoriteItemType,
        intent: FavoritesIntent,
    ) -> FavoritesRequest {
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

fn item(item_type: FavoriteItemType, id: &str, favorite: bool) -> UserItem {
    serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": item_type.as_str(), "UserData": {"IsFavorite": favorite}})).unwrap()
}

fn page(item_type: FavoriteItemType) -> UserItems {
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
    let movie = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    let series = h.dispatch(FavoriteItemType::Series, FavoritesIntent::Enter);
    assert!(
        h.controller
            .dispatch(FavoriteItemType::Series, FavoritesIntent::Enter, 0)
            .is_none()
    );
    let forged = FavoritesRequest {
        item_type: FavoriteItemType::Series,
        ..movie.clone()
    };
    assert!(
        h.complete(&forged, Ok(page(FavoriteItemType::Movie)))
            .is_none()
    );
    assert!(h.data.overrides.is_empty());
    let update = h
        .complete(&series, Ok(page(FavoriteItemType::Series)))
        .unwrap();
    assert_eq!(update.images.unwrap().items.len(), 30);
    assert_eq!(
        h.controller
            .view_model(FavoriteItemType::Movie)
            .paged
            .initial,
        LoadState::Loading
    );
    h.complete(&movie, Err(anyhow::anyhow!("offline"))).unwrap();
    assert!(h.controller.view_model(FavoriteItemType::Movie).can_retry);
    assert_eq!(
        h.controller
            .view_model(FavoriteItemType::Series)
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
            .view_model(FavoriteItemType::Series)
            .paged
            .initial_error
            .is_none()
    );
}

#[test]
fn dirty_pages_and_foreign_workspaces_cannot_mutate_overrides_or_emit_followups() {
    let mut h = Harness::new();
    let old = h.dispatch(FavoriteItemType::Episode, FavoritesIntent::Enter);
    let foreign = WorkspaceIdentity {
        user_id: Some("foreign".into()),
        ..Default::default()
    };
    assert!(
        h.controller
            .complete(
                &old,
                Ok(page(FavoriteItemType::Episode)),
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
        Ok(page(FavoriteItemType::Episode)),
        Err(anyhow::anyhow!("stale")),
    ] {
        assert!(h.complete(&old, result).is_none());
    }
    assert!(h.data.overrides.is_empty());
    let current = h.dispatch(FavoriteItemType::Episode, FavoritesIntent::Enter);
    h.complete(&current, Ok(page(FavoriteItemType::Episode)))
        .unwrap();
    let kind = FavoriteItemType::Episode;
    let removed = h.controller.remove_item("Episode-5");
    assert_eq!(removed.len(), 1);
    h.controller.mark_dirty();
    assert!(
        h.controller
            .dispatch(kind, FavoritesIntent::LoadMore { automatic: false }, 0)
            .is_none()
    );
    h.controller.restore_item(removed);
    let vm = h.controller.view_model(kind);
    assert_eq!(vm.paged.items[5].id, "Episode-5");
    assert_eq!(vm.paged.items.len(), 30);
    assert_eq!(vm.paged.total_record_count, Some(90));
    assert!(vm.paged.dirty);
}

#[test]
fn overlay_reconciliation_precedes_favorite_filtering_and_raw_paging() {
    let mut h = Harness::new();
    let request = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    h.data.bump("recently-removed");
    h.data
        .overrides
        .insert("recently-removed".into(), UserItemData::default());
    h.pending.insert("pending-removal".into());
    h.data
        .overrides
        .insert("pending-removal".into(), UserItemData::default());
    let mut response = page(FavoriteItemType::Movie);
    response.items[0] = item(FavoriteItemType::Movie, "recently-removed", true);
    response.items[1] = item(FavoriteItemType::Movie, "pending-removal", true);
    response.items[2] = item(FavoriteItemType::Movie, "duplicate", false);
    response.items[3] = item(FavoriteItemType::Movie, "duplicate", true);
    response.items[4] = item(FavoriteItemType::Series, "foreign-category", true);
    response.items[5] = item(FavoriteItemType::Movie, " ", true);
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
    let vm = h.controller.view_model(FavoriteItemType::Movie);
    assert_eq!(vm.paged.items.len(), 25);
    assert_eq!(vm.paged.next_start_index, 30);
    let more = h.dispatch(
        FavoriteItemType::Movie,
        FavoritesIntent::LoadMore { automatic: true },
    );
    assert_eq!(more.start_index, 30);
    assert_eq!(more.user_data_revision, 1);
}

#[test]
fn refresh_failure_retains_data_and_pagination_retry_keeps_the_failed_offset() {
    let mut h = Harness::new();
    let kind = FavoriteItemType::Series;
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

#[test]
fn favorite_queries_load_each_type_independently_with_ascending_name() {
    for item_type in FAVORITE_ITEM_TYPES {
        let mut controller = FavoritesController::new(WorkspaceIdentity::default());
        let mut request = controller
            .dispatch(item_type, FavoritesIntent::Enter, 0)
            .unwrap();
        request.start_index = 30;
        let query = favorite_query(&request);
        assert_eq!(query.include_item_types, item_type.video_types());
        assert_eq!(query.is_favorite, Some(true));
        assert!(query.recursive);
        assert_eq!(query.start_index, 30);
        assert_eq!(query.limit, 30);
        assert_eq!(query.sort_by, Some(UserItemsSort::SortName));
        assert_eq!(query.sort_order, SortOrder::Ascending);
        let fields = query.fields.as_deref().unwrap();
        for field in [
            "BasicSyncInfo",
            "CommunityRating",
            "ProductionYear",
            "EndDate",
            "Container",
            "SeriesId",
            "SeriesName",
        ] {
            assert!(fields.split(',').any(|value| value == field));
        }
    }
}

#[test]
fn favorite_people_filter_foreign_items_and_sort_cancels_stale_pages_in_all_categories() {
    let mut h = Harness::new();
    let movie = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    let person = h.dispatch(FavoriteItemType::Person, FavoritesIntent::Enter);
    let mut response = page(FavoriteItemType::Person);
    response.items[0] = item(FavoriteItemType::Movie, "foreign-movie", true);
    response.items[1] = item(FavoriteItemType::Person, " ", true);
    h.complete(&person, Ok(response)).unwrap();
    let section = h.controller.view_model(FavoriteItemType::Person);
    assert_eq!(section.paged.items.len(), 28);
    assert_eq!(section.paged.next_start_index, 30);
    let more = h.dispatch(
        FavoriteItemType::Person,
        FavoritesIntent::LoadMore { automatic: false },
    );
    let sorted = h.dispatch(
        FavoriteItemType::Person,
        FavoritesIntent::SortBy(UserItemsSort::DateCreated),
    );
    assert!(
        h.complete(&more, Ok(page(FavoriteItemType::Person)))
            .is_none()
    );
    assert!(
        h.controller
            .view_model(FavoriteItemType::Person)
            .paged
            .items
            .is_empty()
    );
    assert!(
        h.complete(&sorted, Ok(page(FavoriteItemType::Person)))
            .is_some()
    );
    assert!(
        h.complete(&movie, Ok(page(FavoriteItemType::Movie)))
            .is_none()
    );
    let query = favorite_query(&sorted);
    assert!(query.include_item_types.is_empty());
    assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(query.start_index, 0);
    assert_eq!(
        h.controller.view_model(FavoriteItemType::Movie).sort_by,
        UserItemsSort::DateCreated
    );
    let movie = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    assert_eq!(movie.sort_by, sorted.sort_by);
    assert_eq!(movie.sort_order, sorted.sort_order);
    assert!(
        h.complete(&movie, Ok(page(FavoriteItemType::Movie)))
            .is_some()
    );
}

#[test]
fn favorite_sort_replaces_pending_pages_and_shares_choices_across_categories() {
    let mut h = Harness::new();
    let movie = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    let series = h.dispatch(FavoriteItemType::Series, FavoritesIntent::Enter);
    h.complete(&movie, Ok(page(FavoriteItemType::Movie)))
        .unwrap();
    let old_more = h.dispatch(
        FavoriteItemType::Movie,
        FavoritesIntent::LoadMore { automatic: true },
    );
    let sorted = h.dispatch(
        FavoriteItemType::Movie,
        FavoritesIntent::SortBy(UserItemsSort::DateCreated),
    );
    assert_eq!(sorted.start_index, 0);
    assert!(sorted.initial);
    assert_eq!(sorted.sort_by, UserItemsSort::DateCreated);
    assert_eq!(sorted.sort_order, SortOrder::Ascending);
    assert!(
        h.controller
            .view_model(FavoriteItemType::Movie)
            .paged
            .items
            .is_empty()
    );
    for result in [
        Ok(page(FavoriteItemType::Movie)),
        Err(anyhow::anyhow!("stale")),
    ] {
        assert!(h.complete(&old_more, result).is_none());
    }
    assert!(
        h.complete(&series, Ok(page(FavoriteItemType::Series)))
            .is_none()
    );
    assert_eq!(
        h.controller.view_model(FavoriteItemType::Series).sort_by,
        UserItemsSort::DateCreated
    );
    assert_eq!(
        h.controller
            .view_model(FavoriteItemType::Series)
            .paged
            .items
            .len(),
        0
    );
    let descending = h.dispatch(
        FavoriteItemType::Movie,
        FavoritesIntent::SortOrder(SortOrder::Descending),
    );
    assert!(
        h.complete(&sorted, Ok(page(FavoriteItemType::Movie)))
            .is_none()
    );
    h.complete(&descending, Ok(page(FavoriteItemType::Movie)))
        .unwrap();
    let more = h.dispatch(
        FavoriteItemType::Movie,
        FavoritesIntent::LoadMore { automatic: true },
    );
    let query = favorite_query(&more);
    assert_eq!(query.start_index, 30);
    assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
    assert_eq!(query.sort_order, SortOrder::Descending);
    assert_eq!(query.is_favorite, Some(true));
    assert_eq!(
        query.include_item_types,
        FavoriteItemType::Movie.video_types()
    );
    assert!(
        h.complete(&more, Ok(page(FavoriteItemType::Movie)))
            .is_some()
    );
    h.controller.mark_dirty();
    let refresh = h.dispatch(FavoriteItemType::Movie, FavoritesIntent::Enter);
    assert_eq!(refresh.sort_by, UserItemsSort::DateCreated);
    assert_eq!(refresh.sort_order, SortOrder::Descending);
    assert_eq!(refresh.start_index, 0);
}

#[test]
fn favorite_sort_keeps_current_requests_and_limits_last_episode_sort_to_series() {
    let mut h = Harness::new();
    for kind in FAVORITE_ITEM_TYPES {
        let request = h.dispatch(kind, FavoritesIntent::Enter);
        assert!(
            h.controller
                .dispatch(kind, FavoritesIntent::SortBy(UserItemsSort::SortName), 0)
                .is_none()
        );
        assert!(
            h.controller
                .dispatch(kind, FavoritesIntent::SortOrder(SortOrder::Ascending), 0)
                .is_none()
        );
        assert!(
            h.controller
                .view_model(kind)
                .paged
                .accepts_initial(&request.token)
        );
        h.complete(&request, Ok(page(kind))).unwrap();
    }
    let series = h.dispatch(
        FavoriteItemType::Series,
        FavoritesIntent::SortBy(UserItemsSort::DateLastContentAdded),
    );
    assert_eq!(
        favorite_query(&series).sort_by,
        Some(UserItemsSort::DateLastContentAdded)
    );
    assert!(favorite_query(&series).secondary_sort_by.is_empty());
    for kind in FAVORITE_ITEM_TYPES {
        assert_eq!(
            h.controller.view_model(kind).sort_by,
            if kind == FavoriteItemType::Series {
                UserItemsSort::DateLastContentAdded
            } else {
                UserItemsSort::SortName
            }
        );
        assert!(
            h.controller
                .dispatch(
                    kind,
                    FavoritesIntent::SortBy(UserItemsSort::DateLastContentAdded),
                    0
                )
                .is_none()
        );
    }
    assert!(
        h.controller
            .view_model(FavoriteItemType::Series)
            .paged
            .accepts_initial(&series.token)
    );
}

#[test]
fn overview_requests_and_cached_order_are_independent_of_shared_more_sort() {
    for kind in FAVORITE_ITEM_TYPES {
        let mut h = Harness::new();
        let row = h
            .controller
            .dispatch_overview(kind, FavoritesIntent::Enter, 0)
            .unwrap();
        let old_more = h.dispatch(kind, FavoritesIntent::Enter);
        h.controller.set_sort(crate::media::ItemSortPreferences {
            sort_by: UserItemsSort::PremiereDate,
            sort_order: SortOrder::Descending,
        });
        let more = h.dispatch(kind, FavoritesIntent::Enter);
        assert_eq!(more.sort_by, UserItemsSort::PremiereDate);
        assert_eq!(more.sort_order, SortOrder::Descending);
        let query = favorite_query(&row);
        assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
        assert_eq!(
            query.secondary_sort_by,
            [UserItemsSort::DateLastContentAdded, UserItemsSort::SortName]
        );
        assert!(favorite_query(&more).secondary_sort_by.is_empty());
        assert_eq!(query.sort_order, SortOrder::Descending);
        assert_eq!(query.is_favorite, Some(true));
        assert_eq!(query.start_index, 0);
        assert_eq!(query.limit, FAVORITES_PAGE_LIMIT);
        let forged = FavoritesRequest {
            source: FavoritesSource::Items,
            ..row.clone()
        };
        assert!(h.complete(&forged, Ok(page(kind))).is_none());
        assert!(h.complete(&old_more, Ok(page(kind))).is_none());
        assert!(h.complete(&row, Ok(page(kind))).is_some());
        let mut reversed = page(kind);
        reversed.items.reverse();
        h.complete(&more, Ok(reversed)).unwrap();
        assert_eq!(
            h.controller.overview_view_model(kind).paged.items[0].id,
            format!("{}-0", kind.as_str())
        );
        assert_eq!(
            h.controller.view_model(kind).paged.items[0].id,
            format!("{}-29", kind.as_str())
        );

        let sorted = h.dispatch(kind, FavoritesIntent::SortBy(UserItemsSort::DateCreated));
        assert!(
            h.controller
                .dispatch_overview(kind, FavoritesIntent::Enter, 0)
                .is_none()
        );
        let row_vm = h.controller.overview_view_model(kind);
        assert!(!row_vm.paged.dirty);
        assert_eq!(row_vm.paged.items.len(), 30);
        assert_eq!(row_vm.sort_by, UserItemsSort::DateCreated);
        assert_eq!(row_vm.sort_order, SortOrder::Descending);
        assert!(
            h.controller
                .dispatch_overview(kind, FavoritesIntent::SortOrder(SortOrder::Descending), 0)
                .is_none()
        );
        assert!(
            h.controller
                .view_model(kind)
                .paged
                .accepts_initial(&sorted.token)
        );
        h.controller.mark_dirty();
        let refreshed_row = h
            .controller
            .dispatch_overview(kind, FavoritesIntent::Enter, 0)
            .unwrap();
        assert_eq!(refreshed_row.sort_by, UserItemsSort::DateCreated);
        assert_eq!(refreshed_row.sort_order, SortOrder::Descending);
        let refreshed_more = h.dispatch(kind, FavoritesIntent::Enter);
        assert_eq!(refreshed_more.sort_by, UserItemsSort::DateCreated);
        assert_eq!(refreshed_more.sort_order, SortOrder::Descending);
        assert!(h.complete(&sorted, Ok(page(kind))).is_none());
    }
}
