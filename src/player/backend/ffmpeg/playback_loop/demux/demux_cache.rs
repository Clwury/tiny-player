use super::*;
use crate::player::backend::PlaybackCacheTimeRange;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL: Duration = Duration::from_millis(250);
const DEMUX_STREAM_PACKET_QUEUE_LIMIT: usize = 2048;
const DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT: usize = 4096;
const DEMUX_READ_SLOW_LOG_AFTER: Duration = Duration::from_millis(200);
// Packet-cache read-ahead cap. Kept small (close to mpv's 1s demuxer-readahead) so the
// demux producer actually reaches the cap and *pauses* between refills. With a large cap
// (e.g. 5s) the per-stream forward window — held down by the lagging audio track at
// ~3-4s — rarely reaches it, so the producer reads continuously and thrashes the single
// cache mutex against the coordinator pump, starving the decoder. Deep buffering for
// seeking / network resilience is provided by the byte-level HTTP/disk cache, not here.
const DEMUX_PACKET_CACHE_MAX_READAHEAD: Duration = Duration::from_secs(3);
const DEMUX_WOULD_BLOCK_DIAG_INTERVAL: Duration = Duration::from_millis(500);

/// Read-ahead target for the demux PACKET cache.
///
/// Only a few seconds of demuxed packets are needed to keep the decoder fed; deep
/// buffering for seeking and network resilience is provided by the byte-level
/// HTTP/disk cache. This is intentionally NOT inflated to `cache_secs`: an unbounded
/// packet read-ahead makes the demux producer thread hot-loop without ever pausing,
/// monopolizing the cache mutex and starving the coordinator pump that feeds the
/// decoder (decode then collapses below realtime, causing perpetual rebuffering).
fn demux_packet_cache_readahead_nsecs(
    cache_config: &PlaybackCacheConfig,
    cache_active: bool,
) -> u64 {
    seconds_to_nsecs(cache_config.effective_readahead_secs(cache_active))
        .min(duration_nsecs(DEMUX_PACKET_CACHE_MAX_READAHEAD))
}

/// Hysteresis band for the demux PACKET cache read-ahead.
///
/// The default config sets no hysteresis (mpv parity). But unlike mpv — whose demuxer
/// thread does not share a mutex with the playback consumer — tiny's demux producer and
/// the coordinator pump contend on a single cache mutex. With zero hysteresis the
/// producer resumes reading the instant `forward` dips below the read-ahead target, so it
/// wakes to read+append on *every* consumed packet, thrashing the lock against the pump
/// and starving the decoder. Inject a band (when none is configured) so the producer
/// parks between refills and the pump gets long uncontended windows to feed the decoder.
fn demux_packet_cache_hysteresis_nsecs(
    cache_config: &PlaybackCacheConfig,
    readahead_nsecs: u64,
) -> u64 {
    let configured = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
    if configured > 0 {
        configured
    } else {
        readahead_nsecs / 2
    }
}

pub(super) struct DemuxPacketCache {
    shared: Arc<DemuxPacketCacheShared>,
    handle: Option<JoinHandle<()>>,
}

pub(super) struct DemuxPacketCacheInput {
    pub(super) input: FormatContext,
    pub(super) video_stream: StreamInfo,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) subtitle_stream: Option<StreamInfo>,
    pub(super) duration_seconds: Option<f64>,
    pub(super) start_position_seconds: f64,
    pub(super) session_id: PlaybackSessionId,
    pub(super) cache_config: PlaybackCacheConfig,
}

struct DemuxPacketCacheThreadInput {
    input: FormatContext,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    duration_seconds: Option<f64>,
    start_position_seconds: f64,
    session_id: PlaybackSessionId,
}

pub(super) enum DemuxReadResult {
    Packet(AvPacket),
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy)]
enum DemuxCacheLockWait {
    None,
    Bounded(Duration),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DemuxPacketQueueSnapshot {
    pub(super) total_packets: usize,
    pub(super) total_bytes: usize,
    pub(super) memory_limit_bytes: usize,
    pub(super) streams: Vec<DemuxStreamPacketQueueSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DemuxStreamPacketQueueSnapshot {
    pub(super) stream_index: c_int,
    pub(super) kind: StreamCacheKind,
    pub(super) queued_packets: usize,
    pub(super) packet_limit: usize,
    pub(super) packet_queue_full: bool,
    pub(super) queued_bytes: usize,
    pub(super) forward_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DemuxSeekResult {
    Cached,
    Requested,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct DemuxReaderWatermark {
    pub(in crate::player::backend::ffmpeg) video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) audio_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) selected_min_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) video_underrun: bool,
    pub(in crate::player::backend::ffmpeg) audio_underrun: bool,
    pub(in crate::player::backend::ffmpeg) video_idle: bool,
    pub(in crate::player::backend::ffmpeg) audio_idle: bool,
    pub(in crate::player::backend::ffmpeg) underrun: bool,
    pub(in crate::player::backend::ffmpeg) idle: bool,
    pub(in crate::player::backend::ffmpeg) forward_bytes: u64,
}

struct DemuxPacketCacheShared {
    state: Mutex<DemuxPacketCacheState>,
    ready: Condvar,
    control: Arc<FfmpegControl>,
    event_tx: Sender<BackendEvent>,
    clock_start: Instant,
    demux_read_started_nanos: AtomicU64,
    last_would_block_diag_nanos: AtomicU64,
}

type PacketId = u64;
type RangeId = u64;

struct DemuxPacketCacheState {
    packets: HashMap<PacketId, CachedDemuxPacket>,
    ranges: BTreeMap<RangeId, DemuxCachedRange>,
    disk_cache: Option<DemuxPacketDiskCache>,
    disk_cache_writable: bool,
    read_index: usize,
    consumed_packet_ids: HashSet<PacketId>,
    reader_heads: BTreeMap<c_int, PacketId>,
    read_range_id: RangeId,
    append_range_id: RangeId,
    next_range_id: RangeId,
    next_packet_id: PacketId,
    timeline_anchor_stream_index: c_int,
    stream_kinds: BTreeMap<c_int, StreamCacheKind>,
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
    generation: u64,
    error: Option<String>,
    shutdown: bool,
}

struct DemuxPacketAppendOutcome {
    appended: bool,
    force_cache_state_report: bool,
}

#[derive(Clone, Copy)]
struct DemuxInputRateSample {
    at: Instant,
    bytes: usize,
}

struct DemuxCachedRange {
    id: RangeId,
    global_order: VecDeque<PacketId>,
    stream_queues: BTreeMap<c_int, VecDeque<PacketId>>,
    sparse_stream_pruned_until_nsecs: BTreeMap<c_int, u64>,
    is_bof: bool,
    is_eof: bool,
    last_used_generation: u64,
}

struct DemuxPacketRangeView<'a> {
    global_order: &'a VecDeque<u64>,
    stream_queues: &'a BTreeMap<c_int, VecDeque<u64>>,
    is_bof: bool,
    is_eof: bool,
}

struct ArchivedStreamPruneCandidate {
    stream_index: c_int,
    prune_always: bool,
    seek_start_nsecs: Option<u64>,
}

struct DemuxCachedSeekHit {
    reader_heads: BTreeMap<c_int, PacketId>,
    buffered_until_nsecs: u64,
}

#[derive(Clone, Copy)]
struct StreamForwardWindow {
    stream_index: c_int,
    kind: StreamCacheKind,
    reader_nsecs: u64,
    end_nsecs: u64,
    has_forward_packet: bool,
}

impl StreamForwardWindow {
    fn duration_nsecs(self) -> u64 {
        self.end_nsecs.saturating_sub(self.reader_nsecs)
    }
}

#[derive(Default)]
struct StreamCacheRangeState {
    reader_nsecs: Option<u64>,
    cache_end_nsecs: Option<u64>,
    has_forward_packet: bool,
}

struct CachedDemuxPacket {
    payload: CachedDemuxPacketPayload,
    stream_index: c_int,
    timeline_anchor: bool,
    recovery_point: bool,
    start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
    byte_len: usize,
}

enum CachedDemuxPacketPayload {
    Memory(AvPacket),
    Disk {
        props: AvPacket,
        offset: u64,
        len: usize,
    },
}

struct DemuxPacketDiskCache {
    file: File,
    path: PathBuf,
    next_offset: u64,
    max_bytes: u64,
    unlink_on_drop: bool,
}

#[derive(Clone, Copy)]
struct DemuxSeekRequest {
    position_seconds: f64,
    session_id: PlaybackSessionId,
    seek_generation: u64,
}

struct DemuxPacketTimeline {
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    video_frame_duration_nsecs: u64,
    current_start_position_nsecs: u64,
    video_clock: TimestampMapper,
    audio_clock: TimestampMapper,
    subtitle_clock: TimestampMapper,
    buffered_reporter: BufferedReporter,
    session_id: PlaybackSessionId,
}

impl DemuxPacketCache {
    pub(super) fn spawn(
        cache_input: DemuxPacketCacheInput,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let DemuxPacketCacheInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds,
            session_id,
            cache_config,
        } = cache_input;
        let start_position_seconds = start_position_seconds.max(0.0);
        let start_position_nsecs = seconds_to_nsecs(start_position_seconds);
        let mut state = DemuxPacketCacheState::new(
            start_position_nsecs,
            video_stream.index,
            video_stream.codec_id,
            session_id,
            cache_config,
        );
        if let Some(audio_stream) = audio_stream {
            state.set_stream_kind(audio_stream.index, StreamCacheKind::Audio);
        }
        if let Some(subtitle_stream) = subtitle_stream {
            state.set_stream_kind(subtitle_stream.index, StreamCacheKind::Subtitle);
        }
        let shared = Arc::new(DemuxPacketCacheShared {
            state: Mutex::new(state),
            ready: Condvar::new(),
            control,
            event_tx,
            clock_start: Instant::now(),
            demux_read_started_nanos: AtomicU64::new(0),
            last_would_block_diag_nanos: AtomicU64::new(0),
        });
        shared.enter_initial_cache_pause_if_needed();
        let thread_shared = Arc::clone(&shared);
        let thread_input = DemuxPacketCacheThreadInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds,
            session_id,
        };
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-demux-cache".to_string())
            .spawn(move || run_demux_packet_cache(thread_input, thread_shared))
            .map_err(|error| format!("创建 FFmpeg demux 缓存线程失败：{error}"))?;

        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    #[allow(dead_code)]
    pub(super) fn poll_packet(&self, stream_index: c_int) -> DemuxReadResult {
        self.poll_packet_round_robin(&[stream_index]).0
    }

    #[cfg(test)]
    pub(super) fn read_packet_round_robin(
        &self,
        stream_indices: &[c_int],
    ) -> (DemuxReadResult, Option<usize>) {
        self.read_packet_round_robin_inner(stream_indices, true, DemuxCacheLockWait::None, false)
    }

    pub(super) fn poll_packet_round_robin(
        &self,
        stream_indices: &[c_int],
    ) -> (DemuxReadResult, Option<usize>) {
        self.read_packet_round_robin_inner(stream_indices, false, DemuxCacheLockWait::None, false)
    }

    #[cfg(test)]
    pub(super) fn read_available_packet_round_robin_with_lock_wait(
        &self,
        stream_indices: &[c_int],
        lock_wait: Duration,
    ) -> (DemuxReadResult, Option<usize>) {
        self.read_available_packet_round_robin_with_cache_pause_signal(
            stream_indices,
            lock_wait,
            false,
        )
    }

    pub(super) fn read_available_packet_round_robin_with_cache_pause_signal(
        &self,
        stream_indices: &[c_int],
        lock_wait: Duration,
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>) {
        self.read_packet_round_robin_inner(
            stream_indices,
            false,
            DemuxCacheLockWait::Bounded(lock_wait),
            cache_pause_signal,
        )
    }

    pub(super) fn packet_queue_snapshot(&self) -> DemuxPacketQueueSnapshot {
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.packet_queue_snapshot()
    }

    pub(super) fn reader_watermark(&self) -> DemuxReaderWatermark {
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.reader_watermark()
    }

    pub(super) fn try_monitor_snapshot(
        &self,
    ) -> Option<(DemuxPacketQueueSnapshot, DemuxReaderWatermark)> {
        let guard =
            self.try_lock_state(DemuxCacheLockWait::Bounded(DEMUX_PACKET_CACHE_LOCK_WAIT))?;
        Some((guard.packet_queue_snapshot(), guard.reader_watermark()))
    }

    pub(super) fn demux_read_blocked_for(&self) -> Option<Duration> {
        self.shared.demux_read_blocked_for()
    }

    fn read_packet_round_robin_inner(
        &self,
        stream_indices: &[c_int],
        wait_for_data: bool,
        lock_wait: DemuxCacheLockWait,
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>) {
        let mut guard = if wait_for_data {
            self.lock_state_unbounded()
        } else {
            match self.try_lock_state(lock_wait) {
                Some(guard) => guard,
                None => return (DemuxReadResult::WouldBlock, None),
            }
        };
        let mut logged_wait = false;
        let mut wait_started_at = None;
        let mut next_stall_log_at = None;
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return (DemuxReadResult::Interrupted, None);
            }
            if let Some(error) = guard.error.clone() {
                return (DemuxReadResult::Error(error), None);
            }
            if self.shared.refresh_cache_pause(&mut guard) {
                self.shared.emit_cache_state(&mut guard);
            }
            let packet = match guard.take_packet_round_robin(stream_indices) {
                Ok(packet) => packet,
                Err(error) => return (DemuxReadResult::Error(error), None),
            };
            if let Some((packet, stream_offset)) = packet {
                guard.refresh_readahead_hysteresis();
                if wait_for_data {
                    self.shared.emit_cache_state_after_read(&mut guard, true);
                }
                self.shared.ready.notify_all();
                return (DemuxReadResult::Packet(packet), Some(stream_offset));
            }
            if guard.activate_detached_append_range() {
                self.shared.ready.notify_all();
                continue;
            }
            if self.shared.control.is_cache_paused() && !guard.cache_pause_recovered() {
                if !wait_for_data {
                    return (DemuxReadResult::WouldBlock, None);
                }
                let (next_guard, _) = self
                    .shared
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                guard = next_guard;
                continue;
            }
            if guard.read_range_eof() {
                return (DemuxReadResult::Eof, None);
            }
            if guard.demux_position_detached {
                let session_id = guard.session_id;
                let seek_generation = self.shared.control.seek_generation();
                let continuation_seconds = nsecs_to_seconds(guard.reader_nsecs);
                guard.request_continuation_seek(seek_generation);
                tracing::debug!(
                    ?session_id,
                    position_seconds = continuation_seconds,
                    seek_generation,
                    generation = guard.generation,
                    "FFmpeg demux packet cache exhausted selected stream queues; requested low-level continuation seek"
                );
                self.shared.emit_cache_state(&mut guard);
                self.shared.ready.notify_all();
                continue;
            }
            self.shared
                .enter_cache_pause_if_needed(&mut guard, cache_pause_signal);
            if !logged_wait && !self.shared.control.is_cache_paused() && guard.has_demux_underrun()
            {
                self.shared.emit_cache_state(&mut guard);
            }
            if self.shared.should_log_would_block_diagnostic() {
                guard.log_would_block_diagnostic(stream_indices);
            }
            if !wait_for_data {
                return (DemuxReadResult::WouldBlock, None);
            }
            let now = Instant::now();
            let wait_started = *wait_started_at.get_or_insert(now);
            if !logged_wait {
                let cache_paused = self.shared.control.is_cache_paused();
                let queue_snapshot = guard.packet_queue_snapshot();
                tracing::trace!(
                    session_id = ?guard.session_id,
                    blocked_on = demux_cache_blocked_on(&guard, cache_paused),
                    streams = ?stream_indices,
                    queued_packets = queue_snapshot.total_packets,
                    queued_bytes = queue_snapshot.total_bytes,
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused,
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "waiting for FFmpeg demux per-stream packet queues"
                );
                logged_wait = true;
                next_stall_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
            } else if next_stall_log_at.is_some_and(|deadline| now >= deadline) {
                let cache_paused = self.shared.control.is_cache_paused();
                let queue_snapshot = guard.packet_queue_snapshot();
                tracing::debug!(
                    session_id = ?guard.session_id,
                    blocked_on = demux_cache_blocked_on(&guard, cache_paused),
                    waited_ms = now.saturating_duration_since(wait_started).as_millis(),
                    streams = ?stream_indices,
                    queued_packets = queue_snapshot.total_packets,
                    queued_bytes = queue_snapshot.total_bytes,
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused,
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "still waiting for FFmpeg demux per-stream packet queues"
                );
                next_stall_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL);
            }
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            guard = next_guard;
        }
    }

    fn lock_state_unbounded(&self) -> std::sync::MutexGuard<'_, DemuxPacketCacheState> {
        self.shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
    }

    fn try_lock_state(
        &self,
        lock_wait: DemuxCacheLockWait,
    ) -> Option<std::sync::MutexGuard<'_, DemuxPacketCacheState>> {
        match lock_wait {
            DemuxCacheLockWait::None => self.try_lock_state_once(),
            DemuxCacheLockWait::Bounded(lock_wait) => {
                let deadline = Instant::now().checked_add(lock_wait);
                loop {
                    if let Some(guard) = self.try_lock_state_once() {
                        return Some(guard);
                    }
                    if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                        return None;
                    }
                    thread::yield_now();
                }
            }
        }
    }

    fn try_lock_state_once(&self) -> Option<std::sync::MutexGuard<'_, DemuxPacketCacheState>> {
        match self.shared.state.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("FFmpeg demux packet cache poisoned")
            }
        }
    }

    pub(super) fn seek(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        let (result, should_enter_initial_cache_pause, cache_state, buffered_changed) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = None;
            if let Some(buffered_until) =
                guard.seek_cached_with_generation(target_nsecs, mode, session_id, seek_generation)
            {
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    ?mode,
                    target_nsecs,
                    seek_generation,
                    buffered_until,
                    read_index = guard.read_index,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek hit"
                );
                let cache_state = guard.playback_cache_state(self.shared.control.is_cache_paused());
                let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
                guard.record_cache_state_emit(Instant::now());
                (
                    DemuxSeekResult::Cached,
                    false,
                    cache_state,
                    buffered_changed,
                )
            } else {
                guard.request_seek(position_seconds, session_id, seek_generation, target_nsecs);
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    ?mode,
                    target_nsecs,
                    seek_generation,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek miss; requested low-level seek"
                );
                let cache_state = guard.playback_cache_state(self.shared.control.is_cache_paused());
                let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
                guard.record_cache_state_emit(Instant::now());
                (
                    DemuxSeekResult::Requested,
                    guard.cache_pause_initial,
                    cache_state,
                    buffered_changed,
                )
            }
        };
        self.shared.ready.notify_all();
        self.shared
            .send_cache_state_events(session_id, cache_state, buffered_changed);
        if should_enter_initial_cache_pause {
            self.shared.enter_initial_cache_pause_if_needed();
        }
        result
    }

    pub(super) fn shutdown(&self) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.shutdown = true;
        self.shared.ready.notify_all();
    }

    pub(super) fn apply_cache_config(&self, cache_config: PlaybackCacheConfig) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let had_cache_buffering = guard.cache_buffering_percent.is_some();
        guard.apply_cache_config(cache_config);
        if !guard.cache_pause_enabled {
            let changed = self.shared.control.is_cache_paused()
                && self.shared.control.set_cache_paused(false);
            if had_cache_buffering {
                let _ = self.shared.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
            }
            if changed {
                let _ = self.shared.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::PausedForCacheChanged(false),
                ));
                let _ = self.shared.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::Pause(self.shared.control.is_paused()),
                ));
            }
        }
        self.shared.refresh_cache_pause(&mut guard);
        self.shared.emit_cache_state(&mut guard);
        self.shared.ready.notify_all();
    }

    pub(super) fn wait_until_initial_cache_fill(&self) -> std::result::Result<(), String> {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let wait_started_at = Instant::now();
        let mut next_initial_wait_log_at =
            wait_started_at.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return Ok(());
            }
            if self.shared.control.has_pending_seek() {
                return Ok(());
            }
            if let Some(error) = guard.error.clone() {
                return Err(error);
            }
            if guard.initial_cache_fill_complete() {
                return Ok(());
            }
            let now = Instant::now();
            if next_initial_wait_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    waited_ms = now.saturating_duration_since(wait_started_at).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.shared.control.is_cache_paused(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                    "still waiting for initial FFmpeg demux cache fill"
                );
                next_initial_wait_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL);
            }
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            guard = next_guard;
        }
    }
}

fn demux_cache_blocked_on(state: &DemuxPacketCacheState, cache_paused: bool) -> &'static str {
    if cache_paused && !state.cache_pause_recovered() {
        "demux_cache_pause"
    } else if state.seeking {
        "demux_seek"
    } else if state.has_demux_underrun() {
        "demux_cache_underrun"
    } else if state.stream_packet_queue_full() {
        "packet_queue_full"
    } else if state.should_pause_demux() {
        "demux_packet_cache_full"
    } else {
        "demux_cache"
    }
}

