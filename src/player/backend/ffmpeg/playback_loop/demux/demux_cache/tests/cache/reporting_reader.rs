use super::*;

#[test]
fn demux_packet_cache_monitor_returns_cached_snapshot_while_state_is_locked() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let cache = Arc::new(DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    });
    cache.shared.append_packet(cached_anchor(0, 1_000_000_000));
    let (before, before_watermark, unavailable) = cache.monitor_snapshot();
    assert!(!unavailable);
    assert_eq!(before.total_packets, 1);

    let mut guard = cache.shared.state.lock().expect("hold demux state");
    guard.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    let (tx, rx) = mpsc::channel();
    let reader_cache = Arc::clone(&cache);
    let reader = thread::spawn(move || {
        tx.send(reader_cache.monitor_snapshot())
            .expect("send diagnostic snapshot");
    });
    // Release the lock even on failure so this regression fails rather than
    // deadlocking the test suite if diagnostics start taking a blocking lock.
    let result = rx.recv_timeout(Duration::from_secs(1));
    drop(guard);
    reader.join().expect("diagnostic reader exits");
    let (cached, watermark, unavailable) = result.expect("diagnostics must not wait for demux");
    assert!(unavailable);
    assert_eq!(cached.total_packets, before.total_packets);
    assert_eq!(
        watermark.selected_min_forward_nsecs,
        before_watermark.selected_min_forward_nsecs
    );

    let (fresh, watermark, unavailable) = cache.monitor_snapshot();
    assert!(!unavailable);
    assert_eq!(fresh.total_packets, 2);
    assert_eq!(watermark.selected_min_forward_nsecs, Some(2_000_000_000));
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
fn demux_packet_cache_report_due_publishes_osc_seekable_growth_with_deep_readahead() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_secs = 3600.0;
    config.demuxer_readahead_secs = 3600.0;
    let (shared, event_rx) = shared_with_config_for_test(control, config);
    let packet_count = 1025usize;

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        for index in 0..packet_count {
            let start_nsecs = u64::try_from(index).unwrap() * 1_000_000_000;
            guard.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
        }
        let emitted_state = guard.playback_cache_state(false);
        assert_eq!(
            emitted_state.demux.seekable_ranges.last(),
            Some(&PlaybackCacheTimeRange {
                start: 0.0,
                end: (packet_count - 1) as f64,
            })
        );
        assert!(guard.cached_bytes < 150 * 1024 * 1024);
        guard.record_emitted_cache_state(&emitted_state);
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    let start_nsecs = u64::try_from(packet_count).unwrap() * 1_000_000_000;
    shared.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));

    let cache_state = event_rx.try_iter().find_map(|event| match event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    let cache_state =
        cache_state.expect("mpv-style 250 ms cache tick publishes seekable range growth");
    assert_eq!(
        cache_state.demux.seekable_ranges.last(),
        Some(&PlaybackCacheTimeRange {
            start: 0.0,
            end: packet_count as f64,
        })
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
fn demux_packet_cache_does_not_notify_ui_for_an_unchanged_final_range_vector() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        guard.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
        let baseline = shared.prepare_cache_state_emit_for(&mut guard, "test", true);
        shared.send_cache_state_emit(baseline.into_emit());
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.read_range().mark_seekable_dirty();
        guard.bump_seekability_revision();
        let unchanged = shared.prepare_cache_state_emit_for(&mut guard, "test", false);
        shared.send_cache_state_emit(unchanged.into_emit());
    }

    assert!(
        event_rx
            .try_iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_))),
        "an internal seekability revision must not refresh the UI when the normalized OSC vector and cache state are unchanged"
    );
}

#[test]
fn demux_packet_cache_reports_reader_state_after_packet_read() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    shared.append_packet(cached_anchor(0, 1_000_000_000));
    shared.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let _ = guard.cache_report_snapshot(false);
    }
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

    let cache_state = cache_state.expect("read emits changed seekable range state immediately");
    assert_eq!(cache_state.demux.reader_pts, Some(1.0));
    assert_eq!(cache_state.demux.cache_end, Some(2.0));
    assert_eq!(cache_state.demux.cache_duration, Some(1.0));
    assert_eq!(cache_state.demux.forward_bytes, 1024);
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0,
        }]
    );
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
    assert!(
        event_rx
            .try_iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_))),
        "read-side trim contractions wait for mpv's cache update cadence"
    );
    let expected_ranges = {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let ranges = guard.playback_cache_state(false).demux.seekable_ranges;
        assert!(
            ranges.first().is_some_and(|range| range.start > 1.0),
            "multiple internal trim steps should be coalesced before OSC observes them"
        );
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
        ranges
    };
    assert!(matches!(
        cache.poll_packet_round_robin(&[0]).0,
        DemuxReadResult::Packet(_)
    ));
    let events = event_rx.try_iter().collect::<Vec<_>>();
    let cache_state = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    let cache_state =
        cache_state.expect("250 ms cache tick publishes the coalesced read-side contraction");
    assert_eq!(cache_state.demux.seekable_ranges, expected_ranges);
}

#[test]
fn demux_packet_cache_reports_reader_state_after_nonblocking_packet_read() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    shared.append_packet(cached_anchor(0, 1_000_000_000));
    shared.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let _ = guard.cache_report_snapshot(false);
    }
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
    let cache_state = events.iter().find_map(|event| match &event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });

    let cache_state =
        cache_state.expect("nonblocking read emits changed seekable range state immediately");
    assert_eq!(cache_state.demux.reader_pts, Some(1.0));
    assert_eq!(cache_state.demux.cache_end, Some(2.0));
    assert_eq!(cache_state.demux.cache_duration, Some(1.0));
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0,
        }]
    );
}

