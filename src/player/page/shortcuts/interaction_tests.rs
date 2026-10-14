use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::{cell::RefCell, rc::Rc};
use tiny_playback::{BackendEvent, BgraImage, SharedBgraImage};

fn install_tracks(
    page: &Entity<PlaybackPage>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<FakeState>> {
    let backend = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, _| {
        page.video = adapter(backend.clone(), false);
        let tracks = &mut page.session.source_mut().tracks;
        tracks.audio = vec![
            PlaybackTrack::new(2, "Original", false),
            PlaybackTrack::new(17, "Dubbed", false),
        ];
        tracks.subtitles = vec![
            PlaybackTrack::new(7, "Embedded", false),
            PlaybackTrack::new(19, "External", true)
                .with_external_url(Some("https://example.invalid/subtitle.ass".into()))
                .with_codec(Some("ass".into())),
        ];
        tracks.selected_audio_stream_index = Some(2);
        tracks.selected_subtitle_stream_index = Some(7);
    });
    backend
}

fn back_events(
    page: &Entity<PlaybackPage>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<Vec<PlaybackEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(page, move |_, event, _| {
            if matches!(event, PlaybackEvent::Back { .. }) {
                events.borrow_mut().push(event.clone());
            }
        })
        .detach();
    });
    events
}

#[gpui::test]
fn f8_toggles_the_episode_list_in_windowed_and_fullscreen_playback(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = install_tracks(&page, cx);
    let events = back_events(&page, cx);
    for fullscreen in [false, true] {
        if fullscreen {
            cx.simulate_keystrokes("f");
        }
        page.update(cx, |page, cx| {
            page.session.queue.queue_mut().current_index = 7;
            page.presentation.track_select_open = Some(PlaybackTrackKind::Subtitle);
            page.presentation.fullscreen.controls_visible = false;
            page.presentation.fullscreen.cursor_visible = false;
            cx.notify();
        });
        cx.simulate_keystrokes("f8");
        cx.run_until_parked();
        let panel = cx.debug_bounds("playback-episodes-panel").unwrap();
        let current = cx.debug_bounds("playback-episode-7").unwrap();
        assert!(panel.contains(&current.center()));
        assert!(cx.debug_bounds("playback-progress").is_none());
        page.read_with(cx, |page, _| {
            assert!(page.presentation.episode_list.open);
            assert!(page.presentation.track_select_open.is_none());
            assert!(page.presentation.fullscreen.controls_visible);
            assert!(page.presentation.fullscreen.cursor_visible);
        });
        cx.simulate_keystrokes("f8");
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-episodes-panel").is_none());
        assert!(cx.debug_bounds("playback-progress").is_some());
        assert_eq!(cx.update(|window, _| window.is_fullscreen()), fullscreen);
        page.read_with(cx, |page, _| {
            assert!(!page.presentation.episode_list.open);
            assert_eq!(page.session.queue.queue().current_index, 7);
            assert!(!page.session.timeline().user_paused);
            assert!(!page.playback_reporting_closed());
        });
    }
    assert!(backend.borrow().commands.is_empty());
    assert!(events.borrow().is_empty());
}

#[gpui::test]
fn f8_does_not_open_the_episode_list_while_switching_or_for_movies(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    page.update(cx, |page, cx| {
        page.session
            .queue
            .begin(QueueAction::Next, false, false, false)
            .unwrap();
        cx.notify();
    });
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
    page.read_with(cx, |page, _| {
        assert!(!page.presentation.episode_list.open);
        assert!(page.session.queue.view_model().loading);
        assert_eq!(page.session.queue.queue().current_index, 0);
    });
    page.update(cx, |page, cx| {
        page.session.queue.cancel();
        page.session.queue.queue_mut().items.truncate(1);
        page.session.queue.queue_mut().items[0].series_id = None;
        cx.notify();
    });
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
    page.read_with(cx, |page, _| {
        assert!(!page.presentation.episode_list.open);
        assert!(!page.session.queue.view_model().loading);
        assert!(!page.session.timeline().user_paused);
        assert!(!page.playback_reporting_closed());
    });
}

#[gpui::test]
fn loading_windowed_escape_closes_the_episode_list_before_returning_with_progress(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let events = back_events(&page, cx);
    page.update(cx, |page, cx| {
        page.session.timeline_mut().loaded = false;
        page.session.timeline_mut().duration = None;
        page.presentation.episode_list.open = true;
        page.session
            .queue
            .begin(QueueAction::Next, false, false, false)
            .unwrap();
        cx.notify();
    });
    cx.simulate_keystrokes("escape");
    page.read_with(cx, |page, _| {
        assert!(!page.presentation.episode_list.open);
        assert!(page.session.queue.view_model().loading);
        assert!(!page.playback_reporting_closed());
    });
    assert!(events.borrow().is_empty());
    cx.simulate_keystrokes("escape");
    assert!(!cx.update(|window, _| window.is_fullscreen()));
    page.read_with(cx, |page, _| {
        assert!(!page.session.queue.view_model().loading);
        assert!(page.playback_reporting_closed());
    });
    let events = events.borrow();
    let [PlaybackEvent::Back { update }] = events.as_slice() else {
        panic!("expected one return to media details");
    };
    assert_eq!(update.position_ticks, 450_000_000);
    assert!(!update.ended);
}

