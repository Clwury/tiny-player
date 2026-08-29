use super::*;

#[test]
fn demux_packet_cache_append_trims_backbuffer_incrementally_under_pressure() {
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

    assert!(outcome.timing.trim > Duration::ZERO);
    assert_eq!(state.backward_bytes(), 2 * 1024);
    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert_eq!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .first(),
        Some(&PlaybackCacheTimeRange {
            start: 4.0,
            end: 8.0,
        })
    );
}

#[test]
fn demux_packet_cache_append_trim_coalesces_seekable_window_until_report_due() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let (shared, event_rx) = shared_with_config_for_test(control, config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        for index in 0..8 {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        guard.set_read_index_for_test(6);
        guard.reader_nsecs = 6_000_000_000;
        let emitted_state = guard.playback_cache_state(false);
        assert_eq!(
            emitted_state.demux.seekable_ranges.first(),
            Some(&PlaybackCacheTimeRange {
                start: 0.0,
                end: 7.0,
            })
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_cache_state(&emitted_state);
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));
    assert!(
        event_rx
            .try_iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_))),
        "mpv keeps internal trim contractions off OSC until the 250 ms cache tick"
    );
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert_eq!(
            guard
                .playback_cache_state(false)
                .demux
                .seekable_ranges
                .first(),
            Some(&PlaybackCacheTimeRange {
                start: 4.0,
                end: 8.0,
            })
        );
    }

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    shared.append_packet(cached_anchor(9_000_000_000, 10_000_000_000));
    let emitted_state = event_rx
        .try_iter()
        .find_map(|event| match event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state),
            _ => None,
        })
        .expect("250 ms cache tick publishes the coalesced seekable contraction");
    assert_eq!(
        emitted_state.demux.seekable_ranges.first(),
        Some(&PlaybackCacheTimeRange {
            start: 4.0,
            end: 9.0,
        })
    );
}

#[test]
fn demux_packet_cache_append_trim_keeps_distant_next_seek_boundary() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 1024 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    for index in 0..700_u64 {
        let start_nsecs = index * 40_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            index == 0 || index == 600,
            Some(start_nsecs),
            Some(start_nsecs + 40_000_000),
        ));
    }
    close_seek_range(&mut state, 28_000_000_000);
    state.set_read_index_for_test(650);
    state.reader_nsecs = 26_000_000_000;

    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert!(state.trim_to_limit_for_append());

    assert_eq!(
        state.read_range().stream_queues.get(&0).unwrap().front(),
        Some(&600)
    );
    assert_eq!(
        state
            .read_range()
            .stream_seek_boundaries
            .get(&0)
            .and_then(|boundaries| boundaries.front()),
        Some(&600)
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 24.0,
            end: 28.0,
        }]
    );
}

#[test]
fn demux_packet_cache_trim_preserves_reader_covering_seek_boundary() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 1024 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    for index in 0..=30_u64 {
        let start_nsecs = index * 1_000_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            index % 10 == 0,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    state.set_read_index_for_test(15);
    state.reader_nsecs = 15_000_000_000;

    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert!(state.trim_to_limit_for_append());
    assert_eq!(
        state.read_range().stream_queues.get(&0).unwrap().front(),
        Some(&10)
    );
    assert!(!state.trim_to_limit_for_append());

    assert_eq!(
        state.read_range().stream_queues.get(&0).unwrap().front(),
        Some(&10)
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 10.0,
            end: 30.0,
        }]
    );
}

#[test]
fn demux_packet_cache_trim_keeps_reader_covering_seekable_start() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 1024 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        130_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);

    for second in 130..=170_u64 {
        let start_nsecs = second * 1_000_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            second == 130 || second == 150,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    for second in 120..=170_u64 {
        let start_nsecs = second * 1_000_000_000;
        state.append_packet(cached_packet(
            1,
            false,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    let reader_nsecs = 139_000_000_000;
    set_reader_head_for_stream_time(&mut state, 0, reader_nsecs);
    set_reader_head_for_stream_time(&mut state, 1, reader_nsecs);
    state.reader_nsecs = reader_nsecs;

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 130.0,
            end: 150.0,
        }]
    );
    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert!(state.trim_to_limit());

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 130.0,
            end: 150.0,
        }]
    );
}

#[test]
fn demux_packet_cache_trim_slides_to_shared_seek_boundary() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 1024 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        130_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);

    for second in 130..=170_u64 {
        let start_nsecs = second * 1_000_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            second == 130 || second == 140 || second == 150,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    for second in 120..=170_u64 {
        let start_nsecs = second * 1_000_000_000;
        state.append_packet(cached_packet(
            1,
            false,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    let reader_nsecs = 145_000_000_000;
    set_reader_head_for_stream_time(&mut state, 0, reader_nsecs);
    set_reader_head_for_stream_time(&mut state, 1, reader_nsecs);
    state.reader_nsecs = reader_nsecs;

    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert!(state.trim_to_limit());

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 140.0,
            end: 150.0,
        }]
    );
}

