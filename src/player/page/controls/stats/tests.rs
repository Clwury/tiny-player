use tiny_playback::{ByteCacheState, DemuxCacheState};

use super::*;

fn lines(section: PlaybackDetailSection) -> Vec<String> {
    section.lines().into_iter().map(|line| line.text).collect()
}

fn video_info() -> PlaybackVideoInfo {
    PlaybackVideoInfo {
        codec: "hevc".into(),
        codec_description: Some("HEVC (High Efficiency Video Coding)".into()),
        profile: Some("Main 10".into()),
        decoder: "hevc".into(),
        size: RenderSize {
            width: 3840,
            height: 2160,
        },
        sample_aspect_ratio: Some((1, 1)),
        frame_rate: Some(23.976),
        pixel_format: Some(
            if cfg!(target_endian = "little") {
                "yuv420p10le"
            } else {
                "yuv420p10be"
            }
            .into(),
        ),
        color_range: Some("tv".into()),
        chroma_location: Some("left".into()),
        color_space: Some("bt2020nc".into()),
        color_primaries: Some("bt2020".into()),
        color_transfer: Some("smpte2084".into()),
        bitrate: Some(18_500_000),
        hardware_accelerated: true,
    }
}

fn audio_info() -> PlaybackAudioInfo {
    PlaybackAudioInfo {
        codec: "eac3".into(),
        codec_description: Some("ATSC A/52B (AC-3, E-AC-3)".into()),
        profile: None,
        decoder: "eac3".into(),
        channels: Some(6),
        channel_layout: Some("5.1(side)".into()),
        sample_format: Some("fltp".into()),
        sample_rate: Some(48_000),
        output_channels: Some(2),
        output_sample_format: Some("f32".into()),
        output_sample_rate: Some(48_000),
        output_device: Some("Built-in Audio".into()),
        bitrate: Some(768_000),
    }
}

#[test]
fn http_filename_keeps_parameters_and_fragment_after_the_decoded_basename() {
    for (source, expected) in [
        (
            "https://example.test/Videos/123/stream.mkv?api_key=secret",
            "stream.mkv?api_key=secret",
        ),
        (
            "http://example.test/a/%E7%A4%BA%E4%BE%8B%20A+B%3D1.mkv?name=wrong.mkv#part",
            "示例 A+B=1.mkv?name=wrong.mkv#part",
        ),
        (
            "https://example.test/a%2Fclip.mkv?redirect=/another/file.mp4",
            "clip.mkv?redirect=/another/file.mp4",
        ),
        ("https://example.test/clip%2520one.mkv", "clip%20one.mkv"),
        ("https://example.test/clip%ZZ.mkv", "clip%ZZ.mkv"),
        ("file:///tmp/clip%20one.mkv", "clip one.mkv"),
        ("/tmp/clip%20one.mkv", "clip%20one.mkv"),
        ("clip.mkv", "clip.mkv"),
        (
            "https://user:password@example.test/?api_key=secret",
            "/?api_key=secret",
        ),
        ("https://example.test/clip.mkv?", "clip.mkv?"),
        (
            "https://example.test/clip.mkv#part?value=1",
            "clip.mkv#part?value=1",
        ),
        (
            "https://example.test/clip.mkv?token=a%2Fb%2BC+1&key=&key=2#",
            "clip.mkv?token=a%2Fb%2BC+1&key=&key=2#",
        ),
    ] {
        assert_eq!(playback_filename(source), expected);
    }
}

