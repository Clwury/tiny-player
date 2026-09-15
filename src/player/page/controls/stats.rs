use std::ops::Range;

use super::*;

const PLAYBACK_DETAILS_WIDTH_PX: f32 = 1100.0;
const PLAYBACK_DETAILS_TOP_PX: f32 = 56.0;

// Field order and grouping follow mpv's player/lua/stats.lua, page 1.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackDetailRow {
    label: String,
    value: String,
    inline: bool,
}

impl PlaybackDetailRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            inline: false,
        }
    }

    fn inline(mut self) -> Self {
        self.inline = true;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PlaybackDetailSection {
    title: &'static str,
    summary: String,
    rows: Vec<PlaybackDetailRow>,
}

#[derive(Default)]
struct PlaybackDetailLine {
    text: String,
    labels: Vec<Range<usize>>,
}

impl PlaybackDetailLine {
    fn push(&mut self, label: &str, value: &str) {
        if !self.text.is_empty() {
            self.text.push_str("    ");
        }
        let start = self.text.len();
        self.text.push_str(label);
        self.text.push(':');
        self.labels.push(start..self.text.len());
        self.text.push_str("  ");
        self.text.push_str(value);
    }

    fn styled_text(&self) -> gpui::StyledText {
        gpui::StyledText::new(self.text.clone()).with_highlights(self.labels.iter().map(|range| {
            (
                range.clone(),
                gpui::HighlightStyle {
                    font_weight: Some(gpui::FontWeight::BOLD),
                    ..Default::default()
                },
            )
        }))
    }
}

impl PlaybackDetailSection {
    fn lines(&self) -> Vec<PlaybackDetailLine> {
        let mut header = PlaybackDetailLine::default();
        header.push(self.title, &self.summary);
        let mut lines = vec![header];
        for row in &self.rows {
            if !row.inline {
                lines.push(PlaybackDetailLine::default());
            }
            lines.last_mut().unwrap().push(&row.label, &row.value);
        }
        lines
    }
}

fn playback_detail_section_element(section: PlaybackDetailSection) -> impl IntoElement {
    div()
        .debug_selector(move || format!("playback-stats-{}", section.title))
        .flex()
        .flex_col()
        .flex_none()
        .children(
            section
                .lines()
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    // Outline only the glyphs so stats remain legible over bright video
                    // without putting a translucent panel over the picture.
                    div().relative().when(index > 0, |this| this.ml_5()).child(
                        div()
                            .relative()
                            .children([(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)].map(
                                |(x, y)| {
                                    div()
                                        .absolute()
                                        .left(px(x))
                                        .top(px(y))
                                        .w_full()
                                        .text_color(rgb(0x000000))
                                        .child(line.styled_text())
                                },
                            ))
                            .child(line.styled_text()),
                    )
                }),
        )
}

pub(super) fn playback_details_overlay(
    sections: impl IntoIterator<Item = Option<PlaybackDetailSection>>,
    window: &Window,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("playback-details-overlay")
        .debug_selector(|| "playback-details-overlay".to_string())
        .absolute()
        .left_4()
        .top(px(if window.is_fullscreen() {
            16.0
        } else {
            PLAYBACK_DETAILS_TOP_PX
        }))
        .flex()
        .flex_col()
        .w(px(PLAYBACK_DETAILS_WIDTH_PX))
        .max_w(relative(0.95))
        .max_h(relative(if window.is_fullscreen() { 0.92 } else { 0.82 }))
        .gap_3()
        .overflow_y_scroll()
        .p_1()
        .text_sm()
        .line_height(px(20.0))
        .text_color(rgb(0xffffff))
        // Pointer presses and movement reach the full-window playback surface.
        // Keep wheel events here so scrolling stats does not change the volume.
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .children(
            sections
                .into_iter()
                .flatten()
                .map(playback_detail_section_element),
        )
}

fn playback_filename(source: &str) -> String {
    if let Ok(url) = url::Url::parse(source) {
        // Find the basename within the path so '/' in a query value cannot
        // replace the filename. Keep the original query and fragment intact.
        let path = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy();
        let mut filename = path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&path)
            .to_string();
        if let Some(index) = source.find(['?', '#']) {
            filename.push_str(&source[index..]);
        }
        return filename;
    }
    std::path::Path::new(source)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.to_string())
}

