use gpui::{AppContext as _, Context, ScrollHandle, SharedString};

use crate::{emby::UserItem, ui::text_input::TextInputEvent};

use super::{HomeContent, LoadState};

const SEARCH_LIMIT: u32 = 30;

#[derive(Clone, Debug)]
pub(crate) struct SearchState {
    pub(crate) query: String,
    pub(crate) items: Vec<UserItem>,
    pub(crate) total_record_count: Option<u32>,
    pub(crate) next_start_index: u32,
    pub(crate) initial: LoadState,
    pub(crate) load_more: LoadState,
    pub(crate) initial_error: Option<SharedString>,
    pub(crate) load_more_error: Option<SharedString>,
    pub(crate) generation: u64,
    pub(crate) exhausted: bool,
    pub(crate) scroll_handle: ScrollHandle,
    pub(crate) focused_once: bool,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            items: Vec::new(),
            total_record_count: None,
            next_start_index: 0,
            initial: LoadState::Idle,
            load_more: LoadState::Idle,
            initial_error: None,
            load_more_error: None,
            generation: 0,
            exhausted: false,
            scroll_handle: ScrollHandle::new(),
            focused_once: false,
        }
    }
}

#[derive(Debug)]
struct SearchPage {
    items: Vec<UserItem>,
    total_record_count: u32,
    raw_item_count: u32,
}

#[derive(Clone, Debug)]
struct SearchRequestContext {
    identity: super::WorkspaceIdentity,
    user_data_revision: u64,
    query: String,
    generation: u64,
    start_index: u32,
    initial: bool,
}

impl SearchState {
    fn reset_for_query(&mut self, query: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.query = query;
        self.items.clear();
        self.total_record_count = None;
        self.next_start_index = 0;
        self.initial = LoadState::Idle;
        self.load_more = LoadState::Idle;
        self.initial_error = None;
        self.load_more_error = None;
        self.exhausted = false;
        self.generation
    }

    fn accepts(&self, generation: u64, query: &str) -> bool {
        self.generation == generation && self.query == query
    }

    pub(crate) fn can_load_more(&self) -> bool {
        !self.query.is_empty()
            && self.initial == LoadState::Loaded
            && !self.exhausted
            && self.initial != LoadState::Loading
            && self.load_more != LoadState::Loading
    }

    fn merge_page(&mut self, page: SearchPage) {
        self.next_start_index = self.next_start_index.saturating_add(page.raw_item_count);
        self.total_record_count = Some(page.total_record_count);
        let mut existing = self
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<std::collections::HashSet<_>>();
        self.items.extend(
            page.items
                .into_iter()
                .filter(|item| existing.insert(item.id.clone())),
        );
        self.exhausted =
            page.raw_item_count < SEARCH_LIMIT || self.next_start_index >= page.total_record_count;
    }
}

impl HomeContent {
    pub(super) fn on_search_input_event(&mut self, event: &TextInputEvent, cx: &mut Context<Self>) {
        match event {
            TextInputEvent::Changed => self.reset_search_from_input(cx),
            TextInputEvent::Submitted => self.submit_search_from_input(cx),
        }
    }

    fn reset_search_from_input(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        if self.search.query == query {
            return;
        }
        self.search.reset_for_query(query);
        cx.notify();
    }

    fn submit_search_from_input(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_string();
        if query.is_empty() {
            if !self.search.query.is_empty() || !self.search.items.is_empty() {
                self.search.reset_for_query(String::new());
                cx.notify();
            }
            return;
        }
        if self.search.query == query && self.search.initial == LoadState::Loading {
            return;
        }
        let generation = self.search.reset_for_query(query.clone());
        self.start_search_initial(query, generation, cx);
    }

    pub(super) fn retry_search(&mut self, cx: &mut Context<Self>) {
        if self.search.query.is_empty() || self.search.initial == LoadState::Loading {
            return;
        }
        self.search.generation = self.search.generation.wrapping_add(1);
        let generation = self.search.generation;
        let query = self.search.query.clone();
        self.search.items.clear();
        self.search.total_record_count = None;
        self.search.next_start_index = 0;
        self.search.exhausted = false;
        self.start_search_initial(query, generation, cx);
    }

