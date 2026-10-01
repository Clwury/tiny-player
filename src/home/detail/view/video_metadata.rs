use crate::{
    emby::{MediaSource, MediaStream},
    player::format_video_size,
};

pub(super) fn video_metadata_label(source: &MediaSource) -> Option<String> {
    let streams = source.media_streams.as_deref().unwrap_or_default();
    let mut parts = Vec::new();
    if let Some(quality) = preferred_video_label(streams, video_option_quality) {
        parts.push(quality);
    }
    if let Some(size) = source.size.filter(|size| *size > 0) {
        parts.push(format_video_size(size));
    }
    if let Some(bitrate) = source.bitrate.filter(|bitrate| *bitrate > 0) {
        parts.push(format!("{:.1} Mbps", bitrate as f64 / 1_000_000.0));
    }
    (!parts.is_empty()).then(|| parts.join(" - "))
}

pub(super) fn video_quality_label(streams: &[MediaStream]) -> Option<String> {
    preferred_video_label(streams, |stream| {
        let resolution = video_resolution_label(stream);
        let range = stream
            .video_range
            .as_deref()
            .map(str::trim)
            .filter(|range| !range.is_empty());
        let parts = [resolution.as_deref(), range]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        (!parts.is_empty()).then(|| parts.join(" "))
    })
}

fn preferred_video_label(
    streams: &[MediaStream],
    label: impl Fn(&MediaStream) -> Option<String>,
) -> Option<String> {
    let mut candidates = streams
        .iter()
        .filter(|stream| stream.is_video())
        .filter_map(|stream| {
            label(stream).map(|label| (stream.is_default.unwrap_or(false), label))
        });
    let first = candidates.next()?;
    if first.0 {
        return Some(first.1);
    }
    Some(
        candidates
            .find(|(is_default, _)| *is_default)
            .unwrap_or(first)
            .1,
    )
}

fn video_option_quality(stream: &MediaStream) -> Option<String> {
    if let Some(title) = stream
        .display_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        // Keep the resolution and codec, without HDR or profile suffixes.
        let quality = title
            .split_whitespace()
            .filter(|part| *part != "-")
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        if !quality.is_empty() {
            return Some(quality);
        }
    }
    let mut quality = video_resolution_label(stream)?;
    if let Some(codec) = stream
        .codec
        .as_deref()
        .map(str::trim)
        .filter(|codec| !codec.is_empty())
    {
        quality.push(' ');
        quality.push_str(&codec.to_ascii_uppercase());
    }
    Some(quality)
}

