use crate::home::model::{
    LoadState,
    paged_items::{PAGED_ITEMS_LIMIT, PagedItemsState},
};
use crate::{
    effects::{RequestScope, RequestToken, WorkspaceIdentity},
    emby::{SortOrder, UserItems, UserItemsQuery, UserItemsSort, VideoItemType},
};

pub(crate) const LIBRARY_SORT_OPTIONS: [UserItemsSort; 11] = [
    UserItemsSort::SortName,
    UserItemsSort::DateCreated,
    UserItemsSort::PremiereDate,
    UserItemsSort::ProductionYear,
    UserItemsSort::CommunityRating,
    UserItemsSort::CriticRating,
    UserItemsSort::DatePlayed,
    UserItemsSort::DateLastContentAdded,
    UserItemsSort::PlayCount,
    UserItemsSort::Random,
    UserItemsSort::OfficialRating,
];

pub(crate) fn available_library_sorts(
    item_types: &[VideoItemType],
) -> impl Iterator<Item = UserItemsSort> + '_ {
    LIBRARY_SORT_OPTIONS
        .iter()
        .copied()
        .filter(move |sort_by| library_sort_is_available(*sort_by, item_types))
}

fn library_sort_is_available(sort_by: UserItemsSort, item_types: &[VideoItemType]) -> bool {
    sort_by != UserItemsSort::DateLastContentAdded || matches!(item_types, [VideoItemType::Series])
}

/// One library owns its metadata, sort and paging scope. The page keeps only
/// its menu/scroll/animation and cancellable delivery handle. Sort/open/refresh
/// dispatch here; accepted results leave here before user-data/image side effects.
#[derive(Debug)]
pub(crate) struct LibraryController {
    view_id: String,
    item_types: Vec<VideoItemType>,
    sort_by: UserItemsSort,
    sort_order: SortOrder,
    paged: PagedItemsState,
}

pub(crate) enum LibraryIntent {
    Open(Vec<VideoItemType>),
    SortBy(UserItemsSort),
    SortOrder(SortOrder),
    LoadMore { automatic: bool },
}

#[derive(Default)]
pub(crate) struct LibraryTransition {
    pub(crate) request: Option<LibraryRequest>,
    pub(crate) close_menu: bool,
    pub(crate) cancel: bool,
    pub(crate) notify: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct LibraryRequest {
    pub(crate) view_id: String,
    pub(crate) token: RequestToken,
    pub(crate) start_index: u32,
    pub(crate) initial: bool,
    pub(crate) user_data_revision: u64,
    pub(crate) query: UserItemsQuery,
}

pub(crate) enum LibraryFailure {
    Initial(String),
    Refresh(String),
    More(String),
}

pub(crate) struct LibraryUpdate {
    pub(crate) received: Option<UserItems>,
    pub(crate) images: Option<UserItems>,
    pub(crate) failure: Option<LibraryFailure>,
}

pub(crate) struct LibraryVm<'a> {
    pub(crate) paged: &'a PagedItemsState,
    pub(crate) item_types: &'a [VideoItemType],
    pub(crate) sort_by: UserItemsSort,
    pub(crate) sort_order: SortOrder,
    pub(crate) empty: bool,
}

