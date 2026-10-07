use super::memory_budget::MemoryBudget;
use crate::{
    media::{PlaybackLanguagePreferences, TrackLanguage},
    theme::ColorTheme,
};
use tiny_playback::{
    CacheUnlinkPolicy, HardwareDecodeMode, PlaybackCacheConfig, PlaybackCacheMode,
    PlaybackSeekableCacheMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsMode {
    Development,
    User,
}

impl SettingsMode {
    pub(crate) fn resolve(debug_build: bool, override_value: Option<&str>) -> Self {
        match override_value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("1" | "true" | "on") => Self::Development,
            Some("0" | "false" | "off") => Self::User,
            _ if debug_build => Self::Development,
            _ => Self::User,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Development => "开发设置",
            Self::User => "设置",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsCategory {
    #[default]
    Appearance,
    Playback,
    General,
    Memory,
    Disk,
    Readahead,
    About,
}

impl SettingsCategory {
    pub(crate) const USER: [Self; 5] = [
        Self::Appearance,
        Self::Playback,
        Self::Memory,
        Self::Disk,
        Self::About,
    ];
    pub(crate) const ALL: [Self; 7] = [
        Self::Appearance,
        Self::Playback,
        Self::General,
        Self::Memory,
        Self::Disk,
        Self::Readahead,
        Self::About,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Playback => "播放",
            Self::General => "缓存策略",
            Self::Memory => "内存缓存",
            Self::Disk => "磁盘缓存",
            Self::Readahead => "预读策略",
            Self::About => "关于",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ToggleSetting {
    DecoderFramedrop,
    DiskCache,
    CachePause,
    CachePauseInitial,
    DonateBuffer,
    AdaptiveReadahead,
    AutomaticHysteresis,
    DemuxerCacheWait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NumericSetting {
    TotalCacheMib,
    HttpCacheMib,
    HttpCacheChunkMib,
    DemuxerForwardMib,
    DemuxerBackMib,
    RangeRequestMib,
    CacheSecs,
    ReadaheadSecs,
    PacketReadaheadSecs,
    HysteresisSecs,
    CachePauseWaitSecs,
    MaxRanges,
    DiskCacheGib,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingValidation {
    Valid,
    Invalid,
}

pub(crate) enum SettingsIntent {
    Theme(ColorTheme),
    Language {
        language: TrackLanguage,
        audio: bool,
    },
    HardwareDecode(HardwareDecodeMode),
    MemoryBudget(Option<MemoryBudget>),
    Mode(PlaybackCacheMode),
    SeekableCache(PlaybackSeekableCacheMode),
    UnlinkFiles(CacheUnlinkPolicy),
    Toggle(ToggleSetting),
    EditNumber {
        field: NumericSetting,
        input: String,
    },
    Category(SettingsCategory),
    Search(String),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SettingsSnapshot {
    pub(crate) playback: PlaybackCacheConfig,
    pub(crate) color_theme: ColorTheme,
    pub(crate) track_languages: PlaybackLanguagePreferences,
}

#[derive(Default)]
pub(crate) struct SettingsChange {
    pub(crate) persist: bool,
    pub(crate) view_changed: bool,
    pub(crate) theme: Option<ColorTheme>,
    pub(crate) languages: Option<PlaybackLanguagePreferences>,
}

/// Search/category metadata is independent of the concrete GPUI control.
pub(crate) struct SettingDescriptor {
    pub(crate) category: SettingsCategory,
    pub(crate) section: &'static str,
    pub(crate) title: &'static str,
    pub(crate) description: &'static str,
    pub(crate) keywords: &'static str,
}
