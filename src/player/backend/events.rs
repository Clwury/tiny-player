use std::{fmt, path::PathBuf, sync::Arc};

use gpui::RenderImage;
use serde::{Deserialize, Serialize};

use crate::player::render_host::{PlaybackSessionId, RenderSize};

const CACHE_CHUNK_MIN_BYTES: u64 = 64 * 1024;
const SHARED_CACHE_LAYER_RESERVE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackVideoInfo {
    pub codec: String,
    pub codec_description: Option<String>,
    pub profile: Option<String>,
    pub decoder: String,
    pub size: RenderSize,
    pub sample_aspect_ratio: Option<(u32, u32)>,
    pub frame_rate: Option<f64>,
    pub pixel_format: Option<String>,
    pub color_range: Option<String>,
    pub chroma_location: Option<String>,
    pub color_space: Option<String>,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub bitrate: Option<u64>,
    pub hardware_accelerated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackFileInfo {
    pub format_name: Option<String>,
    pub format_description: Option<String>,
    pub bitrate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackAudioInfo {
    pub codec: String,
    pub codec_description: Option<String>,
    pub profile: Option<String>,
    pub decoder: String,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_format: Option<String>,
    pub sample_rate: Option<u32>,
    pub output_channels: Option<u32>,
    pub output_sample_format: Option<String>,
    pub output_sample_rate: Option<u32>,
    pub output_device: Option<String>,
    pub bitrate: Option<u64>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackCacheMode {
    Auto,
    Enabled,
    Disabled,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackSeekableCacheMode {
    Auto,
    Enabled,
    Disabled,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUnlinkPolicy {
    Immediate,
    WhenDone,
    Never,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackCacheConfig {
    pub mode: PlaybackCacheMode,
    pub seekable_cache: PlaybackSeekableCacheMode,
    pub disk_cache: bool,
    pub disk_cache_max_bytes: u64,
    pub cache_secs: f64,
    pub demuxer_readahead_secs: f64,
    pub demuxer_packet_max_readahead_secs: f64,
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
    /// Upper bound shared by the transport and demux cache layers. A value of
    /// zero keeps the legacy independent-layer limits; when finite, a zero
    /// demux forward limit means "use the remaining shared budget".
    pub total_cache_max_bytes: u64,
    /// Maximum number of archived demux seek ranges retained in memory.
    pub demuxer_max_ranges: usize,
    /// Adapt HTTP range request sizes to observed input throughput. Demux
    /// media-time and forward-byte limits remain independent of download speed.
    pub adaptive_readahead: bool,
    /// Distinguish an automatic refill band from an explicitly disabled band.
    pub automatic_hysteresis: bool,
    /// Opt in to decoder catch-up dropping during ordinary playback. Output
    /// dropping stays enabled; precise-seek preroll has a separate policy.
    pub decoder_framedrop: bool,
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
            demuxer_packet_max_readahead_secs: 0.0,
            demuxer_hysteresis_secs: 0.0,
            demuxer_max_bytes: 150 * 1024 * 1024,
            demuxer_max_back_bytes: 50 * 1024 * 1024,
            demuxer_donate_buffer: true,
            http_cache_max_bytes: 32 * 1024 * 1024,
            http_cache_chunk_bytes: 1024 * 1024,
            http_cache_range_request_bytes: 16 * 1024 * 1024,
            cache_pause: true,
            cache_pause_initial: false,
            cache_pause_wait: 1.0,
            demuxer_cache_wait: false,
            cache_dir: None,
            unlink_files: CacheUnlinkPolicy::Immediate,
            total_cache_max_bytes: 256 * 1024 * 1024,
            demuxer_max_ranges: 10,
            adaptive_readahead: true,
            automatic_hysteresis: true,
            decoder_framedrop: false,
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
        self.demuxer_packet_max_readahead_secs = valid_non_negative_or(
            self.demuxer_packet_max_readahead_secs,
            Self::default().demuxer_packet_max_readahead_secs,
        );
        self.demuxer_hysteresis_secs = valid_non_negative_or(self.demuxer_hysteresis_secs, 0.0);
        self.disk_cache_max_bytes = self.disk_cache_max_bytes.max(1);
        self.http_cache_chunk_bytes = self
            .http_cache_chunk_bytes
            .clamp(CACHE_CHUNK_MIN_BYTES, 16 * 1024 * 1024);
        self.http_cache_range_request_bytes = self
            .http_cache_range_request_bytes
            .clamp(CACHE_CHUNK_MIN_BYTES, 128 * 1024 * 1024);
        self.http_cache_range_request_bytes = self
            .http_cache_range_request_bytes
            .max(self.http_cache_chunk_bytes);
        self.http_cache_max_bytes = self.http_cache_max_bytes.max(self.http_cache_chunk_bytes);
        self.cache_pause_wait = valid_non_negative_or(self.cache_pause_wait, 1.0);
        if self.total_cache_max_bytes > 0 {
            // Keep enough room for one transport chunk and the minimum
            // forward/backward working slices. Without this floor a finite
            // total could resolve a layer to zero, which has a different
            // legacy meaning (unlimited/disabled) inside the cache state.
            let minimum_forward = shared_cache_forward_reserve(self.demuxer_max_bytes);
            let minimum_back = shared_cache_back_reserve(self.demuxer_max_back_bytes);
            let minimum_total = self
                .http_cache_chunk_bytes
                .saturating_add(minimum_forward)
                .saturating_add(minimum_back);
            self.total_cache_max_bytes = self.total_cache_max_bytes.max(minimum_total);
        }
        if self.demuxer_max_ranges == 0 {
            self.demuxer_max_ranges = Self::default().demuxer_max_ranges;
        }
        self.demuxer_max_ranges = self.demuxer_max_ranges.clamp(1, 64);
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

    /// Compute the transport window allowed by the shared cache budget.
    /// Keep at least one configured chunk so a range request can make progress
    /// even when a user deliberately chooses a very small total budget.
    pub fn effective_http_cache_max_bytes(&self) -> u64 {
        self.effective_cache_budgets().0
    }

    /// Return `(http, demux-forward, demux-back)` limits after applying the
    /// optional shared budget. With a finite total, a zero forward demux limit
    /// consumes the remaining shared budget (the equivalent of "unlimited"
    /// within that budget); a zero backward limit remains disabled. A zero
    /// total is the explicit legacy mode where each layer keeps its own cap.
    pub fn effective_cache_budgets(&self) -> (u64, u64, u64) {
        if self.total_cache_max_bytes == 0 {
            return (
                self.http_cache_max_bytes,
                self.demuxer_max_bytes,
                self.demuxer_max_back_bytes,
            );
        }

        let total = self.total_cache_max_bytes;
        let minimum_http = self
            .http_cache_chunk_bytes
            .max(CACHE_CHUNK_MIN_BYTES)
            .min(total);
        let minimum_forward = shared_cache_forward_reserve(self.demuxer_max_bytes);
        let minimum_back = shared_cache_back_reserve(self.demuxer_max_back_bytes);
        let http_capacity = total
            .saturating_sub(minimum_forward)
            .saturating_sub(minimum_back)
            .max(minimum_http)
            .min(total);
        let http = self
            .http_cache_max_bytes
            .min(http_capacity)
            .max(minimum_http)
            .min(total);
        let mut remaining = total.saturating_sub(http);

        // Forward playback gets priority over the optional backward seek
        // reserve when the shared budget is tight. A configured zero is an
        // unlimited request, so it receives all remaining forward capacity.
        let forward_capacity = remaining.saturating_sub(minimum_back);
        let forward_request = if self.demuxer_max_bytes == 0 {
            u64::MAX
        } else {
            self.demuxer_max_bytes
        };
        let forward = forward_request
            .min(forward_capacity)
            .max(minimum_forward)
            .min(remaining);
        remaining = remaining.saturating_sub(forward);
        let back = if self.demuxer_max_back_bytes == 0 {
            0
        } else {
            self.demuxer_max_back_bytes
                .min(remaining)
                .max(minimum_back)
                .min(remaining)
        };
        (http, forward, back)
    }

    pub fn effective_demuxer_max_bytes(&self) -> u64 {
        self.effective_cache_budgets().1
    }

    /// Disk space is a single budget: reserve a small slice for HTTP metadata
    /// probes and give the remainder to seekable demux packet payloads.
    pub fn effective_disk_cache_budgets(&self) -> (u64, u64) {
        let http = (self.disk_cache_max_bytes / 8).min(32 * 1024 * 1024);
        (http, self.disk_cache_max_bytes - http)
    }

    pub fn effective_demuxer_max_back_bytes(&self) -> u64 {
        self.effective_cache_budgets().2
    }

    pub fn seekable_cache_active(&self, cache_active: bool) -> bool {
        match self.seekable_cache {
            PlaybackSeekableCacheMode::Enabled => true,
            PlaybackSeekableCacheMode::Disabled => false,
            PlaybackSeekableCacheMode::Auto => cache_active,
        }
    }
}

fn shared_cache_forward_reserve(configured: u64) -> u64 {
    if configured == 0 {
        SHARED_CACHE_LAYER_RESERVE_BYTES
    } else {
        configured.clamp(1, SHARED_CACHE_LAYER_RESERVE_BYTES)
    }
}

fn shared_cache_back_reserve(configured: u64) -> u64 {
    if configured == 0 {
        0
    } else {
        configured.clamp(1, SHARED_CACHE_LAYER_RESERVE_BYTES)
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
pub struct CacheStorageState {
    /// Cache-owned payloads plus estimated packet/index metadata, not process RSS.
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    /// Live disk-backed media, separately from allocated file length.
    pub disk_bytes: u64,
    pub disk_file_bytes: u64,
    pub disk_limit_bytes: u64,
    pub disk_pending_bytes: u64,
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
    /// Effective targets after adaptive rate and byte-budget clamping.
    pub readahead_secs: f64,
    pub hysteresis_secs: f64,
    pub memory_limit_bytes: u64,
    pub backbuffer_limit_bytes: u64,
    pub cached_range_count: usize,
    pub storage: CacheStorageState,
    pub forward_limit_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ByteCacheState {
    pub ranges: Vec<PlaybackCacheByteRange>,
    pub reader_fraction: Option<f64>,
    pub download_fraction: Option<f64>,
    pub cached_bytes: u64,
    pub content_length: Option<u64>,
    pub disk_cache_enabled: bool,
    pub idle: bool,
    pub raw_input_rate: Option<u64>,
    pub active_forward_bytes: u64,
    pub active_forward_est_seconds: Option<f64>,
    pub range_request_bytes_effective: u64,
    pub byte_level_seeks: u64,
    /// Transport-side watermarks and retained-range pressure, exposed for
    /// diagnostics/UI without requiring another cache lock acquisition.
    pub target_readahead_bytes: u64,
    pub resume_readahead_bytes: u64,
    pub memory_capacity_bytes: u64,
    pub retained_bytes: u64,
    pub prefetch_paused: bool,
    pub retained_range_count: usize,
    pub storage: CacheStorageState,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendDiagnostic {
    pub code: &'static str,
    pub message: String,
}

impl BackendEvent {
    pub fn new(session_id: PlaybackSessionId, kind: BackendEventKind) -> Self {
        Self { session_id, kind }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BackendEventKind {
    Diagnostic(BackendDiagnostic),
    Pause(bool),
    PlaybackEnded,
    PlaybackRestart,
    PlaybackInfoChanged(Option<PlaybackVideoInfo>),
    PlaybackFileInfoChanged(PlaybackFileInfo),
    PlaybackAudioInfoChanged(Option<PlaybackAudioInfo>),
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
    use super::{
        BackendError, CacheUnlinkPolicy, PlaybackCacheConfig, PlaybackCacheMode,
        PlaybackSeekableCacheMode,
    };

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
    fn cache_config_defaults_to_uncapped_packet_readahead() {
        assert_eq!(
            PlaybackCacheConfig::default()
                .normalized()
                .demuxer_packet_max_readahead_secs,
            0.0
        );
    }

    #[test]
    fn cache_config_defaults_match_mpv() {
        let config = PlaybackCacheConfig::default();

        assert_eq!(config.mode, PlaybackCacheMode::Auto);
        assert_eq!(config.seekable_cache, PlaybackSeekableCacheMode::Auto);
        assert!(!config.disk_cache);
        assert_eq!(config.cache_secs, 1000.0 * 60.0 * 60.0);
        assert_eq!(config.demuxer_readahead_secs, 1.0);
        assert_eq!(config.demuxer_hysteresis_secs, 0.0);
        assert_eq!(config.demuxer_max_bytes, 150 * 1024 * 1024);
        assert_eq!(config.demuxer_max_back_bytes, 50 * 1024 * 1024);
        assert!(config.demuxer_donate_buffer);
        assert!(config.cache_pause);
        assert!(!config.cache_pause_initial);
        assert_eq!(config.cache_pause_wait, 1.0);
        assert!(!config.demuxer_cache_wait);
        assert_eq!(config.cache_dir, None);
        assert_eq!(config.unlink_files, CacheUnlinkPolicy::Immediate);
        assert_eq!(config.total_cache_max_bytes, 256 * 1024 * 1024);
        assert_eq!(config.demuxer_max_ranges, 10);
        assert!(config.adaptive_readahead);
        assert!(config.automatic_hysteresis);
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
    fn cache_config_keeps_mpv_prebuffer_defaults_for_network_inputs() {
        let network = PlaybackCacheConfig::default().resolved_for_cacheable_input(true);
        let local = PlaybackCacheConfig::default().resolved_for_cacheable_input(false);

        assert!(!network.demuxer_cache_wait);
        assert!(!network.cache_pause_initial);
        assert_eq!(network.cache_pause_wait, 1.0);
        assert_eq!(network.demuxer_readahead_secs, 1.0);
        assert_eq!(network.demuxer_packet_max_readahead_secs, 0.0);
        assert_eq!(network.demuxer_hysteresis_secs, 0.0);
        assert_eq!(network.demuxer_max_bytes, 150 * 1024 * 1024);
        assert_eq!(network.demuxer_max_back_bytes, 50 * 1024 * 1024);
        assert!(network.demuxer_donate_buffer);
        assert_eq!(network.http_cache_max_bytes, 32 * 1024 * 1024);
        assert_eq!(network.http_cache_range_request_bytes, 16 * 1024 * 1024);
        assert!(!network.disk_cache);
        assert_eq!(network.disk_cache_max_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(network.effective_readahead_secs(true), 1000.0 * 60.0 * 60.0);

        assert!(!local.demuxer_cache_wait);
        assert!(!local.cache_pause_initial);
        assert_eq!(local.cache_pause_wait, 1.0);
        assert_eq!(local.demuxer_readahead_secs, 1.0);
        assert_eq!(local.demuxer_packet_max_readahead_secs, 0.0);
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

    #[test]
    fn finite_total_cache_budget_is_split_in_priority_order() {
        let config = PlaybackCacheConfig {
            total_cache_max_bytes: 96 * 1024 * 1024,
            http_cache_max_bytes: 48 * 1024 * 1024,
            demuxer_max_bytes: 32 * 1024 * 1024,
            demuxer_max_back_bytes: 16 * 1024 * 1024,
            ..PlaybackCacheConfig::default()
        }
        .normalized();
        let (http, forward, back) = config.effective_cache_budgets();

        assert!(http >= config.http_cache_chunk_bytes);
        assert!(forward > 0);
        assert!(back > 0);
        assert!(http + forward + back <= config.total_cache_max_bytes);
        assert_eq!(http, 48 * 1024 * 1024);
        assert_eq!(forward, 32 * 1024 * 1024);
        assert_eq!(back, 16 * 1024 * 1024);
    }

    #[test]
    fn disk_cache_layers_share_one_budget_and_keep_the_http_probe_slice_small() {
        for total in [0, 1, 8, 1024 * 1024, 4 * 1024 * 1024 * 1024, u64::MAX] {
            let config = PlaybackCacheConfig {
                disk_cache_max_bytes: total,
                ..PlaybackCacheConfig::default()
            };
            let (http, demux) = config.effective_disk_cache_budgets();
            assert_eq!(http + demux, total);
            assert!(http <= 32 * 1024 * 1024);
            assert!(http <= total / 8);
        }
    }

    #[test]
    fn finite_total_budget_bounds_an_unlimited_forward_layer() {
        let config = PlaybackCacheConfig {
            total_cache_max_bytes: 8 * 1024 * 1024,
            http_cache_max_bytes: 2 * 1024 * 1024,
            demuxer_max_bytes: 0,
            demuxer_max_back_bytes: 0,
            ..PlaybackCacheConfig::default()
        }
        .normalized();
        let (http, forward, back) = config.effective_cache_budgets();

        assert_eq!(http, 2 * 1024 * 1024);
        assert_eq!(back, 0);
        assert_eq!(forward, config.total_cache_max_bytes - http);
    }

    #[test]
    fn zero_total_preserves_independent_layer_limits() {
        let config = PlaybackCacheConfig {
            total_cache_max_bytes: 0,
            demuxer_max_bytes: 0,
            demuxer_max_back_bytes: 1234,
            ..PlaybackCacheConfig::default()
        }
        .normalized();

        assert_eq!(
            config.effective_cache_budgets(),
            (config.http_cache_max_bytes, 0, 1234)
        );
    }

    #[test]
    fn shared_cache_default_budget_preserves_mpv_forward_and_backward_limits() {
        let config = PlaybackCacheConfig::default().normalized();
        assert_eq!(
            config.effective_cache_budgets(),
            (32 * 1024 * 1024, 150 * 1024 * 1024, 50 * 1024 * 1024)
        );
    }

    #[test]
    fn cache_config_deserializes_older_files_with_new_defaults() {
        let config: PlaybackCacheConfig = serde_json::from_str(r#"{"cache_secs":2.0}"#).unwrap();

        assert_eq!(config.cache_secs, 2.0);
        assert_eq!(config.total_cache_max_bytes, 256 * 1024 * 1024);
        assert_eq!(config.demuxer_max_ranges, 10);
        assert!(config.adaptive_readahead);
        assert!(!config.decoder_framedrop);
    }

    #[test]
    fn decoder_framedrop_requires_explicit_opt_in_and_survives_config_roundtrip() {
        assert!(!PlaybackCacheConfig::default().decoder_framedrop);
        let config: PlaybackCacheConfig =
            serde_json::from_str(r#"{"decoder_framedrop":true}"#).unwrap();
        let encoded = serde_json::to_string(&config.normalized()).unwrap();
        let restored: PlaybackCacheConfig = serde_json::from_str(&encoded).unwrap();
        assert!(restored.decoder_framedrop);
        assert!(
            restored
                .resolved_for_cacheable_input(false)
                .decoder_framedrop
        );
    }

    #[test]
    fn cache_config_serializes_enum_values_in_snake_case() {
        let config = PlaybackCacheConfig {
            unlink_files: CacheUnlinkPolicy::WhenDone,
            ..PlaybackCacheConfig::default()
        };
        let value = serde_json::to_value(config).unwrap();
        assert_eq!(value["unlink_files"], "when_done");
    }
}
