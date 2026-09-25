use crate::ui::radius;

use super::diagnostics::playback_diagnostics_enabled;
use super::fullscreen::{
    PLAYBACK_BACK_BUTTON_OFFSET_PX, PLAYBACK_BACK_BUTTON_SIZE_PX,
    PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX, PLAYBACK_PROGRESS_BAR_HEIGHT_PX,
};
use super::state::effective_playback_paused;
use super::*;

const TRACK_SELECT_MENU_MAX_HEIGHT_PX: f32 = 260.0;
const VOLUME_INDICATOR_HIDE_DELAY: Duration = Duration::from_millis(1200);
const VOLUME_INDICATOR_BAR_HEIGHT_PX: f32 = 192.0;

#[derive(Clone, Copy)]
struct PlaybackControlsRenderState {
    can_switch_previous: bool,
    can_toggle_playback: bool,
    can_switch_next: bool,
    play_pause_icon: &'static str,
    cache_status_enabled: bool,
    cache_status_open: bool,
    can_select_audio: bool,
    can_select_subtitle: bool,
    audio_select_open: bool,
    subtitle_select_open: bool,
}

struct ProgressTimelineRenderState {
    current_time: String,
    duration_time: String,
    played_fraction: f32,
    cached_seek_preview: Option<bool>,
    forward_cache_fraction: Option<f32>,
    cache_ranges: Vec<(f32, f32)>,
}

#[path = "controls/download_speed.rs"]
mod download_speed;
pub(super) use download_speed::DownloadSpeedDisplay;
#[path = "controls/progress.rs"]
mod progress;
#[path = "controls/render.rs"]
mod render;
#[path = "controls/stats.rs"]
mod stats;
use stats::*;
#[path = "controls/tracks.rs"]
mod tracks;
#[path = "controls/transport.rs"]
mod transport;

fn progress_track_fill(color: gpui::Hsla, width_fraction: f32) -> gpui::Div {
    div()
        .absolute()
        .left_0()
        .top(px(11.0))
        .h(px(6.0))
        .w(relative(width_fraction))
        .rounded(px(3.0))
        .bg(color)
}

fn progress_track_played_fill(color: gpui::Hsla, width_fraction: f32) -> impl IntoElement {
    let width_fraction = width_fraction.clamp(0.0, 1.0);
    progress_track_fill(color, width_fraction)
        .when(width_fraction > 0.0, |fill| fill.min_w(px(6.0)))
}

fn progress_track_forward_cache_fill(
    color: gpui::Hsla,
    end_fraction: Option<f32>,
) -> Option<gpui::Div> {
    // The played fill covers this layer, keeping its rounded end continuous
    // with the visible forward cache segment without a seam between caps.
    Some(
        progress_track_fill(color, end_fraction?)
            .debug_selector(|| "playback-progress-forward-cache".to_string()),
    )
}

fn progress_track_seekable_range_fill(
    color: gpui::Hsla,
    start_fraction: f32,
    end_fraction: f32,
) -> impl IntoElement {
    let start_fraction = start_fraction.clamp(0.0, 1.0);
    let end_fraction = end_fraction.clamp(start_fraction, 1.0);
    div()
        .absolute()
        .left(relative(start_fraction))
        .top(px(19.0))
        .h(px(3.0))
        .w(relative(end_fraction - start_fraction))
        .min_w(px(2.0))
        .rounded_full()
        .bg(color)
}

fn progress_track_observer(cx: &Context<PlaybackPage>) -> impl IntoElement {
    let view = cx.entity().downgrade();
    canvas(|bounds, _, _| bounds, {
        let view = view.clone();
        move |_bounds, observed_bounds, window, _app| {
            let view = view.clone();
            window.on_next_frame(move |_, app| {
                let _ = view.update(app, |this, cx| {
                    this.update_progress_track_bounds(observed_bounds, cx);
                });
            });
        }
    })
    .absolute()
    .size_full()
}

fn play_pause_icon_for_user_pause(user_paused: bool) -> &'static str {
    if user_paused {
        "icons/play.svg"
    } else {
        "icons/pause.svg"
    }
}

