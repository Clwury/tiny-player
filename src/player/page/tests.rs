use std::{cell::RefCell, rc::Rc};

use gpui::{Bounds, point, px, size};

use super::{
    AnimationFrameRequestState, BackendSubtitleBitmap, BackendSubtitleCue,
    HttpStreamBufferProgress, PlaybackShortcut, PlaybackVideoInfo, RenderSize, ShutdownOrder,
    aspect_fit_bounds, buffered_progress_fraction, clamp_playback_position,
    combined_buffered_until, format_playback_time, fullscreen_controls_hot_zone_contains,
    fullscreen_progress_controls_contains, http_stream_buffered_range_fractions,
    http_stream_buffered_until, is_seek_position_buffered, local_video_viewport_bounds,
    normalize_video_viewport, playback_info_segments, playback_progress_bar_bounds,
    playback_progress_bar_visible, playback_shortcut_for_key, playback_status_message,
    progress_fraction, progress_fraction_for_cursor, render_output_size,
    should_apply_backend_position, should_render_frame, should_request_animation_frame,
    subtitle_bitmap_bottom_offset, subtitle_bitmap_canvas_size, subtitle_text_overlay_height,
    valid_frame_rate, valid_http_stream_buffer_progress, valid_playback_duration,
    valid_playback_time, viewport_changed,
};
use crate::player::render_host::render_image_from_bgra;

struct DropRecorder {
    name: &'static str,
    drops: Rc<RefCell<Vec<&'static str>>>,
}

impl Drop for DropRecorder {
    fn drop(&mut self) {
        self.drops.borrow_mut().push(self.name);
    }
}

#[test]
fn normalize_video_viewport_rejects_zero_sized_bounds() {
    let zero_width = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));
    let zero_height = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(0.0)));

    assert_eq!(normalize_video_viewport(zero_width), None);
    assert_eq!(normalize_video_viewport(zero_height), None);
}

#[test]
fn normalize_video_viewport_floors_fractional_pixel_sizes() {
    let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));

    assert_eq!(normalize_video_viewport(bounds), Some((640, 359)));
}

#[test]
fn viewport_changed_only_reports_real_differences() {
    let first = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
    let second = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)));

    assert!(viewport_changed(None, first));
    assert!(!viewport_changed(Some(first), first));
    assert!(viewport_changed(Some(first), second));
}

#[test]
fn aspect_fit_bounds_letterboxes_wide_video_in_tall_viewport() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
    let fitted = aspect_fit_bounds(
        bounds,
        RenderSize {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap();

    assert_eq!(fitted.origin, point(px(0.0), px(75.0)));
    assert_eq!(fitted.size, size(px(800.0), px(450.0)));
}

#[test]
fn aspect_fit_bounds_pillarboxes_tall_video_in_wide_viewport() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(720.0)));
    let fitted = aspect_fit_bounds(
        bounds,
        RenderSize {
            width: 640,
            height: 480,
        },
    )
    .unwrap();

    assert_eq!(fitted.origin, point(px(160.0), px(0.0)));
    assert_eq!(fitted.size, size(px(960.0), px(720.0)));
}

#[test]
fn aspect_fit_bounds_rejects_zero_source_or_viewport() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
    let zero_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(600.0)));

    assert_eq!(
        aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 0,
                height: 1080,
            },
        ),
        None
    );
    assert_eq!(
        aspect_fit_bounds(
            zero_bounds,
            RenderSize {
                width: 1920,
                height: 1080,
            },
        ),
        None
    );
}

#[test]
fn aspect_fit_bounds_handles_fractional_viewport_sizes() {
    let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));
    let fitted = aspect_fit_bounds(
        bounds,
        RenderSize {
            width: 3840,
            height: 2160,
        },
    )
    .unwrap();

    assert_eq!(normalize_video_viewport(fitted), Some((639, 359)));
}

#[test]
fn render_output_size_uses_aspect_fitted_viewport() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));

    assert_eq!(
        render_output_size(
            bounds,
            RenderSize {
                width: 3840,
                height: 1600,
            },
        ),
        Some(RenderSize {
            width: 1920,
            height: 800,
        })
    );
}

