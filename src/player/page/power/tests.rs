use std::{cell::RefCell, rc::Rc};

use gpui::{Entity, TestAppContext, VisualTestContext};
use tiny_playback::{BackendEvent, BackendEventKind};

use super::*;
use crate::player::backend::test_support::{FakeState, adapter};

fn video_window(
    cx: &mut TestAppContext,
) -> (
    Entity<PlaybackPage>,
    Rc<RefCell<FakeState>>,
    &mut VisualTestContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState::default()));
    cx.update(|_, cx| {
        page.update(cx, |page, _| {
            page.video = adapter(state.clone(), false);
            page.presentation.frame.source_size = Some(RenderSize {
                width: 1920,
                height: 1080,
            });
            page.session.timeline_mut().loaded = false;
            page.session.queue = crate::player::queue::QueueController::new(
                PlaybackQueue::new(Vec::new(), 0),
                page.emby.server.workspace_identity(),
            );
        });
    });
    (page, state, cx)
}

fn emit(
    page: &Entity<PlaybackPage>,
    state: &Rc<RefCell<FakeState>>,
    event: BackendEventKind,
    cx: &mut VisualTestContext,
) {
    state
        .borrow_mut()
        .events
        .push(BackendEvent::new(Default::default(), event));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| page.poll_backend(window, cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
fn video_playback_holds_one_request_and_releases_on_user_and_cache_pause(cx: &mut TestAppContext) {
    let (page, state, cx) = video_window(cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    for _ in 0..3 {
        emit(&page, &state, BackendEventKind::PositionChanged(50.0), cx);
    }
    assert_eq!(cx.active_idle_sleep_preventions(), 1);

    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 1);

    emit(
        &page,
        &state,
        BackendEventKind::PausedForCacheChanged(true),
        cx,
    );
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    emit(
        &page,
        &state,
        BackendEventKind::PausedForCacheChanged(false),
        cx,
    );
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
}

#[gpui::test]
fn terminal_backend_events_release_power_and_late_restart_cannot_reacquire(
    cx: &mut TestAppContext,
) {
    for event in [
        BackendEventKind::PlaybackEnded,
        BackendEventKind::LoadFailed("offline".into()),
        BackendEventKind::Fatal("decoder failed".into()),
    ] {
        let (page, state, cx) = video_window(cx);
        emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
        assert_eq!(cx.active_idle_sleep_preventions(), 1);
        emit(&page, &state, event, cx);
        assert_eq!(cx.active_idle_sleep_preventions(), 0);
        emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
        assert_eq!(cx.active_idle_sleep_preventions(), 0);
        cx.update(|window, _| window.remove_window());
    }
}

#[gpui::test]
fn back_releases_power_even_when_the_page_entity_is_retained(cx: &mut TestAppContext) {
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    cx.update(|window, cx| {
        page.update(cx, |page, cx| page.back_to_detail(window, cx));
    });
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.playback_reporting_closed()));
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
}

#[gpui::test]
fn closing_the_window_releases_power(cx: &mut TestAppContext) {
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    let weak = page.downgrade();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
}

#[gpui::test]
fn delayed_acquisition_is_cancelled_before_pause_resume_and_release(cx: &mut TestAppContext) {
    cx.set_idle_sleep_prevention_delay(Duration::from_secs(1));
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 0);

    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 1);

    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.run_until_parked();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
}

#[gpui::test]
fn rejected_request_preserves_playback_and_retries_only_after_a_new_playing_interval(
    cx: &mut TestAppContext,
) {
    cx.set_idle_sleep_prevention_fails(true);
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    cx.set_idle_sleep_prevention_fails(false);
    emit(&page, &state, BackendEventKind::PositionChanged(50.0), cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    page.read_with(cx, |page, _| {
        assert!(page.session.timeline().loaded);
        assert!(!page.session.timeline().paused);
        assert!(page.session.controls_view().error.is_none());
    });
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
}

#[gpui::test]
fn failed_pause_command_keeps_the_active_power_request(cx: &mut TestAppContext) {
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    state.borrow_mut().fail_pause = true;
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
}

#[gpui::test]
fn removing_video_and_render_failure_release_power(cx: &mut TestAppContext) {
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    emit(&page, &state, BackendEventKind::VideoSizeChanged(None), cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    emit(
        &page,
        &state,
        BackendEventKind::VideoSizeChanged(Some(RenderSize {
            width: 1920,
            height: 1080,
        })),
        cx,
    );
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    state.borrow_mut().fail_render = true;
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.video = adapter(state.clone(), true);
            page.presentation.frame.viewport_bounds = Some(Bounds::new(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(640.0), px(360.0)),
            ));
            page.poll_backend(window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    assert!(page.read_with(cx, |page, _| page.playback_reporting_closed()));
}

struct OfflineGateway;

impl crate::media::gateway::PlaybackSourceGateway for OfflineGateway {
    fn resolve_source(
        &self,
        _: &str,
        _: &str,
    ) -> anyhow::Result<crate::media::gateway::ResolvedPlayback> {
        anyhow::bail!("offline");
    }

    fn subtitle_tracks(
        &self,
        _: &crate::emby::MediaSource,
        _: &str,
        _: &str,
    ) -> Vec<PlaybackTrack> {
        Vec::new()
    }
}

#[gpui::test]
fn episode_switch_releases_power_and_failed_resolution_reacquires_on_resume(
    cx: &mut TestAppContext,
) {
    let (page, state, cx) = video_window(cx);
    emit(&page, &state, BackendEventKind::PlaybackRestart, cx);
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.session.queue = crate::player::queue::QueueController::new(
                PlaybackQueue::new((0..2).map(test_support::episode).collect(), 0),
                page.emby.server.workspace_identity(),
            );
            page.queue_effects = queue::QueueEffects::new(Arc::new(OfflineGateway));
            page.switch_to_episode(1, window, cx);
        });
    });
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    cx.run_until_parked();
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    assert_eq!(
        state.borrow().commands,
        [BackendCommand::Pause, BackendCommand::Resume]
    );
}
