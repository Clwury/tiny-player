//! Playback engine shared by the application's playback page and settings.
//!
//! This crate owns decoding, caching, audio, scheduling and video rendering.
//! GPUI image types remain part of the presentation interface; application
//! windows, Emby sessions and preference persistence belong to the caller.

pub mod backend;
mod dovi;
mod ffmpeg_dovi;
mod ffmpeg_vulkan;
mod libplacebo;
pub mod rate;
pub mod render_host;
pub mod tracks;
pub mod video_presenter;
pub mod volume;

pub use backend::{
    BackendCommand, BackendControl, BackendEvent, BackendEventKind, BackendLoadRequest,
    CacheUnlinkPolicy, FfmpegBackend, PlaybackCacheConfig, PlaybackCacheMode,
    PlaybackSeekableCacheMode,
};
pub use render_host::{RenderSize, VideoOutputQueue};
pub use tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection};
pub use video_presenter::VideoPresenter;
pub use volume::PlaybackVolumeSettings;