impl Drop for DemuxPacketCache {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl DemuxPacketCacheState {
    fn new(
        reader_nsecs: u64,
        timeline_anchor_stream_index: c_int,
        timeline_anchor_codec_id: ffi::AVCodecID,
        session_id: PlaybackSessionId,
        cache_config: PlaybackCacheConfig,
    ) -> Self {
        let cache_config = cache_config.normalized();
        let disk_cache = DemuxPacketDiskCache::from_config(&cache_config);
        let disk_cache_writable = disk_cache.is_some();
        let memory_limit_bytes =
            usize::try_from(cache_config.demuxer_max_bytes).unwrap_or(usize::MAX);
        let cache_active = !matches!(cache_config.mode, PlaybackCacheMode::Disabled);
        let seekable_cache_active = cache_config.seekable_cache_active(cache_active);
        let backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.demuxer_max_back_bytes).unwrap_or(usize::MAX)
        } else {
            0
        };
        let readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        let hysteresis_nsecs = demux_packet_cache_hysteresis_nsecs(&cache_config, readahead_nsecs);
        let cache_pause_wait_nsecs = seconds_to_nsecs(cache_config.cache_pause_wait);
        let mut stream_kinds = BTreeMap::new();
        stream_kinds.insert(timeline_anchor_stream_index, StreamCacheKind::Video);
        let mut ranges = BTreeMap::new();
        ranges.insert(
            0,
            DemuxCachedRange {
                id: 0,
                global_order: VecDeque::new(),
                stream_queues: BTreeMap::new(),
                sparse_stream_pruned_until_nsecs: BTreeMap::new(),
                is_bof: reader_nsecs == 0,
                is_eof: false,
                last_used_generation: 0,
            },
        );
        Self {
            packets: HashMap::new(),
            ranges,
            disk_cache,
            disk_cache_writable,
            read_index: 0,
            consumed_packet_ids: HashSet::new(),
            reader_heads: BTreeMap::new(),
            read_range_id: 0,
            append_range_id: 0,
            next_range_id: 1,
            next_packet_id: 0,
            timeline_anchor_stream_index,
            stream_kinds,
            cached_seek_preroll_nsecs: video_cached_seek_preroll_nsecs(timeline_anchor_codec_id),
            memory_limit_bytes,
            backbuffer_limit_bytes,
            donate_backbuffer: cache_config.demuxer_donate_buffer,
            readahead_nsecs,
            hysteresis_nsecs,
            hysteresis_active: false,
            cache_pause_enabled: cache_active && cache_config.cache_pause,
            cache_pause_initial: cache_config.cache_pause_initial,
            cache_pause_wait_nsecs,
            cache_buffering_percent: None,
            cached_bytes: 0,
            reader_nsecs,
            session_id,
            seek_request: None,
            demux_position_detached: false,
            resume_append_skip_until_nsecs: None,
            seeking: false,
            demux_ts_nsecs: None,
            cached_seeks: 0,
            low_level_seeks: 0,
            input_rate_samples: VecDeque::new(),
            last_reported_buffered_until: None,
            last_cache_state_emit_at: None,
            generation: 0,
            error: None,
            shutdown: false,
        }
    }

    fn set_stream_kind(&mut self, stream_index: c_int, kind: StreamCacheKind) {
        self.stream_kinds.insert(stream_index, kind);
    }

    fn apply_cache_config(&mut self, cache_config: PlaybackCacheConfig) {
        let cache_config = cache_config.normalized();
        let cache_active = !matches!(cache_config.mode, PlaybackCacheMode::Disabled);
        let seekable_cache_active = cache_config.seekable_cache_active(cache_active);

        self.memory_limit_bytes =
            usize::try_from(cache_config.demuxer_max_bytes).unwrap_or(usize::MAX);
        self.backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.demuxer_max_back_bytes).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.donate_backbuffer = cache_config.demuxer_donate_buffer;
        self.readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        self.hysteresis_nsecs =
            demux_packet_cache_hysteresis_nsecs(&cache_config, self.readahead_nsecs);
        if self.hysteresis_nsecs == 0 {
            self.hysteresis_active = false;
        }
        self.cache_pause_enabled = cache_active && cache_config.cache_pause;
        self.cache_pause_initial = cache_config.cache_pause_initial;
        self.cache_pause_wait_nsecs = seconds_to_nsecs(cache_config.cache_pause_wait);
        if !self.cache_pause_enabled {
            self.cache_buffering_percent = None;
        }

        let disk_cache_requested = cache_config.disk_cache || demux_packet_disk_cache_enabled();
        if disk_cache_requested {
            if self.disk_cache.is_none() {
                self.disk_cache = DemuxPacketDiskCache::from_config(&cache_config);
            }
            self.disk_cache_writable = self.disk_cache.is_some();
        } else {
            self.disk_cache_writable = false;
        }

        self.trim_to_limit();
        self.refresh_readahead_hysteresis();
    }

    fn append_packet(&mut self, mut packet: CachedDemuxPacket) -> DemuxPacketAppendOutcome {
        self.record_input_bytes(packet.byte_len);
        if let Some(demux_ts_nsecs) = packet.start_nsecs.or(packet.end_nsecs) {
            self.demux_ts_nsecs = Some(demux_ts_nsecs);
        }
        if self.should_skip_resume_overlap_packet(&packet) {
            return DemuxPacketAppendOutcome {
                appended: false,
                force_cache_state_report: false,
            };
        }
        let packet_id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.saturating_add(1);
        let stream_index = packet.stream_index;
        if self.disk_cache_writable
            && let Some(disk_cache) = self.disk_cache.as_mut()
            && let Err(error) = packet.spill_to_disk(disk_cache)
        {
            tracing::warn!(%error, "pausing FFmpeg demux packet disk cache writes");
            self.disk_cache_writable = false;
        }
        let cleared_seek = self.seeking;
        self.cached_bytes = self.cached_bytes.saturating_add(packet.byte_len);
        self.append_packet_id_to_append_range(packet_id, stream_index);
        self.packets.insert(packet_id, packet);
        self.maybe_start_reader_head_for_appended_packet(packet_id, stream_index);
        self.seeking = false;
        let pruned = self.trim_to_limit();
        self.refresh_readahead_hysteresis();
        DemuxPacketAppendOutcome {
            appended: true,
            force_cache_state_report: cleared_seek || pruned || self.should_pause_demux(),
        }
    }

    fn append_packet_id_to_append_range(&mut self, packet_id: PacketId, stream_index: c_int) {
        let range = self.append_range_mut();
        range.global_order.push_back(packet_id);
        range
            .stream_queues
            .entry(stream_index)
            .or_default()
            .push_back(packet_id);
    }

    fn maybe_start_reader_head_for_appended_packet(
        &mut self,
        packet_id: PacketId,
        stream_index: c_int,
    ) {
        if self.append_range_id != self.read_range_id {
            return;
        }
        if self.reader_heads.contains_key(&stream_index) {
            return;
        }
        self.reader_heads.insert(stream_index, packet_id);
        self.refresh_reader_tracking();
    }

    fn record_input_bytes(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.input_rate_samples.push_back(DemuxInputRateSample {
            at: Instant::now(),
            bytes,
        });
        self.prune_input_rate_samples();
    }

    fn raw_input_rate(&self) -> Option<u64> {
        let now = Instant::now();
        let bytes: usize = self
            .input_rate_samples
            .iter()
            .filter(|sample| now.saturating_duration_since(sample.at) <= Duration::from_secs(1))
            .map(|sample| sample.bytes)
            .sum();
        (bytes > 0).then(|| u64::try_from(bytes).unwrap_or(u64::MAX))
    }

    fn prune_input_rate_samples(&mut self) {
        let now = Instant::now();
        while self
            .input_rate_samples
            .front()
            .is_some_and(|sample| now.saturating_duration_since(sample.at) > Duration::from_secs(1))
        {
            self.input_rate_samples.pop_front();
        }
    }

    fn should_skip_resume_overlap_packet(&mut self, packet: &CachedDemuxPacket) -> bool {
        let Some(skip_until_nsecs) = self.resume_append_skip_until_nsecs else {
            return false;
        };
        let packet_end_nsecs = packet.end_nsecs.or(packet.start_nsecs);
        if packet_end_nsecs.is_some_and(|end_nsecs| end_nsecs <= skip_until_nsecs) {
            return true;
        }
        if packet
            .start_nsecs
            .is_some_and(|start_nsecs| start_nsecs >= skip_until_nsecs)
            || packet_end_nsecs.is_none()
        {
            self.resume_append_skip_until_nsecs = None;
        }
        false
    }

    fn packet_ref(&self, packet_id: u64) -> std::result::Result<AvPacket, String> {
        let Some(packet) = self.packets.get(&packet_id) else {
            return Err("FFmpeg demux packet cache entry missing".to_string());
        };
        packet.packet_ref(self.disk_cache.as_ref())
    }

    fn packet_end_nsecs(&self, packet_id: u64) -> Option<u64> {
        self.packets
            .get(&packet_id)
            .and_then(|packet| packet.end_nsecs)
    }

    fn take_packet_round_robin(
        &mut self,
        stream_indices: &[c_int],
    ) -> std::result::Result<Option<(AvPacket, usize)>, String> {
        for (stream_offset, stream_index) in stream_indices.iter().copied().enumerate() {
            let Some(packet_id) = self.next_packet_id_for_stream(stream_index) else {
                continue;
            };
            let packet = self.packet_ref(packet_id)?;
            if let Some(end_nsecs) = self.packet_end_nsecs(packet_id) {
                self.reader_nsecs = self.reader_nsecs.max(end_nsecs);
            }
            self.consume_packet_id(packet_id);
            return Ok(Some((packet, stream_offset)));
        }
        Ok(None)
    }

    fn next_packet_id_for_stream(&self, stream_index: c_int) -> Option<PacketId> {
        let packet_id = self.reader_heads.get(&stream_index).copied()?;
        self.read_range()
            .stream_queues
            .get(&stream_index)
            .is_some_and(|queue| queue.iter().any(|candidate| *candidate == packet_id))
            .then_some(packet_id)
            .filter(|packet_id| self.packets.contains_key(packet_id))
    }

    fn consume_packet_id(&mut self, packet_id: PacketId) {
        self.advance_reader_head_over_packet(packet_id);
        self.read_range_mut().is_bof = false;
        self.trim_to_limit();
    }

    fn advance_reader_head_over_packet(&mut self, packet_id: PacketId) {
        let Some(packet) = self.packets.get(&packet_id) else {
            return;
        };
        let stream_index = packet.stream_index;
        if self.reader_heads.get(&stream_index).copied() != Some(packet_id) {
            return;
        }
        let next_packet_id = self
            .read_range()
            .stream_queues
            .get(&stream_index)
            .and_then(|queue| {
                let position = queue.iter().position(|candidate| *candidate == packet_id)?;
                queue
                    .iter()
                    .skip(position.saturating_add(1))
                    .copied()
                    .find(|candidate| self.packets.contains_key(candidate))
            });
        match next_packet_id {
            Some(next_packet_id) => {
                self.reader_heads.insert(stream_index, next_packet_id);
            }
            None => {
                self.reader_heads.remove(&stream_index);
            }
        }
        self.refresh_reader_tracking();
    }

    fn reset_reader_heads_for_read_index(&mut self) {
        let range_len = self.read_range().global_order.len();
        let read_index = self.read_index.min(range_len);
        let packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        let mut reader_heads = BTreeMap::new();
        for (stream_index, queue) in &self.read_range().stream_queues {
            let Some(packet_id) = queue.iter().copied().find(|packet_id| {
                self.packets.contains_key(packet_id)
                    && packet_positions
                        .get(packet_id)
                        .is_some_and(|position| *position >= read_index)
            }) else {
                continue;
            };
            reader_heads.insert(*stream_index, packet_id);
        }
        self.reader_heads = reader_heads;
        self.refresh_reader_tracking();
    }

    fn refresh_reader_tracking(&mut self) {
        let packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        let stream_queues = self.read_range().stream_queues.clone();
        self.reader_heads.retain(|stream_index, packet_id| {
            self.packets.contains_key(packet_id)
                && stream_queues
                    .get(stream_index)
                    .is_some_and(|queue| queue.iter().any(|candidate| *candidate == *packet_id))
        });

        self.read_index = self
            .reader_heads
            .values()
            .filter_map(|packet_id| packet_positions.get(packet_id).copied())
            .min()
            .unwrap_or_else(|| self.read_range().global_order.len());

        let mut consumed = HashSet::new();
        for (stream_index, queue) in &self.read_range().stream_queues {
            let reader_head = self.reader_heads.get(stream_index).copied();
            for packet_id in queue {
                if Some(*packet_id) == reader_head {
                    break;
                }
                consumed.insert(*packet_id);
            }
        }
        self.consumed_packet_ids = consumed;
    }

    fn packet_queue_snapshot(&self) -> DemuxPacketQueueSnapshot {
        let mut stream_ids = self
            .read_range()
            .stream_queues
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if let Some(range) = self.detached_append_range() {
            for stream_index in range.stream_queues.keys().copied() {
                if !stream_ids.contains(&stream_index) {
                    stream_ids.push(stream_index);
                }
            }
        }
        stream_ids.sort_unstable();

        let forward_by_kind = self
            .reader_stream_forward_windows()
            .into_iter()
            .map(|window| (window.kind, window.duration_nsecs()))
            .collect::<Vec<_>>();
        let mut streams = Vec::new();
        for stream_index in stream_ids {
            let kind = self
                .stream_kinds
                .get(&stream_index)
                .copied()
                .unwrap_or(StreamCacheKind::Unknown);
            let queued_packets = self.queued_packet_count_for_stream(stream_index);
            if queued_packets == 0 {
                continue;
            }
            let packet_limit = self.stream_packet_queue_limit(stream_index);
            let queued_bytes = self.queued_bytes_for_stream(stream_index);
            let forward_nsecs = forward_by_kind
                .iter()
                .find_map(|(window_kind, duration)| (*window_kind == kind).then_some(*duration));
            streams.push(DemuxStreamPacketQueueSnapshot {
                stream_index,
                kind,
                queued_packets,
                packet_limit,
                packet_queue_full: queued_packets >= packet_limit,
                queued_bytes,
                forward_nsecs,
            });
        }
        DemuxPacketQueueSnapshot {
            total_packets: streams.iter().map(|stream| stream.queued_packets).sum(),
            total_bytes: streams.iter().map(|stream| stream.queued_bytes).sum(),
            memory_limit_bytes: self.memory_limit_bytes,
            streams,
        }
    }

    fn queued_packet_count_for_stream(&self, stream_index: c_int) -> usize {
        let active_count = self
            .read_range()
            .stream_queues
            .get(&stream_index)
            .map(|queue| {
                queue
                    .iter()
                    .filter(|packet_id| self.active_packet_is_forward(**packet_id))
                    .count()
            })
            .unwrap_or_default();
        let detached_count = self
            .detached_append_range()
            .and_then(|range| range.stream_queues.get(&stream_index))
            .map(|queue| {
                queue
                    .iter()
                    .filter(|packet_id| self.packets.contains_key(packet_id))
                    .count()
            })
            .unwrap_or_default();
        active_count + detached_count
    }

    fn queued_bytes_for_stream(&self, stream_index: c_int) -> usize {
        let active_bytes: usize = self
            .read_range()
            .stream_queues
            .get(&stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter())
            .filter(|packet_id| self.active_packet_is_forward(**packet_id))
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum();
        let detached_bytes: usize = self
            .detached_append_range()
            .and_then(|range| range.stream_queues.get(&stream_index))
            .into_iter()
            .flat_map(|queue| queue.iter())
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum();
        active_bytes + detached_bytes
    }

    fn active_packet_is_forward(&self, packet_id: PacketId) -> bool {
        let Some(packet) = self.packets.get(&packet_id) else {
            return false;
        };
        let Some(reader_head) = self.reader_heads.get(&packet.stream_index).copied() else {
            return false;
        };
        let Some(queue) = self.read_range().stream_queues.get(&packet.stream_index) else {
            return false;
        };
        queue
            .iter()
            .position(|candidate| *candidate == packet_id)
            .is_some_and(|packet_position| {
                queue
                    .iter()
                    .position(|candidate| *candidate == reader_head)
                    .is_some_and(|reader_position| packet_position >= reader_position)
            })
    }

    fn stream_packet_queue_limit(&self, stream_index: c_int) -> usize {
        match self
            .stream_kinds
            .get(&stream_index)
            .copied()
            .unwrap_or(StreamCacheKind::Unknown)
        {
            StreamCacheKind::Subtitle => DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT,
            StreamCacheKind::Video | StreamCacheKind::Audio | StreamCacheKind::Unknown => {
                DEMUX_STREAM_PACKET_QUEUE_LIMIT
            }
        }
    }

    fn stream_packet_queue_full(&self) -> bool {
        let active_append_range = self.append_range_id == self.read_range_id;
        self.append_range()
            .stream_queues
            .iter()
            .any(|(stream_index, queue)| {
                let queued_packets = if active_append_range {
                    queue
                        .iter()
                        .filter(|packet_id| self.active_packet_is_forward(**packet_id))
                        .count()
                } else {
                    queue
                        .iter()
                        .filter(|packet_id| self.packets.contains_key(packet_id))
                        .count()
                };
                queued_packets >= self.stream_packet_queue_limit(*stream_index)
            })
    }

    #[cfg(test)]
    fn seek_cached(&mut self, target_nsecs: u64, session_id: PlaybackSessionId) -> Option<f64> {
        self.seek_cached_with_generation(target_nsecs, PlaybackSeekMode::Precise, session_id, 0)
    }

    #[cfg(test)]
    fn seek_cached_fast(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<f64> {
        self.seek_cached_with_generation(target_nsecs, PlaybackSeekMode::Fast, session_id, 0)
    }

    fn seek_cached_with_generation(
        &mut self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Option<f64> {
        let current_hit = self.seek_cached_in_range(self.read_range(), target_nsecs, mode);
        if let Some(hit) = current_hit {
            let buffered_until_nsecs = hit.buffered_until_nsecs;
            self.reader_heads = hit.reader_heads;
            self.refresh_reader_tracking();
            self.reader_nsecs = target_nsecs;
            self.session_id = session_id;
            self.seek_request = None;
            self.seeking = false;
            self.trim_to_limit();
            self.generation = self.generation.saturating_add(1);
            self.cached_seeks = self.cached_seeks.saturating_add(1);
            self.refresh_readahead_hysteresis();
            return Some(nsecs_to_seconds(buffered_until_nsecs));
        }

        let detached_append_range_id = self.detached_append_range_id();
        let detached_hit = detached_append_range_id.and_then(|range_id| {
            self.ranges.get(&range_id).and_then(|range| {
                self.seek_cached_in_range(range, target_nsecs, mode)
                    .map(|hit| (range_id, hit))
            })
        });
        if let Some((range_id, hit)) = detached_hit {
            let buffered_until_nsecs = hit.buffered_until_nsecs;
            self.preserve_current_range();
            self.activate_range_for_read_with_heads(range_id, hit.reader_heads);
            self.reader_nsecs = target_nsecs;
            self.session_id = session_id;
            self.seek_request = None;
            self.seeking = false;
            self.trim_to_limit();
            self.generation = self.generation.saturating_add(1);
            self.cached_seeks = self.cached_seeks.saturating_add(1);
            self.refresh_readahead_hysteresis();
            return Some(nsecs_to_seconds(buffered_until_nsecs));
        }

        let hit = self
            .ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .find_map(|(range_id, range)| {
                self.seek_cached_in_range(range, target_nsecs, mode)
                    .map(|hit| (*range_id, hit))
            });
        let (range_id, hit) = hit?;
        let buffered_until_nsecs = hit.buffered_until_nsecs;
        self.preserve_current_range();
        self.activate_range_for_read_with_heads(range_id, hit.reader_heads);
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = None;
        self.seeking = false;
        if !self.read_range_eof() {
            self.queue_resume_seek_after_cached_range(buffered_until_nsecs, seek_generation);
        }
        self.trim_to_limit();
        self.generation = self.generation.saturating_add(1);
        self.cached_seeks = self.cached_seeks.saturating_add(1);
        self.refresh_readahead_hysteresis();
        Some(nsecs_to_seconds(buffered_until_nsecs))
    }

    fn seek_cached_in_range(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
    ) -> Option<DemuxCachedSeekHit> {
        let eager_seekable_until = self.range_cached_seekable_until(range, target_nsecs)?;
        let cached_seek_preroll_nsecs = match mode {
            PlaybackSeekMode::Precise => self.cached_seek_preroll_nsecs,
            PlaybackSeekMode::Fast => 0,
        };
        let mut hit = Self::seek_cached_in_packet_range(
            &self.packets,
            self.timeline_anchor_stream_index,
            cached_seek_preroll_nsecs,
            DemuxPacketRangeView {
                global_order: &range.global_order,
                stream_queues: &range.stream_queues,
                is_bof: range.is_bof,
                is_eof: range.is_eof,
            },
            target_nsecs,
        )?;
        hit.buffered_until_nsecs = hit.buffered_until_nsecs.min(eager_seekable_until);
        Some(hit)
    }

    fn range_cached_seekable_until(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
    ) -> Option<u64> {
        let ranges = self.range_seekable_timeline_ranges(range);
        let first = ranges.first().copied()?;
        let last = ranges.last().copied()?;

        if target_nsecs < first.0 {
            return range.is_bof.then_some(first.1);
        }
        if target_nsecs > last.1 {
            return range.is_eof.then_some(last.1);
        }
        ranges
            .into_iter()
            .find(|(start, end)| *start <= target_nsecs && target_nsecs <= *end)
            .map(|(_, end)| end)
    }

    fn activate_range_for_read(&mut self, range_id: RangeId, read_index: usize) {
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        let range_len = self.read_range().global_order.len();
        self.read_range_mut().last_used_generation = self.generation;
        self.read_index = read_index.min(range_len);
        self.reset_reader_heads_for_read_index();
    }

    fn activate_range_for_read_with_heads(
        &mut self,
        range_id: RangeId,
        reader_heads: BTreeMap<c_int, PacketId>,
    ) {
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        self.read_range_mut().last_used_generation = self.generation;
        self.reader_heads = reader_heads;
        self.refresh_reader_tracking();
    }

    fn queue_resume_seek_after_cached_range(
        &mut self,
        buffered_until_nsecs: u64,
        seek_generation: u64,
    ) {
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds: nsecs_to_seconds(buffered_until_nsecs),
            session_id: self.session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = Some(buffered_until_nsecs);
        self.start_detached_append_range();
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.read_range_mut().is_eof = false;
        self.hysteresis_active = false;
    }

    fn request_seek(
        &mut self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        seek_generation: u64,
        target_nsecs: u64,
    ) {
        tracing::debug!(
            ?session_id,
            position_seconds,
            seek_generation,
            target_nsecs,
            packet_count = self.packets.len(),
            global_packet_count = self.read_range().global_order.len(),
            archived_range_count = self.ranges.len(),
            read_index = self.read_index,
            previous_generation = self.generation,
            "preserving FFmpeg demux packet cache range for low-level seek"
        );
        self.preserve_current_range();
        self.preserve_detached_append_range();
        self.reader_heads.clear();
        self.read_index = 0;
        self.consumed_packet_ids.clear();
        self.cache_buffering_percent = None;
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.start_new_current_range(target_nsecs == 0);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.generation = self.generation.saturating_add(1);
        self.hysteresis_active = false;
        self.trim_to_limit();
    }

    fn request_continuation_seek(&mut self, seek_generation: u64) {
        let position_seconds = nsecs_to_seconds(self.reader_nsecs);
        let session_id = self.session_id;
        self.preserve_current_range();
        self.preserve_detached_append_range();
        self.reader_heads.clear();
        self.read_index = 0;
        self.consumed_packet_ids.clear();
        self.cache_buffering_percent = None;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.start_new_current_range(false);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.generation = self.generation.saturating_add(1);
        self.hysteresis_active = false;
        self.trim_to_limit();
    }

    fn take_seek_request(&mut self) -> Option<DemuxSeekRequest> {
        self.seek_request.take()
    }

    fn should_pause_demux(&self) -> bool {
        if self.demux_position_detached {
            return true;
        }
        if self.selected_eager_stream_needs_packet() {
            return false;
        }
        if self.stream_packet_queue_full() {
            return true;
        }
        if self.memory_limit_bytes > 0 && self.forward_bytes() >= self.memory_limit_bytes {
            return true;
        }
        let forward_duration = self.forward_duration_nsecs();
        if forward_duration >= self.readahead_nsecs {
            return true;
        }
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        self.hysteresis_active && self.hysteresis_nsecs > 0 && forward_duration > resume_threshold
    }

    fn reader_watermark(&self) -> DemuxReaderWatermark {
        let mut video_forward_nsecs = None;
        let mut audio_forward_nsecs = None;
        let mut selected_min_forward_nsecs = None;
        let mut video_seen = false;
        let mut audio_seen = false;
        let mut video_underrun = false;
        let mut audio_underrun = false;
        let mut video_idle = false;
        let mut audio_idle = false;
        let reader_windows = self.reader_stream_forward_windows();
        for window in reader_windows.iter().copied() {
            let duration_nsecs = window.duration_nsecs();
            let stream_idle = self.stream_window_idle(window);
            let stream_underrun = self.stream_window_underrun(window);
            match window.kind {
                StreamCacheKind::Video => {
                    video_underrun |= stream_underrun;
                    video_idle = if video_seen {
                        video_idle && stream_idle
                    } else {
                        stream_idle
                    };
                    video_seen = true;
                    video_forward_nsecs = Some(
                        video_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                    selected_min_forward_nsecs = Some(
                        selected_min_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                }
                StreamCacheKind::Audio => {
                    audio_underrun |= stream_underrun;
                    audio_idle = if audio_seen {
                        audio_idle && stream_idle
                    } else {
                        stream_idle
                    };
                    audio_seen = true;
                    audio_forward_nsecs = Some(
                        audio_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                    selected_min_forward_nsecs = Some(
                        selected_min_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                }
                StreamCacheKind::Subtitle | StreamCacheKind::Unknown => {}
            }
        }
        DemuxReaderWatermark {
            video_forward_nsecs,
            audio_forward_nsecs,
            selected_min_forward_nsecs,
            video_underrun,
            audio_underrun,
            video_idle: video_seen && video_idle,
            audio_idle: audio_seen && audio_idle,
            underrun: reader_windows
                .into_iter()
                .any(|window| self.stream_window_underrun(window)),
            idle: self.effective_eof()
                || (!video_underrun
                    && !audio_underrun
                    && selected_min_forward_nsecs.is_some()
                    && self.should_pause_demux()),
            forward_bytes: u64::try_from(self.reader_forward_bytes()).unwrap_or(u64::MAX),
        }
    }

    fn selected_eager_stream_needs_packet(&self) -> bool {
        self.active_stream_forward_windows()
            .into_iter()
            .any(|window| self.stream_window_needs_reader_packet(window))
    }

    fn initial_cache_fill_complete(&self) -> bool {
        if self.cache_pause_enabled && self.cache_pause_initial && self.cache_pause_wait_nsecs > 0 {
            self.cache_pause_recovered()
        } else {
            self.effective_eof() || self.should_pause_demux()
        }
    }

    fn refresh_readahead_hysteresis(&mut self) {
        if self.hysteresis_nsecs == 0 {
            self.hysteresis_active = false;
            return;
        }
        let forward_duration = self.forward_duration_nsecs();
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        if self.hysteresis_active {
            if forward_duration <= resume_threshold {
                self.hysteresis_active = false;
            }
        } else if forward_duration >= self.readahead_nsecs {
            self.hysteresis_active = true;
        }
    }

    fn cached_until_nsecs(&self) -> Option<u64> {
        let active_end = self
            .cached_timeline_range()
            .map(|(_, buffered_until_nsecs)| buffered_until_nsecs);
        let detached_end = self.detached_append_range().and_then(|range| {
            Self::cached_timeline_range_in_packet_range(
                &self.packets,
                self.timeline_anchor_stream_index,
                &range.stream_queues,
            )
            .map(|(_, buffered_until_nsecs)| buffered_until_nsecs)
        });
        active_end.into_iter().chain(detached_end).max()
    }

    fn forward_duration_nsecs(&self) -> u64 {
        self.selected_forward_timeline_window()
            .map(StreamForwardWindow::duration_nsecs)
            .unwrap_or_else(|| {
                self.cached_until_nsecs()
                    .map(|cached_until| cached_until.saturating_sub(self.reader_nsecs))
                    .unwrap_or_default()
            })
    }

    fn cache_pause_percent(&self) -> Option<u8> {
        if self.cache_pause_wait_nsecs == 0 {
            return None;
        }
        let percent =
            self.forward_duration_nsecs().saturating_mul(100) / self.cache_pause_wait_nsecs;
        Some(u8::try_from(percent.min(99)).unwrap_or(99))
    }

    fn cache_pause_can_enter(&self, require_demux_underrun: bool) -> bool {
        self.cache_pause_enabled
            && self.cache_pause_wait_nsecs > 0
            && !self.effective_eof()
            && !self.cache_pause_recovered()
            && (!require_demux_underrun || self.has_demux_underrun())
    }

    fn cache_pause_recovered(&self) -> bool {
        self.effective_eof()
            || self.forward_duration_nsecs() >= self.cache_pause_wait_nsecs
            || self.should_pause_demux()
    }

    fn mark_eof(&mut self) {
        self.seeking = false;
        if let Some(range) = self.detached_append_range_mut() {
            range.is_eof = true;
        } else {
            self.read_range_mut().is_eof = true;
        }
    }

    fn effective_eof(&self) -> bool {
        self.read_range_eof()
            || self.detached_append_range().is_some_and(|range| {
                range.is_eof && self.read_index >= self.read_range().global_order.len()
            })
    }

    fn trim_to_limit(&mut self) -> bool {
        let mut pruned = false;
        while self.backward_bytes() > self.effective_backbuffer_limit() {
            if !self.prune_oldest_backbuffer_range() && !self.prune_active_stream_prefix() {
                break;
            }
            pruned = true;
        }
        pruned
    }

    fn prune_active_stream_prefix(&mut self) -> bool {
        let Some(candidate) = self.active_stream_prune_candidate() else {
            return false;
        };
        let stream_index = candidate.stream_index;
        let Some(prune_count) = self.active_stream_prefix_prune_count(stream_index) else {
            return false;
        };
        if prune_count == 0 {
            return false;
        }
        let range_id = self.read_range_id;
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return false;
        };
        self.remove_range_stream_prefix_packets(&mut range, stream_index, prune_count);
        self.ranges.insert(range_id, range);
        self.refresh_reader_tracking();
        true
    }

    fn active_stream_prune_candidate(&self) -> Option<ArchivedStreamPruneCandidate> {
        self.read_range()
            .stream_queues
            .iter()
            .filter(|(stream_index, queue)| {
                queue.front().is_some_and(|packet_id| {
                    Some(*packet_id) != self.reader_heads.get(stream_index).copied()
                })
            })
            .map(|(stream_index, queue)| {
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(*stream_index, queue);
                let prune_always = self.backbuffer_limit_bytes == 0
                    || seek_start_nsecs.is_none()
                    || head_packet.is_none_or(|packet| {
                        !self.packet_is_stream_seek_boundary(*stream_index, packet)
                    });
                ArchivedStreamPruneCandidate {
                    stream_index: *stream_index,
                    prune_always,
                    seek_start_nsecs,
                }
            })
            .min_by(
                |left, right| match (left.prune_always, right.prune_always) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    (true, true) => left.stream_index.cmp(&right.stream_index),
                    (false, false) => left
                        .seek_start_nsecs
                        .cmp(&right.seek_start_nsecs)
                        .then_with(|| left.stream_index.cmp(&right.stream_index)),
                },
            )
    }

    fn active_stream_prefix_prune_count(&self, stream_index: c_int) -> Option<usize> {
        let queue = self.read_range().stream_queues.get(&stream_index)?;
        let reader_head = self.reader_heads.get(&stream_index).copied();
        let head_is_boundary = queue
            .front()
            .and_then(|packet_id| self.packets.get(packet_id))
            .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet));
        let starts_with_non_boundary = !head_is_boundary;
        let mut boundary_was_pruned = false;
        let mut prune_count = 0;
        let seekable_cache = self.backbuffer_limit_bytes > 0;
        for packet_id in queue {
            if Some(*packet_id) == reader_head {
                break;
            }
            let is_boundary = self
                .packets
                .get(packet_id)
                .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet));
            if is_boundary {
                if seekable_cache && (boundary_was_pruned || starts_with_non_boundary) {
                    break;
                }
                boundary_was_pruned = true;
            }
            prune_count += 1;
        }
        (prune_count > 0).then_some(prune_count)
    }

    fn prune_oldest_backbuffer_range(&mut self) -> bool {
        let detached_append_range_id = self.detached_append_range_id();
        let Some(range_id) = self
            .ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .min_by_key(|(_, range)| range.last_used_generation)
            .map(|(range_id, _)| *range_id)
        else {
            return false;
        };
        if self.prune_archived_stream_prefix(range_id) {
            return true;
        }
        let Some(range) = self.ranges.remove(&range_id) else {
            return false;
        };
        self.remove_range_packets(range);
        true
    }

    fn prune_archived_stream_prefix(&mut self, range_id: RangeId) -> bool {
        let Some(mut range) = self.ranges.remove(&range_id) else {
            return false;
        };
        let Some(candidate) = self.archived_stream_prune_candidate(&range) else {
            self.ranges.insert(range_id, range);
            return false;
        };
        let stream_index = candidate.stream_index;
        let Some(prune_count) = self.archived_stream_prefix_prune_count(&range, stream_index)
        else {
            self.ranges.insert(range_id, range);
            return false;
        };
        if prune_count == 0 {
            self.ranges.insert(range_id, range);
            return false;
        }
        self.remove_range_stream_prefix_packets(&mut range, stream_index, prune_count);
        if range.global_order.is_empty() {
            return true;
        }
        self.ranges.insert(range_id, range);
        true
    }

    fn archived_stream_prune_candidate(
        &self,
        range: &DemuxCachedRange,
    ) -> Option<ArchivedStreamPruneCandidate> {
        range
            .stream_queues
            .iter()
            .filter_map(|(stream_index, queue)| {
                let head_packet = queue
                    .front()
                    .and_then(|packet_id| self.packets.get(packet_id));
                let seek_start_nsecs = self.stream_queue_seek_start_nsecs(*stream_index, queue);
                let prune_always = self.backbuffer_limit_bytes == 0
                    || seek_start_nsecs.is_none()
                    || head_packet.is_none_or(|packet| {
                        !self.packet_is_stream_seek_boundary(*stream_index, packet)
                    });
                if head_packet.is_none() && queue.is_empty() {
                    return None;
                }
                Some(ArchivedStreamPruneCandidate {
                    stream_index: *stream_index,
                    prune_always,
                    seek_start_nsecs,
                })
            })
            .min_by(
                |left, right| match (left.prune_always, right.prune_always) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    (true, true) => left.stream_index.cmp(&right.stream_index),
                    (false, false) => left
                        .seek_start_nsecs
                        .cmp(&right.seek_start_nsecs)
                        .then_with(|| left.stream_index.cmp(&right.stream_index)),
                },
            )
    }

    fn stream_queue_seek_start_nsecs(
        &self,
        stream_index: c_int,
        queue: &VecDeque<PacketId>,
    ) -> Option<u64> {
        if stream_index == self.timeline_anchor_stream_index {
            let mut segment = SeekableTimelineSegment::default();
            let mut ranges = Vec::new();
            for packet_id in queue {
                let Some(packet) = self.packets.get(packet_id) else {
                    continue;
                };
                if !packet.timeline_anchor {
                    continue;
                }
                let Some(start_nsecs) = packet.start_nsecs else {
                    continue;
                };
                let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
                segment.push_packet(start_nsecs, end_nsecs, packet.recovery_point, 0);
            }
            segment.finish_into(&mut ranges);
            return ranges.first().map(|(start_nsecs, _)| *start_nsecs);
        }

        queue.iter().find_map(|packet_id| {
            let packet = self.packets.get(packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            (end_nsecs >= start_nsecs).then_some(start_nsecs)
        })
    }

    fn archived_stream_prefix_prune_count(
        &self,
        range: &DemuxCachedRange,
        stream_index: c_int,
    ) -> Option<usize> {
        let queue = range.stream_queues.get(&stream_index)?;
        let head_is_boundary = queue
            .front()
            .and_then(|packet_id| self.packets.get(packet_id))
            .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet));
        let starts_with_non_boundary = !head_is_boundary;
        let mut boundary_was_pruned = false;
        let mut prune_count = 0;
        let seekable_cache = self.backbuffer_limit_bytes > 0;
        for packet_id in queue {
            let is_boundary = self
                .packets
                .get(packet_id)
                .is_some_and(|packet| self.packet_is_stream_seek_boundary(stream_index, packet));
            if is_boundary {
                if seekable_cache && (boundary_was_pruned || starts_with_non_boundary) {
                    break;
                }
                boundary_was_pruned = true;
            }
            prune_count += 1;
        }
        (prune_count > 0).then_some(prune_count)
    }

    fn packet_is_stream_seek_boundary(
        &self,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
    ) -> bool {
        Self::packet_is_stream_seek_boundary_for(
            self.timeline_anchor_stream_index,
            stream_index,
            packet,
        )
    }

    fn packet_is_stream_seek_boundary_for(
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        packet: &CachedDemuxPacket,
    ) -> bool {
        if stream_index == timeline_anchor_stream_index {
            return packet.timeline_anchor && packet.recovery_point && packet.start_nsecs.is_some();
        }
        packet.start_nsecs.is_some()
    }

    fn remove_range_stream_prefix_packets(
        &mut self,
        range: &mut DemuxCachedRange,
        stream_index: c_int,
        count: usize,
    ) {
        let removed = {
            let Some(queue) = range.stream_queues.get_mut(&stream_index) else {
                return;
            };
            let count = count.min(queue.len());
            let removed = queue.drain(..count).collect::<Vec<_>>();
            if queue.is_empty() {
                range.stream_queues.remove(&stream_index);
            }
            removed
        };
        if removed.is_empty() {
            return;
        }
        let sparse_pruned_until_nsecs = matches!(
            self.stream_kinds.get(&stream_index),
            Some(StreamCacheKind::Subtitle)
        )
        .then(|| {
            removed
                .iter()
                .filter_map(|packet_id| {
                    self.packets
                        .get(packet_id)
                        .and_then(|packet| packet.end_nsecs.or(packet.start_nsecs))
                })
                .max()
        })
        .flatten();
        range.is_bof = false;
        if let Some(pruned_until_nsecs) = sparse_pruned_until_nsecs {
            range
                .sparse_stream_pruned_until_nsecs
                .entry(stream_index)
                .and_modify(|existing| *existing = (*existing).max(pruned_until_nsecs))
                .or_insert(pruned_until_nsecs);
        }
        for packet_id in removed {
            self.consumed_packet_ids.remove(&packet_id);
            if let Some(global_index) = range
                .global_order
                .iter()
                .position(|candidate| *candidate == packet_id)
            {
                range.global_order.remove(global_index);
            }
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            }
        }
    }

    fn backward_bytes(&self) -> usize {
        self.cached_bytes.saturating_sub(self.forward_bytes())
    }

    fn effective_backbuffer_limit(&self) -> usize {
        if self.backbuffer_limit_bytes == 0 {
            return 0;
        }
        if !self.donate_backbuffer {
            return self.backbuffer_limit_bytes;
        }
        let forward_bytes = self.forward_bytes();
        let Some(forward_with_guard) = forward_bytes.checked_add(1) else {
            return self.backbuffer_limit_bytes;
        };
        if self.memory_limit_bytes <= forward_with_guard {
            return self.backbuffer_limit_bytes;
        }
        self.backbuffer_limit_bytes
            .saturating_add(self.memory_limit_bytes - forward_with_guard)
    }

    fn timeline_anchor_packet_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.read_range()
            .stream_queues
            .get(&self.timeline_anchor_stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter().copied())
            .filter(|packet_id| {
                self.packets
                    .get(packet_id)
                    .is_some_and(|packet| packet.timeline_anchor && packet.start_nsecs.is_some())
            })
    }

    fn cached_timeline_range(&self) -> Option<(u64, u64)> {
        let mut first_cached_nsecs = None;
        let mut buffered_until_nsecs = None;
        for packet_id in self.timeline_anchor_packet_ids() {
            let packet = self.packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            first_cached_nsecs = Some(first_cached_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            buffered_until_nsecs = Some(buffered_until_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        first_cached_nsecs.zip(buffered_until_nsecs)
    }

    fn playback_cache_state(&self, paused_for_cache: bool) -> PlaybackCacheState {
        let forward_window = self.selected_forward_timeline_window();
        let cache_end = forward_window
            .map(|window| window.end_nsecs)
            .or_else(|| self.cached_until_nsecs())
            .map(nsecs_to_seconds);
        let reader_pts = Some(nsecs_to_seconds(
            forward_window
                .map(|window| window.reader_nsecs)
                .unwrap_or(self.reader_nsecs),
        ));
        let cache_duration = forward_window
            .map(|window| nsecs_to_seconds(window.duration_nsecs()))
            .or_else(|| {
                ordered_duration_seconds(Some(self.reader_nsecs), self.cached_until_nsecs())
            });
        let seekable_ranges = self.seekable_time_ranges();
        let active_has_seekable_range = self.active_seekable_timeline_ranges().next().is_some();
        let detached_append_range_id = self.detached_append_range_id();
        let detached_has_seekable_range = self
            .detached_append_range()
            .is_some_and(|range| self.range_has_seekable_timeline(range));
        let bof_cached = (active_has_seekable_range && self.read_range().is_bof)
            || (detached_has_seekable_range
                && self
                    .detached_append_range()
                    .is_some_and(|range| range.is_bof))
            || self
                .ranges
                .iter()
                .filter(|(range_id, _)| **range_id != self.read_range_id)
                .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
                .any(|(_, range)| range.is_bof && self.range_has_seekable_timeline(range));
        let eof_cached = (active_has_seekable_range && self.read_range_eof())
            || (detached_has_seekable_range
                && self
                    .detached_append_range()
                    .is_some_and(|range| range.is_eof))
            || self
                .ranges
                .iter()
                .filter(|(range_id, _)| **range_id != self.read_range_id)
                .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
                .any(|(_, range)| range.is_eof && self.range_has_seekable_timeline(range));

        PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end,
                reader_pts,
                cache_duration,
                eof: self.effective_eof(),
                underrun: self.has_demux_underrun(),
                idle: self.effective_eof() || self.should_pause_demux(),
                seeking: self.seeking || self.seek_request.is_some(),
                bof_cached,
                eof_cached,
                total_bytes: u64::try_from(self.cached_bytes).unwrap_or(u64::MAX),
                forward_bytes: u64::try_from(self.forward_bytes()).unwrap_or(u64::MAX),
                file_cache_bytes: self.disk_cache.as_ref().map(|cache| cache.next_offset),
                raw_input_rate: self.raw_input_rate(),
                ts_last: self.demux_ts_nsecs.map(nsecs_to_seconds),
                cached_seeks: self.cached_seeks,
                low_level_seeks: self.low_level_seeks,
                byte_level_seeks: 0,
                seekable_ranges,
                streams: self.stream_cache_states(),
            },
            byte: None,
            paused_for_cache,
            buffering_percent: self.cache_buffering_percent,
        }
    }

    fn take_buffered_changed_for_cache_state(
        &mut self,
        cache_state: &PlaybackCacheState,
    ) -> Option<Option<f64>> {
        let buffered_until = cache_state.demux.cache_end;
        let changed = self
            .last_reported_buffered_until
            .map(|previous| optional_buffered_value_changed(previous, buffered_until))
            .unwrap_or(buffered_until.is_some());
        if !changed {
            return None;
        }
        self.last_reported_buffered_until = Some(buffered_until);
        Some(buffered_until)
    }

    fn cache_state_report_due(&self, now: Instant) -> bool {
        self.last_cache_state_emit_at
            .and_then(|last| now.checked_duration_since(last))
            .is_none_or(|elapsed| elapsed >= DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL)
    }

    fn record_cache_state_emit(&mut self, now: Instant) {
        self.last_cache_state_emit_at = Some(now);
    }

    fn forward_bytes(&self) -> usize {
        let active_bytes: usize = self
            .read_range()
            .global_order
            .iter()
            .filter(|packet_id| self.active_packet_is_forward(**packet_id))
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum();
        active_bytes.saturating_add(
            self.detached_append_range()
                .map(|range| self.range_bytes(range))
                .unwrap_or_default(),
        )
    }

    fn reader_forward_bytes(&self) -> usize {
        self.read_range()
            .global_order
            .iter()
            .filter(|packet_id| self.active_packet_is_forward(**packet_id))
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
    }

    fn range_bytes(&self, range: &DemuxCachedRange) -> usize {
        range
            .global_order
            .iter()
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
    }

    fn has_demux_underrun(&self) -> bool {
        if self.effective_eof() || self.should_pause_demux() {
            return false;
        }
        let detached_has_packets = self
            .detached_append_range()
            .is_some_and(|range| !range.global_order.is_empty());
        if self.read_index >= self.read_range().global_order.len() && !detached_has_packets {
            return true;
        }
        self.active_stream_forward_windows()
            .into_iter()
            .any(|window| self.stream_window_underrun(window))
    }

    fn selected_forward_timeline_window(&self) -> Option<StreamForwardWindow> {
        let mut windows = self.active_stream_forward_windows();
        if windows.is_empty() {
            return None;
        }
        let has_non_subtitle = windows
            .iter()
            .any(|window| !matches!(window.kind, StreamCacheKind::Subtitle));
        windows
            .drain(..)
            .filter(|window| {
                !(has_non_subtitle
                    && matches!(window.kind, StreamCacheKind::Subtitle)
                    && window.duration_nsecs() == 0)
            })
            .min_by_key(|window| window.duration_nsecs())
    }

    fn stream_window_idle(&self, window: StreamForwardWindow) -> bool {
        if self.effective_eof() || self.demux_position_detached {
            return true;
        }
        if self.stream_window_needs_reader_packet(window) {
            return false;
        }
        if self.memory_limit_bytes > 0 && self.forward_bytes() >= self.memory_limit_bytes {
            return true;
        }
        if self
            .append_range()
            .stream_queues
            .get(&window.stream_index)
            .is_some_and(|queue| {
                let queued_packets = if self.append_range_id == self.read_range_id {
                    queue
                        .iter()
                        .filter(|packet_id| self.active_packet_is_forward(**packet_id))
                        .count()
                } else {
                    queue
                        .iter()
                        .filter(|packet_id| self.packets.contains_key(packet_id))
                        .count()
                };
                queued_packets >= self.stream_packet_queue_limit(window.stream_index)
            })
        {
            return true;
        }
        let forward_duration = window.duration_nsecs();
        if forward_duration >= self.readahead_nsecs {
            return true;
        }
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        self.hysteresis_active && self.hysteresis_nsecs > 0 && forward_duration > resume_threshold
    }

    fn stream_window_underrun(&self, window: StreamForwardWindow) -> bool {
        self.stream_window_needs_reader_packet(window) && !self.stream_window_idle(window)
    }

    fn stream_window_needs_reader_packet(&self, window: StreamForwardWindow) -> bool {
        !self.effective_eof()
            && !self.demux_position_detached
            && matches!(window.kind, StreamCacheKind::Video | StreamCacheKind::Audio)
            && !window.has_forward_packet
    }

    fn active_stream_forward_windows(&self) -> Vec<StreamForwardWindow> {
        self.stream_forward_windows(true)
    }

    fn reader_stream_forward_windows(&self) -> Vec<StreamForwardWindow> {
        self.stream_forward_windows(false)
    }

    fn stream_forward_windows(&self, include_detached: bool) -> Vec<StreamForwardWindow> {
        let read_range = self.read_range();
        let detached_append_range = include_detached
            .then(|| self.detached_append_range())
            .flatten();

        let mut windows = Vec::new();
        for (stream_index, kind) in &self.stream_kinds {
            let mut reader_nsecs = None;
            let mut end_nsecs = None;
            let mut has_forward_packet = false;
            if let Some(queue) = read_range.stream_queues.get(stream_index) {
                for packet_id in queue {
                    if !self.active_packet_is_forward(*packet_id) {
                        continue;
                    }
                    let Some(packet) = self.packets.get(packet_id) else {
                        continue;
                    };
                    has_forward_packet = true;
                    if let Some(start_nsecs) = packet.start_nsecs {
                        reader_nsecs = Some(reader_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
                    }
                    if let Some(packet_end_nsecs) = packet.end_nsecs.or(packet.start_nsecs) {
                        end_nsecs =
                            Some(end_nsecs.unwrap_or(packet_end_nsecs).max(packet_end_nsecs));
                    }
                }
            }
            if let Some(queue) =
                detached_append_range.and_then(|range| range.stream_queues.get(stream_index))
            {
                for packet_id in queue {
                    let Some(packet) = self.packets.get(packet_id) else {
                        continue;
                    };
                    has_forward_packet = true;
                    if let Some(start_nsecs) = packet.start_nsecs {
                        reader_nsecs = Some(reader_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
                    }
                    if let Some(packet_end_nsecs) = packet.end_nsecs.or(packet.start_nsecs) {
                        end_nsecs =
                            Some(end_nsecs.unwrap_or(packet_end_nsecs).max(packet_end_nsecs));
                    }
                }
            }

            match reader_nsecs.zip(end_nsecs) {
                Some((reader_nsecs, end_nsecs)) => windows.push(StreamForwardWindow {
                    stream_index: *stream_index,
                    kind: *kind,
                    reader_nsecs,
                    end_nsecs,
                    has_forward_packet,
                }),
                None if has_forward_packet || !self.read_range_eof() => {
                    windows.push(StreamForwardWindow {
                        stream_index: *stream_index,
                        kind: *kind,
                        reader_nsecs: self.reader_nsecs,
                        end_nsecs: self.reader_nsecs,
                        has_forward_packet,
                    })
                }
                None => {}
            }
        }
        windows
    }

    fn seekable_time_ranges(&self) -> Vec<PlaybackCacheTimeRange> {
        let mut ranges = Vec::new();
        self.collect_seekable_time_ranges(self.read_range(), &mut ranges);
        if let Some(range) = self.detached_append_range() {
            self.collect_seekable_time_ranges(range, &mut ranges);
        }
        let detached_append_range_id = self.detached_append_range_id();
        for (range_id, range) in &self.ranges {
            if *range_id == self.read_range_id {
                continue;
            }
            if Some(*range_id) == detached_append_range_id {
                continue;
            }
            self.collect_seekable_time_ranges(range, &mut ranges);
        }
        ranges.sort_by(|left, right| {
            left.start
                .partial_cmp(&right.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut merged: Vec<PlaybackCacheTimeRange> = Vec::new();
        for range in ranges {
            if range.end < range.start {
                continue;
            }
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
                continue;
            }
            merged.push(range);
        }
        merged
    }

    fn collect_seekable_time_ranges(
        &self,
        range: &DemuxCachedRange,
        ranges: &mut Vec<PlaybackCacheTimeRange>,
    ) {
        for (start, end) in self.range_seekable_timeline_ranges(range) {
            ranges.push(PlaybackCacheTimeRange {
                start: nsecs_to_seconds(start),
                end: nsecs_to_seconds(end),
            });
        }
    }

    fn active_seekable_timeline_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.range_seekable_timeline_ranges(self.read_range())
            .into_iter()
    }

    fn range_has_seekable_timeline(&self, range: &DemuxCachedRange) -> bool {
        !self.range_seekable_timeline_ranges(range).is_empty()
    }

    fn range_seekable_timeline_ranges(&self, range: &DemuxCachedRange) -> Vec<(u64, u64)> {
        let mut ranges = Self::seekable_timeline_ranges_in_packet_range(
            &self.packets,
            self.timeline_anchor_stream_index,
            0,
            &range.stream_queues,
        );

        for (stream_index, kind) in &self.stream_kinds {
            if *stream_index == self.timeline_anchor_stream_index {
                continue;
            }
            if !matches!(kind, StreamCacheKind::Audio) {
                continue;
            }
            let Some(queue) = range.stream_queues.get(stream_index) else {
                return Vec::new();
            };
            let stream_ranges = Self::stream_timeline_ranges_in_packet_queue(&self.packets, queue);
            if stream_ranges.is_empty() {
                return Vec::new();
            }
            ranges = Self::intersect_timeline_ranges(&ranges, &stream_ranges);
            if ranges.is_empty() {
                return ranges;
            }
        }

        if let Some(pruned_until_nsecs) = range
            .sparse_stream_pruned_until_nsecs
            .values()
            .copied()
            .max()
        {
            ranges = ranges
                .into_iter()
                .filter_map(|(start, end)| {
                    let start = start.max(pruned_until_nsecs);
                    (end > start).then_some((start, end))
                })
                .collect();
        }

        ranges
    }

    fn stream_cache_states(&self) -> Vec<StreamCacheState> {
        let mut streams: BTreeMap<c_int, StreamCacheRangeState> = BTreeMap::new();
        let read_packet_positions = self
            .read_range()
            .global_order
            .iter()
            .copied()
            .enumerate()
            .map(|(index, packet_id)| (packet_id, index))
            .collect::<HashMap<_, _>>();
        self.collect_stream_cache_ranges(
            &self.read_range().stream_queues,
            &mut streams,
            |packet_id| {
                read_packet_positions.contains_key(&packet_id)
                    && self.active_packet_is_forward(packet_id)
            },
        );
        if let Some(range) = self.detached_append_range() {
            self.collect_stream_cache_ranges(&range.stream_queues, &mut streams, |_| true);
        }
        if !self.read_range_eof() {
            for stream_index in self.stream_kinds.keys() {
                streams
                    .entry(*stream_index)
                    .or_insert(StreamCacheRangeState {
                        reader_nsecs: Some(self.reader_nsecs),
                        cache_end_nsecs: Some(self.reader_nsecs),
                        has_forward_packet: false,
                    });
            }
        }
        streams
            .into_iter()
            .map(|(stream_index, state)| {
                let reader_pts = state.reader_nsecs.map(nsecs_to_seconds);
                let cache_end = state.cache_end_nsecs.map(nsecs_to_seconds);
                let kind = self
                    .stream_kinds
                    .get(&stream_index)
                    .copied()
                    .unwrap_or(StreamCacheKind::Unknown);
                let reader_nsecs = state.reader_nsecs.unwrap_or(self.reader_nsecs);
                let end_nsecs = state.cache_end_nsecs.unwrap_or(reader_nsecs);
                let window = StreamForwardWindow {
                    stream_index,
                    kind,
                    reader_nsecs,
                    end_nsecs,
                    has_forward_packet: state.has_forward_packet,
                };
                StreamCacheState {
                    kind,
                    cache_end,
                    reader_pts,
                    cache_duration: ordered_duration_seconds(
                        state.reader_nsecs,
                        state.cache_end_nsecs,
                    ),
                    underrun: self.stream_window_underrun(window),
                    idle: self.stream_window_idle(window),
                }
            })
            .collect()
    }

    fn collect_stream_cache_ranges(
        &self,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
        streams: &mut BTreeMap<c_int, StreamCacheRangeState>,
        mut include_packet: impl FnMut(PacketId) -> bool,
    ) {
        for (stream_index, queue) in stream_queues {
            for packet_id in queue {
                if !include_packet(*packet_id) {
                    continue;
                }
                let Some(packet) = self.packets.get(packet_id) else {
                    continue;
                };
                let entry = streams.entry(*stream_index).or_default();
                entry.has_forward_packet = true;
                if let Some(start_nsecs) = packet.start_nsecs {
                    entry.reader_nsecs =
                        Some(entry.reader_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
                }
                if let Some(end_nsecs) = packet.end_nsecs.or(packet.start_nsecs) {
                    entry.cache_end_nsecs =
                        Some(entry.cache_end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
                }
            }
        }
    }

    fn next_range_id(&mut self) -> RangeId {
        let range_id = self.next_range_id;
        self.next_range_id = self.next_range_id.saturating_add(1);
        range_id
    }

    fn log_would_block_diagnostic(&self, stream_indices: &[c_int]) {
        let per_stream: Vec<String> = stream_indices
            .iter()
            .copied()
            .map(|stream_index| {
                let reader_head = self.reader_heads.get(&stream_index).copied();
                let head_in_active_range = reader_head.is_some_and(|head| {
                    self.read_range()
                        .stream_queues
                        .get(&stream_index)
                        .is_some_and(|queue| queue.iter().any(|candidate| *candidate == head))
                });
                let active_queue_len = self
                    .read_range()
                    .stream_queues
                    .get(&stream_index)
                    .map(|queue| queue.len())
                    .unwrap_or(0);
                let detached_queue_len = self
                    .detached_append_range()
                    .and_then(|range| range.stream_queues.get(&stream_index))
                    .map(|queue| queue.len())
                    .unwrap_or(0);
                format!(
                    "stream={stream_index} head={reader_head:?} head_in_active={head_in_active_range} active_q={active_queue_len} detached_q={detached_queue_len}"
                )
            })
            .collect();
        tracing::debug!(
            session_id = ?self.session_id,
            read_range_id = self.read_range_id,
            append_range_id = self.append_range_id,
            append_range_detached = self.append_range_id != self.read_range_id,
            detached_append_range_id = ?self.detached_append_range_id(),
            demux_position_detached = self.demux_position_detached,
            forward_duration_ms = self.forward_duration_nsecs() as f64 / 1_000_000.0,
            reader_pts_seconds = nsecs_to_seconds(self.reader_nsecs),
            per_stream = ?per_stream,
            "FFmpeg demux pump WouldBlock with buffered packets: reader_head/range state"
        );
    }

    fn read_range(&self) -> &DemuxCachedRange {
        self.ranges
            .get(&self.read_range_id)
            .expect("FFmpeg demux packet cache read range missing")
    }

    fn read_range_mut(&mut self) -> &mut DemuxCachedRange {
        self.ranges
            .get_mut(&self.read_range_id)
            .expect("FFmpeg demux packet cache read range missing")
    }

    fn append_range(&self) -> &DemuxCachedRange {
        self.ranges
            .get(&self.append_range_id)
            .expect("FFmpeg demux packet cache append range missing")
    }

    fn append_range_mut(&mut self) -> &mut DemuxCachedRange {
        self.ranges
            .get_mut(&self.append_range_id)
            .expect("FFmpeg demux packet cache append range missing")
    }

    fn read_range_eof(&self) -> bool {
        self.read_range().is_eof
    }

    fn detached_append_range_id(&self) -> Option<RangeId> {
        (self.append_range_id != self.read_range_id).then_some(self.append_range_id)
    }

    fn detached_append_range(&self) -> Option<&DemuxCachedRange> {
        let range_id = self.detached_append_range_id()?;
        self.ranges.get(&range_id)
    }

    fn detached_append_range_mut(&mut self) -> Option<&mut DemuxCachedRange> {
        let range_id = self.detached_append_range_id()?;
        self.ranges.get_mut(&range_id)
    }

    fn start_new_current_range(&mut self, is_bof: bool) {
        let range_id = self.next_range_id();
        self.ranges.insert(
            range_id,
            DemuxCachedRange {
                id: range_id,
                global_order: VecDeque::new(),
                stream_queues: BTreeMap::new(),
                sparse_stream_pruned_until_nsecs: BTreeMap::new(),
                is_bof,
                is_eof: false,
                last_used_generation: self.generation,
            },
        );
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        self.read_index = 0;
        self.consumed_packet_ids.clear();
        self.reader_heads.clear();
    }

    fn start_detached_append_range(&mut self) {
        if self.detached_append_range().is_some() {
            return;
        }
        let range_id = self.next_range_id();
        self.append_range_id = range_id;
        self.ranges.insert(
            range_id,
            DemuxCachedRange {
                id: range_id,
                global_order: VecDeque::new(),
                stream_queues: BTreeMap::new(),
                sparse_stream_pruned_until_nsecs: BTreeMap::new(),
                is_bof: false,
                is_eof: false,
                last_used_generation: self.generation,
            },
        );
    }

    fn preserve_current_range(&mut self) {
        if self.read_range().global_order.is_empty() {
            self.ranges.remove(&self.read_range_id);
            self.read_index = 0;
            self.consumed_packet_ids.clear();
            self.reader_heads.clear();
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            if let Some(range) = self.ranges.remove(&self.read_range_id) {
                self.remove_range_packets(range);
            }
            self.read_index = 0;
            self.consumed_packet_ids.clear();
            self.reader_heads.clear();
            return;
        }
        let generation = self.generation;
        self.read_range_mut().last_used_generation = generation;
        self.read_index = 0;
        self.consumed_packet_ids.clear();
        self.reader_heads.clear();
    }

    fn preserve_detached_append_range(&mut self) {
        let Some(range_id) = self.detached_append_range_id() else {
            return;
        };
        let Some(mut range) = self.ranges.remove(&range_id) else {
            self.append_range_id = self.read_range_id;
            return;
        };
        self.append_range_id = self.read_range_id;
        if range.global_order.is_empty() {
            return;
        }
        if self.backbuffer_limit_bytes == 0 {
            self.remove_range_packets(range);
        } else {
            range.last_used_generation = self.generation;
            self.ranges.insert(range.id, range);
        }
    }

    fn activate_detached_append_range(&mut self) -> bool {
        let Some(range_id) = self.detached_append_range_id() else {
            return false;
        };
        let Some(range) = self.ranges.get(&range_id) else {
            self.append_range_id = self.read_range_id;
            return false;
        };
        if range.global_order.is_empty() && !range.is_eof {
            return false;
        }
        self.preserve_current_range();
        self.activate_range_for_read(range_id, 0);
        true
    }

    fn remove_range_packets(&mut self, range: DemuxCachedRange) {
        for packet_id in range.global_order {
            self.consumed_packet_ids.remove(&packet_id);
            if let Some(packet) = self.packets.remove(&packet_id) {
                self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            }
        }
    }

    #[cfg(test)]
    fn archived_bytes(&self) -> usize {
        let detached_append_range_id = self.detached_append_range_id();
        self.ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .map(|(_, range)| range)
            .flat_map(|range| range.global_order.iter())
            .filter_map(|packet_id| self.packets.get(packet_id))
            .map(|packet| packet.byte_len)
            .sum()
    }

    #[cfg(test)]
    fn set_read_index_for_test(&mut self, read_index: usize) {
        self.read_index = read_index;
        self.reset_reader_heads_for_read_index();
    }

    fn seek_cached_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        range: DemuxPacketRangeView<'_>,
        target_nsecs: u64,
    ) -> Option<DemuxCachedSeekHit> {
        let (first_cached_nsecs, buffered_until_nsecs) =
            Self::cached_timeline_range_in_packet_range(
                packets,
                timeline_anchor_stream_index,
                range.stream_queues,
            )?;
        if (target_nsecs < first_cached_nsecs && !range.is_bof)
            || (target_nsecs > buffered_until_nsecs && !range.is_eof)
        {
            return None;
        }
        let seek_target_nsecs = target_nsecs.clamp(first_cached_nsecs, buffered_until_nsecs);

        let mut covering_anchor_index = None;
        let mut keyframe_anchor_index = None;
        let mut preroll_keyframe_anchor_index = None;
        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            range.stream_queues,
        ) {
            let packet = packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            let packet_index = range
                .global_order
                .iter()
                .position(|candidate| *candidate == packet_id);
            if packet.recovery_point && start_nsecs <= seek_target_nsecs {
                if cached_seek_preroll_nsecs > 0 {
                    preroll_keyframe_anchor_index = keyframe_anchor_index;
                }
                keyframe_anchor_index = packet_index;
            }
            if covering_anchor_index.is_none()
                && start_nsecs <= seek_target_nsecs
                && seek_target_nsecs <= end_nsecs
            {
                covering_anchor_index = packet_index;
            }
        }

        let _covering_anchor_index = covering_anchor_index?;
        let read_index = if cached_seek_preroll_nsecs > 0 {
            let read_index = preroll_keyframe_anchor_index?;
            let required_preroll_start =
                seek_target_nsecs.saturating_sub(cached_seek_preroll_nsecs);
            let packet_id = *range.global_order.get(read_index)?;
            let read_start_nsecs = packets.get(&packet_id)?.start_nsecs?;
            if read_start_nsecs > required_preroll_start {
                return None;
            }
            read_index
        } else {
            keyframe_anchor_index?
        };
        let anchor_packet_id = *range.global_order.get(read_index)?;
        let anchor_seek_target_nsecs = packets
            .get(&anchor_packet_id)?
            .start_nsecs
            .unwrap_or(seek_target_nsecs);
        let mut reader_heads = BTreeMap::new();
        for (stream_index, queue) in range.stream_queues {
            let packet_id = if *stream_index == timeline_anchor_stream_index {
                Some(anchor_packet_id)
            } else {
                Self::find_stream_seek_target_in_packet_queue(
                    packets,
                    timeline_anchor_stream_index,
                    *stream_index,
                    queue,
                    anchor_seek_target_nsecs,
                )
            };
            if let Some(packet_id) = packet_id {
                reader_heads.insert(*stream_index, packet_id);
            }
        }
        Some(DemuxCachedSeekHit {
            reader_heads,
            buffered_until_nsecs,
        })
    }

    fn find_stream_seek_target_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        queue: &VecDeque<u64>,
        target_nsecs: u64,
    ) -> Option<PacketId> {
        let mut target = None;
        for packet_id in queue {
            let Some(packet) = packets.get(packet_id) else {
                continue;
            };
            if !Self::packet_is_stream_seek_boundary_for(
                timeline_anchor_stream_index,
                stream_index,
                packet,
            ) {
                continue;
            }
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            if target.is_some() && start_nsecs > target_nsecs {
                break;
            }
            target = Some(*packet_id);
        }
        target
    }

    fn timeline_anchor_packet_ids_in_packet_range<'a>(
        packets: &'a HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_queues: &'a BTreeMap<c_int, VecDeque<u64>>,
    ) -> impl Iterator<Item = u64> + 'a {
        stream_queues
            .get(&timeline_anchor_stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter().copied())
            .filter(|packet_id| {
                packets
                    .get(packet_id)
                    .is_some_and(|packet| packet.timeline_anchor && packet.start_nsecs.is_some())
            })
    }

    fn cached_timeline_range_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
    ) -> Option<(u64, u64)> {
        let mut first_cached_nsecs = None;
        let mut buffered_until_nsecs = None;
        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            stream_queues,
        ) {
            let packet = packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            first_cached_nsecs = Some(first_cached_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            buffered_until_nsecs = Some(buffered_until_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        first_cached_nsecs.zip(buffered_until_nsecs)
    }

    fn seekable_timeline_ranges_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
    ) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        let mut segment = SeekableTimelineSegment::default();

        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            stream_queues,
        ) {
            let Some(packet) = packets.get(&packet_id) else {
                continue;
            };
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);

            if segment
                .end_nsecs
                .is_some_and(|end_nsecs| start_nsecs > end_nsecs)
            {
                segment.finish_into(&mut ranges);
                segment = SeekableTimelineSegment::default();
            }

            segment.push_packet(
                start_nsecs,
                end_nsecs,
                packet.recovery_point,
                cached_seek_preroll_nsecs,
            );
        }

        segment.finish_into(&mut ranges);
        ranges
    }

    fn stream_timeline_ranges_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        queue: &VecDeque<u64>,
    ) -> Vec<(u64, u64)> {
        let mut include_all = |_| true;
        Self::stream_timeline_ranges_in_packet_queue_filtered(packets, queue, &mut include_all)
    }

    fn stream_timeline_ranges_in_packet_queue_filtered(
        packets: &HashMap<u64, CachedDemuxPacket>,
        queue: &VecDeque<u64>,
        include_packet: &mut impl FnMut(PacketId) -> bool,
    ) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        let mut current = None;
        for packet_id in queue {
            if !include_packet(*packet_id) {
                continue;
            }
            let Some(packet) = packets.get(packet_id) else {
                continue;
            };
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            if end_nsecs <= start_nsecs {
                continue;
            }
            match current {
                Some((current_start, current_end)) if start_nsecs <= current_end => {
                    current = Some((current_start, current_end.max(end_nsecs)));
                }
                Some(previous) => {
                    ranges.push(previous);
                    current = Some((start_nsecs, end_nsecs));
                }
                None => {
                    current = Some((start_nsecs, end_nsecs));
                }
            }
        }
        if let Some(range) = current {
            ranges.push(range);
        }
        ranges
    }

    fn intersect_timeline_ranges(left: &[(u64, u64)], right: &[(u64, u64)]) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        let mut left_index = 0;
        let mut right_index = 0;

        while left_index < left.len() && right_index < right.len() {
            let (left_start, left_end) = left[left_index];
            let (right_start, right_end) = right[right_index];
            let start = left_start.max(right_start);
            let end = left_end.min(right_end);
            if end > start {
                ranges.push((start, end));
            }
            if left_end <= right_end {
                left_index += 1;
            } else {
                right_index += 1;
            }
        }

        ranges
    }
}

