use super::model::{SearchPage, SearchRequest, SearchState};
use crate::home::model::LoadState;
use crate::{
    effects::WorkspaceIdentity,
    emby::{UserItem, UserItems},
    search_history::SearchHistory,
};

/// Owns search data, history and the active Search scope. Presentation owns the
/// editor, scroll and delivery handle; reset cancels that handle, and results
/// are accepted here before images, notifications or user-data are touched.
#[derive(Debug)]
pub(crate) struct SearchController {
    state: SearchState,
}

pub(crate) enum SearchIntent {
    InputChanged(String),
    Submit(String),
    LoadMore,
    ClearHistory,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchRequestContext {
    pub(crate) user_data_revision: u64,
    pub(crate) request: SearchRequest,
}

#[derive(Default)]
pub(crate) struct SearchTransition {
    pub(crate) request: Option<SearchRequestContext>,
    pub(crate) reset_presentation: bool,
    pub(crate) history_changed: bool,
    pub(crate) notify: bool,
}

pub(crate) struct SearchUpdate {
    pub(crate) received: Option<UserItems>,
    pub(crate) initial: bool,
    pub(crate) error: Option<String>,
}

pub(crate) struct SearchVm<'a> {
    pub(crate) items: &'a [UserItem],
    pub(crate) history: &'a SearchHistory,
    pub(crate) empty_results: bool,
    pub(crate) can_load_more: bool,
    pub(crate) has_results: bool,
}

impl SearchController {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            state: SearchState::new(identity),
        }
    }

    pub(crate) fn view_model(&self) -> SearchVm<'_> {
        SearchVm {
            items: &self.state.items,
            history: &self.state.history,
            empty_results: !self.state.query.is_empty()
                && self.state.initial == LoadState::Loaded
                && self.state.items.is_empty()
                && self.state.exhausted,
            can_load_more: self.state.can_load_more(),
            has_results: !self.state.query.is_empty() && !self.state.items.is_empty(),
        }
    }

    pub(crate) fn restore_history(&mut self, history: SearchHistory) {
        self.state.history = history;
    }

    pub(crate) fn dispatch(&mut self, intent: SearchIntent, revision: u64) -> SearchTransition {
        let mut transition = SearchTransition::default();
        let request = match intent {
            SearchIntent::InputChanged(query) => {
                let query = query.trim();
                if self.state.query != query {
                    self.state.reset_for_query(query.into());
                    transition.reset_presentation = true;
                    transition.notify = true;
                }
                None
            }
            SearchIntent::Submit(query) => {
                let query = query.trim();
                if query.is_empty() {
                    if !self.state.query.is_empty() || !self.state.items.is_empty() {
                        self.state.reset_for_query(String::new());
                        transition.reset_presentation = true;
                        transition.notify = true;
                    }
                    return transition;
                }
                if self.state.query == query && self.state.initial == LoadState::Loading {
                    return transition;
                }
                transition.history_changed = self.state.history.record(query);
                self.state.reset_for_query(query.into());
                transition.reset_presentation = true;
                self.state.begin_initial()
            }
            SearchIntent::LoadMore => self.state.begin_load_more(),
            SearchIntent::ClearHistory => {
                self.state.history.clear();
                transition.history_changed = true;
                transition.notify = true;
                None
            }
        };
        transition.notify |= request.is_some();
        transition.request = request.map(|request| SearchRequestContext {
            request,
            user_data_revision: revision,
        });
        transition
    }

    pub(crate) fn complete(
        &mut self,
        request: &SearchRequestContext,
        result: anyhow::Result<SearchPage>,
        current_workspace: &WorkspaceIdentity,
    ) -> Option<SearchUpdate> {
        if !request.request.token.is_for(current_workspace)
            || !self.state.accepts_request(&request.request)
        {
            return None;
        }
        let received = result.as_ref().ok().map(|page| UserItems {
            items: page.items.clone(),
            total_record_count: page.total_record_count,
        });
        if !self.state.finish(&request.request, result) {
            return None;
        }
        let initial = request.request.initial;
        let error = if initial {
            self.state.initial_error.clone()
        } else {
            self.state.load_more_error.clone()
        };
        Some(SearchUpdate {
            received,
            initial,
            error,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_state(&self) -> &SearchState {
        &self.state
    }
    #[cfg(test)]
    pub(crate) fn test_state_mut(&mut self) -> &mut SearchState {
        &mut self.state
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
