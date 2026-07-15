use gpui::{AppContext as _, Context, SharedString};

use crate::emby::{SortOrder, UserItems, UserItemsQuery, UserItemsSort, UserView, VideoItemType};

use super::{
    HomeContent, HomeContentEvent,
    navigation::HomeRoute,
    paged_items::{PAGED_ITEMS_LIMIT, PagedItemsState},
};

#[derive(Clone, Debug)]
pub(crate) struct LibraryState {
    pub(crate) title: SharedString,
    pub(crate) item_types: Vec<VideoItemType>,
    pub(crate) paged: PagedItemsState,
}

#[derive(Clone, Debug)]
struct LibraryRequestContext {
    identity: super::WorkspaceIdentity,
    user_data_revision: u64,
    view_id: String,
    generation: u64,
    start_index: u32,
}

impl LibraryState {
    fn new(title: String, item_types: Vec<VideoItemType>) -> Self {
        Self {
            title: title.into(),
            item_types,
            paged: PagedItemsState::default(),
        }
    }
}

impl HomeContent {
    pub(super) fn open_library_for_view(&mut self, view: &UserView, cx: &mut Context<Self>) {
        let Some(item_types) = library_item_types(view.collection_type.as_deref()) else {
            return;
        };
        let state = self
            .libraries
            .entry(view.id.clone())
            .or_insert_with(|| LibraryState::new(view.name.clone(), item_types.clone()));
        state.title = view.name.clone().into();
        state.item_types = item_types.clone();
        let should_load = state.paged.initial == super::LoadState::Idle
            || (state.paged.initial == super::LoadState::Failed && state.paged.items.is_empty());
        self.navigation
            .push_library(view.id.clone(), view.name.clone(), item_types);
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.series_detail = None;
        self.detail_history.clear();
        if should_load {
            self.load_library_initial(view.id.clone(), false, cx);
        }
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn open_library_by_id(&mut self, view_id: &str, cx: &mut Context<Self>) {
        let view = self
            .user_views
            .as_ref()
            .and_then(|views| views.items.iter().find(|view| view.id == view_id))
            .cloned();
        if let Some(view) = view {
            self.open_library_for_view(&view, cx);
        }
    }

    pub(super) fn retry_current_library(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.navigation.current() else {
            return;
        };
        self.load_library_initial(view_id.clone(), false, cx);
    }

    pub(super) fn load_more_current_library(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.navigation.current() else {
            return;
        };
        self.load_more_library(view_id.clone(), cx);
    }

    pub(super) fn auto_load_more_library(&mut self, view_id: &str, cx: &mut Context<Self>) {
        let HomeRoute::Library {
            view_id: current_view_id,
            ..
        } = self.navigation.current()
        else {
            return;
        };
        if current_view_id != view_id {
            return;
        }
        if self
            .libraries
            .get(view_id)
            .is_none_or(|state| !state.paged.can_auto_load_more())
        {
            return;
        }
        self.load_more_library(view_id.to_string(), cx);
    }

    fn load_more_library(&mut self, view_id: String, cx: &mut Context<Self>) {
        let Some(state) = self.libraries.get_mut(&view_id) else {
            return;
        };
        let Some((generation, start_index)) = state.paged.begin_load_more() else {
            return;
        };
        let item_types = state.item_types.clone();
        cx.notify();

        let server = self.current_server.clone();
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task_view_id = view_id.clone();
        let request = LibraryRequestContext {
            identity,
            user_data_revision,
            view_id,
            generation,
            start_index,
        };
        let task = cx.background_spawn(async move {
            client.query_user_items(
                &server,
                &UserItemsQuery {
                    parent_id: Some(task_view_id),
                    include_item_types: item_types,
                    recursive: true,
                    start_index,
                    limit: PAGED_ITEMS_LIMIT,
                    sort_by: Some(UserItemsSort::SortName),
                    sort_order: SortOrder::Ascending,
                    ..UserItemsQuery::default()
                },
            )
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_library_load_more(request, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn load_library_initial(&mut self, view_id: String, clear: bool, cx: &mut Context<Self>) {
        let Some(state) = self.libraries.get_mut(&view_id) else {
            return;
        };
        let Some(generation) = state
            .paged
            .begin_initial(clear || state.paged.items.is_empty())
        else {
            return;
        };
        let item_types = state.item_types.clone();
        cx.notify();

        let server = self.current_server.clone();
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task_view_id = view_id.clone();
        let request = LibraryRequestContext {
            identity,
            user_data_revision,
            view_id,
            generation,
            start_index: 0,
        };
        let task = cx.background_spawn(async move {
            client.query_user_items(
                &server,
                &UserItemsQuery {
                    parent_id: Some(task_view_id),
                    include_item_types: item_types,
                    recursive: true,
                    start_index: 0,
                    limit: PAGED_ITEMS_LIMIT,
                    sort_by: Some(UserItemsSort::SortName),
                    sort_order: SortOrder::Ascending,
                    ..UserItemsQuery::default()
                },
            )
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_library_initial(request, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_library_initial(
        &mut self,
        request: LibraryRequestContext,
        mut result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&request.identity) {
            return;
        }
        if self
            .libraries
            .get(&request.view_id)
            .is_none_or(|state| !state.paged.accepts_initial(request.generation))
        {
            return;
        }
        let item_types = self
            .libraries
            .get(&request.view_id)
            .map(|state| state.item_types.clone())
            .unwrap_or_default();
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            filter_supported_items(items, &item_types);
            self.absorb_user_items_user_data(items, request.user_data_revision);
        }
        let applied = self
            .libraries
            .get_mut(&request.view_id)
            .is_some_and(|state| {
                state.paged.finish_initial_with_raw_count(
                    request.generation,
                    result,
                    PAGED_ITEMS_LIMIT,
                    raw_count,
                )
            });
        if applied {
            let items = self.libraries.get(&request.view_id).map(|state| UserItems {
                items: state.paged.items.clone(),
                total_record_count: state.paged.total_record_count.unwrap_or_default(),
            });
            if let Some(items) = items {
                self.ensure_user_items_images(&items, cx);
            }
            cx.notify();
        }
    }

    fn finish_library_load_more(
        &mut self,
        request: LibraryRequestContext,
        mut result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&request.identity) {
            return;
        }
        if self.libraries.get(&request.view_id).is_none_or(|state| {
            !state
                .paged
                .accepts_load_more(request.generation, request.start_index)
        }) {
            return;
        }
        let item_types = self
            .libraries
            .get(&request.view_id)
            .map(|state| state.item_types.clone())
            .unwrap_or_default();
        let raw_count = result
            .as_ref()
            .ok()
            .map(|items| items.items.len() as u32)
            .unwrap_or_default();
        if let Ok(items) = result.as_mut() {
            filter_supported_items(items, &item_types);
            self.absorb_user_items_user_data(items, request.user_data_revision);
            self.ensure_user_items_images(items, cx);
        }
        let applied = self
            .libraries
            .get_mut(&request.view_id)
            .is_some_and(|state| {
                state.paged.finish_load_more_with_raw_count(
                    request.generation,
                    request.start_index,
                    result,
                    PAGED_ITEMS_LIMIT,
                    raw_count,
                )
            });
        if applied {
            cx.notify();
        }
    }
}

pub(crate) fn library_item_types(collection_type: Option<&str>) -> Option<Vec<VideoItemType>> {
    match normalized_collection_type(collection_type).as_deref() {
        Some("movies") => Some(vec![VideoItemType::Movie]),
        Some("tvshows") => Some(vec![VideoItemType::Series]),
        Some("mixed") | None => Some(vec![VideoItemType::Movie, VideoItemType::Series]),
        Some(collection_type) if known_unsupported_collection(collection_type) => None,
        Some(_) => Some(vec![VideoItemType::Movie, VideoItemType::Series]),
    }
}

pub(crate) fn latest_item_types(collection_type: Option<&str>) -> Option<Vec<VideoItemType>> {
    match normalized_collection_type(collection_type).as_deref() {
        Some("movies") => Some(vec![VideoItemType::Movie]),
        Some("tvshows") => Some(vec![VideoItemType::Series, VideoItemType::Episode]),
        Some("mixed") | None => Some(vec![
            VideoItemType::Movie,
            VideoItemType::Series,
            VideoItemType::Episode,
        ]),
        Some(collection_type) if known_unsupported_collection(collection_type) => None,
        Some(_) => Some(vec![
            VideoItemType::Movie,
            VideoItemType::Series,
            VideoItemType::Episode,
        ]),
    }
}

pub(crate) fn is_supported_view(view: &UserView) -> bool {
    !view.id.trim().is_empty() && library_item_types(view.collection_type.as_deref()).is_some()
}

fn normalized_collection_type(collection_type: Option<&str>) -> Option<String> {
    collection_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn known_unsupported_collection(collection_type: &str) -> bool {
    matches!(
        collection_type,
        "music"
            | "musicvideos"
            | "audiobooks"
            | "books"
            | "boxsets"
            | "playlists"
            | "homevideos"
            | "homevideosandphotos"
            | "photos"
            | "trailers"
            | "folders"
            | "games"
            | "livetv"
            | "channels"
    )
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
mod tests {
    use super::*;

    #[test]
    fn maps_collection_types_to_v1_root_and_latest_types() {
        assert_eq!(
            library_item_types(Some("movies")),
            Some(vec![VideoItemType::Movie])
        );
        assert_eq!(
            library_item_types(Some("tvshows")),
            Some(vec![VideoItemType::Series])
        );
        assert_eq!(
            latest_item_types(Some("tvshows")),
            Some(vec![VideoItemType::Series, VideoItemType::Episode])
        );
        assert_eq!(
            library_item_types(Some(" mixed ")),
            Some(vec![VideoItemType::Movie, VideoItemType::Series])
        );
        assert_eq!(
            latest_item_types(None),
            Some(vec![
                VideoItemType::Movie,
                VideoItemType::Series,
                VideoItemType::Episode,
            ])
        );
        assert_eq!(
            library_item_types(Some("TVSHOWS")),
            Some(vec![VideoItemType::Series])
        );
        assert!(library_item_types(Some("music")).is_none());
        assert_eq!(
            library_item_types(Some("future-video-kind")),
            Some(vec![VideoItemType::Movie, VideoItemType::Series])
        );
    }
}
