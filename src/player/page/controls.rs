use super::fullscreen::{
    PLAYBACK_BACK_BUTTON_OFFSET_PX, PLAYBACK_BACK_BUTTON_SIZE_PX,
    PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX, PLAYBACK_PROGRESS_BAR_HEIGHT_PX,
};
use super::state::effective_playback_paused;
use super::*;

const TRACK_SELECT_MENU_MAX_HEIGHT_PX: f32 = 260.0;
const VOLUME_INDICATOR_HIDE_DELAY: Duration = Duration::from_millis(1200);
const VOLUME_INDICATOR_BAR_HEIGHT_PX: f32 = 192.0;
const PLAYBACK_DETAILS_WIDTH_PX: f32 = 500.0;
const PLAYBACK_DETAILS_TOP_PX: f32 = 56.0;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackDetailRow {
    label: String,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackDetailSection {
    title: &'static str,
    summary: String,
    rows: Vec<PlaybackDetailRow>,
}

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
    cache_ranges: Vec<(f32, f32)>,
}

#[path = "controls/progress.rs"]
mod progress;
#[path = "controls/render.rs"]
mod render;
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

impl PlaybackDetailRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

fn playback_detail_section_element<T>(
    section: PlaybackDetailSection,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::media_overlay(cx);

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(format!("{}:", section.title)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(section.summary),
                ),
        )
        .children(section.rows.into_iter().map(|row| {
            div()
                .flex()
                .items_start()
                .gap_2()
                .pl_2()
                .child(
                    div()
                        .flex_none()
                        .w(px(112.0))
                        .text_color(theme.muted_foreground)
                        .child(format!("{}:", row.label)),
                )
                .child(div().min_w_0().flex_1().child(row.value))
        }))
}

fn playback_file_detail_section(
    title: &str,
    content_length: Option<u64>,
    protocol: Option<&str>,
    file_info: Option<&PlaybackFileInfo>,
    duration: Option<f64>,
    cache_state: Option<&PlaybackCacheState>,
) -> PlaybackDetailSection {
    let mut rows = Vec::new();
    let content_length = content_length.or_else(|| {
        cache_state.and_then(|state| state.byte.as_ref().and_then(|cache| cache.content_length))
    });
    if let Some(size) = content_length.filter(|size| *size > 0) {
        rows.push(PlaybackDetailRow::new("Size", format_cache_bytes(size)));
    }
    if let Some(format_protocol) = playback_format_protocol(file_info, protocol) {
        rows.push(PlaybackDetailRow::new("Format/Protocol", format_protocol));
    }
    if let Some(duration) = duration.filter(|duration| duration.is_finite() && *duration > 0.0) {
        rows.push(PlaybackDetailRow::new(
            "Duration",
            format_playback_time(duration),
        ));
    }
    if let Some(bitrate) = file_info.and_then(|info| info.bitrate) {
        rows.push(PlaybackDetailRow::new(
            "Overall Bitrate",
            format_bitrate(bitrate),
        ));
    }
    if let Some(total_cache) = playback_total_cache(cache_state) {
        rows.push(PlaybackDetailRow::new("Total Cache", total_cache));
    }

    PlaybackDetailSection {
        title: "File",
        summary: title.to_string(),
        rows,
    }
}

fn playback_display_detail_section(
    display_size: RenderSize,
    output_size: Option<RenderSize>,
    presenter: Option<VideoPresenterSnapshot>,
) -> PlaybackDetailSection {
    let mut rows = vec![
        PlaybackDetailRow::new("Context", "libplacebo / Vulkan"),
        PlaybackDetailRow::new("Resolution", format_render_size(display_size)),
    ];
    if let Some(output_size) = output_size {
        rows.push(PlaybackDetailRow::new(
            "Output Resolution",
            format_render_size(output_size),
        ));
    }
    if let Some(presenter) = presenter {
        rows.push(PlaybackDetailRow::new(
            "Dropped Frames",
            presenter.dropped_frames.to_string(),
        ));
        rows.push(PlaybackDetailRow::new(
            "Frame Queue",
            format!("{} / {}", presenter.queued, presenter.queue_capacity),
        ));
        if presenter.average_render_ms.is_finite() && presenter.average_render_ms > 0.0 {
            rows.push(PlaybackDetailRow::new(
                "Render Time",
                format!("{:.2} ms", presenter.average_render_ms),
            ));
        }
    }

    PlaybackDetailSection {
        title: "Display",
        summary: "GPUI video output".to_string(),
        rows,
    }
}