#[test]
fn local_video_viewport_bounds_strips_window_origin_for_overlay_layout() {
    let observed = Bounds::new(
        point(px(0.0), px(1043.3334)),
        size(px(1485.3334), px(1008.0)),
    );

    assert_eq!(
        local_video_viewport_bounds(observed),
        Bounds::new(point(px(0.0), px(0.0)), observed.size)
    );
}

#[test]
fn playback_progress_bar_is_gated_by_fullscreen_controls_visibility() {
    assert!(playback_progress_bar_visible(false, false));
    assert!(playback_progress_bar_visible(false, true));
    assert!(!playback_progress_bar_visible(true, false));
    assert!(playback_progress_bar_visible(true, true));
}

#[test]
fn fullscreen_controls_hot_zone_starts_at_viewport_lower_half() {
    let viewport = Bounds::new(point(px(0.0), px(100.0)), size(px(800.0), px(600.0)));

    assert!(!fullscreen_controls_hot_zone_contains(
        point(px(400.0), px(399.0)),
        viewport
    ));
    assert!(fullscreen_controls_hot_zone_contains(
        point(px(400.0), px(400.0)),
        viewport
    ));
}

#[test]
fn fullscreen_progress_controls_hit_area_matches_progress_bar_layout() {
    let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(1000.0)));

    assert_eq!(
        playback_progress_bar_bounds(viewport),
        Bounds::new(point(px(300.0), px(882.0)), size(px(400.0), px(94.0)))
    );
    assert!(fullscreen_progress_controls_contains(
        point(px(500.0), px(900.0)),
        viewport
    ));
    assert!(!fullscreen_progress_controls_contains(
        point(px(500.0), px(800.0)),
        viewport
    ));
    assert!(!fullscreen_progress_controls_contains(
        point(px(750.0), px(900.0)),
        viewport
    ));
}

#[test]
fn subtitle_text_overlay_height_stops_at_progress_bar_top() {
    let video_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
    let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

    assert_eq!(
        subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true),
        px(407.0)
    );
}

#[test]
fn subtitle_text_overlay_height_uses_video_bottom_without_visible_progress_bar() {
    let video_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(700.0)));
    let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

    assert_eq!(
        subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true),
        px(450.0)
    );
    assert_eq!(
        subtitle_text_overlay_height(video_fitted_bounds, video_bounds, false),
        px(450.0)
    );
}

#[test]
fn subtitle_bitmap_bottom_offset_lifts_only_overlapping_bitmap_content() {
    let image = render_image_from_bgra(vec![0, 0, 0, 0], 1, 1).unwrap();
    let cue = BackendSubtitleCue {
        text: String::new(),
        bitmaps: vec![BackendSubtitleBitmap {
            image,
            x: 0,
            y: 900,
            width: 100,
            height: 100,
            canvas_width: 1920,
            canvas_height: 1080,
        }],
        start_nsecs: 0,
        end_nsecs: 1_000_000_000,
    };
    let bitmap_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));

    assert_eq!(
        subtitle_bitmap_bottom_offset(&cue, bitmap_bounds, 1.0, px(960.0)),
        px(40.0)
    );
    assert_eq!(
        subtitle_bitmap_bottom_offset(&cue, bitmap_bounds, 1.0, px(1000.0)),
        px(0.0)
    );
}

#[test]
fn subtitle_bitmap_canvas_size_uses_largest_bitmap_canvas() {
    let image = render_image_from_bgra(vec![0, 0, 0, 0], 1, 1).unwrap();
    let cue = BackendSubtitleCue {
        text: String::new(),
        bitmaps: vec![
            BackendSubtitleBitmap {
                image: image.clone(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                canvas_width: 1920,
                canvas_height: 800,
            },
            BackendSubtitleBitmap {
                image,
                x: 0,
                y: 900,
                width: 1,
                height: 1,
                canvas_width: 1920,
                canvas_height: 1080,
            },
        ],
        start_nsecs: 0,
        end_nsecs: 1_000_000_000,
    };

    assert_eq!(
        subtitle_bitmap_canvas_size(&cue),
        Some(RenderSize {
            width: 1920,
            height: 1080,
        })
    );
}

#[test]
fn render_output_size_does_not_upscale_past_source() {
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(3840.0), px(2160.0)));

    assert_eq!(
        render_output_size(
            bounds,
            RenderSize {
                width: 1280,
                height: 720,
            },
        ),
        Some(RenderSize {
            width: 1280,
            height: 720,
        })
    );
}

