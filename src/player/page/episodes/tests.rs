use std::{
    cell::RefCell,
    io::{Read, Write},
    net::TcpListener,
    rc::Rc,
    thread,
};

use gpui::{Entity, Modifiers, TestAppContext, TouchPhase, VisualTestContext, point, size};
use serde_json::json;

use super::*;

fn episode(index: usize) -> PlaybackQueueItem {
    PlaybackQueueItem {
        item_id: format!("episode-{index}"),
        title: format!("Series S1E{}", index + 1).into(),
        episode_label: format!("E{}: Episode title", index + 1).into(),
        overview: Some(
            "An episode overview that is long enough to wrap across multiple lines in the list."
                .into(),
        ),
        primary_image_tag: None,
        series_id: Some("series".into()),
        season_id: Some("season".into()),
        premiere_date: Some("1998-04-03T00:00:00.0000000Z".into()),
        run_time_ticks: Some(18_000_000_000),
        playback_position_ticks: Some(100_000_000),
        media_sources: vec![
            serde_json::from_value(json!({
                "Id": format!("source-{index}"), "Size": 1_320_702_444_u64
            }))
            .unwrap(),
        ],
    }
}

#[test]
fn episode_metadata_formats_dates_durations_and_sizes_and_omits_missing_fields() {
    for (date, ticks, size, expected) in [
        (
            Some("1998-04-03T00:00:00.0000000Z"),
            Some(14_550_000_000),
            Some(1_320_702_444),
            Some("1998-04-03 24:15 1.23 GiB"),
        ),
        (Some("1998-04-03"), None, None, Some("1998-04-03")),
        (
            None,
            Some(36_610_000_000),
            Some(1_048_576),
            Some("1:01:01 1.00 MiB"),
        ),
        (Some("invalid"), Some(14_550_000_000), None, Some("24:15")),
        (Some("invalid"), Some(0), Some(0), None),
        (None, None, None, None),
    ] {
        let mut item = episode(0);
        item.premiere_date = date.map(str::to_string);
        item.run_time_ticks = ticks;
        assert_eq!(episode_metadata_label(&item, size).as_deref(), expected);
    }
}

