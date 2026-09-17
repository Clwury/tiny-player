use crate::{emby::MediaSource, player::format_video_size};

pub(super) fn video_metadata_label(source: &MediaSource) -> Option<String> {
    let streams = source.media_streams.as_deref().unwrap_or_default();
    let mut video_titles = streams.iter().filter_map(|stream| {
        let title = stream.display_title.as_deref()?.trim();
        (stream.is_video() && !title.is_empty())
            .then_some((stream.is_default.unwrap_or(false), title))
    });
    let first_title = video_titles.next();
    let title = first_title
        .filter(|(is_default, _)| *is_default)
        .or_else(|| video_titles.find(|(is_default, _)| *is_default))
        .or(first_title)
        .map(|(_, title)| title);

    let mut parts = Vec::new();
    if let Some(title) = title {
        // Keep the resolution and codec, without HDR or profile suffixes.
        let quality = title
            .split_whitespace()
            .filter(|part| *part != "-")
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        if !quality.is_empty() {
            parts.push(quality);
        }
    }
    if let Some(size) = source.size.filter(|size| *size > 0) {
        parts.push(format_video_size(size));
    }
    if let Some(bitrate) = source.bitrate.filter(|bitrate| *bitrate > 0) {
        parts.push(format!("{:.1} Mbps", bitrate as f64 / 1_000_000.0));
    }
    (!parts.is_empty()).then(|| parts.join(" - "))
}

#[cfg(test)]
mod tests {
    use super::*;

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
