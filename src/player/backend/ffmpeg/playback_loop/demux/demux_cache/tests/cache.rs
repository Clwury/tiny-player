use std::{
    collections::VecDeque,
    os::raw::c_int,
    ptr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::AtomicU64,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

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
    AvPacket, CachedDemuxPacket, CachedDemuxPacketPayload, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DEMUX_PACKET_APPEND_TRIM_INTERVAL, DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS,
    DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL, DEMUX_PACKET_READ_TRIM_INTERVAL,
    DEMUX_STREAM_PACKET_QUEUE_LIMIT, DemuxPacketCache, DemuxPacketCacheMonitorSnapshot,
    DemuxPacketCacheReadTiming, DemuxPacketCacheShared, DemuxPacketCacheState,
    DemuxPacketDiskCache, DemuxPacketTimeline, DemuxReadResult, DemuxSeekRequest, DemuxSeekResult,
    DemuxSelectedStreams, FfmpegControl, StreamInfo, demux_cache_blocked_on,
    demux_packet_cache_hysteresis_nsecs, demux_packet_cache_readahead_nsecs, duration_nsecs,
    seconds_to_nsecs,
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
        payload: CachedDemuxPacketPayload::Memory(Arc::new(Mutex::new(packet))),
        stream_index,
        timeline_anchor,
        recovery_point: keyframe,
        safe_seek_point: keyframe,
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
    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config,
    );
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
fn demux_packet_timeline_switches_selected_audio_stream() {
    let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
    let old_audio_stream = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_EAC3);
    let new_audio_stream = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        Some(old_audio_stream),
        None,
        0.0,
        PlaybackSessionId(1),
    );
    let (event_tx, _event_rx) = mpsc::channel();

    timeline.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(new_audio_stream),
        subtitle_stream: None,
    });

    let old_packet = demux_packet_for_stream(2);
    let new_packet = demux_packet_for_stream(1);
    assert!(
        timeline
            .cache_packet(&old_packet, &event_tx)
            .expect("old stream packet can be dropped")
            .is_none()
    );
    assert!(
        timeline
            .cache_packet(&new_packet, &event_tx)
            .expect("new stream packet can be cached")
            .is_some()
    );
    assert!(!timeline.should_cache_stream(2));
    assert!(timeline.should_cache_stream(1));
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
fn demux_packet_cache_state_caps_readahead_with_configured_packet_limit() {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 120.0,
        demuxer_readahead_secs: 2.0,
        demuxer_packet_max_readahead_secs: 30.0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 30_000_000_000);
}

#[test]
fn demux_packet_cache_state_can_disable_packet_readahead_time_cap() {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 120.0,
        demuxer_readahead_secs: 2.0,
        demuxer_packet_max_readahead_secs: 0.0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 120_000_000_000);
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
        demuxer_packet_max_readahead_secs: 1.5,
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
    assert_eq!(state.readahead_nsecs, 1_500_000_000);
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
fn demux_packet_cache_append_trims_backbuffer_incrementally() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    for index in 0..8 {
        let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
        state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
    }
    state.set_read_index_for_test(6);
    state.reader_nsecs = 6_000_000_000;

    assert_eq!(state.backward_bytes(), 6 * 1024);

    let outcome = state.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));

    assert!(!outcome.force_cache_state_report);
    assert_eq!(state.backward_bytes(), 6 * 1024);
    assert!(state.backward_bytes() > state.effective_backbuffer_limit());

    let mut trimmed = false;
    for index in 9..(9 + DEMUX_PACKET_APPEND_TRIM_INTERVAL as u64) {
        let start_nsecs = index * 1_000_000_000;
        let outcome = state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        trimmed |= outcome.timing.trim > Duration::ZERO;
        if trimmed {
            break;
        }
    }
    assert!(trimmed);
    assert!(state.backward_bytes() < 6 * 1024);

    assert!(state.trim_to_limit());
    assert!(state.backward_bytes() <= state.effective_backbuffer_limit());
}

#[test]
fn demux_packet_cache_read_defers_backbuffer_trim_off_hot_path() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    for index in 0..8 {
        let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
        state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
    }
    state.set_read_index_for_test(6);
    state.reader_nsecs = 6_000_000_000;

    assert_eq!(state.backward_bytes(), 6 * 1024);

    let mut timing = DemuxPacketCacheReadTiming::default();
    let packet = state
        .take_packet_round_robin(&[0], &mut timing)
        .expect("read packet")
        .expect("packet exists");

    assert_eq!(packet.stream_offset, 0);
    assert_eq!(state.read_range().global_order.len(), 8);
    assert!(state.packets.contains_key(&0));
    assert_eq!(state.reader_heads.get(&0), Some(&7));
    assert_eq!(state.reader_head_positions.get(&0), Some(&7));
    assert_eq!(state.backward_bytes(), 7 * 1024);
    assert!(state.backward_bytes() > state.effective_backbuffer_limit());

    assert!(state.trim_to_limit());
    assert!(state.backward_bytes() <= state.effective_backbuffer_limit());
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
fn demux_packet_cache_read_advances_reader_tracking_incrementally() {
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

    let mut timing = DemuxPacketCacheReadTiming::default();
    let packet = state
        .take_packet_round_robin(&[0], &mut timing)
        .expect("read packet")
        .expect("packet exists");

    assert_eq!(packet.stream_offset, 0);
    assert_eq!(state.reader_heads.get(&0), Some(&1));
    assert_eq!(state.reader_head_positions.get(&0), Some(&1));
    assert_eq!(state.read_index, 1);
    assert!(state.consumed_packet_ids.contains(&0));
    assert_eq!(state.reader_forward_bytes(), 2 * 1024);
    assert_eq!(state.forward_bytes(), 2 * 1024);
    assert_eq!(timing.refresh_reader_tracking, Duration::ZERO);
}

