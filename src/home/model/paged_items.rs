use std::collections::HashSet;

use crate::emby::{UserItem, UserItems};

use super::LoadState;
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

pub(crate) const PAGED_ITEMS_LIMIT: u32 = 60;

/// Owned by a library or favorite section. begin/finish/dirty and optimistic
/// remove/restore methods own transitions; dirty/refresh invalidate requests.
/// Dropped with its workspace, independently of the grid presentation.
#[derive(Debug)]
pub(crate) struct PagedItemsState {
    pub(crate) items: Vec<UserItem>,
    pub(crate) total_record_count: Option<u32>,
    pub(crate) next_start_index: u32,
    pub(crate) initial: LoadState,
    pub(crate) load_more: LoadState,
    pub(crate) initial_error: Option<String>,
    pub(crate) load_more_error: Option<String>,
    pub(crate) refresh_error: Option<String>,
    pub(crate) dirty: bool,
    requests: RequestSlot,
    pub(crate) exhausted: bool,
    refresh_checkpoint: Option<(Option<u32>, u32, bool)>,
}

impl PagedItemsState {
    pub(crate) fn new(scope: RequestScope, identity: WorkspaceIdentity) -> Self {
        Self {
            items: Vec::new(),
            total_record_count: None,
            next_start_index: 0,
            initial: LoadState::Idle,
            load_more: LoadState::Idle,
            initial_error: None,
            load_more_error: None,
            refresh_error: None,
            dirty: false,
            requests: RequestSlot::new(scope, identity),
            exhausted: false,
            refresh_checkpoint: None,
        }
    }
}

impl PagedItemsState {
    pub(crate) fn accepts_initial(&self, token: &RequestToken) -> bool {
        token.is_current(&self.requests) && self.initial == LoadState::Loading
    }

    pub(crate) fn accepts_load_more(&self, token: &RequestToken, start_index: u32) -> bool {
        token.is_current(&self.requests)
            && self.load_more == LoadState::Loading
            && start_index == self.next_start_index
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.requests.invalidate();
        if self.initial == LoadState::Loading {
            self.initial = if self.items.is_empty() {
                LoadState::Idle
            } else {
                LoadState::Loaded
            };
        }
        self.load_more = LoadState::Idle;
        self.load_more_error = None;
        self.refresh_checkpoint = None;
    }

    pub(crate) fn reset_for_sort(&mut self) {
        self.mark_dirty();
        self.items.clear();
        self.total_record_count = None;
        self.next_start_index = 0;
        self.exhausted = false;
        self.initial = LoadState::Idle;
        self.initial_error = None;
        self.refresh_error = None;
    }

    pub(crate) fn begin_initial(&mut self, clear_items: bool) -> Option<RequestToken> {
        if self.initial == LoadState::Loading {
            return None;
        }
        self.requests.invalidate();
        self.initial = LoadState::Loading;
        self.initial_error = None;
        self.refresh_error = None;
        self.load_more = LoadState::Idle;
        self.load_more_error = None;
        self.refresh_checkpoint = (!clear_items && !self.items.is_empty()).then_some((
            self.total_record_count,
            self.next_start_index,
            self.exhausted,
        ));
        self.exhausted = false;
        self.next_start_index = 0;
        if clear_items {
            self.items.clear();
            self.total_record_count = None;
        }
        Some(self.requests.issue())
    }

    pub(crate) fn begin_load_more(&mut self) -> Option<(RequestToken, u32)> {
        if !self.can_load_more() || self.load_more == LoadState::Loading {
            return None;
        }
        self.load_more = LoadState::Loading;
        self.load_more_error = None;
        Some((self.requests.issue(), self.next_start_index))
    }

    #[cfg(test)]
    pub(crate) fn finish_initial(
        &mut self,
        token: &RequestToken,
        result: anyhow::Result<UserItems>,
        limit: u32,
    ) -> bool {
        let raw_count = result
            .as_ref()
            .ok()
            .map(|page| page.items.len() as u32)
            .unwrap_or_default();
        self.finish_initial_with_raw_count(token, result, limit, raw_count)
    }