pub(in crate::player::page) fn playback_window(
    cx: &mut TestAppContext,
) -> (Entity<PlaybackPage>, &mut VisualTestContext) {
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let emby = EmbyPlaybackContext {
            client: crate::emby::EmbyClient::new("episode-list-test".into()).unwrap(),
            server: serde_json::from_value(json!({
                "id": "episode-list-test",
                "endpoint": {"protocol": "Http", "address": "", "port": 80, "path": "/emby"},
                "username": "test", "password": "", "user_id": "user",
                "access_token": "test-token", "added_at_unix": 0
            }))
            .unwrap(),
            item_id: "episode-0".into(),
            media_source_id: "source-0".into(),
            play_session_id: None,
            run_time_ticks: Some(18_000_000_000),
        };
        // Exercise the real page and event flow without starting FFmpeg or Vulkan.
        PlaybackPage::register_image_cleanup(cx);
        PlaybackPage {
            focus_handle: cx.focus_handle(),
            title: "Series S1E1".into(),
            video: ShutdownOrder::new(None, None),
            frame: PlaybackFrameState::default(),
            timeline: PlaybackTimelineState {
                loaded: true,
                duration: Some(1800.0),
                position: Some(45.0),
                user_paused: false,
                paused: false,
                ..Default::default()
            },
            download_speed: controls::DownloadSpeedDisplay::default(),
            playback_details_visible: false,
            fullscreen: FullscreenControlsState {
                controls_visible: true,
                cursor_visible: true,
                ..Default::default()
            },
            window_drag: WindowDragState::default(),
            source_protocol: None,
            source_url: "episode.mkv".to_string(),
            content_length: None,
            playback_file_info: None,
            playback_info: None,
            playback_audio_info: None,
            queue: PlaybackQueue::new((0..12).map(episode).collect(), 0),
            episode_list: PlaybackEpisodeListState::default(),
            reporting: session::PlaybackReportingState::new(&emby),
            emby,
            queue_switch: queue::PlaybackQueueSwitchState::default(),
            track_preference_key: crate::player::PlaybackTrackPreferenceKey {
                item_id: "episode-0".into(),
                media_source_id: "source-0".into(),
            },
            tracks: TrackSelectState::new(
                Vec::new(),
                Vec::new(),
                PlaybackTrackSelection::default(),
            ),
            remember_subtitle_on_start: false,
            subtitle: SubtitleOverlayState::default(),
            volume: PlaybackVolumeState::new(PlaybackVolumeSettings::default()),
            rate: rate::PlaybackRateState::default(),
            error_message: None,
        }
    });
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    (page, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_up(bounds.center(), MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

fn scrollbar_thumb(cx: &mut VisualTestContext) -> Option<Bounds<Pixels>> {
    cx.update(|window, cx| {
        let scale = window.scale_factor();
        let theme = theme::get(cx);
        let thumb = window.painted_quads().into_iter().find(|quad| {
            (quad.background == theme.scrollbar_thumb.into()
                || quad.background == theme.scrollbar_thumb_hover.into())
                && quad.bounds.size.width
                    == px(crate::ui::scrollbar::SCROLLBAR_WIDTH_PX).scale(scale)
        })?;
        assert_eq!(
            thumb.bounds.intersect(&thumb.content_mask.bounds),
            thumb.bounds
        );
        Some(Bounds::new(
            point(
                px(thumb.bounds.left().0 / scale),
                px(thumb.bounds.top().0 / scale),
            ),
            size(
                px(thumb.bounds.size.width.0 / scale),
                px(thumb.bounds.size.height.0 / scale),
            ),
        ))
    })
}

#[gpui::test]
fn episode_sizes_use_the_playing_version_and_the_queued_default_source(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    page.update(cx, |page, _| {
        let gib = 1_u64 << 30;
        page.queue.items[0].media_sources = serde_json::from_value(json!([
            {"Id": "default-source", "Type": "Default", "Size": gib},
            {"Id": "source-0", "Size": 2 * gib}
        ]))
        .unwrap();
        page.queue.items[1].media_sources = serde_json::from_value(json!([
            {"Id": "alternate-source", "Size": gib},
            {"Id": "source-1", "Type": "Default", "Size": 4 * gib}
        ]))
        .unwrap();

        page.content_length = Some(3 * gib);
        assert_eq!(page.episode_file_size(0), Some(3 * gib));
        assert_eq!(page.episode_file_size(1), Some(4 * gib));

        page.content_length = None;
        page.emby.media_source_id = "resolved-source".into();
        assert_eq!(page.episode_file_size(0), Some(2 * gib));
        page.emby.media_source_id = "default-source".into();
        assert_eq!(page.episode_file_size(0), Some(gib));

        page.content_length = Some(0);
        page.emby.media_source_id = "missing".into();
        page.track_preference_key.media_source_id = "missing".into();
        assert_eq!(page.episode_file_size(0), None);
        page.queue.items[1].media_sources[1].size = None;
        assert_eq!(page.episode_file_size(1), None);
    });
}

#[gpui::test]
fn episode_card_metadata_fits_between_the_title_and_overview(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    for theme in crate::theme::ColorTheme::ALL {
        cx.update(|_, cx| crate::theme::set(theme, cx));
        click(cx, "playback-episodes-button");
        for (card_id, title_id, metadata_id, overview_id) in [
            (
                "playback-episode-0",
                "playback-episode-label-0",
                "playback-episode-metadata-0",
                "playback-episode-overview-0",
            ),
            (
                "playback-episode-1",
                "playback-episode-label-1",
                "playback-episode-metadata-1",
                "playback-episode-overview-1",
            ),
        ] {
            let card = cx.debug_bounds(card_id).unwrap();
            let title = cx.debug_bounds(title_id).unwrap();
            let metadata = cx.debug_bounds(metadata_id).unwrap();
            let overview = cx.debug_bounds(overview_id).unwrap();
            assert!(title.bottom() <= metadata.top());
            assert!(metadata.bottom() <= overview.top());
            assert!(metadata.size.height < title.size.height);
            assert!(metadata.left() >= title.left());
            assert!(metadata.right() <= card.right());
            assert!(title.top() >= card.top());
            assert!(overview.bottom() <= card.bottom());
        }
        click(cx, "playback-episodes-close");
    }

    page.update(cx, |page, cx| {
        let item = &mut page.queue.items[1];
        item.premiere_date = None;
        item.run_time_ticks = None;
        item.media_sources.clear();
        cx.notify();
    });
    click(cx, "playback-episodes-button");
    assert!(cx.debug_bounds("playback-episode-metadata-1").is_none());
    click(cx, "playback-episode-metadata-0");
    page.read_with(cx, |page, _| {
        assert!(!page.episode_list.open);
        assert!(!page.queue_switch.loading);
        assert_eq!(page.queue.current_index, 0);
    });
}

#[gpui::test]
fn episode_drawer_hides_controls_and_restores_them_when_dismissed(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    for width in [900.0, 1100.0, 1920.0] {
        cx.simulate_resize(size(px(width), px(800.0)));
        cx.run_until_parked();
        let episodes = cx.debug_bounds("playback-episodes-button").unwrap();
        let audio = cx.debug_bounds("playback-audio-button").unwrap();
        let next = cx.debug_bounds("playback-next-button").unwrap();
        assert!(next.right() < episodes.left());
        assert!(episodes.right() < audio.left());

        click(cx, "playback-episodes-button");
        let panel = cx.debug_bounds("playback-episodes-panel").unwrap();
        assert_eq!(panel.right(), px(width));
        assert_eq!(panel.top(), px(0.0));
        assert_eq!(panel.bottom(), px(800.0));
        cx.update(|window, cx| {
            let theme = theme::media_overlay(cx);
            let panel_fill = window
                .painted_quads()
                .into_iter()
                .find(|quad| {
                    quad.bounds == panel.scale(window.scale_factor())
                        && quad.background == theme.panel_background.into()
                })
                .expect("sidebar background should meet the window edges");
            assert_eq!(panel_fill.border_widths.top, gpui::ScaledPixels(0.0));
            assert_eq!(panel_fill.border_widths.right, gpui::ScaledPixels(0.0));
            assert_eq!(panel_fill.border_widths.bottom, gpui::ScaledPixels(0.0));
            assert_eq!(panel_fill.corner_radii.top_left, gpui::ScaledPixels(0.0));
            assert_eq!(panel_fill.corner_radii.top_right, gpui::ScaledPixels(0.0));
            // TestWindow uses system decorations on Linux.
            let bottom_radius = if cfg!(any(target_os = "windows", target_os = "linux")) {
                0.0
            } else {
                (f32::from(theme.radius_lg) - 1.0).max(0.0) * window.scale_factor()
            };
            assert_eq!(
                panel_fill.corner_radii.bottom_right,
                gpui::ScaledPixels(bottom_radius)
            );
        });
        assert!(cx.debug_bounds("playback-progress").is_none());
        assert!(cx.debug_bounds("playback-episodes-button").is_none());
        click(cx, "playback-episodes-close");
        assert!(cx.debug_bounds("playback-episodes-panel").is_none());
        assert!(cx.debug_bounds("playback-progress").is_some());
    }

    page.update(cx, |page, cx| {
        page.playback_details_visible = true;
        cx.notify();
    });
    for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
        click(cx, "playback-episodes-button");
        let outside = point(px(300.0), px(250.0));
        cx.simulate_mouse_move(outside, None, Modifiers::default());
        cx.simulate_mouse_down(outside, button, Modifiers::default());
        cx.simulate_mouse_up(outside, button, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-episodes-panel").is_none());
        assert!(cx.debug_bounds("playback-progress").is_some());
        assert!(!page.read_with(cx, |page, _| page.timeline.user_paused));
    }

    click(cx, "playback-episodes-button");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
    assert!(cx.debug_bounds("playback-progress").is_some());
}

#[gpui::test]
fn episode_drawer_scrolls_to_current_and_keeps_the_fullscreen_cursor_visible(
    cx: &mut TestAppContext,
) {
    let (page, cx) = playback_window(cx);
    page.update(cx, |page, cx| {
        page.queue = PlaybackQueue::new((0..100).map(episode).collect(), 80);
        cx.notify();
    });
    cx.update(|window, _| window.toggle_fullscreen());
    cx.run_until_parked();
    click(cx, "playback-episodes-button");
    assert!(cx.debug_bounds("playback-episode-0").is_none());
    let current = cx.debug_bounds("playback-episode-80").unwrap();
    let list = cx.debug_bounds("playback-episodes-list").unwrap();
    let thumb = scrollbar_thumb(cx).expect("long episode list must paint a scrollbar thumb");
    assert!(list.contains(&thumb.center()));
    assert!(list.contains(&current.center()));
    assert!(
        cx.update(|window, cx| window.painted_quads().iter().any(|quad| {
            quad.bounds == current.scale(window.scale_factor())
                && quad.border_color == theme::media_overlay(cx).input_border_focused
        }))
    );
    let volume = page.read_with(cx, |page, _| page.volume.level);
    let scroll_before = page.read_with(cx, |page, _| {
        page.episode_list.scroll.0.borrow().base_handle.offset()
    });
    cx.simulate_mouse_move(list.center(), None, Modifiers::default());
    cx.simulate_event(ScrollWheelEvent {
        position: list.center(),
        delta: ScrollDelta::Lines(point(0.0, -6.0)),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    });
    cx.run_until_parked();
    assert_eq!(page.read_with(cx, |page, _| page.volume.level), volume);
    assert!(page.read_with(cx, |page, _| {
        page.episode_list.scroll.0.borrow().base_handle.offset().y < scroll_before.y
    }));
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(page.read_with(cx, |page, _| page.fullscreen.cursor_visible
        && page.episode_list.open));
    assert!(cx.debug_bounds("playback-progress").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.update(|window, _| window.is_fullscreen()));
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
    assert!(cx.debug_bounds("playback-progress").is_some());
}

#[gpui::test]
fn current_episode_and_movie_do_not_start_queue_switches(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    click(cx, "playback-episodes-button");
    click(cx, "playback-episode-0");
    page.read_with(cx, |page, _| {
        assert!(!page.queue_switch.loading);
        assert!(!page.timeline.user_paused);
        assert_eq!(page.queue.current_index, 0);
        assert!(!page.episode_list.open);
    });
    page.update(cx, |page, cx| {
        page.queue.items.truncate(1);
        page.queue.items[0].series_id = None;
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, "playback-episodes-button");
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
}

#[gpui::test]
fn unavailable_episode_restores_current_playback_and_shows_error(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    page.update(cx, |page, _| page.queue.items[2].media_sources.clear());
    click(cx, "playback-episodes-button");
    click(cx, "playback-episode-2");
    page.read_with(cx, |page, _| {
        assert!(!page.queue_switch.loading);
        assert_eq!(page.queue.current_index, 0);
        assert!(!page.timeline.user_paused);
        assert!(!page.timeline.paused);
        assert!(!page.episode_list.open);
    });
    assert!(cx.debug_bounds("playback-queue-switch-error").is_some());
    assert!(cx.debug_bounds("playback-progress").is_some());
}

#[gpui::test]
fn episode_scrollbar_drags_the_list_and_hides_without_overflow(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    click(cx, "playback-episodes-button");
    let thumb = scrollbar_thumb(cx).expect("overflow scrollbar");
    let start = thumb.center();
    let end = start + point(px(0.0), px(100.0));
    cx.simulate_mouse_move(start, None, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert!(page.episode_list.scroll.0.borrow().base_handle.offset().y < px(0.0));
        assert!(page.episode_list.open);
        assert!(!page.queue_switch.loading);
    });
    assert!(scrollbar_thumb(cx).unwrap().top() > thumb.top());
    click(cx, "playback-episodes-close");
    page.update(cx, |page, cx| {
        page.queue.items.truncate(2);
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, "playback-episodes-button");
    assert!(scrollbar_thumb(cx).is_none());
}

#[gpui::test]
fn clicking_non_adjacent_episode_resolves_playback_and_preserves_resume_positions(
    cx: &mut TestAppContext,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let worker = thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        started.elapsed() < Duration::from_secs(5),
                        "no playback info request"
                    );
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        };
        // Accepted sockets inherit nonblocking mode on Windows.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let body = json!({
            "MediaSources": [{"Id": "source-2", "ItemId": "episode-2-version", "DirectStreamUrl": "/Videos/episode-2/stream.mkv"}],
            "PlaySessionId": "new-session"
        }).to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        headers
    });
    let (page, cx) = playback_window(cx);
    page.update(cx, |page, cx| {
        page.emby.server.endpoint.address = "127.0.0.1".into();
        page.emby.server.endpoint.port = port;
        let source = serde_json::from_value(json!({
            "Id": "source-2", "DefaultSubtitleStreamIndex": 10,
            "MediaStreams": [
                {"Index": 1, "Type": "Audio", "DisplayTitle": "Japanese"},
                {"Index": 5, "Type": "Audio", "DisplayTitle": "Chinese", "IsDefault": true},
                {"Index": 9, "Type": "Subtitle", "DisplayTitle": "Chinese Simplified (默认 ASS)", "Codec": "ass", "IsDefault": true},
                {"Index": 10, "Type": "Subtitle", "DisplayTitle": "Chinese Simplified (ASS)", "Codec": "ass"}
            ]
        })).unwrap();
        let audio = playback_audio_tracks_for_source(&source);
        let key = crate::player::PlaybackTrackPreferenceKey {
            item_id: "episode-2".into(),
            media_source_id: "source-2".into(),
        };
        crate::player::PlaybackTrackPreferences::remember(
            &page.emby.server, std::slice::from_ref(&key), PlaybackTrackKind::Audio,
            audio.first(), cx,
        );
        crate::player::PlaybackTrackPreferences::remember(
            &page.emby.server, &[key], PlaybackTrackKind::Subtitle, None, cx,
        );
        page.queue.items[2].media_sources = vec![source];
    });
    let replacements = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let replacements = replacements.clone();
        cx.subscribe(&page, move |_, event: &PlaybackEvent, _| {
            if let PlaybackEvent::Replace { request, update } = event {
                replacements
                    .borrow_mut()
                    .push((request.clone(), update.clone()));
            }
        })
        .detach();
    });
    click(cx, "playback-episodes-button");
    click(cx, "playback-episode-2");
    let headers = worker.join().unwrap();
    assert!(headers.starts_with("POST /emby/Items/episode-2/PlaybackInfo?"));
    assert!(headers.contains("MediaSourceId=source-2"));
    let replacements = replacements.borrow();
    assert_eq!(replacements.len(), 1);
    let (request, update) = &replacements[0];
    assert_eq!(request.queue.current_index, 2);
    assert_eq!(request.emby.item_id, "episode-2-version");
    assert_eq!(request.selected_tracks.audio_stream_index, Some(1));
    assert_eq!(request.selected_tracks.subtitle_stream_index, None);
    assert_eq!(request.track_preference_key.item_id, "episode-2");
    assert_eq!(request.track_preference_key.media_source_id, "source-2");
    assert_eq!(request.initial_position_seconds, 10.0);
    assert_eq!(
        request.queue.items[0].playback_position_ticks,
        Some(450_000_000)
    );
    assert_eq!(
        request.queue.items[2].episode_label,
        episode(2).episode_label
    );
    assert_eq!(
        request.queue.items[2].premiere_date,
        episode(2).premiere_date
    );
    assert_eq!(update.list_item_id, "episode-0");
    assert_eq!(update.selected_item_id.as_deref(), Some("episode-2"));
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
}
