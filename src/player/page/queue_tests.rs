use super::*;
use crate::player::gateway::{PlaybackReport, ResolvedPlayback};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
struct FakeGateway {
    fail: bool,
    calls: Mutex<Vec<(String, String)>>,
}
impl PlaybackGateway for FakeGateway {
    fn report(&self, _: &PlaybackReport) -> anyhow::Result<()> {
        unreachable!()
    }
    fn resolve_source(&self, item: &str, source: &str) -> anyhow::Result<ResolvedPlayback> {
        self.calls
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        anyhow::ensure!(!self.fail, "offline");
        Ok(ResolvedPlayback {
            item_id: format!("{item}-resolved"),
            media_source_id: source.into(),
            url: "https://example.invalid/video".into(),
            http_headers: vec![],
            content_length: Some(123),
            play_session_id: None,
        })
    }
    fn subtitle_tracks(
        &self,
        _: &crate::emby::MediaSource,
        _: &str,
        _: &str,
    ) -> Vec<PlaybackTrack> {
        vec![]
    }
}

fn events(
    page: &gpui::Entity<PlaybackPage>,
    cx: &mut gpui::VisualTestContext,
) -> Rc<RefCell<Vec<PlaybackEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(page, move |_, event: &PlaybackEvent, _| {
            events.borrow_mut().push(event.clone())
        })
        .detach();
    });
    events
}
struct Cancelled(Arc<AtomicBool>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn pending_task(cx: &mut Context<PlaybackPage>, cancelled: Arc<AtomicBool>) -> gpui::Task<()> {
    let guard = Cancelled(cancelled);
    cx.spawn(async move |_, _| {
        let _guard = guard;
        std::future::pending::<()>().await;
    })
}

#[gpui::test]
fn failed_manual_switch_restores_user_pause_and_cache_pause_policy(cx: &mut gpui::TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let gateway = Arc::new(FakeGateway {
        fail: true,
        ..Default::default()
    });
    let recorded = events(&page, cx);
    for (user_paused, cache_paused, expected_paused) in [
        (false, false, false),
        (true, false, true),
        (false, true, true),
    ] {
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.queue_effects.gateway = gateway.clone();
                page.session.timeline_mut().user_paused = user_paused;
                page.session.timeline_mut().paused = user_paused || cache_paused;
                page.session.timeline_mut().paused_for_cache = cache_paused;
                page.begin_queue_switch(QueueAction::Select(2), false, window, cx);
                assert!(page.session.queue.view_model().loading);
                assert!(page.session.timeline().user_paused);
            })
        });
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert!(!page.session.queue.view_model().loading);
            assert_eq!(page.session.queue.queue().current_index, 0);
            assert_eq!(
                page.session.queue.view_model().error,
                Some("切换剧集失败：offline")
            );
            assert_eq!(page.session.timeline().user_paused, user_paused);
            assert_eq!(page.session.timeline().paused, expected_paused);
            assert!(!page.playback_reporting_closed());
        });
    }
    assert_eq!(gateway.calls.lock().unwrap().len(), 3);
    assert!(recorded.borrow().is_empty());
}

#[gpui::test]
fn backend_pause_failure_prevents_queue_resolution_without_changing_playback(
    cx: &mut gpui::TestAppContext,
) {
    use crate::player::backend::test_support::{FakeState, adapter};
    let (page, cx) = episodes::tests::playback_window(cx);
    let gateway = Arc::new(FakeGateway::default());
    let state = Rc::new(RefCell::new(FakeState {
        fail_pause: true,
        ..Default::default()
    }));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.queue_effects.gateway = gateway.clone();
            page.video = adapter(state.clone(), false);
            page.begin_queue_switch(QueueAction::Next, false, window, cx);
            assert!(!page.session.queue.view_model().loading);
            assert!(
                page.session
                    .queue
                    .view_model()
                    .error
                    .unwrap()
                    .contains("synthetic command failure")
            );
            assert!(!page.session.timeline().paused && !page.session.timeline().user_paused);
            assert_eq!(page.session.queue.queue().current_index, 0);
        })
    });
    cx.run_until_parked();
    assert!(gateway.calls.lock().unwrap().is_empty());
    assert_eq!(state.borrow().commands, [BackendCommand::Pause]);
}

#[gpui::test]
fn failed_queue_resolution_keeps_pause_when_backend_resume_also_fails(
    cx: &mut gpui::TestAppContext,
) {
    use crate::player::backend::test_support::{FakeState, adapter};
    let (page, cx) = episodes::tests::playback_window(cx);
    let gateway = Arc::new(FakeGateway {
        fail: true,
        ..Default::default()
    });
    let state = Rc::new(RefCell::new(FakeState {
        fail_resume: true,
        ..Default::default()
    }));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.queue_effects.gateway = gateway.clone();
            page.video = adapter(state.clone(), false);
            page.begin_queue_switch(QueueAction::Next, false, window, cx);
        })
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert!(!page.session.queue.view_model().loading);
        assert!(page.session.timeline().paused && page.session.timeline().user_paused);
        let error = page.session.queue.view_model().error.unwrap();
        assert!(error.contains("切换下一集失败：offline"));
        assert!(error.contains("synthetic command failure"));
        assert_eq!(page.session.queue.queue().current_index, 0);
        assert!(!page.playback_reporting_closed());
    });
    assert_eq!(gateway.calls.lock().unwrap().len(), 1);
    assert_eq!(
        state.borrow().commands,
        [BackendCommand::Pause, BackendCommand::Resume]
    );
}