fn ordered_duration_seconds(
    reader_nsecs: Option<u64>,
    cache_end_nsecs: Option<u64>,
) -> Option<f64> {
    let (reader_nsecs, cache_end_nsecs) = reader_nsecs.zip(cache_end_nsecs)?;
    (cache_end_nsecs >= reader_nsecs)
        .then(|| nsecs_to_seconds(cache_end_nsecs.saturating_sub(reader_nsecs)))
}

#[derive(Default)]
struct SeekableTimelineSegment {
    seek_start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
    previous_recovery_start_nsecs: Option<u64>,
}

impl SeekableTimelineSegment {
    fn push_packet(
        &mut self,
        start_nsecs: u64,
        end_nsecs: u64,
        recovery_point: bool,
        cached_seek_preroll_nsecs: u64,
    ) {
        self.end_nsecs = Some(self.end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        if !recovery_point {
            return;
        }

        if self.seek_start_nsecs.is_none() {
            self.seek_start_nsecs = if cached_seek_preroll_nsecs == 0 {
                Some(start_nsecs)
            } else {
                self.previous_recovery_start_nsecs.map(|previous_start| {
                    if previous_start == 0 {
                        start_nsecs
                    } else {
                        start_nsecs.max(previous_start.saturating_add(cached_seek_preroll_nsecs))
                    }
                })
            };
        }

        self.previous_recovery_start_nsecs = Some(start_nsecs);
    }

    fn finish_into(self, ranges: &mut Vec<(u64, u64)>) {
        let Some(start_nsecs) = self.seek_start_nsecs else {
            return;
        };
        let Some(end_nsecs) = self.end_nsecs else {
            return;
        };
        if end_nsecs > start_nsecs {
            ranges.push((start_nsecs, end_nsecs));
        }
    }
}

impl CachedDemuxPacket {
    fn from_packet(
        packet: &AvPacket,
        stream_index: c_int,
        timeline_anchor: bool,
        recovery_point: bool,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
    ) -> std::result::Result<Self, String> {
        Ok(Self {
            payload: CachedDemuxPacketPayload::Memory(AvPacket::ref_from(packet)?),
            stream_index,
            timeline_anchor,
            recovery_point,
            start_nsecs,
            end_nsecs,
            byte_len: packet.byte_len(),
        })
    }

