#[path = "cache/buffer.rs"]
mod buffer;
#[path = "cache/config.rs"]
mod config;
#[path = "cache/events.rs"]
mod events;
#[path = "cache/model.rs"]
mod model;
#[path = "cache/shared.rs"]
mod shared;
#[path = "cache/state.rs"]
mod state;
#[path = "cache/storage.rs"]
mod storage;
#[cfg(test)]
#[path = "cache/tests.rs"]
mod tests;

pub(in crate::player::backend::ffmpeg::avio::cache) use super::super::{
    FfmpegControl, HTTP_CACHE_CHUNK_SIZE, HTTP_CACHE_CONTENT_LEN_WAIT,
    HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR, HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR,
    HTTP_CACHE_PARTIAL_READ_MIN_BYTES, HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER,
    HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL, HTTP_CACHE_PROGRESS_REPORT_THRESHOLD,
    HTTP_CACHE_SIDE_DOWNLOAD_WORKERS, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES,
    HTTP_CACHE_STALL_LOG_AFTER, HTTP_CACHE_STALL_LOG_INTERVAL, HTTP_CACHE_WAIT_INTERVAL,
};
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::avio::cache) use super::super::{
    HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS, HTTP_CACHE_DEFAULT_READAHEAD_SECONDS,
    HTTP_CACHE_PROBE_READ_WAIT, HTTP_CACHE_RANGE_REQUEST_BYTES, HTTP_RING_CACHE_CAPACITY,
};
pub(in crate::player::backend::ffmpeg::avio::cache) use super::{
    download::{http_ring_cache_download_loop, http_ring_cache_side_download_loop},
    http::reqwest_header_pairs,
};
pub(in crate::player::backend::ffmpeg::avio::cache) use buffer::ByteRingBuffer;
pub(in crate::player::backend::ffmpeg::avio::cache) use events::{
    http_stream_cache_status_changed, playback_cache_state_from_http_status,
};
pub(super) use model::{CacheAppendPermit, CacheAppendResult, HttpRingCacheShared};
pub(in crate::player::backend::ffmpeg) use model::{
    CacheReadResult, CacheRestartRequest, HttpCacheRangeKind, HttpRingCache, HttpRingCacheState,
};
pub(in crate::player::backend::ffmpeg::avio::cache) use model::{
    HttpCacheConfig, HttpCachedByteRange, HttpDiskCache, HttpPlaybackBufferRange, InputRateSample,
    RetainedCacheRange,
};