#[test]
fn demux_packet_cache_trim_falls_back_to_anchor_when_audio_is_at_anchor_limit() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_back_bytes = 3500;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.mark_read_stream_bof(0, false);
    state.mark_read_stream_bof(1, false);

    for second in [0_u64, 10, 20, 30] {
        let start_nsecs = second * 1_000_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            true,
            Some(start_nsecs),
            Some(start_nsecs + 1_000_000_000),
        ));
    }
    for start_nsecs in [487_000_000_u64, 510_000_000, 20_000_000_000] {
        state.append_packet(cached_packet(
            1,
            false,
            Some(start_nsecs),
            Some(start_nsecs + 20_000_000),
        ));
    }

    set_reader_head_for_stream_time(&mut state, 0, 20_000_000_000);
    set_reader_head_for_stream_time(&mut state, 1, 20_000_000_000);
    state.reader_nsecs = 20_000_000_000;

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 20.02,
        }]
    );
    assert!(state.backward_bytes() > state.effective_backbuffer_limit());
    assert!(state.trim_to_limit());

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 10.5,
            end: 20.02,
        }]
    );
}

#[test]
fn demux_packet_cache_append_trim_emit_keeps_seekable_start_before_reader_with_dense_audio() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 1024 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let (shared, event_rx) = shared_with_config_for_test(control, config);
    let reader_nsecs = 139_000_000_000;
    let initial_cached_bytes;

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.set_stream_kind(1, StreamCacheKind::Audio);
        guard.mark_read_stream_bof(0, false);
        guard.mark_read_stream_bof(1, false);

        for second in 130..=170_u64 {
            let start_nsecs = second * 1_000_000_000;
            guard.append_packet(cached_packet_with_keyframe(
                0,
                true,
                second == 130 || second == 150,
                Some(start_nsecs),
                Some(start_nsecs + 1_000_000_000),
            ));
        }
        for packet_index in 0..=DEMUX_STREAM_PACKET_QUEUE_LIMIT {
            let start_nsecs = 120_000_000_000 + u64::try_from(packet_index).unwrap() * 20_000_000;
            let mut packet =
                cached_packet(1, false, Some(start_nsecs), Some(start_nsecs + 20_000_000));
            packet.byte_len = 1;
            guard.append_packet(packet);
        }

        assert!(
            guard.read_range().stream_queues.get(&1).unwrap().len()
                > DEMUX_STREAM_PACKET_QUEUE_LIMIT
        );
        set_reader_head_for_stream_time(&mut guard, 0, reader_nsecs);
        set_reader_head_for_stream_time(&mut guard, 1, reader_nsecs);
        guard.reader_nsecs = reader_nsecs;
        assert_eq!(
            guard.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 130.0,
                end: 150.0,
            }]
        );
        assert!(guard.backward_bytes() > guard.effective_backbuffer_limit());
        guard.append_trim_pressure_packets = DEMUX_PACKET_APPEND_TRIM_INTERVAL - 1;
        initial_cached_bytes = guard.cached_bytes;
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_packet_with_keyframe(
        0,
        true,
        false,
        Some(171_000_000_000),
        Some(172_000_000_000),
    ));

    let events = event_rx.try_iter().collect::<Vec<_>>();
    let emitted_state = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    let emitted_state = emitted_state.expect("append trim emits cache state for dense audio case");
    assert_eq!(
        emitted_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 130.0,
            end: 150.0,
        }]
    );
    assert!(
        emitted_state.demux.seekable_ranges[0].start < 139.0,
        "seekable range start jumped to reader/current"
    );

    let guard = shared
        .state
        .lock()
        .expect("FFmpeg demux packet cache poisoned");
    assert!(guard.cached_bytes < initial_cached_bytes + 1024);
    assert!(guard.read_range().stream_boundary(1).pruned_packet_count > 0);
    let video_front = guard
        .read_range()
        .stream_queues
        .get(&0)
        .and_then(|queue| queue.front())
        .and_then(|packet_id| guard.packets.get(packet_id))
        .expect("video backbuffer front remains cached");
    assert_eq!(video_front.start_nsecs, Some(130_000_000_000));
    assert!(video_front.timeline_anchor);
}

