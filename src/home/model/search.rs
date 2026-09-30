use crate::{emby::UserItem, search_history::SearchHistory};

use super::LoadState;
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

pub(crate) const SEARCH_LIMIT: u32 = 30;

/// SearchController owns this model; only query and request transitions write it.
/// Query reset invalidates all responses; workspace release drops the model.
#[derive(Debug)]
pub(crate) struct SearchState {
    pub(crate) history: SearchHistory,
    pub(crate) query: String,
    pub(crate) items: Vec<UserItem>,
    pub(crate) total_record_count: Option<u32>,
    pub(crate) next_start_index: u32,
    pub(crate) initial: LoadState,
    pub(crate) load_more: LoadState,
    pub(crate) initial_error: Option<String>,
    pub(crate) load_more_error: Option<String>,
    requests: RequestSlot,
    pub(crate) exhausted: bool,
}

impl SearchState {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            history: SearchHistory::default(),
            query: String::new(),
            items: Vec::new(),
            total_record_count: None,
            next_start_index: 0,
            initial: LoadState::Idle,
            load_more: LoadState::Idle,
            initial_error: None,
            load_more_error: None,
            requests: RequestSlot::new(RequestScope::Search, identity),
            exhausted: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct SearchPage {
    pub(crate) items: Vec<UserItem>,
    pub(crate) total_record_count: u32,
    pub(crate) raw_item_count: u32,
}

impl SearchPage {
    pub(crate) fn from_response(response: crate::emby::UserItems) -> Self {
        let raw_item_count = response.items.len() as u32;
        Self {
            items: response
                .items
                .into_iter()
                .filter(|item| {
                    !item.id.trim().is_empty()
                        && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
                })
                .collect(),
            total_record_count: response.total_record_count,
            raw_item_count,
        }
    }
}

/// Query identity belongs to SearchState; Home's effect runner additionally
/// checks workspace identity before forwarding any response to this reducer.
#[derive(Clone, Debug)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) token: RequestToken,
    pub(crate) start_index: u32,
    pub(crate) initial: bool,
}

impl SearchState {
    pub(crate) fn begin_initial(&mut self) -> Option<SearchRequest> {
        if self.query.is_empty() || self.initial == LoadState::Loading {
            return None;
        }
        self.initial = LoadState::Loading;
        self.initial_error = None;
        self.load_more = LoadState::Idle;
        self.load_more_error = None;
        Some(SearchRequest {
            query: self.query.clone(),
            token: self.requests.issue(),
            start_index: 0,
            initial: true,
        })
    }

    pub(crate) fn begin_load_more(&mut self) -> Option<SearchRequest> {
        if !self.can_load_more() {
            return None;
        }
        self.load_more = LoadState::Loading;
        self.load_more_error = None;
        Some(SearchRequest {
            query: self.query.clone(),
            token: self.requests.issue(),
            start_index: self.next_start_index,
            initial: false,
        })
    }

    pub(crate) fn accepts_request(&self, request: &SearchRequest) -> bool {
        request.token.is_current(&self.requests)
            && self.query == request.query
            && if request.initial {
                self.initial == LoadState::Loading
            } else {
                self.load_more == LoadState::Loading && request.start_index == self.next_start_index
            }
    }

    pub(crate) fn finish(
        &mut self,
        request: &SearchRequest,
        result: anyhow::Result<SearchPage>,
    ) -> bool {
        if !self.accepts_request(request) {
            return false;
        }
        if !self.requests.commit(&request.token) {
            return false;
        }
        let status = if result.is_ok() {
            LoadState::Loaded
        } else {
            LoadState::Failed
        };
        if request.initial {
            self.initial = status;
        } else {
            self.load_more = status;
        }
        match result {
            Ok(page) => {
                if request.initial {
                    self.items.clear();
                    self.next_start_index = 0;
                    self.initial_error = None;
                } else {
                    self.load_more_error = None;
                }
                self.merge_page(page);
            }
            Err(error) => {
                if request.initial {
                    self.initial_error = Some(error.to_string());
                } else {
                    self.load_more_error = Some(error.to_string());
                }
            }
        }
        true
    }