#[test]
fn demux_packet_cache_append_skips_heavy_maintenance_after_hysteresis_active() {
    let mut config = cache_config_for_test();
    config.mode = PlaybackCacheMode::Enabled;
    config.cache_secs = 3.0;
    config.demuxer_readahead_secs = 2.0;
    config.demuxer_hysteresis_secs = 0.5;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    let readahead_nsecs = state.readahead_nsecs;

    let crossing = state.append_packet(cached_anchor(0, readahead_nsecs));
    assert!(state.hysteresis_active);
    assert!(crossing.timing.refresh_readahead_hysteresis > Duration::ZERO);

    let after_active = state.append_packet(cached_anchor(
        readahead_nsecs,
        readahead_nsecs.saturating_add(1_000_000_000),
    ));

    assert!(state.hysteresis_active);
    assert_eq!(after_active.timing.trim, Duration::ZERO);
    assert_eq!(
        after_active.timing.refresh_readahead_hysteresis,
        Duration::ZERO
    );
    assert_eq!(after_active.timing.should_pause_demux, Duration::ZERO);
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
fn demux_packet_cache_state_counts_blocked_overlap_in_raw_input_rate() {
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

    assert!(outcome.appended);
    assert_eq!(state.cached_bytes, 4096);
    assert_eq!(state.next_packet_id_for_stream(0), None);
    assert_eq!(state.forward_bytes(), 0);
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
        BackendEventKind::BufferedChanged(buffered_until) => buffered_until.to_owned(),
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
    shared.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    let cache_end = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => state.demux.cache_end,
        _ => None,
    });
    let buffered_until = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::BufferedChanged(buffered_until) => buffered_until.to_owned(),
        _ => None,
    });

    assert_eq!(cache_end, Some(2.0));
    assert_eq!(buffered_until, cache_end);
}

#[test]
fn demux_packet_cache_coalesces_nonforced_append_cache_state_until_report_due() {
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

    shared.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
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
    shared.append_packet(cached_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    assert!(
        event_rx
            .try_iter()
            .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
    );
}

#[test]
fn demux_packet_cache_append_report_due_refreshes_seekable_ranges() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1.0;
    config.demuxer_readahead_secs = 1.0;
    let (shared, event_rx) = shared_with_config_for_test(control, config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.set_stream_kind(1, StreamCacheKind::Audio);
        let stale_state = PlaybackCacheState {
            demux: DemuxCacheState {
                seekable_ranges: Vec::new(),
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_cache_state(&stale_state);
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_anchor(0, 2_000_000_000));
    assert!(
        !event_rx
            .try_iter()
            .any(|event| matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );

    shared.append_packet(cached_packet(1, false, Some(0), Some(2_000_000_000)));
    let _ = event_rx.try_iter().collect::<Vec<_>>();
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    shared.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    let cache_state = events.iter().rev().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    let cache_state = cache_state.expect("forced append emits cache state");

    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );
}

#[test]
fn demux_packet_cache_append_percent_change_does_not_force_cache_state() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    config.demuxer_readahead_secs = 20.0;
    let (shared, event_rx) = shared_with_config_for_test(control, config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        shared.enter_cache_pause(&mut guard);
        guard.last_cache_state_emit_at = Some(Instant::now());
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_anchor(0, 1_000_000_000));

    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheBufferingChanged(Some(10))
        )
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
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
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.cache_state_emit_dirty());
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }

    shared.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    let cache_state = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });

    let cache_state = cache_state.expect("append emits dirty reader state when report is due");
    assert_eq!(cache_state.demux.reader_pts, Some(1.0));
    assert_eq!(cache_state.demux.cache_end, Some(3.0));
    assert_eq!(cache_state.demux.cache_duration, Some(2.0));
    assert_eq!(cache_state.demux.forward_bytes, 2048);
}

