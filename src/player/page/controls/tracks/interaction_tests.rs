use super::*;
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point};

const TRACK_MENUS: [(PlaybackTrackKind, &str, &str); 2] = [
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
];
const SHORT_MENU_ROWS: [&str; 5] = [
    "playback-track-off-option",
    "playback-track-option-0",
    "playback-track-option-1",
    "playback-track-option-2",
    "playback-track-option-3",
];

fn set_track_choices(
    page: &Entity<PlaybackPage>,
    cx: &mut VisualTestContext,
    kind: PlaybackTrackKind,
    count: usize,
    selected: Option<usize>,
) {
    page.update(cx, |page, cx| {
        let tracks = (0..count)
            .map(|index| PlaybackTrack::new(17 + 14 * index, format!("Track {index}"), false))
            .collect();
        let state = &mut page.session.source_mut().tracks;
        match kind {
            PlaybackTrackKind::Audio => {
                state.audio = tracks;
                state.selected_audio_stream_index = selected;
            }
            PlaybackTrackKind::Subtitle => {
                state.subtitles = tracks;
                state.selected_subtitle_stream_index = selected;
            }
        }
        cx.notify();
    });
    cx.run_until_parked();
}

fn scroll_menu(cx: &mut VisualTestContext, position: Point<Pixels>, lines: f32) {
    cx.simulate_mouse_move(position, None, Modifiers::default());
    cx.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0.0, lines)),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
}

fn assert_option_visible(
    page: &Entity<PlaybackPage>,
    cx: &mut VisualTestContext,
    id: &'static str,
) {
    let viewport = page.read_with(cx, |page, _| {
        page.presentation.track_select_scroll_handle.bounds()
    });
    let row = cx.debug_bounds(id).unwrap();
    assert!(row.top() >= viewport.top(), "{id}: {row:?} in {viewport:?}");
    assert!(
        row.bottom() <= viewport.bottom(),
        "{id}: {row:?} in {viewport:?}"
    );
    assert!(row.left() >= viewport.left());
    assert!(row.right() <= viewport.right());
}

#[gpui::test]
fn track_menus_fit_up_to_five_options_without_scrolling(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    for (kind, trigger_id, menu_id) in TRACK_MENUS {
        for count in 0_usize..=4 {
            // A stale selection keeps an Off-only menu available as well.
            let selected = Some(if count == 0 {
                999
            } else {
                17 + 14 * (count - 1)
            });
            set_track_choices(&page, cx, kind, count, selected);
            let trigger = cx.debug_bounds(trigger_id).unwrap().center();
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
            let menu = cx.debug_bounds(menu_id).unwrap();
            for id in SHORT_MENU_ROWS.iter().take(count + 1) {
                assert_option_visible(&page, cx, id);
            }
            let off = cx.debug_bounds("playback-track-off-option").unwrap();
            let volume = page.read_with(cx, |page, _| page.session.controls_view().volume.level);
            scroll_menu(cx, menu.center(), -8.0);
            assert_eq!(cx.debug_bounds("playback-track-off-option").unwrap(), off);
            page.read_with(cx, |page, _| {
                assert_eq!(page.presentation.track_select_open, Some(kind));
                assert_eq!(page.session.controls_view().volume.level, volume);
                assert_eq!(
                    page.presentation.track_select_scroll_handle.offset().y,
                    px(0.0)
                );
                assert_eq!(
                    page.presentation.track_select_scroll_handle.max_offset().y,
                    px(0.0)
                );
            });
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
        }
    }
}

#[gpui::test]
fn track_menus_reveal_selected_track_when_opened(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    for (kind, trigger_id, menu_id) in TRACK_MENUS {
        for (selected, id) in [
            (Some(129), "playback-track-option-8"),
            (Some(73), "playback-track-option-4"),
            (Some(59), "playback-track-option-3"),
            (Some(17), "playback-track-option-0"),
            (None, "playback-track-off-option"),
            (Some(999), "playback-track-off-option"),
        ] {
            set_track_choices(&page, cx, kind, 9, selected);
            let trigger = cx.debug_bounds(trigger_id).unwrap().center();
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
            assert_option_visible(&page, cx, id);
            let scroll = page.read_with(cx, |page, _| {
                page.presentation.track_select_scroll_handle.clone()
            });
            let initial_offset = scroll.offset();
            assert!(scroll.max_offset().y > px(0.0));
            let menu = cx.debug_bounds(menu_id).unwrap();
            let lines = if initial_offset.y == px(0.0) {
                -24.0
            } else {
                24.0
            };
            scroll_menu(cx, menu.center(), lines);
            let manual_offset = scroll.offset();
            assert_ne!(
                manual_offset, initial_offset,
                "{kind:?}, {selected:?}, {id}"
            );
            page.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
            assert_eq!(scroll.offset(), manual_offset);

            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
            assert_option_visible(&page, cx, id);
            cx.simulate_click(trigger, Modifiers::default());
            cx.run_until_parked();
        }
    }
}

#[gpui::test]
fn track_menus_reset_scroll_when_switching_kind_or_shortening_options(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    set_track_choices(&page, cx, PlaybackTrackKind::Audio, 9, Some(129));
    set_track_choices(&page, cx, PlaybackTrackKind::Subtitle, 9, Some(31));
    let audio = cx.debug_bounds("playback-audio-button").unwrap().center();
    let subtitles = cx.debug_bounds("playback-caption-button").unwrap().center();
    cx.simulate_click(audio, Modifiers::default());
    cx.run_until_parked();
    assert_option_visible(&page, cx, "playback-track-option-8");

    cx.simulate_click(subtitles, Modifiers::default());
    cx.run_until_parked();
    assert_option_visible(&page, cx, "playback-track-off-option");
    assert_option_visible(&page, cx, "playback-track-option-1");
    let menu = cx.debug_bounds("playback-caption-menu").unwrap();
    scroll_menu(cx, menu.center(), -24.0);
    assert_option_visible(&page, cx, "playback-track-option-8");

    cx.simulate_click(audio, Modifiers::default());
    cx.run_until_parked();
    assert_option_visible(&page, cx, "playback-track-option-8");
    cx.simulate_click(audio, Modifiers::default());
    cx.run_until_parked();
    set_track_choices(&page, cx, PlaybackTrackKind::Audio, 4, Some(59));
    cx.simulate_click(audio, Modifiers::default());
    cx.run_until_parked();
    for id in SHORT_MENU_ROWS {
        assert_option_visible(&page, cx, id);
    }
    page.read_with(cx, |page, _| {
        let scroll = &page.presentation.track_select_scroll_handle;
        assert_eq!(scroll.offset().y, px(0.0));
        assert_eq!(scroll.max_offset().y, px(0.0));
    });
}
