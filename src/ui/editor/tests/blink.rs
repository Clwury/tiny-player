use super::*;
use gpui::AppContext as _;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[gpui::test]
fn focus_reuses_a_wait_but_refocus_and_other_editors_reject_old_completions(
    cx: &mut gpui::TestAppContext,
) {
    let editor = cx.new(|cx| Editor::new("", cx));
    let other = cx.new(|cx| Editor::new("", cx));
    let notifications = Rc::new(Cell::new(0));
    let _subscription = cx.update(|cx| {
        cx.observe(&editor, {
            let notifications = notifications.clone();
            move |_, _| notifications.set(notifications.get() + 1)
        })
    });
    let old = editor.update(cx, |editor, cx| {
        editor.sync_cursor_blink(true, cx);
        let token = editor.cursor_blink.latest().unwrap();
        editor.sync_cursor_blink(true, cx);
        assert!(token.is_current(&editor.cursor_blink));
        editor.note_cursor_activity();
        assert!(token.is_current(&editor.cursor_blink));
        token
    });
    editor.update(cx, |editor, cx| {
        editor.sync_cursor_blink(false, cx);
        editor.finish_cursor_blink(&old, cx);
        assert!(!editor.cursor_blink.is_active());
        editor.sync_cursor_blink(true, cx);
        let current = editor.cursor_blink.latest().unwrap();
        let deadline = editor.cursor_blink_deadline;
        editor.finish_cursor_blink(&old, cx);
        assert_eq!(editor.cursor_blink_deadline, deadline);
        assert!(current.is_current(&editor.cursor_blink));
    });
    let foreign = other.update(cx, |editor, cx| {
        editor.sync_cursor_blink(true, cx);
        editor.cursor_blink.latest().unwrap()
    });
    editor.update(cx, |editor, cx| editor.finish_cursor_blink(&foreign, cx));
    cx.run_until_parked();
    assert_eq!(notifications.get(), 0);
    editor.update(cx, |editor, cx| {
        let current = editor.cursor_blink.latest().unwrap();
        editor.finish_cursor_blink(&current, cx);
        editor.finish_cursor_blink(&current, cx);
        assert!(!editor.cursor_blink.is_active());
    });
    cx.run_until_parked();
    assert_eq!(notifications.get(), 1);
}

#[gpui::test]
fn activity_extends_the_current_wait_and_due_timer_notifies_once(cx: &mut gpui::TestAppContext) {
    let editor = cx.new(|cx| Editor::new("", cx));
    let notifications = Rc::new(Cell::new(0));
    let _subscription = cx.update(|cx| {
        cx.observe(&editor, {
            let notifications = notifications.clone();
            move |_, _| notifications.set(notifications.get() + 1)
        })
    });
    let token = editor.update(cx, |editor, cx| {
        editor.sync_cursor_blink(true, cx);
        editor.cursor_blink.latest().unwrap()
    });
    cx.run_until_parked();
    editor.update(cx, |editor, _| {
        editor.note_cursor_activity();
        // Keep the monotonic deadline explicitly in the future while advancing
        // GPUI's fake timer clock; no wall-clock sleeps are needed.
        editor.cursor_blink_deadline = Instant::now() + Duration::from_secs(60);
    });
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(notifications.get(), 0);
    editor.update(cx, |editor, _| {
        assert!(token.is_current(&editor.cursor_blink));
        editor.cursor_blink_deadline = Instant::now();
    });
    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(notifications.get(), 1);
    editor.read_with(cx, |editor, _| assert!(!editor.cursor_blink.is_active()));
    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();
    assert_eq!(notifications.get(), 1);
}

struct Cancelled(Arc<AtomicBool>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[gpui::test]
fn blur_and_entity_release_cancel_the_owned_caret_task(cx: &mut gpui::TestAppContext) {
    for blur in [true, false] {
        let editor = cx.new(|cx| Editor::new("", cx));
        let cancelled = Arc::new(AtomicBool::new(false));
        editor.update(cx, |editor, cx| {
            editor.sync_cursor_blink(true, cx);
            let guard = Cancelled(cancelled.clone());
            editor
                .cursor_blink_task
                .replace(cx.spawn(async move |_, _| {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                }));
        });
        cx.run_until_parked();
        assert!(!cancelled.load(Ordering::SeqCst));
        if blur {
            editor.update(cx, |editor, cx| editor.sync_cursor_blink(false, cx));
        }
        cx.update(|_| drop(editor));
        cx.run_until_parked();
        assert!(cancelled.load(Ordering::SeqCst));
    }
}
