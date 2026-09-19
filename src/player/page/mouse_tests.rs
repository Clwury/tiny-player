use gpui::{
    DispatchEventResult, Modifiers, PlatformInput, TestAppContext, VisualTestContext, point,
};

use super::*;

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

fn playback_with_titlebar(
    cx: &mut TestAppContext,
) -> (gpui::Entity<PlaybackPage>, &mut VisualTestContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    cx.update(|window, cx| window.replace_root(cx, |_, _| PlaybackWithTitlebar(page.clone())));
    cx.run_until_parked();
    (page, cx)
}

fn dispatch_mouse_press(
    cx: &mut VisualTestContext,
    position: Point<Pixels>,
    button: MouseButton,
    click_count: usize,
) -> DispatchEventResult {
    cx.simulate_mouse_move(position, None, Modifiers::default());
    cx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                button,
                click_count,
                modifiers: Modifiers::default(),
                first_mouse: false,
            }),
            cx,
        )
    })
}

fn dispatch_left_release(
    cx: &mut VisualTestContext,
    position: Point<Pixels>,
) -> DispatchEventResult {
    cx.update(|window, cx| {
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Modifiers::default(),
            }),
            cx,
        )
    })
}

#[cfg(target_os = "windows")]
#[gpui::test]
fn playback_controls_do_not_consume_native_caption_clicks(cx: &mut TestAppContext) {
    let (page, cx) = playback_with_titlebar(cx);
    for controls_visible in [false, true, false, true] {
        page.update(cx, |page, cx| {
            page.fullscreen.controls_visible = controls_visible;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("playback-progress").is_some(),
            controls_visible
        );
        for selector in [
            "window-control-minimize",
            "window-control-maximize",
            "window-control-close",
        ] {
            let position = cx.debug_bounds(selector).unwrap().center();
            // GPUI runs native caption actions only when both events propagate.
            // TestPlatform doesn't execute the actual Windows system commands.
            assert!(
                dispatch_mouse_press(cx, position, MouseButton::Left, 1).propagate,
                "{selector} press"
            );
            assert!(
                dispatch_left_release(cx, position).propagate,
                "{selector} release with controls visible: {controls_visible}"
            );
            page.read_with(cx, |page, _| {
                assert!(page.timeline.progress_drag_position.is_none());
                assert!(!page.timeline.user_paused);
            });
        }
    }
}

#[gpui::test]
fn progress_drag_consumes_its_own_release_outside_the_track_only(cx: &mut TestAppContext) {
    let (page, cx) = playback_with_titlebar(cx);
    cx.update(|window, cx| window.simulate_next_frame(cx));
    let track = cx.debug_bounds("playback-progress-track").unwrap();
    let caption = cx.debug_bounds("window-control-maximize").unwrap().center();
    let video = point(px(1060.0), px(400.0));
    for end in [caption, video] {
        assert!(!dispatch_mouse_press(cx, track.center(), MouseButton::Left, 1).propagate);
        assert!(page.read_with(cx, |page, _| page.timeline.progress_drag_position.is_some()));
        // Releasing a seek over a caption button must finish the seek without
        // accidentally activating the button; subsequent releases must pass.
        assert!(!dispatch_left_release(cx, end).propagate);
        assert!(page.read_with(cx, |page, _| page.timeline.progress_drag_position.is_none()));
        assert!(dispatch_left_release(cx, end).propagate);
    }
}

#[gpui::test]
fn surface_drag_press_reaches_windows_but_menu_dismissal_does_not(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    cx.simulate_keystrokes("i");
    let file = cx.debug_bounds("playback-stats-File").unwrap();
    let video = point(px(1060.0), px(400.0));

    for origin in [video, file.origin + point(px(12.0), px(10.0))] {
        let result = dispatch_mouse_press(cx, origin, MouseButton::Left, 1);
        // Windows enters its native move loop only if the press is unhandled.
        // Linux consumes the press and starts a compositor move on motion.
        assert_eq!(result.propagate, cfg!(target_os = "windows"));
        page.read_with(cx, |page, _| {
            assert_eq!(page.window_drag, WindowDragState::Pending);
            assert!(!page.timeline.user_paused);
            assert!(page.timeline.progress_drag_position.is_none());
        });
        assert!(!cx.update(|window, _| window.is_fullscreen()));
        cx.simulate_mouse_up(origin, MouseButton::Left, Modifiers::default());

        page.update(cx, |page, cx| {
            page.tracks.open = Some(PlaybackTrackKind::Audio);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!dispatch_mouse_press(cx, origin, MouseButton::Left, 1).propagate);
        page.read_with(cx, |page, _| {
            assert!(page.tracks.open.is_none());
            assert_ne!(page.window_drag, WindowDragState::Pending);
        });
        cx.simulate_mouse_up(origin, MouseButton::Left, Modifiers::default());
    }
}

#[gpui::test]
fn surface_double_click_keeps_fullscreen_and_blocks_native_maximize(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let video = point(px(1060.0), px(400.0));
    for was_fullscreen in [false, true] {
        assert_eq!(
            cx.update(|window, _| window.is_fullscreen()),
            was_fullscreen
        );
        let first_press = dispatch_mouse_press(cx, video, MouseButton::Left, 1);
        assert_eq!(
            first_press.propagate,
            cfg!(target_os = "windows") && !was_fullscreen
        );
        if was_fullscreen {
            assert_eq!(
                page.read_with(cx, |page, _| page.window_drag),
                WindowDragState::Idle
            );
        }
        cx.simulate_mouse_up(video, MouseButton::Left, Modifiers::default());

        assert!(!dispatch_mouse_press(cx, video, MouseButton::Left, 2).propagate);
        assert_eq!(
            cx.update(|window, _| window.is_fullscreen()),
            !was_fullscreen
        );
        assert_eq!(
            page.read_with(cx, |page, _| page.window_drag),
            WindowDragState::Idle
        );
        cx.simulate_mouse_up(video, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
    }
}

#[cfg(target_os = "windows")]
#[gpui::test]
fn native_surface_suppresses_system_menu_and_preserves_volume_scroll(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    cx.simulate_keystrokes("i");
    let file = cx.debug_bounds("playback-stats-File").unwrap();
    let video = point(px(1060.0), px(400.0));
    for origin in [video, file.origin + point(px(12.0), px(10.0))] {
        assert!(!dispatch_mouse_press(cx, origin, MouseButton::Right, 1).propagate);
        let release = cx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    position: origin,
                    button: MouseButton::Right,
                    click_count: 1,
                    modifiers: Modifiers::default(),
                }),
                cx,
            )
        });
        assert!(
            !release.propagate,
            "right release must not open the Windows system menu"
        );
    }

    let initial_volume = page.read_with(cx, |page, _| page.volume.level);
    cx.simulate_event(ScrollWheelEvent {
        position: video,
        delta: ScrollDelta::Lines(point(0.0, -3.0)),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    let actual_volume = page.read_with(cx, |page, _| page.volume.level);
    assert!((actual_volume - (initial_volume - PLAYBACK_VOLUME_STEP)).abs() < f32::EPSILON);
}

#[gpui::test]
fn control_panel_background_closes_track_menus_without_playback_side_effects(
    cx: &mut TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    page.update(cx, |page, cx| {
        page.tracks.selected_audio_stream_index = Some(0);
        page.tracks.selected_subtitle_stream_index = Some(1);
        cx.notify();
    });
    cx.run_until_parked();
    let panel = cx.debug_bounds("playback-progress").unwrap();
    let track = cx.debug_bounds("playback-progress-track").unwrap();
    let positions = [
        point(panel.left() + px(4.0), panel.center().y),
        point(track.left() - px(24.0), track.center().y),
        point(track.right() + px(24.0), track.center().y),
        cx.debug_bounds("playback-previous-button")
            .unwrap()
            .center(),
    ];
    let volume = page.read_with(cx, |page, _| page.volume.level);
    let position = page.read_with(cx, |page, _| page.timeline.position);
    let user_paused = page.read_with(cx, |page, _| page.timeline.user_paused);

    for fullscreen in [false, true] {
        if fullscreen {
            cx.update(|window, _| window.toggle_fullscreen());
        }
        for (kind, trigger) in [
            (PlaybackTrackKind::Audio, "playback-audio-button"),
            (PlaybackTrackKind::Subtitle, "playback-caption-button"),
        ] {
            for point in positions {
                for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                    let trigger = cx.debug_bounds(trigger).unwrap().center();
                    cx.simulate_click(trigger, Modifiers::default());
                    cx.run_until_parked();
                    assert_eq!(page.read_with(cx, |page, _| page.tracks.open), Some(kind));
                    cx.simulate_mouse_move(point, None, Modifiers::default());
                    cx.simulate_event(ScrollWheelEvent {
                        position: point,
                        delta: ScrollDelta::Lines(gpui::point(0.0, -3.0)),
                        modifiers: Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    });
                    assert_eq!(page.read_with(cx, |page, _| page.tracks.open), Some(kind));
                    cx.simulate_mouse_down(point, button, Modifiers::default());
                    page.read_with(cx, |page, _| {
                        assert!(
                            page.tracks.open.is_none(),
                            "{kind:?}: {button:?} at {point:?}"
                        );
                        assert_ne!(page.window_drag, WindowDragState::Pending);
                    });
                    cx.simulate_mouse_up(point, button, Modifiers::default());
                    cx.run_until_parked();
                    page.read_with(cx, |page, _| {
                        assert_eq!(page.volume.level, volume);
                        assert_eq!(page.timeline.position, position);
                        assert_eq!(page.timeline.user_paused, user_paused);
                        assert!(page.timeline.progress_drag_position.is_none());
                    });
                    assert_eq!(cx.update(|window, _| window.is_fullscreen()), fullscreen);
                }
            }
        }
    }
}