    fn packet_ref(
        &self,
        disk_cache: Option<&DemuxPacketDiskCache>,
    ) -> std::result::Result<AvPacket, String> {
        match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => AvPacket::ref_from(packet),
            CachedDemuxPacketPayload::Disk { props, offset, len } => {
                let disk_cache = disk_cache
                    .ok_or_else(|| "FFmpeg demux packet disk cache unavailable".to_string())?;
                disk_cache.read_packet(*offset, *len, props)
            }
        }
    }

    fn spill_to_disk(
        &mut self,
        disk_cache: &mut DemuxPacketDiskCache,
    ) -> std::result::Result<(), String> {
        let CachedDemuxPacketPayload::Memory(packet) = &self.payload else {
            return Ok(());
        };
        let Some(data) = packet.data() else {
            return Ok(());
        };
        if data.is_empty() {
            return Ok(());
        }
        let data = data.to_vec();
        let props = AvPacket::props_from(packet)?;
        let offset = disk_cache.write_packet(&data)?;
        self.payload = CachedDemuxPacketPayload::Disk {
            props,
            offset,
            len: data.len(),
        };
        Ok(())
    }
}

impl DemuxPacketDiskCache {
    fn from_config(config: &PlaybackCacheConfig) -> Option<Self> {
        if !config.disk_cache && !demux_packet_disk_cache_enabled() {
            return None;
        }
        let max_bytes = env::var("TINY_DEMUX_PACKET_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(config.disk_cache_max_bytes);
        Self::new(max_bytes, config.cache_dir.clone(), config.unlink_files)
    }

    fn new(
        max_bytes: u64,
        configured_dir: Option<PathBuf>,
        unlink_files: CacheUnlinkPolicy,
    ) -> Option<Self> {
        let dir = configured_dir
            .or_else(|| {
                env::var("TINY_DEMUX_PACKET_CACHE_DIR")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(env::temp_dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, path = %dir.display(), "failed to create demux packet cache directory");
            return None;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "tiny-demux-packet-cache-{}-{stamp}.tmp",
            std::process::id()
        ));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to create demux packet cache file");
                return None;
            }
        };
        let mut unlink_on_drop = matches!(unlink_files, CacheUnlinkPolicy::WhenDone);
        if matches!(unlink_files, CacheUnlinkPolicy::Immediate) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to immediately unlink demux packet cache file");
                    unlink_on_drop = true;
                }
            }
        }
        Some(Self {
            file,
            path,
            next_offset: 0,
            max_bytes,
            unlink_on_drop,
        })
    }

    fn write_packet(&mut self, data: &[u8]) -> std::result::Result<u64, String> {
        let len = u64::try_from(data.len())
            .map_err(|_| "FFmpeg demux packet payload 过大".to_string())?;
        let offset = self.next_offset;
        let next = offset
            .checked_add(len)
            .ok_or_else(|| "FFmpeg demux packet disk cache offset overflow".to_string())?;
        if next > self.max_bytes {
            return Err("FFmpeg demux packet disk cache 已满".to_string());
        }
        let mut written = 0;
        while written < data.len() {
            let written_now = self
                .file
                .write_at(&data[written..], offset.saturating_add(written as u64))
                .map_err(|error| format!("写入 FFmpeg demux packet disk cache 失败：{error}"))?;
            if written_now == 0 {
                return Err("写入 FFmpeg demux packet disk cache 返回 0 字节".to_string());
            }
            written += written_now;
        }
        self.next_offset = next;
        Ok(offset)
    }

    fn read_packet(
        &self,
        offset: u64,
        len: usize,
        props: &AvPacket,
    ) -> std::result::Result<AvPacket, String> {
        let mut data = vec![0; len];
        let mut read = 0;
        while read < data.len() {
            let read_now = self
                .file
                .read_at(&mut data[read..], offset.saturating_add(read as u64))
                .map_err(|error| format!("读取 FFmpeg demux packet disk cache 失败：{error}"))?;
            if read_now == 0 {
                return Err("读取 FFmpeg demux packet disk cache 返回 0 字节".to_string());
            }
            read += read_now;
        }
        AvPacket::from_data_and_props(&data, props)
    }
}