#[test]
fn demux_packet_cache_read_trim_emits_seekable_range_change_after_coalesced_maintenance() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 512 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let (shared, event_rx) = shared_with_config_for_test(control, config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let packet_count = DEMUX_PACKET_READ_TRIM_INTERVAL + 8;
        for index in 0..packet_count {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        guard.set_read_index_for_test(6);
        guard.reader_nsecs = 6_000_000_000;
        let emitted_state = guard.playback_cache_state(false);
        assert_eq!(
            emitted_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: u64::try_from(packet_count - 1).unwrap() as f64,
            }]
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_cache_state(&emitted_state);
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    let shared = Arc::new(shared);
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert!(matches!(
        cache.poll_packet_round_robin(&[0]).0,
        DemuxReadResult::Packet(_)
    ));
    assert!(
        event_rx.try_iter().all(|event| {
            let BackendEventKind::CacheStateChanged(state) = event.kind else {
                return true;
            };
            !state
                .demux
                .seekable_ranges
                .first()
                .is_some_and(|range| range.start == 1.0)
        }),
        "first read defers seekable range trim off the read hot path"
    );
    for _ in 1..DEMUX_PACKET_READ_TRIM_INTERVAL {
        assert!(matches!(
            cache.poll_packet_round_robin(&[0]).0,
            DemuxReadResult::Packet(_)
        ));
    }
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.cache_state_emit_dirty());
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    assert!(matches!(
        cache.poll_packet_round_robin(&[0]).0,
        DemuxReadResult::Packet(_)
    ));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );

    let emit = shared
        .prepare_cache_state_emit_after_append(false)
        .expect("coalesced maintenance emits changed seekable ranges when report is due");
    let cache_state = emit.cache_state;
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
            end: u64::try_from(DEMUX_PACKET_READ_TRIM_INTERVAL + 7).unwrap() as f64,
        }]
    );
}

#[test]
fn demux_packet_cache_reports_reader_state_after_nonblocking_packet_read() {
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
        cache.poll_packet_round_robin(&[0]).0,
        DemuxReadResult::Packet(_)
    ));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.cache_state_emit_dirty());
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }

    shared.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    let cache_state = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });

    let cache_state =
        cache_state.expect("append emits dirty nonblocking reader state when report is due");
    assert_eq!(cache_state.demux.reader_pts, Some(1.0));
    assert_eq!(cache_state.demux.cache_end, Some(3.0));
    assert_eq!(cache_state.demux.cache_duration, Some(2.0));
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );
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
fn demux_packet_cache_reports_per_stream_packet_queue_limit_without_pausing() {
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
    assert!(video_queue.prefetch_packet_queue_full);
    assert!(video_queue.reader_head_available);
    assert_eq!(
        video_queue.readable_packets_for_stream,
        DEMUX_STREAM_PACKET_QUEUE_LIMIT
    );
    assert!(video_queue.consumer_drainable);
    assert!(state.stream_packet_queue_full());
    assert!(!state.should_pause_demux());
    assert_eq!(demux_cache_blocked_on(&state, false), "demux_cache");
}