#[gpui::test]
fn fullscreen_escape_closes_the_episode_list_then_exits_fullscreen_before_returning(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let events = back_events(&page, cx);
    cx.simulate_keystrokes("f");
    page.update(cx, |page, cx| {
        page.presentation.episode_list.open = true;
        cx.notify();
    });
    cx.simulate_keystrokes("escape");
    assert!(cx.update(|window, _| window.is_fullscreen()));
    assert!(!page.read_with(cx, |page, _| page.presentation.episode_list.open));
    assert!(events.borrow().is_empty());
    cx.simulate_keystrokes("escape");
    assert!(!cx.update(|window, _| window.is_fullscreen()));
    assert!(!page.read_with(cx, |page, _| page.playback_reporting_closed()));
    assert!(events.borrow().is_empty());
    cx.simulate_keystrokes("escape");
    assert!(page.read_with(cx, |page, _| page.playback_reporting_closed()));
    assert_eq!(events.borrow().len(), 1);
}

#[gpui::test]
fn track_keys_dispatch_backend_commands_and_keep_external_subtitle_metadata(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = install_tracks(&page, cx);
    page.update(cx, |page, cx| {
        page.presentation.track_select_open = Some(PlaybackTrackKind::Subtitle);
        page.presentation.subtitle.active = Some(BackendSubtitleCue {
            text: "Old subtitle".into(),
            bitmaps: Vec::new(),
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        });
        cx.notify();
    });
    for (key, expected) in [
        ("j", Some(19)),
        ("shift-j", Some(7)),
        ("shift-j", None),
        ("shift-j", Some(19)),
        ("j", None),
        ("j", Some(7)),
    ] {
        cx.simulate_keystrokes(key);
        page.read_with(cx, |page, _| {
            assert_eq!(page.session.controls_view().subtitles.selected, expected);
            assert!(page.presentation.subtitle.active.is_none());
            assert!(page.presentation.track_select_open.is_none());
        });
    }
    let external = page.read_with(cx, |page, _| {
        page.session.controls_view().subtitles.tracks[1].clone()
    });
    assert_eq!(
        backend.borrow().commands.first(),
        Some(&BackendCommand::SetSubtitleTrack {
            track: Some(external),
            position_seconds: 45.0,
        })
    );
    for expected in [Some(17), None, Some(2)] {
        cx.simulate_keystrokes("shift-3->#");
        assert_eq!(
            page.read_with(cx, |page, _| page.session.controls_view().audio.selected),
            expected
        );
        assert_eq!(
            backend.borrow().commands.last(),
            Some(&BackendCommand::SetAudioTrack {
                track_index: expected,
                position_seconds: 45.0,
            })
        );
    }
}

#[gpui::test]
fn subtitle_visibility_keeps_track_and_images_and_restores_the_latest_cue(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = install_tracks(&page, cx);
    let bitmap = SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 128], 1, 1).unwrap());
    let mut cue = BackendSubtitleCue {
        text: "First subtitle".into(),
        bitmaps: vec![BackendSubtitleBitmap {
            image: bitmap.clone(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            canvas_width: 1920,
            canvas_height: 1080,
        }],
        start_nsecs: 0,
        end_nsecs: 1_000_000_000,
    };
    backend.borrow_mut().events.push(BackendEvent::new(
        Default::default(),
        BackendEventKind::SubtitleChanged(Some(cue.clone())),
    ));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.presentation.frame.source_size = Some(RenderSize {
                width: 1920,
                height: 1080,
            });
            page.presentation.frame.viewport_bounds = Some(Bounds::new(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(1100.0), px(800.0)),
            ));
            page.presentation.subtitle.vertical_offset_fraction = Some(0.1);
            page.poll_backend(window, cx);
            cx.notify();
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-subtitle-text-overlay").is_some());
    let image = page.read_with(cx, |page, _| {
        page.presentation
            .subtitle
            .images
            .get(&bitmap)
            .unwrap()
            .clone()
    });
    cx.simulate_keystrokes("v");
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-subtitle-text-overlay").is_none());
    assert!(
        cx.debug_bounds("playback-subtitle-bitmap-overlay")
            .is_none()
    );
    cue.text = "Latest subtitle".into();
    backend.borrow_mut().events.push(BackendEvent::new(
        Default::default(),
        BackendEventKind::SubtitleChanged(Some(cue.clone())),
    ));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.poll_backend(window, cx);
            cx.notify();
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-subtitle-text-overlay").is_none());
    cx.simulate_keystrokes("v");
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-subtitle-text-overlay").is_some());
    assert!(
        cx.debug_bounds("playback-subtitle-bitmap-overlay")
            .is_some()
    );
    page.read_with(cx, |page, _| {
        assert_eq!(page.session.controls_view().subtitles.selected, Some(7));
        assert_eq!(
            page.presentation.subtitle.active.as_ref().unwrap().text,
            cue.text
        );
        assert_eq!(
            page.presentation.subtitle.vertical_offset_fraction,
            Some(0.1)
        );
        assert!(Arc::ptr_eq(
            &image,
            page.presentation.subtitle.images.get(&bitmap).unwrap()
        ));
    });
    assert!(backend.borrow().commands.is_empty());
}
