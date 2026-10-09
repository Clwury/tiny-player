use crate::home::model::favorites::FavoriteItemType;
use crate::{
    effects::{RequestScope, RequestToken, WorkspaceIdentity},
    emby::{SortOrder, UserItem, UserItems, UserItemsQuery, UserItemsSort},
    home::model::{
        LoadState,
        paged_items::PagedItemsState,
        user_data::{PendingUserData, UserDataState},
    },
    media::ItemSortPreferences,
};

pub(crate) const FAVORITE_ITEM_TYPES: [FavoriteItemType; 4] = [
    FavoriteItemType::Movie,
    FavoriteItemType::Series,
    FavoriteItemType::Episode,
    FavoriteItemType::Person,
];
pub(crate) const FAVORITES_PAGE_LIMIT: u32 = 30;
const FAVORITE_OVERVIEW_SORT: ItemSortPreferences = ItemSortPreferences {
    sort_by: UserItemsSort::DateCreated,
    sort_order: SortOrder::Descending,
};
pub(crate) use crate::home::model::favorite_section_title;

pub(super) fn section_index(item_type: FavoriteItemType) -> usize {
    match item_type {
        FavoriteItemType::Movie => 0,
        FavoriteItemType::Series => 1,
        FavoriteItemType::Episode => 2,
        FavoriteItemType::Person => 3,
    }
}

/// Workspace owner for the independent Favorites paging scopes. Enter,
/// refresh, pagination and optimistic remove/restore write this state. Dirty
/// invalidates every scope; the presentation cancels delivery handles and drops
/// them on release. Notification phases remain initial/refresh/load-more.
#[derive(Debug)]
pub(crate) struct FavoritesController {
    sections: [FavoriteSectionModel; 4],
    sort: crate::media::ItemSortPreferences,
}

