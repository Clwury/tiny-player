use super::*;
use std::collections::{BTreeMap, HashMap};

pub(super) struct DemuxPacketCache {
    shared: Arc<DemuxPacketCacheShared>,
    handle: Option<JoinHandle<()>>,
}

pub(super) struct DemuxPacketCacheInput {
    pub(super) input: FormatContext,
    pub(super) video_stream: StreamInfo,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) duration_seconds: Option<f64>,
    pub(super) start_position_seconds: f64,
    pub(super) session_id: PlaybackSessionId,
}

pub(super) enum DemuxReadResult {
    Packet(AvPacket),
    Eof,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DemuxSeekResult {
    Cached,
    Requested,
}

struct DemuxPacketCacheShared {
    state: Mutex<DemuxPacketCacheState>,
    ready: Condvar,
    control: Arc<FfmpegControl>,
    event_tx: Sender<BackendEvent>,
}

struct DemuxPacketCacheState {
    packets: HashMap<u64, CachedDemuxPacket>,
    global_order: VecDeque<u64>,
    stream_queues: BTreeMap<c_int, VecDeque<u64>>,
    disk_cache: Option<DemuxPacketDiskCache>,
    disk_cache_writable: bool,
    read_index: usize,
    next_packet_id: u64,
    timeline_anchor_stream_index: c_int,
    cached_seek_preroll_nsecs: u64,
    cached_bytes: usize,
    reader_nsecs: u64,
    session_id: PlaybackSessionId,
    seek_request: Option<DemuxSeekRequest>,
    generation: u64,
    eof: bool,
    error: Option<String>,
    shutdown: bool,
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
    video_frame_duration_nsecs: u64,
    current_start_position_nsecs: u64,
    video_clock: TimestampMapper,
    audio_clock: TimestampMapper,
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
            duration_seconds,
            start_position_seconds,
            session_id,
        } = cache_input;
        let start_position_seconds = start_position_seconds.max(0.0);
        let start_position_nsecs = seconds_to_nsecs(start_position_seconds);
        let shared = Arc::new(DemuxPacketCacheShared {
            state: Mutex::new(DemuxPacketCacheState::new(
                start_position_nsecs,
                video_stream.index,
                video_stream.codec_id,
                session_id,
            )),
            ready: Condvar::new(),
            control,
            event_tx,
        });
        let thread_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-demux-cache".to_string())
            .spawn(move || {
                run_demux_packet_cache(
                    input,
                    video_stream,
                    audio_stream,
                    duration_seconds,
                    start_position_seconds,
                    session_id,
                    thread_shared,
                )
            })
            .map_err(|error| format!("创建 FFmpeg demux 缓存线程失败：{error}"))?;

        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    pub(super) fn read_packet(&self) -> DemuxReadResult {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let mut logged_wait = false;
        let mut wait_started_at = None;
        let mut next_stall_log_at = None;
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return DemuxReadResult::Interrupted;
            }
            if let Some(error) = guard.error.clone() {
                return DemuxReadResult::Error(error);
            }
            if guard.read_index < guard.global_order.len() {
                let Some(packet_id) = guard.global_order.get(guard.read_index).copied() else {
                    return DemuxReadResult::Error(
                        "FFmpeg demux packet cache read cursor invalid".to_string(),
                    );
                };
                let packet = match guard.packet_ref(packet_id) {
                    Ok(packet) => packet,
                    Err(error) => return DemuxReadResult::Error(error),
                };
                if let Some(end_nsecs) = guard.packet_end_nsecs(packet_id) {
                    guard.reader_nsecs = end_nsecs;
                }
                guard.read_index += 1;
                guard.trim_to_limit();
                self.shared.ready.notify_all();
                return DemuxReadResult::Packet(packet);
            }
            if guard.eof {
                return DemuxReadResult::Eof;
            }
            let now = Instant::now();
            let wait_started = *wait_started_at.get_or_insert(now);
            if !logged_wait {
                tracing::trace!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    reader_nsecs = guard.reader_nsecs,
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "waiting for FFmpeg demux packet cache data"
                );
                logged_wait = true;
                next_stall_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
            } else if next_stall_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    waited_ms = now.saturating_duration_since(wait_started).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    reader_nsecs = guard.reader_nsecs,
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "still waiting for FFmpeg demux packet cache data"
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

    pub(super) fn seek(
        &self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        let (result, buffered_until) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = None;
            guard.eof = false;
            if let Some(buffered_until) = guard.seek_cached(target_nsecs, session_id) {
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    target_nsecs,
                    seek_generation,
                    buffered_until,
                    read_index = guard.read_index,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek hit"
                );
                (DemuxSeekResult::Cached, Some(buffered_until))
            } else {
                guard.request_seek(position_seconds, session_id, seek_generation, target_nsecs);
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    target_nsecs,
                    seek_generation,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek miss; requested low-level seek"
                );
                (DemuxSeekResult::Requested, Some(position_seconds))
            }
        };
        self.shared.ready.notify_all();
        if let Some(buffered_until) = buffered_until {
            let _ = self.shared.event_tx.send(BackendEvent::new(
                session_id,
                BackendEventKind::BufferedChanged(Some(buffered_until)),
            ));
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
    ) -> Self {
        let disk_cache = DemuxPacketDiskCache::from_env();
        let disk_cache_writable = disk_cache.is_some();
        Self {
            packets: HashMap::new(),
            global_order: VecDeque::new(),
            stream_queues: BTreeMap::new(),
            disk_cache,
            disk_cache_writable,
            read_index: 0,
            next_packet_id: 0,
            timeline_anchor_stream_index,
            cached_seek_preroll_nsecs: video_seek_preroll_nsecs(timeline_anchor_codec_id),
            cached_bytes: 0,
            reader_nsecs,
            session_id,
            seek_request: None,
            generation: 0,
            eof: false,
            error: None,
            shutdown: false,
        }
    }

    fn append_packet(&mut self, mut packet: CachedDemuxPacket) {
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
        self.cached_bytes = self.cached_bytes.saturating_add(packet.byte_len);
        self.global_order.push_back(packet_id);
        self.stream_queues
            .entry(stream_index)
            .or_default()
            .push_back(packet_id);
        self.packets.insert(packet_id, packet);
        self.trim_to_limit();
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

    fn packet_start_nsecs_at_global_index(&self, index: usize) -> Option<u64> {
        let packet_id = *self.global_order.get(index)?;
        self.packets
            .get(&packet_id)
            .and_then(|packet| packet.start_nsecs)
    }

    fn seek_cached(&mut self, target_nsecs: u64, session_id: PlaybackSessionId) -> Option<f64> {
        let (first_cached_nsecs, buffered_until_nsecs) = self.cached_timeline_range()?;
        if target_nsecs < first_cached_nsecs || target_nsecs > buffered_until_nsecs {
            return None;
        }

        let mut covering_anchor_index = None;
        let mut keyframe_anchor_index = None;
        let mut preroll_keyframe_anchor_index = None;
        for packet_id in self.timeline_anchor_packet_ids() {
            let packet = self.packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            let packet_index = self
                .global_order
                .iter()
                .position(|candidate| *candidate == packet_id);
            if packet.recovery_point && start_nsecs <= target_nsecs {
                if self.cached_seek_preroll_nsecs > 0 {
                    preroll_keyframe_anchor_index = keyframe_anchor_index;
                }
                keyframe_anchor_index = packet_index;
            }
            if covering_anchor_index.is_none()
                && start_nsecs <= target_nsecs
                && target_nsecs <= end_nsecs
            {
                covering_anchor_index = packet_index;
            }
        }

        let _covering_anchor_index = covering_anchor_index?;
        let read_index = if self.cached_seek_preroll_nsecs > 0 {
            let read_index = preroll_keyframe_anchor_index?;
            let required_preroll_start =
                target_nsecs.saturating_sub(self.cached_seek_preroll_nsecs);
            let read_start_nsecs = self.packet_start_nsecs_at_global_index(read_index)?;
            if read_start_nsecs > required_preroll_start {
                return None;
            }
            read_index
        } else {
            keyframe_anchor_index?
        };
        self.read_index = read_index;
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.eof = false;
        self.generation = self.generation.saturating_add(1);
        Some(nsecs_to_seconds(buffered_until_nsecs))
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
            global_packet_count = self.global_order.len(),
            read_index = self.read_index,
            previous_generation = self.generation,
            "clearing FFmpeg demux packet cache for low-level seek"
        );
        self.packets.clear();
        self.global_order.clear();
        self.stream_queues.clear();
        if let Some(disk_cache) = self.disk_cache.as_mut() {
            match disk_cache.clear() {
                Ok(()) => self.disk_cache_writable = true,
                Err(error) => {
                    tracing::warn!(%error, "disabling FFmpeg demux packet disk cache writes after clear failure");
                    self.disk_cache_writable = false;
                }
            }
        }
        self.read_index = 0;
        self.cached_bytes = 0;
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.generation = self.generation.saturating_add(1);
    }

    fn take_seek_request(&mut self) -> Option<DemuxSeekRequest> {
        self.seek_request.take()
    }

    fn should_pause_demux(&self) -> bool {
        self.cached_bytes >= DEMUX_PACKET_CACHE_MEMORY_BYTES
            || self.cached_until_nsecs().is_some_and(|cached_until| {
                cached_until.saturating_sub(self.reader_nsecs) >= DEMUX_PACKET_CACHE_READAHEAD_NSECS
            })
    }

    fn cached_until_nsecs(&self) -> Option<u64> {
        self.cached_timeline_range()
            .map(|(_, buffered_until_nsecs)| buffered_until_nsecs)
    }

    fn trim_to_limit(&mut self) {
        while self.cached_bytes >= DEMUX_PACKET_CACHE_MEMORY_BYTES && self.read_index > 0 {
            let Some(packet_id) = self.global_order.pop_front() else {
                break;
            };
            let Some(packet) = self.packets.remove(&packet_id) else {
                self.read_index -= 1;
                continue;
            };
            self.remove_stream_packet(packet.stream_index, packet_id);
            self.cached_bytes = self.cached_bytes.saturating_sub(packet.byte_len);
            self.read_index -= 1;
        }
    }

    fn remove_stream_packet(&mut self, stream_index: c_int, packet_id: u64) {
        let Some(queue) = self.stream_queues.get_mut(&stream_index) else {
            return;
        };
        if queue.front().copied() == Some(packet_id) {
            queue.pop_front();
        } else {
            queue.retain(|candidate| *candidate != packet_id);
        }
        if queue.is_empty() {
            self.stream_queues.remove(&stream_index);
        }
    }

    fn timeline_anchor_packet_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.stream_queues
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
    fn from_env() -> Option<Self> {
        if !demux_packet_disk_cache_enabled() {
            return None;
        }
        let max_bytes = env::var("TINY_DEMUX_PACKET_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEMUX_PACKET_CACHE_DEFAULT_DISK_BYTES);
        Self::new(max_bytes)
    }

    fn new(max_bytes: u64) -> Option<Self> {
        let dir = env::var("TINY_DEMUX_PACKET_CACHE_DIR")
            .ok()
            .map(PathBuf::from)
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
        Some(Self {
            file,
            path,
            next_offset: 0,
            max_bytes,
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

    fn clear(&mut self) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.next_offset = 0;
        Ok(())
    }
}

impl Drop for DemuxPacketDiskCache {
    fn drop(&mut self) {
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
        start_position_seconds: f64,
        session_id: PlaybackSessionId,
    ) -> Self {
        let video_frame_duration_nsecs = video_stream
            .frame_duration_nsecs
            .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
        let current_start_position_nsecs = seconds_to_nsecs(start_position_seconds.max(0.0));
        let buffered_reporter = BufferedReporter::new(false);
        Self {
            video_stream,
            audio_stream,
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
        self.buffered_reporter = BufferedReporter::new(false);
        self.buffered_reporter
            .reset_to(position_seconds, session_id, event_tx);
    }

    fn cache_packet(
        &mut self,
        packet: &AvPacket,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<CachedDemuxPacket, String> {
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
            return (None, None);
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
    mut input: FormatContext,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    duration_seconds: Option<f64>,
    start_position_seconds: f64,
    session_id: PlaybackSessionId,
    shared: Arc<DemuxPacketCacheShared>,
) {
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        audio_stream,
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
        let read = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
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
                Ok(cached) => shared.append_packet(cached),
                Err(error) => shared.set_error(error),
            }
            packet.unref();
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
            if guard.eof || guard.error.is_some() {
                let (next_guard, _) = self
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                guard = next_guard;
                continue;
            }
            if !guard.should_pause_demux() {
                return None;
            }
            let now = Instant::now();
            let pause_started = *prefetch_pause_started_at.get_or_insert(now);
            if !logged_prefetch_pause {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    reader_nsecs = guard.reader_nsecs,
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
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
                    packet_count = guard.global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    reader_nsecs = guard.reader_nsecs,
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
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
        guard.append_packet(packet);
        self.ready.notify_all();
    }

    fn mark_eof(&self) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.eof = true;
        self.ready.notify_all();
    }

    fn set_error(&self, error: String) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.error = Some(error);
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
        CachedDemuxPacket {
            payload: CachedDemuxPacketPayload::Memory(AvPacket::new().expect("packet allocates")),
            stream_index,
            timeline_anchor,
            recovery_point: keyframe,
            start_nsecs,
            end_nsecs,
            byte_len: 1024,
        }
    }

    fn shared_for_test(control: Arc<FfmpegControl>) -> DemuxPacketCacheShared {
        let (event_tx, _) = mpsc::channel();
        DemuxPacketCacheShared {
            state: Mutex::new(DemuxPacketCacheState::new(
                0,
                0,
                ffi::AVCodecID::AV_CODEC_ID_MPEG4,
                PlaybackSessionId(1),
            )),
            ready: Condvar::new(),
            control,
            event_tx,
        }
    }

    #[test]
    fn demux_packet_cache_state_trims_consumed_packet_at_memory_limit() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
        );
        let mut packet = cached_packet(0, true, Some(0), Some(1_000_000_000));
        packet.byte_len = DEMUX_PACKET_CACHE_MEMORY_BYTES;
        state.append_packet(packet);

        assert_eq!(state.cached_bytes, DEMUX_PACKET_CACHE_MEMORY_BYTES);
        assert!(state.should_pause_demux());

        state.read_index = 1;
        state.reader_nsecs = 1_000_000_000;
        state.trim_to_limit();

        assert_eq!(state.cached_bytes, 0);
        assert_eq!(state.read_index, 0);
        assert_eq!(state.global_order.len(), 0);
        assert!(!state.should_pause_demux());
    }

    #[test]
    fn demux_packet_cache_state_seeks_inside_cached_timeline_range() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
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
    }

    #[test]
    fn demux_packet_cache_state_cached_seek_invalidates_inflight_demux_read() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
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
    fn demux_packet_cache_state_seeks_from_nearest_previous_keyframe() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
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
    }

    #[test]
    fn demux_packet_cache_state_rejects_hevc_cached_seek_with_short_preroll_window() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
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
    fn demux_packet_cache_state_rejects_timeline_gaps() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
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
        );
        state.append_packet(cached_anchor(0, 1_000_000_000));

        assert_eq!(state.seek_cached(3_000_000_000, PlaybackSessionId(7)), None);
        state.request_seek(3.0, PlaybackSessionId(7), 1, 3_000_000_000);

        assert!(state.packets.is_empty());
        assert!(state.global_order.is_empty());
        assert!(state.stream_queues.is_empty());
        assert_eq!(state.read_index, 0);
        assert_eq!(state.cached_bytes, 0);
        assert_eq!(state.reader_nsecs, 3_000_000_000);
        assert_eq!(state.session_id, PlaybackSessionId(7));
        assert_eq!(
            state.seek_request.map(|request| request.session_id),
            Some(PlaybackSessionId(7))
        );
    }

    #[test]
    fn demux_packet_cache_state_indexes_packets_by_stream() {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
            PlaybackSessionId(1),
        );
        state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
        state.append_packet(cached_packet(1, false, Some(0), Some(500_000_000)));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));

        assert_eq!(state.global_order.len(), 3);
        assert_eq!(state.stream_queues.get(&0).map(VecDeque::len), Some(2));
        assert_eq!(state.stream_queues.get(&1).map(VecDeque::len), Some(1));
        assert_eq!(state.cached_timeline_range(), Some((0, 2_000_000_000)));
    }

    #[test]
    fn demux_packet_disk_cache_restores_packet_payload() {
        let props = AvPacket::new().expect("packet allocates");
        let packet =
            AvPacket::from_data_and_props(b"packet-payload", &props).expect("packet has data");
        let mut cached = CachedDemuxPacket::from_packet(&packet, 0, true, true, Some(0), Some(1))
            .expect("packet caches");
        let mut disk_cache = DemuxPacketDiskCache::new(1024).expect("disk cache creates");

        cached
            .spill_to_disk(&mut disk_cache)
            .expect("packet spills to disk");
        let restored = cached
            .packet_ref(Some(&disk_cache))
            .expect("packet restores from disk");

        assert_eq!(restored.data(), Some(&b"packet-payload"[..]));
    }
}