fn video_resolution_label(stream: &MediaStream) -> Option<String> {
    let width = stream.width.filter(|value| *value > 0);
    let height = stream.height.filter(|value| *value > 0);
    let (long_edge, short_edge) = match (width, height) {
        (Some(width), Some(height)) => (width.max(height), width.min(height)),
        (Some(width), None) => (width, 0),
        (None, Some(height)) => (0, height),
        (None, None) => return None,
    };
    // Width keeps cropped widescreen movies in their original resolution tier.
    for (width, height, label) in [
        (7680, 4320, "8K"),
        (3840, 2160, "4K"),
        (2560, 1440, "1440p"),
        (1920, 1080, "1080p"),
        (1280, 720, "720p"),
    ] {
        if long_edge >= width || short_edge >= height {
            return Some(label.into());
        }
    }
    if short_edge >= 576 {
        Some("576p".into())
    } else if short_edge >= 480 || long_edge >= 720 {
        Some("480p".into())
    } else {
        (short_edge > 0).then(|| format!("{short_edge}p"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_resolution_handles_cropped_and_portrait_dimensions() {
        for (width, height, expected) in [
            (Some(7680), Some(3200), Some("8K")),
            (Some(3840), Some(1600), Some("4K")),
            (Some(4096), Some(1716), Some("4K")),
            (Some(2560), Some(1080), Some("1440p")),
            (Some(1920), Some(800), Some("1080p")),
            (Some(1080), Some(1920), Some("1080p")),
            (Some(1280), Some(536), Some("720p")),
            (Some(720), Some(576), Some("576p")),
            (Some(640), Some(480), Some("480p")),
            (Some(640), Some(360), Some("360p")),
            (None, Some(2160), Some("4K")),
            (Some(1920), None, Some("1080p")),
            (Some(0), Some(0), None),
            (None, None, None),
        ] {
            let stream = serde_json::from_value(serde_json::json!({
                "Type": "Video", "Width": width, "Height": height
            }))
            .unwrap();
            assert_eq!(video_resolution_label(&stream).as_deref(), expected);
        }
    }

    #[test]
    fn video_options_use_dimensions_when_the_default_display_title_is_empty() {
        for title in [None, Some(""), Some(" \t ")] {
            let source = serde_json::from_value(serde_json::json!({
                "Bitrate": 20_000_000,
                "MediaStreams": [
                    {"Type": "Audio", "DisplayTitle": "English DTS"},
                    {"Type": "Video", "DisplayTitle": "720p H264"},
                    {"Type": "Video", "IsDefault": true, "DisplayTitle": title,
                     "Width": 3840, "Height": 1600, "Codec": "hevc", "VideoRange": "HDR10"}
                ]
            }))
            .unwrap();
            assert_eq!(
                video_metadata_label(&source).as_deref(),
                Some("4K HEVC - 20.0 Mbps")
            );
        }
    }

    #[test]
    fn hero_video_quality_uses_the_default_video_range_without_inventing_missing_fields() {
        for (json, expected) in [
            (
                serde_json::json!([
                    {"Type": "Audio", "Width": 3840, "Height": 2160, "VideoRange": "HDR10"},
                    {"Type": "Video", "Width": 1280, "Height": 720, "VideoRange": "SDR"},
                    {"Type": "Video", "IsDefault": true, "Width": 3840, "Height": 1600,
                     "VideoRange": " Dolby Vision "}
                ]),
                Some("4K Dolby Vision"),
            ),
            (
                serde_json::json!([{"Type": "Video", "Width": 1920, "Height": 800}]),
                Some("1080p"),
            ),
            (
                serde_json::json!([{"Type": "Video", "VideoRange": "HDR10+"}]),
                Some("HDR10+"),
            ),
            (
                serde_json::json!([{"Type": "Video", "VideoRange": " \t "}]),
                None,
            ),
            (serde_json::json!([]), None),
        ] {
            let streams: Vec<MediaStream> = serde_json::from_value(json).unwrap();
            assert_eq!(video_quality_label(&streams).as_deref(), expected);
        }
    }

    #[test]
    fn video_metadata_uses_video_display_title_and_source_size_and_bitrate() {
        let source = serde_json::from_value(serde_json::json!({
            "Name": "Movie.mkv",
            "Size": 13_249_974_108_u64,
            "Bitrate": 38_543_210,
            "MediaStreams": [
                {"Type": "Audio", "DisplayTitle": "English DTS"},
                {"Type": "Video", "DisplayTitle": "1080p HEVC", "BitRate": 30_000_000},
                {"Type": "Subtitle", "DisplayTitle": "Chinese Simplified (ASS)"}
            ]
        }))
        .unwrap();

        assert_eq!(
            video_metadata_label(&source).as_deref(),
            Some("1080p HEVC - 12.34 GiB - 38.5 Mbps")
        );
    }

    #[test]
    fn video_metadata_prefers_default_video_and_omits_display_title_suffixes() {
        let mut source: MediaSource = serde_json::from_value(serde_json::json!({
            "MediaStreams": [
                {"Type": "Video", "DisplayTitle": "480p MPEG2"},
                {"Type": "Video", "DisplayTitle": " 4K  HEVC HDR10 (Main 10) ", "IsDefault": true}
            ]
        }))
        .unwrap();
        assert_eq!(video_metadata_label(&source).as_deref(), Some("4K HEVC"));

        let stream = &mut source.media_streams.as_mut().unwrap()[1];
        stream.display_title = Some("1080i - H264 (High)".into());
        assert_eq!(video_metadata_label(&source).as_deref(), Some("1080i H264"));
    }

    #[test]
    fn video_metadata_omits_unavailable_parts_without_empty_separators() {
        for (json, expected) in [
            (serde_json::json!({}), None),
            (serde_json::json!({"Size": null, "Bitrate": null}), None),
            (serde_json::json!({"Size": 0, "Bitrate": 0}), None),
            (serde_json::json!({"Size": 512}), Some("512.00 B")),
            (serde_json::json!({"Bitrate": 8_000_000}), Some("8.0 Mbps")),
            (
                serde_json::json!({
                    "Size": 536_870_912, "Bitrate": 8_000_000,
                    "MediaStreams": [{"Type": "Video", "DisplayTitle": " \t "}]
                }),
                Some("512.00 MiB - 8.0 Mbps"),
            ),
            (
                serde_json::json!({
                    "MediaStreams": [
                        {"Type": "Video", "IsDefault": true},
                        {"Type": "Video", "DisplayTitle": "720p AV1"}
                    ]
                }),
                Some("720p AV1"),
            ),
        ] {
            let source = serde_json::from_value(json).unwrap();
            assert_eq!(video_metadata_label(&source).as_deref(), expected);
        }
    }
}