#[test]
fn cache_full_decoder_empty_drains_existing_packets() {
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

    let before = state.packet_queue_snapshot();
    let video_queue = before
        .streams
        .iter()
        .find(|stream| stream.stream_index == 0)
        .expect("video stream snapshot exists");
    assert!(video_queue.prefetch_packet_queue_full);
    assert!(video_queue.consumer_drainable);
    assert!(video_queue.reader_head_available);
    assert_eq!(
        video_queue.readable_packets_for_stream,
        DEMUX_STREAM_PACKET_QUEUE_LIMIT
    );

    let mut timing = DemuxPacketCacheReadTiming::default();
    let packet = state
        .take_packet_round_robin(&[0], &mut timing)
        .expect("full queue drain succeeds")
        .expect("packet exists");

    assert_eq!(packet.stream_offset, 0);
    assert_eq!(state.next_packet_id_for_stream(0), Some(1));
    let after = state.packet_queue_snapshot();
    let video_queue = after
        .streams
        .iter()
        .find(|stream| stream.stream_index == 0)
        .expect("video stream snapshot exists");
    assert!(video_queue.consumer_drainable);
    assert_eq!(
        video_queue.readable_packets_for_stream,
        DEMUX_STREAM_PACKET_QUEUE_LIMIT - 1
    );
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
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
    );
    {
        let guard = shared.state.lock().expect("cache state");
        assert!(guard.should_pause_demux());
        assert!(guard.cache_state_emit_dirty());
    }

    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    shared.append_packet(cached_anchor(1_000_000_000, 1_500_000_000));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
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
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(
        state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
        Some(2.0)
    );
    assert_eq!(state.read_index, 1);
    assert_eq!(state.reader_nsecs, 1_000_000_000);
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
    close_seek_range(&mut state, 1_000_000_000);

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
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

    let cache_state = state.playback_cache_state(false);
    assert!(cache_state.demux.bof_cached);
    assert!(!state.read_range().is_bof);
    assert_eq!(state.seek_cached(0, PlaybackSessionId(3)), Some(1.0));
    assert_eq!(state.reader_nsecs, 500_000_000);
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
    close_seek_range(&mut state, 11_000_000_000);

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
    close_seek_range(&mut state, 1_000_000_000);
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
    close_seek_range(&mut state, 1_000_000_000);
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
    close_seek_range(&mut state, 1_000_000_000);
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
fn low_level_seek_interrupts_cache_pause_and_avio_wait() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
    let generation = shared.generation();
    let seek_generation = control.seek_generation();

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        shared.enter_cache_pause(&mut guard);
    }
    assert!(control.is_cache_paused());

    control.request_seek();

    assert!(shared.wait_for_demux_permit().is_none());
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
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );
    assert!(
        shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .cache_state_emit_dirty()
    );

    shared.append_packet(cached_anchor(0, 2_000_000_000));

    assert!(!control.is_cache_paused());
    assert!(!control.is_paused());
    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .any(|event| { matches!(&event.kind, BackendEventKind::CacheBufferingChanged(None)) })
    );
    assert!(
        events
            .iter()
            .any(|event| { matches!(&event.kind, BackendEventKind::PausedForCacheChanged(false)) })
    );
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
fn demux_packet_cache_pause_waits_for_three_seconds_forward_before_resume() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 3.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        shared.enter_cache_pause_if_needed(&mut guard, true);
    }
    assert!(control.is_cache_paused());

    shared.append_packet(cached_anchor(0, 2_000_000_000));

    assert!(control.is_cache_paused());
    assert!(control.is_paused());

    shared.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));

    assert!(!control.is_cache_paused());
    assert!(!control.is_paused());
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

    let (result, stream_offset) = cache.read_available_packet_round_robin_with_cache_pause_signal(
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
fn demux_packet_cache_clear_pause_for_decoded_resume_clears_buffering_state() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
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
    assert!(control.is_cache_paused());
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    let cache = DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    };
    cache.clear_cache_pause_for_decoded_resume();

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
fn demux_packet_cache_coalesces_underrun_state_after_read_without_cache_pause() {
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
    let mut underrun_dirty = false;
    while Instant::now() < deadline {
        {
            let mut guard = shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            if guard.cache_state_emit_dirty() && guard.has_demux_underrun() {
                guard.last_cache_state_emit_at =
                    Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
                underrun_dirty = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(underrun_dirty, "read underrun marks cache state dirty");
    assert!(event_rx.try_iter().all(|event| {
        !matches!(
            event.kind,
            BackendEventKind::CacheStateChanged(state) if state.demux.underrun
        )
    }));

    let emit = shared
        .prepare_cache_state_emit_after_append(false)
        .expect("coalesced maintenance emits dirty underrun state when report is due");
    let underrun_state = emit.cache_state;

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
    assert!(underrun_state.demux.underrun);
    assert!(!underrun_state.demux.idle);
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
    close_seek_range(&mut state, 4_000_000_000);

    assert_eq!(
        state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_uses_safe_point_before_preroll_target() {
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
    close_seek_range(&mut state, 4_000_000_000);

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("cached seek hits");
    assert_eq!(hit.buffered_until_nsecs, 4_000_000_000);
    assert_eq!(hit.target_nsecs, 3_500_000_000);
    assert_eq!(hit.anchor_nsecs, 2_000_000_000);
    assert_eq!(hit.anchor_packet_id, 2);
    assert_eq!(hit.video_reader_head, 2);
    assert!(hit.anchor_is_recovery_point);
    assert!(hit.anchor_is_safe_seek_point);
    assert!(hit.requires_precise_trim);
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_rejects_recovery_point_without_safe_seek_point()
 {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let mut recovery_only_packet =
        cached_key_packet(0, true, Some(2_000_000_000), Some(3_000_000_000));
    recovery_only_packet.safe_seek_point = false;
    state.append_packet(recovery_only_packet);
    state.append_packet(cached_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);

    assert_eq!(state.seek_cached(3_500_000_000, PlaybackSessionId(2)), None);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 0);
    assert_eq!(state.cached_seeks, 0);
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_uses_latest_safe_point_before_effective_target()
 {
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
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(3_200_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("cached seek hits");
    assert_eq!(hit.anchor_nsecs, 2_000_000_000);
    assert_eq!(hit.anchor_packet_id, 0);
    assert!(hit.anchor_is_safe_seek_point);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
}

#[test]
fn demux_packet_cache_state_hits_hevc_cached_seek_from_first_safe_point_after_preroll() {
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
    close_seek_range(&mut state, 4_000_000_000);

    assert_eq!(
        state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
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
    close_seek_range(&mut state, 4_000_000_000);

    assert_eq!(
        state.seek_cached_fast(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
}

#[test]
fn demux_packet_cache_state_hits_hevc_cached_seek_with_short_recovery_window() {
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
    close_seek_range(&mut state, 8_000_000_000);

    assert_eq!(
        state.seek_cached(7_500_000_000, PlaybackSessionId(2)),
        Some(8.0)
    );
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 6_000_000_000);
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
    close_seek_range(&mut state, 3_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
            end: 3.0,
        }]
    );
    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(2)),
        Some(3.0)
    );
    assert_eq!(
        state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
        Some(3.0)
    );
    assert_eq!(state.read_index, 1);
}

#[test]
fn demux_packet_cache_state_reports_hevc_seekable_range_after_cached_seek_preroll() {
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
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 2.0,
        }]
    );
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
    close_seek_range(&mut state, 3_000_000_000);
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
    close_seek_range(&mut state, 3_000_000_000);

    let mut timing = DemuxPacketCacheReadTiming::default();
    assert!(
        state
            .take_packet_round_robin(&[0], &mut timing)
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
fn demux_packet_cache_state_materializes_disk_packet_after_reader_advance() {
    let temp_dir = tempfile::tempdir().expect("temp dir creates");
    let mut config = cache_config_for_test();
    config.disk_cache = true;
    config.cache_dir = Some(temp_dir.path().to_path_buf());
    config.unlink_files = CacheUnlinkPolicy::Never;
    let mut packet = AvPacket::from_data_and_props(
        b"packet-payload",
        &AvPacket::new().expect("packet allocates"),
    )
    .expect("packet has data");
    unsafe {
        (*packet.as_mut_ptr()).stream_index = 0;
    }
    let cached =
        CachedDemuxPacket::from_packet(&packet, 0, true, true, true, Some(0), Some(1_000_000_000))
            .expect("packet caches");
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.append_packet(cached);
    let mut timing = DemuxPacketCacheReadTiming::default();

    let source = state
        .take_packet_round_robin(&[0], &mut timing)
        .expect("packet source reads")
        .expect("packet source exists");

    assert_eq!(state.read_index, 1);
    drop(state);
    let (restored, stream_offset) = source.packet_ref(&mut timing).expect("packet restores");
    assert_eq!(stream_offset, 0);
    assert_eq!(restored.data(), Some(&b"packet-payload"[..]));
    assert_eq!(timing.disk_reads, 1);
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
    close_seek_range(&mut state, 6_000_000_000);

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
    close_seek_range(&mut state, 6_000_000_000);

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
    close_seek_range(&mut state, 3_000_000_000);

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
fn demux_packet_cache_state_rejects_cached_seek_when_selected_audio_stream_is_missing() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_EAC3)),
        subtitle_stream: None,
    });
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_packet(2, false, Some(0), Some(1_000_000_000)));
    close_seek_range(&mut state, 1_000_000_000);

    assert_eq!(
        state.seek_cached_fast(500_000_000, PlaybackSessionId(2)),
        Some(1.0)
    );
    assert_eq!(state.cached_seeks, 1);

    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC)),
        subtitle_stream: None,
    });

    assert_eq!(
        state.seek_cached_fast(500_000_000, PlaybackSessionId(3)),
        None
    );
    assert_eq!(state.cached_seeks, 1);
    assert_eq!(
        state.stream_kinds.get(&1).copied(),
        Some(StreamCacheKind::Audio)
    );
    assert!(!state.stream_kinds.contains_key(&2));
    assert!(!state.reader_heads.contains_key(&2));
}

