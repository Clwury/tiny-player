use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use gpui::{point, size};
use std::{cell::RefCell, rc::Rc};
use tiny_playback::{BackendEvent, BgraImage, PlaybackRateChange};

#[gpui::test]
fn command_failures_preserve_pause_seek_volume_and_rate(cx: &mut gpui::TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState {
        fail_commands: true,
        ..Default::default()
    }));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.video = adapter(state.clone(), false);
            page.toggle_playback_pause_command(cx);
            assert!(!page.session.timeline().user_paused && !page.session.timeline().paused);
            assert!(
                page.session
                    .controls_view()
                    .error
                    .as_ref()
                    .unwrap()
                    .starts_with("控制播放失败：")
            );
            page.session.clear_error();
            page.session.timeline_mut().buffered_until = Some(90.0);
            page.seek_to_position(120.125, window, cx);
            assert_eq!(page.session.timeline().position, Some(45.0));
            assert_eq!(page.session.timeline().buffered_until, Some(90.0));
            assert!(page.session.timeline().pending_seek_position.is_none());
            assert!(!page.session.timeline().buffering);
            assert!(
                page.session
                    .controls_view()
                    .error
                    .as_ref()
                    .unwrap()
                    .starts_with("跳转播放位置失败：")
            );
            let volume = page.session.controls_view().volume;
            page.toggle_playback_mute(cx);
            assert_eq!(page.session.controls_view().volume, volume);
            assert!(
                page.session
                    .controls_view()
                    .error
                    .as_ref()
                    .unwrap()
                    .starts_with("调整音量失败：")
            );
            page.change_playback_rate(PlaybackRateChange::Double, cx);
            assert_eq!(page.session.controls_view().rate, 1.0);
            assert!(
                page.session
                    .controls_view()
                    .error
                    .as_ref()
                    .unwrap()
                    .starts_with("调整播放速度失败：")
            );
        })
    });
    assert_eq!(state.borrow().commands.len(), 4);
}

#[gpui::test]
fn successful_commands_keep_cache_pause_and_precise_seek_and_emit_saved_volume(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState::default()));
    let volumes = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let volumes = volumes.clone();
        cx.subscribe(&page, move |_, event: &PlaybackEvent, _| {
            if let PlaybackEvent::VolumeChanged { settings } = event {
                volumes.borrow_mut().push(*settings);
            }
        })
        .detach();
    });
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.video = adapter(state.clone(), false);
            page.session.timeline_mut().user_paused = true;
            page.session.timeline_mut().paused_for_cache = true;
            page.session.timeline_mut().paused = true;
            page.toggle_playback_pause_command(cx);
            assert!(!page.session.timeline().user_paused && page.session.timeline().paused);
            page.seek_to_position(120.1234567, window, cx);
            assert_eq!(page.session.timeline().position, Some(120.1234567));
            assert_eq!(
                page.session.timeline().pending_seek_position,
                Some(120.1234567)
            );
            page.toggle_playback_mute(cx);
            assert_eq!(page.session.controls_view().volume.level, 0.0);
            page.change_playback_rate(PlaybackRateChange::Double, cx);
            assert_eq!(page.session.controls_view().rate, 2.0);
        })
    });
    cx.run_until_parked();
    assert_eq!(volumes.borrow().len(), 1);
    assert_eq!(volumes.borrow()[0].level, 0.0);
    assert_eq!(
        state.borrow().commands,
        [
            BackendCommand::Resume,
            BackendCommand::Seek {
                position_seconds: 120.1234567,
                mode: PlaybackSeekMode::Precise
            },
            BackendCommand::SetVolume { volume: 0.0 },
            BackendCommand::SetPlaybackRate { rate: 2.0 },
        ]
    );
}

#[gpui::test]
fn render_failure_clears_frame_closes_reporting_and_cancels_queue_before_release(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState {
        fail_render: true,
        ..Default::default()
    }));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.video = adapter(state.clone(), true);
            page.presentation.frame.source_size = Some(RenderSize {
                width: 2,
                height: 1,
            });
            page.presentation.frame.viewport_bounds = Some(Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(640.0), px(360.0)),
            ));
            page.presentation.frame.current =
                Some(render_image(BgraImage::new(vec![0; 8], 2, 1).unwrap()));
            page.session
                .queue
                .begin(crate::player::queue::QueueAction::Next, false, false, false)
                .unwrap();
            page.poll_backend(window, cx);
            assert!(page.session.timeline().user_paused && page.session.timeline().paused);
            assert!(page.presentation.frame.current.is_none());
            assert!(!page.session.queue.view_model().loading);
            assert!(page.playback_reporting_closed());
            assert_eq!(
                page.session.controls_view().error,
                Some("渲染视频失败：synthetic render failure")
            );
        })
    });
    let weak = page.downgrade();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    let state = state.borrow();
    assert!(
        state
            .operations
            .ends_with(&["drop_presenter", "drop_backend"])
    );
    assert_eq!(
        state
            .operations
            .iter()
            .filter(|operation| **operation == "drop_backend")
            .count(),
        1
    );
}

#[gpui::test]
fn paused_poll_reads_driver_events_at_interval_and_stops_after_cache_is_idle(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let state = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, cx| {
        page.video = adapter(state.clone(), false);
        page.session.timeline_mut().paused = true;
        page.session.timeline_mut().user_paused = true;
        page.session.timeline_mut().cache_state = Some(PlaybackCacheState {
            paused_for_cache: true,
            ..Default::default()
        });
        cx.notify();
    });
    cx.run_until_parked();
    let polls = state.borrow().operations.len();
    state.borrow_mut().events = vec![
        BackendEvent::new(
            Default::default(),
            BackendEventKind::PositionChanged(46.125),
        ),
        BackendEvent::new(
            Default::default(),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: tiny_playback::DemuxCacheState {
                    idle: true,
                    ..Default::default()
                },
                ..Default::default()
            }),
        ),
    ];
    cx.executor().advance_clock(Duration::from_millis(249));
    cx.run_until_parked();
    assert_eq!(state.borrow().operations.len(), polls);
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(page.session.timeline().position, Some(46.125))
    });
    let polls = state.borrow().operations.len();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(state.borrow().operations.len(), polls);
}
