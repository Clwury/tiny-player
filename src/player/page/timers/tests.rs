use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};

fn advance(cx: &mut gpui::VisualTestContext, milliseconds: u64) {
    cx.executor()
        .advance_clock(Duration::from_millis(milliseconds));
    cx.run_until_parked();
}

#[gpui::test]
fn volume_and_rate_hide_independently_and_repeated_volume_restarts_its_delay(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    page.update(cx, |page, cx| {
        page.video = adapter(Rc::new(RefCell::new(FakeState::default())), false);
        page.show_volume_indicator(cx);
        page.change_playback_rate(tiny_playback::PlaybackRateChange::Double, cx);
    });
    cx.run_until_parked();
    advance(cx, 600);
    page.update(cx, |page, cx| page.show_volume_indicator(cx));
    cx.run_until_parked();
    advance(cx, 600);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.volume_indicator_visible);
        assert!(!page.presentation.rate_indicator_visible);
        assert_eq!(page.session.controls_view().rate, 2.0);
    });
    advance(cx, 599);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.volume_indicator_visible)
    });
    advance(cx, 1);
    page.read_with(cx, |page, _| {
        assert!(!page.presentation.volume_indicator_visible)
    });
}

#[gpui::test]
fn controls_deadline_respects_hover_drag_and_episode_drawer(cx: &mut gpui::TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    for blocked in 0..4 {
        page.update(cx, |page, cx| {
            page.presentation.fullscreen.controls_visible = true;
            page.presentation.fullscreen.cursor_visible = true;
            page.presentation.fullscreen.mouse_in_controls = blocked == 0;
            page.presentation.fullscreen.mouse_in_back_button = blocked == 1;
            page.session.timeline_mut().progress_drag_position = (blocked == 2).then_some(20.0);
            page.presentation.episode_list.open = blocked == 3;
            page.schedule_fullscreen_controls_hide(cx);
        });
        cx.run_until_parked();
        advance(cx, 1000);
        page.read_with(cx, |page, _| {
            assert!(page.presentation.fullscreen.controls_visible)
        });
    }
    page.update(cx, |page, cx| {
        page.presentation.episode_list.open = false;
        page.schedule_fullscreen_controls_hide(cx);
    });
    cx.run_until_parked();
    advance(cx, 500);
    page.update(cx, |page, cx| page.schedule_fullscreen_controls_hide(cx));
    cx.run_until_parked();
    advance(cx, 500);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.fullscreen.controls_visible)
    });
    advance(cx, 500);
    page.read_with(cx, |page, _| {
        assert!(
            !page.presentation.fullscreen.controls_visible
                && !page.presentation.fullscreen.cursor_visible
        );
    });
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.schedule_fullscreen_controls_hide(cx);
            page.handle_back_button_hover(&true, window, cx);
        });
    });
    cx.run_until_parked();
    advance(cx, 1000);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.fullscreen.controls_visible)
    });
}

struct Cancelled(Arc<AtomicBool>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn pending(cx: &mut Context<PlaybackPage>, cancelled: Arc<AtomicBool>) -> gpui::Task<()> {
    let guard = Cancelled(cancelled);
    cx.spawn(async move |_, _| {
        let _guard = guard;
        std::future::pending::<()>().await;
    })
}

#[gpui::test]
fn stale_callbacks_keep_current_task_and_back_release_cancel_all_tasks(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let replaced = Arc::new(AtomicBool::new(false));
    let on_back: [_; 4] = std::array::from_fn(|_| Arc::new(AtomicBool::new(false)));
    page.update(cx, |page, cx| {
        let old = page
            .presentation
            .presentation_timers
            .model
            .begin(PresentationTimer::Volume)
            .unwrap();
        page.presentation.presentation_timers.tasks[PresentationTimer::Volume.index()]
            .replace(pending(cx, replaced.clone()));
        page.show_volume_indicator(cx);
        page.presentation.presentation_timers.tasks[PresentationTimer::Volume.index()].replace(
            pending(cx, on_back[PresentationTimer::Volume.index()].clone()),
        );
        page.complete_presentation_timer(PresentationTimer::Volume, &old, cx);
        assert!(page.presentation.volume_indicator_visible);
        for kind in [
            PresentationTimer::Controls,
            PresentationTimer::Rate,
            PresentationTimer::DownloadSpeed,
        ] {
            page.presentation
                .presentation_timers
                .model
                .begin(kind)
                .unwrap();
            page.presentation.presentation_timers.tasks[kind.index()]
                .replace(pending(cx, on_back[kind.index()].clone()));
        }
    });
    cx.run_until_parked();
    assert!(replaced.load(Ordering::SeqCst));
    assert!(on_back.iter().all(|flag| !flag.load(Ordering::SeqCst)));
    cx.update(|window, cx| page.update(cx, |page, cx| page.back_to_detail(window, cx)));
    cx.run_until_parked();
    assert!(on_back.iter().all(|flag| flag.load(Ordering::SeqCst)));
    let on_release: [_; 4] = std::array::from_fn(|_| Arc::new(AtomicBool::new(false)));
    page.update(cx, |page, cx| {
        for kind in PresentationTimer::ALL {
            page.presentation.presentation_timers.tasks[kind.index()]
                .replace(pending(cx, on_release[kind.index()].clone()));
        }
    });
    cx.run_until_parked();
    let weak = page.downgrade();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert!(on_release.iter().all(|flag| flag.load(Ordering::SeqCst)));
}