#[test]
fn demux_packet_cache_state_realigns_audio_reader_head_to_timeline() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC)),
        subtitle_stream: None,
    });
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(5_000_000_000),
        Some(6_000_000_000),
    ));
    state.append_packet(cached_packet(
        1,
        false,
        Some(5_000_000_000),
        Some(5_200_000_000),
    ));
    state.append_packet(cached_packet(
        1,
        false,
        Some(5_400_000_000),
        Some(5_600_000_000),
    ));
    state.append_packet(cached_packet(
        1,
        false,
        Some(5_800_000_000),
        Some(6_000_000_000),
    ));

    let result = state
        .realign_stream_reader_to_timeline(1, 5_650_000_000, "test_audio_realign")
        .expect("audio reader head realigns inside current range");

    assert_eq!(result.stream_index, 1);
    assert_eq!(result.target_timeline_nsecs, 5_650_000_000);
    assert_eq!(result.old_packet_id, Some(1));
    assert_eq!(result.new_packet_id, 2);
    assert_eq!(result.new_start_nsecs, Some(5_400_000_000));
    assert_eq!(result.new_end_nsecs, Some(5_600_000_000));
    assert_eq!(state.next_packet_id_for_stream(1), Some(2));
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
    state.mark_eof();

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
    close_seek_range(&mut state, 5_000_000_000);

    assert_eq!(
        state.seek_cached_fast(4_500_000_000, PlaybackSessionId(2)),
        Some(5.0)
    );
    assert_eq!(state.forward_bytes(), 1024);
    assert_eq!(state.backward_bytes(), 3 * 1024);

    state.append_packet(cached_anchor(5_000_000_000, 6_000_000_000));
    close_seek_range(&mut state, 6_000_000_000);

    assert_eq!(state.forward_bytes(), 2 * 1024);
    assert_eq!(state.backward_bytes(), 3 * 1024);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
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

    let mut timing = DemuxPacketCacheReadTiming::default();
    state.consume_packet_id(0, &mut timing);

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
    close_seek_range(&mut state, 5_000_000_000);

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
fn demux_packet_cache_state_uses_per_stream_bof_for_seekable_start() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(500_000_000, 1_000_000_000));
    state.append_packet(cached_packet(
        1,
        false,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    let mut timing = DemuxPacketCacheReadTiming::default();
    assert!(
        state
            .take_packet_round_robin(&[1], &mut timing)
            .expect("audio packet reads")
            .is_some()
    );

    let cache_state = state.playback_cache_state(false);
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
            end: 2.0,
        }]
    );
    assert!(!cache_state.demux.bof_cached);
    assert_eq!(state.seek_cached(750_000_000, PlaybackSessionId(2)), None);
    assert_eq!(
        state.seek_cached(1_500_000_000, PlaybackSessionId(3)),
        Some(2.0)
    );
}