#[derive(Debug)]
struct FavoriteSectionModel {
    paged: PagedItemsState,
    overview: PagedItemsState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FavoritesSource {
    Overview,
    Items,
}

#[derive(Debug)]
pub(crate) struct RemovedFavorite {
    source: FavoritesSource,
    item_type: FavoriteItemType,
    index: usize,
    item: UserItem,
}

pub(crate) enum FavoritesIntent {
    Enter,
    Refresh,
    SortBy(UserItemsSort),
    SortOrder(SortOrder),
    LoadMore { automatic: bool },
}

#[derive(Clone)]
pub(crate) struct FavoritesRequest {
    pub(crate) source: FavoritesSource,
    pub(crate) item_type: FavoriteItemType,
    pub(crate) user_data_revision: u64,
    pub(crate) token: RequestToken,
    pub(crate) start_index: u32,
    pub(crate) initial: bool,
    pub(crate) sort_by: UserItemsSort,
    pub(crate) sort_order: SortOrder,
}

pub(crate) struct FavoritesUpdate {
    pub(crate) images: Option<UserItems>,
    pub(crate) failure: Option<(&'static str, String)>,
}

pub(crate) struct FavoriteSectionVm<'a> {
    pub(crate) paged: &'a PagedItemsState,
    pub(crate) sort_by: UserItemsSort,
    pub(crate) sort_order: SortOrder,
    pub(crate) show_section: bool,
    pub(crate) has_items: bool,
    pub(crate) can_retry: bool,
}

impl FavoritesController {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            sections: FAVORITE_ITEM_TYPES.map(|item_type| FavoriteSectionModel {
                paged: PagedItemsState::new(
                    RequestScope::Favorites { item_type },
                    identity.clone(),
                ),
                overview: PagedItemsState::new(
                    RequestScope::FavoriteOverview { item_type },
                    identity.clone(),
                ),
            }),
            sort: crate::media::ItemSortPreferences::default(),
        }
    }

    pub(crate) fn view_model(&self, item_type: FavoriteItemType) -> FavoriteSectionVm<'_> {
        self.section_view_model(
            item_type,
            FavoritesSource::Items,
            self.sort.for_item_types(item_type.video_types()),
        )
    }

    pub(crate) fn overview_view_model(&self, item_type: FavoriteItemType) -> FavoriteSectionVm<'_> {
        self.section_view_model(item_type, FavoritesSource::Overview, FAVORITE_OVERVIEW_SORT)
    }

    fn section_view_model(
        &self,
        item_type: FavoriteItemType,
        source: FavoritesSource,
        sort: ItemSortPreferences,
    ) -> FavoriteSectionVm<'_> {
        let section = &self.sections[section_index(item_type)];
        let paged = match source {
            FavoritesSource::Overview => &section.overview,
            FavoritesSource::Items => &section.paged,
        };
        FavoriteSectionVm {
            paged,
            sort_by: sort.sort_by,
            sort_order: sort.sort_order,
            show_section: !paged.items.is_empty() || paged.initial == LoadState::Failed,
            has_items: !paged.items.is_empty(),
            can_retry: paged.initial == LoadState::Failed,
        }
    }

    pub(crate) fn set_sort(&mut self, preferences: crate::media::ItemSortPreferences) -> bool {
        if self.sort == preferences {
            return false;
        }
        self.sort = preferences;
        for section in &mut self.sections {
            section.paged.reset_for_sort();
        }
        true
    }

    pub(crate) fn dispatch(
        &mut self,
        item_type: FavoriteItemType,
        intent: FavoritesIntent,
        revision: u64,
    ) -> Option<FavoritesRequest> {
        let sort = match intent {
            FavoritesIntent::SortBy(sort_by) => Some(ItemSortPreferences {
                sort_by,
                ..self.sort
            }),
            FavoritesIntent::SortOrder(sort_order) => Some(ItemSortPreferences {
                sort_order,
                ..self.sort
            }),
            _ => None,
        };
        if matches!(intent, FavoritesIntent::SortBy(sort_by) if !crate::media::item_sort_is_available(sort_by, item_type.video_types()))
        {
            return None;
        }
        if let Some(sort) = sort
            && !self.set_sort(sort)
        {
            return None;
        }
        dispatch_page(
            &mut self.sections[section_index(item_type)].paged,
            FavoritesSource::Items,
            item_type,
            intent,
            revision,
            self.sort.for_item_types(item_type.video_types()),
        )
    }

    pub(crate) fn dispatch_overview(
        &mut self,
        item_type: FavoriteItemType,
        intent: FavoritesIntent,
        revision: u64,
    ) -> Option<FavoritesRequest> {
        if !matches!(intent, FavoritesIntent::Enter | FavoritesIntent::Refresh) {
            return None;
        }
        dispatch_page(
            &mut self.sections[section_index(item_type)].overview,
            FavoritesSource::Overview,
            item_type,
            intent,
            revision,
            FAVORITE_OVERVIEW_SORT,
        )
    }

    fn state_mut(
        &mut self,
        item_type: FavoriteItemType,
        source: FavoritesSource,
    ) -> &mut PagedItemsState {
        let section = &mut self.sections[section_index(item_type)];
        match source {
            FavoritesSource::Overview => &mut section.overview,
            FavoritesSource::Items => &mut section.paged,
        }
    }

    pub(crate) fn complete(
        &mut self,
        request: &FavoritesRequest,
        mut result: anyhow::Result<UserItems>,
        identity: &WorkspaceIdentity,
        user_data: &mut UserDataState,
        pending: PendingUserData<'_>,
    ) -> Option<FavoritesUpdate> {
        let state = self.state_mut(request.item_type, request.source);
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
        self.sections
            .iter()
            .flat_map(|section| &section.paged.items)
            .chain(self.overview_items())
    }

    pub(crate) fn overview_items(&self) -> impl Iterator<Item = &UserItem> {
        self.sections
            .iter()
            .flat_map(|section| &section.overview.items)
    }

    pub(crate) fn has_items(&self) -> bool {
        self.overview_items().next().is_some()
    }

    pub(crate) fn mark_dirty(&mut self) {
        for state in &mut self.sections {
            state.paged.mark_dirty();
            state.overview.mark_dirty();
        }
    }

    pub(crate) fn remove_item(&mut self, item_id: &str) -> Vec<RemovedFavorite> {
        let mut removed = Vec::new();
        for item_type in FAVORITE_ITEM_TYPES {
            for source in [FavoritesSource::Items, FavoritesSource::Overview] {
                if let Some((index, item)) = self.state_mut(item_type, source).remove_item(item_id)
                {
                    removed.push(RemovedFavorite {
                        source,
                        item_type,
                        index,
                        item,
                    });
                }
            }
        }
        removed
    }

    pub(crate) fn restore_item(&mut self, removed: Vec<RemovedFavorite>) {
        for removed in removed {
            self.state_mut(removed.item_type, removed.source)
                .restore_item(removed.index, removed.item);
        }
    }

    #[cfg(test)]
    pub(crate) fn test_state_mut(&mut self, item_type: FavoriteItemType) -> &mut PagedItemsState {
        &mut self.sections[section_index(item_type)].paged
    }

    #[cfg(test)]
    pub(crate) fn test_overview_state_mut(
        &mut self,
        item_type: FavoriteItemType,
    ) -> &mut PagedItemsState {
        &mut self.sections[section_index(item_type)].overview
    }
}

fn dispatch_page(
    state: &mut PagedItemsState,
    source: FavoritesSource,
    item_type: FavoriteItemType,
    intent: FavoritesIntent,
    revision: u64,
    sort: ItemSortPreferences,
) -> Option<FavoritesRequest> {
    let initial = match intent {
        FavoritesIntent::Enter => {
            if !matches!(state.initial, LoadState::Idle | LoadState::Failed) && !state.dirty {
                return None;
            }
            true
        }
        FavoritesIntent::Refresh | FavoritesIntent::SortBy(_) | FavoritesIntent::SortOrder(_) => {
            true
        }
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
        source,
        item_type,
        user_data_revision: revision,
        token,
        start_index,
        initial,
        sort_by: sort.sort_by,
        sort_order: sort.sort_order,
    })
}

pub(super) fn favorite_query(request: &FavoritesRequest) -> UserItemsQuery {
    UserItemsQuery {
        include_item_types: request.item_type.video_types().to_vec(),
        fields: Some("BasicSyncInfo,CommunityRating,ProductionYear,EndDate,Container,ParentId,SeriesId,SeriesName,ParentIndexNumber,IndexNumber,UserData".into()),
        is_favorite: Some(true),
        recursive: true,
        start_index: request.start_index,
        limit: FAVORITES_PAGE_LIMIT,
        sort_by: Some(request.sort_by),
        secondary_sort_by: match request.source {
            FavoritesSource::Overview => vec![UserItemsSort::DateLastContentAdded, UserItemsSort::SortName],
            FavoritesSource::Items => Vec::new(),
        },
        sort_order: request.sort_order,
        ..UserItemsQuery::default()
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