    pub(super) fn load_more_search(&mut self, cx: &mut Context<Self>) {
        if !self.search.can_load_more() {
            return;
        }
        let query = self.search.query.clone();
        let generation = self.search.generation;
        let start_index = self.search.next_start_index;
        self.search.load_more = LoadState::Loading;
        self.search.load_more_error = None;
        cx.notify();
        self.spawn_search_request(query, generation, start_index, false, cx);
    }

    fn start_search_initial(&mut self, query: String, generation: u64, cx: &mut Context<Self>) {
        if !self.search.accepts(generation, &query) || self.search.initial == LoadState::Loading {
            return;
        }
        self.search.initial = LoadState::Loading;
        self.search.initial_error = None;
        self.search.load_more = LoadState::Idle;
        self.search.load_more_error = None;
        cx.notify();
        self.spawn_search_request(query, generation, 0, true, cx);
    }

    fn spawn_search_request(
        &mut self,
        query: String,
        generation: u64,
        start_index: u32,
        initial: bool,
        cx: &mut Context<Self>,
    ) {
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task_query = query.clone();
        let request = SearchRequestContext {
            identity,
            user_data_revision,
            query,
            generation,
            start_index,
            initial,
        };
        let task = cx.background_spawn(async move {
            let response = client.search_items(&server, &task_query, start_index, SEARCH_LIMIT)?;
            let raw_item_count = response.items.len() as u32;
            Ok(SearchPage {
                items: response.items,
                total_record_count: response.total_record_count,
                raw_item_count,
            })
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_search_request(request, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_search_request(
        &mut self,
        request: SearchRequestContext,
        mut result: anyhow::Result<SearchPage>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&request.identity)
            || !self.search.accepts(request.generation, &request.query)
            || (!request.initial && request.start_index != self.search.next_start_index)
        {
            return;
        }
        if let Ok(page) = result.as_mut() {
            page.items.retain(|item| {
                !item.id.trim().is_empty()
                    && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
            });
            let items = crate::emby::UserItems {
                items: page.items.clone(),
                total_record_count: page.total_record_count,
            };
            self.absorb_user_items_user_data(&items, request.user_data_revision);
            self.ensure_feed_user_items_images(&items, cx);
        }
        if request.initial {
            self.search.initial = match &result {
                Ok(_) => LoadState::Loaded,
                Err(_) => LoadState::Failed,
            };
            match result {
                Ok(page) => {
                    self.search.items.clear();
                    self.search.next_start_index = 0;
                    self.search.merge_page(page);
                    self.search.initial_error = None;
                }
                Err(error) => {
                    self.search.initial_error = Some(error.to_string().into());
                }
            }
        } else {
            self.search.load_more = match &result {
                Ok(_) => LoadState::Loaded,
                Err(_) => LoadState::Failed,
            };
            match result {
                Ok(page) => {
                    self.search.merge_page(page);
                    self.search.load_more_error = None;
                }
                Err(error) => {
                    self.search.load_more_error = Some(error.to_string().into());
                }
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> UserItem {
        serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Movie"})).unwrap()
    }

    #[test]
    fn old_generation_is_rejected_after_query_changes_or_clears() {
        let mut state = SearchState::default();
        let old = state.reset_for_query("old".into());
        let new = state.reset_for_query("new".into());
        assert!(!state.accepts(old, "old"));
        assert!(state.accepts(new, "new"));
        let cleared = state.reset_for_query(String::new());
        assert!(!state.accepts(new, "new"));
        assert!(state.accepts(cleared, ""));
    }

    #[test]
    fn search_pages_keep_server_order_and_deduplicate_across_pages() {
        let mut state = SearchState::default();
        state.reset_for_query("q".into());
        state.merge_page(SearchPage {
            items: vec![item("b"), item("a")],
            total_record_count: 60,
            raw_item_count: 30,
        });
        state.merge_page(SearchPage {
            items: vec![item("a"), item("c")],
            total_record_count: 60,
            raw_item_count: 30,
        });

        assert_eq!(
            state
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a", "c"]
        );
        assert_eq!(state.next_start_index, 60);
    }

    #[test]
    fn empty_filtered_page_can_still_load_later_item_pages() {
        let mut state = SearchState::default();
        state.reset_for_query("q".into());
        state.initial = LoadState::Loaded;
        state.merge_page(SearchPage {
            items: Vec::new(),
            total_record_count: 60,
            raw_item_count: 30,
        });

        assert!(state.can_load_more());
        assert_eq!(state.next_start_index, 30);
    }
}