#[test]
fn demux_packet_cache_state_uses_per_stream_eof_for_seekable_end() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(8_000_000_000)));
    state.mark_eof();
    state.read_range_mut().ensure_stream_boundary(1).is_eof = false;

    let cache_state = state.playback_cache_state(false);
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 8.0,
        }]
    );
    assert!(!cache_state.demux.eof_cached);
    assert_eq!(state.seek_cached(9_000_000_000, PlaybackSessionId(2)), None);
    assert_eq!(
        state.seek_cached(7_000_000_000, PlaybackSessionId(3)),
        Some(8.0)
    );
}

#[test]
fn demux_packet_cache_state_does_not_split_seekable_range_at_audio_gap() {
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
    state.append_packet(cached_anchor(5_000_000_000, 6_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 5.0,
        }]
    );
    assert_eq!(
        state.seek_cached(4_000_000_000, PlaybackSessionId(3)),
        Some(5.0)
    );
}

#[test]
fn demux_packet_cache_state_limits_seekable_range_to_audio_eager_end() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 12_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(10_000_000_000)));
    state.append_packet(cached_anchor(12_000_000_000, 13_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 10.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_extends_seekable_range_to_max_eof_end() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(12_000_000_000)));
    state.mark_eof();

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 12.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_does_not_shorten_seekable_end_for_subtitle_gaps() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(0, 12_000_000_000));
    state.append_packet(cached_packet(2, false, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        2,
        false,
        Some(10_000_000_000),
        Some(11_000_000_000),
    ));
    state.append_packet(cached_anchor(12_000_000_000, 13_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 12.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_omits_seekable_range_without_recovery_point() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_packet(0, true, Some(0), Some(12_000_000_000)));

    let cache_state = state.playback_cache_state(false);
    assert_eq!(cache_state.demux.cache_end, Some(12.0));
    assert!(cache_state.demux.seekable_ranges.is_empty());
}

#[test]
fn demux_packet_cache_state_reports_single_seekable_range_across_timeline_gaps() {
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
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(4_000_000_000),
        Some(5_000_000_000),
    ));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 4.0,
        }]
    );
    assert_eq!(state.seek_cached(2_000_000_000, PlaybackSessionId(2)), None);
}

#[test]
fn demux_packet_cache_state_reports_hevc_seekable_range_after_cached_preroll_from_first_anchor() {
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
    close_seek_range(&mut state, 8_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 4.5,
            end: 8.0,
        }]
    );
    assert_eq!(
        state.seek_cached(7_500_000_000, PlaybackSessionId(2)),
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
fn demux_packet_cache_forced_low_level_seek_bypasses_cached_hit() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        close_seek_range(&mut guard, 1_000_000_000);
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert_eq!(
        cache.seek_low_level(0.5, PlaybackSessionId(9), 12, "test_forced_seek"),
        DemuxSeekResult::Requested
    );

    let guard = shared.state.lock().expect("cache state");
    assert_eq!(guard.cached_seeks, 0);
    assert_eq!(guard.low_level_seeks, 1);
    assert!(guard.seeking);
    assert_eq!(guard.reader_nsecs, 500_000_000);
    let request = guard.seek_request.expect("low-level seek queued");
    assert_eq!(request.position_seconds, 0.5);
    assert_eq!(request.session_id, PlaybackSessionId(9));
    assert_eq!(request.seek_generation, 12);
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
fn demux_packet_cache_state_skips_far_ahead_packets_after_low_level_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.request_seek(35.0, PlaybackSessionId(2), 1, 35_000_000_000);

    let blocked = state.append_packet(cached_anchor(237_000_000_000, 238_000_000_000));

    assert!(blocked.appended);
    assert_eq!(state.read_range().global_order.len(), 1);
    assert!(state.reader_heads.is_empty());
    assert_eq!(state.next_packet_id_for_stream(0), None);
    assert_eq!(state.forward_bytes(), 0);
    assert_eq!(state.cached_bytes, 2 * 1024);
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(35_000_000_000)
    );

    let accepted = state.append_packet(cached_anchor(35_000_000_000, 36_000_000_000));

    assert!(accepted.appended);
    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(state.reader_nsecs, 35_000_000_000);
    assert_eq!(state.next_packet_id_for_stream(0), Some(2));
    assert_eq!(state.forward_bytes(), 1024);
    assert_eq!(state.low_level_append_guard_target_nsecs, None);
}

