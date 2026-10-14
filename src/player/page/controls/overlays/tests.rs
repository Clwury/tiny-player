use super::*;
use crate::{i18n::AppLanguage, player::model::shortcuts::PlaybackShortcut};
use gpui::{Modifiers, TestAppContext, VisualTestContext, point, size};
use tiny_playback::ByteCacheState;

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).unwrap();
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn overlay_icons_follow_cache_speed_and_remain_available_without_a_duration(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    page.update(cx, |page, cx| {
        page.session.source_mut().source_protocol = Some("https".into());
        page.session.timeline_mut().cache_state = Some(PlaybackCacheState {
            byte: Some(ByteCacheState {
                raw_input_rate: Some(1024),
                ..Default::default()
            }),
            ..Default::default()
        });
        cx.notify();
    });
    for selection in theme::ColorTheme::ALL {
        cx.update(|_, cx| theme::set(selection, cx));
        for (width, height) in [(640.0, 360.0), (1280.0, 720.0)] {
            cx.simulate_resize(size(px(width), px(height)));
            cx.run_until_parked();
            let speed = cx.debug_bounds("playback-download-speed").unwrap();
            let keyboard = cx.debug_bounds("playback-keyboard-button").unwrap();
            let info = cx.debug_bounds("playback-info-button").unwrap();
            assert!(speed.right() < keyboard.left());
            assert!(keyboard.right() < info.left());
            assert_eq!(speed.top(), keyboard.top());
            assert_eq!(keyboard.top(), info.top());
            assert_eq!(info.top(), px(16.0));
            assert_eq!(info.right(), px(width - 16.0));
        }
    }
    page.update(cx, |page, cx| {
        page.session.timeline_mut().duration = None;
        page.session.timeline_mut().loaded = false;
        page.session.source_mut().source_protocol = None;
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-download-speed").is_none());
    assert!(cx.debug_bounds("playback-keyboard-button").is_some());
    assert!(cx.debug_bounds("playback-info-button").is_some());
    click(cx, "playback-keyboard-button");
    assert!(cx.debug_bounds("playback-shortcuts-overlay").is_some());
}

#[gpui::test]
fn mouse_and_keyboard_share_mutually_exclusive_overlays_at_the_same_position(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    for fullscreen in [false, true] {
        cx.update(|window, _| {
            if window.is_fullscreen() != fullscreen {
                window.toggle_fullscreen();
                window.refresh();
            }
        });
        cx.run_until_parked();
        click(cx, "playback-info-button");
        let info = cx.debug_bounds("playback-details-overlay").unwrap();
        assert!(cx.debug_bounds("playback-shortcuts-overlay").is_none());
        click(cx, "playback-keyboard-button");
        let shortcuts = cx.debug_bounds("playback-shortcuts-overlay").unwrap();
        assert_eq!(shortcuts.origin, info.origin);
        assert_eq!(shortcuts.size.width, info.size.width);
        assert!(cx.debug_bounds("playback-details-overlay").is_none());
        cx.simulate_keystrokes("i");
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-details-overlay").is_some());
        assert!(cx.debug_bounds("playback-shortcuts-overlay").is_none());
        click(cx, "playback-info-button");
        assert!(cx.debug_bounds("playback-details-overlay").is_none());
        click(cx, "playback-keyboard-button");
        click(cx, "playback-keyboard-button");
        assert!(cx.debug_bounds("playback-shortcuts-overlay").is_none());
        page.read_with(cx, |page, _| {
            assert!(page.presentation.active_overlay.is_none());
            assert!(!page.session.timeline().user_paused);
            assert!(!page.playback_reporting_closed());
        });
    }
}

#[gpui::test]
fn shortcut_help_translations_wrap_and_scroll_without_adjusting_volume(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    click(cx, "playback-keyboard-button");
    for language in AppLanguage::ALL {
        cx.update(|_, cx| language.apply(cx));
        for width in [640.0, 1280.0] {
            cx.simulate_resize(size(px(width), px(480.0)));
            cx.run_until_parked();
            let overlay = cx.debug_bounds("playback-shortcuts-overlay").unwrap();
            assert!(overlay.right() <= px(width));
            assert!(overlay.bottom() <= px(480.0));
            for (_, description) in shortcuts::SHORTCUT_HELP
                .iter()
                .chain(shortcuts::GAMEPAD_HELP)
            {
                let translated = crate::i18n::translate(language, description);
                if language == AppLanguage::English {
                    assert_ne!(translated, *description, "{language:?}: {description}");
                }
            }
            for selector in [
                "playback-shortcut-key-Space",
                "playback-shortcut-key-P",
                "playback-shortcut-key-F8",
                "playback-shortcut-key-Shift-J",
                "playback-shortcut-key-Backspace",
            ] {
                if let Some(tag) = cx.debug_bounds(selector) {
                    assert!(tag.left() >= overlay.left());
                    assert!(tag.right() <= overlay.right());
                }
            }
            if let Some(space) = cx.debug_bounds("playback-shortcut-key-Space") {
                let separator = cx
                    .debug_bounds("playback-shortcut-separator-Space-1")
                    .unwrap();
                let p = cx.debug_bounds("playback-shortcut-key-P").unwrap();
                assert!(space.right() < separator.left());
                assert!(separator.right() < p.left());
            }
            let volume = page.read_with(cx, |page, _| page.session.controls_view().volume.level);
            cx.simulate_event(ScrollWheelEvent {
                position: overlay.center(),
                delta: ScrollDelta::Lines(point(0.0, -100.0)),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            });
            cx.run_until_parked();
            assert_eq!(
                page.read_with(cx, |page, _| page.session.controls_view().volume.level),
                volume
            );
            let last = cx.debug_bounds("playback-gamepad-key-R3-↓").unwrap();
            assert!(overlay.contains(&last.center()));
            for selector in [
                "playback-gamepad-key-L3",
                "playback-gamepad-key-R3-↑",
                "playback-gamepad-key-X",
            ] {
                let tag = cx.debug_bounds(selector).unwrap();
                assert!(overlay.contains(&tag.center()));
                assert!(tag.right() <= overlay.right());
            }
        }
    }
}

#[gpui::test]
fn hovered_toolbar_keeps_fullscreen_controls_visible_and_gamepad_back_dismisses_help(
    cx: &mut TestAppContext,
) {
    let (page, cx) = test_support::playback_window(cx);
    cx.simulate_keystrokes("f");
    cx.simulate_mouse_move(point(px(500.0), px(650.0)), None, Modifiers::default());
    cx.run_until_parked();
    click(cx, "playback-keyboard-button");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-keyboard-button").is_some());
    page.read_with(cx, |page, _| {
        assert!(page.presentation.fullscreen.cursor_visible);
        assert_eq!(
            page.presentation.active_overlay,
            Some(PlaybackOverlay::Shortcuts)
        );
    });
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            page.handle_gamepad_shortcut(PlaybackShortcut::Back, window, cx)
        })
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-shortcuts-overlay").is_none());
    page.read_with(cx, |page, _| {
        assert!(page.presentation.active_overlay.is_none());
        assert!(!page.playback_reporting_closed());
    });
}
