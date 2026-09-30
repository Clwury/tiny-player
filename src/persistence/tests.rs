use std::sync::atomic::{AtomicBool, Ordering};

use gpui::TestAppContext;

use super::*;

#[derive(Default)]
struct MemoryPersistence {
    attempts: Mutex<Vec<String>>,
    fail_settings: AtomicBool,
    fail_home: AtomicBool,
}

impl AppPersistence for MemoryPersistence {
    fn load_settings(&self) -> anyhow::Result<ServerCache> {
        Ok(ServerCache::empty())
    }
    fn load_home(&self, _: &CachedServer) -> anyhow::Result<Option<HomeSnapshot>> {
        Ok(None)
    }
    fn save_settings(&self, cache: &ServerCache) -> anyhow::Result<()> {
        self.attempts
            .lock()
            .unwrap()
            .push(format!("settings:{}", cache.playback.cache_secs));
        anyhow::ensure!(
            !self.fail_settings.load(Ordering::SeqCst),
            "settings write failed"
        );
        Ok(())
    }
    fn save_home(&self, server: &CachedServer, snapshot: &HomeSnapshot) -> anyhow::Result<()> {
        let value = serde_json::to_value(snapshot)?;
        self.attempts.lock().unwrap().push(format!(
            "home:{}:{}",
            server.user_id.as_deref().unwrap(),
            value["saved_at_unix"]
        ));
        anyhow::ensure!(!self.fail_home.load(Ordering::SeqCst), "home write failed");
        Ok(())
    }
}

fn cache(value: f64) -> ServerCache {
    let mut cache = ServerCache::empty();
    cache.playback.cache_secs = value;
    cache
}

fn server(user: &str) -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "local", "server_id": "remote", "user_id": user,
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap()
}

fn snapshot(user: &str, value: u64) -> HomeSnapshot {
    serde_json::from_value(serde_json::json!({
        "version": 2, "server_id": "local", "remote_server_id": "remote",
        "user_id": user, "saved_at_unix": value
    }))
    .unwrap()
}

