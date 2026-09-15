use gpui::{Modifiers, TestAppContext, point};

use super::*;

#[gpui::test]
fn control_panel_background_ignores_clicks_and_scrolls(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    page.update(cx, |page, cx| {
        page.tracks.selected_audio_stream_index = Some(0);
        cx.notify();
    });
    cx.run_until_parked();
    let panel = cx.debug_bounds("playback-progress").unwrap();
    let track = cx.debug_bounds("playback-progress-track").unwrap();
    let audio = cx.debug_bounds("playback-audio-button").unwrap().center();
    let positions = [
        point(panel.left() + px(4.0), panel.center().y),
        point(track.left() - px(24.0), track.center().y),
        point(track.right() + px(24.0), track.center().y),
        cx.debug_bounds("playback-previous-button")
            .unwrap()
            .center(),
    ];
    cx.simulate_click(audio, Modifiers::default());
    assert_eq!(
        page.read_with(cx, |page, _| page.tracks.open),
        Some(PlaybackTrackKind::Audio)
    );
    let volume = page.read_with(cx, |page, _| page.volume.level);
    let position = page.read_with(cx, |page, _| page.timeline.position);

    for point in positions {
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            cx.simulate_mouse_move(point, None, Modifiers::default());
            cx.simulate_mouse_down(point, button, Modifiers::default());
            cx.simulate_mouse_up(point, button, Modifiers::default());
        }
        cx.simulate_event(ScrollWheelEvent {
            position: point,
            delta: ScrollDelta::Lines(gpui::point(0.0, -3.0)),
            modifiers: Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        page.read_with(cx, |page, _| {
            assert_eq!(page.tracks.open, Some(PlaybackTrackKind::Audio));
            assert_eq!(page.volume.level, volume);
            assert_eq!(page.timeline.position, position);
            assert!(page.timeline.progress_drag_position.is_none());
        });
        assert!(!cx.update(|window, _| window.is_fullscreen()));
    }

    cx.simulate_click(audio, Modifiers::default());
    assert!(page.read_with(cx, |page, _| page.tracks.open.is_none()));
}

#[gpui::test]
fn control_panel_presses_do_not_become_window_drags_outside_the_panel(cx: &mut TestAppContext) {
    struct PlaybackWithTitlebar(gpui::Entity<PlaybackPage>);

    impl Render for PlaybackWithTitlebar {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(crate::ui::titlebar::app_titlebar(
                    window,
                    cx,
                    "Playback".into(),
                ))
                .child(div().flex_1().min_h_0().child(self.0.clone()))
        }
    }

    let (page, cx) = episodes::tests::playback_window(cx);
    cx.simulate_keystrokes("i");
    cx.update(|window, cx| window.replace_root(cx, |_, _| PlaybackWithTitlebar(page.clone())));
    cx.run_until_parked();
    let panel = cx.debug_bounds("playback-progress").unwrap();
    let track = cx.debug_bounds("playback-progress-track").unwrap();
    let file = cx.debug_bounds("playback-stats-File").unwrap();
    let play = cx.debug_bounds("playback-play-pause-button").unwrap();
    let disabled = cx.debug_bounds("playback-previous-button").unwrap();
    let starts = [
        point(panel.left() + px(4.0), panel.center().y),
        point(track.left() - px(24.0), track.center().y),
        play.center(),
        disabled.center(),
    ];
    let stats = file.origin + point(px(12.0), px(10.0));
    let video = point(px(1060.0), px(400.0));

    for start in starts {
        cx.simulate_mouse_move(start, None, Modifiers::default());
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        // The test platform panics on a native window move, so these crossings
        // exercise the actual playback surface rather than just a bounds helper.
        for position in [stats, video, point(px(550.0), px(10.0)), start, stats] {
            cx.simulate_mouse_move(position, Some(MouseButton::Left), Modifiers::default());
        }
        cx.simulate_mouse_up(stats, MouseButton::Left, Modifiers::default());
        page.read_with(cx, |page, _| {
            assert!(page.playback_details_visible);
            assert!(page.timeline.progress_drag_position.is_none());
        });
    }
}

#[gpui::test]
fn window_drag_origin_resets_on_release_or_a_new_control_press(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    cx.simulate_keystrokes("i");
    let file = cx.debug_bounds("playback-stats-File").unwrap();
    let play = cx
        .debug_bounds("playback-play-pause-button")
        .unwrap()
        .center();
    let video = point(px(1060.0), px(400.0));

    for origin in [video, file.origin + point(px(12.0), px(10.0))] {
        cx.simulate_mouse_move(origin, None, Modifiers::default());
        cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::default());
        assert_eq!(
            page.read_with(cx, |page, _| page.window_drag),
            WindowDragState::Pending
        );

        // A release over the occluding controls must still clear the origin.
        // Skip motion here because native moves are unavailable in tests.
        cx.simulate_mouse_up(play, MouseButton::Left, Modifiers::default());
        assert_eq!(
            page.read_with(cx, |page, _| page.window_drag),
            WindowDragState::Idle
        );
        cx.simulate_mouse_move(video, Some(MouseButton::Left), Modifiers::default());

        cx.simulate_mouse_down(origin, MouseButton::Left, Modifiers::default());
        assert_eq!(
            page.read_with(cx, |page, _| page.window_drag),
            WindowDragState::Pending
        );
        // Model a release consumed outside the window: a new press must discard
        // the previous origin even when a button handles that press itself.
        cx.simulate_mouse_down(play, MouseButton::Left, Modifiers::default());
        assert_eq!(
            page.read_with(cx, |page, _| page.window_drag),
            WindowDragState::Blocked
        );
        cx.simulate_mouse_move(origin, Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(origin, MouseButton::Left, Modifiers::default());
    }

    cx.update(|window, _| window.toggle_fullscreen());
    cx.simulate_mouse_down(video, MouseButton::Left, Modifiers::default());
    assert_eq!(
        page.read_with(cx, |page, _| page.window_drag),
        WindowDragState::Idle
    );
    cx.simulate_mouse_move(
        video + point(px(10.0), px(10.0)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(video, MouseButton::Left, Modifiers::default());
}