impl LibraryController {
    pub(crate) fn new(
        item_types: Vec<VideoItemType>,
        view_id: String,
        identity: WorkspaceIdentity,
    ) -> Self {
        Self {
            paged: PagedItemsState::new(
                RequestScope::Library {
                    view_id: view_id.clone(),
                },
                identity,
            ),
            view_id,
            item_types,
            sort_by: UserItemsSort::SortName,
            sort_order: SortOrder::Ascending,
        }
    }
    pub(crate) fn view_model(&self) -> LibraryVm<'_> {
        LibraryVm {
            paged: &self.paged,
            item_types: &self.item_types,
            sort_by: self.sort_by,
            sort_order: self.sort_order,
            empty: self.paged.initial != LoadState::Loading
                && self.paged.initial_error.is_none()
                && self.paged.items.is_empty(),
        }
    }
    pub(crate) fn dispatch(&mut self, intent: LibraryIntent, revision: u64) -> LibraryTransition {
        let mut transition = LibraryTransition::default();
        let clear = match intent {
            LibraryIntent::Open(item_types) => {
                self.item_types = item_types;
                transition.close_menu = true;
                transition.notify = true;
                let reset = !library_sort_is_available(self.sort_by, &self.item_types);
                if reset {
                    self.sort_by = UserItemsSort::SortName;
                    self.paged.mark_dirty();
                    transition.cancel = true;
                }
                if !(reset
                    || self.paged.initial == LoadState::Idle
                    || (self.paged.initial == LoadState::Failed && self.paged.items.is_empty()))
                {
                    return transition;
                }
                Some(reset)
            }
            LibraryIntent::SortBy(sort_by) => {
                if !library_sort_is_available(sort_by, &self.item_types) {
                    return transition;
                }
                transition.close_menu = true;
                transition.notify = true;
                if self.sort_by == sort_by {
                    return transition;
                }
                self.sort_by = sort_by;
                self.paged.mark_dirty();
                transition.cancel = true;
                Some(true)
            }
            LibraryIntent::SortOrder(sort_order) => {
                transition.close_menu = true;
                transition.notify = true;
                if self.sort_order == sort_order {
                    return transition;
                }
                self.sort_order = sort_order;
                self.paged.mark_dirty();
                transition.cancel = true;
                Some(true)
            }
            LibraryIntent::LoadMore { automatic } => {
                if automatic && !self.paged.can_auto_load_more() {
                    return transition;
                }
                None
            }
        };
        let issued = if let Some(clear) = clear {
            self.paged
                .begin_initial(clear || self.paged.items.is_empty())
                .map(|token| (token, 0))
        } else {
            self.paged.begin_load_more()
        };
        if let Some((token, start_index)) = issued {
            transition.notify = true;
            transition.request = Some(LibraryRequest {
                view_id: self.view_id.clone(),
                token,
                start_index,
                initial: clear.is_some(),
                user_data_revision: revision,
                query: self.query(start_index),
            });
        }
        transition
    }
    fn query(&self, start_index: u32) -> UserItemsQuery {
        UserItemsQuery {
            parent_id: Some(self.view_id.clone()),
            include_item_types: self.item_types.clone(),
            recursive: true,
            start_index,
            limit: PAGED_ITEMS_LIMIT,
            sort_by: Some(self.sort_by),
            sort_order: self.sort_order,
            ..Default::default()
        }
    }
    pub(crate) fn complete(
        &mut self,
        request: &LibraryRequest,
        mut result: anyhow::Result<UserItems>,
        current_workspace: &WorkspaceIdentity,
    ) -> Option<LibraryUpdate> {
        if request.view_id != self.view_id
            || !request.token.is_for(current_workspace)
            || !(if request.initial {
                self.paged.accepts_initial(&request.token)
            } else {
                self.paged
                    .accepts_load_more(&request.token, request.start_index)
            })
        {
            return None;
        }
        let raw = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            filter_supported_items(items, &self.item_types);
        }
        let received = result.as_ref().ok().cloned();
        if request.initial {
            self.paged.finish_initial_with_raw_count(
                &request.token,
                result,
                PAGED_ITEMS_LIMIT,
                raw,
            );
        } else {
            self.paged.finish_load_more_with_raw_count(
                &request.token,
                request.start_index,
                result,
                PAGED_ITEMS_LIMIT,
                raw,
            );
        }
        let failure = if request.initial {
            self.paged
                .initial_error
                .clone()
                .map(LibraryFailure::Initial)
                .or_else(|| {
                    self.paged
                        .refresh_error
                        .clone()
                        .map(LibraryFailure::Refresh)
                })
        } else {
            self.paged.load_more_error.clone().map(LibraryFailure::More)
        };
        let images = if request.initial {
            Some(UserItems {
                items: self.paged.items.clone(),
                total_record_count: self.paged.total_record_count.unwrap_or_default(),
            })
        } else {
            received.clone()
        };
        Some(LibraryUpdate {
            received,
            images,
            failure,
        })
    }
    #[cfg(test)]
    pub(crate) fn test_paged_mut(&mut self) -> &mut PagedItemsState {
        &mut self.paged
    }
}

fn filter_supported_items(items: &mut UserItems, allowed: &[VideoItemType]) {
    items.items.retain(|item| {
        !item.id.trim().is_empty()
            && allowed
                .iter()
                .any(|allowed| item.item_type.as_deref() == Some(allowed.as_str()))
    });
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