pub(super) fn playback_file_detail_section(
    source: &str,
    title: &str,
    content_length: Option<u64>,
    file_info: Option<&PlaybackFileInfo>,
    cache_state: Option<&PlaybackCacheState>,
) -> PlaybackDetailSection {
    let filename = playback_filename(source);
    let mut rows = Vec::new();
    if !title.is_empty() && title != filename {
        rows.push(PlaybackDetailRow::new("Title", title));
    }
    let content_length = cache_state
        .and_then(|state| state.byte.as_ref().and_then(|cache| cache.content_length))
        .or(content_length);
    if let Some(size) = content_length {
        rows.push(PlaybackDetailRow::new("Size", format_file_size(size)));
    }
    // file-format uses the demuxer's filetype, or its short name. mpv selects
    // its native Matroska demuxer before lavf; other lavf formats keep .name.
    if let Some(format) = file_info.and_then(|info| non_empty(info.format_name.as_deref())) {
        let format = match format {
            "matroska,webm" | "matroska" | "webm" => "mkv",
            other => other,
        };
        let row = PlaybackDetailRow::new("Format/Protocol", format);
        rows.push(if content_length.is_some() {
            row.inline()
        } else {
            row
        });
    }
    if let Some(total_cache) = playback_total_cache(cache_state) {
        rows.push(PlaybackDetailRow::new("Total Cache", total_cache));
    }
    PlaybackDetailSection {
        title: "File",
        summary: filename,
        rows,
    }
}

pub(super) fn playback_display_detail_section(
    display_size: RenderSize,
    presenter: VideoPresenterSnapshot,
) -> PlaybackDetailSection {
    PlaybackDetailSection {
        title: "Display",
        summary: "gpui".to_string(),
        rows: vec![
            PlaybackDetailRow::new("Context", "vulkan").inline(),
            PlaybackDetailRow::new(
                "Dropped Frames",
                format!("{} (output)", presenter.dropped_frames),
            ),
            PlaybackDetailRow::new("Resolution", format_render_size(display_size)),
        ],
    }
}

pub(super) fn playback_video_detail_section(
    info: Option<&PlaybackVideoInfo>,
) -> Option<PlaybackDetailSection> {
    let info = info?;
    let mut rows = Vec::new();
    if info.hardware_accelerated {
        rows.push(PlaybackDetailRow::new("HW", "vulkan").inline());
    }
    if let Some(frame_rate) = info
        .frame_rate
        .and_then(valid_frame_rate)
        .filter(|rate| *rate >= 0.1)
    {
        // container-fps is a float, printed with four decimals and trailing zeros
        // removed by mpv's options/m_option.c. We have no estimated-vf-fps yet.
        rows.push(PlaybackDetailRow::new(
            "Frame Rate",
            format!(
                "{} fps (specified)",
                format_decimal(f64::from(frame_rate as f32))
            ),
        ));
    }
    rows.push(PlaybackDetailRow::new(
        "Resolution",
        format_render_size(info.size),
    ));
    if let Some(size) = video_display_size(info).filter(|size| *size != info.size) {
        rows.push(PlaybackDetailRow::new(
            "Output Resolution",
            format_render_size(size),
        ));
    }
    push_detail_group(
        &mut rows,
        &[
            (
                "Format",
                info.pixel_format.as_deref().map(pixel_format_name),
            ),
            ("Levels", info.color_range.as_deref().map(color_levels_name)),
            (
                "Chroma Loc",
                info.chroma_location.as_deref().map(chroma_location_name),
            ),
        ],
    );
    push_detail_group(
        &mut rows,
        &[
            (
                "Colormatrix",
                info.color_space
                    .as_deref()
                    .map(|space| color_matrix_name(space, info.color_transfer.as_deref())),
            ),
            (
                "Primaries",
                info.color_primaries.as_deref().map(color_primaries_name),
            ),
            (
                "Transfer",
                info.color_transfer.as_deref().map(color_transfer_name),
            ),
        ],
    );
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }
    Some(PlaybackDetailSection {
        title: "Video",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
            &info.decoder,
        ),
        rows,
    })
}