fn playback_video_detail_section(
    info: Option<&PlaybackVideoInfo>,
    loaded: bool,
) -> PlaybackDetailSection {
    let Some(info) = info else {
        return PlaybackDetailSection {
            title: "Video",
            summary: if loaded { "Unavailable" } else { "Loading…" }.to_string(),
            rows: Vec::new(),
        };
    };

    let mut rows = vec![PlaybackDetailRow::new("Decoder", info.decoder.clone())];
    rows.push(PlaybackDetailRow::new(
        "Decode Mode",
        if info.hardware_accelerated {
            "Vulkan HW"
        } else {
            "Software"
        },
    ));
    if let Some(frame_rate) = info.frame_rate.and_then(valid_frame_rate) {
        rows.push(PlaybackDetailRow::new(
            "Frame Rate",
            format!("{frame_rate:.3} fps"),
        ));
    }
    let mut resolution = format_render_size(info.size);
    if let Some((numerator, denominator)) = info
        .sample_aspect_ratio
        .filter(|(numerator, denominator)| *numerator != *denominator)
    {
        resolution.push_str(&format!("  SAR {numerator}:{denominator}"));
    }
    rows.push(PlaybackDetailRow::new("Resolution", resolution));
    push_optional_detail(&mut rows, "Format", info.pixel_format.as_deref());
    push_optional_detail(&mut rows, "Levels", info.color_range.as_deref());
    push_optional_detail(&mut rows, "Chroma Loc", info.chroma_location.as_deref());
    push_optional_detail(&mut rows, "Colormatrix", info.color_space.as_deref());
    push_optional_detail(&mut rows, "Primaries", info.color_primaries.as_deref());
    push_optional_detail(&mut rows, "Transfer", info.color_transfer.as_deref());
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }

    PlaybackDetailSection {
        title: "Video",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
        ),
        rows,
    }
}

fn playback_audio_detail_section(
    info: Option<&PlaybackAudioInfo>,
    loaded: bool,
    volume: f32,
) -> PlaybackDetailSection {
    let Some(info) = info else {
        return PlaybackDetailSection {
            title: "Audio",
            summary: if loaded { "No audio" } else { "Loading…" }.to_string(),
            rows: Vec::new(),
        };
    };

    let mut rows = vec![PlaybackDetailRow::new("Decoder", info.decoder.clone())];
    let has_audio_output = info.output_device.is_some()
        || info.output_channels.is_some()
        || info.output_sample_format.is_some()
        || info.output_sample_rate.is_some();
    if has_audio_output {
        rows.push(PlaybackDetailRow::new("AO", "cpal"));
        push_optional_detail(&mut rows, "Device", info.output_device.as_deref());
        rows.push(PlaybackDetailRow::new(
            "AO Volume",
            format!("{}%", playback_volume_percent(volume)),
        ));
    }

    let input_channels = audio_channel_description(info.channels, info.channel_layout.as_deref());
    let output_channels = info.output_channels.map(|channels| channels.to_string());
    if let Some(channels) = transition_value(input_channels, output_channels) {
        rows.push(PlaybackDetailRow::new("Channels", channels));
    }
    if let Some(format) = transition_value(
        info.sample_format.clone(),
        info.output_sample_format.clone(),
    ) {
        rows.push(PlaybackDetailRow::new("Format", format));
    }
    if let Some(sample_rate) = transition_value(
        info.sample_rate.map(|rate| rate.to_string()),
        info.output_sample_rate.map(|rate| rate.to_string()),
    ) {
        rows.push(PlaybackDetailRow::new(
            "Sample Rate",
            format!("{sample_rate} Hz"),
        ));
    }
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }

    PlaybackDetailSection {
        title: "Audio",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
        ),
        rows,
    }
}

fn push_optional_detail(rows: &mut Vec<PlaybackDetailRow>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        rows.push(PlaybackDetailRow::new(label, value));
    }
}

fn playback_format_protocol(
    file_info: Option<&PlaybackFileInfo>,
    protocol: Option<&str>,
) -> Option<String> {
    let format = file_info.and_then(|info| {
        match (
            info.format_description.as_deref(),
            info.format_name.as_deref(),
        ) {
            (Some(description), Some(name)) if !description.eq_ignore_ascii_case(name) => {
                Some(format!("{description} ({name})"))
            }
            (Some(description), _) => Some(description.to_string()),
            (_, Some(name)) => Some(name.to_string()),
            _ => None,
        }
    });
    transition_value(format, protocol.map(ToString::to_string))
        .map(|value| value.replace(" → ", " / "))
}