#[test]
fn demux_packet_cache_read_coalesces_truehd_seekable_growth_until_report_due() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.set_selected_streams(DemuxSelectedStreams {
            audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
            subtitle_stream: None,
        });
        guard.append_packet(cached_anchor(0, 10_000_000_000));
        guard.append_packet(cached_key_packet(1, false, Some(0), None));
        guard.append_packet(cached_packet(1, false, Some(1_000_000_000), None));
        guard.append_packet(cached_key_packet(1, false, Some(2_000_000_000), None));
        close_seek_range(&mut guard, 10_000_000_000);
        let emitted_state = guard.playback_cache_state(false);
        assert_eq!(
            emitted_state.demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0,
            }]
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_cache_state(&emitted_state);

        guard.append_packet(cached_packet(1, false, Some(3_000_000_000), None));
        guard.append_packet(cached_key_packet(1, false, Some(4_000_000_000), None));
        assert!(guard.seekable_ranges_changed_since_last_emit());
        // Model the producer-side 250 ms maintenance pass. The consumer
        // emission below must only copy this prepared result.
        let _ = guard.cache_report_snapshot(false);
        shared.emit_cache_state_after_read(&mut guard, false);
    }
    assert!(
        event_rx
            .try_iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_))),
        "ordinary TrueHD range growth waits for mpv's 250 ms cache tick"
    );

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
        shared.emit_cache_state_after_read(&mut guard, false);
    }
    let cache_state = event_rx.try_iter().find_map(|event| match event.kind {
        BackendEventKind::CacheStateChanged(state) => Some(state),
        _ => None,
    });
    assert_eq!(
        cache_state
            .expect("250 ms cache tick publishes coalesced TrueHD range growth")
            .demux
            .seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 3.0,
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
fn post_seek_producer_error_keeps_drainable_packet_cache_readable() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    let mut config = cache_config_for_test();
    config.cache_pause = false;
    let (shared, _) = shared_with_config_for_test(Arc::clone(&control), config);
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
    }

    let (consumer_drainable, forward_duration_nsecs) = shared
        .note_producer_recovering("FFmpeg 读取媒体包失败：Input/output error".to_string(), 10);
    assert!(consumer_drainable);
    assert_eq!(forward_duration_nsecs, 1_000_000_000);
    {
        let guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.error.is_none());
        assert_eq!(guard.producer_recovery_consecutive_errors, 10);
        assert!(guard.producer_recovery_error.is_some());
    }

    let cache = DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    };
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
fn demux_packet_cache_attaches_sequential_reader_diagnostics() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = false;
    let (shared, _) = shared_with_config_for_test(control, config);
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        guard.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
    }
    let cache = DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    };

    let first = match cache.poll_packet(0) {
        DemuxReadResult::Packet(packet) => packet,
        _ => panic!("expected first video packet"),
    };
    let first = first
        .read_diagnostic()
        .expect("first cache packet has read diagnostic");
    assert_eq!(first.read_sequence, 1);
    assert_eq!(first.packet_id, 0);
    assert_eq!(first.storage, AvPacketStorageKind::Memory);
    assert_eq!(first.reader_head_before, Some(0));
    assert_eq!(first.reader_head_after, Some(1));
    assert_eq!(first.previous_read_packet_id, None);
    assert_eq!(first.sequence_contiguous, None);

    let second = match cache.poll_packet(0) {
        DemuxReadResult::Packet(packet) => packet,
        _ => panic!("expected second video packet"),
    };
    let second = second
        .read_diagnostic()
        .expect("second cache packet has read diagnostic");
    assert_eq!(second.read_sequence, 2);
    assert_eq!(second.packet_id, 1);
    assert_eq!(second.previous_read_packet_id, Some(0));
    assert_eq!(second.previous_expected_next_packet_id, Some(1));
    assert_eq!(second.sequence_contiguous, Some(true));
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
    assert!(!video_queue.prefetch_packet_queue_full);
    assert!(!snapshot.prefetch_queue_full());
    assert!(video_queue.reader_head_available);
    // The readable count saturates at the snapshot scan limit to keep the
    // per-read monitor refresh cheap.
    assert_eq!(
        video_queue.readable_packets_for_stream,
        DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT
    );
    assert!(video_queue.consumer_drainable);
    assert!(state.stream_packet_queue_full());
    assert!(!state.should_pause_demux());
    assert_eq!(demux_cache_blocked_on(&state, false), "demux_cache");
}

#[test]
fn read_trim_backbuffer_overrun_paces_by_interval() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_bytes = 128 * 1024;
    config.demuxer_max_back_bytes = 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    for index in 0..300_u64 {
        let start_nsecs = index * 1_000_000_000;
        state.append_packet(cached_anchor(start_nsecs, start_nsecs + 1_000_000_000));
    }
    state.set_read_index_for_test(200);
    assert!(!state.memory_pressure());
    assert!(state.backbuffer_pressure());

    // A large backward-cache overrun is still paced instead of adding trim
    // work to every consumer read.
    for _ in 0..DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL - 1 {
        assert!(!state.read_trim_due());
    }
    assert!(state.read_trim_due());
    assert!(!state.read_trim_due());
}