#[gpui::test]
fn track_menus_keep_inside_clicks_and_buttons_can_toggle_switch_and_select(
    cx: &mut TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    page.update(cx, |page, cx| {
        page.tracks.selected_audio_stream_index = Some(0);
        page.tracks.selected_subtitle_stream_index = Some(1);
        cx.notify();
    });
    cx.run_until_parked();
    for (kind, trigger, menu) in [
        (
            PlaybackTrackKind::Audio,
            "playback-audio-button",
            "playback-audio-menu",
        ),
        (
            PlaybackTrackKind::Subtitle,
            "playback-caption-button",
            "playback-caption-menu",
        ),
    ] {
        let trigger = cx.debug_bounds(trigger).unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        let menu = cx.debug_bounds(menu).unwrap();
        let padding = menu.origin + point(px(2.0), px(2.0));
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            cx.simulate_mouse_move(padding, None, Modifiers::default());
            cx.simulate_mouse_down(padding, button, Modifiers::default());
            cx.simulate_mouse_up(padding, button, Modifiers::default());
            cx.run_until_parked();
            assert_eq!(page.read_with(cx, |page, _| page.tracks.open), Some(kind));
        }
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        assert!(page.read_with(cx, |page, _| page.tracks.open.is_none()));
    }
    for (kind, trigger) in [
        (PlaybackTrackKind::Audio, "playback-audio-button"),
        (PlaybackTrackKind::Subtitle, "playback-caption-button"),
        (PlaybackTrackKind::Audio, "playback-audio-button"),
    ] {
        let trigger = cx.debug_bounds(trigger).unwrap().center();
        cx.simulate_click(trigger, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |page, _| page.tracks.open), Some(kind));
    }
    let off = cx
        .debug_bounds("playback-track-off-option")
        .unwrap()
        .center();
    cx.simulate_click(off, Modifiers::default());
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.tracks.open.is_none()));
    assert!(cx.debug_bounds("playback-audio-menu").is_none());
    assert!(cx.debug_bounds("playback-caption-menu").is_none());
}

