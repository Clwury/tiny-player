use std::{cell::RefCell, rc::Rc};

use gpui::{Bounds, Entity, TestAppContext, VisualTestContext, point, px, size};
use tiny_playback::{BackendEvent, BackendEventKind, BgraImage, DemuxCacheState};

use super::*;
use crate::player::backend::test_support::{FakeState, adapter};

fn draw_frame(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    cx.run_until_parked();
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
    page.update(cx, |_, cx| cx.notify());
    draw_frame(cx);
}

#[gpui::test]
fn output_rebuffer_shows_loading_over_retained_frame_until_output_actually_resumes(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, cx| {
        page.video = adapter(state.clone(), true);
        page.presentation.frame.source_size = Some(RenderSize {
            width: 2,
            height: 1,
        });
        page.presentation.frame.viewport_bounds = Some(Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(1100.0), px(800.0)),
        ));
        page.presentation.frame.current =
            Some(render_image(BgraImage::new(vec![0; 8], 2, 1).unwrap()));
        cx.notify();
    });
    draw_frame(cx);
    assert!(cx.debug_bounds("playback-loader").is_none());
    assert_eq!(cx.active_idle_sleep_preventions(), 1);

    emit(
        &page,
        &state,
        BackendEventKind::OutputRebufferingChanged(true),
        cx,
    );
    assert!(cx.debug_bounds("playback-loader").is_some());
    assert_eq!(cx.active_idle_sleep_preventions(), 0);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.frame.current.is_some());
        assert!(!page.session.timeline().paused_for_cache);
        assert!(!page.session.timeline().user_paused);
    });

    // Refilled input and a first-frame/seek notification are independent of
    // output resume: the combined video/audio waterline may still be unmet.
    for event in [
        BackendEventKind::CacheStateChanged(PlaybackCacheState {
            demux: DemuxCacheState {
                cache_duration: Some(2.0),
                underrun: false,
                ..Default::default()
            },
            ..Default::default()
        }),
        BackendEventKind::Buffering(false),
        BackendEventKind::PlaybackRestart,
    ] {
        emit(&page, &state, event, cx);
        assert!(cx.debug_bounds("playback-loader").is_some());
    }

    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    draw_frame(cx);
    assert!(cx.debug_bounds("playback-loader").is_none());
    page.update(cx, |page, cx| page.toggle_playback_pause_command(cx));
    draw_frame(cx);
    assert!(cx.debug_bounds("playback-loader").is_some());
    assert_eq!(cx.active_idle_sleep_preventions(), 0);

    emit(
        &page,
        &state,
        BackendEventKind::OutputRebufferingChanged(false),
        cx,
    );
    assert!(cx.debug_bounds("playback-loader").is_none());
    assert_eq!(cx.active_idle_sleep_preventions(), 1);
    page.read_with(cx, |page, _| {
        assert!(page.presentation.frame.current.is_some())
    });
}

#[test]
fn cached_seek_frame_preservation_does_not_hide_a_real_output_stall() {
    let mut timeline = PlaybackTimelineState {
        loaded: true,
        user_paused: false,
        output_rebuffering: true,
        pending_seek_position: Some(120.0),
        pending_seek_keeps_frame: true,
        ..Default::default()
    };
    assert_eq!(
        playback_status(&timeline, true, false, None),
        Some(PlaybackStatus::Loading)
    );
    timeline.user_paused = true;
    assert_eq!(playback_status(&timeline, true, false, None), None);
    timeline.user_paused = false;
    timeline.output_rebuffering = false;
    assert_eq!(playback_status(&timeline, true, false, None), None);
}
