//! Workspace snapshot submission and close/quit flush binding.
use crate::home::{HomeContent, cache as home_cache};
use gpui::{App, Context, Task};
use std::sync::Arc;

impl HomeContent {
    pub(in crate::home) fn snapshot_dirty_key(&self) -> crate::persistence::DirtyKey {
        crate::persistence::DirtyKey::HomeSnapshot(self.request_identity())
    }
    pub(in crate::home) fn snapshot_persistence_adapter(
        &self,
    ) -> Arc<dyn crate::persistence::AppPersistence> {
        Arc::new(crate::persistence::FilePersistence {
            #[cfg(test)]
            settings_path: None,
            #[cfg(test)]
            home_path: self.snapshot_save_path.clone(),
        })
    }
    pub(in crate::home) fn schedule_home_snapshot_save(&mut self, cx: &mut Context<Self>) {
        self.submit_home_snapshot(cx);
        if self.controller.favorite_pending() {
            self.invalidate_pending_home_snapshot_save();
        }
    }
    pub(in crate::home) fn submit_home_snapshot(&self, cx: &mut App) {
        #[cfg(test)]
        if self.snapshot_save_path.is_none() {
            return;
        }
        self.persistence.schedule_home(
            self.current_server.clone(),
            self.home_snapshot(),
            self.snapshot_persistence_adapter(),
            cx,
        );
    }
    pub(in crate::home) fn invalidate_pending_home_snapshot_save(&mut self) {
        self.persistence.suspend(&self.snapshot_dirty_key());
    }
    pub(in crate::home) fn finish_home_snapshot_saves(&mut self, cx: &mut App) -> Task<()> {
        let key = self.snapshot_dirty_key();
        if self.sync_track_preferences(cx) || self.persistence.needs_flush_snapshot(&key) {
            #[cfg(test)]
            if self.snapshot_save_path.is_none() {
                return Task::ready(());
            }
            self.persistence.flush_home_snapshot(
                self.current_server.clone(),
                self.home_snapshot(),
                self.snapshot_persistence_adapter(),
                cx,
            );
        }
        self.persistence.flush(&key, cx);
        self.persistence.drain(cx)
    }
    pub(in crate::home) fn home_snapshot(&self) -> home_cache::HomeSnapshot {
        self.controller.snapshot(home_cache::current_unix_time())
    }
}