#[gpui::test]
fn track_menus_show_metadata_below_labels_and_scroll_to_select_last_track(cx: &mut TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    page.update(cx, |page, cx| {
        let streams = (0..8)
            .flat_map(|index| {
                [
                    serde_json::json!({
                        "Index": index, "Type": "Audio", "Language": "jpn",
                        "DisplayTitle": "Japanese AAC stereo (original soundtrack)",
                        "Title": "日语原声音轨 · A long track title that must fit inside the menu"
                    }),
                    serde_json::json!({
                        "Index": index + 10, "Type": "Subtitle", "Language": "chi",
                        "DisplayTitle": "Chinese Simplified (ASS)",
                        "Title": "简体中文 · A long subtitle title that must fit inside the menu"
                    }),
                ]
            })
            .collect::<Vec<_>>();
        let source = serde_json::from_value(serde_json::json!({"MediaStreams": streams})).unwrap();
        page.tracks.audio = playback_audio_tracks_for_source(&source);
        page.tracks.subtitles = playback_subtitle_tracks_for_source(
            &source,
            &page.emby.server,
            &page.emby.item_id,
            &page.emby.media_source_id,
        );
        page.tracks.selected_audio_stream_index = Some(0);
        page.tracks.selected_subtitle_stream_index = Some(10);
        cx.notify();
    });
    cx.run_until_parked();

    for theme in crate::theme::ColorTheme::ALL {
        cx.update(|_, cx| crate::theme::set(theme, cx));
        for (trigger, menu_id) in [
            ("playback-audio-button", "playback-audio-menu"),
            ("playback-caption-button", "playback-caption-menu"),
        ] {
            let trigger = cx.debug_bounds(trigger).unwrap().center();
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
            let menu = cx.debug_bounds(menu_id).unwrap();
            assert!(menu.left() >= px(0.0));
            assert!(menu.top() >= px(0.0));
            assert!(menu.size.width <= px(280.0));
            assert!(menu.size.height <= px(260.0));
            for (id, label_id, metadata_id) in [
                (
                    "playback-track-off-option",
                    "playback-track-off-option-label",
                    "playback-track-off-option-metadata",
                ),
                (
                    "playback-track-option-0",
                    "playback-track-option-0-label",
                    "playback-track-option-0-metadata",
                ),
            ] {
                let row = cx.debug_bounds(id).unwrap();
                let label = cx.debug_bounds(label_id).unwrap();
                let metadata = cx.debug_bounds(metadata_id).unwrap();
                assert!(label.bottom() <= metadata.top());
                assert!(metadata.size.height < label.size.height);
                assert!(label.top() >= row.top());
                assert!(metadata.bottom() <= row.bottom());
                assert!(label.right() <= row.right());
                assert!(metadata.right() <= row.right());
                assert!(row.top() >= menu.top());
                assert!(row.bottom() <= menu.bottom());
            }

            cx.simulate_mouse_move(menu.center(), None, Modifiers::default());
            cx.simulate_event(ScrollWheelEvent {
                position: menu.center(),
                delta: ScrollDelta::Lines(point(0.0, -24.0)),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            let last = cx.debug_bounds("playback-track-option-7").unwrap();
            let metadata = cx.debug_bounds("playback-track-option-7-metadata").unwrap();
            assert!(last.top() >= menu.top());
            assert!(last.bottom() <= menu.bottom());
            assert!(metadata.right() <= menu.right());
            cx.simulate_click(metadata.center(), Modifiers::default());
            cx.run_until_parked();
            assert!(cx.debug_bounds(menu_id).is_none());
            assert!(page.read_with(cx, |page, _| page.tracks.open.is_none()));
        }
    }
}

#[gpui::test]
fn control_panel_presses_do_not_become_window_drags_outside_the_panel(cx: &mut TestAppContext) {
    let (page, cx) = playback_with_titlebar(cx);
    cx.simulate_keystrokes("i");
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
        assert!(!dispatch_mouse_press(cx, start, MouseButton::Left, 1).propagate);
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
