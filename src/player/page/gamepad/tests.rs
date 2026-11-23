use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use tiny_playback::{BgraImage, SharedBgraImage};

fn dispatch(
    page: &gpui::Entity<PlaybackPage>,
    cx: &mut gpui::VisualTestContext,
    shortcut: PlaybackShortcut,
) {
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.handle_gamepad_shortcut(shortcut, window, cx)
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
fn keyboard_and_gamepad_share_backend_commands_and_presentation(cx: &mut gpui::TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, _| page.video = adapter(backend.clone(), false));
    dispatch(&page, cx, PlaybackShortcut::TogglePlayback);
    assert!(page.read_with(cx, |page, _| page.session.timeline().user_paused));
    cx.simulate_keystrokes("space");
    assert!(!page.read_with(cx, |page, _| page.session.timeline().user_paused));
    assert!(matches!(
        backend.borrow().commands.as_slice(),
        [BackendCommand::Pause, BackendCommand::Resume]
    ));
    dispatch(&page, cx, PlaybackShortcut::SeekRelative(5));
    assert_eq!(
        page.read_with(cx, |page, _| page.session.timeline().position),
        Some(50.0)
    );
    cx.simulate_keystrokes("left");
    assert_eq!(
        page.read_with(cx, |page, _| page.session.timeline().position),
        Some(45.0)
    );
    let initial_volume = page.read_with(cx, |page, _| page.session.controls_view().volume.level);
    dispatch(&page, cx, PlaybackShortcut::DecreaseVolume);
    cx.simulate_keystrokes("0");
    assert!(
        (page.read_with(cx, |page, _| page.session.controls_view().volume.level) - initial_volume)
            .abs()
            < f32::EPSILON
    );
    assert!(page.read_with(cx, |page, _| page.presentation.volume_indicator_visible));
}

#[gpui::test]
fn keyboard_and_gamepad_resize_visible_subtitles_within_the_supported_range(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, cx| {
        page.video = adapter(backend.clone(), false);
        page.subtitle_scale = 1.2;
        page.presentation.frame.source_size = Some(RenderSize {
            width: 1920,
            height: 1080,
        });
        page.presentation.frame.viewport_bounds = Some(Bounds::new(
            gpui::point(px(0.0), px(0.0)),
            gpui::size(px(1100.0), px(800.0)),
        ));
        let cue = BackendSubtitleCue {
            text: "Subtitle".into(),
            bitmaps: vec![BackendSubtitleBitmap {
                image: SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 128], 1, 1).unwrap()),
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
        page.presentation.subtitle.images.update(Some(&cue));
        page.presentation.subtitle.active = Some(cue);
        cx.notify();
    });
    cx.run_until_parked();
    let dimensions = |cx: &mut gpui::VisualTestContext| {
        let text = cx.debug_bounds("playback-subtitle-text").unwrap();
        let bitmap = cx.debug_bounds("playback-subtitle-bitmap-overlay").unwrap();
        (text.size.height, bitmap.size.width)
    };
    let (initial_text_height, initial_bitmap_width) = dimensions(cx);

    cx.simulate_keystrokes("shift-g");
    cx.run_until_parked();
    let (larger_text_height, larger_bitmap_width) = dimensions(cx);
    assert!(larger_text_height > initial_text_height);
    assert!(larger_bitmap_width > initial_bitmap_width);
    assert!((page.read_with(cx, |page, _| page.subtitle_scale) - 1.3).abs() <= f32::EPSILON);

    dispatch(&page, cx, PlaybackShortcut::DecreaseSubtitleScale);
    let (smaller_text_height, smaller_bitmap_width) = dimensions(cx);
    assert!(smaller_text_height < larger_text_height);
    assert!(smaller_bitmap_width < larger_bitmap_width);
    assert!((page.read_with(cx, |page, _| page.subtitle_scale) - 1.2).abs() <= f32::EPSILON);
    page.read_with(cx, |page, _| {
        assert!(matches!(
            &page.presentation.feedback,
            Some(PlaybackFeedback::SubtitleScale(label)) if label.as_ref() == "字幕大小：1.20×"
        ));
    });

    page.update(cx, |page, _| page.subtitle_scale = 2.95);
    dispatch(&page, cx, PlaybackShortcut::IncreaseSubtitleScale);
    assert_eq!(page.read_with(cx, |page, _| page.subtitle_scale), 3.0);
    page.update(cx, |page, _| page.subtitle_scale = 0.55);
    cx.simulate_keystrokes("shift-f");
    assert_eq!(page.read_with(cx, |page, _| page.subtitle_scale), 0.5);
    assert!(!cx.update(|window, _| window.is_fullscreen()));
    assert!(backend.borrow().commands.is_empty());
}

