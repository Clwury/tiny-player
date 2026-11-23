use super::*;
use crate::player::backend::test_support::{FakeState, adapter};
use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use std::{cell::RefCell, rc::Rc};

fn install_tracks(
    page: &Entity<PlaybackPage>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<FakeState>> {
    let backend = Rc::new(RefCell::new(FakeState::default()));
    page.update(cx, |page, cx| {
        page.video = adapter(backend.clone(), false);
        let tracks = &mut page.session.source_mut().tracks;
        tracks.audio = vec![
            PlaybackTrack::new(1, "Japanese (AAC)", false),
            PlaybackTrack::new(8, "English (AAC)", false),
        ];
        tracks.subtitles = vec![
            PlaybackTrack::new(7, "Chinese Simplified (ASS)", false),
            PlaybackTrack::new(19, "English (SRT)", true)
                .with_external_url(Some("https://example.invalid/subtitle.srt".into())),
        ];
        tracks.selected_audio_stream_index = Some(1);
        tracks.selected_subtitle_stream_index = Some(7);
        cx.notify();
    });
    cx.run_until_parked();
    backend
}

fn label(page: &Entity<PlaybackPage>, cx: &VisualTestContext) -> Option<String> {
    page.read_with(cx, |page, _| match &page.presentation.feedback {
        Some(PlaybackFeedback::TrackSwitch(label)) => Some(label.to_string()),
        _ => None,
    })
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let position = cx.debug_bounds(selector).unwrap().center();
    cx.simulate_click(position, Modifiers::default());
    cx.run_until_parked();
}

fn advance(cx: &mut VisualTestContext, milliseconds: u64) {
    cx.executor()
        .advance_clock(Duration::from_millis(milliseconds));
    cx.run_until_parked();
}

#[gpui::test]
fn shortcuts_and_menus_show_track_names_and_off_beside_back(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    install_tracks(&page, cx);
    assert!(label(&page, cx).is_none());
    cx.simulate_keystrokes("j");
    assert_eq!(
        label(&page, cx).as_deref(),
        Some("字幕：English (SRT) 外挂")
    );
    let indicator = cx.debug_bounds("playback-track-switch-indicator").unwrap();
    assert_eq!(
        indicator.left(),
        px(PLAYBACK_BACK_BUTTON_OFFSET_PX + PLAYBACK_BACK_BUTTON_SIZE_PX + 12.0)
    );
    assert_eq!(indicator.top(), px(PLAYBACK_BACK_BUTTON_OFFSET_PX));
    assert_eq!(indicator.size.height, px(PLAYBACK_BACK_BUTTON_SIZE_PX));
    assert!(indicator.size.width > px(0.0) && indicator.size.height > px(0.0));
    cx.simulate_keystrokes("shift-j");
    assert_eq!(
        label(&page, cx).as_deref(),
        Some("字幕：Chinese Simplified (ASS)")
    );
    click(cx, "playback-audio-button");
    click(cx, "playback-track-option-1");
    assert_eq!(label(&page, cx).as_deref(), Some("音轨：English (AAC)"));
    cx.simulate_keystrokes("#");
    assert_eq!(label(&page, cx).as_deref(), Some("音轨：关闭"));
    click(cx, "playback-caption-button");
    click(cx, "playback-track-off-option");
    assert_eq!(label(&page, cx).as_deref(), Some("字幕：关闭"));
}

#[gpui::test]
fn latest_track_change_restarts_its_timer_without_extending_volume(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    install_tracks(&page, cx);
    page.update(cx, |page, cx| {
        page.presentation.fullscreen.controls_visible = false;
        page.show_volume_indicator(cx);
        page.change_playback_rate(tiny_playback::PlaybackRateChange::Double, cx);
    });
    cx.simulate_keystrokes("#");
    assert!(cx.debug_bounds("playback-rate-indicator").is_none());
    advance(cx, 1000);
    cx.simulate_keystrokes("j");
    advance(cx, 1000);
    assert_eq!(
        label(&page, cx).as_deref(),
        Some("字幕：English (SRT) 外挂")
    );
    assert!(cx.debug_bounds("playback-track-switch-indicator").is_some());
    page.read_with(cx, |page, _| {
        assert!(!page.presentation.fullscreen.controls_visible);
        assert!(!page.presentation.volume_indicator_visible);
    });
    advance(cx, 999);
    assert!(label(&page, cx).is_some());
    advance(cx, 1);
    assert!(label(&page, cx).is_none());
    assert!(cx.debug_bounds("playback-track-switch-indicator").is_none());
}

#[gpui::test]
fn subtitle_scale_replaces_track_feedback_and_refreshes_at_the_limit(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    install_tracks(&page, cx);
    let scale_indicator = "playback-subtitle-scale-indicator";
    assert!(cx.debug_bounds(scale_indicator).is_none());

    cx.simulate_keystrokes("j");
    let track_indicator = "playback-track-switch-indicator";
    let track_bounds = cx.debug_bounds(track_indicator).unwrap();
    advance(cx, 1000);
    cx.simulate_keystrokes("f shift-g");
    assert!(cx.update(|window, _| window.is_fullscreen()));
    assert!(cx.debug_bounds(track_indicator).is_none());
    let scale_bounds = cx.debug_bounds(scale_indicator).unwrap();
    assert_eq!(scale_bounds.origin, track_bounds.origin);
    assert_eq!(scale_bounds.size.height, track_bounds.size.height);
    page.read_with(cx, |page, _| {
        assert!(matches!(
            &page.presentation.feedback,
            Some(PlaybackFeedback::SubtitleScale(label)) if label.as_ref() == "字幕大小：1.10×"
        ));
    });

    advance(cx, 1000);
    assert!(cx.debug_bounds(scale_indicator).is_some());
    cx.update(|_, cx| crate::settings::AppLanguage::English.apply(cx));
    page.update(cx, |page, _| page.subtitle_scale = 3.0);
    cx.simulate_keystrokes("shift-g");
    page.read_with(cx, |page, _| {
        assert!(matches!(
            &page.presentation.feedback,
            Some(PlaybackFeedback::SubtitleScale(label)) if label.as_ref() == "Subtitle size: 3.00×"
        ));
    });
    advance(cx, 1999);
    assert!(cx.debug_bounds(scale_indicator).is_some());
    advance(cx, 1);
    assert!(cx.debug_bounds(scale_indicator).is_none());
    assert!(cx.debug_bounds(track_indicator).is_none());
}

#[gpui::test]
fn rejected_and_empty_track_changes_do_not_show_or_refresh_the_indicator(cx: &mut TestAppContext) {
    let (page, cx) = test_support::playback_window(cx);
    let backend = install_tracks(&page, cx);
    backend.borrow_mut().fail_commands = true;
    cx.simulate_keystrokes("j");
    assert!(label(&page, cx).is_none());
    backend.borrow_mut().fail_commands = false;
    cx.simulate_keystrokes("#");
    assert_eq!(label(&page, cx).as_deref(), Some("音轨：English (AAC)"));
    advance(cx, 1000);
    backend.borrow_mut().fail_commands = true;
    cx.simulate_keystrokes("j");
    assert_eq!(label(&page, cx).as_deref(), Some("音轨：English (AAC)"));
    advance(cx, 1000);
    assert!(label(&page, cx).is_none());
    page.update(cx, |page, _| {
        let tracks = &mut page.session.source_mut().tracks;
        tracks.audio.clear();
        tracks.subtitles.clear();
        tracks.selected_audio_stream_index = None;
        tracks.selected_subtitle_stream_index = None;
    });
    backend.borrow_mut().commands.clear();
    cx.simulate_keystrokes("j #");
    assert!(label(&page, cx).is_none());
    assert!(backend.borrow().commands.is_empty());
}