fn playback_total_cache(cache_state: Option<&PlaybackCacheState>) -> Option<String> {
    let cache_state = cache_state?;
    let bytes = cache_state.demux.forward_bytes;
    let duration = cache_state
        .demux
        .cache_duration
        .filter(|duration| duration.is_finite() && *duration > 0.0);
    if bytes == 0 && duration.is_none() {
        return None;
    }
    match (bytes > 0, duration) {
        (true, Some(duration)) => {
            Some(format!("{} ({duration:.1} sec)", format_cache_bytes(bytes)))
        }
        (true, None) => Some(format_cache_bytes(bytes)),
        (false, Some(duration)) => Some(format!("{duration:.1} sec")),
        (false, None) => None,
    }
}

fn codec_summary(codec: &str, description: Option<&str>, profile: Option<&str>) -> String {
    let mut summary = description
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .unwrap_or(codec)
        .to_string();
    if let Some(profile) = profile.map(str::trim).filter(|profile| !profile.is_empty()) {
        summary.push_str(&format!(" [{profile}]"));
    }
    summary
}

fn audio_channel_description(channels: Option<u32>, layout: Option<&str>) -> Option<String> {
    match (
        channels,
        layout.map(str::trim).filter(|layout| !layout.is_empty()),
    ) {
        (Some(channels), Some(layout)) => Some(format!("{layout} ({channels})")),
        (Some(channels), None) => Some(channels.to_string()),
        (None, Some(layout)) => Some(layout.to_string()),
        (None, None) => None,
    }
}

fn transition_value(input: Option<String>, output: Option<String>) -> Option<String> {
    match (input, output) {
        (Some(input), Some(output)) if input != output => Some(format!("{input} → {output}")),
        (Some(input), _) => Some(input),
        (None, Some(output)) => Some(output),
        (None, None) => None,
    }
}

fn format_render_size(size: RenderSize) -> String {
    format!("{}x{}", size.width, size.height)
}

