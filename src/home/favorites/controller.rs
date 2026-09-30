use crate::{
    effects::{RequestScope, RequestToken, WorkspaceIdentity},
    emby::{SortOrder, UserItem, UserItems, UserItemsQuery, UserItemsSort, VideoItemType},
    home::model::{
        LoadState,
        paged_items::PagedItemsState,
        user_data::{PendingUserData, UserDataState},
    },
};

pub(crate) const FAVORITE_ITEM_TYPES: [VideoItemType; 3] = [
    VideoItemType::Movie,
    VideoItemType::Series,
    VideoItemType::Episode,
];
pub(crate) const FAVORITES_PAGE_LIMIT: u32 = 30;
pub(crate) use crate::home::model::favorite_section_title;

pub(super) fn section_index(item_type: VideoItemType) -> usize {
    match item_type {
        VideoItemType::Movie => 0,
        VideoItemType::Series => 1,
        VideoItemType::Episode => 2,
    }
}

/// Workspace owner for the three independent Favorites paging scopes. Enter,
/// refresh, pagination and optimistic remove/restore write this state. Dirty
/// invalidates every scope; the presentation cancels delivery handles and drops
/// them on release. Notification phases remain initial/refresh/load-more.
#[derive(Debug)]
pub(crate) struct FavoritesController {
    sections: [PagedItemsState; 3],
}

pub(crate) enum FavoritesIntent {
    Enter,
    Refresh,
    LoadMore { automatic: bool },
}

#[derive(Clone)]
pub(crate) struct FavoritesRequest {
    pub(crate) item_type: VideoItemType,
    pub(crate) user_data_revision: u64,
    pub(crate) token: RequestToken,
    pub(crate) start_index: u32,
    pub(crate) initial: bool,
}

pub(crate) struct FavoritesUpdate {
    pub(crate) images: Option<UserItems>,
    pub(crate) failure: Option<(&'static str, String)>,
}

pub(crate) struct FavoriteSectionVm<'a> {
    pub(crate) paged: &'a PagedItemsState,
    pub(crate) show_section: bool,
    pub(crate) has_items: bool,
    pub(crate) can_retry: bool,
}

impl FavoritesController {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            sections: FAVORITE_ITEM_TYPES.map(|item_type| {
                PagedItemsState::new(RequestScope::Favorites { item_type }, identity.clone())
            }),
        }
    }

    pub(crate) fn view_model(&self, item_type: VideoItemType) -> FavoriteSectionVm<'_> {
        let paged = &self.sections[section_index(item_type)];
        FavoriteSectionVm {
            paged,
            show_section: !paged.items.is_empty() || paged.initial == LoadState::Failed,
            has_items: !paged.items.is_empty(),
            can_retry: paged.initial == LoadState::Failed,
        }
    }

    pub(crate) fn dispatch(
        &mut self,
        item_type: VideoItemType,
        intent: FavoritesIntent,
        revision: u64,
    ) -> Option<FavoritesRequest> {
        let state = &mut self.sections[section_index(item_type)];
        let initial = match intent {
            FavoritesIntent::Enter => {
                if !matches!(state.initial, LoadState::Idle | LoadState::Failed) && !state.dirty {
                    return None;
                }
                true
            }
            FavoritesIntent::Refresh => true,
            FavoritesIntent::LoadMore { automatic } => {
                if state.dirty || (automatic && !state.can_auto_load_more()) {
                    return None;
                }
                false
            }
        };
        let (token, start_index) = if initial {
            (state.begin_initial(state.items.is_empty())?, 0)
        } else {
            state.begin_load_more()?
        };
        Some(FavoritesRequest {
            item_type,
            user_data_revision: revision,
            token,
            start_index,
            initial,
        })
    }

    pub(crate) fn complete(
        &mut self,
        request: &FavoritesRequest,
        mut result: anyhow::Result<UserItems>,
        identity: &WorkspaceIdentity,
        user_data: &mut UserDataState,
        pending: PendingUserData<'_>,
    ) -> Option<FavoritesUpdate> {
        let state = &mut self.sections[section_index(request.item_type)];
        if !request.token.is_for(identity)
            || !(if request.initial {
                state.accepts_initial(&request.token)
            } else {
                state.accepts_load_more(&request.token, request.start_index)
            })
        {
            return None;
        }
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            items.items.retain(|item| {
                !item.id.trim().is_empty()
                    && item.item_type.as_deref() == Some(request.item_type.as_str())
            });
            user_data.absorb_items(items, request.user_data_revision, pending);
            items.items.retain(|item| {
                user_data
                    .overrides
                    .get(&item.id)
                    .is_none_or(|data| data.is_favorite)
            });
        }
        let images = result.as_ref().ok().cloned();
        if request.initial {
            state.finish_initial_with_raw_count(
                &request.token,
                result,
                FAVORITES_PAGE_LIMIT,
                raw_count,
            );
        } else {
            state.finish_load_more_with_raw_count(
                &request.token,
                request.start_index,
                result,
                FAVORITES_PAGE_LIMIT,
                raw_count,
            );
        }
        let failure = if request.initial {
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
        Some(FavoritesUpdate { images, failure })
    }

    pub(crate) fn items(&self) -> impl Iterator<Item = &UserItem> {
        self.sections.iter().flat_map(|state| &state.items)
    }

    pub(crate) fn has_items(&self) -> bool {
        self.items().next().is_some()
    }

    pub(crate) fn mark_dirty(&mut self) {
        for state in &mut self.sections {
            state.mark_dirty();
        }
    }

    pub(crate) fn remove_item(
        &mut self,
        item_id: &str,
    ) -> Option<(VideoItemType, usize, UserItem)> {
        FAVORITE_ITEM_TYPES.into_iter().find_map(|item_type| {
            self.sections[section_index(item_type)]
                .remove_item(item_id)
                .map(|(index, item)| (item_type, index, item))
        })
    }

    pub(crate) fn restore_item(&mut self, item_type: VideoItemType, index: usize, item: UserItem) {
        self.sections[section_index(item_type)].restore_item(index, item);
    }

    #[cfg(test)]
    pub(crate) fn test_state_mut(&mut self, item_type: VideoItemType) -> &mut PagedItemsState {
        &mut self.sections[section_index(item_type)]
    }
}

pub(super) fn favorite_query(item_type: VideoItemType, start_index: u32) -> UserItemsQuery {
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

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
