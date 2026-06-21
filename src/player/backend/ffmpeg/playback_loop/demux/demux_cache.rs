use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    os::raw::c_int,
    sync::{Arc, Condvar, Mutex, atomic::AtomicU64, mpsc::Sender},
    thread::JoinHandle,
    time::Instant,
};

use crate::player::{
    backend::{
        BackendEvent, BackendEventKind, CacheUnlinkPolicy, DemuxCacheState, PlaybackCacheConfig,
        PlaybackCacheMode, PlaybackCacheState, PlaybackCacheTimeRange, PlaybackSeekMode,
        StreamCacheKind, StreamCacheState,
    },
    render_host::PlaybackSessionId,
};

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::DEMUX_PACKET_CACHE_MEMORY_BYTES;
use super::{
    AvPacket, BufferedReporter, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DEMUX_CACHE_LOCK_TIMING_LOG_AFTER, DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER,
    DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL, DEMUX_PACKET_CACHE_STALL_LOG_AFTER,
    DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL, DEMUX_PACKET_CACHE_WAIT_INTERVAL, FfmpegControl,
    FormatContext, StreamInfo, TimestampMapper, duration_nsecs, ffmpeg_error, nsecs_to_seconds,
    optional_buffered_value_changed, packet_duration_nsecs, packet_is_video_seek_point,
    playback_buffered_near_duration, preroll_seek_position_seconds, seconds_to_nsecs,
    video_cached_seek_preroll_nsecs, video_seek_preroll_nsecs,
};

#[path = "demux_cache/cache.rs"]
mod cache;
#[path = "demux_cache/model.rs"]
mod model;
#[path = "demux_cache/policy.rs"]
mod policy;
#[path = "demux_cache/runtime.rs"]
mod runtime;
#[path = "demux_cache/shared.rs"]
mod shared;
#[path = "demux_cache/state.rs"]
mod state;
#[path = "demux_cache/storage.rs"]
mod storage;
#[path = "demux_cache/telemetry.rs"]
mod telemetry;

#[cfg(test)]
use model::CachedDemuxPacketPayload;
use model::{
    ArchivedStreamPruneCandidate, CachePauseRefresh, CacheStateEmit, CachedDemuxPacket,
    DemuxCacheLockWait, DemuxCachedRange, DemuxCachedSeekHit, DemuxInputRateSample,
    DemuxPacketAppendOutcome, DemuxPacketAppendTiming, DemuxPacketCacheThreadInput,
    DemuxPacketRangeView, DemuxPacketReadSource, DemuxPacketTimeline, DemuxSeekRequest,
    DemuxSelectedStreams, PacketId, RangeId, SeekableTimelineSummary, StreamCacheRangeState,
    StreamForwardState, StreamForwardWindow, ordered_duration_seconds,
};
pub(super) use model::{DemuxPacketCacheInput, DemuxReadResult, DemuxSeekResult};
use policy::*;
use runtime::run_demux_packet_cache;
use storage::{
    DemuxPacketDiskCache, demux_packet_disk_cache_enabled, read_demux_packet_disk_payload,
};
pub(in crate::player::backend::ffmpeg) use telemetry::DemuxReaderWatermark;
use telemetry::{
    DemuxPacketCacheMonitorSnapshot, demux_cache_blocked_on, log_demux_packet_append_timing,
};
pub(super) use telemetry::{
    DemuxPacketCacheReadTiming, DemuxPacketQueueSnapshot, DemuxStreamPacketQueueSnapshot,
};

pub(super) struct DemuxPacketCache {
    shared: Arc<DemuxPacketCacheShared>,
    handle: Option<JoinHandle<()>>,
}

struct DemuxPacketCacheShared {
    state: Mutex<DemuxPacketCacheState>,
    monitor_snapshot: Mutex<DemuxPacketCacheMonitorSnapshot>,
    ready: Condvar,
    control: Arc<FfmpegControl>,
    event_tx: Sender<BackendEvent>,
    clock_start: Instant,
    demux_read_started_nanos: AtomicU64,
    last_would_block_diag_nanos: AtomicU64,
}

struct DemuxPacketCacheState {
    packets: HashMap<PacketId, CachedDemuxPacket>,
    ranges: BTreeMap<RangeId, DemuxCachedRange>,
    disk_cache: Option<DemuxPacketDiskCache>,
    disk_cache_writable: bool,
    read_index: usize,
    consumed_packet_ids: HashSet<PacketId>,
    reader_heads: BTreeMap<c_int, PacketId>,
    reader_head_positions: BTreeMap<c_int, usize>,
    forward_streams: BTreeMap<c_int, StreamForwardState>,
    reader_forward_bytes: usize,
    read_range_id: RangeId,
    append_range_id: RangeId,
    next_range_id: RangeId,
    next_packet_id: PacketId,
    timeline_anchor_stream_index: c_int,
    stream_kinds: BTreeMap<c_int, StreamCacheKind>,
    selected_streams: DemuxSelectedStreams,
    cached_seek_preroll_nsecs: u64,
    memory_limit_bytes: usize,
    backbuffer_limit_bytes: usize,
    donate_backbuffer: bool,
    readahead_nsecs: u64,
    hysteresis_nsecs: u64,
    hysteresis_active: bool,
    cache_pause_enabled: bool,
    cache_pause_initial: bool,
    cache_pause_wait_nsecs: u64,
    cache_buffering_percent: Option<u8>,
    cached_bytes: usize,
    append_maintenance_packets: usize,
    reader_nsecs: u64,
    session_id: PlaybackSessionId,
    seek_request: Option<DemuxSeekRequest>,
    demux_position_detached: bool,
    resume_append_skip_until_nsecs: Option<u64>,
    seeking: bool,
    demux_ts_nsecs: Option<u64>,
    cached_seeks: u64,
    low_level_seeks: u64,
    input_rate_samples: VecDeque<DemuxInputRateSample>,
    last_reported_buffered_until: Option<Option<f64>>,
    last_cache_state_emit_at: Option<Instant>,
    last_emitted_cache_state: Option<PlaybackCacheState>,
    generation: u64,
    error: Option<String>,
    shutdown: bool,
}

#[cfg(test)]
#[path = "demux_cache/tests.rs"]
mod tests;