#[test]
fn file_fields_follow_mpv_page_one_order_and_grouping() {
    let file = PlaybackFileInfo {
        format_name: Some("matroska,webm".into()),
        format_description: Some("Matroska / WebM".into()),
        bitrate: Some(20_000_000),
    };
    let cache = PlaybackCacheState {
        demux: DemuxCacheState {
            forward_bytes: 32 * 1024 * 1024,
            cache_duration: Some(12.5),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        lines(playback_file_detail_section(
            "https://example.test/Videos/123/stream.mkv?api_key=secret",
            "示例视频",
            Some(2 * 1024 * 1024 * 1024),
            Some(&file),
            Some(&cache),
        )),
        [
            "File:  stream.mkv?api_key=secret",
            "Title:  示例视频",
            "Size:  2.000 GiB    Format/Protocol:  mkv",
            "Total Cache:  32.00 MiB  (12.5 sec)",
        ]
    );
}

#[test]
fn file_format_is_a_demuxer_name_even_without_size() {
    let file = PlaybackFileInfo {
        format_name: Some("mov,mp4,m4a,3gp,3g2,mj2".into()),
        format_description: Some("QuickTime / MOV".into()),
        ..Default::default()
    };
    assert_eq!(
        lines(playback_file_detail_section(
            "clip.mp4",
            "clip.mp4",
            None,
            Some(&file),
            None
        )),
        [
            "File:  clip.mp4",
            "Format/Protocol:  mov,mp4,m4a,3gp,3g2,mj2",
        ]
    );
    assert_eq!(
        lines(playback_file_detail_section("stream", "", None, None, None)),
        ["File:  stream"]
    );
}

#[test]
fn file_size_prefers_the_http_response_over_catalog_metadata() {
    let cache = PlaybackCacheState {
        byte: Some(ByteCacheState {
            content_length: Some(4096),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        lines(playback_file_detail_section(
            "clip.mkv",
            "",
            Some(10),
            None,
            Some(&cache)
        )),
        ["File:  clip.mkv", "Size:  4.000 KiB",]
    );
}

#[test]
fn file_size_matches_mpv_precision_at_unit_boundaries() {
    for (bytes, expected) in [
        (0, "0 B"),
        (1023, "1023 B"),
        (1024, "1.000 KiB"),
        (1025, "1.001 KiB"),
        (1024 * 1024 - 1, "1023.999 KiB"),
        (1024 * 1024, "1.000 MiB"),
        (1 << 30, "1.000 GiB"),
        (1 << 40, "1.000 TiB"),
        (1 << 50, "1024.000 TiB"),
    ] {
        assert_eq!(format_file_size(bytes), expected);
    }
}

#[test]
fn cache_always_pairs_two_decimal_bytes_with_duration() {
    for (bytes, duration, expected) in [
        (0, None, None),
        (0, Some(f64::NAN), None),
        (0, Some(-1.0), None),
        (0, Some(1.25), Some("0.00 Bytes  (1.2 sec)")),
        (512, None, Some("512.00 Bytes  (0.0 sec)")),
        (1024, Some(f64::INFINITY), Some("1.00 KiB  (0.0 sec)")),
        (1 << 30, Some(123.4), Some("1.00 GiB  (123.4 sec)")),
    ] {
        let cache = PlaybackCacheState {
            demux: DemuxCacheState {
                forward_bytes: bytes,
                cache_duration: duration,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(playback_total_cache(Some(&cache)).as_deref(), expected);
    }
    assert_eq!(format_stats_cache_bytes(1 << 50), "1.00 PiB");
    assert_eq!(format_stats_cache_bytes(1 << 60), "1.00 *1024^6");
}

#[test]
fn video_fields_match_mpv_hdr_names_and_grouping() {
    assert_eq!(
        lines(playback_video_detail_section(Some(&video_info())).unwrap()),
        [
            "Video:  HEVC (High Efficiency Video Coding) [Main 10]    HW:  vulkan",
            "Frame Rate:  23.976 fps (specified)",
            "Resolution:  3840 x 2160  1.78:1 (16:9)",
            "Format:  yuv420p10    Levels:  limited    Chroma Loc:  mpeg2/4/h264",
            "Colormatrix:  bt.2020-ncl    Primaries:  bt.2020    Transfer:  pq",
            "Bitrate:  18.500 Mbps",
        ]
    );
}

#[test]
fn software_decoder_and_missing_fields_do_not_create_extra_stats_rows() {
    let mut info = video_info();
    info.hardware_accelerated = false;
    info.decoder = "hevc_custom".into();
    info.frame_rate = Some(f64::NAN);
    info.pixel_format = None;
    info.color_range = None;
    info.chroma_location = None;
    info.color_space = None;
    info.color_primaries = None;
    info.color_transfer = None;
    info.bitrate = None;
    assert_eq!(
        lines(playback_video_detail_section(Some(&info)).unwrap()),
        [
            "Video:  HEVC (High Efficiency Video Coding) [Main 10] [hevc_custom]",
            "Resolution:  3840 x 2160  1.78:1 (16:9)",
        ]
    );
    assert!(playback_video_detail_section(None).is_none());
    assert!(playback_audio_detail_section(None, 1.0).is_none());
}

#[test]
fn video_storage_and_display_aspects_are_distinct_for_anamorphic_video() {
    let mut info = video_info();
    info.size = RenderSize {
        width: 720,
        height: 576,
    };
    info.sample_aspect_ratio = Some((16, 15));
    let section = lines(playback_video_detail_section(Some(&info)).unwrap());
    assert!(section.contains(&"Resolution:  720 x 576  1.25:1 (5:4)".to_string()));
    assert!(section.contains(&"Output Resolution:  768 x 576  1.33:1 (4:3)".to_string()));
    info.sample_aspect_ratio = Some((0, 1));
    assert_eq!(video_display_size(&info), None);
}

#[test]
fn audio_fields_use_counts_mpv_sample_formats_and_transform_arrow() {
    assert_eq!(
        lines(playback_audio_detail_section(Some(&audio_info()), 0.75).unwrap()),
        [
            "Audio:  ATSC A/52B (AC-3, E-AC-3)    AO:  cpal",
            "Device:  Built-in Audio    AO Volume:  75%",
            "Channels:  6 ➜ 2    Format:  floatp ➜ float",
            "Sample Rate:  48000 Hz",
            "Bitrate:  768 kbps",
        ]
    );
    let mut info = audio_info();
    info.output_device = None;
    info.channels = Some(2);
    info.sample_format = Some("flt".into());
    info.output_sample_rate = Some(44_100);
    let section = lines(playback_audio_detail_section(Some(&info), 0.0).unwrap());
    assert!(section.contains(&"AO Volume:  0% (Muted)".to_string()));
    assert!(section.contains(&"Channels:  2    Format:  float".to_string()));
    assert!(section.contains(&"Sample Rate:  48000 ➜ 44100 Hz".to_string()));
}

#[test]
fn bitrate_and_frame_rate_use_mpv_property_print_precision() {
    for (bitrate, expected) in [
        (0, "0 kbps"),
        (768_000, "768 kbps"),
        (768_750, "769 kbps"),
        (999_999, "1000 kbps"),
        (1_000_000, "1.000 Mbps"),
        (18_543_210, "18.543 Mbps"),
        (1_000_000_000, "1000.000 Mbps"),
    ] {
        assert_eq!(format_bitrate(bitrate), expected);
    }
    for (rate, expected) in [
        (24.0, "24"),
        (60_000.0 / 1001.0, "59.9401"),
        (23.976, "23.976"),
    ] {
        assert_eq!(format_decimal(rate), expected);
    }
}

#[test]
fn color_names_match_mpv_sdr_hlg_and_chroma_conventions() {
    assert_eq!(color_levels_name("pc"), "full");
    assert_eq!(color_matrix_name("bt470bg", None), "bt.601");
    assert_eq!(color_matrix_name("bt709", None), "bt.709");
    assert_eq!(
        color_matrix_name("ictcp", Some("arib-std-b67")),
        "bt.2100-hlg"
    );
    assert_eq!(color_matrix_name("ictcp", Some("smpte2084")), "bt.2100-pq");
    assert_eq!(color_primaries_name("smpte170m"), "bt.601-525");
    assert_eq!(color_primaries_name("smpte432"), "display-p3");
    assert_eq!(color_transfer_name("bt709"), "bt.1886");
    assert_eq!(color_transfer_name("arib-std-b67"), "hlg");
    assert_eq!(chroma_location_name("topleft"), "uhd");
    assert_eq!(chroma_location_name("center"), "mpeg1/jpeg");
}

#[test]
fn grouped_labels_keep_utf8_highlight_boundaries() {
    let section = PlaybackDetailSection {
        title: "File",
        summary: "示例.mkv".into(),
        rows: vec![PlaybackDetailRow::new("Format/Protocol", "matroska,webm").inline()],
    };
    let line = section.lines().remove(0);
    let labels: Vec<_> = line
        .labels
        .into_iter()
        .map(|range| line.text[range].to_string())
        .collect();
    assert_eq!(labels, ["File:", "Format/Protocol:"]);
}

#[gpui::test]
fn grouped_stats_wrap_without_overlapping_sections_at_narrow_widths(cx: &mut gpui::TestAppContext) {
    struct StatsPreview;

    impl Render for StatsPreview {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().relative().size_full().child(playback_details_overlay(
                [
                    Some(playback_file_detail_section(
                        "Example movie with a long HTTP filename.mkv",
                        "示例视频",
                        Some(2 << 30),
                        Some(&PlaybackFileInfo {
                            format_name: Some("matroska,webm".into()),
                            ..Default::default()
                        }),
                        None,
                    )),
                    playback_video_detail_section(Some(&video_info())),
                    playback_audio_detail_section(Some(&audio_info()), 0.75),
                ],
                window,
            ))
        }
    }

    let (_, cx) = cx.add_window_view(|_, _| StatsPreview);
    for width in [1280.0, 640.0] {
        cx.simulate_resize(gpui::size(px(width), px(720.0)));
        cx.run_until_parked();
        let overlay = cx.debug_bounds("playback-details-overlay").unwrap();
        let file = cx.debug_bounds("playback-stats-File").unwrap();
        let video = cx.debug_bounds("playback-stats-Video").unwrap();
        let audio = cx.debug_bounds("playback-stats-Audio").unwrap();
        assert!(overlay.right() <= px(width));
        assert!(file.bottom() < video.top());
        assert!(video.bottom() < audio.top());
        assert!(audio.bottom() <= overlay.bottom());
        assert!(video.size.height >= px(120.0));
    }
}

#[gpui::test]
fn stats_forward_pointer_events_to_the_playback_surface_without_copying(
    cx: &mut gpui::TestAppContext,
) {
    #[derive(Default)]
    struct StatsPreview {
        presses: usize,
        drags: usize,
        scrolls: usize,
    }

    impl Render for StatsPreview {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _, _, _| {
                                view.presses += 1;
                            }),
                        )
                        .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, _| {
                            if event.dragging() {
                                view.drags += 1;
                            }
                        }))
                        .on_scroll_wheel(cx.listener(|view, _, _, _| view.scrolls += 1)),
                )
                .child(playback_details_overlay(
                    [
                        Some(playback_file_detail_section(
                            "https://example.test/video.mkv?token=a%2Fb&redirect=/next.mkv",
                            "示例视频",
                            Some(2 << 30),
                            None,
                            None,
                        )),
                        playback_video_detail_section(Some(&video_info())),
                    ],
                    window,
                ))
        }
    }

    let (view, cx) = cx.add_window_view(|_, _| StatsPreview::default());
    cx.update(|_, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("unchanged".into()));
    });
    let mut expected_events = 0;
    for width in [1280.0, 640.0] {
        cx.simulate_resize(gpui::size(px(width), px(480.0)));
        cx.run_until_parked();
        let overlay = cx.debug_bounds("playback-details-overlay").unwrap();
        let file = cx.debug_bounds("playback-stats-File").unwrap();
        let video = cx.debug_bounds("playback-stats-Video").unwrap();
        let points = [
            file.origin + gpui::point(px(12.0), px(10.0)),
            gpui::point(file.left() + px(24.0), file.bottom() - px(8.0)),
            gpui::point(file.left() + px(12.0), (file.bottom() + video.top()) / 2.0),
            overlay.origin + gpui::point(px(1.0), px(1.0)),
            gpui::point(overlay.right() - px(8.0), file.top() + px(10.0)),
            gpui::point(px(width - 8.0), px(460.0)),
        ];
        for position in points {
            cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
            cx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
            let end = position + gpui::point(px(2.0), px(2.0));
            cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
            cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
            cx.run_until_parked();
            expected_events += 1;
            view.read_with(cx, |view, _| {
                assert_eq!(view.presses, expected_events);
                assert_eq!(view.drags, expected_events);
            });
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("unchanged")
            );
        }
        cx.simulate_event(ScrollWheelEvent {
            position: points[0],
            delta: ScrollDelta::Lines(gpui::point(0.0, -3.0)),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.scrolls), 0);
    }
}