#[test]
fn demux_packet_cache_donated_append_budget_uses_trim_hysteresis() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 8 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = true;
    let (shared, event_rx) = shared_with_config_for_test(control, config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        for index in 0..8 {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        guard.set_read_index_for_test(6);
        guard.reader_nsecs = 6_000_000_000;
        let emitted_state = guard.playback_cache_state(false);
        assert_eq!(
            emitted_state.demux.seekable_ranges.first(),
            Some(&PlaybackCacheTimeRange {
                start: 0.0,
                end: 7.0,
            })
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_cache_state(&emitted_state);
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));
    assert!(
        event_rx
            .try_iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_))),
        "one-byte donated-budget overrun stays inside append trim hysteresis"
    );
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert_eq!(
            guard
                .playback_cache_state(false)
                .demux
                .seekable_ranges
                .first(),
            Some(&PlaybackCacheTimeRange {
                start: 0.0,
                end: 8.0,
            })
        );
    }

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    shared.append_packet(cached_anchor(9_000_000_000, 10_000_000_000));
    let emitted_state = event_rx.try_iter().find_map(|event| match event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    let emitted_state =
        emitted_state.expect("250 ms cache tick publishes contraction after trim hysteresis");
    assert!(
        emitted_state
            .demux
            .seekable_ranges
            .first()
            .is_some_and(|range| range.start > 0.0 && range.end == 9.0)
    );
}

#[test]
fn demux_packet_cache_donated_backbuffer_is_not_forward_memory_pressure() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
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
    for index in 0..8_u64 {
        let start_nsecs = index * 1_000_000_000;
        state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
    }
    state.set_read_index_for_test(6);

    assert_eq!(state.cached_bytes, 8 * 1024);
    assert_eq!(state.forward_bytes(), 2 * 1024);
    assert!(!state.memory_pressure());
    assert!(!state.backbuffer_pressure());

    state.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));

    assert_eq!(state.cached_bytes, 9 * 1024);
    assert_eq!(state.forward_bytes(), 3 * 1024);
    assert!(!state.memory_pressure());
    assert!(state.backbuffer_pressure());
    assert!(!state.append_trim_active);
    assert_eq!(state.append_trim_pressure_packets, 1);
}

#[test]
fn demux_packet_cache_append_defers_trim_for_waiting_consumer() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let (shared, _event_rx) = shared_with_config_for_test(control, config);
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        for index in 0..8_u64 {
            let start_nsecs = index * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        guard.set_read_index_for_test(6);
        guard.append_trim_pressure_packets = DEMUX_PACKET_APPEND_TRIM_INTERVAL - 1;
    }

    shared.consumer_waiting_readers.store(1, Ordering::Release);
    shared.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert_eq!(
            guard.read_range().stream_queues.get(&0).unwrap().front(),
            Some(&0)
        );
        assert!(guard.append_trim_pending);
    }

    shared.consumer_waiting_readers.store(0, Ordering::Release);
    shared.append_packet(cached_anchor(9_000_000_000, 10_000_000_000));
    let guard = shared
        .state
        .lock()
        .expect("FFmpeg demux packet cache poisoned");
    assert!(
        guard
            .read_range()
            .stream_queues
            .get(&0)
            .and_then(|queue| queue.front())
            .is_some_and(|packet_id| *packet_id > 0)
    );
    assert!(!guard.append_trim_pending);
}

#[test]
fn demux_packet_cache_append_defers_trim_during_playback_recovery() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let (shared, _event_rx) = shared_with_config_for_test(control, config);
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        for index in 0..8_u64 {
            let start_nsecs = index * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        guard.set_read_index_for_test(6);
        guard.append_trim_pressure_packets = DEMUX_PACKET_APPEND_TRIM_INTERVAL - 1;
    }

    shared.set_playback_recovery_demand(true, false, false);
    shared.append_packet(cached_anchor(8_000_000_000, 9_000_000_000));
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert_eq!(
            guard.read_range().stream_queues.get(&0).unwrap().front(),
            Some(&0)
        );
        assert!(guard.append_trim_pending);
    }

    shared.set_playback_recovery_demand(false, false, false);
    shared.append_packet(cached_anchor(9_000_000_000, 10_000_000_000));
    let guard = shared
        .state
        .lock()
        .expect("FFmpeg demux packet cache poisoned");
    assert!(
        guard
            .read_range()
            .stream_queues
            .get(&0)
            .and_then(|queue| queue.front())
            .is_some_and(|packet_id| *packet_id > 0)
    );
    assert!(!guard.append_trim_pending);
}

