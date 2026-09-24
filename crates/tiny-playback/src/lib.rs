//! Playback engine shared by the application's playback page and settings.
//!
//! This crate owns decoding, caching, audio, scheduling and video rendering.
//! Outputs use owned BGRA pixels and shared subtitle images. Window-system
//! integration, Emby sessions and preference persistence belong to the caller.
//! Native frames, GPU resources and playback queues are private implementation
//! details; [`VideoOutput`] only connects a backend to a [`VideoPresenter`].
//!
//! Internal rendering modules and native frame types are not an integration API:
//! ```compile_fail
//! use tiny_playback::render_host::DecodedFrame;
//! ```
//! ```compile_fail
//! use tiny_playback::FfmpegFrameRef;
//! ```

#![deny(private_interfaces, private_bounds, unnameable_types)]

mod backend;
mod bitmap;
mod dovi;
mod ffmpeg_dovi;
mod ffmpeg_vulkan;
mod libplacebo;
mod rate;
mod render_host;
mod tracks;
mod video_presenter;
mod volume;

pub use backend::{
    BackendCommand, BackendControl, BackendDiagnostic, BackendError, BackendEvent,
    BackendEventKind, BackendLoadRequest, BackendSubtitleBitmap, BackendSubtitleCue,
    ByteCacheState, CacheStorageState, CacheUnlinkPolicy, DemuxCacheState, FfmpegBackend,
    PlaybackAudioInfo, PlaybackCacheByteRange, PlaybackCacheConfig, PlaybackCacheMode,
    PlaybackCacheState, PlaybackCacheTimeRange, PlaybackFileInfo, PlaybackSeekMode,
    PlaybackSeekableCacheMode, PlaybackVideoInfo, Result, StreamCacheKind, StreamCacheState,
};
pub use bitmap::{BgraImage, SharedBgraImage};
pub use rate::{MAX_PLAYBACK_RATE, MIN_PLAYBACK_RATE, PlaybackRateChange, clamp_playback_rate};
pub use render_host::frame::{PlaybackSessionId, RenderSize};
pub use tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection};
pub use video_presenter::{VideoOutput, VideoPresenter, VideoPresenterSnapshot};
pub use volume::{PlaybackVolumeSettings, clamp_playback_volume};