pub(super) fn cache_status_segments(cache_state: Option<&PlaybackCacheState>) -> Vec<String> {
    let Some(cache_state) = cache_state else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    if let Some(rate) = cache_state.demux.raw_input_rate {
        segments.push(format!("速率 {}/s", format_cache_bytes(rate)));
    }
    if let Some(duration) = cache_state
        .demux
        .cache_duration
        .filter(|duration| duration.is_finite())
    {
        segments.push(format!("Demux {:.1}s", duration.max(0.0)));
    }
    if cache_state.demux.readahead_secs.is_finite() && cache_state.demux.readahead_secs > 0.0 {
        segments.push(format!("目标 {:.1}s", cache_state.demux.readahead_secs));
    }
    if cache_state.demux.cached_range_count > 0 {
        segments.push(format!("Ranges {}", cache_state.demux.cached_range_count));
    }
    for stream in &cache_state.demux.streams {
        let Some(duration) = stream
            .cache_duration
            .filter(|duration| duration.is_finite())
        else {
            continue;
        };
        let label = match stream.kind {
            StreamCacheKind::Video => "V",
            StreamCacheKind::Audio => "A",
            StreamCacheKind::Subtitle => "S",
            StreamCacheKind::Unknown => "?",
        };
        let status = if stream.underrun {
            " 断供"
        } else if stream.idle {
            " 空闲"
        } else {
            ""
        };
        segments.push(format!("{label} {:.1}s{status}", duration.max(0.0)));
    }
    if let Some(byte_cache) = cache_state.byte.as_ref()
        && byte_cache.cached_bytes > 0
    {
        segments.push(format!(
            "Byte {}",
            format_cache_bytes(byte_cache.cached_bytes)
        ));
    }
    if let Some(byte_cache) = cache_state.byte.as_ref()
        && byte_cache.target_readahead_bytes > 0
        && byte_cache.memory_capacity_bytes > 0
    {
        segments.push(format!(
            "Byte 预读 {}/{}",
            format_cache_bytes(byte_cache.target_readahead_bytes),
            format_cache_bytes(byte_cache.memory_capacity_bytes)
        ));
        if byte_cache.prefetch_paused {
            segments.push("Byte 暂停".to_string());
        }
        if byte_cache.retained_range_count > 0 {
            segments.push(format!("Byte ranges {}", byte_cache.retained_range_count));
        }
    }
    let demux_storage = &cache_state.demux.storage;
    let byte_storage = cache_state.byte.as_ref().map(|cache| &cache.storage);
    let memory = demux_storage
        .memory_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.memory_bytes));
    let memory_limit = demux_storage
        .memory_limit_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.memory_limit_bytes));
    if memory > 0 || memory_limit > 0 {
        segments.push(format!(
            "缓存内存 ≈{}/{}",
            format_cache_bytes(memory),
            if demux_storage.memory_limit_bytes == 0 {
                "不限".to_string()
            } else {
                format_cache_bytes(memory_limit)
            }
        ));
    }
    if cache_state.demux.forward_limit_bytes > 0 {
        segments.push(format!(
            "媒体前向 {}/{}",
            format_cache_bytes(cache_state.demux.forward_bytes),
            format_cache_bytes(cache_state.demux.forward_limit_bytes)
        ));
    }
    let disk = demux_storage
        .disk_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.disk_bytes));
    let disk_file = demux_storage
        .disk_file_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.disk_file_bytes));
    let disk_limit = demux_storage
        .disk_limit_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.disk_limit_bytes));
    if disk_file > 0 || disk_limit > 0 {
        segments.push(format!(
            "磁盘有效 {} · 文件 {}/{}",
            format_cache_bytes(disk),
            format_cache_bytes(disk_file),
            format_cache_bytes(disk_limit)
        ));
    }
    let pending = demux_storage
        .disk_pending_bytes
        .saturating_add(byte_storage.map_or(0, |s| s.disk_pending_bytes));
    if pending > 0 {
        segments.push(format!("磁盘待回收 {}", format_cache_bytes(pending)));
    }
    segments.push(if cache_state.demux.idle {
        "状态 空闲".to_string()
    } else {
        "状态 读取".to_string()
    });
    if let Some(percent) = cache_state.buffering_percent {
        segments.push(format!("缓冲 {percent}%"));
    }
    segments.push(format!(
        "Seek {}/{}/{}",
        cache_state.demux.cached_seeks,
        cache_state.demux.low_level_seeks,
        cache_state.demux.byte_level_seeks
    ));
    segments
}