#[test]
fn demux_packet_cache_state_blocks_far_ahead_audio_reader_after_low_level_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.request_seek(35.0, PlaybackSessionId(2), 1, 35_000_000_000);

    let blocked_audio = state.append_packet(cached_packet(
        1,
        false,
        Some(237_000_000_000),
        Some(238_000_000_000),
    ));

    assert!(blocked_audio.appended);
    assert_eq!(state.next_packet_id_for_stream(1), None);
    assert_eq!(state.forward_bytes(), 0);

    let accepted_video = state.append_packet(cached_anchor(35_000_000_000, 36_000_000_000));

    assert!(accepted_video.appended);
    assert_eq!(state.next_packet_id_for_stream(0), Some(2));
    assert_eq!(state.next_packet_id_for_stream(1), None);
    assert_eq!(state.forward_bytes(), 1024);
}

#[test]
fn demux_packet_cache_state_ignores_reader_heads_from_previous_generation() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));

    assert_eq!(state.next_packet_id_for_stream(0), Some(0));

    state
        .reader_head_generations
        .insert(0, state.generation.saturating_add(1));

    assert_eq!(state.next_packet_id_for_stream(0), None);

    state.reset_reader_heads_for_read_index();

    assert_eq!(state.next_packet_id_for_stream(0), Some(0));
}

#[test]
fn demux_packet_cache_coalesces_seek_completion_state_after_seek_result_appends() {
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
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.cache_state_emit_dirty());
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }

    shared.append_packet(cached_anchor(11_000_000_000, 12_000_000_000));

    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if !state.demux.seeking
                    && state.demux.reader_pts == Some(10.0)
                    && state.demux.cache_end == Some(12.0)
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
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);
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
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(3)),
        Some(1.0)
    );
    assert_eq!(state.reader_nsecs, 0);
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
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);
    assert_eq!(
        state.seek_cached_with_generation(
            500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(3),
            7
        ),
        Some(1.0)
    );
    assert_eq!(state.read_range().global_order.len(), 2);

    state.append_packet(cached_anchor(500_000_000, 1_000_000_000));

    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(1)
    );
    assert_eq!(state.cached_bytes, 3 * 1024);
    assert_eq!(state.resume_append_skip_until_nsecs, Some(1_000_000_000));
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(1_000_000_000)
    );
    let request = state.seek_request.expect("resume seek is queued");
    assert_eq!(request.seek_generation, 7);

    state.seek_request = None;
    let blocked_far_ahead = state.append_packet(cached_anchor(237_000_000_000, 238_000_000_000));
    assert!(blocked_far_ahead.appended);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(2)
    );
    assert_eq!(state.cached_bytes, 4 * 1024);
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(1_000_000_000)
    );

    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(4)
    );
    assert_eq!(state.cached_bytes, 5 * 1024);
    assert_eq!(state.forward_bytes(), 2 * 1024);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0
            },
            PlaybackCacheTimeRange {
                start: 1.0,
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
    assert_eq!(state.read_range().global_order.len(), 4);
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
        close_seek_range(&mut guard, 1_000_000_000);
        guard.demux_position_detached = true;
        guard.set_read_index_for_test(2);
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
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    state.append_packet(cached_anchor(11_000_000_000, 12_000_000_000));

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
    assert_eq!(cache_state.demux.total_bytes, 4096);
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
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);

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
fn demux_packet_cache_state_reports_large_active_streams_from_forward_cache() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 0;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);

    for index in 0..8192u64 {
        let start = index * 1_000_000_000;
        let end = start + 1_000_000_000;
        state.append_packet(cached_anchor(start, end));
        state.append_packet(cached_packet(1, false, Some(start), Some(end)));
    }

    let report_started_at = Instant::now();
    let cache_state = state.playback_cache_state(false);
    let report_elapsed = report_started_at.elapsed();
    let streams = cache_state.demux.streams;

    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].kind, StreamCacheKind::Video);
    assert_eq!(streams[0].reader_pts, Some(0.0));
    assert_eq!(streams[0].cache_end, Some(8192.0));
    assert_eq!(streams[0].cache_duration, Some(8192.0));
    assert_eq!(streams[1].kind, StreamCacheKind::Audio);
    assert_eq!(streams[1].reader_pts, Some(0.0));
    assert_eq!(streams[1].cache_end, Some(8192.0));
    assert_eq!(streams[1].cache_duration, Some(8192.0));
    assert_eq!(cache_state.demux.forward_bytes, 8192 * 2 * 1024);
    assert!(
        report_elapsed < Duration::from_millis(100),
        "large active cache state report took {report_elapsed:?}"
    );
}

