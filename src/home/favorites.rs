#[cfg(test)]
mod integration_tests;
pub(super) mod render;
mod state;

pub(crate) use state::{
    FAVORITE_ITEM_TYPES, FAVORITES_PAGE_LIMIT, FavoritesState, favorite_section_title,
};

use std::{borrow::Cow, collections::HashMap};

use gpui::{AppContext as _, ClickEvent, Context, SharedString, Window};

use crate::emby::{
    ResumeItem, ResumeItems, SortOrder, UserItem, UserItemData, UserItems, UserItemsQuery,
    UserItemsSort, VideoItemType,
};

use super::{
    HomeContent, HomeContentEvent, LoadState,
    navigation::{HomeRoot, HomeRoute},
    notification::NotificationScope,
};

#[derive(Clone, Debug)]
pub(crate) struct FavoriteRollback {
    previous_override: Option<UserItemData>,
    removed: Option<(VideoItemType, usize, UserItem)>,
}

struct FavoritesRequest {
    identity: super::WorkspaceIdentity,
    item_type: VideoItemType,
    user_data_revision: u64,
    generation: u64,
    start_index: u32,
    initial: bool,
}

impl HomeContent {
    pub(super) fn enter_favorites_if_needed(&mut self, cx: &mut Context<Self>) {
        let item_types = match self.navigation.current() {
            HomeRoute::FavoriteItems { item_type } => vec![*item_type],
            _ => FAVORITE_ITEM_TYPES.to_vec(),
        };
        for item_type in item_types {
            let state = &self.favorites[item_type].paged;
            if matches!(state.initial, LoadState::Idle | LoadState::Failed) || state.dirty {
                self.load_favorites_initial(item_type, cx);
            }
        }
    }

    pub(super) fn open_favorite_items(&mut self, item_type: VideoItemType, cx: &mut Context<Self>) {
        self.favorites.sync_previous_offsets();
        self.navigation.push_favorite_items(item_type);
        self.enter_favorites_if_needed(cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn auto_load_more_favorites(
        &mut self,
        item_type: VideoItemType,
        cx: &mut Context<Self>,
    ) {
        if self.navigation.current() != &(HomeRoute::FavoriteItems { item_type })
            || self.favorites[item_type].paged.dirty
            || !self.favorites[item_type].paged.can_auto_load_more()
        {
            return;
        }
        self.load_more_favorites(item_type, cx);
    }

    pub(super) fn load_more_favorites(&mut self, item_type: VideoItemType, cx: &mut Context<Self>) {
        self.request_favorites_page(item_type, false, cx);
    }

    pub(super) fn load_favorites_initial(
        &mut self,
        item_type: VideoItemType,
        cx: &mut Context<Self>,
    ) {
        self.request_favorites_page(item_type, true, cx);
    }

    fn clear_favorite_notifications(&mut self, item_type: VideoItemType) {
        for phase in ["initial", "refresh", "load-more"] {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(item_type, phase),
            );
        }
    }

    fn request_favorites_page(
        &mut self,
        item_type: VideoItemType,
        initial: bool,
        cx: &mut Context<Self>,
    ) {
        let state = &mut self.favorites[item_type].paged;
        if !initial && state.dirty {
            return;
        }
        let request = if initial {
            state
                .begin_initial(state.items.is_empty())
                .map(|generation| (generation, 0))
        } else {
            state.begin_load_more()
        };
        let Some((generation, start_index)) = request else {
            return;
        };
        if initial {
            self.clear_favorite_notifications(item_type);
        } else {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(item_type, "load-more"),
            );
        }
        let request = FavoritesRequest {
            identity: self.request_identity(),
            item_type,
            user_data_revision: self.user_data_request_revision(),
            generation,
            start_index,
            initial,
        };
        cx.notify();
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move {
            client.query_user_items(&server, &favorite_query(item_type, start_index))
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_favorites_page(request, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_favorites_page(
        &mut self,
        request: FavoritesRequest,
        mut result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let FavoritesRequest {
            identity,
            item_type,
            user_data_revision,
            generation,
            start_index,
            initial,
        } = request;
        if !self.matches_request_identity(&identity) {
            return;
        }
        let state = &self.favorites[item_type].paged;
        if !(if initial {
            state.accepts_initial(generation)
        } else {
            state.accepts_load_more(generation, start_index)
        }) {
            return;
        }
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            items.items.retain(|item| {
                !item.id.trim().is_empty() && item.item_type.as_deref() == Some(item_type.as_str())
            });
            self.absorb_user_items_user_data(items, user_data_revision);
            items.items.retain(|item| {
                self.user_data_overrides
                    .get(&item.id)
                    .is_none_or(|data| data.is_favorite)
            });
            self.ensure_favorite_items_images(items, cx);
        }
        let state = &mut self.favorites[item_type].paged;
        if initial {
            state.finish_initial_with_raw_count(
                generation,
                result,
                FAVORITES_PAGE_LIMIT,
                raw_count,
            );
        } else {
            state.finish_load_more_with_raw_count(
                generation,
                start_index,
                result,
                FAVORITES_PAGE_LIMIT,
                raw_count,
            );
        }
        let error = if initial {
            state
                .initial_error
                .clone()
                .map(|error| ("initial", error))
                .or_else(|| state.refresh_error.clone().map(|error| ("refresh", error)))
        } else {
            state
                .load_more_error
                .clone()
                .map(|error| ("load-more", error))
        };
        if let Some((phase, error)) = error {
            self.push_error_notification(
                NotificationScope::Favorites,
                favorite_notification_key(item_type, phase),
                format!("加载收藏{}失败：{error}", favorite_section_title(item_type)),
                cx,
            );
        } else if initial {
            self.clear_favorite_notifications(item_type);
        } else {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(item_type, "load-more"),
            );
        }
        cx.notify();
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
            .and_then(|detail| detail.selected_playback_item())
            .map(|item| item.id.clone())
        else {
            return;
        };
        let fallback = self
            .series_detail
            .as_ref()
            .and_then(|detail| detail.selected_playback_item())
            .and_then(|item| item.user_data.clone());
        self.toggle_item_favorite(item_id, fallback, cx);
    }

    pub(super) fn toggle_item_favorite(
        &mut self,
        item_id: String,
        fallback: Option<UserItemData>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_user_data_pending() {
            return;
        }
        if let Some(detail) = self.series_detail.as_mut() {
            detail.open_select = None;
        }
        let old = self
            .effective_user_data(&item_id, fallback.as_ref())
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
                    if let Some((item_type, index, item)) = rollback.removed {
                        self.favorites.restore_item(item_type, index, item);
                    }
                }
                let message: SharedString = format!("更新收藏失败：{error}").into();
                let is_detail = self.series_detail.as_ref().is_some_and(|detail| {
                    detail.series_id == item_id
                        || detail.episodes.as_ref().is_some_and(|episodes| {
                            episodes.items.iter().any(|item| item.id == item_id)
                        })
                });
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
        if matches!(
            self.navigation.current(),
            HomeRoute::Root(HomeRoot::Favorites) | HomeRoute::FavoriteItems { .. }
        ) {
            self.enter_favorites_if_needed(cx);
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
            if self
                .series_user_data_response_is_current(item.series_id.as_deref(), request_revision)
            {
                self.absorb_user_data(&item.id, item.user_data.as_ref(), request_revision);
            }
        }
    }

