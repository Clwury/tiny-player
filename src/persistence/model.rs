//! Persistence state machine. No executor, filesystem or GPUI dependencies.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

/// Logical reasons for saving. Settings/history/window share one atomic file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DirtyKey {
    Settings,
    SearchHistory,
    Window,
    HomeSnapshot(WorkspaceIdentity),
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ResourceKey {
    Config,
    Home(WorkspaceIdentity),
}

impl DirtyKey {
    pub(super) fn resource(&self) -> ResourceKey {
        match self {
            Self::Settings | Self::SearchHistory | Self::Window => ResourceKey::Config,
            Self::HomeSnapshot(identity) => ResourceKey::Home(identity.clone()),
        }
    }
}

impl ResourceKey {
    fn identity(&self) -> WorkspaceIdentity {
        match self {
            Self::Config => WorkspaceIdentity::default(),
            Self::Home(identity) => identity.clone(),
        }
    }
}

pub(super) struct WriteRequest<S> {
    pub(super) resource: ResourceKey,
    pub(super) token: RequestToken,
    pub(super) snapshot: Arc<S>,
    pub(super) error_prefix: &'static str,
}

pub(super) struct Entry<S> {
    dirty: HashSet<DirtyKey>,
    latest: Arc<S>,
    last_saved: Option<Arc<S>>,
    failure: Option<String>,
    error_prefix: &'static str,
    deadline: Option<Instant>,
    snapshot_valid: bool,
    timer_slot: RequestSlot,
    timer_token: Option<RequestToken>,
    write_slot: RequestSlot,
    in_flight: Option<Arc<S>>,
    flush_requested: bool,
}

/// One application owns this store. schedule replaces immutable snapshots;
/// suspend invalidates debounce delivery; flush requests serialize writes.
/// Completion never clears edits newer than its snapshot. Failed snapshots stay
/// dirty until the next schedule/explicit flush. The runner drains on shutdown.
pub(super) struct PersistenceCoordinator<S> {
    entries: HashMap<ResourceKey, Entry<S>>,
}

impl<S> Default for PersistenceCoordinator<S> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<S> PersistenceCoordinator<S> {
    pub(super) fn schedule(
        &mut self,
        key: DirtyKey,
        snapshot: S,
        error_prefix: &'static str,
        now: Instant,
        debounce: Duration,
    ) -> Option<RequestToken> {
        let resource = key.resource();
        let snapshot = Arc::new(snapshot);
        let entry = self
            .entries
            .entry(resource.clone())
            .or_insert_with(|| Entry {
                dirty: HashSet::new(),
                latest: snapshot.clone(),
                last_saved: None,
                failure: None,
                error_prefix,
                deadline: None,
                snapshot_valid: true,
                timer_slot: RequestSlot::new(RequestScope::Persistence, resource.identity()),
                timer_token: None,
                write_slot: RequestSlot::new(RequestScope::Persistence, resource.identity()),
                in_flight: None,
                flush_requested: false,
            });
        entry.dirty.insert(key);
        entry.latest = snapshot;
        entry.snapshot_valid = true;
        entry.flush_requested = false;
        entry.error_prefix = error_prefix;
        entry.deadline = Some(now + debounce);
        // One timer for an entire burst; its wake-up reads the latest deadline.
        if entry.timer_token.is_some() {
            return None;
        }
        let token = entry.timer_slot.issue();
        entry.timer_token = Some(token.clone());
        Some(token)
    }