#[test]
fn playback_status_stays_visible_until_first_frame() {
    assert_eq!(playback_status_message(true, false), "正在缓冲视频…");
    assert_eq!(playback_status_message(false, false), "正在加载视频…");
    assert_eq!(playback_status_message(false, true), "");
}

#[test]
fn playback_time_helpers_reject_invalid_values() {
    assert_eq!(valid_playback_time(12.0), Some(12.0));
    assert_eq!(valid_playback_time(-1.0), None);
    assert_eq!(valid_playback_time(f64::NAN), None);
    assert_eq!(valid_playback_duration(12.0), Some(12.0));
    assert_eq!(valid_playback_duration(0.0), None);
}

#[test]
fn progress_fraction_clamps_position_to_duration() {
    assert_eq!(clamp_playback_position(-5.0, 100.0), 0.0);
    assert_eq!(clamp_playback_position(25.0, 100.0), 25.0);
    assert_eq!(clamp_playback_position(125.0, 100.0), 100.0);
    assert_eq!(progress_fraction(25.0, 100.0), 0.25);
    assert_eq!(progress_fraction(125.0, 100.0), 1.0);
    assert_eq!(progress_fraction(25.0, 0.0), 0.0);
}

#[test]
fn buffered_progress_never_falls_behind_played_progress() {
    assert_eq!(buffered_progress_fraction(Some(20.0), 40.0, 100.0), 0.4);
    assert_eq!(buffered_progress_fraction(Some(80.0), 40.0, 100.0), 0.8);
    assert_eq!(buffered_progress_fraction(None, 40.0, 100.0), 0.4);
}

#[test]
fn backend_position_updates_wait_until_seek_finishes() {
    assert!(should_apply_backend_position(None, None));
    assert!(!should_apply_backend_position(Some(40.0), None));
    assert!(!should_apply_backend_position(None, Some(80.0)));
}

#[test]
fn cached_seek_detects_playback_and_http_buffered_ranges() {
    assert!(is_seek_position_buffered(
        60.0,
        Some(40.0),
        Some(80.0),
        None,
        Some(100.0),
    ));
    assert!(!is_seek_position_buffered(
        20.0,
        Some(40.0),
        Some(80.0),
        None,
        Some(100.0),
    ));

    let http_range = Some(HttpStreamBufferProgress {
        start_fraction: 0.2,
        end_fraction: 0.8,
    });
    assert!(is_seek_position_buffered(
        30.0,
        Some(90.0),
        None,
        http_range,
        Some(100.0),
    ));
    assert!(!is_seek_position_buffered(
        90.0,
        Some(10.0),
        None,
        http_range,
        Some(100.0),
    ));
}

#[test]
fn http_stream_buffer_progress_validates_and_clamps_fraction_range() {
    assert_eq!(
        valid_http_stream_buffer_progress(HttpStreamBufferProgress {
            start_fraction: -0.5,
            end_fraction: 1.5,
        }),
        Some(HttpStreamBufferProgress {
            start_fraction: 0.0,
            end_fraction: 1.0,
        })
    );
    assert_eq!(
        valid_http_stream_buffer_progress(HttpStreamBufferProgress {
            start_fraction: 0.8,
            end_fraction: 0.2,
        }),
        None
    );
    assert_eq!(
        valid_http_stream_buffer_progress(HttpStreamBufferProgress {
            start_fraction: f64::NAN,
            end_fraction: 0.2,
        }),
        None
    );
}

#[test]
fn http_stream_buffer_progress_only_applies_to_current_playback_range() {
    let range = Some(HttpStreamBufferProgress {
        start_fraction: 0.4,
        end_fraction: 0.7,
    });

    assert_eq!(http_stream_buffered_until(range, 50.0, 100.0), Some(70.0));
    assert_eq!(http_stream_buffered_until(range, 10.0, 100.0), None);
    assert_eq!(http_stream_buffered_until(range, 90.0, 100.0), None);
}