    pub(crate) fn reset_for_query(&mut self, query: String) {
        self.requests.invalidate();
        self.query = query;
        self.items.clear();
        self.total_record_count = None;
        self.next_start_index = 0;
        self.initial = LoadState::Idle;
        self.load_more = LoadState::Idle;
        self.initial_error = None;
        self.load_more_error = None;
        self.exhausted = false;
    }

    #[cfg(test)]
    pub(crate) fn request_token(&self) -> Option<RequestToken> {
        self.requests.latest()
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

#[cfg(test)]
impl Default for SearchState {
    fn default() -> Self {
        Self::new(WorkspaceIdentity::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> UserItem {
        serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Movie"})).unwrap()
    }

    #[test]
    fn old_request_is_rejected_after_query_changes_or_clears() {
        let mut state = SearchState::default();
        let old = begin(&mut state, "old");
        let new = begin(&mut state, "new");
        assert!(!state.accepts_request(&old));
        assert!(state.accepts_request(&new));
        state.reset_for_query(String::new());
        assert!(!state.accepts_request(&new));
        assert!(state.begin_initial().is_none());
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

    fn begin(state: &mut SearchState, query: &str) -> SearchRequest {
        state.reset_for_query(query.into());
        state.begin_initial().unwrap()
    }

    fn page(id: &str, raw_item_count: u32) -> SearchPage {
        SearchPage {
            items: vec![item(id)],
            total_record_count: 90,
            raw_item_count,
        }
    }

    #[test]
    fn late_success_and_failure_cannot_replace_the_new_query() {
        let mut state = SearchState::default();
        let old = begin(&mut state, "old");
        let current = begin(&mut state, "current");
        assert!(!state.finish(&old, Ok(page("old-item", 30))));
        assert!(!state.finish(&old, Err(anyhow::anyhow!("old failure"))));
        assert_eq!(state.initial, LoadState::Loading);
        assert!(state.initial_error.is_none());
        assert!(state.items.is_empty());
        assert!(state.finish(&current, Ok(page("current-item", 30))));
        assert_eq!(state.items[0].id, "current-item");
    }

    #[test]
    fn duplicate_submissions_and_results_do_not_repeat_transitions() {
        let mut state = SearchState::default();
        let request = begin(&mut state, "query");
        assert!(state.begin_initial().is_none());
        assert!(state.begin_load_more().is_none());
        assert!(state.finish(&request, Ok(page("first", 30))));
        assert!(!state.finish(&request, Err(anyhow::anyhow!("late failure"))));
        assert_eq!(state.initial, LoadState::Loaded);
        let more = state.begin_load_more().unwrap();
        assert!(state.begin_load_more().is_none());
        let mut wrong_offset = more.clone();
        wrong_offset.start_index = 0;
        assert!(!state.finish(&wrong_offset, Ok(page("wrong", 30))));
        assert!(state.finish(&more, Ok(page("second", 30))));
        assert!(!state.finish(&more, Ok(page("duplicate", 30))));
        assert_eq!(state.next_start_index, 60);
        assert_eq!(state.items.len(), 2);
    }

    #[test]
    fn failed_more_keeps_items_and_retries_the_same_server_offset() {
        let mut state = SearchState::default();
        let request = begin(&mut state, "query");
        state.finish(&request, Ok(page("first", 30)));
        let more = state.begin_load_more().unwrap();
        assert!(state.finish(&more, Err(anyhow::anyhow!("offline"))));
        assert_eq!(state.items[0].id, "first");
        assert_eq!(state.load_more_error.as_deref(), Some("offline"));
        let retry = state.begin_load_more().unwrap();
        assert_eq!(retry.start_index, 30);
        assert!(state.load_more_error.is_none());
        assert!(state.finish(&retry, Ok(page("second", 1))));
        assert!(state.exhausted);
        assert!(state.begin_load_more().is_none());
    }

    #[test]
    fn response_filtering_preserves_raw_count_and_server_order() {
        let mut invalid_type = item("episode");
        invalid_type.item_type = Some("Episode".into());
        let response = SearchPage::from_response(crate::emby::UserItems {
            items: vec![item("b"), item(" "), invalid_type, item("a")],
            total_record_count: 80,
        });
        assert_eq!(response.raw_item_count, 4);
        assert_eq!(response.total_record_count, 80);
        assert_eq!(
            response
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
    }
}
