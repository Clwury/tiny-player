mod adapter;
mod model;

#[cfg(test)]
mod tests;

pub(crate) use adapter::{AppPersistence, FilePersistence};
pub(crate) use model::DirtyKey;

use std::{
    cell::RefCell,
    collections::HashMap,
    future::poll_fn,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
    time::Duration,
};

use gpui::{App, AppContext as _, Global, Task};

use crate::{
    effects::EffectHandle, home::cache::HomeSnapshot, observability::TraceId, server::CachedServer,
    storage::ServerCache,
};
use model::{PersistenceCoordinator, ResourceKey, WriteRequest};

pub(crate) const SETTINGS_DEBOUNCE: Duration = Duration::from_millis(350);
const HOME_DEBOUNCE: Duration = Duration::from_millis(450);

enum Snapshot {
    Settings {
        cache: ServerCache,
        adapter: Arc<dyn AppPersistence>,
    },
    Home {
        server: Box<CachedServer>,
        snapshot: HomeSnapshot,
        adapter: Arc<dyn AppPersistence>,
    },
}

impl Snapshot {
    fn write(&self) -> anyhow::Result<()> {
        let trace = TraceId::start(match self {
            Self::Settings { .. } => "persistence.settings",
            Self::Home { .. } => "persistence.home_snapshot",
        });
        let result = match self {
            Self::Settings { cache, adapter } => adapter.save_settings(cache),
            Self::Home {
                server,
                snapshot,
                adapter,
            } => adapter.save_home(server, snapshot),
        };
        trace.record(if result.is_ok() { "saved" } else { "failed" });
        result
    }
}

#[derive(Default)]
struct SharedState {
    coordinator: PersistenceCoordinator<Snapshot>,
    home_worker_running: bool,
    waiters: Vec<Waker>,
}

#[derive(Default)]
struct Runtime {
    // Only short reducer operations hold this lock. Rendering never accesses it,
    // and IO always runs after dropping the guard.
    state: Arc<Mutex<SharedState>>,
    timers: RefCell<HashMap<ResourceKey, EffectHandle<Task<()>>>>,
    home_worker: RefCell<Option<Task<()>>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // A committed write is drained, not cancelled, even if the last page
        // and the global are released together during application shutdown.
        if let Some(worker) = self.home_worker.get_mut().take() {
            worker.detach();
        }
    }
}

/// Application-wide runner, shared by the shell and each workspace. Timers are
/// cancellable and token-checked; committed Home writes use one serial worker.
/// Closing a page flushes its immutable snapshot; close/quit await the same
/// drain barrier. No task owns an entity or keeps a closed page alive.
#[derive(Clone, Default)]
pub(crate) struct PersistenceService(Rc<Runtime>);

impl Global for PersistenceService {}

impl std::fmt::Debug for PersistenceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistenceService").finish_non_exhaustive()
    }
}

impl PersistenceService {
    pub(crate) fn get(cx: &mut App) -> Self {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        cx.global::<Self>().clone()
    }

    pub(crate) fn schedule_settings(
        &self,
        key: DirtyKey,
        cache: ServerCache,
        adapter: Arc<dyn AppPersistence>,
        error_prefix: &'static str,
        on_error: impl Fn(String, &mut App) + 'static,
        cx: &mut App,
    ) {
        debug_assert!(matches!(key.resource(), ResourceKey::Config));
        self.schedule(
            key,
            Snapshot::Settings { cache, adapter },
            error_prefix,
            on_error,
            cx,
        );
    }

    pub(crate) fn schedule_home(
        &self,
        server: CachedServer,
        snapshot: HomeSnapshot,
        adapter: Arc<dyn AppPersistence>,
        cx: &mut App,
    ) {
        let key = DirtyKey::HomeSnapshot(server.workspace_identity());
        self.schedule(
            key,
            Snapshot::Home {
                server: Box::new(server),
                snapshot,
                adapter,
            },
            "保存首页缓存失败",
            |_, _| {},
            cx,
        );
    }

    fn schedule(
        &self,
        key: DirtyKey,
        snapshot: Snapshot,
        error_prefix: &'static str,
        on_error: impl Fn(String, &mut App) + 'static,
        cx: &mut App,
    ) {
        let resource = key.resource();
        let debounce = match resource {
            ResourceKey::Config => SETTINGS_DEBOUNCE,
            ResourceKey::Home(_) => HOME_DEBOUNCE,
        };
        let token = self.0.state.lock().unwrap().coordinator.schedule(
            key.clone(),
            snapshot,
            error_prefix,
            cx.background_executor().now(),
            debounce,
        );
        let Some(token) = token else { return };
        let weak = Rc::downgrade(&self.0);
        let task_resource = resource.clone();
        let timer = cx.spawn(async move |cx| {
            loop {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                let remaining = runtime.state.lock().unwrap().coordinator.remaining(
                    &task_resource,
                    &token,
                    cx.background_executor().now(),
                );
                let Some(remaining) = remaining else { return };
                if remaining.is_zero() {
                    cx.update(|cx| {
                        // The executing timer remains in its handle until the
                        // next schedule/drop; flush does not cancel itself.
                        let service = Self(runtime);
                        if let Some(error) = service.flush_inner(&key, cx) {
                            on_error(error, cx);
                        }
                    });
                    return;
                }
                drop(runtime);
                cx.background_executor().timer(remaining).await;
            }
        });
        self.0
            .timers
            .borrow_mut()
            .entry(resource)
            .or_default()
            .replace(timer);
    }

    pub(crate) fn suspend(&self, key: &DirtyKey) {
        self.0
            .state
            .lock()
            .unwrap()
            .coordinator
            .suspend(&key.resource());
        self.0.timers.borrow_mut().remove(&key.resource());
    }