#[test]
fn http_stream_buffer_progress_range_fills_gap_from_continuous_cache() {
    let range = Some(HttpStreamBufferProgress {
        start_fraction: 0.4,
        end_fraction: 0.7,
    });

    assert_eq!(
        http_stream_buffered_range_fractions(range, 0.1),
        Some((0.1, 0.7))
    );
    assert_eq!(
        http_stream_buffered_range_fractions(range, 0.5),
        Some((0.4, 0.7))
    );
    assert_eq!(http_stream_buffered_range_fractions(range, 0.8), None);
    assert_eq!(http_stream_buffered_until(range, 10.0, 100.0), None);
}

#[test]
fn combined_buffered_progress_uses_furthest_playable_buffer() {
    let range = Some(HttpStreamBufferProgress {
        start_fraction: 0.2,
        end_fraction: 0.8,
    });

    assert_eq!(
        combined_buffered_until(Some(30.0), range, 40.0, 100.0),
        Some(80.0)
    );
    assert_eq!(
        combined_buffered_until(Some(90.0), range, 40.0, 100.0),
        Some(90.0)
    );
    assert_eq!(
        combined_buffered_until(None, range, 40.0, 100.0),
        Some(80.0)
    );
}

#[test]
fn progress_cursor_fraction_uses_track_bounds_and_clamps_edges() {
    let bounds = Bounds::new(point(px(100.0), px(0.0)), size(px(400.0), px(28.0)));

    assert_eq!(progress_fraction_for_cursor(px(100.0), bounds), Some(0.0));
    assert_eq!(progress_fraction_for_cursor(px(300.0), bounds), Some(0.5));
    assert_eq!(progress_fraction_for_cursor(px(700.0), bounds), Some(1.0));
    assert_eq!(
        progress_fraction_for_cursor(
            px(100.0),
            Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(28.0))),
        ),
        None
    );
}

#[test]
fn playback_shortcut_keys_map_to_player_actions() {
    assert_eq!(
        playback_shortcut_for_key("space"),
        Some(PlaybackShortcut::TogglePlayback)
    );
    assert_eq!(
        playback_shortcut_for_key(" "),
        Some(PlaybackShortcut::TogglePlayback)
    );
    assert_eq!(
        playback_shortcut_for_key("p"),
        Some(PlaybackShortcut::TogglePlayback)
    );
    assert_eq!(
        playback_shortcut_for_key("f"),
        Some(PlaybackShortcut::ToggleFullscreen)
    );
    assert_eq!(
        playback_shortcut_for_key("escape"),
        Some(PlaybackShortcut::ExitFullscreen)
    );
    assert_eq!(
        playback_shortcut_for_key("left"),
        Some(PlaybackShortcut::SeekBackward)
    );
    assert_eq!(
        playback_shortcut_for_key("right"),
        Some(PlaybackShortcut::SeekForward)
    );
    assert_eq!(
        playback_shortcut_for_key("i"),
        Some(PlaybackShortcut::ToggleInfoOverlay)
    );
    assert_eq!(playback_shortcut_for_key("enter"), None);
}

#[test]
fn format_playback_time_switches_to_hours_when_needed() {
    assert_eq!(format_playback_time(65.0), "1:05");
    assert_eq!(format_playback_time(3661.0), "1:01:01");
    assert_eq!(format_playback_time(f64::NAN), "0:00");
}

#[test]
fn playback_info_segments_include_hw_badge_and_frame_rate() {
    let info = PlaybackVideoInfo {
        decoder: "hevc".to_string(),
        size: RenderSize {
            width: 3840,
            height: 2160,
        },
        frame_rate: Some(23.976),
        hardware_accelerated: true,
    };

    assert_eq!(
        playback_info_segments(&info),
        vec![
            "hevc".to_string(),
            "3840x2160".to_string(),
            "23.98 FPS".to_string(),
            "HW".to_string()
        ]
    );
}

