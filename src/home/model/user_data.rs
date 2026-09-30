use crate::emby::{ResumeItem, ResumeItems, UserItem, UserItemData, UserItems};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

/// Workspace-owned user-data overlay and response revision fences. Mutations
/// advance revisions; accepted reads merge through absorb_* before selectors
/// expose effective data. The entire state is dropped with its workspace.
#[derive(Debug, Default)]
pub(crate) struct UserDataState {
    pub(crate) overrides: HashMap<String, UserItemData>,
    pub(crate) revision: u64,
    pub(crate) item_revisions: HashMap<String, u64>,
    pub(crate) series_revisions: HashMap<String, u64>,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingUserData<'a> {
    pub(crate) played: bool,
    pub(crate) favorites: &'a dyn PendingFavorites,
}

pub(crate) trait PendingFavorites {
    fn contains(&self, item_id: &str) -> bool;
}

impl PendingFavorites for HashSet<String> {
    fn contains(&self, item_id: &str) -> bool {
        HashSet::contains(self, item_id)
    }
}

impl UserDataState {
    pub(crate) fn bump(&mut self, item_id: &str) {
        self.revision = self.revision.wrapping_add(1);
        self.item_revisions.insert(item_id.into(), self.revision);
    }

    pub(crate) fn series_response_is_current(
        &self,
        series_id: Option<&str>,
        revision: u64,
    ) -> bool {
        series_id
            .and_then(|id| self.series_revisions.get(id))
            .is_none_or(|current| *current <= revision)
    }

    pub(crate) fn absorb_items(
        &mut self,
        items: &UserItems,
        revision: u64,
        pending: PendingUserData<'_>,
    ) {
        for item in &items.items {
            if self.series_response_is_current(item.series_id.as_deref(), revision) {
                self.absorb(&item.id, item.user_data.as_ref(), revision, pending);
            }
        }
    }

    pub(crate) fn absorb_resume(
        &mut self,
        items: &ResumeItems,
        revision: u64,
        pending: PendingUserData<'_>,
    ) {
        for item in &items.items {
            if self.series_response_is_current(item.series_id.as_deref(), revision) {
                self.absorb(&item.id, item.user_data.as_ref(), revision, pending);
            }
        }
    }

    pub(crate) fn absorb(
        &mut self,
        item_id: &str,
        data: Option<&UserItemData>,
        revision: u64,
        pending: PendingUserData<'_>,
    ) {
        if pending.played
            || !user_data_response_is_current(
                item_id,
                revision,
                pending.favorites,
                &self.item_revisions,
            )
        {
            return;
        }
        if let Some(data) = data {
            self.overrides.insert(item_id.into(), data.clone());
        }
    }

    pub(crate) fn effective<'a>(
        &'a self,
        item_id: &str,
        fallback: Option<&'a UserItemData>,
    ) -> Option<&'a UserItemData> {
        self.overrides.get(item_id).or(fallback)
    }

    pub(crate) fn item<'a>(&self, item: &'a UserItem) -> Cow<'a, UserItem> {
        effective_user_item(item, &self.overrides)
    }

    pub(crate) fn resume<'a>(&self, item: &'a ResumeItem) -> Cow<'a, ResumeItem> {
        effective_resume_item(item, &self.overrides)
    }
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
    favorite_requests: &dyn PendingFavorites,
    item_revisions: &std::collections::HashMap<String, u64>,
) -> bool {
    !favorite_requests.contains(item_id)
        && item_revisions
            .get(item_id)
            .is_none_or(|revision| *revision <= request_revision)
}

pub(crate) fn apply_media_item_user_data_overrides(
    items: &mut [crate::emby::MediaItem],
    overrides: &std::collections::HashMap<String, crate::emby::UserItemData>,
) {
    for item in items {
        if let Some(data) = overrides.get(&item.id) {
            item.user_data = Some(data.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_series_and_pending_mutation_fences_apply_to_browsing_and_resume_responses() {
        let mut state = UserDataState::default();
        state.bump("recent");
        state.series_revisions.insert("series".into(), 1);
        let favorites = HashSet::from(["pending".into()]);
        let pending = PendingUserData {
            played: false,
            favorites: &favorites,
        };
        let items: UserItems = serde_json::from_value(serde_json::json!({
            "Items": [
                {"Id": "recent", "Name": "recent", "UserData": {"IsFavorite": true}},
                {"Id": "pending", "Name": "pending", "UserData": {"IsFavorite": true}},
                {"Id": "episode", "Name": "episode", "SeriesId": "series", "UserData": {"IsFavorite": true}},
                {"Id": "accepted", "Name": "accepted", "UserData": {"IsFavorite": true}}
            ], "TotalRecordCount": 4
        })).unwrap();
        state.absorb_items(&items, 0, pending);
        assert_eq!(state.overrides.len(), 1);
        assert!(state.overrides["accepted"].is_favorite);
        state.overrides.clear();
        state.absorb_items(
            &items,
            1,
            PendingUserData {
                played: true,
                ..pending
            },
        );
        assert!(state.overrides.is_empty());
        state.absorb_items(&items, 1, pending);
        assert_eq!(state.overrides.len(), 3);
        assert!(!state.overrides.contains_key("pending"));
        let resume: ResumeItems = serde_json::from_value(serde_json::json!({
            "Items": [{"Id": "resume", "Name": "resume", "SeriesId": "series", "UserData": {"IsFavorite": true}}],
            "TotalRecordCount": 1
        })).unwrap();
        state.absorb_resume(&resume, 0, pending);
        assert!(!state.overrides.contains_key("resume"));
        state.absorb_resume(&resume, 1, pending);
        assert!(state.overrides["resume"].is_favorite);
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