#[gpui::test]
fn progress_drag_across_stats_does_not_move_the_playback_window(cx: &mut gpui::TestAppContext) {
    let (view, cx) = crate::player::page::episodes::tests::playback_window(cx);
    cx.simulate_keystrokes("i");
    for fullscreen in [false, true] {
        cx.update(|window, _| {
            if window.is_fullscreen() != fullscreen {
                window.toggle_fullscreen();
                window.refresh();
            }
        });
        cx.run_until_parked();
        // Flush the layout observer before seeking; the test window has no
        // compositor frame clock to publish the progress-track bounds for us.
        cx.update(|window, cx| window.simulate_next_frame(cx));
        let track = cx.debug_bounds("playback-progress-track").unwrap();
        let file = cx.debug_bounds("playback-stats-File").unwrap();
        let start = track.center();
        let end = gpui::point(start.x + px(10.0), file.top() + px(10.0));
        cx.simulate_mouse_move(start, None, gpui::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_mouse_down(start, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(view.read_with(cx, |page, _| page.timeline.progress_drag_position.is_some()));
        // GPUI's test window does not implement native window moves. Reaching
        // that path while seeking would panic instead of completing this drag.
        cx.simulate_mouse_move(end, Some(MouseButton::Left), gpui::Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        view.read_with(cx, |page, _| {
            assert!(page.playback_details_visible);
            assert!(page.timeline.progress_drag_position.is_none());
        });
    }
}