#[gpui::test]
fn back_dismisses_overlays_before_returning_and_stops_retained_page_input(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    let back_events = Rc::new(Cell::new(0));
    let count = back_events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&page, move |_, event, _| {
            if matches!(event, PlaybackEvent::Back { .. }) {
                count.set(count.get() + 1);
            }
        })
        .detach();
    });
    page.update(cx, |page, _| {
        page.presentation.episode_list.open = true;
        page.presentation.track_select_open = Some(PlaybackTrackKind::Audio);
        page.presentation.active_overlay = Some(PlaybackOverlay::Info);
    });
    for _ in 0..3 {
        dispatch(&page, cx, PlaybackShortcut::Back);
        assert_eq!(back_events.get(), 0);
    }
    dispatch(&page, cx, PlaybackShortcut::Back);
    assert_eq!(back_events.get(), 1);
    page.read_with(cx, |page, _| {
        assert!(page.playback_reporting_closed());
        assert!(page.gamepad.started);
        assert!(page.gamepad.input.is_none());
    });
}

#[gpui::test]
fn open_menus_consume_gamepad_transport_shortcuts(cx: &mut gpui::TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, _| {
        page.video = adapter(backend.clone(), false);
        page.presentation.track_select_open = Some(PlaybackTrackKind::Subtitle);
    });
    for shortcut in [
        PlaybackShortcut::SeekRelative(5),
        PlaybackShortcut::IncreaseVolume,
        PlaybackShortcut::TogglePlayback,
    ] {
        dispatch(&page, cx, shortcut);
    }
    assert!(backend.borrow().commands.is_empty());
    assert!(page.read_with(cx, |page, _| page.presentation.gamepad_help_first));
    dispatch(&page, cx, PlaybackShortcut::Back);
    dispatch(&page, cx, PlaybackShortcut::SeekRelative(5));
    assert_eq!(
        page.read_with(cx, |page, _| page.session.timeline().position),
        Some(50.0)
    );
}

#[gpui::test]
fn gamepad_feedback_extends_controls_visibility_and_view_button_can_hide_it(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    page.update(cx, |page, _| {
        page.presentation.fullscreen.controls_visible = false
    });
    dispatch(&page, cx, PlaybackShortcut::ToggleInfoOverlay);
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.presentation.fullscreen.controls_visible));
    dispatch(&page, cx, PlaybackShortcut::ToggleMute);
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.presentation.fullscreen.controls_visible));
    dispatch(&page, cx, PlaybackShortcut::ToggleControls);
    assert!(!page.read_with(cx, |page, _| page.presentation.fullscreen.controls_visible));
    dispatch(&page, cx, PlaybackShortcut::ToggleControls);
    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();
    assert!(!page.read_with(cx, |page, _| page.presentation.fullscreen.controls_visible));
}