fn format_cache_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

pub(super) fn track_select_option(
    label: impl Into<SharedString>,
    metadata: impl Into<SharedString>,
    selected: bool,
    id: String,
    cx: &Context<PlaybackPage>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::media_overlay(cx);
    let label = label.into();
    let metadata = metadata.into();
    let label_id = format!("{id}-label");
    let metadata_id = format!("{id}-metadata");
    let hover_background = if selected {
        theme.input_border_focused.opacity(0.34)
    } else {
        theme.foreground.opacity(0.12)
    };

    div()
        .id(id.clone())
        .debug_selector(move || id.clone())
        .flex()
        .flex_none()
        .h(px(48.0))
        .min_h(px(48.0))
        .items_center()
        .rounded(radius::CONTROL)
        .px_2()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.foreground
        } else {
            theme.foreground.opacity(0.86)
        })
        .bg(if selected {
            theme.input_border_focused.opacity(0.24)
        } else {
            theme.foreground.opacity(0.0)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_background))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(px(2.0))
                .child(
                    div()
                        .debug_selector(move || label_id.clone())
                        .truncate()
                        .line_height(px(18.0))
                        .child(label),
                )
                .child(
                    div()
                        .debug_selector(move || metadata_id.clone())
                        .truncate()
                        .text_xs()
                        .line_height(px(14.0))
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(theme.foreground.opacity(if selected { 0.9 } else { 0.72 }))
                        .child(metadata),
                ),
        )
}

pub(super) fn valid_frame_rate(frame_rate: f64) -> Option<f64> {
    frame_rate
        .is_finite()
        .then_some(frame_rate)
        .filter(|rate| *rate > 0.0)
}

#[cfg(test)]
mod tests {
    use crate::player::page::state::user_pause_from_effective_pause_event;
    use tiny_playback::{ByteCacheState, DemuxCacheState, PlaybackCacheState, StreamCacheState};

    use super::*;

