#[path = "cache/archived_seek.rs"]
mod archived_seek;
#[path = "cache/audio_selection.rs"]
mod audio_selection;
#[path = "cache/config.rs"]
mod config;
#[path = "cache/forward_cache.rs"]
mod forward_cache;
#[path = "cache/h264_ranges.rs"]
mod h264_ranges;
#[path = "cache/hevc_ranges.rs"]
mod hevc_ranges;
#[path = "cache/pause_read.rs"]
mod pause_read;
#[path = "cache/pruning_storage.rs"]
mod pruning_storage;
#[path = "cache/range_handoff.rs"]
mod range_handoff;
#[path = "cache/range_reporting.rs"]
mod range_reporting;
#[path = "cache/range_resume.rs"]
mod range_resume;
#[path = "cache/reporting_reader.rs"]
mod reporting_reader;
#[path = "cache/seek.rs"]
mod seek;
#[path = "cache/state.rs"]
mod state;
#[path = "cache/storage_worker.rs"]
mod storage_worker;
#[path = "cache/stream_alignment.rs"]
mod stream_alignment;
#[path = "cache/subtitle_seek.rs"]
mod subtitle_seek;
#[path = "cache/subtitle_selection.rs"]
mod subtitle_selection;
#[path = "cache/timeline.rs"]
mod timeline;
#[path = "cache/trim.rs"]
mod trim;

