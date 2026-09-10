use super::*;

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
    assert!(video_queue.packet_queue_full);
    assert!(!video_queue.prefetch_packet_queue_full);
    assert!(video_queue.consumer_drainable);
    assert!(video_queue.reader_head_available);
    assert_eq!(
        video_queue.readable_packets_for_stream,
        DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT
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
        DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT
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
    assert!(!snapshot.prefetch_queue_full());
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
fn cached_seek_advances_reader_generation_without_moving_demux_input() {
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
    let demux_input_generation = state.demux_input_generation;

    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(2)),
        Some(1.0)
    );
    assert!(state.generation > generation);
    assert_eq!(state.demux_input_generation, demux_input_generation);
}

#[test]
fn control_seek_request_alone_does_not_discard_linear_demux_read() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = shared_for_test(Arc::clone(&control));
    let demux_input_generation = shared.demux_input_generation();
    let seek_generation = control.seek_generation();

    control.request_seek();

    assert!(!shared.should_discard_demux_read_result(demux_input_generation));
    assert!(shared.should_discard_demux_seek_result(demux_input_generation, seek_generation));
}

#[test]
fn pending_seek_wakes_cache_pause_without_dropping_linear_demux_read() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
    let demux_input_generation = shared.demux_input_generation();

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
    assert!(!shared.should_discard_demux_read_result(demux_input_generation));
}

#[test]
fn low_level_seek_request_fences_inflight_demux_read() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = shared_for_test(Arc::clone(&control));
    let demux_input_generation = shared.demux_input_generation();
    let seek_generation = control.request_seek();

    shared
        .state
        .lock()
        .expect("FFmpeg demux packet cache poisoned")
        .request_seek(10.0, PlaybackSessionId(2), seek_generation, 10_000_000_000);

    assert!(shared.should_discard_demux_read_result(demux_input_generation));
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
    control.set_output_underrun_for_cache_pause(true);
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
