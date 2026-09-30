use super::*;
use gpui::AppContext as _;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn pushing_same_key_replaces_the_previous_notification() {
    let mut queue = NotificationQueue::default();
    let first = queue.push("home", "first".into());
    let second = queue.push("home", "second".into());

    assert_ne!(first, second);
    assert_eq!(queue.iter().count(), 1);
    assert_eq!(queue.iter().next().unwrap().notification.message, "second");
}

#[test]
fn removing_notification_by_id_reports_whether_it_existed() {
    let mut queue = NotificationQueue::default();
    let id = queue.push("home", "error".into());

    assert!(queue.remove(id));
    assert!(!queue.remove(id));
    assert!(queue.is_empty());
}

#[test]
fn queue_keeps_only_the_most_recent_notifications() {
    let mut queue = NotificationQueue::default();
    for index in 0..(NOTIFICATION_MAX_ITEMS + 2) {
        queue.push(index, format!("error-{index}").into());
    }

    let retained = queue.iter().map(|entry| entry.key).collect::<Vec<_>>();
    assert_eq!(
        retained,
        (2..(NOTIFICATION_MAX_ITEMS + 2)).collect::<Vec<_>>()
    );
}

#[test]
fn clearing_queue_keeps_ids_monotonic_for_old_autohide_timers() {
    let mut queue = NotificationQueue::default();
    let old_id = queue.push("old", "old error".into());
    queue.clear();
    let new_id = queue.push("new", "new error".into());

    assert!(new_id > old_id);
    assert!(!queue.remove(old_id));
    assert_eq!(queue.iter().next().unwrap().notification.id, new_id);
}

#[test]
fn deadlines_reject_other_owners_accounts_and_reused_ids() {
    let mut first = NotificationQueue::default();
    let id = first.push("home", "old".into());
    let old = first.items.back_mut().unwrap().deadline.issue();
    let mut other = NotificationQueue::default();
    assert_eq!(other.push("home", "other owner".into()), id);
    let other_token = other.items.back_mut().unwrap().deadline.issue();
    assert!(!other.expire(id, &old));
    assert!(other.expire(id, &other_token));
    let mut other_account = NotificationQueue::new(WorkspaceIdentity {
        user_id: Some("another user".into()),
        ..Default::default()
    });
    assert_eq!(other_account.push("home", "other account".into()), id);
    let account_token = other_account.items.back_mut().unwrap().deadline.issue();
    assert!(!other_account.expire(id, &old));
    assert!(other_account.expire(id, &account_token));
    first.clear();
    first.next_id = u64::MAX; // Exercise ID wrap without waiting for it.
    first.push("unrelated", "zero".into());
    assert_eq!(first.push("home", "replacement".into()), id);
    let current = first.items.back_mut().unwrap().deadline.issue();
    assert!(!first.expire(id, &old));
    assert!(first.expire(id, &current));
    assert!(!first.expire(id, &current));
    assert_eq!(first.iter().next().unwrap().key, "unrelated");
}

#[derive(Default)]
struct Host {
    notifications: NotificationQueue<usize>,
}

impl Host {
    fn push(&mut self, key: usize, cx: &mut Context<Self>) {
        self.notifications
            .push_autohide(key, format!("error-{key}").into(), cx, |host| {
                &mut host.notifications
            });
    }
}

#[gpui::test]
fn replacement_restarts_only_its_own_five_second_deadline(cx: &mut gpui::TestAppContext) {
    let host = cx.new(|_| Host::default());
    host.update(cx, |host, cx| {
        host.push(0, cx);
        host.push(1, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();
    host.update(cx, |host, cx| host.push(0, cx));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    host.read_with(cx, |host, _| {
        assert_eq!(
            host.notifications
                .iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            [0]
        );
    });
    cx.executor().advance_clock(Duration::from_millis(2999));
    cx.run_until_parked();
    host.read_with(cx, |host, _| assert!(!host.notifications.is_empty()));
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    host.read_with(cx, |host, _| assert!(host.notifications.is_empty()));
}

struct Cancelled(Arc<AtomicBool>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn watch_latest(host: &mut Host, cx: &mut Context<Host>) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = Cancelled(cancelled.clone());
    host.notifications
        .items
        .back_mut()
        .unwrap()
        .task
        .replace(cx.spawn(async move |_, _| {
            let _guard = guard;
            std::future::pending::<()>().await;
        }));
    cancelled
}

#[gpui::test]
fn removing_entries_cancels_tasks_and_stale_callbacks_preserve_replacements(
    cx: &mut gpui::TestAppContext,
) {
    let host = cx.new(|_| Host::default());
    let replaced = host.update(cx, |host, cx| {
        host.push(0, cx);
        watch_latest(host, cx)
    });
    cx.run_until_parked();
    let (dismissed, retained, evicted, cleared) = host.update(cx, |host, cx| {
        let old = host
            .notifications
            .items
            .back()
            .unwrap()
            .deadline
            .latest()
            .unwrap();
        host.push(0, cx);
        let id = host
            .notifications
            .items
            .back()
            .unwrap()
            .entry
            .notification
            .id;
        let dismissed = watch_latest(host, cx);
        assert!(!host.notifications.expire(id, &old));
        assert!(!dismissed.load(Ordering::SeqCst));
        assert!(host.notifications.remove(id));
        host.push(1, cx);
        let retained = watch_latest(host, cx);
        host.push(2, cx);
        let evicted = watch_latest(host, cx);
        host.notifications.retain(|entry| entry.key != 1);
        for key in 3..(3 + NOTIFICATION_MAX_ITEMS) {
            host.push(key, cx);
        }
        let cleared = watch_latest(host, cx);
        host.notifications.clear();
        (dismissed, retained, evicted, cleared)
    });
    cx.run_until_parked();
    for cancelled in [replaced, dismissed, retained, evicted, cleared] {
        assert!(cancelled.load(Ordering::SeqCst));
    }
    let on_drop = host.update(cx, |host, cx| {
        host.push(99, cx);
        watch_latest(host, cx)
    });
    cx.run_until_parked();
    assert!(!on_drop.load(Ordering::SeqCst));
    let weak = host.downgrade();
    cx.update(|_| drop(host));
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert!(on_drop.load(Ordering::SeqCst));
}