impl Drop for DemuxPacketDiskCache {
    fn drop(&mut self) {
        if !self.unlink_on_drop {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::debug!(%error, path = %self.path.display(), "failed to remove demux packet cache file");
        }
    }
}

fn demux_packet_disk_cache_enabled() -> bool {
    env::var("TINY_DEMUX_PACKET_CACHE_ON_DISK")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

impl DemuxPacketTimeline {
    fn new(
        video_stream: StreamInfo,
        audio_stream: Option<StreamInfo>,
        subtitle_stream: Option<StreamInfo>,
        start_position_seconds: f64,
        session_id: PlaybackSessionId,
    ) -> Self {
        let video_frame_duration_nsecs = video_stream
            .frame_duration_nsecs
            .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
        let current_start_position_nsecs = seconds_to_nsecs(start_position_seconds.max(0.0));
        let buffered_reporter = BufferedReporter::new_with_events(false, false);
        Self {
            video_stream,
            audio_stream,
            subtitle_stream,
            video_frame_duration_nsecs,
            current_start_position_nsecs,
            video_clock: TimestampMapper::new(
                video_stream.start_nsecs,
                current_start_position_nsecs,
                Some(video_frame_duration_nsecs),
            ),
            audio_clock: TimestampMapper::new(
                audio_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
                None,
            ),
            subtitle_clock: TimestampMapper::new(
                subtitle_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
                None,
            ),
            buffered_reporter,
            session_id,
        }
    }

    fn reset(
        &mut self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        let position_seconds = position_seconds.max(0.0);
        self.session_id = session_id;
        self.current_start_position_nsecs = seconds_to_nsecs(position_seconds);
        self.video_clock = TimestampMapper::new(
            self.video_stream.start_nsecs,
            self.current_start_position_nsecs,
            Some(self.video_frame_duration_nsecs),
        );
        self.audio_clock = TimestampMapper::new(
            self.audio_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.subtitle_clock = TimestampMapper::new(
            self.subtitle_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.buffered_reporter = BufferedReporter::new_with_events(false, false);
        self.buffered_reporter
            .reset_to(position_seconds, session_id, event_tx);
    }

    fn cache_packet(
        &mut self,
        packet: &AvPacket,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<Option<CachedDemuxPacket>, String> {
        if !self.should_cache_stream(packet.stream_index()) {
            return Ok(None);
        }
        let (start_nsecs, end_nsecs) = self.packet_timeline_range(packet);
        if packet.stream_index() == self.video_stream.index
            && let Some(end_nsecs) = end_nsecs
        {
            self.buffered_reporter.report_video_timeline_nsecs(
                end_nsecs,
                self.session_id,
                event_tx,
            );
        }
        CachedDemuxPacket::from_packet(
            packet,
            packet.stream_index(),
            packet.stream_index() == self.video_stream.index,
            packet.stream_index() == self.video_stream.index
                && packet_is_video_seek_point(packet, self.video_stream.codec_id),
            start_nsecs,
            end_nsecs,
        )
        .map(Some)
    }

    fn should_cache_stream(&self, stream_index: c_int) -> bool {
        stream_index == self.video_stream.index
            || self
                .audio_stream
                .is_some_and(|stream| stream.index == stream_index)
            || self
                .subtitle_stream
                .is_some_and(|stream| stream.index == stream_index)
    }

    fn packet_timeline_range(&mut self, packet: &AvPacket) -> (Option<u64>, Option<u64>) {
        if packet.stream_index() == self.video_stream.index {
            let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self.video_clock.map(timestamp, self.video_stream.time_base);
            let duration_nsecs = packet_duration_nsecs(packet, self.video_stream)
                .unwrap_or(self.video_frame_duration_nsecs);
            let end_nsecs = mapped.timeline_nsecs.saturating_add(duration_nsecs);
            return (Some(mapped.timeline_nsecs), Some(end_nsecs));
        }
        let Some(audio_stream) = self
            .audio_stream
            .filter(|stream| packet.stream_index() == stream.index)
        else {
            let Some(subtitle_stream) = self
                .subtitle_stream
                .filter(|stream| packet.stream_index() == stream.index)
            else {
                return (None, None);
            };
            let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self
                .subtitle_clock
                .map(timestamp, subtitle_stream.time_base);
            let end_nsecs = packet_duration_nsecs(packet, subtitle_stream)
                .map(|duration| mapped.timeline_nsecs.saturating_add(duration))
                .or(Some(mapped.timeline_nsecs));
            return (Some(mapped.timeline_nsecs), end_nsecs);
        };
        let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
        let mapped = self.audio_clock.map(timestamp, audio_stream.time_base);
        let end_nsecs = packet_duration_nsecs(packet, audio_stream)
            .map(|duration| mapped.timeline_nsecs.saturating_add(duration));
        (Some(mapped.timeline_nsecs), end_nsecs)
    }

    fn buffered_until(&self) -> Option<f64> {
        self.buffered_reporter.buffered_until()
    }

    fn set_session_id(&mut self, session_id: PlaybackSessionId) {
        self.session_id = session_id;
    }
}

fn run_demux_packet_cache(
    thread_input: DemuxPacketCacheThreadInput,
    shared: Arc<DemuxPacketCacheShared>,
) {
    let DemuxPacketCacheThreadInput {
        mut input,
        video_stream,
        audio_stream,
        subtitle_stream,
        duration_seconds,
        start_position_seconds,
        session_id,
    } = thread_input;
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        audio_stream,
        subtitle_stream,
        start_position_seconds,
        session_id,
    );
    timeline.reset(start_position_seconds, session_id, &shared.event_tx);
    let mut packet = match AvPacket::new() {
        Ok(packet) => packet,
        Err(error) => {
            shared.set_error(error);
            return;
        }
    };

    loop {
        if shared.should_stop() {
            return;
        }
        let request = shared.wait_for_demux_permit();
        if shared.should_stop() {
            return;
        }
        if let Some(request) = request {
            if shared.should_skip_seek_request(&request) {
                tracing::debug!(
                    ?request.session_id,
                    position_seconds = request.position_seconds,
                    request_seek_generation = request.seek_generation,
                    current_seek_generation = shared.control.seek_generation(),
                    "skipping stale FFmpeg demux low-level seek request"
                );
                continue;
            }
            let generation = shared.generation();
            let seek_generation = request.seek_generation;
            tracing::debug!(
                ?request.session_id,
                position_seconds = request.position_seconds,
                seek_position_seconds = preroll_seek_position_seconds(
                    video_stream.codec_id,
                    request.position_seconds
                ),
                preroll_nsecs = video_seek_preroll_nsecs(video_stream.codec_id),
                generation,
                seek_generation,
                "FFmpeg demux thread applying low-level seek"
            );
            if let Err(error) = input.seek_stream(
                video_stream,
                preroll_seek_position_seconds(video_stream.codec_id, request.position_seconds),
            ) {
                if shared.should_discard_demux_result(generation, seek_generation) {
                    tracing::debug!(
                        ?request.session_id,
                        position_seconds = request.position_seconds,
                        generation,
                        current_generation = shared.generation(),
                        seek_generation,
                        current_seek_generation = shared.control.seek_generation(),
                        %error,
                        "discarding FFmpeg demux seek error after newer seek"
                    );
                    continue;
                }
                shared.set_error(error);
                continue;
            }
            if shared.should_discard_demux_result(generation, seek_generation) {
                tracing::debug!(
                    ?request.session_id,
                    position_seconds = request.position_seconds,
                    generation,
                    current_generation = shared.generation(),
                    seek_generation,
                    current_seek_generation = shared.control.seek_generation(),
                    "discarding FFmpeg demux seek result after newer seek"
                );
                continue;
            }
            tracing::debug!(
                ?request.session_id,
                position_seconds = request.position_seconds,
                seek_position_seconds = preroll_seek_position_seconds(
                    video_stream.codec_id,
                    request.position_seconds
                ),
                generation,
                seek_generation,
                "FFmpeg demux thread low-level seek applied"
            );
            timeline.reset(
                request.position_seconds,
                request.session_id,
                &shared.event_tx,
            );
        }

        let generation = shared.generation();
        let seek_generation = shared.control.seek_generation();
        shared.mark_demux_read_started();
        let read_started_at = Instant::now();
        let read = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        let read_elapsed = read_started_at.elapsed();
        shared.mark_demux_read_finished();
        if read_elapsed >= DEMUX_READ_SLOW_LOG_AFTER {
            shared.log_slow_demux_read(read_elapsed, read);
        }
        if shared.should_discard_demux_result(generation, seek_generation) {
            tracing::debug!(
                generation,
                current_generation = shared.generation(),
                seek_generation,
                current_seek_generation = shared.control.seek_generation(),
                read_result = read,
                "discarding FFmpeg demux read result after newer seek"
            );
            packet.unref();
            continue;
        }
        timeline.set_session_id(shared.session_id());
        if read >= 0 {
            match timeline.cache_packet(&packet, &shared.event_tx) {
                Ok(Some(cached)) => shared.append_packet(cached),
                Ok(None) => {}
                Err(error) => shared.set_error(error),
            }
            packet.unref();
            // Yield after each appended packet so the coordinator pump — which feeds
            // the decoder under the same cache mutex — gets fair access. Without this,
            // a producer draining an already-buffered byte cache can starve the pump on
            // the non-fair mutex and throttle decode below realtime.
            thread::yield_now();
            continue;
        }
        packet.unref();

        tracing::debug!(
            read_result = read,
            error = %ffmpeg_error(read),
            generation,
            seek_generation,
            pending_seek = shared.control.has_pending_seek(),
            buffered_until = ?timeline.buffered_until(),
            "FFmpeg demux av_read_frame returned error"
        );
        if shared.control.has_pending_seek() {
            thread::yield_now();
            continue;
        }
        if read == ffi::AVERROR_EOF
            || (read == ffi::AVERROR(ffi::EIO)
                && playback_buffered_near_duration(duration_seconds, timeline.buffered_until()))
        {
            timeline.buffered_reporter.report_value(
                duration_seconds,
                timeline.session_id,
                &shared.event_tx,
            );
            shared.mark_eof();
            continue;
        }
        if read == ffi::AVERROR(ffi::EAGAIN) {
            thread::sleep(DEMUX_PACKET_CACHE_WAIT_INTERVAL);
            continue;
        }
        shared.set_error(format!("FFmpeg 读取媒体包失败：{}", ffmpeg_error(read)));
    }
}

impl DemuxPacketCacheShared {
    fn should_log_would_block_diagnostic(&self) -> bool {
        let now = duration_nsecs(self.clock_start.elapsed());
        let last = self.last_would_block_diag_nanos.load(Ordering::Relaxed);
        if now.saturating_sub(last) < duration_nsecs(DEMUX_WOULD_BLOCK_DIAG_INTERVAL) {
            return false;
        }
        self.last_would_block_diag_nanos
            .store(now, Ordering::Relaxed);
        true
    }

    fn mark_demux_read_started(&self) {
        let nanos = duration_nsecs(self.clock_start.elapsed()).max(1);
        self.demux_read_started_nanos
            .store(nanos, Ordering::Release);
    }

    fn mark_demux_read_finished(&self) {
        self.demux_read_started_nanos.store(0, Ordering::Release);
    }

    fn demux_read_blocked_for(&self) -> Option<Duration> {
        let started = self.demux_read_started_nanos.load(Ordering::Acquire);
        (started != 0).then(|| {
            Duration::from_nanos(duration_nsecs(self.clock_start.elapsed()).saturating_sub(started))
        })
    }

    fn log_slow_demux_read(&self, elapsed: Duration, read_result: c_int) {
        let guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        tracing::warn!(
            session_id = ?guard.session_id,
            read_ms = elapsed.as_secs_f64() * 1000.0,
            read_result,
            read_index = guard.read_index,
            reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
            forward_bytes = guard.forward_bytes(),
            forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
            cached_until_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
            demux_position_detached = guard.demux_position_detached,
            raw_input_rate_bps = ?guard.raw_input_rate(),
            cache_paused = self.control.is_cache_paused(),
            "FFmpeg demux av_read_frame 慢读/疑似卡住"
        );
    }

    fn send_cache_state_events(
        &self,
        session_id: PlaybackSessionId,
        cache_state: PlaybackCacheState,
        buffered_changed: Option<Option<f64>>,
    ) {
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::CacheStateChanged(cache_state),
        ));
        if let Some(buffered_until) = buffered_changed {
            let _ = self.event_tx.send(BackendEvent::new(
                session_id,
                BackendEventKind::BufferedChanged(buffered_until),
            ));
        }
    }

    fn emit_cache_state(&self, guard: &mut DemuxPacketCacheState) {
        let cache_state = guard.playback_cache_state(self.control.is_cache_paused());
        let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
        guard.record_cache_state_emit(Instant::now());
        self.send_cache_state_events(guard.session_id, cache_state, buffered_changed);
    }

    fn emit_cache_state_after_append(
        &self,
        guard: &mut DemuxPacketCacheState,
        outcome: DemuxPacketAppendOutcome,
    ) {
        let now = Instant::now();
        if !outcome.force_cache_state_report && !guard.cache_state_report_due(now) {
            return;
        }
        let cache_state = guard.playback_cache_state(self.control.is_cache_paused());
        let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
        guard.record_cache_state_emit(now);
        self.send_cache_state_events(guard.session_id, cache_state, buffered_changed);
    }

    fn emit_cache_state_after_read(&self, guard: &mut DemuxPacketCacheState, force: bool) {
        let now = Instant::now();
        if !force && !guard.cache_state_report_due(now) {
            return;
        }
        let cache_state = guard.playback_cache_state(self.control.is_cache_paused());
        let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
        guard.record_cache_state_emit(now);
        self.send_cache_state_events(guard.session_id, cache_state, buffered_changed);
    }

    fn enter_initial_cache_pause_if_needed(&self) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        if guard.cache_pause_initial && guard.cache_pause_can_enter(false) {
            self.enter_cache_pause(&mut guard);
        }
        self.emit_cache_state(&mut guard);
    }

    fn enter_cache_pause_if_needed(
        &self,
        guard: &mut DemuxPacketCacheState,
        cache_pause_signal: bool,
    ) {
        if !cache_pause_signal || !guard.cache_pause_can_enter(true) {
            return;
        }
        if self.enter_cache_pause(guard) {
            self.emit_cache_state(guard);
        }
    }

    fn enter_cache_pause(&self, guard: &mut DemuxPacketCacheState) -> bool {
        let changed = self.control.set_cache_paused(true);
        let percent = guard.cache_pause_percent();
        if changed {
            tracing::debug!(
                session_id = ?guard.session_id,
                buffering_percent = ?percent,
                read_index = guard.read_index,
                packet_count = guard.read_range().global_order.len(),
                cached_bytes = guard.cached_bytes,
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                reader_nsecs = guard.reader_nsecs,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                cached_until_nsecs = ?guard.cached_until_nsecs(),
                cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                should_pause_demux = guard.should_pause_demux(),
                readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                "FFmpeg demux packet cache pause entered"
            );
        }
        if changed {
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::PausedForCacheChanged(true),
            ));
        }
        let percent_changed = guard.cache_buffering_percent != percent;
        if guard.cache_buffering_percent != percent {
            guard.cache_buffering_percent = percent;
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::CacheBufferingChanged(percent),
            ));
        }
        if changed {
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::Pause(self.control.is_paused()),
            ));
        }
        changed || percent_changed
    }

    fn refresh_cache_pause(&self, guard: &mut DemuxPacketCacheState) -> bool {
        if !self.control.is_cache_paused() {
            if guard.cache_buffering_percent.is_some() {
                guard.cache_buffering_percent = None;
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
                return true;
            }
            return false;
        }

        if guard.cache_pause_recovered() {
            let had_percent = guard.cache_buffering_percent.take().is_some();
            let changed = self.control.set_cache_paused(false);
            if changed {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                    "FFmpeg demux packet cache pause recovered"
                );
            }
            if had_percent {
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
            }
            if changed {
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::PausedForCacheChanged(false),
                ));
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::Pause(self.control.is_paused()),
                ));
            }
            return had_percent || changed;
        }

        let percent = guard.cache_pause_percent();
        if guard.cache_buffering_percent != percent {
            guard.cache_buffering_percent = percent;
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::CacheBufferingChanged(percent),
            ));
            return true;
        }
        false
    }

    fn should_skip_seek_request(&self, request: &DemuxSeekRequest) -> bool {
        request.seek_generation < self.control.seek_generation()
    }

    fn should_discard_demux_result(&self, generation: u64, seek_generation: u64) -> bool {
        self.generation() != generation || self.control.seek_generation() != seek_generation
    }

    fn should_stop(&self) -> bool {
        self.control.should_stop()
            || self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned")
                .shutdown
    }

    fn wait_for_demux_permit(&self) -> Option<DemuxSeekRequest> {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let mut logged_prefetch_pause = false;
        let mut prefetch_pause_started_at = None;
        let mut next_prefetch_pause_log_at = None;
        loop {
            if guard.shutdown || self.control.should_stop() {
                return None;
            }
            if let Some(request) = guard.take_seek_request() {
                return Some(request);
            }
            if guard.read_range_eof() || guard.error.is_some() {
                let (next_guard, _) = self
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                guard = next_guard;
                continue;
            }
            let should_pause_demux = guard.should_pause_demux();
            if !should_pause_demux {
                return None;
            }
            let now = Instant::now();
            let pause_started = *prefetch_pause_started_at.get_or_insert(now);
            if !logged_prefetch_pause {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.control.is_cache_paused(),
                    should_pause_demux,
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    generation = guard.generation,
                    seek_generation = self.control.seek_generation(),
                    "FFmpeg demux packet cache prefetch paused"
                );
                logged_prefetch_pause = true;
                next_prefetch_pause_log_at =
                    now.checked_add(DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER);
            } else if next_prefetch_pause_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    paused_ms = now.saturating_duration_since(pause_started).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.control.is_cache_paused(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    generation = guard.generation,
                    seek_generation = self.control.seek_generation(),
                    "FFmpeg demux packet cache prefetch still paused"
                );
                next_prefetch_pause_log_at =
                    now.checked_add(DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL);
            }
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            guard = next_guard;
        }
    }

    fn generation(&self) -> u64 {
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .generation
    }

    fn session_id(&self) -> PlaybackSessionId {
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .session_id
    }

    fn append_packet(&self, packet: CachedDemuxPacket) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let mut append_outcome = guard.append_packet(packet);
        let cache_pause_changed = self.refresh_cache_pause(&mut guard);
        if append_outcome.appended {
            append_outcome.force_cache_state_report |= cache_pause_changed;
            self.emit_cache_state_after_append(&mut guard, append_outcome);
        } else if cache_pause_changed {
            self.emit_cache_state(&mut guard);
        }
        self.ready.notify_all();
    }

    fn mark_eof(&self) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.mark_eof();
        self.refresh_cache_pause(&mut guard);
        self.emit_cache_state(&mut guard);
        self.ready.notify_all();
    }

    fn set_error(&self, error: String) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.error = Some(error);
        guard.seeking = false;
        guard.cache_buffering_percent = None;
        if self.control.set_cache_paused(false) {
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::CacheBufferingChanged(None),
            ));
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::PausedForCacheChanged(false),
            ));
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::Pause(self.control.is_paused()),
            ));
        }
        self.emit_cache_state(&mut guard);
        self.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_anchor(start_nsecs: u64, end_nsecs: u64) -> CachedDemuxPacket {
        cached_key_packet(0, true, Some(start_nsecs), Some(end_nsecs))
    }

    fn cached_packet(
        stream_index: c_int,
        timeline_anchor: bool,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
    ) -> CachedDemuxPacket {
        cached_packet_with_keyframe(stream_index, timeline_anchor, false, start_nsecs, end_nsecs)
    }

    fn cached_key_packet(
        stream_index: c_int,
        timeline_anchor: bool,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
    ) -> CachedDemuxPacket {
        cached_packet_with_keyframe(stream_index, timeline_anchor, true, start_nsecs, end_nsecs)
    }

    fn cached_packet_with_keyframe(
        stream_index: c_int,
        timeline_anchor: bool,
        keyframe: bool,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
    ) -> CachedDemuxPacket {
        let mut packet = AvPacket::new().expect("packet allocates");
        unsafe {
            (*packet.as_mut_ptr()).stream_index = stream_index;
        }
        CachedDemuxPacket {
            payload: CachedDemuxPacketPayload::Memory(packet),
            stream_index,
            timeline_anchor,
            recovery_point: keyframe,
            start_nsecs,
            end_nsecs,
            byte_len: 1024,
        }
    }

    fn stream_info_for_test(index: c_int, codec_id: ffi::AVCodecID) -> StreamInfo {
        StreamInfo {
            index,
            stream: ptr::null_mut(),
            decoder: ptr::null(),
            codec_id,
            time_base: ffi::AVRational { num: 1, den: 1_000 },
            start_nsecs: None,
            frame_duration_nsecs: Some(DEFAULT_VIDEO_FRAME_DURATION_NSECS),
        }
    }

    fn demux_packet_for_stream(stream_index: c_int) -> AvPacket {
        let mut packet = AvPacket::new().expect("packet allocates");
        unsafe {
            (*packet.as_mut_ptr()).stream_index = stream_index;
        }
        packet
    }

    fn shared_for_test(control: Arc<FfmpegControl>) -> DemuxPacketCacheShared {
        let (shared, _) = shared_with_config_for_test(control, PlaybackCacheConfig::default());
        shared
    }

    fn shared_with_config_for_test(
        control: Arc<FfmpegControl>,
        cache_config: PlaybackCacheConfig,
    ) -> (DemuxPacketCacheShared, Receiver<BackendEvent>) {
        let (event_tx, event_rx) = mpsc::channel();
        let shared = DemuxPacketCacheShared {
            state: Mutex::new(DemuxPacketCacheState::new(
                0,
                0,
                ffi::AVCodecID::AV_CODEC_ID_MPEG4,
                PlaybackSessionId(1),
                cache_config,
            )),
            ready: Condvar::new(),
            control,
            event_tx,
            clock_start: Instant::now(),
            demux_read_started_nanos: AtomicU64::new(0),
            last_would_block_diag_nanos: AtomicU64::new(0),
        };
        (shared, event_rx)
    }

    fn cache_config_for_test() -> PlaybackCacheConfig {
        PlaybackCacheConfig::default()
    }

    #[test]
    fn demux_packet_timeline_drops_unselected_stream_packets() {
        let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
        let audio_stream = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_AAC);
        let mut timeline = DemuxPacketTimeline::new(
            video_stream,
            Some(audio_stream),
            None,
            0.0,
            PlaybackSessionId(1),
        );
        let (event_tx, _event_rx) = mpsc::channel();

        let packet = demux_packet_for_stream(1);
        let cached = timeline
            .cache_packet(&packet, &event_tx)
            .expect("unselected packet is accepted as droppable");

        assert!(cached.is_none());
        assert!(!timeline.should_cache_stream(1));
        assert!(timeline.should_cache_stream(0));
        assert!(timeline.should_cache_stream(2));
    }

    #[test]
    fn demux_packet_cache_state_uses_local_auto_as_cache_inactive() {
        let config = PlaybackCacheConfig {
            cache_secs: 30.0,
            demuxer_readahead_secs: 2.0,
            cache_pause: true,
            ..PlaybackCacheConfig::default()
        }
        .resolved_for_cacheable_input(false);

        let state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert_eq!(state.readahead_nsecs, 2_000_000_000);
        assert_eq!(state.backbuffer_limit_bytes, 0);
        assert!(!state.cache_pause_enabled);
    }

    #[test]
    fn demux_packet_cache_state_keeps_forced_seekable_cache_when_local_auto_is_inactive() {
        let config = PlaybackCacheConfig {
            seekable_cache: PlaybackSeekableCacheMode::Enabled,
            demuxer_max_back_bytes: 2048,
            ..PlaybackCacheConfig::default()
        }
        .resolved_for_cacheable_input(false);

        let state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert_eq!(state.backbuffer_limit_bytes, 2048);
    }

    #[test]
    fn demux_packet_cache_state_allows_zero_cache_secs_to_use_demux_readahead() {
        let config = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Enabled,
            cache_secs: 0.0,
            demuxer_readahead_secs: 2.0,
            ..PlaybackCacheConfig::default()
        };

        let state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert_eq!(state.readahead_nsecs, 2_000_000_000);
    }

    #[test]
    fn demux_packet_cache_state_caps_readahead_below_cache_secs_when_cache_is_active() {
        let config = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Enabled,
            cache_secs: 30.0,
            demuxer_readahead_secs: 2.0,
            ..PlaybackCacheConfig::default()
        };

        let state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        // The packet read-ahead is capped (deep buffering lives in the byte cache) so
        // the producer pauses and releases the cache mutex instead of hot-looping
        // toward cache_secs and starving the pump.
        assert_eq!(
            state.readahead_nsecs,
            duration_nsecs(DEMUX_PACKET_CACHE_MAX_READAHEAD)
        );
    }

    #[test]
    fn demux_packet_cache_state_allows_zero_demuxer_max_bytes() {
        let config = PlaybackCacheConfig {
            demuxer_max_bytes: 0,
            ..PlaybackCacheConfig::default()
        };

        let state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert_eq!(state.memory_limit_bytes, 0);
        assert!(!state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_applies_live_cache_config() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.set_read_index_for_test(1);

        let config = PlaybackCacheConfig {
            mode: PlaybackCacheMode::Disabled,
            cache_secs: 3.0,
            demuxer_readahead_secs: 2.0,
            demuxer_hysteresis_secs: 0.5,
            demuxer_max_bytes: 1024,
            demuxer_max_back_bytes: 2048,
            demuxer_donate_buffer: false,
            cache_pause: false,
            ..PlaybackCacheConfig::default()
        };
        state.cache_buffering_percent = Some(25);
        state.apply_cache_config(config);

        assert_eq!(state.memory_limit_bytes, 1024);
        assert_eq!(state.backbuffer_limit_bytes, 0);
        assert_eq!(state.readahead_nsecs, 2_000_000_000);
        assert_eq!(state.hysteresis_nsecs, 500_000_000);
        assert!(!state.donate_backbuffer);
        assert!(!state.cache_pause_enabled);
        assert_eq!(state.cache_buffering_percent, None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.cached_bytes, 1024);
    }

    #[test]
    fn demux_packet_cache_state_trims_consumed_packet_at_memory_limit() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        let mut packet = cached_packet(0, true, Some(0), Some(1_000_000_000));
        packet.byte_len = DEMUX_PACKET_CACHE_MEMORY_BYTES;
        state.append_packet(packet);

        assert_eq!(state.cached_bytes, DEMUX_PACKET_CACHE_MEMORY_BYTES);
        assert!(state.should_pause_demux());

        state.set_read_index_for_test(1);
        state.reader_nsecs = 1_000_000_000;
        state.trim_to_limit();

        assert_eq!(state.cached_bytes, 0);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.read_range().global_order.len(), 0);
        assert!(!state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_does_not_pause_for_hysteresis_before_readahead_target() {
        let mut config = cache_config_for_test();
        config.cache_secs = 3.0;
        config.demuxer_readahead_secs = 3.0;
        config.demuxer_hysteresis_secs = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));

        assert!(!state.hysteresis_active);
        assert!(!state.should_pause_demux());
        assert!(!state.playback_cache_state(false).demux.idle);
    }

    #[test]
    fn demux_packet_cache_state_pauses_prefetch_until_hysteresis_threshold() {
        let mut config = cache_config_for_test();
        config.cache_secs = 3.0;
        config.demuxer_readahead_secs = 3.0;
        config.demuxer_hysteresis_secs = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

        assert!(state.hysteresis_active);
        assert!(state.should_pause_demux());
        assert!(state.playback_cache_state(false).demux.idle);

        state.set_read_index_for_test(1);
        state.reader_nsecs = 1_000_000_000;
        state.refresh_readahead_hysteresis();

        assert!(!state.hysteresis_active);
        assert!(!state.should_pause_demux());

        state.set_read_index_for_test(2);
        state.reader_nsecs = 2_000_000_000;
        state.refresh_readahead_hysteresis();

        assert!(!state.hysteresis_active);
        assert!(!state.should_pause_demux());
        assert!(!state.playback_cache_state(false).demux.idle);
    }

    #[test]
    fn demux_packet_cache_state_initial_cache_wait_completes_at_prefetch_limit_or_eof() {
        let mut config = cache_config_for_test();
        config.cache_secs = 2.0;
        config.demuxer_readahead_secs = 2.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert!(!state.initial_cache_fill_complete());

        state.append_packet(cached_anchor(0, 1_000_000_000));
        assert!(!state.initial_cache_fill_complete());

        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        assert!(state.initial_cache_fill_complete());

        let mut eof_state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        eof_state.mark_eof();
        assert!(eof_state.initial_cache_fill_complete());
    }

    #[test]
    fn demux_packet_cache_state_initial_cache_wait_uses_cache_pause_target() {
        let mut config = cache_config_for_test();
        config.mode = PlaybackCacheMode::Enabled;
        config.cache_pause = true;
        config.cache_pause_initial = true;
        config.cache_pause_wait = 1.0;
        config.demuxer_readahead_secs = 5.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        assert!(!state.initial_cache_fill_complete());

        state.append_packet(cached_anchor(0, 1_000_000_000));

        assert!(state.initial_cache_fill_complete());
        assert!(!state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_uses_shortest_selected_forward_stream_duration() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 5_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(2_000_000_000)));

        let cache_state = state.playback_cache_state(false);
        assert_eq!(cache_state.demux.cache_end, Some(2.0));
        assert_eq!(cache_state.demux.cache_duration, Some(2.0));
        assert!(!cache_state.demux.underrun);
        assert_eq!(state.forward_duration_nsecs(), 2_000_000_000);
    }

    #[test]
    fn demux_packet_cache_state_reports_recent_raw_input_rate() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        let mut first = cached_anchor(0, 1_000_000_000);
        first.byte_len = 1500;
        let mut second = cached_anchor(1_000_000_000, 2_000_000_000);
        second.byte_len = 2500;

        state.append_packet(first);
        state.append_packet(second);

        assert_eq!(
            state.playback_cache_state(false).demux.raw_input_rate,
            Some(4000)
        );
    }

    #[test]
    fn demux_packet_cache_state_reports_last_demux_timestamp() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );

        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

        assert_eq!(state.playback_cache_state(false).demux.ts_last, Some(2.0));
    }

    #[test]
    fn demux_packet_cache_state_clears_last_demux_timestamp_on_low_level_seek() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        assert_eq!(state.playback_cache_state(false).demux.ts_last, None);
    }

    #[test]
    fn demux_packet_cache_state_counts_skipped_overlap_in_raw_input_rate() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.resume_append_skip_until_nsecs = Some(2_000_000_000);
        let mut packet = cached_anchor(0, 1_000_000_000);
        packet.byte_len = 4096;

        let outcome = state.append_packet(packet);

        assert!(!outcome.appended);
        assert_eq!(state.cached_bytes, 0);
        assert_eq!(
            state.playback_cache_state(false).demux.raw_input_rate,
            Some(4096)
        );
        assert_eq!(state.playback_cache_state(false).demux.ts_last, Some(0.0));
    }

    #[test]
    fn demux_packet_cache_state_keeps_prefetching_when_selected_audio_has_no_forward_packet() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1.0;
        config.demuxer_readahead_secs = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 5_000_000_000));

        let cache_state = state.playback_cache_state(false);
        assert_eq!(cache_state.demux.cache_duration, Some(0.0));
        assert!(cache_state.demux.underrun);
        assert!(!state.should_pause_demux());
        assert!(!cache_state.demux.idle);
    }

    #[test]
    fn demux_packet_cache_state_reads_needed_eager_stream_despite_byte_limit() {
        let mut config = cache_config_for_test();
        config.demuxer_max_bytes = 1024;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));

        let cache_state = state.playback_cache_state(false);

        assert_eq!(state.memory_limit_bytes, 1024);
        assert_eq!(state.forward_bytes(), 1024);
        assert!(cache_state.demux.underrun);
        assert!(!state.should_pause_demux());
        assert!(!cache_state.demux.idle);
    }

    #[test]
    fn demux_packet_cache_state_omits_invalid_cache_duration_when_end_precedes_reader() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.set_read_index_for_test(1);
        state.reader_nsecs = 2_000_000_000;
        state.mark_eof();

        let cache_state = state.playback_cache_state(false);

        assert_eq!(cache_state.demux.reader_pts, Some(2.0));
        assert_eq!(cache_state.demux.cache_end, Some(1.0));
        assert_eq!(cache_state.demux.cache_duration, None);
    }

    #[test]
    fn demux_packet_cache_state_ignores_empty_subtitle_duration_when_video_has_forward_cache() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1.0;
        config.demuxer_readahead_secs = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(2, StreamCacheKind::Subtitle);
        state.append_packet(cached_anchor(0, 2_000_000_000));

        let cache_state = state.playback_cache_state(false);
        assert_eq!(cache_state.demux.cache_duration, Some(2.0));
        assert!(!cache_state.demux.underrun);
        assert!(state.should_pause_demux());
        assert!(cache_state.demux.idle);
    }

    #[test]
    fn demux_packet_cache_buffered_changed_is_derived_from_cache_state_end() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(0, 1_000_000_000));
        let events = event_rx.try_iter().collect::<Vec<_>>();
        let cache_end = events.iter().find_map(|event| match &event.kind {
            BackendEventKind::CacheStateChanged(state) => state.demux.cache_end,
            _ => None,
        });
        let buffered_until = events.iter().find_map(|event| match &event.kind {
            BackendEventKind::BufferedChanged(buffered_until) => *buffered_until,
            _ => None,
        });

        assert_eq!(cache_end, Some(1.0));
        assert_eq!(buffered_until, cache_end);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.last_cache_state_emit_at =
                Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
        }
        shared.append_packet(cached_anchor(1_000_000_000, 1_020_000_000));
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::BufferedChanged(_)))
        );
    }

    #[test]
    fn demux_packet_cache_throttles_continuous_append_cache_state_events() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.demuxer_readahead_secs = 10.0;
        let (shared, event_rx) = shared_with_config_for_test(control, config);
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(0, 1_000_000_000));
        assert!(
            event_rx
                .try_iter()
                .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
        );

        shared.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
        );

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.last_cache_state_emit_at =
                Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
        }
        shared.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        assert!(
            event_rx
                .try_iter()
                .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
        );
    }

    #[test]
    fn demux_packet_cache_reports_reader_state_after_packet_read() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        shared.append_packet(cached_anchor(0, 1_000_000_000));
        shared.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.last_cache_state_emit_at =
                Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
        }

        let shared = Arc::new(shared);
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };

        assert!(matches!(
            cache.read_packet_round_robin(&[0]).0,
            DemuxReadResult::Packet(_)
        ));
        let events = event_rx.try_iter().collect::<Vec<_>>();
        let cache_state = events.iter().find_map(|event| match &event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state),
            _ => None,
        });

        let cache_state = cache_state.expect("read emits cache state when report is due");
        assert_eq!(cache_state.demux.reader_pts, Some(1.0));
        assert_eq!(cache_state.demux.cache_end, Some(2.0));
        assert_eq!(cache_state.demux.cache_duration, Some(1.0));
        assert_eq!(cache_state.demux.forward_bytes, 1024);
    }

    #[test]
    fn demux_packet_cache_polls_per_stream_queues_independently() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, _) = shared_with_config_for_test(control, config);
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.set_stream_kind(1, StreamCacheKind::Audio);
            guard.append_packet(cached_anchor(0, 1_000_000_000));
            guard.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
            guard.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        }
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };

        let snapshot = cache.packet_queue_snapshot();
        assert_eq!(snapshot.total_packets, 3);
        assert_eq!(
            snapshot
                .streams
                .iter()
                .find(|stream| stream.stream_index == 0)
                .map(|stream| stream.queued_packets),
            Some(2)
        );
        assert_eq!(
            snapshot
                .streams
                .iter()
                .find(|stream| stream.stream_index == 1)
                .map(|stream| stream.queued_packets),
            Some(1)
        );

        assert!(matches!(cache.poll_packet(1), DemuxReadResult::Packet(_)));
        let snapshot = cache.packet_queue_snapshot();
        assert_eq!(snapshot.total_packets, 2);
        assert_eq!(
            snapshot
                .streams
                .iter()
                .find(|stream| stream.stream_index == 1)
                .map(|stream| stream.queued_packets),
            None
        );
        assert!(matches!(cache.poll_packet(1), DemuxReadResult::WouldBlock));
        assert!(matches!(cache.poll_packet(0), DemuxReadResult::Packet(_)));
    }

    #[test]
    fn demux_packet_cache_round_robin_polls_selected_stream_queues_only() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, _) = shared_with_config_for_test(control, config);
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.set_stream_kind(1, StreamCacheKind::Audio);
            guard.set_stream_kind(2, StreamCacheKind::Subtitle);
            guard.append_packet(cached_anchor(0, 1_000_000_000));
            guard.append_packet(cached_packet(2, false, Some(0), Some(1_000_000_000)));
            guard.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        }
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };

        let (result, stream_offset) = cache.poll_packet_round_robin(&[1, 0]);
        let packet = match result {
            DemuxReadResult::Packet(packet) => packet,
            _ => panic!("expected selected audio packet"),
        };
        assert_eq!(packet.stream_index(), 1);
        assert_eq!(stream_offset, Some(0));

        let (result, stream_offset) = cache.poll_packet_round_robin(&[1, 0]);
        let packet = match result {
            DemuxReadResult::Packet(packet) => packet,
            _ => panic!("expected selected video packet"),
        };
        assert_eq!(packet.stream_index(), 0);
        assert_eq!(stream_offset, Some(1));

        let (result, stream_offset) = cache.poll_packet_round_robin(&[1, 0]);
        assert!(matches!(result, DemuxReadResult::WouldBlock));
        assert_eq!(stream_offset, None);

        let snapshot = cache.packet_queue_snapshot();
        assert_eq!(
            snapshot
                .streams
                .iter()
                .find(|stream| stream.stream_index == 2)
                .map(|stream| stream.queued_packets),
            Some(1)
        );

        let (result, stream_offset) = cache.poll_packet_round_robin(&[2]);
        let packet = match result {
            DemuxReadResult::Packet(packet) => packet,
            _ => panic!("expected unconsumed subtitle packet"),
        };
        assert_eq!(packet.stream_index(), 2);
        assert_eq!(stream_offset, Some(0));
    }

    #[test]
    fn demux_packet_cache_pauses_on_per_stream_packet_queue_limit() {
        let mut config = cache_config_for_test();
        config.demuxer_readahead_secs = 3600.0;
        config.demuxer_max_bytes = 0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        for packet_index in 0..DEMUX_STREAM_PACKET_QUEUE_LIMIT {
            let start_nsecs = packet_index as u64;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1));
        }

        let snapshot = state.packet_queue_snapshot();
        let video_queue = snapshot
            .streams
            .iter()
            .find(|stream| stream.stream_index == 0)
            .expect("video stream snapshot exists");
        assert_eq!(video_queue.queued_packets, DEMUX_STREAM_PACKET_QUEUE_LIMIT);
        assert_eq!(video_queue.packet_limit, DEMUX_STREAM_PACKET_QUEUE_LIMIT);
        assert!(video_queue.packet_queue_full);
        assert!(state.stream_packet_queue_full());
        assert!(state.should_pause_demux());
        assert_eq!(demux_cache_blocked_on(&state, false), "packet_queue_full");
    }

    #[test]
    fn demux_packet_cache_reads_needed_eager_stream_despite_other_stream_queue_limit() {
        let mut config = cache_config_for_test();
        config.demuxer_readahead_secs = 3600.0;
        config.demuxer_max_bytes = 0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);

        for packet_index in 0..DEMUX_STREAM_PACKET_QUEUE_LIMIT {
            let start_nsecs = packet_index as u64;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1));
        }

        let snapshot = state.packet_queue_snapshot();
        assert!(
            snapshot
                .streams
                .iter()
                .any(|stream| stream.stream_index == 0 && stream.packet_queue_full)
        );
        assert!(state.stream_packet_queue_full());
        assert!(state.has_demux_underrun());
        assert!(!state.should_pause_demux());
        assert_eq!(
            demux_cache_blocked_on(&state, false),
            "demux_cache_underrun"
        );
    }

    #[test]
    fn demux_packet_cache_does_not_pause_before_compressed_queue_limits() {
        let mut config = cache_config_for_test();
        config.demuxer_readahead_secs = 3600.0;
        config.demuxer_hysteresis_secs = 0.0;
        config.demuxer_max_bytes = 0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        for packet_index in 0..DEMUX_STREAM_PACKET_QUEUE_LIMIT - 1 {
            let start_nsecs = packet_index as u64;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1));
        }

        let snapshot = state.packet_queue_snapshot();
        let video_queue = snapshot
            .streams
            .iter()
            .find(|stream| stream.stream_index == 0)
            .expect("video stream snapshot exists");
        assert_eq!(
            video_queue.queued_packets,
            DEMUX_STREAM_PACKET_QUEUE_LIMIT - 1
        );
        assert!(!video_queue.packet_queue_full);
        assert!(!state.stream_packet_queue_full());
        assert!(!state.should_pause_demux());
        assert_eq!(demux_cache_blocked_on(&state, false), "demux_cache");
    }

    #[test]
    fn demux_packet_cache_reports_append_when_prefetch_limit_is_reached() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_secs = 1.0;
        config.demuxer_readahead_secs = 1.0;
        let (shared, event_rx) = shared_with_config_for_test(control, config);
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(0, 500_000_000));
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(500_000_000, 1_000_000_000));
        let events = event_rx.try_iter().collect::<Vec<_>>();

        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
        );
        assert!(
            shared
                .state
                .lock()
                .expect("cache state")
                .should_pause_demux()
        );
    }

    #[test]
    fn demux_packet_cache_state_seeks_inside_cached_timeline_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));

        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
            Some(2.0)
        );
        assert_eq!(state.read_index, 1);
        assert_eq!(state.reader_nsecs, 1_500_000_000);
        assert_eq!(state.session_id, PlaybackSessionId(2));
        assert_eq!(state.cached_seeks, 1);
        assert_eq!(state.low_level_seeks, 0);
        assert_eq!(state.playback_cache_state(false).demux.cached_seeks, 1);
    }

    #[test]
    fn demux_packet_cache_state_treats_initial_range_as_bof_even_with_positive_first_packet() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(500_000_000, 1_000_000_000));

        let cache_state = state.playback_cache_state(false);
        assert_eq!(
            cache_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.5,
                end: 1.0,
            }]
        );
        assert!(cache_state.demux.bof_cached);
        assert_eq!(state.seek_cached(0, PlaybackSessionId(2)), Some(1.0));
        assert_eq!(state.cached_seeks, 1);
    }

    #[test]
    fn demux_packet_cache_state_preserves_bof_flag_on_archived_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(500_000_000, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let cache_state = state.playback_cache_state(false);
        assert!(cache_state.demux.bof_cached);
        assert!(!state.read_range().is_bof);
        assert_eq!(state.seek_cached(0, PlaybackSessionId(3)), Some(1.0));
        assert_eq!(state.reader_nsecs, 0);
    }

    #[test]
    fn demux_packet_cache_state_omits_unseekable_bof_eof_ranges() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
        state.mark_eof();
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let cache_state = state.playback_cache_state(false);

        assert_eq!(
            cache_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 10.0,
                end: 11.0,
            }]
        );
        assert!(!cache_state.demux.bof_cached);
        assert!(!cache_state.demux.eof_cached);
    }

    #[test]
    fn demux_packet_cache_state_uses_eof_flag_for_cached_seek_after_last_packet() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.mark_eof();

        assert!(state.playback_cache_state(false).demux.eof_cached);
        assert_eq!(
            state.seek_cached(2_000_000_000, PlaybackSessionId(2)),
            Some(1.0)
        );
        assert!(state.read_range_eof());
    }

    #[test]
    fn demux_packet_cache_state_preserves_eof_flag_on_archived_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.mark_eof();
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        assert!(state.playback_cache_state(false).demux.eof_cached);
        assert_eq!(
            state.seek_cached(2_000_000_000, PlaybackSessionId(3)),
            Some(1.0)
        );
        assert!(state.read_range_eof());
        assert!(state.seek_request.is_none());
        assert_eq!(state.resume_append_skip_until_nsecs, None);
        assert_eq!(state.low_level_seeks, 1);
    }

    #[test]
    fn demux_packet_cache_state_reports_idle_when_effective_eof_comes_from_detached_append_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
        state.mark_eof();

        assert_eq!(
            state.seek_cached(500_000_000, PlaybackSessionId(3)),
            Some(1.0)
        );
        state.mark_eof();
        state.set_read_index_for_test(state.read_range().global_order.len());

        let cache_state = state.playback_cache_state(false);

        assert!(cache_state.demux.eof);
        assert!(cache_state.demux.idle);
        assert!(!cache_state.demux.underrun);
    }

    #[test]
    fn demux_packet_cache_state_does_not_mark_seeked_range_as_bof() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let cache_state = state.playback_cache_state(false);
        assert!(!cache_state.demux.bof_cached);
        assert!(!cache_state.demux.eof_cached);
    }

    #[test]
    fn demux_packet_cache_state_cached_seek_invalidates_inflight_demux_read() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        let generation = state.generation;

        assert_eq!(
            state.seek_cached(500_000_000, PlaybackSessionId(2)),
            Some(1.0)
        );
        assert!(state.generation > generation);
    }

    #[test]
    fn demux_packet_cache_discards_inflight_result_after_control_seek_request() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let shared = shared_for_test(Arc::clone(&control));
        let generation = shared.generation();
        let seek_generation = control.seek_generation();

        control.request_seek();

        assert!(shared.should_discard_demux_result(generation, seek_generation));
    }

    #[test]
    fn demux_packet_cache_skips_stale_low_level_seek_request() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let first_generation = control.request_seek();
        let shared = shared_for_test(Arc::clone(&control));
        let request = DemuxSeekRequest {
            position_seconds: 10.0,
            session_id: PlaybackSessionId(1),
            seek_generation: first_generation,
        };

        assert!(!shared.should_skip_seek_request(&request));
        control.request_seek();
        assert!(shared.should_skip_seek_request(&request));
    }

    #[test]
    fn demux_packet_cache_pause_enters_on_underrun_and_resumes_after_wait_target() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 2.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, true);
        }

        assert!(control.is_cache_paused());
        assert!(control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(BackendEventKind::PausedForCacheChanged(true))
        ));
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheBufferingChanged(Some(0))
            )
        }));
        assert!(
            events
                .iter()
                .any(|event| { matches!(&event.kind, BackendEventKind::Pause(true)) })
        );
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheStateChanged(state)
                    if state.paused_for_cache && state.buffering_percent == Some(0)
            )
        }));

        shared.append_packet(cached_anchor(0, 2_000_000_000));

        assert!(!control.is_cache_paused());
        assert!(!control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events.iter().any(|event| {
                matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None))
            })
        );
        assert!(events.iter().any(|event| {
            matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false))
        }));
        assert!(
            events
                .iter()
                .any(|event| { matches!(&event.kind, BackendEventKind::Pause(false)) })
        );
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheStateChanged(state)
                    if !state.paused_for_cache && state.buffering_percent.is_none()
            )
        }));
    }

    #[test]
    fn demux_packet_cache_read_activates_detached_append_range_before_cache_pause_wait() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 10.0;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };
        {
            let mut guard = cache
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.append_packet(cached_anchor(0, 1_000_000_000));
            let read_range_len = guard.read_range().global_order.len();
            guard.set_read_index_for_test(read_range_len);
            guard.start_detached_append_range();
            guard.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        }
        control.set_cache_paused(true);

        let (result, stream_offset) = cache
            .read_available_packet_round_robin_with_cache_pause_signal(
                &[0],
                Duration::from_millis(0),
                false,
            );

        assert!(matches!(result, DemuxReadResult::Packet(_)));
        assert_eq!(stream_offset, Some(0));
    }

    #[test]
    fn demux_packet_cache_pause_resume_keeps_user_pause_active() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        control.set_user_paused(true);
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 1.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, true);
        }
        assert!(control.is_paused());
        assert!(control.is_user_paused());
        assert!(control.is_cache_paused());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(0, 1_000_000_000));

        assert!(control.is_paused());
        assert!(control.is_user_paused());
        assert!(!control.is_cache_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| { matches!(&event.kind, BackendEventKind::Pause(true)) })
        );
    }

    #[test]
    fn demux_packet_cache_apply_config_disables_cache_pause_and_clears_buffering() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 2.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config.clone());
        let shared = Arc::new(shared);
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, true);
        }
        assert!(control.is_cache_paused());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        config.cache_pause = false;
        cache.apply_cache_config(config);

        assert!(!control.is_cache_paused());
        assert!(!control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::Pause(false)))
        );
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheStateChanged(state)
                    if !state.paused_for_cache && state.buffering_percent.is_none()
            )
        }));
    }

    #[test]
    fn demux_packet_cache_apply_config_resumes_when_new_wait_target_is_met() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 10.0;
        config.demuxer_readahead_secs = 20.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config.clone());
        let shared = Arc::new(shared);
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };

        shared.append_packet(cached_anchor(0, 2_000_000_000));
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause(&mut guard);
        }
        assert!(control.is_cache_paused());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        config.cache_pause_wait = 1.0;
        cache.apply_cache_config(config);

        assert!(!control.is_cache_paused());
        assert!(!control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false)))
        );
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheStateChanged(state)
                    if !state.paused_for_cache && state.buffering_percent.is_none()
            )
        }));
    }

    #[test]
    fn demux_packet_cache_pause_percent_is_capped_below_100() {
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        state.append_packet(cached_anchor(0, 2_000_000_000));

        assert_eq!(state.cache_pause_percent(), Some(99));
    }

    #[test]
    fn demux_packet_cache_reports_underrun_promptly_without_cache_pause() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, event_rx) = shared_with_config_for_test(control, config);
        let shared = Arc::new(shared);
        let read_shared = Arc::clone(&shared);
        let read_handle = thread::spawn(move || {
            let cache = DemuxPacketCache {
                shared: read_shared,
                handle: None,
            };
            cache.read_packet_round_robin(&[0]).0
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut underrun_state = None;
        while Instant::now() < deadline {
            for event in event_rx.try_iter() {
                if let BackendEventKind::CacheStateChanged(state) = event.kind
                    && state.demux.underrun
                    && !state.demux.idle
                {
                    underrun_state = Some(state);
                    break;
                }
            }
            if underrun_state.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.shutdown = true;
        }
        shared.ready.notify_all();

        assert!(matches!(
            read_handle.join().expect("read thread joins"),
            DemuxReadResult::Interrupted
        ));
        let underrun_state = underrun_state.expect("underrun emits cache state promptly");
        assert!(!underrun_state.paused_for_cache);
        assert_eq!(underrun_state.buffering_percent, None);
    }

    #[test]
    fn demux_packet_cache_pause_resumes_on_eof_before_wait_target() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 10.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, true);
        }
        assert!(control.is_cache_paused());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.mark_eof();

        assert!(!control.is_cache_paused());
        assert!(!control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::Pause(false)))
        );
    }

    #[test]
    fn demux_packet_cache_pause_resumes_when_demux_becomes_idle_before_wait_target() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 10.0;
        config.cache_secs = 1.0;
        config.demuxer_readahead_secs = 1.0;
        let (shared, event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, true);
        }
        assert!(control.is_cache_paused());
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(0, 1_000_000_000));

        assert!(!control.is_cache_paused());
        assert!(!control.is_paused());
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None)))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false)))
        );
        let cache_state = events.iter().find_map(|event| match &event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state),
            _ => None,
        });
        assert!(cache_state.is_some_and(|state| state.demux.idle));
    }

    #[test]
    fn demux_packet_cache_pause_does_not_enter_without_output_wait_signal() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 1.0;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            shared.enter_cache_pause_if_needed(&mut guard, false);
        }

        assert!(!control.is_cache_paused());
    }

    #[test]
    fn demux_packet_cache_try_read_returns_would_block_without_marking_underrun() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };

        let started_at = Instant::now();

        assert!(matches!(
            cache.poll_packet_round_robin(&[0]).0,
            DemuxReadResult::WouldBlock
        ));
        assert!(
            started_at.elapsed() < Duration::from_millis(50),
            "nonblocking demux read should not wait for cache data"
        );
    }

    #[test]
    fn demux_packet_cache_try_read_returns_would_block_when_state_lock_is_busy() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };
        let _guard = cache
            .shared
            .state
            .lock()
            .expect("test demux packet cache lock");

        let started_at = Instant::now();

        assert!(matches!(
            cache.poll_packet_round_robin(&[0]).0,
            DemuxReadResult::WouldBlock
        ));
        assert!(
            started_at.elapsed() < Duration::from_millis(50),
            "nonblocking demux read should not wait for the shared cache lock"
        );
    }

    #[test]
    fn demux_packet_cache_available_read_serves_cached_packet_while_cache_paused() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_wait = 10.0;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.append_packet(cached_anchor(0, 1_000_000_000));
            shared.enter_cache_pause(&mut guard);
        }
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };

        assert!(control.is_cache_paused());

        let (result, stream_offset) =
            cache.read_available_packet_round_robin_with_lock_wait(&[0], Duration::from_millis(2));

        assert!(matches!(result, DemuxReadResult::Packet(_)));
        assert_eq!(stream_offset, Some(0));
        assert!(control.is_cache_paused());
    }

    #[test]
    fn demux_packet_cache_available_read_waits_for_busy_lock() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        let shared = Arc::new(shared);
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.append_packet(cached_anchor(0, 1_000_000_000));
        }
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };
        let guard = shared.state.lock().expect("test demux packet cache lock");

        let reader = thread::spawn(move || cache.read_packet_round_robin(&[0]));
        thread::sleep(Duration::from_millis(25));
        drop(guard);

        let (result, stream_offset) = reader.join().expect("reader thread exits");
        assert!(matches!(result, DemuxReadResult::Packet(_)));
        assert_eq!(stream_offset, Some(0));
    }

    #[test]
    fn demux_packet_cache_bounded_available_read_gives_up_on_busy_lock() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        let shared = Arc::new(shared);
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.append_packet(cached_anchor(0, 1_000_000_000));
        }
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };
        let _guard = shared.state.lock().expect("test demux packet cache lock");
        let started_at = Instant::now();

        let (result, stream_offset) =
            cache.read_available_packet_round_robin_with_lock_wait(&[0], Duration::from_millis(2));

        assert!(matches!(result, DemuxReadResult::WouldBlock));
        assert_eq!(stream_offset, None);
        assert!(
            started_at.elapsed() < Duration::from_millis(50),
            "bounded available demux read should not wait indefinitely for cache lock"
        );
    }

    #[test]
    fn demux_packet_cache_available_read_does_not_wait_for_data() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, _event_rx) = shared_with_config_for_test(control, config);
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };
        let started_at = Instant::now();

        let (result, stream_offset) =
            cache.read_available_packet_round_robin_with_lock_wait(&[0], Duration::from_millis(2));

        assert!(matches!(result, DemuxReadResult::WouldBlock));
        assert_eq!(stream_offset, None);
        assert!(
            started_at.elapsed() < Duration::from_millis(50),
            "available demux read should not wait for cache data"
        );
    }

    #[test]
    fn demux_packet_cache_initial_pause_enters_without_output_gate_signal() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = true;
        config.cache_pause_initial = true;
        config.cache_pause_wait = 1.0;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

        shared.enter_initial_cache_pause_if_needed();

        assert!(control.is_cache_paused());
    }

    #[test]
    fn demux_packet_cache_blocking_read_waits_for_demux_without_output_gate_signal() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
        let shared = Arc::new(shared);
        let cache = DemuxPacketCache {
            shared: Arc::clone(&shared),
            handle: None,
        };

        let reader = thread::spawn(move || cache.read_packet_round_robin(&[0]).0);
        thread::sleep(Duration::from_millis(50));

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.shutdown = true;
            shared.ready.notify_all();
        }

        assert!(matches!(
            reader.join().expect("reader thread exits"),
            DemuxReadResult::Interrupted
        ));
    }

    #[test]
    fn demux_packet_cache_poll_returns_would_block_without_output_gate_signal() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let mut config = cache_config_for_test();
        config.cache_pause = false;
        let (shared, _event_rx) = shared_with_config_for_test(control, config);
        let cache = DemuxPacketCache {
            shared: Arc::new(shared),
            handle: None,
        };

        let (result, stream_offset) = cache.poll_packet_round_robin(&[0]);

        assert!(matches!(result, DemuxReadResult::WouldBlock));
        assert_eq!(stream_offset, None);
    }

    #[test]
    fn demux_packet_cache_state_seeks_from_nearest_previous_keyframe() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(
            state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
            Some(4.0)
        );
        assert_eq!(state.read_index, 2);
        assert_eq!(state.reader_nsecs, 3_500_000_000);
    }

    #[test]
    fn demux_packet_cache_state_prerolls_hevc_cached_seek() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(
            state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
            Some(4.0)
        );
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 3_500_000_000);
    }

    #[test]
    fn demux_packet_cache_state_rejects_hevc_cached_seek_without_preroll() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(state.seek_cached(3_500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 0);
        assert_eq!(state.cached_seeks, 0);
    }

    #[test]
    fn demux_packet_cache_state_fast_hevc_cached_seek_uses_nearest_recovery_point() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(
            state.seek_cached_fast(3_500_000_000, PlaybackSessionId(2)),
            Some(4.0)
        );
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 3_500_000_000);
        assert_eq!(state.cached_seeks, 1);
    }

    #[test]
    fn demux_packet_cache_state_rejects_hevc_cached_seek_with_short_preroll_window() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(4_000_000_000),
            Some(5_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(5_000_000_000),
            Some(6_000_000_000),
        ));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(6_000_000_000),
            Some(7_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(7_000_000_000),
            Some(8_000_000_000),
        ));

        assert_eq!(state.seek_cached(7_500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 0);
    }

    #[test]
    fn demux_packet_cache_state_requires_previous_keyframe() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));

        assert_eq!(state.seek_cached(1_500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 0);
    }

    #[test]
    fn demux_packet_cache_state_requires_previous_recovery_point() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        let mut key_packet = cached_key_packet(0, true, Some(0), Some(1_000_000_000));
        key_packet.recovery_point = false;
        state.append_packet(key_packet);
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));

        assert_eq!(state.seek_cached(1_500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 0);
    }

    #[test]
    fn demux_packet_cache_state_reports_seekable_range_after_first_recovery_point() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 1.0,
                end: 3.0,
            }]
        );
        assert_eq!(state.seek_cached(500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
            Some(3.0)
        );
        assert_eq!(state.read_index, 1);
    }

    #[test]
    fn demux_packet_cache_state_reports_full_active_seekable_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.set_read_index_for_test(2);
        state.reader_nsecs = 2_000_000_000;

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 3.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_keeps_consumed_packet_in_seekable_backbuffer_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

        assert!(
            state
                .take_packet_round_robin(&[0])
                .expect("read packet")
                .is_some()
        );

        let cache_state = state.playback_cache_state(false);
        assert_eq!(state.read_index, 1);
        assert_eq!(cache_state.demux.reader_pts, Some(1.0));
        assert_eq!(cache_state.demux.cache_duration, Some(2.0));
        assert_eq!(
            cache_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 3.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_donates_unused_forward_budget_after_fast_seek() {
        let mut config = cache_config_for_test();
        config.demuxer_max_bytes = 8 * 1024;
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = true;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        for index in 0..6 {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }

        assert_eq!(
            state.seek_cached_fast(4_500_000_000, PlaybackSessionId(2)),
            Some(6.0)
        );
        assert_eq!(state.read_index, 4);
        assert!(state.backward_bytes() <= state.effective_backbuffer_limit());
        assert!(!state.trim_to_limit());

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 6.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_trims_active_backbuffer_after_fast_seek() {
        let mut config = cache_config_for_test();
        config.demuxer_max_bytes = 6 * 1024;
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        for index in 0..6 {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }

        assert_eq!(
            state.seek_cached_fast(4_500_000_000, PlaybackSessionId(2)),
            Some(6.0)
        );

        assert_eq!(state.backward_bytes(), 1024);
        assert_eq!(state.forward_bytes(), 2 * 1024);
        assert_eq!(state.read_index, 1);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 3.0,
                end: 6.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_cached_seek_sets_per_stream_reader_heads() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_packet(
            1,
            false,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));

        assert_eq!(
            state.seek_cached_fast(2_500_000_000, PlaybackSessionId(2)),
            Some(3.0)
        );

        assert_eq!(state.reader_heads.get(&0), Some(&2));
        assert_eq!(state.reader_heads.get(&1), Some(&5));
        assert_eq!(state.read_index, 2);
        assert_eq!(state.forward_bytes(), 2 * 1024);
        assert!(!state.active_packet_is_forward(3));
        assert!(!state.active_packet_is_forward(4));
        assert!(state.active_packet_is_forward(5));
    }

    #[test]
    fn demux_packet_cache_state_active_trim_never_crosses_per_stream_reader_heads() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_packet(
            1,
            false,
            Some(2_000_000_000),
            Some(3_000_000_000),
        ));

        assert_eq!(
            state.seek_cached_fast(2_500_000_000, PlaybackSessionId(2)),
            Some(3.0)
        );

        assert_eq!(state.forward_bytes(), 2 * 1024);
        assert_eq!(state.backward_bytes(), 0);
        assert_eq!(state.next_packet_id_for_stream(0), Some(2));
        assert_eq!(state.next_packet_id_for_stream(1), Some(5));
        assert_eq!(
            state.read_range().stream_queues.get(&0).cloned(),
            Some(VecDeque::from([2]))
        );
        assert_eq!(
            state.read_range().stream_queues.get(&1).cloned(),
            Some(VecDeque::from([5]))
        );
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 2.0,
                end: 3.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_forward_growth_reclaims_donated_backbuffer() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1000.0;
        config.demuxer_readahead_secs = 1000.0;
        config.demuxer_max_bytes = 4 * 1024;
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = true;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        for index in 0..5 {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }

        assert_eq!(
            state.seek_cached_fast(4_500_000_000, PlaybackSessionId(2)),
            Some(5.0)
        );
        assert_eq!(state.forward_bytes(), 1024);
        assert_eq!(state.backward_bytes(), 3 * 1024);

        state.append_packet(cached_anchor(5_000_000_000, 6_000_000_000));

        assert_eq!(state.forward_bytes(), 2 * 1024);
        assert_eq!(state.backward_bytes(), 2 * 1024);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 2.0,
                end: 6.0,
            }]
        );
    }

    #[test]
    fn demux_packet_cache_queue_full_ignores_consumed_backbuffer_packets() {
        let mut config = cache_config_for_test();
        config.demuxer_readahead_secs = 3600.0;
        config.demuxer_max_bytes = 0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );

        for packet_index in 0..DEMUX_STREAM_PACKET_QUEUE_LIMIT {
            let start_nsecs = packet_index as u64;
            state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1));
        }
        assert!(state.stream_packet_queue_full());

        state.consume_packet_id(0);

        let snapshot = state.packet_queue_snapshot();
        let video_queue = snapshot
            .streams
            .iter()
            .find(|stream| stream.stream_index == 0)
            .expect("video stream snapshot exists");
        assert_eq!(
            video_queue.queued_packets,
            DEMUX_STREAM_PACKET_QUEUE_LIMIT - 1
        );
        assert!(!video_queue.packet_queue_full);
        assert!(!state.stream_packet_queue_full());
        assert!(!state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_intersects_seekable_range_with_selected_audio() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 5_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(2_000_000_000)));

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 2.0,
            }]
        );
        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
            Some(2.0)
        );
        assert_eq!(state.seek_cached(4_000_000_000, PlaybackSessionId(3)), None);
    }

    #[test]
    fn demux_packet_cache_state_rejects_cached_seek_inside_audio_gap() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 5_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(
            1,
            false,
            Some(3_000_000_000),
            Some(5_000_000_000),
        ));

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![
                PlaybackCacheTimeRange {
                    start: 0.0,
                    end: 1.0,
                },
                PlaybackCacheTimeRange {
                    start: 3.0,
                    end: 5.0,
                },
            ]
        );
        assert_eq!(state.seek_cached(2_000_000_000, PlaybackSessionId(2)), None);
        assert_eq!(
            state.seek_cached(4_000_000_000, PlaybackSessionId(3)),
            Some(5.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_splits_seekable_ranges_at_timeline_gaps() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![
                PlaybackCacheTimeRange {
                    start: 0.0,
                    end: 1.0,
                },
                PlaybackCacheTimeRange {
                    start: 3.0,
                    end: 4.0,
                },
            ]
        );
        assert_eq!(state.seek_cached(2_000_000_000, PlaybackSessionId(2)), None);
    }

    #[test]
    fn demux_packet_cache_state_reports_hevc_seekable_range_without_precise_preroll() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(4_000_000_000),
            Some(5_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(5_000_000_000),
            Some(6_000_000_000),
        ));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(6_000_000_000),
            Some(7_000_000_000),
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(7_000_000_000),
            Some(8_000_000_000),
        ));

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 4.0,
                end: 8.0,
            }]
        );
        assert_eq!(state.seek_cached(7_500_000_000, PlaybackSessionId(2)), None);
        assert_eq!(
            state.seek_cached_fast(7_500_000_000, PlaybackSessionId(2)),
            Some(8.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_rejects_timeline_gaps() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_key_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));

        assert_eq!(state.seek_cached(2_000_000_000, PlaybackSessionId(2)), None);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.reader_nsecs, 0);
    }

    #[test]
    fn demux_packet_cache_state_requests_low_level_seek_outside_cache() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));

        assert_eq!(state.seek_cached(3_000_000_000, PlaybackSessionId(7)), None);
        state.request_seek(3.0, PlaybackSessionId(7), 1, 3_000_000_000);

        assert_eq!(state.packets.len(), 1);
        assert_eq!(state.ranges.len(), 2);
        assert!(state.read_range().global_order.is_empty());
        assert!(state.read_range().stream_queues.is_empty());
        assert_eq!(state.read_index, 0);
        assert_eq!(state.cached_bytes, 1024);
        assert_eq!(state.reader_nsecs, 3_000_000_000);
        assert_eq!(state.session_id, PlaybackSessionId(7));
        assert_eq!(state.cached_seeks, 0);
        assert_eq!(state.low_level_seeks, 1);
        assert!(state.playback_cache_state(false).demux.seeking);
        assert_eq!(
            state.seek_request.map(|request| request.session_id),
            Some(PlaybackSessionId(7))
        );
    }

    #[test]
    fn demux_packet_cache_state_clears_seeking_after_seek_result_appends() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(3.0, PlaybackSessionId(2), 1, 3_000_000_000);

        assert!(state.playback_cache_state(false).demux.seeking);

        let _ = state.take_seek_request().expect("low-level seek is taken");
        state.append_packet(cached_anchor(3_000_000_000, 4_000_000_000));

        assert!(!state.playback_cache_state(false).demux.seeking);
    }

    #[test]
    fn demux_packet_cache_reports_seek_completion_promptly_after_seek_result_appends() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
            let _ = guard.take_seek_request().expect("low-level seek is taken");
            guard.last_cache_state_emit_at = Some(Instant::now());
        }
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        shared.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                &event.kind,
                BackendEventKind::CacheStateChanged(state)
                    if !state.demux.seeking
                        && state.demux.reader_pts == Some(10.0)
                        && state.demux.cache_end == Some(11.0)
            )
        }));
    }

    #[test]
    fn demux_packet_cache_state_indexes_archived_ranges_by_range_id() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
        state.request_seek(20.0, PlaybackSessionId(3), 2, 20_000_000_000);
        state.append_packet(cached_anchor(20_000_000_000, 21_000_000_000));

        assert_eq!(state.read_range_id, 2);
        assert_eq!(state.append_range_id, 2);
        assert_eq!(
            state
                .ranges
                .keys()
                .copied()
                .filter(|range_id| *range_id != state.read_range_id)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );

        assert_eq!(
            state.seek_cached(500_000_000, PlaybackSessionId(4)),
            Some(1.0)
        );

        assert_eq!(state.read_range_id, 0);
        assert_ne!(state.read_range_id, state.append_range_id);
        assert_eq!(
            state
                .ranges
                .iter()
                .filter(|(range_id, _)| **range_id != state.read_range_id)
                .filter(|(range_id, _)| **range_id != state.append_range_id)
                .map(|(range_id, _)| *range_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            state
                .ranges
                .get(&state.append_range_id)
                .map(|range| range.global_order.len()),
            Some(0)
        );
    }

    #[test]
    fn demux_packet_cache_state_seeks_inside_archived_range_after_low_level_seek() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        assert_eq!(
            state.seek_cached(500_000_000, PlaybackSessionId(3)),
            Some(1.0)
        );
        assert_eq!(state.reader_nsecs, 500_000_000);
        assert_eq!(state.session_id, PlaybackSessionId(3));
        assert_eq!(state.read_index, 0);
        assert!(!state.demux_position_detached);
        assert_eq!(state.resume_append_skip_until_nsecs, Some(1_000_000_000));
        assert_ne!(state.read_range_id, state.append_range_id);
        assert_eq!(
            state
                .ranges
                .get(&state.append_range_id)
                .map(|range| (range.id, range.global_order.len())),
            Some((state.append_range_id, 0))
        );
        assert_eq!(state.archived_bytes(), 1024);
        assert_eq!(state.cached_seeks, 1);
        assert_eq!(state.low_level_seeks, 2);
        assert!(state.playback_cache_state(false).demux.seeking);
        let request = state.seek_request.expect("resume seek is queued");
        assert_eq!(request.position_seconds, 1.0);
        assert_eq!(request.session_id, PlaybackSessionId(3));
    }

    #[test]
    fn demux_packet_cache_state_skips_resume_overlap_packets_after_archived_seek() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
        assert_eq!(
            state.seek_cached_with_generation(
                500_000_000,
                PlaybackSeekMode::Precise,
                PlaybackSessionId(3),
                7
            ),
            Some(1.0)
        );
        assert_eq!(state.read_range().global_order.len(), 1);

        state.append_packet(cached_anchor(500_000_000, 1_000_000_000));

        assert_eq!(state.read_range().global_order.len(), 1);
        assert_eq!(state.cached_bytes, 2 * 1024);
        assert_eq!(state.resume_append_skip_until_nsecs, Some(1_000_000_000));
        let request = state.seek_request.expect("resume seek is queued");
        assert_eq!(request.seek_generation, 7);

        state.seek_request = None;
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));

        assert_eq!(state.read_range().global_order.len(), 1);
        assert_eq!(
            state
                .ranges
                .get(&state.append_range_id)
                .map(|range| range.global_order.len()),
            Some(1)
        );
        assert_eq!(state.cached_bytes, 3 * 1024);
        assert_eq!(state.forward_bytes(), 2 * 1024);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![
                PlaybackCacheTimeRange {
                    start: 0.0,
                    end: 2.0
                },
                PlaybackCacheTimeRange {
                    start: 10.0,
                    end: 11.0
                },
            ]
        );
        assert_eq!(state.resume_append_skip_until_nsecs, None);

        state.set_read_index_for_test(state.read_range().global_order.len());
        assert!(state.activate_detached_append_range());
        assert_eq!(state.read_range_id, state.append_range_id);
        assert_eq!(state.read_range().global_order.len(), 1);
        assert!(state.detached_append_range().is_none());
    }

    #[test]
    fn demux_packet_cache_state_requests_continuation_after_detached_range_exhausts() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.demux_position_detached = true;
        state.set_read_index_for_test(1);
        state.reader_nsecs = 1_000_000_000;

        state.request_continuation_seek(4);

        assert!(!state.demux_position_detached);
        assert!(state.read_range().global_order.is_empty());
        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.low_level_seeks, 1);
        let request = state.seek_request.expect("continuation seek is queued");
        assert_eq!(request.position_seconds, 1.0);
        assert_eq!(request.seek_generation, 4);
    }

    #[test]
    fn demux_packet_cache_reports_continuation_seek_promptly_after_detached_range_exhausts() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
        let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.append_packet(cached_anchor(0, 1_000_000_000));
            guard.demux_position_detached = true;
            guard.set_read_index_for_test(1);
            guard.reader_nsecs = 1_000_000_000;
        }
        let shared = Arc::new(shared);
        let read_shared = Arc::clone(&shared);
        let read_handle = thread::spawn(move || {
            let cache = DemuxPacketCache {
                shared: read_shared,
                handle: None,
            };
            cache.read_packet_round_robin(&[0]).0
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut continuation_state = None;
        while Instant::now() < deadline {
            for event in event_rx.try_iter() {
                if let BackendEventKind::CacheStateChanged(state) = event.kind
                    && state.demux.seeking
                    && state.demux.low_level_seeks == 1
                {
                    continuation_state = Some(state);
                    break;
                }
            }
            if continuation_state.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.shutdown = true;
        }
        shared.ready.notify_all();

        assert!(matches!(
            read_handle.join().expect("read thread joins"),
            DemuxReadResult::Interrupted
        ));
        let continuation_state =
            continuation_state.expect("continuation seek emits cache state promptly");
        assert_eq!(continuation_state.demux.reader_pts, Some(1.0));
        assert_eq!(
            continuation_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0
            }]
        );
    }

    #[test]
    fn demux_packet_cache_state_reports_multiple_seekable_ranges() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let cache_state = state.playback_cache_state(false);

        assert_eq!(
            cache_state.demux.seekable_ranges,
            vec![
                PlaybackCacheTimeRange {
                    start: 0.0,
                    end: 1.0
                },
                PlaybackCacheTimeRange {
                    start: 10.0,
                    end: 11.0
                }
            ]
        );
        assert_eq!(cache_state.demux.cached_seeks, 0);
        assert_eq!(cache_state.demux.low_level_seeks, 1);
        assert_eq!(cache_state.demux.total_bytes, 2048);
    }

    #[test]
    fn demux_packet_cache_state_reports_stream_kinds() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.set_stream_kind(2, StreamCacheKind::Subtitle);
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(500_000_000)));
        state.append_packet(cached_packet(2, false, Some(0), Some(2_000_000_000)));

        let streams = state.playback_cache_state(false).demux.streams;

        assert_eq!(
            streams.iter().map(|stream| stream.kind).collect::<Vec<_>>(),
            vec![
                StreamCacheKind::Video,
                StreamCacheKind::Audio,
                StreamCacheKind::Subtitle,
            ]
        );
        assert_eq!(streams[0].cache_duration, Some(1.0));
        assert_eq!(streams[1].cache_duration, Some(0.5));
        assert_eq!(streams[2].cache_duration, Some(2.0));
    }

    #[test]
    fn demux_packet_cache_state_omits_invalid_stream_cache_duration() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_packet(
            1,
            false,
            Some(2_000_000_000),
            Some(1_000_000_000),
        ));

        let streams = state.playback_cache_state(false).demux.streams;

        let audio = streams
            .iter()
            .find(|stream| stream.kind == StreamCacheKind::Audio)
            .expect("audio stream cache state");
        assert_eq!(audio.reader_pts, Some(2.0));
        assert_eq!(audio.cache_end, Some(1.0));
        assert_eq!(audio.cache_duration, None);
    }

    #[test]
    fn demux_packet_cache_state_stream_windows_ignore_archived_ranges() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        let streams = state.playback_cache_state(false).demux.streams;

        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].reader_pts, Some(10.0));
        assert_eq!(streams[0].cache_end, Some(11.0));
        assert_eq!(streams[0].cache_duration, Some(1.0));
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![
                PlaybackCacheTimeRange {
                    start: 0.0,
                    end: 1.0
                },
                PlaybackCacheTimeRange {
                    start: 10.0,
                    end: 11.0
                }
            ]
        );
    }

    #[test]
    fn demux_packet_cache_state_stream_windows_ignore_consumed_packets() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.set_read_index_for_test(2);
        state.reader_nsecs = 1_000_000_000;

        let streams = state.playback_cache_state(false).demux.streams;

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].kind, StreamCacheKind::Video);
        assert_eq!(streams[0].reader_pts, Some(1.0));
        assert_eq!(streams[0].cache_end, Some(2.0));
        assert_eq!(streams[0].cache_duration, Some(1.0));
        assert!(!streams[0].underrun);
        assert_eq!(streams[1].kind, StreamCacheKind::Audio);
        assert_eq!(streams[1].reader_pts, Some(1.0));
        assert_eq!(streams[1].cache_end, Some(2.0));
        assert_eq!(streams[1].cache_duration, Some(1.0));
        assert!(!streams[1].underrun);

        state.set_read_index_for_test(4);
        state.reader_nsecs = 2_000_000_000;
        let streams = state.playback_cache_state(false).demux.streams;
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].kind, StreamCacheKind::Video);
        assert_eq!(streams[0].reader_pts, Some(2.0));
        assert_eq!(streams[0].cache_end, Some(2.0));
        assert_eq!(streams[0].cache_duration, Some(0.0));
        assert!(streams[0].underrun);
        assert!(!streams[0].idle);
        assert_eq!(streams[1].kind, StreamCacheKind::Audio);
        assert_eq!(streams[1].reader_pts, Some(2.0));
        assert_eq!(streams[1].cache_end, Some(2.0));
        assert_eq!(streams[1].cache_duration, Some(0.0));
        assert!(streams[1].underrun);
        assert!(!streams[1].idle);
    }

    #[test]
    fn demux_packet_cache_state_reports_per_stream_idle_and_underrun() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1.0;
        config.demuxer_readahead_secs = 1.0;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));

        let streams = state.playback_cache_state(false).demux.streams;

        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].kind, StreamCacheKind::Video);
        assert_eq!(streams[0].cache_duration, Some(1.0));
        assert!(!streams[0].underrun);
        assert!(streams[0].idle);
        assert_eq!(streams[1].kind, StreamCacheKind::Audio);
        assert_eq!(streams[1].reader_pts, Some(0.0));
        assert_eq!(streams[1].cache_end, Some(0.0));
        assert_eq!(streams[1].cache_duration, Some(0.0));
        assert!(streams[1].underrun);
        assert!(!streams[1].idle);
    }

    #[test]
    fn demux_packet_cache_reader_watermark_reports_selected_stream_minimum() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 2_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_500_000_000)));

        let watermark = state.reader_watermark();

        assert_eq!(watermark.video_forward_nsecs, Some(2_000_000_000));
        assert_eq!(watermark.audio_forward_nsecs, Some(1_500_000_000));
        assert_eq!(watermark.selected_min_forward_nsecs, Some(1_500_000_000));
        assert!(!watermark.video_underrun);
        assert!(!watermark.audio_underrun);
        assert!(!watermark.underrun);
    }

    #[test]
    fn demux_packet_cache_reader_watermark_reports_per_stream_underrun() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));

        let watermark = state.reader_watermark();

        assert_eq!(watermark.video_forward_nsecs, Some(1_000_000_000));
        assert_eq!(watermark.audio_forward_nsecs, Some(0));
        assert_eq!(watermark.selected_min_forward_nsecs, Some(0));
        assert!(!watermark.video_underrun);
        assert!(watermark.audio_underrun);
        assert!(watermark.underrun);
    }

    #[test]
    fn demux_packet_cache_reader_watermark_ignores_detached_append_range_until_activated() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.set_read_index_for_test(state.read_range().global_order.len());
        state.start_detached_append_range();
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));

        let watermark = state.reader_watermark();
        let snapshot = state.packet_queue_snapshot();

        assert_eq!(watermark.video_forward_nsecs, Some(0));
        assert_eq!(watermark.selected_min_forward_nsecs, Some(0));
        assert_eq!(watermark.forward_bytes, 0);
        assert!(watermark.video_underrun);
        assert!(watermark.underrun);
        assert_eq!(snapshot.total_packets, 1);
        assert_eq!(snapshot.streams[0].forward_nsecs, Some(0));

        assert!(state.activate_detached_append_range());
        let watermark = state.reader_watermark();

        assert_eq!(watermark.video_forward_nsecs, Some(1_000_000_000));
        assert_eq!(watermark.forward_bytes, 1024);
        assert!(!watermark.video_underrun);
        assert!(!watermark.underrun);
    }

    #[test]
    fn demux_packet_cache_prefetch_pause_uses_readahead_hysteresis_independent_of_output() {
        let mut config = cache_config_for_test();
        config.cache_secs = 2.0;
        config.demuxer_readahead_secs = 2.0;
        config.demuxer_hysteresis_secs = 1.0;
        config.demuxer_max_bytes = 16 * 1024;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.hysteresis_active = true;
        state.append_packet(cached_anchor(500_000_000, 2_000_000_000));

        assert!(state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_prunes_archived_ranges_by_backbuffer_limit() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
        state.request_seek(20.0, PlaybackSessionId(3), 2, 20_000_000_000);

        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.archived_bytes(), 1024);
        assert_eq!(state.cached_bytes, 1024);
        assert_eq!(state.seek_cached(500_000_000, PlaybackSessionId(4)), None);
        assert_eq!(
            state.seek_cached(10_500_000_000, PlaybackSessionId(4)),
            Some(11.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_prunes_archived_range_at_recovery_boundaries() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 2 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.append_packet(cached_anchor(3_000_000_000, 4_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.archived_bytes(), 2 * 1024);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges[0],
            PlaybackCacheTimeRange {
                start: 2.0,
                end: 4.0,
            }
        );
        assert_eq!(state.seek_cached(500_000_000, PlaybackSessionId(3)), None);
        assert_eq!(
            state.seek_cached(2_500_000_000, PlaybackSessionId(3)),
            Some(4.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_prunes_non_anchor_packets_with_archived_prefix() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 3 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        let range = state
            .ranges
            .values()
            .next()
            .expect("archived range remains");
        assert_eq!(state.archived_bytes(), 3 * 1024);
        assert_eq!(range.global_order.len(), 3);
        assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(2));
        assert_eq!(range.stream_queues.get(&1).map(VecDeque::len), Some(1));
        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(3)),
            Some(3.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_prunes_non_anchor_prefix_without_shrinking_seekable_range() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 3 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_packet(1, false, Some(0), Some(500_000_000)));
        state.append_packet(cached_anchor(500_000_000, 1_500_000_000));
        state.append_packet(cached_packet(
            1,
            false,
            Some(500_000_000),
            Some(2_500_000_000),
        ));
        state.append_packet(cached_anchor(1_500_000_000, 2_500_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        let range = state
            .ranges
            .values()
            .next()
            .expect("archived range remains");
        assert_eq!(state.archived_bytes(), 3 * 1024);
        assert_eq!(range.global_order.len(), 3);
        assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(2));
        assert_eq!(range.stream_queues.get(&1).map(VecDeque::len), Some(1));
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges[0],
            PlaybackCacheTimeRange {
                start: 0.5,
                end: 2.5,
            }
        );
        assert_eq!(
            state.seek_cached(750_000_000, PlaybackSessionId(3)),
            Some(2.5)
        );
    }

    #[test]
    fn demux_packet_cache_state_prunes_earliest_stream_queue_before_video_boundary() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 3 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(3_000_000_000),
        ));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        let range = state
            .ranges
            .values()
            .next()
            .expect("archived range remains");
        assert_eq!(state.archived_bytes(), 3 * 1024);
        assert_eq!(range.global_order.len(), 3);
        assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(2));
        assert_eq!(range.stream_queues.get(&1).map(VecDeque::len), Some(1));
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges[0],
            PlaybackCacheTimeRange {
                start: 1.0,
                end: 3.0,
            }
        );
        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(3)),
            Some(3.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_excludes_pruned_sparse_stream_from_seekable_range() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 3 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(2, StreamCacheKind::Subtitle);
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_packet(
            2,
            false,
            Some(500_000_000),
            Some(1_500_000_000),
        ));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.append_packet(cached_packet(
            2,
            false,
            Some(1_500_000_000),
            Some(3_000_000_000),
        ));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges[0],
            PlaybackCacheTimeRange {
                start: 1.5,
                end: 3.0,
            }
        );
        assert_eq!(state.seek_cached(1_250_000_000, PlaybackSessionId(3)), None);
        assert_eq!(
            state.seek_cached(1_750_000_000, PlaybackSessionId(4)),
            Some(3.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_prunes_anchor_prefix_without_dropping_parallel_stream_packets() {
        let mut config = cache_config_for_test();
        config.demuxer_max_back_bytes = 4 * 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_packet(
            1,
            false,
            Some(1_000_000_000),
            Some(3_000_000_000),
        ));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        let range = state
            .ranges
            .values()
            .next()
            .expect("archived range remains");
        assert_eq!(state.archived_bytes(), 4 * 1024);
        assert_eq!(range.global_order.len(), 4);
        assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(2));
        assert_eq!(range.stream_queues.get(&1).map(VecDeque::len), Some(2));
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges[0],
            PlaybackCacheTimeRange {
                start: 1.0,
                end: 3.0,
            }
        );
        assert_eq!(
            state.seek_cached(1_500_000_000, PlaybackSessionId(3)),
            Some(3.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_donates_unused_forward_budget_to_backbuffer() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1000.0;
        config.demuxer_max_bytes = 4096;
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = true;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        assert_eq!(state.forward_bytes(), 1024);
        assert_eq!(state.backward_bytes(), 3072);
        assert_eq!(state.ranges.len(), 2);
        assert_eq!(state.archived_bytes(), 3072);
        assert_eq!(
            state.seek_cached(2_500_000_000, PlaybackSessionId(3)),
            Some(3.0)
        );
    }

    #[test]
    fn demux_packet_cache_state_forward_limit_ignores_archived_backbuffer_bytes() {
        let mut config = cache_config_for_test();
        config.cache_secs = 1000.0;
        config.demuxer_readahead_secs = 1000.0;
        config.demuxer_max_bytes = 2048;
        config.demuxer_max_back_bytes = 4096;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));
        state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

        assert_eq!(state.cached_bytes, 4096);
        assert_eq!(state.forward_bytes(), 1024);
        assert_eq!(state.backward_bytes(), 3072);
        assert!(!state.should_pause_demux());
        assert!(!state.playback_cache_state(false).demux.idle);
    }

    #[test]
    fn demux_packet_cache_state_drops_backbuffer_when_seekable_cache_disabled() {
        let mut config = cache_config_for_test();
        config.seekable_cache = PlaybackSeekableCacheMode::Disabled;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        assert_eq!(state.ranges.len(), 1);
        assert!(state.read_range().global_order.is_empty());
        assert!(state.packets.is_empty());
        assert_eq!(state.cached_bytes, 0);
    }

    #[test]
    fn demux_packet_cache_state_preserves_seekable_backbuffer_when_forced_with_cache_disabled() {
        let mut config = cache_config_for_test();
        config.mode = PlaybackCacheMode::Disabled;
        config.seekable_cache = PlaybackSeekableCacheMode::Enabled;
        config.demuxer_max_back_bytes = 1024;
        config.demuxer_donate_buffer = false;
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            config,
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));

        state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

        assert_eq!(state.archived_bytes(), 1024);
        assert_eq!(
            state.seek_cached(500_000_000, PlaybackSessionId(3)),
            Some(1.0)
        );
        assert!(!state.cache_pause_enabled);
    }

    #[test]
    fn demux_packet_cache_state_indexes_packets_by_stream() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(1, false, Some(0), Some(500_000_000)));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));

        assert_eq!(state.read_range().global_order.len(), 3);
        assert_eq!(
            state.read_range().stream_queues.get(&0).map(VecDeque::len),
            Some(2)
        );
        assert_eq!(
            state.read_range().stream_queues.get(&1).map(VecDeque::len),
            Some(1)
        );
        assert_eq!(state.cached_timeline_range(), Some((0, 2_000_000_000)));
    }

    #[test]
    fn demux_packet_disk_cache_restores_packet_payload() {
        let props = AvPacket::new().expect("packet allocates");
        let packet =
            AvPacket::from_data_and_props(b"packet-payload", &props).expect("packet has data");
        let mut cached = CachedDemuxPacket::from_packet(&packet, 0, true, true, Some(0), Some(1))
            .expect("packet caches");
        let mut disk_cache = DemuxPacketDiskCache::new(1024, None, CacheUnlinkPolicy::WhenDone)
            .expect("disk cache creates");

        cached
            .spill_to_disk(&mut disk_cache)
            .expect("packet spills to disk");
        let restored = cached
            .packet_ref(Some(&disk_cache))
            .expect("packet restores from disk");

        assert_eq!(restored.data(), Some(&b"packet-payload"[..]));
    }

    #[test]
    fn demux_packet_disk_cache_unlinks_immediately_but_keeps_open_file_usable() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let mut disk_cache = DemuxPacketDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::Immediate,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();

        assert!(!path.exists());
        let props = AvPacket::new().expect("packet allocates");
        let offset = disk_cache.write_packet(b"payload").expect("payload writes");
        let restored = disk_cache
            .read_packet(offset, "payload".len(), &props)
            .expect("payload reads from unlinked file");

        assert_eq!(restored.data(), Some(&b"payload"[..]));
    }

    #[test]
    fn demux_packet_disk_cache_removes_file_when_done() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let path = {
            let disk_cache = DemuxPacketDiskCache::new(
                1024,
                Some(dir.path().to_path_buf()),
                CacheUnlinkPolicy::WhenDone,
            )
            .expect("disk cache creates");
            let path = disk_cache.path.clone();
            assert!(path.exists());
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn demux_packet_disk_cache_can_leave_file_for_inspection() {
        let dir = tempfile::tempdir().expect("temp dir creates");
        let path = {
            let disk_cache = DemuxPacketDiskCache::new(
                1024,
                Some(dir.path().to_path_buf()),
                CacheUnlinkPolicy::Never,
            )
            .expect("disk cache creates");
            let path = disk_cache.path.clone();
            assert!(path.exists());
            path
        };

        assert!(path.exists());
        std::fs::remove_file(path).expect("leftover cache file removes");
    }

    #[test]
    fn demux_packet_cache_readahead_is_capped_below_cache_secs() {
        // With the cache active, effective_readahead_secs() inflates to cache_secs
        // (up to an hour). The packet read-ahead must be capped so the producer pauses
        // and releases the cache mutex to the pump instead of hot-looping toward it.
        let cached = PlaybackCacheConfig {
            demuxer_readahead_secs: 1.0,
            cache_secs: 3600.0,
            ..PlaybackCacheConfig::default()
        };
        assert_eq!(
            demux_packet_cache_readahead_nsecs(&cached, true),
            duration_nsecs(DEMUX_PACKET_CACHE_MAX_READAHEAD)
        );

        // A configured read-ahead below the cap is respected verbatim.
        let small = PlaybackCacheConfig {
            demuxer_readahead_secs: 2.0,
            ..PlaybackCacheConfig::default()
        };
        assert_eq!(
            demux_packet_cache_readahead_nsecs(&small, false),
            seconds_to_nsecs(2.0)
        );
    }
}