    #[test]
    fn cache_status_segments_include_compact_cache_metrics() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_duration: Some(2.25),
                idle: true,
                storage: tiny_playback::CacheStorageState {
                    memory_bytes: 2 * 1024 * 1024,
                    memory_limit_bytes: 8 * 1024 * 1024,
                    disk_bytes: 3 * 1024 * 1024,
                    disk_file_bytes: 4 * 1024 * 1024,
                    disk_limit_bytes: 16 * 1024 * 1024,
                    ..Default::default()
                },
                raw_input_rate: Some(1536),
                cached_seeks: 1,
                low_level_seeks: 2,
                byte_level_seeks: 3,
                streams: vec![
                    StreamCacheState {
                        kind: StreamCacheKind::Video,
                        cache_end: Some(3.0),
                        reader_pts: Some(1.0),
                        cache_duration: Some(2.0),
                        underrun: false,
                        idle: true,
                    },
                    StreamCacheState {
                        kind: StreamCacheKind::Audio,
                        cache_end: Some(2.5),
                        reader_pts: Some(1.0),
                        cache_duration: Some(1.5),
                        underrun: true,
                        idle: false,
                    },
                ],
                ..DemuxCacheState::default()
            },
            byte: Some(ByteCacheState {
                ranges: Vec::new(),
                reader_fraction: None,
                download_fraction: None,
                cached_bytes: 8 * 1024,
                content_length: Some(64 * 1024),
                disk_cache_enabled: true,
                idle: true,
                raw_input_rate: Some(1536),
                byte_level_seeks: 3,
                ..ByteCacheState::default()
            }),
            paused_for_cache: true,
            buffering_percent: Some(42),
        };

        assert_eq!(
            cache_status_segments(Some(&state)),
            vec![
                "速率 1.5 KiB/s".to_string(),
                "Demux 2.2s".to_string(),
                "V 2.0s 空闲".to_string(),
                "A 1.5s 断供".to_string(),
                "Byte 8.0 KiB".to_string(),
                "缓存内存 ≈2.0 MiB/8.0 MiB".to_string(),
                "磁盘有效 3.0 MiB · 文件 4.0 MiB/16.0 MiB".to_string(),
                "状态 空闲".to_string(),
                "缓冲 42%".to_string(),
                "Seek 1/2/3".to_string(),
            ]
        );
        assert!(cache_status_segments(None).is_empty());
    }

    #[test]
    fn cache_status_segments_expose_effective_prefetch_pressure() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                readahead_secs: 1.25,
                cached_range_count: 3,
                ..DemuxCacheState::default()
            },
            byte: Some(ByteCacheState {
                target_readahead_bytes: 2 * 1024 * 1024,
                memory_capacity_bytes: 8 * 1024 * 1024,
                prefetch_paused: true,
                retained_range_count: 2,
                ..ByteCacheState::default()
            }),
            ..PlaybackCacheState::default()
        };
        let segments = cache_status_segments(Some(&state));

        assert!(segments.iter().any(|segment| segment == "目标 1.2s"));
        assert!(segments.iter().any(|segment| segment == "Ranges 3"));
        assert!(
            segments
                .iter()
                .any(|segment| segment == "Byte 预读 2.0 MiB/8.0 MiB")
        );
        assert!(segments.iter().any(|segment| segment == "Byte 暂停"));
        assert!(segments.iter().any(|segment| segment == "Byte ranges 2"));
    }

    #[test]
    fn play_pause_icon_reflects_user_pause_not_cache_pause() {
        assert_eq!(play_pause_icon_for_user_pause(true), "icons/play.svg");
        assert_eq!(play_pause_icon_for_user_pause(false), "icons/pause.svg");
        assert!(effective_playback_paused(false, true));
        assert!(!effective_playback_paused(false, false));
        assert!(!user_pause_from_effective_pause_event(false, true, false));
        assert!(!user_pause_from_effective_pause_event(false, true, true));
        assert!(user_pause_from_effective_pause_event(false, false, true));
    }

    #[test]
    fn playback_volume_helpers_clamp_and_scale_scroll() {
        assert_eq!(playback_volume_percent(-0.5), 0);
        assert_eq!(playback_volume_percent(0.525), 52);
        assert_eq!(playback_volume_percent(f32::NAN), 100);
        assert_eq!(playback_volume_percent(1.5), 100);
        assert_eq!(
            volume_delta_from_scroll_delta(ScrollDelta::Lines(gpui::Point { x: 0.0, y: 3.0 })),
            0.02
        );
        assert_eq!(
            volume_delta_from_scroll_delta(ScrollDelta::Pixels(gpui::Point {
                x: px(0.0),
                y: px(-1000.0),
            })),
            -0.2
        );
    }

    #[test]
    fn linux_wheel_detents_change_volume_by_two_percent() {
        // GPUI reports three lines for one physical notch, including on X11 and Wayland.
        for (lines, expected) in [
            (3.0, 0.02),
            (-3.0, -0.02),
            (6.0, 0.04),
            (-6.0, -0.04),
            (1.5, 0.01),
            (0.0, 0.0),
            (90.0, 0.2),
            (-90.0, -0.2),
        ] {
            let delta = ScrollDelta::Lines(gpui::Point { x: 3.0, y: lines });
            assert_eq!(volume_delta_from_scroll_delta(delta), expected);
        }
    }

    #[test]
    fn continuous_scroll_keeps_proportional_volume_adjustment() {
        for (pixels, expected) in [(25.0, 0.02), (-25.0, -0.02), (12.5, 0.01), (0.0, 0.0)] {
            let pixel_delta = ScrollDelta::Pixels(gpui::Point {
                x: px(25.0),
                y: px(pixels),
            });
            assert_eq!(volume_delta_from_scroll_delta(pixel_delta), expected);
        }
    }
}
