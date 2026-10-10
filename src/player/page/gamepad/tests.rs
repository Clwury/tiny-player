use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

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
        page.presentation.playback_details_visible = true;
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