#[test]
fn playback_info_segments_mark_software_and_skip_invalid_rate() {
    let info = PlaybackVideoInfo {
        decoder: "h264".to_string(),
        size: RenderSize {
            width: 1920,
            height: 1080,
        },
        frame_rate: Some(f64::NAN),
        hardware_accelerated: false,
    };

    assert_eq!(
        playback_info_segments(&info),
        vec![
            "h264".to_string(),
            "1920x1080".to_string(),
            "SW".to_string()
        ]
    );
    assert_eq!(valid_frame_rate(0.0), None);
    assert_eq!(valid_frame_rate(f64::INFINITY), None);
    assert_eq!(valid_frame_rate(60.0), Some(60.0));
}

#[test]
fn should_render_frame_requires_loaded_file_video_size_and_valid_viewport() {
    assert!(should_render_frame(true, true, false, true, true));
    assert!(!should_render_frame(false, true, false, true, true));
    assert!(!should_render_frame(true, false, false, true, true));
    assert!(!should_render_frame(true, true, true, true, true));
    assert!(!should_render_frame(true, true, false, false, true));
    assert!(!should_render_frame(true, true, false, true, false));
}

#[test]
fn should_request_animation_frame_drives_initial_load() {
    assert!(should_request_animation_frame(AnimationFrameRequestState {
        has_loaded_file: false,
        has_viewport: false,
        playback_paused: true,
        ..animation_frame_request_state()
    }));
}

#[test]
fn should_request_animation_frame_requires_backend_and_presenter() {
    assert!(!should_request_animation_frame(
        AnimationFrameRequestState {
            has_backend: false,
            has_loaded_file: false,
            has_viewport: false,
            playback_paused: true,
            ..animation_frame_request_state()
        }
    ));
    assert!(!should_request_animation_frame(
        AnimationFrameRequestState {
            has_video_presenter: false,
            has_loaded_file: false,
            has_viewport: false,
            playback_paused: true,
            ..animation_frame_request_state()
        }
    ));
}

#[test]
fn should_request_animation_frame_stops_on_error() {
    assert!(!should_request_animation_frame(
        AnimationFrameRequestState {
            has_loaded_file: false,
            has_error: true,
            has_viewport: false,
            playback_paused: true,
            ..animation_frame_request_state()
        }
    ));
}

#[test]
fn should_request_animation_frame_requires_unpaused_loaded_video_with_viewport() {
    assert!(should_request_animation_frame(AnimationFrameRequestState {
        playback_paused: false,
        ..animation_frame_request_state()
    }));
    assert!(!should_request_animation_frame(
        animation_frame_request_state()
    ));
    assert!(!should_request_animation_frame(
        AnimationFrameRequestState {
            has_viewport: false,
            playback_paused: false,
            ..animation_frame_request_state()
        }
    ));
}

#[test]
fn should_request_animation_frame_continues_until_first_visible_frame() {
    assert!(should_request_animation_frame(AnimationFrameRequestState {
        has_visible_frame: false,
        ..animation_frame_request_state()
    }));
}

#[test]
fn should_request_animation_frame_continues_while_buffering() {
    assert!(should_request_animation_frame(AnimationFrameRequestState {
        has_viewport: false,
        playback_buffering: true,
        ..animation_frame_request_state()
    }));
}

#[test]
fn should_request_animation_frame_continues_during_soft_seek() {
    assert!(should_request_animation_frame(AnimationFrameRequestState {
        has_viewport: false,
        pending_seek: true,
        ..animation_frame_request_state()
    }));
}

fn animation_frame_request_state() -> AnimationFrameRequestState {
    AnimationFrameRequestState {
        has_backend: true,
        has_video_presenter: true,
        has_loaded_file: true,
        has_error: false,
        has_viewport: true,
        has_visible_frame: true,
        playback_paused: true,
        playback_buffering: false,
        pending_seek: false,
    }
}

#[test]
fn shutdown_order_drops_dependent_before_owner() {
    let drops = Rc::new(RefCell::new(Vec::new()));
    let recorded_drops = Rc::clone(&drops);

    let presenter = DropRecorder {
        name: "presenter",
        drops: Rc::clone(&drops),
    };
    let backend = DropRecorder {
        name: "backend",
        drops,
    };

    drop(ShutdownOrder::new(Some(backend), Some(presenter)));

    assert_eq!(&*recorded_drops.borrow(), &["presenter", "backend"]);
}
