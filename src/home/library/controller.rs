use crate::home::model::{
    LoadState,
    paged_items::{PAGED_ITEMS_LIMIT, PagedItemsState},
};
use crate::{
    effects::{RequestScope, RequestToken, WorkspaceIdentity},
    emby::{MediaGenre, SortOrder, UserItems, UserItemsQuery, UserItemsSort, VideoItemType},
    media::ItemSortOptions,
};

const GENRE_ITEMS_FIELDS: &str = "BasicSyncInfo,CommunityRating,ProviderIds,ProductionYear,EndDate,PrimaryImageAspectRatio,Container";

pub(crate) fn available_library_sorts(
    options: ItemSortOptions,
) -> impl Iterator<Item = UserItemsSort> {
    options.iter()
}

/// Libraries and person filmographies share sorting and paging policy while
/// keeping distinct request scopes and API filters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::home) enum LibrarySource {
    View(String),
    Person(String),
    Genre(String),
}

impl LibrarySource {
    pub(in crate::home) fn id(&self) -> &str {
        match self {
            Self::View(id) | Self::Person(id) | Self::Genre(id) => id,
        }
    }

    fn scope(&self) -> RequestScope {
        match self {
            Self::View(id) => RequestScope::Library {
                view_id: id.clone(),
            },
            Self::Person(id) => RequestScope::PersonItems {
                person_id: id.clone(),
            },
            Self::Genre(key) => RequestScope::GenreItems {
                genre_key: key.clone(),
            },
        }
    }
}

/// One library owns its metadata, sort and paging scope. The page keeps only
/// its menu/scroll/animation and cancellable delivery handle. Sort/open/refresh
/// dispatch here; accepted results leave here before user-data/image side effects.
#[derive(Debug)]
pub(crate) struct LibraryController {
    source: LibrarySource,
    genre: Option<MediaGenre>,
    item_types: Vec<VideoItemType>,
    sort: crate::media::ItemSortPreferences,
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
    pub(in crate::home) source: LibrarySource,
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
    pub(crate) sort_options: ItemSortOptions,
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
        Self::from_source(item_types, LibrarySource::View(view_id), identity)
    }

    pub(in crate::home) fn for_person(person_id: String, identity: WorkspaceIdentity) -> Self {
        Self::from_source(
            vec![VideoItemType::Movie, VideoItemType::Series],
            LibrarySource::Person(person_id),
            identity,
        )
    }

    pub(in crate::home) fn for_genre(genre: MediaGenre, identity: WorkspaceIdentity) -> Self {
        let mut controller = Self::from_source(
            vec![VideoItemType::Movie, VideoItemType::Series],
            LibrarySource::Genre(genre.key()),
            identity,
        );
        controller.genre = Some(genre);
        controller
    }

    fn from_source(
        item_types: Vec<VideoItemType>,
        source: LibrarySource,
        identity: WorkspaceIdentity,
    ) -> Self {
        Self {
            paged: PagedItemsState::new(source.scope(), identity),
            source,
            genre: None,
            item_types,
            sort: crate::media::ItemSortPreferences::default(),
        }
    }
    pub(crate) fn view_model(&self) -> LibraryVm<'_> {
        let sort_options = self.sort_options();
        let sort = self.sort.for_options(sort_options);
        LibraryVm {
            paged: &self.paged,
            item_types: &self.item_types,
            sort_options,
            sort_by: sort.sort_by,
            sort_order: sort.sort_order,
            empty: self.paged.initial != LoadState::Loading
                && self.paged.initial_error.is_none()
                && self.paged.items.is_empty(),
        }
    }
    fn sort_options(&self) -> ItemSortOptions {
        if matches!(self.source, LibrarySource::Genre(_)) {
            ItemSortOptions::ALL
        } else {
            ItemSortOptions::for_item_types(&self.item_types)
        }
    }
    pub(crate) fn set_sort(&mut self, preferences: crate::media::ItemSortPreferences) -> bool {
        if self.sort == preferences {
            return false;
        }
        self.sort = preferences;
        self.paged.reset_for_sort();
        true
    }

    pub(crate) fn dispatch(&mut self, intent: LibraryIntent, revision: u64) -> LibraryTransition {
        let mut transition = LibraryTransition::default();
        let clear = match intent {
            LibraryIntent::Open(item_types) => {
                let changed_types = self.item_types != item_types;
                self.item_types = item_types;
                transition.close_menu = true;
                transition.notify = true;
                if changed_types {
                    self.paged.reset_for_sort();
                    transition.cancel = true;
                }
                if !(self.paged.dirty
                    || self.paged.initial == LoadState::Idle
                    || (self.paged.initial == LoadState::Failed && self.paged.items.is_empty()))
                {
                    return transition;
                }
                Some(self.paged.dirty)
            }
            LibraryIntent::SortBy(sort_by) => {
                if !self.sort_options().contains(sort_by) {
                    return transition;
                }
                transition.close_menu = true;
                transition.notify = true;
                if self.sort.sort_by == sort_by {
                    return transition;
                }
                self.set_sort(crate::media::ItemSortPreferences {
                    sort_by,
                    ..self.sort
                });
                transition.cancel = true;
                Some(true)
            }
            LibraryIntent::SortOrder(sort_order) => {
                transition.close_menu = true;
                transition.notify = true;
                if self.sort.sort_order == sort_order {
                    return transition;
                }
                self.set_sort(crate::media::ItemSortPreferences {
                    sort_order,
                    ..self.sort
                });
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
                source: self.source.clone(),
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
        let sort = self.sort.for_options(self.sort_options());
        let mut query = UserItemsQuery {
            parent_id: match &self.source {
                LibrarySource::View(id) => Some(id.clone()),
                LibrarySource::Person(_) | LibrarySource::Genre(_) => None,
            },
            person_ids: match &self.source {
                LibrarySource::Person(id) => vec![id.clone()],
                LibrarySource::View(_) | LibrarySource::Genre(_) => Vec::new(),
            },
            include_item_types: self.item_types.clone(),
            recursive: true,
            start_index,
            limit: PAGED_ITEMS_LIMIT,
            sort_by: Some(sort.sort_by),
            sort_order: sort.sort_order,
            ..Default::default()
        };
        if let Some(genre) = &self.genre {
            query.fields = Some(GENRE_ITEMS_FIELDS.to_string());
            query.collapse_box_set_items = Some(false);
            query.group_programs_by_series = true;
            if let Some(id) = &genre.id {
                query.genre_ids.push(id.clone());
            } else {
                query.genres.push(genre.name.clone());
            }
        }
        query
    }
    pub(crate) fn complete(
        &mut self,
        request: &LibraryRequest,
        mut result: anyhow::Result<UserItems>,
        current_workspace: &WorkspaceIdentity,
    ) -> Option<LibraryUpdate> {
        if request.source != self.source
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