#[test]
fn demux_packet_cache_seekable_summary_invalidates_after_stream_kind_change() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );

    state.set_stream_kind(1, StreamCacheKind::Audio);
    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );

    state.append_packet(cached_packet(1, false, Some(0), Some(2_000_000_000)));
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );
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
    close_seek_range(&mut state, 11_000_000_000);
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
    close_seek_range(&mut state, 4_000_000_000);

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
    close_seek_range(&mut state, 3_000_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

    let range = state
        .ranges
        .values()
        .next()
        .expect("archived range remains");
    assert_eq!(state.archived_bytes(), 3 * 1024);
    assert_eq!(range.global_order.len(), 4);
    assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(3));
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
    close_seek_range(&mut state, 2_500_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

    let range = state
        .ranges
        .values()
        .next()
        .expect("archived range remains");
    assert_eq!(state.archived_bytes(), 3 * 1024);
    assert_eq!(range.global_order.len(), 4);
    assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(3));
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
    close_seek_range(&mut state, 3_000_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

    let range = state
        .ranges
        .values()
        .next()
        .expect("archived range remains");
    assert_eq!(state.archived_bytes(), 3 * 1024);
    assert_eq!(range.global_order.len(), 4);
    assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(3));
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
    close_seek_range(&mut state, 3_000_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 1.5,
            end: 3.0,
        }
    );
    assert_eq!(
        state.seek_cached(1_250_000_000, PlaybackSessionId(3)),
        Some(3.0)
    );
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
    close_seek_range(&mut state, 3_000_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);

    let range = state
        .ranges
        .values()
        .next()
        .expect("archived range remains");
    assert_eq!(state.archived_bytes(), 4 * 1024);
    assert_eq!(range.global_order.len(), 5);
    assert_eq!(range.stream_queues.get(&0).map(VecDeque::len), Some(3));
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
    close_seek_range(&mut state, 3_000_000_000);
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
    close_seek_range(&mut state, 1_000_000_000);

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
    close_seek_range(&mut state, 1_000_000_000);

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
    let packet = AvPacket::from_data_and_props(b"packet-payload", &props).expect("packet has data");
    let mut cached = CachedDemuxPacket::from_packet(&packet, 0, true, true, true, Some(0), Some(1))
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
fn demux_packet_cache_readahead_defaults_to_cache_secs_when_cache_is_active() {
    // With the cache active, effective_readahead_secs() inflates to cache_secs
    // (up to an hour). This matches mpv: the deep packet prefetch target is then
    // bounded by demuxer_max_bytes instead of an extra time cap.
    let cached = PlaybackCacheConfig {
        demuxer_readahead_secs: 1.0,
        cache_secs: 3600.0,
        ..PlaybackCacheConfig::default()
    };
    assert_eq!(
        demux_packet_cache_readahead_nsecs(&cached, true),
        seconds_to_nsecs(3600.0)
    );

    // A local/cache-inactive input still uses the explicit demuxer readahead.
    let small = PlaybackCacheConfig {
        demuxer_readahead_secs: 2.0,
        ..PlaybackCacheConfig::default()
    };
    assert_eq!(
        demux_packet_cache_readahead_nsecs(&small, false),
        seconds_to_nsecs(2.0)
    );

    // A non-zero override can still cap packet prefetch for diagnostics or
    // constrained environments.
    let capped = PlaybackCacheConfig {
        demuxer_readahead_secs: 1.0,
        cache_secs: 120.0,
        demuxer_packet_max_readahead_secs: 30.0,
        ..PlaybackCacheConfig::default()
    };
    assert_eq!(
        demux_packet_cache_readahead_nsecs(&capped, true),
        seconds_to_nsecs(30.0)
    );
}

#[test]
fn demux_packet_cache_auto_hysteresis_is_capped_for_large_readahead() {
    let config = PlaybackCacheConfig {
        demuxer_hysteresis_secs: 0.0,
        ..PlaybackCacheConfig::default()
    };

    assert_eq!(
        demux_packet_cache_hysteresis_nsecs(&config, seconds_to_nsecs(60.0)),
        duration_nsecs(DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS)
    );

    let configured = PlaybackCacheConfig {
        demuxer_hysteresis_secs: 12.0,
        ..PlaybackCacheConfig::default()
    };
    assert_eq!(
        demux_packet_cache_hysteresis_nsecs(&configured, seconds_to_nsecs(60.0)),
        seconds_to_nsecs(12.0)
    );
}