pub(super) fn playback_audio_detail_section(
    info: Option<&PlaybackAudioInfo>,
    volume: f32,
) -> Option<PlaybackDetailSection> {
    let info = info?;
    let mut rows = Vec::new();
    let has_audio_output = info.output_device.is_some()
        || info.output_channels.is_some()
        || info.output_sample_format.is_some()
        || info.output_sample_rate.is_some();
    if has_audio_output {
        rows.push(PlaybackDetailRow::new("AO", "cpal").inline());
        let device = non_empty(info.output_device.as_deref());
        if let Some(device) = device {
            rows.push(PlaybackDetailRow::new("Device", device));
        }
        let mute = if volume <= f32::EPSILON {
            " (Muted)"
        } else {
            ""
        };
        let row = PlaybackDetailRow::new(
            "AO Volume",
            format!("{}%{mute}", playback_volume_percent(volume)),
        );
        rows.push(if device.is_some() { row.inline() } else { row });
    }
    let channels = transition_value(
        info.channels.map(|n| n.to_string()),
        info.output_channels.map(|n| n.to_string()),
    );
    let format = transition_value(
        info.sample_format
            .as_deref()
            .map(|format| audio_format_name(format).to_string()),
        info.output_sample_format
            .as_deref()
            .map(|format| audio_format_name(format).to_string()),
    );
    push_detail_group(
        &mut rows,
        &[
            ("Channels", channels.as_deref()),
            ("Format", format.as_deref()),
        ],
    );
    if let Some(rate) = transition_value(
        info.sample_rate.map(|n| n.to_string()),
        info.output_sample_rate.map(|n| n.to_string()),
    ) {
        rows.push(PlaybackDetailRow::new("Sample Rate", format!("{rate} Hz")));
    }
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }
    Some(PlaybackDetailSection {
        title: "Audio",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
            &info.decoder,
        ),
        rows,
    })
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn push_detail_group(rows: &mut Vec<PlaybackDetailRow>, fields: &[(&str, Option<&str>)]) {
    let mut inline = false;
    for (label, value) in fields {
        if let Some(value) = non_empty(*value) {
            let row = PlaybackDetailRow::new(*label, value);
            rows.push(if inline { row.inline() } else { row });
            inline = true;
        }
    }
}

fn playback_total_cache(cache_state: Option<&PlaybackCacheState>) -> Option<String> {
    let cache = &cache_state?.demux;
    let duration = cache
        .cache_duration
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(0.0);
    (cache.forward_bytes > 0 || duration > 0.0).then(|| {
        format!(
            "{}  ({duration:.1} sec)",
            format_stats_cache_bytes(cache.forward_bytes)
        )
    })
}

fn codec_summary(
    codec: &str,
    description: Option<&str>,
    profile: Option<&str>,
    decoder: &str,
) -> String {
    let mut summary = non_empty(description).unwrap_or(codec).to_string();
    if let Some(profile) = non_empty(profile) {
        summary.push_str(&format!(" [{profile}]"));
    }
    if let Some(decoder) = non_empty(Some(decoder)).filter(|decoder| *decoder != codec) {
        summary.push_str(&format!(" [{decoder}]"));
    }
    summary
}

fn transition_value(input: Option<String>, output: Option<String>) -> Option<String> {
    match (input, output) {
        (Some(input), Some(output)) if input != output => Some(format!("{input} ➜ {output}")),
        (Some(input), _) => Some(input),
        (None, output) => output,
    }
}

fn format_decimal(value: f64) -> String {
    format!("{value:.4}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn format_render_size(size: RenderSize) -> String {
    let mut result = format!("{} x {}", size.width, size.height);
    if size.height > 0 && size.width > 0 {
        let aspect = f64::from(size.width) / f64::from(size.height);
        result.push_str(&format!("  {aspect:.2}:1"));
        if let Some(name) = aspect_ratio_name(aspect) {
            result.push_str(&format!(" ({name})"));
        }
    }
    result
}

fn video_display_size(info: &PlaybackVideoInfo) -> Option<RenderSize> {
    let (num, den) = info.sample_aspect_ratio?;
    if num == 0 || den == 0 {
        return None;
    }
    let mut size = info.size;
    if num > den {
        size.width = (u64::from(size.width) * u64::from(num) / u64::from(den))
            .clamp(1, i32::MAX as u64) as u32;
    } else if den > num {
        size.height = (u64::from(size.height) * u64::from(den) / u64::from(num))
            .clamp(1, i32::MAX as u64) as u32;
    }
    Some(size)
}

fn aspect_ratio_name(aspect: f64) -> Option<&'static str> {
    // Match command.c:get_aspect_ratio_name, including its first-match order.
    [
        (9.0 / 16.0, "Vertical"),
        (1.0, "Square"),
        (19.0 / 16.0, "Movietone Ratio"),
        (5.0 / 4.0, "5:4"),
        (4.0 / 3.0, "4:3"),
        (11.0 / 8.0, "Academy Ratio"),
        (1.43, "IMAX Ratio"),
        (3.0 / 2.0, "VistaVision Ratio"),
        (16.0 / 10.0, "16:10"),
        (5.0 / 3.0, "35mm Widescreen Ratio"),
        (16.0 / 9.0, "16:9"),
        (7.0 / 4.0, "Early 35mm Widescreen Ratio"),
        (1.85, "Academy Flat"),
        (256.0 / 135.0, "SMPTE/DCI Ratio"),
        (2.0, "Univisium"),
        (2.208, "70mm film"),
        (2.35, "Scope"),
        (2.39, "Panavision"),
        (2.55, "Original CinemaScope"),
        (2.59, "Full-frame Cinerama"),
        (24.0 / 9.0, "Full-frame Super 16mm"),
        (2.76, "Ultra Panavision 70"),
        (32.0 / 9.0, "32:9"),
        (3.6, "Ultra-WideScreen 3.6"),
        (4.0, "Polyvision"),
        (12.0, "Circle-Vision 360°"),
    ]
    .into_iter()
    .find(|(reference, _)| (aspect - reference).abs() < 0.025)
    .map(|(_, name)| name)
}