fn format_bitrate(bits_per_second: u64) -> String {
    const KILOBIT: f64 = 1_000.0;
    const MEGABIT: f64 = 1_000_000.0;
    const GIGABIT: f64 = 1_000_000_000.0;
    let bitrate = bits_per_second as f64;
    if bitrate >= GIGABIT {
        format!("{:.2} Gb/s", bitrate / GIGABIT)
    } else if bitrate >= MEGABIT {
        format!("{:.2} Mb/s", bitrate / MEGABIT)
    } else if bitrate >= KILOBIT {
        format!("{:.1} kb/s", bitrate / KILOBIT)
    } else {
        format!("{bits_per_second} b/s")
    }
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
    selected: bool,
    cx: &Context<PlaybackPage>,
) -> gpui::Div {
    let theme = theme::media_overlay(cx);
    let hover_background = if selected {
        theme.input_border_focused.opacity(0.34)
    } else {
        theme.foreground.opacity(0.12)
    };

    div()
        .flex()
        .flex_none()
        .h(px(32.0))
        .min_h(px(32.0))
        .items_center()
        .rounded(px(6.0))
        .px_1()
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
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

pub(super) fn valid_frame_rate(frame_rate: f64) -> Option<f64> {
    frame_rate
        .is_finite()
        .then_some(frame_rate)
        .filter(|rate| *rate > 0.0)
}

#[cfg(test)]
mod tests {
    use crate::player::{
        backend::{
            ByteCacheState, DemuxCacheState, PlaybackAudioInfo, PlaybackCacheState,
            PlaybackFileInfo, PlaybackVideoInfo, StreamCacheState,
        },
        page::state::user_pause_from_effective_pause_event,
        render_host::RenderSize,
    };

    use super::*;

    #[test]
    fn video_detail_section_contains_mpv_style_status() {
        let info = PlaybackVideoInfo {
            codec: "hevc".to_string(),
            codec_description: Some("HEVC (High Efficiency Video Coding)".to_string()),
            profile: Some("Main 10".to_string()),
            decoder: "hevc".to_string(),
            size: RenderSize {
                width: 3840,
                height: 2160,
            },
            sample_aspect_ratio: Some((1, 1)),
            frame_rate: Some(23.976),
            pixel_format: Some("yuv420p10le".to_string()),
            color_range: Some("tv".to_string()),
            chroma_location: Some("left".to_string()),
            color_space: Some("bt2020nc".to_string()),
            color_primaries: Some("bt2020".to_string()),
            color_transfer: Some("smpte2084".to_string()),
            bitrate: Some(18_500_000),
            hardware_accelerated: true,
        };

        let section = playback_video_detail_section(Some(&info), true);

        assert_eq!(section.title, "Video");
        assert_eq!(
            section.summary,
            "HEVC (High Efficiency Video Coding) [Main 10]"
        );
        assert_eq!(detail_row_value(&section, "Decoder"), Some("hevc"));
        assert_eq!(detail_row_value(&section, "Decode Mode"), Some("Vulkan HW"));
        assert_eq!(detail_row_value(&section, "Frame Rate"), Some("23.976 fps"));
        assert_eq!(detail_row_value(&section, "Resolution"), Some("3840x2160"));
        assert_eq!(detail_row_value(&section, "Format"), Some("yuv420p10le"));
        assert_eq!(detail_row_value(&section, "Colormatrix"), Some("bt2020nc"));
        assert_eq!(detail_row_value(&section, "Bitrate"), Some("18.50 Mb/s"));
    }

    #[test]
    fn audio_detail_section_reports_input_to_output_transforms() {
        let info = PlaybackAudioInfo {
            codec: "eac3".to_string(),
            codec_description: Some("ATSC A/52B (AC-3, E-AC-3)".to_string()),
            profile: None,
            decoder: "eac3".to_string(),
            channels: Some(6),
            channel_layout: Some("5.1(side)".to_string()),
            sample_format: Some("fltp".to_string()),
            sample_rate: Some(48_000),
            output_channels: Some(2),
            output_sample_format: Some("f32".to_string()),
            output_sample_rate: Some(48_000),
            output_device: Some("Built-in Audio".to_string()),
            bitrate: Some(768_000),
        };

        let section = playback_audio_detail_section(Some(&info), true, 0.75);

        assert_eq!(section.title, "Audio");
        assert_eq!(detail_row_value(&section, "AO"), Some("cpal"));
        assert_eq!(detail_row_value(&section, "AO Volume"), Some("75%"));
        assert_eq!(
            detail_row_value(&section, "Channels"),
            Some("5.1(side) (6) → 2")
        );
        assert_eq!(detail_row_value(&section, "Format"), Some("fltp → f32"));
        assert_eq!(detail_row_value(&section, "Sample Rate"), Some("48000 Hz"));
        assert_eq!(detail_row_value(&section, "Bitrate"), Some("768.0 kb/s"));
    }

    #[test]
    fn file_detail_section_combines_format_protocol_and_cache() {
        let file_info = PlaybackFileInfo {
            format_name: Some("matroska,webm".to_string()),
            format_description: Some("Matroska / WebM".to_string()),
            bitrate: Some(20_000_000),
        };
        let cache_state = PlaybackCacheState {
            demux: DemuxCacheState {
                forward_bytes: 32 * 1024 * 1024,
                cache_duration: Some(12.5),
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        let section = playback_file_detail_section(
            "示例视频",
            Some(2 * 1024 * 1024 * 1024),
            Some("https"),
            Some(&file_info),
            Some(3661.0),
            Some(&cache_state),
        );

        assert_eq!(section.title, "File");
        assert_eq!(section.summary, "示例视频");
        assert_eq!(detail_row_value(&section, "Size"), Some("2.0 GiB"));
        assert_eq!(
            detail_row_value(&section, "Format/Protocol"),
            Some("Matroska / WebM (matroska,webm) / https")
        );
        assert_eq!(detail_row_value(&section, "Duration"), Some("1:01:01"));
        assert_eq!(
            detail_row_value(&section, "Total Cache"),
            Some("32.0 MiB (12.5 sec)")
        );
    }

    #[test]
    fn playback_detail_helpers_reject_invalid_frame_rates() {
        assert_eq!(valid_frame_rate(0.0), None);
        assert_eq!(valid_frame_rate(f64::INFINITY), None);
        assert_eq!(valid_frame_rate(60.0), Some(60.0));
    }

    fn detail_row_value<'a>(section: &'a PlaybackDetailSection, label: &str) -> Option<&'a str> {
        section
            .rows
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.value.as_str())
    }

    #[test]
    fn cache_status_segments_include_compact_cache_metrics() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_duration: Some(2.25),
                idle: true,
                storage: crate::player::backend::CacheStorageState {
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
            volume_delta_from_scroll_delta(ScrollDelta::Lines(gpui::Point { x: 0.0, y: 1.0 })),
            0.05
        );
        assert_eq!(
            volume_delta_from_scroll_delta(ScrollDelta::Pixels(gpui::Point {
                x: px(0.0),
                y: px(-250.0),
            })),
            -0.2
        );
    }
}