    #[cfg(test)]
    pub(crate) fn is_dirty(&self, key: &DirtyKey) -> bool {
        self.0.state.lock().unwrap().coordinator.is_dirty(key)
    }

    pub(crate) fn needs_flush_snapshot(&self, key: &DirtyKey) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .coordinator
            .needs_flush_snapshot(key)
    }

    pub(crate) fn flush_home_snapshot(
        &self,
        server: CachedServer,
        snapshot: HomeSnapshot,
        adapter: Arc<dyn AppPersistence>,
        cx: &mut App,
    ) {
        let key = DirtyKey::HomeSnapshot(server.workspace_identity());
        // on_app_quit cannot spawn foreground timers. Stage the final immutable
        // snapshot directly, then drain through the same worker as autosave.
        self.0.state.lock().unwrap().coordinator.schedule(
            key.clone(),
            Snapshot::Home {
                server: Box::new(server),
                snapshot,
                adapter,
            },
            "保存首页缓存失败",
            cx.background_executor().now(),
            Duration::ZERO,
        );
        self.flush(&key, cx);
    }

    pub(crate) fn pending_settings_error_prefix(&self) -> Option<&'static str> {
        self.0
            .state
            .lock()
            .unwrap()
            .coordinator
            .pending_error_prefix(&ResourceKey::Config)
    }

    pub(crate) fn flush(&self, key: &DirtyKey, cx: &mut App) -> Option<String> {
        self.0.timers.borrow_mut().remove(&key.resource());
        self.flush_inner(key, cx)
    }

    fn flush_inner(&self, key: &DirtyKey, cx: &mut App) -> Option<String> {
        let resource = key.resource();
        self.0.state.lock().unwrap().coordinator.flush(&resource);
        match resource {
            ResourceKey::Config => self.write_settings().err().map(|error| error.to_string()),
            ResourceKey::Home(_) => {
                self.start_home_worker(cx);
                None
            }
        }
    }

    pub(crate) fn flush_all(&self, cx: &mut App) -> (Option<String>, Task<()>) {
        self.0.timers.borrow_mut().clear();
        self.0.state.lock().unwrap().coordinator.flush_all();
        let error = self.write_settings().err().map(|error| error.to_string());
        self.start_home_worker(cx);
        (error, self.drain(cx))
    }

    fn write_settings(&self) -> anyhow::Result<()> {
        let request = self
            .0
            .state
            .lock()
            .unwrap()
            .coordinator
            .begin_write(&ResourceKey::Config);
        let Some(request) = request else {
            return Ok(());
        };
        let result = request.snapshot.write();
        self.complete(&request, &result);
        result.map_err(|error| anyhow::anyhow!("{}：{error}", request.error_prefix))
    }

    fn complete(&self, request: &WriteRequest<Snapshot>, result: &anyhow::Result<()>) {
        self.0.state.lock().unwrap().coordinator.complete(
            request,
            result.as_ref().map(|_| ()).map_err(ToString::to_string),
        );
    }

    /// Transactional writes retain the pre-edit pending state on failure. This
    /// is also used by server add/edit/delete, before publishing their result.
    pub(crate) fn save_settings_now(
        &self,
        cache: ServerCache,
        adapter: Arc<dyn AppPersistence>,
    ) -> anyhow::Result<()> {
        let previous = {
            let mut state = self.0.state.lock().unwrap();
            let previous = state.coordinator.take_config();
            state.coordinator.schedule(
                DirtyKey::Settings,
                Snapshot::Settings { cache, adapter },
                "",
                std::time::Instant::now(),
                Duration::ZERO,
            );
            state.coordinator.flush(&ResourceKey::Config);
            previous
        };
        // Preserve the original error text for immediate-save callers.
        let request = self
            .0
            .state
            .lock()
            .unwrap()
            .coordinator
            .begin_write(&ResourceKey::Config)
            .expect("scheduled config");
        let result = request.snapshot.write();
        self.complete(&request, &result);
        if result.is_err() {
            self.0
                .state
                .lock()
                .unwrap()
                .coordinator
                .restore_config(previous);
        } else {
            self.0.timers.borrow_mut().remove(&ResourceKey::Config);
        }
        result
    }

    fn start_home_worker(&self, cx: &mut App) {
        let first = {
            let mut state = self.0.state.lock().unwrap();
            if state.home_worker_running {
                return;
            }
            let Some(first) = state.coordinator.next_home_write() else {
                return;
            };
            state.home_worker_running = true;
            first
        };
        let shared = self.0.state.clone();
        self.0
            .home_worker
            .replace(Some(cx.background_spawn(async move {
                let mut request = first;
                loop {
                    let result = request.snapshot.write();
                    let (next, waiters) = {
                        let mut state = shared.lock().unwrap();
                        state
                            .coordinator
                            .complete(&request, result.map_err(|error| error.to_string()));
                        let next = state.coordinator.next_home_write();
                        let waiters = if next.is_none() {
                            state.home_worker_running = false;
                            std::mem::take(&mut state.waiters)
                        } else {
                            Vec::new()
                        };
                        (next, waiters)
                    };
                    for waiter in waiters {
                        waiter.wake();
                    }
                    let Some(next) = next else { break };
                    request = next;
                }
            })));
    }

    pub(crate) fn drain(&self, cx: &mut App) -> Task<()> {
        let shared = self.0.state.clone();
        cx.background_spawn(poll_fn(move |cx| {
            let mut state = shared.lock().unwrap();
            if !state.home_worker_running {
                return Poll::Ready(());
            }
            if !state
                .waiters
                .iter()
                .any(|waker| waker.will_wake(cx.waker()))
            {
                state.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        }))
    }
}
