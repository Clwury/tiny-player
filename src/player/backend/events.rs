use std::{fmt, path::PathBuf, sync::Arc};

use gpui::RenderImage;

use crate::player::render_host::{PlaybackSessionId, RenderSize};

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackVideoInfo {
    pub decoder: String,
    pub size: RenderSize,
    pub frame_rate: Option<f64>,
    pub hardware_accelerated: bool,
}

#[derive(Clone)]
pub struct BackendSubtitleBitmap {
    pub image: Arc<RenderImage>,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub canvas_width: u32,
    pub canvas_height: u32,
}

impl fmt::Debug for BackendSubtitleBitmap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackendSubtitleBitmap")
            .field("image", &"<render-image>")
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("canvas_width", &self.canvas_width)
            .field("canvas_height", &self.canvas_height)
            .finish()
    }
}

impl PartialEq for BackendSubtitleBitmap {
    fn eq(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.canvas_width == other.canvas_width
            && self.canvas_height == other.canvas_height
            && self.image == other.image
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackendSubtitleCue {
    pub text: String,
    pub bitmaps: Vec<BackendSubtitleBitmap>,
    pub start_nsecs: u64,
    pub end_nsecs: u64,
}

impl BackendSubtitleCue {
    pub fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.bitmaps.is_empty()
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackCacheMode {
    Auto,
    Enabled,
    Disabled,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackSeekableCacheMode {
    Auto,
    Enabled,
    Disabled,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheUnlinkPolicy {
    Immediate,
    WhenDone,
    Never,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackCacheConfig {
    pub mode: PlaybackCacheMode,
    pub seekable_cache: PlaybackSeekableCacheMode,
    pub disk_cache: bool,
    pub disk_cache_max_bytes: u64,
    pub cache_secs: f64,
    pub demuxer_readahead_secs: f64,
    pub demuxer_hysteresis_secs: f64,
    pub demuxer_max_bytes: u64,
    pub demuxer_max_back_bytes: u64,
    pub demuxer_donate_buffer: bool,
    pub http_cache_max_bytes: u64,
    pub http_cache_chunk_bytes: u64,
    pub http_cache_range_request_bytes: u64,
    pub cache_pause: bool,
    pub cache_pause_initial: bool,
    pub cache_pause_wait: f64,
    pub demuxer_cache_wait: bool,
    pub cache_dir: Option<PathBuf>,
    pub unlink_files: CacheUnlinkPolicy,
}

impl Default for PlaybackCacheConfig {
    fn default() -> Self {
        Self {
            mode: PlaybackCacheMode::Auto,
            seekable_cache: PlaybackSeekableCacheMode::Auto,
            disk_cache: false,
            disk_cache_max_bytes: 4 * 1024 * 1024 * 1024,
            cache_secs: 1000.0 * 60.0 * 60.0,
            demuxer_readahead_secs: 1.0,
            demuxer_hysteresis_secs: 0.0,
            demuxer_max_bytes: 150 * 1024 * 1024,
            demuxer_max_back_bytes: 50 * 1024 * 1024,
            demuxer_donate_buffer: true,
            http_cache_max_bytes: 500 * 1024 * 1024,
            http_cache_chunk_bytes: 1024 * 1024,
            http_cache_range_request_bytes: 32 * 1024 * 1024,
            cache_pause: true,
            cache_pause_initial: false,
            cache_pause_wait: 1.0,
            demuxer_cache_wait: false,
            cache_dir: None,
            unlink_files: CacheUnlinkPolicy::Immediate,
        }
    }
}

impl PlaybackCacheConfig {
    pub fn normalized(mut self) -> Self {
        self.cache_secs = valid_non_negative_or(self.cache_secs, Self::default().cache_secs);
        self.demuxer_readahead_secs = valid_non_negative_or(
            self.demuxer_readahead_secs,
            Self::default().demuxer_readahead_secs,
        );
        self.demuxer_hysteresis_secs = valid_non_negative_or(self.demuxer_hysteresis_secs, 0.0);
        self.disk_cache_max_bytes = self.disk_cache_max_bytes.max(1);
        self.http_cache_chunk_bytes = self
            .http_cache_chunk_bytes
            .clamp(64 * 1024, 16 * 1024 * 1024);
        self.http_cache_range_request_bytes = self
            .http_cache_range_request_bytes
            .clamp(64 * 1024, 128 * 1024 * 1024);
        self.http_cache_range_request_bytes = self
            .http_cache_range_request_bytes
            .max(self.http_cache_chunk_bytes);
        self.http_cache_max_bytes = self.http_cache_max_bytes.max(self.http_cache_chunk_bytes);
        self.cache_pause_wait = valid_non_negative_or(self.cache_pause_wait, 1.0);
        self
    }

    pub fn resolved_for_cacheable_input(mut self, input_cacheable: bool) -> Self {
        self = self.normalized();
        if matches!(self.mode, PlaybackCacheMode::Auto) {
            self.mode = if input_cacheable {
                PlaybackCacheMode::Enabled
            } else {
                PlaybackCacheMode::Disabled
            };
        }
        self
    }

    pub fn effective_readahead_secs(&self, cache_active: bool) -> f64 {
        if cache_active {
            self.demuxer_readahead_secs.max(self.cache_secs)
        } else {
            self.demuxer_readahead_secs
        }
    }

    pub fn seekable_cache_active(&self, cache_active: bool) -> bool {
        match self.seekable_cache {
            PlaybackSeekableCacheMode::Enabled => true,
            PlaybackSeekableCacheMode::Disabled => false,
            PlaybackSeekableCacheMode::Auto => cache_active,
        }
    }
}

fn valid_non_negative_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        fallback
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackCacheTimeRange {
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackCacheByteRange {
    pub start_fraction: f64,
    pub end_fraction: f64,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamCacheKind {
    Video,
    Audio,
    Subtitle,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamCacheState {
    pub kind: StreamCacheKind,
    pub cache_end: Option<f64>,
    pub reader_pts: Option<f64>,
    pub cache_duration: Option<f64>,
    pub underrun: bool,
    pub idle: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DemuxCacheState {
    pub cache_end: Option<f64>,
    pub reader_pts: Option<f64>,
    pub cache_duration: Option<f64>,
    pub eof: bool,
    pub underrun: bool,
    pub idle: bool,
    pub seeking: bool,
    pub bof_cached: bool,
    pub eof_cached: bool,
    pub total_bytes: u64,
    pub forward_bytes: u64,
    pub file_cache_bytes: Option<u64>,
    pub raw_input_rate: Option<u64>,
    pub ts_last: Option<f64>,
    pub cached_seeks: u64,
    pub low_level_seeks: u64,
    pub byte_level_seeks: u64,
    pub seekable_ranges: Vec<PlaybackCacheTimeRange>,
    pub streams: Vec<StreamCacheState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ByteCacheState {
    pub ranges: Vec<PlaybackCacheByteRange>,
    pub reader_fraction: Option<f64>,
    pub download_fraction: Option<f64>,
    pub cached_bytes: u64,
    pub content_length: Option<u64>,
    pub disk_cache_enabled: bool,
    pub idle: bool,
    pub raw_input_rate: Option<u64>,
    pub byte_level_seeks: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackCacheState {
    pub demux: DemuxCacheState,
    pub byte: Option<ByteCacheState>,
    pub paused_for_cache: bool,
    pub buffering_percent: Option<u8>,
}

#[derive(Debug)]
pub struct BackendEvent {
    pub session_id: PlaybackSessionId,
    pub kind: BackendEventKind,
}

impl BackendEvent {
    pub fn new(session_id: PlaybackSessionId, kind: BackendEventKind) -> Self {
        Self { session_id, kind }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BackendEventKind {
    Pause(bool),
    PlaybackEnded,
    PlaybackRestart,
    PlaybackInfoChanged(Option<PlaybackVideoInfo>),
    VideoSizeChanged(Option<RenderSize>),
    Buffering(bool),
    PositionChanged(f64),
    DurationChanged(f64),
    BufferedChanged(Option<f64>),
    CacheStateChanged(PlaybackCacheState),
    #[allow(dead_code)]
    PausedForCacheChanged(bool),
    #[allow(dead_code)]
    CacheBufferingChanged(Option<u8>),
    SubtitleChanged(Option<BackendSubtitleCue>),
    LoadFailed(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum BackendError {
    EmptyUrl,
    UnsupportedCommand(&'static str),
    Ffmpeg(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "播放地址为空"),
            Self::UnsupportedCommand(command) => write!(f, "当前播放后端不支持{command}"),
            Self::Ffmpeg(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BackendError {}

#[cfg(test)]
mod tests {
    use super::{BackendError, PlaybackCacheConfig, PlaybackCacheMode};

    #[test]
    fn backend_error_displays_user_facing_messages() {
        assert_eq!(BackendError::EmptyUrl.to_string(), "播放地址为空");
        assert_eq!(
            BackendError::UnsupportedCommand("切换音轨").to_string(),
            "当前播放后端不支持切换音轨"
        );
        assert_eq!(
            BackendError::Ffmpeg("解码失败".to_string()).to_string(),
            "解码失败"
        );
    }

    #[test]
    fn cache_config_normalizes_http_range_request_budget() {
        let config = PlaybackCacheConfig {
            http_cache_chunk_bytes: 512 * 1024,
            http_cache_range_request_bytes: 1,
            disk_cache_max_bytes: 0,
            ..PlaybackCacheConfig::default()
        }
        .normalized();

        assert_eq!(config.http_cache_range_request_bytes, 512 * 1024);
        assert_eq!(config.disk_cache_max_bytes, 1);
    }

    #[test]
    fn cache_config_allows_zero_cache_secs() {
        let config = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Enabled,
            cache_secs: 0.0,
            demuxer_readahead_secs: 2.0,
            ..PlaybackCacheConfig::default()
        }
        .normalized();

        assert_eq!(config.cache_secs, 0.0);
        assert_eq!(config.effective_readahead_secs(true), 2.0);
    }

    #[test]
    fn cache_config_allows_zero_demuxer_max_bytes() {
        let config = PlaybackCacheConfig {
            demuxer_max_bytes: 0,
            ..PlaybackCacheConfig::default()
        }
        .normalized();

        assert_eq!(config.demuxer_max_bytes, 0);
    }

    #[test]
    fn cache_config_resolves_auto_mode_from_input_cacheability() {
        let network = PlaybackCacheConfig::default().resolved_for_cacheable_input(true);
        let local = PlaybackCacheConfig::default().resolved_for_cacheable_input(false);

        assert_eq!(network.mode, PlaybackCacheMode::Enabled);
        assert_eq!(local.mode, PlaybackCacheMode::Disabled);

        let forced = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Enabled,
            ..PlaybackCacheConfig::default()
        }
        .resolved_for_cacheable_input(false);
        assert_eq!(forced.mode, PlaybackCacheMode::Enabled);
    }

    #[test]
    fn cache_config_resolves_network_demux_prebuffer_defaults() {
        let network = PlaybackCacheConfig::default().resolved_for_cacheable_input(true);
        let local = PlaybackCacheConfig::default().resolved_for_cacheable_input(false);

        assert!(!network.demuxer_cache_wait);
        assert!(!network.cache_pause_initial);
        assert_eq!(network.cache_pause_wait, 1.0);
        assert_eq!(network.demuxer_readahead_secs, 1.0);
        assert_eq!(network.demuxer_hysteresis_secs, 0.0);
        assert_eq!(network.demuxer_max_bytes, 150 * 1024 * 1024);
        assert_eq!(network.demuxer_max_back_bytes, 50 * 1024 * 1024);
        assert!(network.demuxer_donate_buffer);
        assert_eq!(network.effective_readahead_secs(true), 1000.0 * 60.0 * 60.0);

        assert!(!local.demuxer_cache_wait);
        assert!(!local.cache_pause_initial);
        assert_eq!(local.cache_pause_wait, 1.0);
        assert_eq!(local.demuxer_readahead_secs, 1.0);
        assert_eq!(local.demuxer_hysteresis_secs, 0.0);
        assert!(local.demuxer_donate_buffer);
        assert_eq!(local.effective_readahead_secs(false), 1.0);
    }

    #[test]
    fn cache_config_respects_disabled_cache_pause_for_network_inputs() {
        let network = PlaybackCacheConfig {
            cache_pause: false,
            ..PlaybackCacheConfig::default()
        }
        .resolved_for_cacheable_input(true);

        assert!(!network.cache_pause);
        assert!(!network.cache_pause_initial);
        assert!(!network.demuxer_cache_wait);
    }
}