#[gpui::test]
fn app_shared_service_coalesces_keys_and_flush_all_drains_every_workspace(cx: &mut TestAppContext) {
    let adapter = Arc::new(MemoryPersistence::default());
    let service = cx.update(PersistenceService::get);
    let second = cx.update(PersistenceService::get);
    assert!(Rc::ptr_eq(&service.0, &second.0));
    cx.update(|cx| {
        for (key, value) in [
            (DirtyKey::Settings, 10.0),
            (DirtyKey::Window, 20.0),
            (DirtyKey::SearchHistory, 30.0),
        ] {
            service.schedule_settings(
                key,
                cache(value),
                adapter.clone(),
                "failed",
                |_, _| panic!("unexpected failure"),
                cx,
            );
        }
        for user in ["one", "two"] {
            second.schedule_home(server(user), snapshot(user, 1), adapter.clone(), cx);
            second.schedule_home(server(user), snapshot(user, 2), adapter.clone(), cx);
        }
    });
    assert!(adapter.attempts.lock().unwrap().is_empty());
    cx.run_until_parked();
    let (error, task) = cx.update(|cx| service.flush_all(cx));
    assert!(error.is_none());
    task.detach();
    cx.run_until_parked();
    let mut writes = adapter.attempts.lock().unwrap().clone();
    writes.sort();
    assert_eq!(writes, ["home:one:2", "home:two:2", "settings:30"]);
    for _ in 0..2 {
        cx.update(|cx| service.flush_all(cx).1.detach());
    }
    cx.executor().advance_clock(HOME_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(adapter.attempts.lock().unwrap().len(), 3);
}

#[gpui::test]
fn failed_immediate_edit_restores_pending_snapshot_and_its_existing_timer(cx: &mut TestAppContext) {
    let adapter = Arc::new(MemoryPersistence::default());
    let service = cx.update(PersistenceService::get);
    cx.update(|cx| {
        service.schedule_settings(
            DirtyKey::Window,
            cache(10.0),
            adapter.clone(),
            "window failed",
            |_, _| panic!("unexpected failure"),
            cx,
        )
    });
    cx.run_until_parked();
    adapter.fail_settings.store(true, Ordering::SeqCst);
    assert!(
        service
            .save_settings_now(cache(20.0), adapter.clone())
            .is_err()
    );
    assert_eq!(
        service.pending_settings_error_prefix(),
        Some("window failed")
    );
    assert!(service.is_dirty(&DirtyKey::Window));
    adapter.fail_settings.store(false, Ordering::SeqCst);
    cx.executor().advance_clock(SETTINGS_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(
        *adapter.attempts.lock().unwrap(),
        ["settings:20", "settings:10"]
    );
    assert!(!service.is_dirty(&DirtyKey::Window));
}

#[gpui::test]
fn successful_immediate_edit_supersedes_debounced_snapshot(cx: &mut TestAppContext) {
    let adapter = Arc::new(MemoryPersistence::default());
    let service = cx.update(PersistenceService::get);
    cx.update(|cx| {
        service.schedule_settings(
            DirtyKey::Settings,
            cache(10.0),
            adapter.clone(),
            "failed",
            |_, _| panic!("unexpected failure"),
            cx,
        )
    });
    cx.run_until_parked();
    service
        .save_settings_now(cache(20.0), adapter.clone())
        .unwrap();
    cx.executor().advance_clock(SETTINGS_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(*adapter.attempts.lock().unwrap(), ["settings:20"]);
    assert!(service.pending_settings_error_prefix().is_none());
}

#[gpui::test]
fn failed_home_write_is_retained_for_explicit_flush_without_a_retry_loop(cx: &mut TestAppContext) {
    let adapter = Arc::new(MemoryPersistence::default());
    adapter.fail_home.store(true, Ordering::SeqCst);
    let service = cx.update(PersistenceService::get);
    let key = DirtyKey::HomeSnapshot(server("one").workspace_identity());
    cx.update(|cx| service.schedule_home(server("one"), snapshot("one", 1), adapter.clone(), cx));
    cx.run_until_parked();
    cx.executor().advance_clock(HOME_DEBOUNCE);
    cx.run_until_parked();
    assert!(service.is_dirty(&key));
    assert_eq!(adapter.attempts.lock().unwrap().len(), 1);
    cx.executor().advance_clock(HOME_DEBOUNCE * 10);
    cx.run_until_parked();
    assert_eq!(adapter.attempts.lock().unwrap().len(), 1);
    adapter.fail_home.store(false, Ordering::SeqCst);
    cx.update(|cx| service.flush_all(cx).1.detach());
    cx.run_until_parked();
    assert!(!service.is_dirty(&key));
    assert_eq!(adapter.attempts.lock().unwrap().len(), 2);
}

#[gpui::test]
fn suspended_home_does_not_write_on_settings_close_and_final_snapshot_flushes_once(
    cx: &mut TestAppContext,
) {
    let adapter = Arc::new(MemoryPersistence::default());
    let service = cx.update(PersistenceService::get);
    let key = DirtyKey::HomeSnapshot(server("one").workspace_identity());
    cx.update(|cx| {
        service.schedule_home(server("one"), snapshot("one", 1), adapter.clone(), cx);
        service.suspend(&key);
        service.flush_all(cx).1.detach();
    });
    cx.run_until_parked();
    assert!(adapter.attempts.lock().unwrap().is_empty());
    cx.update(|cx| {
        service.flush_home_snapshot(server("one"), snapshot("one", 2), adapter.clone(), cx);
        service.flush_all(cx).1.detach();
        service.flush(&key, cx);
    });
    cx.run_until_parked();
    assert_eq!(*adapter.attempts.lock().unwrap(), ["home:one:2"]);
}