    pub(super) fn absorb_resume_items_user_data(
        &mut self,
        items: &ResumeItems,
        request_revision: u64,
    ) {
        for item in &items.items {
            if self
                .series_user_data_response_is_current(item.series_id.as_deref(), request_revision)
            {
                self.absorb_user_data(&item.id, item.user_data.as_ref(), request_revision);
            }
        }
    }

    pub(super) fn absorb_user_data(
        &mut self,
        item_id: &str,
        data: Option<&UserItemData>,
        request_revision: u64,
    ) {
        if self.played_request.is_some()
            || !user_data_response_is_current(
                item_id,
                request_revision,
                &self.favorite_requests,
                &self.user_data_item_revisions,
            )
        {
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

    pub(super) fn effective_user_item<'a>(&self, item: &'a UserItem) -> Cow<'a, UserItem> {
        effective_user_item(item, &self.user_data_overrides)
    }

    pub(super) fn effective_resume_item<'a>(&self, item: &'a ResumeItem) -> Cow<'a, ResumeItem> {
        effective_resume_item(item, &self.user_data_overrides)
    }
}

fn favorite_query(item_type: VideoItemType, start_index: u32) -> UserItemsQuery {
    UserItemsQuery {
        include_item_types: vec![item_type],
        fields: Some("BasicSyncInfo,CommunityRating,ProductionYear,EndDate,Container,ParentId,SeriesId,SeriesName,ParentIndexNumber,IndexNumber,UserData".into()),
        is_favorite: Some(true),
        recursive: true,
        start_index,
        limit: FAVORITES_PAGE_LIMIT,
        sort_by: Some(UserItemsSort::DateCreated),
        sort_order: SortOrder::Descending,
        ..UserItemsQuery::default()
    }
}

fn favorite_notification_key(item_type: VideoItemType, phase: &str) -> String {
    format!("favorites:{}:{phase}", item_type.as_str())
}

fn effective_user_item<'a>(
    item: &'a UserItem,
    overrides: &HashMap<String, UserItemData>,
) -> Cow<'a, UserItem> {
    let Some(data) = overrides.get(&item.id) else {
        return Cow::Borrowed(item);
    };
    if item.user_data.as_ref() == Some(data) {
        return Cow::Borrowed(item);
    }

    let mut item = item.clone();
    item.user_data = Some(data.clone());
    Cow::Owned(item)
}

fn effective_resume_item<'a>(
    item: &'a ResumeItem,
    overrides: &HashMap<String, UserItemData>,
) -> Cow<'a, ResumeItem> {
    let Some(data) = overrides.get(&item.id) else {
        return Cow::Borrowed(item);
    };
    if item.user_data.as_ref() == Some(data) {
        return Cow::Borrowed(item);
    }

    let mut item = item.clone();
    item.user_data = Some(data.clone());
    Cow::Owned(item)
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
    fn favorite_queries_load_each_type_independently_with_newest_first() {
        for item_type in FAVORITE_ITEM_TYPES {
            let query = favorite_query(item_type, 30);
            assert_eq!(query.include_item_types, vec![item_type]);
            assert_eq!(query.is_favorite, Some(true));
            assert!(query.recursive);
            assert_eq!(query.start_index, 30);
            assert_eq!(query.limit, 30);
            assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
            assert_eq!(query.sort_order, SortOrder::Descending);
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
    fn unchanged_user_data_reuses_the_original_item() {
        let item: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1",
            "Name": "电影",
            "Type": "Movie",
            "UserData": { "IsFavorite": false }
        }))
        .unwrap();
        let overrides =
            HashMap::from([(item.id.clone(), item.user_data.clone().unwrap_or_default())]);

        assert!(matches!(
            effective_user_item(&item, &overrides),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn changed_user_data_only_clones_the_overridden_item() {
        let item: UserItem = serde_json::from_value(serde_json::json!({
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

        assert!(matches!(
            effective_user_item(&item, &overrides),
            Cow::Owned(_)
        ));
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
