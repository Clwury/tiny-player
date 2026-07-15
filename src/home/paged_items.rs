use std::collections::HashSet;

use gpui::{ScrollHandle, SharedString};

use crate::emby::{UserItem, UserItems};

use super::LoadState;

pub(crate) const PAGED_ITEMS_LIMIT: u32 = 60;

#[derive(Clone, Debug)]
pub(crate) struct PagedItemsState {
    pub(crate) items: Vec<UserItem>,
    pub(crate) total_record_count: Option<u32>,
    pub(crate) next_start_index: u32,
    pub(crate) initial: LoadState,
    pub(crate) load_more: LoadState,
    pub(crate) initial_error: Option<SharedString>,
    pub(crate) load_more_error: Option<SharedString>,
    pub(crate) refresh_error: Option<SharedString>,
    pub(crate) dirty: bool,
    pub(crate) generation: u64,
    pub(crate) exhausted: bool,
    pub(crate) scroll_handle: ScrollHandle,
    refresh_checkpoint: Option<(Option<u32>, u32, bool)>,
}

impl Default for PagedItemsState {
    fn default() -> Self {
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
            generation: 0,
            exhausted: false,
            scroll_handle: ScrollHandle::new(),
            refresh_checkpoint: None,
        }
    }
}

impl PagedItemsState {
    pub(crate) fn accepts_initial(&self, generation: u64) -> bool {
        generation == self.generation && self.initial == LoadState::Loading
    }

    pub(crate) fn accepts_load_more(&self, generation: u64, start_index: u32) -> bool {
        generation == self.generation
            && self.load_more == LoadState::Loading
            && start_index == self.next_start_index
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
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

    pub(crate) fn begin_initial(&mut self, clear_items: bool) -> Option<u64> {
        if self.initial == LoadState::Loading {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
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
        Some(self.generation)
    }

    pub(crate) fn begin_load_more(&mut self) -> Option<(u64, u32)> {
        if !self.can_load_more() || self.load_more == LoadState::Loading {
            return None;
        }
        self.load_more = LoadState::Loading;
        self.load_more_error = None;
        Some((self.generation, self.next_start_index))
    }

    #[cfg(test)]
    pub(crate) fn finish_initial(
        &mut self,
        generation: u64,
        result: anyhow::Result<UserItems>,
        limit: u32,
    ) -> bool {
        let raw_count = result
            .as_ref()
            .ok()
            .map(|page| page.items.len() as u32)
            .unwrap_or_default();
        self.finish_initial_with_raw_count(generation, result, limit, raw_count)
    }

    pub(crate) fn finish_initial_with_raw_count(
        &mut self,
        generation: u64,
        result: anyhow::Result<UserItems>,
        limit: u32,
        raw_count: u32,
    ) -> bool {
        if !self.accepts_initial(generation) {
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
                    self.initial_error = Some(error.to_string().into());
                } else {
                    if let Some((total, next_start_index, exhausted)) =
                        self.refresh_checkpoint.take()
                    {
                        self.total_record_count = total;
                        self.next_start_index = next_start_index;
                        self.exhausted = exhausted;
                    }
                    self.refresh_error = Some(format!("刷新失败：{error}").into());
                }
            }
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn finish_load_more(
        &mut self,
        generation: u64,
        start_index: u32,
        result: anyhow::Result<UserItems>,
        limit: u32,
    ) -> bool {
        let raw_count = result
            .as_ref()
            .ok()
            .map(|page| page.items.len() as u32)
            .unwrap_or_default();
        self.finish_load_more_with_raw_count(generation, start_index, result, limit, raw_count)
    }

    pub(crate) fn finish_load_more_with_raw_count(
        &mut self,
        generation: u64,
        start_index: u32,
        result: anyhow::Result<UserItems>,
        limit: u32,
        raw_count: u32,
    ) -> bool {
        if !self.accepts_load_more(generation, start_index) {
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
                self.load_more_error = Some(error.to_string().into());
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
mod tests {
    use super::*;

    fn item(id: &str) -> UserItem {
        serde_json::from_value(serde_json::json!({"Id": id, "Name": id, "Type": "Movie"})).unwrap()
    }

    #[test]
    fn pagination_advances_by_raw_count_and_stably_deduplicates() {
        let mut state = PagedItemsState::default();
        let generation = state.begin_initial(true).unwrap();
        assert!(state.finish_initial(
            generation,
            Ok(UserItems {
                items: vec![item("a"), item("b")],
                total_record_count: 4,
            }),
            2,
        ));
        let (generation, start_index) = state.begin_load_more().unwrap();
        assert_eq!(start_index, 2);
        assert!(state.finish_load_more(
            generation,
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
        let generation = state.begin_initial(true).unwrap();
        state.finish_initial(
            generation,
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
        let generation = state.begin_initial(true).unwrap();
        assert!(state.finish_initial(
            generation,
            Ok(UserItems {
                items: vec![item("a"), item("b")],
                total_record_count: 4,
            }),
            2,
        ));
        assert!(state.can_auto_load_more());

        let (generation, start_index) = state.begin_load_more().unwrap();
        assert!(!state.can_auto_load_more());
        assert!(state.finish_load_more(
            generation,
            start_index,
            Err(anyhow::anyhow!("offline")),
            2,
        ));

        assert!(state.can_load_more());
        assert!(!state.can_auto_load_more());
    }

    #[test]
    fn filtered_page_still_advances_by_server_raw_count() {
        let mut state = PagedItemsState::default();
        let generation = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            generation,
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
        let generation = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            generation,
            Ok(UserItems {
                items: vec![item("a")],
                total_record_count: 120,
            }),
            60,
            60,
        );
        let generation = state.begin_initial(false).unwrap();
        state.finish_initial_with_raw_count(generation, Err(anyhow::anyhow!("offline")), 60, 0);

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
        let generation = state.begin_initial(true).unwrap();
        state.finish_initial_with_raw_count(
            generation,
            Ok(UserItems {
                items: vec![item("a")],
                total_record_count: 120,
            }),
            60,
            60,
        );
        let (generation, start_index) = state.begin_load_more().unwrap();
        state.finish_load_more_with_raw_count(
            generation,
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
        let (generation, start_index) = state.begin_load_more().unwrap();

        state.mark_dirty();

        assert!(state.dirty);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.load_more, LoadState::Idle);
        assert!(!state.finish_load_more_with_raw_count(
            generation,
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
