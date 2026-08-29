use gpui::{AppContext as _, ClickEvent, Context, SharedString, Window};

use crate::emby::{
    ResumeItem, ResumeItems, SortOrder, UserItem, UserItemData, UserItems, UserItemsQuery,
    UserItemsSort, VideoItemType,
};

use super::{
    HomeContent, LoadState,
    navigation::{HomeRoot, HomeRoute},
    notification::{
        FAVORITES_INITIAL_NOTIFICATION_KEY, FAVORITES_LOAD_MORE_NOTIFICATION_KEY,
        FAVORITES_REFRESH_NOTIFICATION_KEY, NotificationScope,
    },
    paged_items::PAGED_ITEMS_LIMIT,
};

#[derive(Clone, Debug)]
pub(crate) struct FavoriteRollback {
    previous_override: Option<UserItemData>,
    removed: Option<(usize, UserItem)>,
}

impl HomeContent {
    pub(super) fn enter_favorites_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.favorites.initial == LoadState::Idle
            || self.favorites.initial == LoadState::Failed
            || self.favorites.dirty
        {
            self.load_favorites_initial(cx);
        }
    }

    pub(super) fn auto_load_more_favorites(&mut self, cx: &mut Context<Self>) {
        if self.navigation.current() != &HomeRoute::Root(HomeRoot::Favorites) {
            return;
        }
        self.load_more_favorites(cx);
    }

    pub(super) fn load_more_favorites(&mut self, cx: &mut Context<Self>) {
        let Some((generation, start_index)) = self.favorites.begin_load_more() else {
            return;
        };
        self.clear_notification(
            NotificationScope::Favorites,
            FAVORITES_LOAD_MORE_NOTIFICATION_KEY,
        );
        cx.notify();
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move {
            client.query_user_items(&server, &favorite_query(start_index))
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_favorites_load_more(
                    identity,
                    user_data_revision,
                    generation,
                    start_index,
                    result,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    fn load_favorites_initial(&mut self, cx: &mut Context<Self>) {
        let clear = self.favorites.items.is_empty();
        let Some(generation) = self.favorites.begin_initial(clear) else {
            return;
        };
        self.clear_notifications_for_scope(NotificationScope::Favorites);
        cx.notify();
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task = cx
            .background_spawn(async move { client.query_user_items(&server, &favorite_query(0)) });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_favorites_initial(identity, user_data_revision, generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_favorites_initial(
        &mut self,
        identity: super::WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        mut result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        if !self.favorites.accepts_initial(generation) {
            return;
        }
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            items.items.retain(is_movie_or_series);
            self.absorb_user_items_user_data(items, user_data_revision);
            items.items.retain(|item| {
                self.user_data_overrides
                    .get(&item.id)
                    .is_none_or(|data| data.is_favorite)
            });
            self.ensure_user_items_images(items, cx);
        }
        let finished = self.favorites.finish_initial_with_raw_count(
            generation,
            result,
            PAGED_ITEMS_LIMIT,
            raw_count,
        );
        if finished {
            if self.favorites.initial == LoadState::Failed {
                let (key, message): (&str, SharedString) =
                    if let Some(error) = self.favorites.initial_error.clone() {
                        (
                            FAVORITES_INITIAL_NOTIFICATION_KEY,
                            format!("加载收藏失败：{error}").into(),
                        )
                    } else if let Some(error) = self.favorites.refresh_error.clone() {
                        (FAVORITES_REFRESH_NOTIFICATION_KEY, error)
                    } else {
                        (FAVORITES_INITIAL_NOTIFICATION_KEY, "加载收藏失败".into())
                    };
                self.push_error_notification(NotificationScope::Favorites, key, message, cx);
            } else {
                self.clear_notifications_for_scope(NotificationScope::Favorites);
            }
            cx.notify();
        }
    }

    fn finish_favorites_load_more(
        &mut self,
        identity: super::WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        start_index: u32,
        mut result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        if !self.favorites.accepts_load_more(generation, start_index) {
            return;
        }
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            items.items.retain(is_movie_or_series);
            self.absorb_user_items_user_data(items, user_data_revision);
            items.items.retain(|item| {
                self.user_data_overrides
                    .get(&item.id)
                    .is_none_or(|data| data.is_favorite)
            });
            self.ensure_user_items_images(items, cx);
        }
        let finished = self.favorites.finish_load_more_with_raw_count(
            generation,
            start_index,
            result,
            PAGED_ITEMS_LIMIT,
            raw_count,
        );
        if finished {
            if self.favorites.load_more == LoadState::Failed {
                if let Some(error) = self.favorites.load_more_error.clone() {
                    self.push_error_notification(
                        NotificationScope::Favorites,
                        FAVORITES_LOAD_MORE_NOTIFICATION_KEY,
                        format!("加载更多收藏失败：{error}"),
                        cx,
                    );
                }
            } else {
                self.clear_notification(
                    NotificationScope::Favorites,
                    FAVORITES_LOAD_MORE_NOTIFICATION_KEY,
                );
            }
            cx.notify();
        }
    }

    pub(super) fn toggle_detail_favorite(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item_id) = self
            .series_detail
            .as_ref()
            .map(|detail| detail.series_id.clone())
        else {
            return;
        };
        if self.favorite_requests.contains(&item_id) {
            return;
        }
        let fallback = self
            .series_detail
            .as_ref()
            .and_then(|detail| detail.item.as_ref())
            .and_then(|item| item.user_data.as_ref());
        let old = self
            .effective_user_data(&item_id, fallback)
            .cloned()
            .unwrap_or_default();
        let desired = !old.is_favorite;
        let mut optimistic = old;
        optimistic.is_favorite = desired;
        self.invalidate_pending_home_snapshot_save();
        self.bump_user_data_revision(&item_id);
        let previous_override = self.user_data_overrides.insert(item_id.clone(), optimistic);
        let removed = if self.navigation.root() == HomeRoot::Favorites && !desired {
            self.favorites.remove_item(&item_id)
        } else {
            None
        };
        self.favorites.mark_dirty();
        self.favorite_rollbacks.insert(
            item_id.clone(),
            FavoriteRollback {
                previous_override,
                removed,
            },
        );
        self.favorite_requests.insert(item_id.clone());
        self.clear_notification(
            NotificationScope::Detail,
            &format!("detail:favorite:{item_id}"),
        );
        cx.notify();

        let server = self.current_server.clone();
        let identity = self.request_identity();
        let client = self.emby_client.clone();
        let task_item_id = item_id.clone();
        let task = cx
            .background_spawn(async move { client.set_favorite(&server, &task_item_id, desired) });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_toggle_favorite(identity, item_id, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_toggle_favorite(
        &mut self,
        identity: super::WorkspaceIdentity,
        item_id: String,
        result: anyhow::Result<UserItemData>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        self.favorite_requests.remove(&item_id);
        let rollback = self.favorite_rollbacks.remove(&item_id);
        self.bump_user_data_revision(&item_id);
        match result {
            Ok(data) => {
                self.user_data_overrides.insert(item_id.clone(), data);
            }
            Err(error) => {
                if let Some(rollback) = rollback {
                    match rollback.previous_override {
                        Some(previous) => {
                            self.user_data_overrides.insert(item_id.clone(), previous);
                        }
                        None => {
                            self.user_data_overrides.remove(&item_id);
                        }
                    }
                    if let Some((index, item)) = rollback.removed {
                        self.favorites.restore_item(index, item);
                    }
                }
                let message: SharedString = format!("更新收藏失败：{error}").into();
                let is_detail = self
                    .series_detail
                    .as_ref()
                    .is_some_and(|detail| detail.series_id == item_id);
                if is_detail {
                    self.push_error_notification(
                        NotificationScope::Detail,
                        format!("detail:favorite:{item_id}"),
                        message,
                        cx,
                    );
                }
            }
        }
        self.schedule_home_snapshot_save(cx);
        self.favorites.mark_dirty();
        if self.navigation.current() == &HomeRoute::Root(HomeRoot::Favorites) {
            self.load_favorites_initial(cx);
        }
        cx.notify();
    }

    pub(super) fn user_data_request_revision(&self) -> u64 {
        self.user_data_revision
    }

    pub(super) fn bump_user_data_revision(&mut self, item_id: &str) {
        self.user_data_revision = self.user_data_revision.wrapping_add(1);
        self.user_data_item_revisions
            .insert(item_id.to_string(), self.user_data_revision);
    }

    pub(super) fn absorb_user_items_user_data(&mut self, items: &UserItems, request_revision: u64) {
        for item in &items.items {
            self.absorb_user_data(&item.id, item.user_data.as_ref(), request_revision);
        }
    }

    pub(super) fn absorb_resume_items_user_data(
        &mut self,
        items: &ResumeItems,
        request_revision: u64,
    ) {
        for item in &items.items {
            self.absorb_user_data(&item.id, item.user_data.as_ref(), request_revision);
        }
    }

    pub(super) fn absorb_user_data(
        &mut self,
        item_id: &str,
        data: Option<&UserItemData>,
        request_revision: u64,
    ) {
        if !user_data_response_is_current(
            item_id,
            request_revision,
            &self.favorite_requests,
            &self.user_data_item_revisions,
        ) {
            return;
        }
        if let Some(data) = data {
            self.user_data_overrides
                .insert(item_id.to_string(), data.clone());
        }
    }

    pub(super) fn effective_user_data<'a>(
        &'a self,
        item_id: &str,
        fallback: Option<&'a UserItemData>,
    ) -> Option<&'a UserItemData> {
        self.user_data_overrides.get(item_id).or(fallback)
    }

    pub(super) fn effective_user_item(&self, item: &UserItem) -> UserItem {
        effective_user_item(item, &self.user_data_overrides)
    }

    pub(super) fn effective_resume_item(&self, item: &ResumeItem) -> ResumeItem {
        effective_resume_item(item, &self.user_data_overrides)
    }

    pub(super) fn favorite_is_pending(&self, item_id: &str) -> bool {
        self.favorite_requests.contains(item_id)
    }
}

fn favorite_query(start_index: u32) -> UserItemsQuery {
    UserItemsQuery {
        include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
        is_favorite: Some(true),
        recursive: true,
        start_index,
        limit: PAGED_ITEMS_LIMIT,
        sort_by: Some(UserItemsSort::SortName),
        sort_order: SortOrder::Ascending,
        ..UserItemsQuery::default()
    }
}

fn is_movie_or_series(item: &UserItem) -> bool {
    !item.id.trim().is_empty() && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
}

fn effective_user_item(
    item: &UserItem,
    overrides: &std::collections::HashMap<String, UserItemData>,
) -> UserItem {
    let mut item = item.clone();
    if let Some(data) = overrides.get(&item.id) {
        item.user_data = Some(data.clone());
    }
    item
}

fn effective_resume_item(
    item: &ResumeItem,
    overrides: &std::collections::HashMap<String, UserItemData>,
) -> ResumeItem {
    let mut item = item.clone();
    if let Some(data) = overrides.get(&item.id) {
        item.user_data = Some(data.clone());
    }
    item
}

fn user_data_response_is_current(
    item_id: &str,
    request_revision: u64,
    favorite_requests: &std::collections::HashSet<String>,
    item_revisions: &std::collections::HashMap<String, u64>,
) -> bool {
    !favorite_requests.contains(item_id)
        && item_revisions
            .get(item_id)
            .is_none_or(|revision| *revision <= request_revision)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[test]
    fn favorite_query_uses_v1_types_and_sorting() {
        let query = favorite_query(60);
        assert_eq!(
            query.include_item_types,
            vec![VideoItemType::Movie, VideoItemType::Series]
        );
        assert_eq!(query.is_favorite, Some(true));
        assert_eq!(query.start_index, 60);
        assert_eq!(query.limit, 60);
        assert_eq!(query.sort_by, Some(UserItemsSort::SortName));
    }

    #[test]
    fn favorite_override_propagates_and_rollback_restores_all_copies() {
        let item: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1",
            "Name": "电影",
            "Type": "Movie",
            "UserData": { "IsFavorite": false }
        }))
        .unwrap();
        let mut overrides = HashMap::new();
        overrides.insert(
            item.id.clone(),
            UserItemData {
                is_favorite: true,
                ..UserItemData::default()
            },
        );

        let home_copy = effective_user_item(&item, &overrides);
        let search_copy = effective_user_item(&item, &overrides);
        assert!(home_copy.is_favorite());
        assert!(search_copy.is_favorite());

        overrides.remove(&item.id);
        assert!(!effective_user_item(&item, &overrides).is_favorite());
    }

    #[test]
    fn resume_movie_uses_the_same_favorite_override() {
        let item: ResumeItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1",
            "Name": "电影",
            "Type": "Movie",
            "UserData": { "IsFavorite": false }
        }))
        .unwrap();
        let overrides = HashMap::from([(
            item.id.clone(),
            UserItemData {
                is_favorite: true,
                ..UserItemData::default()
            },
        )]);
        let effective = effective_resume_item(&item, &overrides);

        assert!(effective.is_favorite());
    }

    #[test]
    fn stale_or_in_flight_user_data_cannot_replace_a_favorite_mutation() {
        let item_revisions = HashMap::from([("movie-1".to_string(), 5)]);
        let mut pending = HashSet::new();

        assert!(!user_data_response_is_current(
            "movie-1",
            4,
            &pending,
            &item_revisions,
        ));
        assert!(user_data_response_is_current(
            "movie-1",
            5,
            &pending,
            &item_revisions,
        ));
        pending.insert("movie-1".to_string());
        assert!(!user_data_response_is_current(
            "movie-1",
            5,
            &pending,
            &item_revisions,
        ));
        assert!(user_data_response_is_current(
            "movie-2",
            0,
            &pending,
            &item_revisions,
        ));
    }
}