#[test]
fn demux_packet_cache_recovery_priority_yields_then_forces_bounded_producer_progress() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = Arc::new(shared_for_test(control));
    shared.append_packet(cached_anchor(0, 1_000_000_000));
    shared.set_playback_recovery_demand(true, true, false);

    let barrier = Arc::new(Barrier::new(2));
    let (result_tx, result_rx) = mpsc::channel();
    let thread_shared = Arc::clone(&shared);
    let thread_barrier = Arc::clone(&barrier);
    let handle = thread::spawn(move || {
        thread_barrier.wait();
        result_tx
            .send(thread_shared.wait_for_demux_permit())
            .expect("send demux permit result");
    });
    barrier.wait();

    assert!(result_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert!(result_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    handle.join().expect("demux permit waiter joins");
}

#[test]
fn demux_packet_cache_recovery_demand_does_not_yield_for_unrequested_audio() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = Arc::new(shared_for_test(control));
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.set_selected_streams(DemuxSelectedStreams {
            audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
            subtitle_stream: None,
        });
    }
    shared.append_packet(cached_packet(1, false, Some(0), Some(1_000_000)));
    shared.set_playback_recovery_demand(true, true, false);

    let barrier = Arc::new(Barrier::new(2));
    let (result_tx, result_rx) = mpsc::channel();
    let thread_shared = Arc::clone(&shared);
    let thread_barrier = Arc::clone(&barrier);
    let handle = thread::spawn(move || {
        thread_barrier.wait();
        result_tx
            .send(thread_shared.wait_for_demux_permit())
            .expect("send demux permit result");
    });
    barrier.wait();

    let result = result_rx.recv_timeout(Duration::from_millis(100));
    shared.set_playback_recovery_demand(false, false, false);
    handle.join().expect("demux permit waiter joins");
    assert!(result.is_ok());
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
fn demux_packet_cache_read_suppresses_trim_during_playback_recovery() {
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
    for index in 0..8_u64 {
        let start_nsecs = index * 1_000_000_000;
        state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
    }
    state.set_read_index_for_test(6);
    state.read_trim_pressure_packets = DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL - 1;

    let mut timing = DemuxPacketCacheReadTiming::default();
    let packet = state
        .take_packet_round_robin_with_trim(&[0], &mut timing, false)
        .expect("read packet")
        .expect("packet exists");

    assert_eq!(packet.stream_offset, 0);
    assert_eq!(timing.trim, Duration::ZERO);
    assert!(!timing.trim_outcome.performed);
    assert_eq!(
        state.read_trim_pressure_packets,
        DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL - 1
    );
}

#[test]
fn demux_packet_cache_large_trim_is_packet_bounded_and_reports_work() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024 * 1024;
    config.demuxer_max_back_bytes = 16 * 1024 * 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    for index in 0..6000_u64 {
        state.append_packet(cached_anchor(index, index.saturating_add(1)));
    }
    state.set_read_index_for_test(5500);
    state.backbuffer_limit_bytes = 1024;

    let global_order_len_before = state.read_range().global_order.len();
    let outcome = state.trim_to_limit_for_append_with_outcome();

    assert!(outcome.performed);
    assert!(outcome.steps <= DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT);
    assert!(
        outcome.removed_packets
            <= DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT * DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP
    );
    assert_eq!(outcome.global_order_len_before, global_order_len_before);
    assert_eq!(
        outcome.global_order_len_after,
        global_order_len_before.saturating_sub(outcome.compacted_global_entries)
    );
    assert_eq!(outcome.compacted_global_entries, outcome.removed_packets);
    assert!(outcome.remaining_overrun_bytes > 0);
}

#[test]
fn demux_packet_cache_large_dense_audio_trim_avoids_full_global_retain() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 64 * 1024 * 1024;
    config.demuxer_max_back_bytes = 16 * 1024 * 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(1000, 1001));
    for index in 0..6000_u64 {
        state.append_packet(cached_packet(
            1,
            false,
            Some(index),
            Some(index.saturating_add(1)),
        ));
    }
    state.append_packet(cached_anchor(6001, 6002));
    state.set_read_index_for_test(5500);
    state.backbuffer_limit_bytes = 1024;

    let global_order_len_before = state.read_range().global_order.len();
    let reader_head_position_before = state.reader_head_positions.get(&1).copied();
    let outcome = state.trim_to_limit_for_append_with_outcome();

    assert!(outcome.performed);
    assert!(outcome.removed_packets > 0);
    assert_eq!(outcome.compacted_global_entries, 0);
    assert_eq!(
        state.read_range().global_order.len(),
        global_order_len_before
    );
    assert_eq!(
        state.reader_head_positions.get(&1).copied(),
        reader_head_position_before
    );
    assert!(
        outcome.removed_packets
            <= DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT * DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP
    );
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