    pub(crate) fn finish_initial_with_raw_count(
        &mut self,
        token: &RequestToken,
        result: anyhow::Result<UserItems>,
        limit: u32,
        raw_count: u32,
    ) -> bool {
        if !self.accepts_initial(token) {
            return false;
        }
        if !self.requests.commit(token) {
            return false;
        }
        match result {
            Ok(page) => {
                let had_items = !self.items.is_empty();
                if had_items {
                    self.items.clear();
                }
                self.merge_page(page, limit, raw_count);
                self.initial = LoadState::Loaded;
                self.initial_error = None;
                self.refresh_error = None;
                self.dirty = false;
                self.refresh_checkpoint = None;
            }
            Err(error) => {
                self.initial = LoadState::Failed;
                if self.items.is_empty() {
                    self.initial_error = Some(error.to_string());
                } else {
                    if let Some((total, next_start_index, exhausted)) =
                        self.refresh_checkpoint.take()
                    {
                        self.total_record_count = total;
                        self.next_start_index = next_start_index;
                        self.exhausted = exhausted;
                    }
                    self.refresh_error = Some(format!("刷新失败：{error}"));
                }
            }
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn finish_load_more(
        &mut self,
        token: &RequestToken,
        start_index: u32,
        result: anyhow::Result<UserItems>,
        limit: u32,
    ) -> bool {
        let raw_count = result
            .as_ref()
            .ok()
            .map(|page| page.items.len() as u32)
            .unwrap_or_default();
        self.finish_load_more_with_raw_count(token, start_index, result, limit, raw_count)
    }

    pub(crate) fn finish_load_more_with_raw_count(
        &mut self,
        token: &RequestToken,
        start_index: u32,
        result: anyhow::Result<UserItems>,
        limit: u32,
        raw_count: u32,
    ) -> bool {
        if !self.accepts_load_more(token, start_index) {
            return false;
        }
        if !self.requests.commit(token) {
            return false;
        }
        match result {
            Ok(page) => {
                self.merge_page(page, limit, raw_count);
                self.load_more = LoadState::Loaded;
                self.load_more_error = None;
            }
            Err(error) => {
                self.load_more = LoadState::Failed;
                self.load_more_error = Some(error.to_string());
            }
        }
        true
    }

    fn merge_page(&mut self, page: UserItems, limit: u32, raw_count: u32) {
        self.next_start_index = self.next_start_index.saturating_add(raw_count);
        self.total_record_count = Some(page.total_record_count);
        let mut seen = self
            .items
            .iter()
            .map(|item| item.id.clone())
            .collect::<HashSet<_>>();
        self.items.extend(
            page.items
                .into_iter()
                .filter(|item| seen.insert(item.id.clone())),
        );
        self.exhausted = raw_count < limit || self.items.len() as u32 >= page.total_record_count;
    }

    pub(crate) fn can_load_more(&self) -> bool {
        !self.items.is_empty() && !self.exhausted && self.initial != LoadState::Loading
    }

    pub(crate) fn can_auto_load_more(&self) -> bool {
        self.can_load_more() && matches!(self.load_more, LoadState::Idle | LoadState::Loaded)
    }

    pub(crate) fn remove_item(&mut self, item_id: &str) -> Option<(usize, UserItem)> {
        let index = self.items.iter().position(|item| item.id == item_id)?;
        let item = self.items.remove(index);
        if let Some(total) = self.total_record_count.as_mut() {
            *total = total.saturating_sub(1);
        }
        Some((index, item))
    }

    pub(crate) fn restore_item(&mut self, index: usize, item: UserItem) {
        let index = index.min(self.items.len());
        self.items.insert(index, item);
        if let Some(total) = self.total_record_count.as_mut() {
            *total = total.saturating_add(1);
        }
    }
}

#[cfg(test)]
impl Default for PagedItemsState {
    fn default() -> Self {
        Self::new(
            RequestScope::Library {
                view_id: "test".into(),
            },
            WorkspaceIdentity::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_do_not_cross_library_views_or_favorite_categories() {
        let identity = WorkspaceIdentity {
            user_id: Some("user".into()),
            ..Default::default()
        };
        let mut first = PagedItemsState::new(
            RequestScope::Library {
                view_id: "first".into(),
            },
            identity.clone(),
        );
        let mut second = PagedItemsState::new(
            RequestScope::Library {
                view_id: "second".into(),
            },
            identity.clone(),
        );
        let mut favorites = PagedItemsState::new(
            RequestScope::Favorites {
                item_type: crate::home::FavoriteItemType::Movie,
            },
            identity,
        );
        let first_token = first.begin_initial(true).unwrap();
        let second_token = second.begin_initial(true).unwrap();
        let favorite_token = favorites.begin_initial(true).unwrap();
        assert!(!first.accepts_initial(&second_token));
        assert!(!first.accepts_initial(&favorite_token));
        assert!(!favorites.finish_initial(&first_token, Err(anyhow::anyhow!("wrong scope")), 30));
        assert!(first.accepts_initial(&first_token));
        assert!(second.accepts_initial(&second_token));
        assert!(favorites.accepts_initial(&favorite_token));
        assert!(favorites.initial_error.is_none());
    }

    #[test]
    fn old_pagination_failure_cannot_finish_a_retry_at_the_same_offset() {
        let mut state = PagedItemsState::default();
        let first = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            &first,
            Ok(UserItems {
                items: vec![item("first")],
                total_record_count: 120,
            }),
            60,
            60,
        );
        let (failed, offset) = state.begin_load_more().unwrap();
        state.finish_load_more(&failed, offset, Err(anyhow::anyhow!("offline")), 60);
        let (retry, retry_offset) = state.begin_load_more().unwrap();
        assert_eq!(retry_offset, offset);
        assert!(!state.finish_load_more(&failed, offset, Err(anyhow::anyhow!("late error")), 60));
        assert_eq!(state.load_more, LoadState::Loading);
        assert!(state.load_more_error.is_none());
        assert!(state.finish_load_more(
            &retry,
            retry_offset,
            Ok(UserItems {
                items: vec![item("last")],
                total_record_count: 61
            }),
            60
        ));
        assert_eq!(state.items.len(), 2);
    }

    fn item(id: &str) -> UserItem {
        serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Movie"})).unwrap()
    }

    #[test]
    fn pagination_advances_by_raw_count_and_stably_deduplicates() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        assert!(state.finish_initial(
            &token,
            Ok(UserItems {
                items: vec![item("a"), item("b")],
                total_record_count: 4,
            }),
            2,
        ));
        let (token, start_index) = state.begin_load_more().unwrap();
        assert_eq!(start_index, 2);
        assert!(state.finish_load_more(
            &token,
            start_index,
            Ok(UserItems {
                items: vec![item("b"), item("c")],
                total_record_count: 4,
            }),
            2,
        ));

        assert_eq!(state.next_start_index, 4);
        assert_eq!(
            state
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn short_page_stops_loading_more() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        state.finish_initial(
            &token,
            Ok(UserItems {
                items: vec![item("a")],
                total_record_count: 10,
            }),
            60,
        );

        assert!(!state.can_load_more());
    }

    #[test]
    fn automatic_pagination_pauses_while_loading_or_after_failure() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        assert!(state.finish_initial(
            &token,
            Ok(UserItems {
                items: vec![item("a"), item("b")],
                total_record_count: 4,
            }),
            2,
        ));
        assert!(state.can_auto_load_more());

        let (token, start_index) = state.begin_load_more().unwrap();
        assert!(!state.can_auto_load_more());
        assert!(state.finish_load_more(&token, start_index, Err(anyhow::anyhow!("offline")), 2,));

        assert!(state.can_load_more());
        assert!(!state.can_auto_load_more());
    }

    #[test]
    fn filtered_page_still_advances_by_server_raw_count() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            &token,
            Ok(UserItems {
                items: vec![item("supported")],
                total_record_count: 120,
            }),
            60,
            60,
        );

        assert_eq!(state.next_start_index, 60);
        assert!(state.can_load_more());
    }

