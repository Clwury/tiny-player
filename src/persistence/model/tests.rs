use super::*;

const DELAY: Duration = Duration::from_millis(350);

fn schedule(
    store: &mut PersistenceCoordinator<&'static str>,
    key: DirtyKey,
    value: &'static str,
    now: Instant,
) -> Option<RequestToken> {
    store.schedule(key, value, "save failed", now, DELAY)
}

fn home(user: &str) -> DirtyKey {
    DirtyKey::HomeSnapshot(WorkspaceIdentity {
        local_server_id: "server".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some(user.into()),
    })
}

#[test]
fn config_keys_share_one_debounce_and_the_latest_complete_snapshot() {
    let mut store = PersistenceCoordinator::default();
    let now = Instant::now();
    let token = schedule(&mut store, DirtyKey::Settings, "settings", now).unwrap();
    assert!(
        schedule(
            &mut store,
            DirtyKey::SearchHistory,
            "settings+history",
            now + Duration::from_millis(200)
        )
        .is_none()
    );
    assert!(
        schedule(
            &mut store,
            DirtyKey::Window,
            "all",
            now + Duration::from_millis(300)
        )
        .is_none()
    );
    assert_eq!(
        store.remaining(&ResourceKey::Config, &token, now + DELAY),
        Some(Duration::from_millis(300))
    );
    store.flush(&ResourceKey::Config);
    let write = store.begin_write(&ResourceKey::Config).unwrap();
    assert_eq!(*write.snapshot, "all");
    assert!(store.complete(&write, Ok(())));
    for key in [
        DirtyKey::Settings,
        DirtyKey::SearchHistory,
        DirtyKey::Window,
    ] {
        assert!(!store.is_dirty(&key));
    }
    assert!(store.remaining(&ResourceKey::Config, &token, now).is_none());
    store.flush_all();
    assert!(store.begin_write(&ResourceKey::Config).is_none());
}

#[test]
fn edits_during_write_survive_old_completion_and_flush_in_order() {
    let mut store = PersistenceCoordinator::default();
    let now = Instant::now();
    let key = home("one");
    schedule(&mut store, key.clone(), "old", now);
    store.flush(&key.resource());
    let old = store.next_home_write().unwrap();
    schedule(&mut store, key.clone(), "new", now);
    store.flush_all();
    assert!(store.next_home_write().is_none());
    assert!(store.complete(&old, Ok(())));
    assert!(store.is_dirty(&key));
    let new = store.next_home_write().unwrap();
    assert_eq!(*new.snapshot, "new");
    assert!(!store.complete(&old, Err("late failure".into())));
    assert!(store.complete(&new, Ok(())));
    assert!(!store.is_dirty(&key));
    assert_eq!(
        store.entries[&key.resource()].last_saved.as_deref(),
        Some(&"new")
    );
}

#[test]
fn repeated_flush_while_writing_does_not_duplicate_or_retry_the_same_write() {
    let mut store = PersistenceCoordinator::default();
    let key = home("one");
    schedule(&mut store, key.clone(), "value", Instant::now());
    store.flush_all();
    let write = store.next_home_write().unwrap();
    for _ in 0..3 {
        store.flush_all();
    }
    assert!(!store.needs_flush_snapshot(&key));
    assert!(store.complete(&write, Err("disk full".into())));
    assert!(store.next_home_write().is_none());
    assert!(store.is_dirty(&key));
    assert_eq!(
        store.entries[&key.resource()].failure.as_deref(),
        Some("disk full")
    );
    store.flush_all();
    let retry = store.next_home_write().unwrap();
    assert_eq!(*retry.snapshot, "value");
    assert_ne!(retry.token, write.token);
    assert!(store.complete(&retry, Ok(())));
    assert!(!store.is_dirty(&key));
}

#[test]
fn failed_save_keeps_last_valid_snapshot_and_retries_latest_edit() {
    let mut store = PersistenceCoordinator::default();
    let now = Instant::now();
    schedule(&mut store, DirtyKey::Settings, "valid", now);
    store.flush_all();
    let valid = store.begin_write(&ResourceKey::Config).unwrap();
    store.complete(&valid, Ok(()));
    schedule(&mut store, DirtyKey::Settings, "failed", now);
    store.flush_all();
    let failed = store.begin_write(&ResourceKey::Config).unwrap();
    store.complete(&failed, Err("denied".into()));
    assert_eq!(
        store.entries[&ResourceKey::Config].last_saved.as_deref(),
        Some(&"valid")
    );
    assert!(store.is_dirty(&DirtyKey::Settings));
    schedule(&mut store, DirtyKey::Settings, "latest", now);
    store.flush_all();
    let retry = store.begin_write(&ResourceKey::Config).unwrap();
    assert_eq!(*retry.snapshot, "latest");
    store.complete(&retry, Ok(()));
    assert!(store.entries[&ResourceKey::Config].failure.is_none());
}

#[test]
fn suspended_optimistic_snapshot_requires_owner_to_submit_settled_data() {
    let mut store = PersistenceCoordinator::default();
    let key = home("one");
    let now = Instant::now();
    let token = schedule(&mut store, key.clone(), "optimistic", now).unwrap();
    store.suspend(&key.resource());
    assert!(
        store
            .remaining(&key.resource(), &token, now + DELAY)
            .is_none()
    );
    store.flush_all();
    assert!(store.next_home_write().is_none());
    assert!(store.needs_flush_snapshot(&key));
    let settled = schedule(&mut store, key.clone(), "rolled back", now).unwrap();
    assert_ne!(token, settled);
    store.flush_all();
    assert_eq!(*store.next_home_write().unwrap().snapshot, "rolled back");
}

#[test]
fn account_scopes_reject_crossed_results_even_at_the_same_generation() {
    let mut store = PersistenceCoordinator::default();
    let one = home("one");
    let two = home("two");
    let now = Instant::now();
    let timer = schedule(&mut store, one.clone(), "one", now).unwrap();
    schedule(&mut store, two.clone(), "two", now);
    assert!(store.remaining(&two.resource(), &timer, now).is_none());
    store.flush_all();
    let mut write = store.begin_write(&one.resource()).unwrap();
    write.resource = two.resource();
    assert!(!store.complete(&write, Ok(())));
    assert!(store.is_dirty(&one));
    assert!(store.is_dirty(&two));
    write.resource = one.resource();
    assert!(store.complete(&write, Ok(())));
    assert!(!store.is_dirty(&one));
    assert!(store.is_dirty(&two));
}