    pub(super) fn remaining(
        &self,
        resource: &ResourceKey,
        token: &RequestToken,
        now: Instant,
    ) -> Option<Duration> {
        let entry = self.entries.get(resource)?;
        if !token.is_current(&entry.timer_slot) {
            return None;
        }
        entry
            .deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(super) fn suspend(&mut self, resource: &ResourceKey) {
        if let Some(entry) = self.entries.get_mut(resource) {
            entry.timer_slot.invalidate();
            entry.timer_token = None;
            entry.deadline = None;
            entry.flush_requested = false;
            entry.snapshot_valid = false;
        }
    }

    pub(super) fn flush(&mut self, resource: &ResourceKey) {
        if let Some(entry) = self.entries.get_mut(resource) {
            entry.timer_slot.invalidate();
            entry.timer_token = None;
            entry.deadline = None;
            entry.flush_requested = entry.snapshot_valid
                && !entry.dirty.is_empty()
                && entry
                    .in_flight
                    .as_ref()
                    .is_none_or(|snapshot| !Arc::ptr_eq(snapshot, &entry.latest));
        }
    }

    pub(super) fn flush_all(&mut self) -> Vec<ResourceKey> {
        let resources = self.entries.keys().cloned().collect::<Vec<_>>();
        for resource in &resources {
            self.flush(resource);
        }
        resources
    }

    pub(super) fn begin_write(&mut self, resource: &ResourceKey) -> Option<WriteRequest<S>> {
        let entry = self.entries.get_mut(resource)?;
        if !entry.flush_requested || entry.in_flight.is_some() || entry.dirty.is_empty() {
            return None;
        }
        entry.flush_requested = false;
        entry.in_flight = Some(entry.latest.clone());
        Some(WriteRequest {
            resource: resource.clone(),
            token: entry.write_slot.issue(),
            snapshot: entry.latest.clone(),
            error_prefix: entry.error_prefix,
        })
    }

    pub(super) fn complete(
        &mut self,
        request: &WriteRequest<S>,
        result: Result<(), String>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&request.resource) else {
            return false;
        };
        if !entry.write_slot.commit(&request.token) {
            return false;
        }
        entry.in_flight = None;
        match result {
            Ok(()) => {
                entry.last_saved = Some(request.snapshot.clone());
                entry.failure = None;
                if Arc::ptr_eq(&entry.latest, &request.snapshot) {
                    entry.dirty.clear();
                    entry.flush_requested = false;
                }
            }
            Err(error) => entry.failure = Some(error),
        }
        true
    }

    pub(super) fn next_home_write(&mut self) -> Option<WriteRequest<S>> {
        let resource = self.entries.iter().find_map(|(resource, entry)| {
            (matches!(resource, ResourceKey::Home(_))
                && entry.flush_requested
                && entry.in_flight.is_none())
            .then(|| resource.clone())
        })?;
        self.begin_write(&resource)
    }

    pub(super) fn is_dirty(&self, key: &DirtyKey) -> bool {
        self.entries
            .get(&key.resource())
            .is_some_and(|entry| entry.dirty.contains(key))
    }

    pub(super) fn needs_flush_snapshot(&self, key: &DirtyKey) -> bool {
        if !self.is_dirty(key) {
            return false;
        }
        self.entries.get(&key.resource()).is_some_and(|entry| {
            !entry.dirty.is_empty()
                && !entry.flush_requested
                && entry
                    .in_flight
                    .as_ref()
                    .is_none_or(|snapshot| !Arc::ptr_eq(snapshot, &entry.latest))
        })
    }

    pub(super) fn pending_error_prefix(&self, resource: &ResourceKey) -> Option<&'static str> {
        self.entries
            .get(resource)
            .filter(|entry| !entry.dirty.is_empty())
            .map(|entry| entry.error_prefix)
    }

    // Immediate server edits are transactional: on failure restore the previous
    // pending config rather than retrying a rejected add/edit/delete later.
    pub(super) fn take_config(&mut self) -> Option<Entry<S>> {
        self.entries.remove(&ResourceKey::Config)
    }

    pub(super) fn restore_config(&mut self, previous: Option<Entry<S>>) {
        self.entries.remove(&ResourceKey::Config);
        if let Some(previous) = previous {
            self.entries.insert(ResourceKey::Config, previous);
        }
    }
}