use std::{
    collections::VecDeque,
    os::raw::c_int,
    ptr,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::backend::ffmpeg::AudioOutputLifecycle;
use crate::player::{
    backend::{
        BackendEvent, BackendEventKind, CacheUnlinkPolicy, DemuxCacheState, PlaybackCacheConfig,
        PlaybackCacheMode, PlaybackCacheState, PlaybackCacheTimeRange, PlaybackSeekMode,
        PlaybackSeekableCacheMode, StreamCacheKind,
    },
    render_host::PlaybackSessionId,
};

use super::super::DEMUX_PACKET_CACHE_MEMORY_BYTES;
use super::{
    AvPacket, AvPacketStorageKind, CachedDemuxPacket, CachedDemuxPacketPayload,
    CachedDemuxPacketRecovery, CachedSeekMiss, CachedSeekMissReason,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DEMUX_PACKET_APPEND_TRIM_INTERVAL,
    DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT, DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS,
    DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL, DEMUX_PACKET_READ_TRIM_INTERVAL,
    DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL, DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT,
    DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP, DEMUX_STREAM_PACKET_QUEUE_LIMIT, DemuxCachedRange,
    DemuxCachedSeekInfo, DemuxPacketCache, DemuxPacketCacheMonitorSnapshot,
    DemuxPacketCacheReadTiming, DemuxPacketCacheShared, DemuxPacketCacheState,
    DemuxPacketDiskCache, DemuxPacketTimeline, DemuxReadResult, DemuxSeekRequest, DemuxSeekResult,
    DemuxSelectedStreams, FfmpegControl, PacketId, StreamInfo, VideoRecoveryPointKind,
    demux_cache_blocked_on, demux_packet_cache_hysteresis_nsecs,
    demux_packet_cache_readahead_nsecs, duration_nsecs, seconds_to_nsecs,
};

fn cached_anchor(start_nsecs: u64, end_nsecs: u64) -> CachedDemuxPacket {
    cached_key_packet(0, true, Some(start_nsecs), Some(end_nsecs))
}

fn cached_seek_closer(at_nsecs: u64) -> CachedDemuxPacket {
    let mut packet = cached_anchor(at_nsecs, at_nsecs);
    packet.byte_len = 0;
    packet
}

fn close_seek_range(state: &mut DemuxPacketCacheState, at_nsecs: u64) {
    state.append_packet(cached_seek_closer(at_nsecs));
}

fn finish_bounded_read_trim(state: &mut DemuxPacketCacheState) {
    let maximum_steps = state.packets.len().saturating_add(1);
    for _ in 0..maximum_steps {
        if !state.backbuffer_pressure() {
            return;
        }
        let outcome = state.trim_to_limit_for_read_with_outcome();
        assert!(
            outcome.performed,
            "bounded read trim must make progress while backbuffer pressure remains"
        );
    }
    panic!("bounded read trim did not converge within one step per cached packet");
}

fn set_reader_head_for_stream_time(
    state: &mut DemuxPacketCacheState,
    stream_index: c_int,
    at_or_after_nsecs: u64,
) -> PacketId {
    let packet_id = state
        .read_range()
        .stream_queues
        .get(&stream_index)
        .and_then(|queue| {
            queue.iter().copied().find(|packet_id| {
                state
                    .packets
                    .get(packet_id)
                    .and_then(|packet| packet.start_nsecs)
                    .is_some_and(|start_nsecs| start_nsecs >= at_or_after_nsecs)
            })
        })
        .expect("reader head packet exists for stream time");
    state.set_reader_head_for_current_generation(stream_index, packet_id);
    state.refresh_reader_tracking();
    packet_id
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

fn cached_runtime_packet(
    stream_index: c_int,
    timeline_anchor: bool,
    start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
) -> CachedDemuxPacket {
    cached_runtime_packet_with_keyframe(
        stream_index,
        timeline_anchor,
        false,
        start_nsecs,
        end_nsecs,
        start_nsecs,
    )
}

fn cached_runtime_key_packet(
    stream_index: c_int,
    timeline_anchor: bool,
    start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
) -> CachedDemuxPacket {
    cached_runtime_packet_with_keyframe(
        stream_index,
        timeline_anchor,
        true,
        start_nsecs,
        end_nsecs,
        start_nsecs,
    )
}

fn cached_runtime_packet_with_keyframe(
    stream_index: c_int,
    timeline_anchor: bool,
    keyframe: bool,
    start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
    seek_timestamp_nsecs: Option<u64>,
) -> CachedDemuxPacket {
    let mut packet = cached_packet_with_keyframe(
        stream_index,
        timeline_anchor,
        keyframe,
        start_nsecs,
        end_nsecs,
    );
    packet.seek_timestamp_nsecs = seek_timestamp_nsecs;
    packet.raw_pts = seek_timestamp_nsecs.and_then(|timestamp| i64::try_from(timestamp).ok());
    packet
}

fn cached_video_recovery_packet(
    kind: VideoRecoveryPointKind,
    safe_seek_point: bool,
    start_nsecs: u64,
    end_nsecs: u64,
) -> CachedDemuxPacket {
    let mut packet = cached_key_packet(0, true, Some(start_nsecs), Some(end_nsecs));
    packet.recovery_kind = kind;
    packet.safe_seek_point = safe_seek_point;
    packet
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
        (*packet.as_mut_ptr()).flags = if keyframe { ffi::AV_PKT_FLAG_KEY } else { 0 };
    }
    CachedDemuxPacket {
        payload: CachedDemuxPacketPayload::Memory(Arc::new(Mutex::new(packet))),
        stream_index,
        timeline_anchor,
        demux_keyframe: keyframe,
        recovery_point: keyframe,
        recovery_kind: if keyframe {
            VideoRecoveryPointKind::Keyframe
        } else {
            VideoRecoveryPointKind::None
        },
        safe_seek_point: keyframe,
        seek_timestamp_nsecs: start_nsecs,
        raw_pts: None,
        raw_dts: None,
        raw_pos: start_nsecs.and_then(|value| i64::try_from(value).ok()),
        start_nsecs,
        end_nsecs,
        byte_len: 1024,
        properties_byte_len: 0,
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

fn demux_packet_with_data_for_stream(stream_index: c_int, data: &[u8]) -> AvPacket {
    let mut props = AvPacket::new().expect("packet props allocate");
    unsafe {
        (*props.as_mut_ptr()).stream_index = stream_index;
    }
    AvPacket::from_data_and_props(data, &props).expect("packet data allocates")
}

fn shared_for_test(control: Arc<FfmpegControl>) -> DemuxPacketCacheShared {
    let (shared, _) = shared_with_config_for_test(control, PlaybackCacheConfig::default());
    shared
}

fn shared_with_config_for_test(
    control: Arc<FfmpegControl>,
    cache_config: PlaybackCacheConfig,
) -> (DemuxPacketCacheShared, Receiver<BackendEvent>) {
    shared_with_codec_and_config_for_test(control, ffi::AVCodecID::AV_CODEC_ID_MPEG4, cache_config)
}

fn shared_with_codec_and_config_for_test(
    control: Arc<FfmpegControl>,
    codec_id: ffi::AVCodecID,
    cache_config: PlaybackCacheConfig,
) -> (DemuxPacketCacheShared, Receiver<BackendEvent>) {
    let (event_tx, event_rx) = mpsc::channel();
    let state = DemuxPacketCacheState::new(0, 0, codec_id, PlaybackSessionId(1), cache_config);
    let monitor_snapshot = DemuxPacketCacheMonitorSnapshot::from_state(&state);
    let shared = DemuxPacketCacheShared {
        state: Mutex::new(state),
        monitor_snapshot: Mutex::new(monitor_snapshot),
        ready: Condvar::new(),
        control,
        event_tx,
        clock_start: Instant::now(),
        demux_read_started_nanos: AtomicU64::new(0),
        last_would_block_diag_nanos: AtomicU64::new(0),
        last_recovery_demand_diag_nanos: AtomicU64::new(0),
        consumer_waiting_readers: AtomicUsize::new(0),
        consumer_lock_pressure_until_nanos: AtomicU64::new(0),
        playback_recovery_critical: AtomicBool::new(false),
        playback_recovery_demand: AtomicU8::new(0),
        disk_worker_started: AtomicBool::new(false),
    };
    (shared, event_rx)
}

fn cache_config_for_test() -> PlaybackCacheConfig {
    PlaybackCacheConfig::default()
}

fn append_hevc_prefix_trim_regression_packets(state: &mut DemuxPacketCacheState) {
    state.append_packet(cached_key_packet(0, true, Some(0), Some(40_000_000)));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(100_000_000),
        Some(140_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(300_000_000),
        Some(340_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(800_000_000),
        Some(1_200_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_500_000_000),
        Some(1_540_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(2_040_000_000),
    ));
}

fn assert_every_advertised_range_sample_cached_seeks(mut state: DemuxPacketCacheState) {
    let ranges = state.playback_cache_state(false).demux.seekable_ranges;
    assert!(
        !ranges.is_empty(),
        "test fixture must advertise a cached range"
    );
    let mut seek_generation = 10u64;
    for range in ranges {
        let start_nsecs = seconds_to_nsecs(range.start);
        let end_nsecs = seconds_to_nsecs(range.end);
        let targets = [
            start_nsecs,
            start_nsecs.saturating_add(end_nsecs.saturating_sub(start_nsecs) / 2),
            end_nsecs.saturating_sub(1),
        ];
        for target_nsecs in targets {
            let hit = state.seek_cached_with_generation_hit(
                target_nsecs,
                PlaybackSeekMode::Precise,
                PlaybackSessionId(seek_generation),
                seek_generation,
            );
            assert!(
                hit.is_some(),
                "advertised target {target_nsecs}ns in {range:?} must cached-seek"
            );
            seek_generation = seek_generation.saturating_add(1);
        }
    }
}
