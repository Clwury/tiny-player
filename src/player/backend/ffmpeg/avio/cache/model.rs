use std::{
    collections::VecDeque,
    fs::File,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, mpsc::Sender},
    time::Instant,
};

use crate::player::backend::{BackendEvent, ByteCacheState, CacheUnlinkPolicy};

use super::{ByteRingBuffer, FfmpegControl};

#[derive(Clone)]
pub(in crate::player::backend::ffmpeg) struct HttpRingCache {
    pub(in crate::player::backend::ffmpeg::avio::cache) shared: Arc<HttpRingCacheShared>,
}

pub(in crate::player::backend::ffmpeg::avio) struct HttpRingCacheShared {
    pub(in crate::player::backend::ffmpeg::avio::cache) state: Mutex<HttpRingCacheState>,
    pub(in crate::player::backend::ffmpeg::avio::cache) ready: Condvar,
    pub(in crate::player::backend::ffmpeg::avio::cache) control: Arc<FfmpegControl>,
    pub(in crate::player::backend::ffmpeg::avio::cache) event_tx: Sender<BackendEvent>,
}

pub(in crate::player::backend::ffmpeg) struct HttpRingCacheState {
    pub(in crate::player::backend::ffmpeg::avio::cache) buffer: ByteRingBuffer,
    pub(in crate::player::backend::ffmpeg) base_offset: u64,
    pub(in crate::player::backend::ffmpeg) next_offset: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) active_request_start_offset: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) retained_ranges:
        VecDeque<RetainedCacheRange>,
    pub(in crate::player::backend::ffmpeg::avio::cache) disk_cache: Option<HttpDiskCache>,
    pub(in crate::player::backend::ffmpeg::avio::cache) disk_cache_writable: bool,
    pub(in crate::player::backend::ffmpeg::avio::cache) config: HttpCacheConfig,
    pub(in crate::player::backend::ffmpeg::avio::cache) active_range_kind: HttpCacheRangeKind,
    pub(in crate::player::backend::ffmpeg::avio::cache) pending_seek_range_kind:
        Option<(u64, HttpCacheRangeKind)>,
    pub(in crate::player::backend::ffmpeg::avio::cache) reader_offset: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) byte_level_seeks: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) input_rate_samples:
        VecDeque<InputRateSample>,
    pub(in crate::player::backend::ffmpeg::avio::cache) retained_access_generation: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) duration_seconds: Option<f64>,
    pub(in crate::player::backend::ffmpeg::avio::cache) prefetch_paused: bool,
    pub(in crate::player::backend::ffmpeg::avio::cache) content_len: Option<u64>,
    pub(in crate::player::backend::ffmpeg) eof: bool,
    pub(in crate::player::backend::ffmpeg::avio::cache) shutdown: bool,
    pub(in crate::player::backend::ffmpeg::avio::cache) restart_request:
        Option<CacheRestartRequest>,
    pub(in crate::player::backend::ffmpeg::avio::cache) side_download_requests:
        VecDeque<CacheRestartRequest>,
    pub(in crate::player::backend::ffmpeg::avio::cache) side_download_active:
        Vec<CacheRestartRequest>,
    pub(in crate::player::backend::ffmpeg::avio::cache) error: Option<String>,
    pub(in crate::player::backend::ffmpeg::avio::cache) last_reported_status:
        Option<ByteCacheState>,
}

pub(in crate::player::backend::ffmpeg) enum CacheReadResult {
    Data(usize),
    Eof,
    #[cfg(test)]
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::player::backend::ffmpeg) struct HttpPlaybackBufferRange {
    pub(in crate::player::backend::ffmpeg) start_offset: u64,
    pub(in crate::player::backend::ffmpeg) end_offset: u64,
    pub(in crate::player::backend::ffmpeg) content_len: u64,
}

pub(in crate::player::backend::ffmpeg::avio) enum CacheAppendPermit {
    Ready(usize),
    Full,
    Restart(u64),
    Stopped,
}

pub(in crate::player::backend::ffmpeg::avio) enum CacheAppendResult {
    Appended,
    Restart(u64),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::player::backend::ffmpeg) enum HttpCacheRangeKind {
    Playback,
    TailMetadataProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct CacheRestartRequest {
    pub(in crate::player::backend::ffmpeg) offset: u64,
    pub(in crate::player::backend::ffmpeg) range_kind: HttpCacheRangeKind,
}

pub(in crate::player::backend::ffmpeg::avio::cache) struct RetainedCacheRange {
    pub(in crate::player::backend::ffmpeg::avio::cache) buffer: ByteRingBuffer,
    pub(in crate::player::backend::ffmpeg::avio::cache) base_offset: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) next_offset: u64,
    #[allow(dead_code)]
    pub(in crate::player::backend::ffmpeg::avio::cache) range_kind: HttpCacheRangeKind,
    pub(in crate::player::backend::ffmpeg::avio::cache) last_used_generation: u64,
}

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::avio::cache) struct InputRateSample {
    pub(in crate::player::backend::ffmpeg::avio::cache) at: Instant,
    pub(in crate::player::backend::ffmpeg::avio::cache) bytes: usize,
}

#[derive(Clone)]
pub(in crate::player::backend::ffmpeg::avio::cache) struct HttpCacheConfig {
    pub(in crate::player::backend::ffmpeg::avio::cache) memory_capacity: usize,
    pub(in crate::player::backend::ffmpeg::avio::cache) chunk_size: usize,
    pub(in crate::player::backend::ffmpeg::avio::cache) range_request_bytes: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) readahead_seconds: f64,
    pub(in crate::player::backend::ffmpeg::avio::cache) hysteresis_seconds: f64,
    pub(in crate::player::backend::ffmpeg::avio::cache) max_readahead_bytes: Option<u64>,
    pub(in crate::player::backend::ffmpeg::avio::cache) disk_cache_bytes: Option<u64>,
    pub(in crate::player::backend::ffmpeg::avio::cache) cache_dir: Option<PathBuf>,
    pub(in crate::player::backend::ffmpeg::avio::cache) unlink_files: CacheUnlinkPolicy,
}

pub(in crate::player::backend::ffmpeg::avio::cache) struct HttpDiskCache {
    pub(in crate::player::backend::ffmpeg::avio::cache) file: File,
    pub(in crate::player::backend::ffmpeg::avio::cache) path: PathBuf,
    pub(in crate::player::backend::ffmpeg::avio::cache) ranges: Vec<HttpCachedByteRange>,
    pub(in crate::player::backend::ffmpeg::avio::cache) max_bytes: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) access_generation: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) unlink_on_drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::avio::cache) struct HttpCachedByteRange {
    pub(in crate::player::backend::ffmpeg::avio::cache) start: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) end: u64,
    pub(in crate::player::backend::ffmpeg::avio::cache) last_used_generation: u64,
}
