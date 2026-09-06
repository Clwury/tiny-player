use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::emby::{MediaSource, MediaStream};

/// The same language choices as Tsukimi's preferred audio/subtitle settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TrackLanguage {
    English,
    ChineseSimplified,
    Japanese,
    ChineseTraditional,
    Arabic,
    NorwegianBokmal,
    Portuguese,
    French,
    Russian,
    #[default]
    #[serde(other)]
    Default,
}

impl TrackLanguage {
    pub(crate) const ALL: [Self; 10] = [
        Self::Default,
        Self::English,
        Self::ChineseSimplified,
        Self::Japanese,
        Self::ChineseTraditional,
        Self::Arabic,
        Self::NorwegianBokmal,
        Self::Portuguese,
        Self::French,
        Self::Russian,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Default => "language-default",
            Self::English => "language-english",
            Self::ChineseSimplified => "language-chinese-simplified",
            Self::Japanese => "language-japanese",
            Self::ChineseTraditional => "language-chinese-traditional",
            Self::Arabic => "language-arabic",
            Self::NorwegianBokmal => "language-norwegian-bokmal",
            Self::Portuguese => "language-portuguese",
            Self::French => "language-french",
            Self::Russian => "language-russian",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::English => "English",
            Self::ChineseSimplified => "简体中文",
            Self::Japanese => "日本語",
            Self::ChineseTraditional => "繁體中文",
            Self::Arabic => "اَلْعَرَبِيَّةُ",
            Self::NorwegianBokmal => "Norwegian Bokmål",
            Self::Portuguese => "Portuguese",
            Self::French => "Français",
            Self::Russian => "Русский",
        }
    }

    fn codes(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::English => &["en", "eng"],
            Self::ChineseSimplified | Self::ChineseTraditional => &["zh", "zho", "chi"],
            Self::Japanese => &["ja", "jpn"],
            Self::Arabic => &["ar", "ara"],
            Self::NorwegianBokmal => &["nb", "nob", "no", "nor"],
            Self::Portuguese => &["pt", "por"],
            Self::French => &["fr", "fra", "fre"],
            Self::Russian => &["ru", "rus"],
        }
    }

    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::English => &["english", "英语", "英文"],
            Self::ChineseSimplified => &["simplified", "简体", "簡體"],
            Self::Japanese => &["japanese", "日本語", "日语", "日文"],
            Self::ChineseTraditional => &["traditional", "繁体", "繁體"],
            Self::Arabic => &["arabic", "العربية", "اَلْعَرَبِيَّةُ", "阿拉伯"],
            Self::NorwegianBokmal => &["norwegian", "bokmål", "bokmal", "挪威"],
            Self::Portuguese => &["portuguese", "português", "葡萄牙"],
            Self::French => &["french", "français", "法语", "法文"],
            Self::Russian => &["russian", "русский", "俄语", "俄文"],
        }
    }

    fn match_score(self, stream: &MediaStream) -> Option<u8> {
        if self == Self::Default {
            return None;
        }
        let code = stream
            .language
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .replace('_', "-");
        let base_code = code.split('-').next().unwrap_or_default();
        let title = format!(
            "{} {}",
            stream.display_title.as_deref().unwrap_or_default(),
            stream.title.as_deref().unwrap_or_default()
        )
        .to_lowercase();
        let unspecified = matches!(base_code, "" | "und");
        if matches!(self, Self::ChineseSimplified | Self::ChineseTraditional) {
            // Emby often labels both scripts as chi/zho. Prefer an explicit
            // script in the language tag or title over unspecified Chinese.
            let script = match code.as_str() {
                "chs" => Some(Self::ChineseSimplified),
                "cht" => Some(Self::ChineseTraditional),
                _ if self.codes().contains(&base_code) => {
                    if code
                        .split('-')
                        .any(|part| matches!(part, "hans" | "cn" | "sg"))
                    {
                        Some(Self::ChineseSimplified)
                    } else if code
                        .split('-')
                        .any(|part| matches!(part, "hant" | "tw" | "hk" | "mo"))
                    {
                        Some(Self::ChineseTraditional)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(script) = script {
                return (self == script).then_some(3);
            }
            if !unspecified && !self.codes().contains(&base_code) {
                return None;
            }
            let simplified = Self::ChineseSimplified
                .names()
                .iter()
                .any(|name| title.contains(name));
            let traditional = Self::ChineseTraditional
                .names()
                .iter()
                .any(|name| title.contains(name));
            if simplified || traditional {
                return match self {
                    Self::ChineseSimplified => simplified.then_some(2),
                    _ => traditional.then_some(2),
                };
            }
            return (self.codes().contains(&base_code)
                || ["chinese", "中文", "汉语", "漢語", "国语", "國語"]
                    .iter()
                    .any(|name| title.contains(name)))
            .then_some(1);
        }
        if self.codes().contains(&base_code) {
            Some(3)
        } else if unspecified && self.names().iter().any(|name| title.contains(name)) {
            Some(2)
        } else {
            None
        }
    }

    fn matching_stream_position(self, streams: &[&MediaStream]) -> Option<usize> {
        let mut best = None;
        for (position, stream) in streams.iter().enumerate() {
            if stream.index.is_none() {
                continue;
            }
            if let Some(score) = self.match_score(stream) {
                let priority = (score, stream.is_default.unwrap_or(false));
                // Preserve source order when equally suitable tracks remain.
                if best.is_none_or(|(_, best_priority)| priority > best_priority) {
                    best = Some((position, priority));
                }
            }
        }
        best.map(|(position, _)| position)
    }

    pub(crate) fn matching_audio_stream_index(self, source: &MediaSource) -> Option<usize> {
        let streams = source.audio_streams();
        let position = self.matching_stream_position(&streams)?;
        usize::try_from(streams[position].index?).ok()
    }

    pub(crate) fn preferred_subtitle_stream_position(self, source: &MediaSource) -> Option<usize> {
        self.matching_stream_position(&source.subtitle_streams())
            .or_else(|| source.preferred_subtitle_stream_position())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PlaybackLanguagePreferences {
    pub(crate) audio: TrackLanguage,
    pub(crate) subtitle: TrackLanguage,
}

impl Global for PlaybackLanguagePreferences {}

impl PlaybackLanguagePreferences {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn apply(self, cx: &mut App) {
        if cx.try_global::<Self>() != Some(&self) {
            cx.set_global(self);
            cx.refresh_windows();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn language_matching_accepts_iso_aliases_and_regional_tags() {
        for (language, codes) in [
            (TrackLanguage::English, vec!["en", "ENG", "en-US"]),
            (TrackLanguage::Japanese, vec!["ja", "jpn", "JA_jp"]),
            (TrackLanguage::Arabic, vec!["ar", "ara"]),
            (
                TrackLanguage::NorwegianBokmal,
                vec!["nb", "nob", "no", "nor"],
            ),
            (TrackLanguage::Portuguese, vec!["pt", "por", "pt-BR"]),
            (TrackLanguage::French, vec!["fr", "fra", "fre"]),
            (TrackLanguage::Russian, vec!["ru", "rus"]),
        ] {
            for code in codes {
                let stream: MediaStream =
                    serde_json::from_value(json!({"Language": code})).unwrap();
                assert!(
                    language.match_score(&stream).is_some(),
                    "{language:?}: {code}"
                );
                assert_eq!(TrackLanguage::Default.match_score(&stream), None);
            }
        }
    }

    #[test]
    fn chinese_scripts_use_tags_or_titles_and_prefer_specific_matches() {
        for (language, codes, title) in [
            (
                TrackLanguage::ChineseSimplified,
                ["chs", "zh-Hans", "zh_CN"],
                "Chinese Simplified (ASS)",
            ),
            (
                TrackLanguage::ChineseTraditional,
                ["cht", "zh-Hant", "zh_TW"],
                "繁體中文",
            ),
        ] {
            let other = if language == TrackLanguage::ChineseSimplified {
                TrackLanguage::ChineseTraditional
            } else {
                TrackLanguage::ChineseSimplified
            };
            for code in codes {
                let stream = serde_json::from_value(json!({"Language": code})).unwrap();
                assert!(language.match_score(&stream).is_some());
                assert_eq!(other.match_score(&stream), None);
            }
            let source: MediaSource = serde_json::from_value(json!({"MediaStreams": [
                {"Index": 3, "Type": "Subtitle", "Language": "zho", "IsDefault": true},
                {"Index": 9, "Type": "Subtitle", "Language": "chi", "DisplayTitle": title}
            ]}))
            .unwrap();
            assert_eq!(
                language.preferred_subtitle_stream_position(&source),
                Some(1)
            );
            assert_eq!(other.preferred_subtitle_stream_position(&source), Some(0));
        }
    }

    #[test]
    fn missing_language_uses_title_but_conflicting_metadata_is_respected() {
        let source: MediaSource = serde_json::from_value(json!({"MediaStreams": [
            {"Type": "Audio", "Language": "jpn"},
            {"Index": 4, "Type": "Audio", "Language": "eng", "DisplayTitle": "Japanese Commentary"},
            {"Index": 8, "Type": "Audio", "Language": "und", "DisplayTitle": "日本語"}
        ]}))
        .unwrap();
        assert_eq!(
            TrackLanguage::Japanese.matching_audio_stream_index(&source),
            Some(8)
        );
        assert_eq!(
            TrackLanguage::Russian.matching_audio_stream_index(&source),
            None
        );
    }

    #[test]
    fn equally_matching_tracks_prefer_default_then_source_order() {
        let source: MediaSource = serde_json::from_value(json!({"MediaStreams": [
            {"Index": 2, "Type": "Audio", "Language": "eng"},
            {"Index": 6, "Type": "Audio", "Language": "eng", "IsDefault": true},
            {"Index": 8, "Type": "Audio", "Language": "eng", "IsDefault": true}
        ]}))
        .unwrap();
        assert_eq!(
            TrackLanguage::English.matching_audio_stream_index(&source),
            Some(6)
        );
    }
}