fn format_file_size(bytes: u64) -> String {
    // options/m_option.c:format_file_size uses three decimals, capped at TiB.
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["KiB", "MiB", "GiB", "TiB"] {
        value /= 1024.0;
        if value < 1024.0 || unit == "TiB" {
            return format!("{value:.3} {unit}");
        }
    }
    unreachable!()
}

fn format_stats_cache_bytes(bytes: u64) -> String {
    // player/lua/defaults.lua:format_bytes_humanized is intentionally different.
    let units = ["Bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 {
        value /= 1024.0;
        index += 1;
    }
    let unit = units
        .get(index)
        .map(|unit| unit.to_string())
        .unwrap_or_else(|| format!("*1024^{index}"));
    format!("{value:.2} {unit}")
}

fn format_bitrate(bits_per_second: u64) -> String {
    // command.c:mp_property_packet_bitrate (no switch to Gbps).
    if bits_per_second < 1_000_000 {
        format!("{:.0} kbps", bits_per_second as f64 / 1000.0)
    } else {
        format!("{:.3} Mbps", bits_per_second as f64 / 1_000_000.0)
    }
}

fn pixel_format_name(format: &str) -> &str {
    let native_endian = if cfg!(target_endian = "little") {
        "le"
    } else {
        "be"
    };
    format.strip_suffix(native_endian).unwrap_or(format)
}

fn audio_format_name(format: &str) -> &str {
    match format {
        "flt" | "f32" => "float",
        "fltp" => "floatp",
        "dbl" | "f64" => "double",
        "dblp" => "doublep",
        "i8" => "s8",
        "i16" => "s16",
        "i32" => "s32",
        "i64" => "s64",
        other => other,
    }
}

// FFmpeg property names -> mpv's video/csputils.c display names.
fn color_levels_name(value: &str) -> &str {
    match value {
        "tv" => "limited",
        "pc" => "full",
        other => other,
    }
}

fn chroma_location_name(value: &str) -> &str {
    match value {
        "left" => "mpeg2/4/h264",
        "center" => "mpeg1/jpeg",
        "topleft" => "uhd",
        "bottomleft" => "bottom-left",
        other => other,
    }
}

fn color_matrix_name<'a>(value: &'a str, transfer: Option<&str>) -> &'a str {
    match value {
        "bt709" => "bt.709",
        "bt470bg" | "smpte170m" => "bt.601",
        "smpte240m" => "smpte-240m",
        "bt2020nc" => "bt.2020-ncl",
        "bt2020c" => "bt.2020-cl",
        "gbr" => "rgb",
        "ictcp" if transfer == Some("arib-std-b67") => "bt.2100-hlg",
        "ictcp" => "bt.2100-pq",
        other => other,
    }
}

fn color_primaries_name(value: &str) -> &str {
    match value {
        "bt709" => "bt.709",
        "bt470m" => "bt.470m",
        "bt470bg" => "bt.601-625",
        "smpte170m" | "smpte240m" => "bt.601-525",
        "film" => "film-c",
        "bt2020" => "bt.2020",
        "smpte428" => "cie1931",
        "smpte431" => "dci-p3",
        "smpte432" => "display-p3",
        "jedec-p22" | "ebu3213" => "ebu3213",
        other => other,
    }
}

fn color_transfer_name(value: &str) -> &str {
    match value {
        "bt709" | "smpte170m" | "smpte240m" | "bt2020-10" | "bt2020-12" | "iec61966-2-4"
        | "bt1361e" => "bt.1886",
        "gamma22" => "gamma2.2",
        "gamma28" => "gamma2.8",
        "iec61966-2-1" => "srgb",
        "smpte2084" => "pq",
        "arib-std-b67" => "hlg",
        "smpte428" => "st428",
        other => other,
    }
}

#[cfg(test)]
#[path = "stats/tests.rs"]
mod tests;
