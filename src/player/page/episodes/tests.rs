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
        run_time_ticks: Some(18_000_000_000),
        playback_position_ticks: Some(100_000_000),
        media_sources: vec![
            serde_json::from_value(json!({"Id": format!("source-{index}")})).unwrap(),
        ],
    }
}

fn playback_window(cx: &mut TestAppContext) -> (Entity<PlaybackPage>, &mut VisualTestContext) {
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
            source_protocol: None,
            content_length: None,
            playback_file_info: None,
            playback_info: None,
            playback_audio_info: None,
            queue: PlaybackQueue::new((0..12).map(episode).collect(), 0),
            episode_list: PlaybackEpisodeListState::default(),
            reporting: session::PlaybackReportingState::new(&emby),
            emby,
            queue_switch: queue::PlaybackQueueSwitchState::default(),
            tracks: TrackSelectState::new(
                Vec::new(),
                Vec::new(),
                PlaybackTrackSelection::default(),
            ),
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
fn episode_image_label_and_overview_start_switching_on_mouse_down(cx: &mut TestAppContext) {
    let (page, cx) = playback_window(cx);
    // Resolve locally to an error after checking the synchronous selection, so
    // each target can be pressed independently without starting a decoder.
    page.update(cx, |page, _| page.queue.items[2].media_sources.clear());
    for selector in [
        "playback-episode-image-2",
        "playback-episode-label-2",
        "playback-episode-overview-2",
    ] {
        click(cx, "playback-episodes-button");
        let position = cx.debug_bounds(selector).unwrap().center();
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
        page.read_with(cx, |page, _| {
            assert!(
                !page.episode_list.open,
                "{selector} must select on mouse down"
            );
            assert!(page.queue_switch.loading);
            assert!(page.timeline.user_paused);
        });
        cx.run_until_parked();
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert!(!page.queue_switch.loading);
            assert!(!page.timeline.user_paused);
        });
    }
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
    page.update(cx, |page, _| {
        page.emby.server.endpoint.address = "127.0.0.1".into();
        page.emby.server.endpoint.port = port;
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
    assert_eq!(request.initial_position_seconds, 10.0);
    assert_eq!(
        request.queue.items[0].playback_position_ticks,
        Some(450_000_000)
    );
    assert_eq!(
        request.queue.items[2].episode_label,
        episode(2).episode_label
    );
    assert_eq!(update.list_item_id, "episode-0");
    assert_eq!(update.selected_item_id.as_deref(), Some("episode-2"));
    assert!(cx.debug_bounds("playback-episodes-panel").is_none());
}