#[gpui::test]
fn automatic_failure_publishes_finished_update_without_resuming_episode(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let recorded = events(&page, cx);
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.queue_effects.gateway = Arc::new(FakeGateway {
                fail: true,
                ..Default::default()
            });
            page.session.timeline_mut().ended = true;
            page.session.timeline_mut().paused = true;
            page.begin_queue_switch(QueueAction::Next, true, window, cx);
        })
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert!(page.session.timeline().ended && page.session.timeline().paused);
        assert!(!page.session.timeline().user_paused);
        assert!(page.playback_reporting_closed());
        assert_eq!(
            page.session.queue.view_model().error,
            Some("切换下一集失败：offline")
        );
    });
    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 1);
    let PlaybackEvent::Update { update } = &recorded[0] else {
        panic!("expected final update");
    };
    assert!(update.ended);
    assert_eq!(update.position_ticks, 18_000_000_000);
    assert!(update.selected_item_id.is_none());
}

#[gpui::test]
fn stale_results_cannot_replace_page_clear_error_or_cancel_current_effect(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let recorded = events(&page, cx);
    let cancelled = Arc::new(AtomicBool::new(false));
    page.update(cx, |page, cx| {
        let old = page
            .session
            .queue
            .begin(QueueAction::Select(2), false, false, false)
            .unwrap();
        page.cancel_queue_switch();
        let current = page
            .session
            .queue
            .begin(QueueAction::Next, false, false, false)
            .unwrap();
        page.queue_effects
            .task
            .replace(pending_task(cx, cancelled.clone()));
        let gateway = FakeGateway::default();
        let result = effect::resolve(&gateway, &old.queue, Default::default(), Default::default());
        page.finish_queue_switch(old.clone(), result, cx);
        page.finish_queue_switch(old, Err(anyhow::anyhow!("late error")), cx);
        assert!(page.session.queue.view_model().loading);
        assert!(page.session.queue.view_model().error.is_none());
        assert!(!cancelled.load(Ordering::SeqCst));
        let result = effect::resolve(
            &gateway,
            &current.queue,
            Default::default(),
            Default::default(),
        );
        page.finish_queue_switch(current.clone(), result, cx);
        page.finish_queue_switch(current, Err(anyhow::anyhow!("duplicate")), cx);
    });
    cx.run_until_parked();
    assert!(cancelled.load(Ordering::SeqCst));
    let recorded = recorded.borrow();
    assert_eq!(recorded.len(), 1);
    let PlaybackEvent::Replace { request, update } = &recorded[0] else {
        panic!("expected replacement");
    };
    assert_eq!(request.queue.current_index, 1);
    assert_eq!(request.emby.item_id, "episode-1-resolved");
    assert_eq!(
        request.queue.items[0].playback_position_ticks,
        Some(450_000_000)
    );
    assert_eq!(update.selected_item_id.as_deref(), Some("episode-1"));
}

#[gpui::test]
fn back_and_page_release_cancel_owned_queue_continuation(cx: &mut gpui::TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_on_release = Arc::new(AtomicBool::new(false));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.session
                .queue
                .begin(QueueAction::Next, false, false, false)
                .unwrap();
            page.queue_effects
                .task
                .replace(pending_task(cx, cancelled.clone()));
            page.back_to_detail(window, cx);
            assert!(!page.session.queue.view_model().loading);
        })
    });
    cx.run_until_parked();
    assert!(cancelled.load(Ordering::SeqCst));
    page.update(cx, |page, cx| {
        page.queue_effects
            .task
            .replace(pending_task(cx, cancelled_on_release.clone()))
    });
    cx.run_until_parked();
    let weak = page.downgrade();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert!(cancelled_on_release.load(Ordering::SeqCst));
}

#[gpui::test]
fn returning_to_detail_cancels_pending_backend_poll(cx: &mut gpui::TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let cancelled = Arc::new(AtomicBool::new(false));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.session.timeline_mut().paused = true;
            page.session.timeline_mut().cache_state = Some(PlaybackCacheState {
                paused_for_cache: true,
                ..Default::default()
            });
            let token = page.session.begin_poll(true, false).unwrap();
            page.backend_poll
                .replace(pending_task(cx, cancelled.clone()));
            page.back_to_detail(window, cx);
            assert!(!page.session.complete_poll(
                &token,
                &page.emby.server.workspace_identity(),
                true,
                false
            ));
            assert!(page.session.begin_poll(true, false).is_none());
        })
    });
    cx.run_until_parked();
    assert!(cancelled.load(Ordering::SeqCst));
}