    #[test]
    fn failed_refresh_keeps_existing_pagination_cursor() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            &token,
            Ok(UserItems {
                items: vec![item("a")],
                total_record_count: 120,
            }),
            60,
            60,
        );
        let token = state.begin_initial(false).unwrap();
        state.finish_initial_with_raw_count(&token, Err(anyhow::anyhow!("offline")), 60, 0);

        assert_eq!(state.next_start_index, 60);
        assert_eq!(state.total_record_count, Some(120));
        assert!(state.can_load_more());
        assert!(
            state
                .refresh_error
                .as_deref()
                .is_some_and(|error| error.starts_with("刷新失败："))
        );
    }

    #[test]
    fn load_more_failure_keeps_loaded_items_and_cursor() {
        let mut state = PagedItemsState::default();
        let token = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            &token,
            Ok(UserItems {
                items: vec![item("a")],
                total_record_count: 120,
            }),
            60,
            60,
        );
        let (token, start_index) = state.begin_load_more().unwrap();
        state.finish_load_more_with_raw_count(
            &token,
            start_index,
            Err(anyhow::anyhow!("offline")),
            60,
            0,
        );

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.next_start_index, 60);
        assert!(state.load_more_error.is_some());
    }

    #[test]
    fn removing_and_restoring_favorite_preserves_position() {
        let mut state = PagedItemsState {
            items: vec![item("a"), item("b"), item("c")],
            total_record_count: Some(3),
            ..PagedItemsState::default()
        };
        let removed = state.remove_item("b").unwrap();
        state.restore_item(removed.0, removed.1);

        assert_eq!(
            state
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(state.total_record_count, Some(3));
    }

    #[test]
    fn marking_dirty_invalidates_in_flight_pages_without_losing_items() {
        let mut state = PagedItemsState {
            items: vec![item("a")],
            total_record_count: Some(120),
            next_start_index: 60,
            initial: LoadState::Loaded,
            ..PagedItemsState::default()
        };
        let (token, start_index) = state.begin_load_more().unwrap();

        state.mark_dirty();

        assert!(state.dirty);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.load_more, LoadState::Idle);
        assert!(!state.finish_load_more_with_raw_count(
            &token,
            start_index,
            Ok(UserItems {
                items: vec![item("stale")],
                total_record_count: 120,
            }),
            60,
            60,
        ));
        assert_eq!(state.items[0].id, "a");
    }
}