#[gpui::test]
fn delivery_rejects_actions_queued_before_focus_loss_and_back(cx: &mut gpui::TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let (input, sender, receiver) = GamepadInput::test_channel();
    let backend = Rc::new(RefCell::new(FakeState::default()));
    cx.update(|window, cx| {
        window.activate_window();
        page.update(cx, |page, cx| {
            page.video = adapter(backend.clone(), false);
            page.attach_gamepad_input(input, receiver, window, cx);
        });
    });
    cx.run_until_parked();
    let message = page.read_with(cx, |page, _| {
        page.gamepad
            .input
            .as_ref()
            .unwrap()
            .message_for_test(PlaybackShortcut::TogglePlayback)
    });
    cx.deactivate_window();
    assert!(!cx.update(|window, _| window.is_window_active()));
    // The native worker can finish sending an event captured before blur.
    sender.try_send(message).unwrap();
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    assert!(backend.borrow().commands.is_empty());
    assert!(!page.read_with(cx, |page, _| page.presentation.gamepad_help_first));
    let message = page.read_with(cx, |page, _| {
        page.gamepad
            .input
            .as_ref()
            .unwrap()
            .message_for_test(PlaybackShortcut::TogglePlayback)
    });
    sender.try_send(message).unwrap();
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.session.timeline().user_paused));
    assert!(page.read_with(cx, |page, _| page.presentation.gamepad_help_first));
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            let message = page
                .gamepad
                .input
                .as_ref()
                .unwrap()
                .message_for_test(PlaybackShortcut::SeekRelative(5));
            sender.try_send(message).unwrap();
            page.back_to_detail(window, cx);
        })
    });
    cx.run_until_parked();
    assert_eq!(
        page.read_with(cx, |page, _| page.session.timeline().position),
        Some(45.0)
    );
    assert!(sender.is_closed());
}

#[gpui::test]
fn modifier_input_moves_gamepad_help_above_keyboard_and_keeps_that_order_when_reopened(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    cx.simulate_resize(gpui::size(px(1280.0), px(1100.0)));
    cx.run_until_parked();
    let button = cx
        .debug_bounds("playback-keyboard-button")
        .unwrap()
        .center();
    cx.simulate_click(button, gpui::Modifiers::default());
    cx.run_until_parked();
    let keyboard = cx.debug_bounds("playback-shortcut-section").unwrap();
    let gamepad = cx.debug_bounds("playback-gamepad-section").unwrap();
    assert!(keyboard.bottom() < gamepad.top());

    let backend = Rc::new(RefCell::new(FakeState::default()));
    let (input, sender, receiver) = GamepadInput::test_channel();
    cx.update(|window, cx| {
        window.activate_window();
        page.update(cx, |page, cx| {
            page.video = adapter(backend.clone(), false);
            page.attach_gamepad_input(input, receiver, window, cx);
        });
    });
    cx.run_until_parked();
    let message = page.read_with(cx, |page, _| {
        page.gamepad.input.as_ref().unwrap().message_for_test(None)
    });
    sender.try_send(message).unwrap();
    cx.run_until_parked();
    let keyboard = cx.debug_bounds("playback-shortcut-section").unwrap();
    let gamepad = cx.debug_bounds("playback-gamepad-section").unwrap();
    assert!(gamepad.bottom() < keyboard.top());
    assert!(backend.borrow().commands.is_empty());
    page.read_with(cx, |page, _| {
        assert!(page.presentation.gamepad_help_first);
        assert!(!page.session.timeline().user_paused);
    });

    cx.simulate_keystrokes("i");
    cx.run_until_parked();
    cx.simulate_click(button, gpui::Modifiers::default());
    cx.run_until_parked();
    let keyboard = cx.debug_bounds("playback-shortcut-section").unwrap();
    let gamepad = cx.debug_bounds("playback-gamepad-section").unwrap();
    assert!(gamepad.bottom() < keyboard.top());
}

#[gpui::test]
fn closing_window_stops_input_even_if_page_is_retained(cx: &mut gpui::TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let (input, sender, receiver) = GamepadInput::test_channel();
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.attach_gamepad_input(input, receiver, window, cx)
        })
    });
    cx.run_until_parked();
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    page.read_with(cx, |page, _| assert!(page.gamepad.input.is_none()));
    assert!(sender.is_closed());
}
