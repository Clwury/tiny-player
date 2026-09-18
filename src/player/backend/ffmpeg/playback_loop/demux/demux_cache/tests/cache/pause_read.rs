use super::*;

#[test]
fn steady_playback_cache_pause_requires_demux_and_actual_output_underrun() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 2.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.has_demux_underrun());
        shared.enter_cache_pause_if_needed(&mut guard, true);
    }
    assert!(!control.is_cache_paused());

    control.set_output_underrun_for_cache_pause(true);
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        shared.enter_cache_pause_if_needed(&mut guard, true);
    }
    assert!(control.is_cache_paused());
}

#[test]
fn demux_packet_cache_does_not_pause_with_twenty_two_seconds_forward() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 30.0;
    config.demuxer_readahead_secs = 60.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);

    shared.append_packet(cached_anchor(0, 22_000_000_000));
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let watermark = guard.reader_watermark();
        assert_eq!(watermark.video_forward_nsecs, Some(22_000_000_000));
        assert!(!watermark.video_underrun);
        assert!(!watermark.underrun);

        // The true output-underrun signal alone is insufficient: like mpv,
        // cache pause also requires an actual demux underrun.
        shared.enter_cache_pause_if_needed(&mut guard, true);
    }

    assert!(!control.is_cache_paused());
    assert!(!control.is_paused());
}

#[test]
fn demux_packet_cache_pause_waits_for_three_seconds_forward_before_resume() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    control.set_output_underrun_for_cache_pause(true);
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
fn demux_packet_cache_pause_uses_exact_seek_target_not_idr_anchor() {
    let target_nsecs = 184_692_319_900;
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 3.0;
    let mut state = DemuxPacketCacheState::new(
        target_nsecs,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        config,
    );

    state.append_packet(cached_anchor(179_900_000_000, 183_133_333_333));

    assert_eq!(state.exact_seek_target_nsecs, target_nsecs);
    assert!(!state.cache_pause_target_covered());
    assert_eq!(state.cache_pause_forward_duration_nsecs(), 0);
    assert!(!state.cache_pause_recovered());

    state.append_packet(cached_anchor(183_133_333_333, 187_692_319_900));

    assert!(state.cache_pause_target_covered());
    assert_eq!(state.cache_pause_forward_duration_nsecs(), 3_000_000_000);
    assert!(state.cache_pause_recovered());
}

#[test]
fn demux_packet_cache_pause_requires_audio_to_cover_exact_seek_target() {
    let target_nsecs = 184_692_319_900;
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 1.0;
    let mut state = DemuxPacketCacheState::new(
        target_nsecs,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        config,
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC)),
        subtitle_stream: None,
    });
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_packet(
        2,
        false,
        Some(179_900_000_000),
        Some(180_900_000_000),
    ));
    state.append_packet(cached_anchor(179_900_000_000, 186_000_000_000));
    state.append_packet(cached_packet(
        1,
        false,
        Some(179_900_000_000),
        Some(183_133_333_333),
    ));

    assert!(!state.cache_pause_target_covered());
    assert_eq!(state.cache_pause_forward_duration_nsecs(), 0);

    state.append_packet(cached_packet(
        1,
        false,
        Some(183_133_333_333),
        Some(185_692_319_900),
    ));

    assert!(state.cache_pause_target_covered());
    assert_eq!(state.cache_pause_forward_duration_nsecs(), 1_000_000_000);
    assert!(state.cache_pause_recovered());
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
    control.set_output_underrun_for_cache_pause(true);
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
    control.set_output_underrun_for_cache_pause(true);
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
    control.set_output_underrun_for_cache_pause(true);
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
    let mut underrun_state = None;
    while Instant::now() < deadline {
        for event in event_rx.try_iter() {
            if let BackendEventKind::CacheStateChanged(state) = event.kind
                && state.demux.underrun
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
    let underrun_state = underrun_state.expect("read underrun emits cache state immediately");

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
    control.set_output_underrun_for_cache_pause(true);
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
    control.set_output_underrun_for_cache_pause(true);
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
fn demux_packet_cache_wait_for_cached_input_wakes_on_producer_append() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
    let cache = Arc::new(DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    });
    control.set_cache_paused(true);
    let (started_tx, started_rx) = mpsc::channel();
    let waiting_cache = Arc::clone(&cache);
    let waiter = thread::spawn(move || {
        started_tx.send(()).expect("waiter start signal sends");
        let started_at = Instant::now();
        waiting_cache.wait_for_consumer_drainable(&[0], Duration::from_secs(1));
        started_at.elapsed()
    });
    started_rx.recv().expect("waiter starts");
    thread::sleep(Duration::from_millis(20));

    cache.shared.append_packet(cached_anchor(0, 1_000_000_000));

    let waited = waiter.join().expect("cached-input waiter joins");
    assert!(waited < Duration::from_millis(500));
    assert!(
        cache
            .packet_queue_snapshot()
            .consumer_drainable_for_streams(&[0])
    );
    assert!(control.is_cache_paused());
}

#[test]
fn demux_packet_cache_wait_for_cached_input_wakes_on_seek_generation() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
    let cache = Arc::new(DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    });
    control.set_cache_paused(true);
    let (started_tx, started_rx) = mpsc::channel();
    let waiting_cache = Arc::clone(&cache);
    let waiter = thread::spawn(move || {
        started_tx.send(()).expect("waiter start signal sends");
        let started_at = Instant::now();
        waiting_cache.wait_for_consumer_drainable(&[0], Duration::from_secs(1));
        started_at.elapsed()
    });
    started_rx.recv().expect("waiter starts");

    control.request_seek();

    let waited = waiter.join().expect("seek-interrupted waiter joins");
    assert!(waited < Duration::from_millis(100), "waited={waited:?}");
    assert!(control.has_pending_seek());
}

#[test]
fn demux_packet_cache_generation_wait_does_not_return_for_existing_drainable_input() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let mut config = cache_config_for_test();
    config.cache_pause = true;
    config.cache_pause_wait = 10.0;
    let (shared, _event_rx) = shared_with_config_for_test(Arc::clone(&control), config);
    let cache = Arc::new(DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    });
    control.set_cache_paused(true);
    cache.shared.append_packet(cached_anchor(0, 1_000_000_000));
    let observed_generation = cache.packet_queue_snapshot().cache_generation;
    let (started_tx, started_rx) = mpsc::channel();
    let waiting_cache = Arc::clone(&cache);
    let waiter = thread::spawn(move || {
        started_tx.send(()).expect("waiter start signal sends");
        let started_at = Instant::now();
        let changed = waiting_cache
            .wait_for_cache_generation_change(observed_generation, Duration::from_secs(1));
        (changed, started_at.elapsed())
    });
    started_rx.recv().expect("waiter starts");
    thread::sleep(Duration::from_millis(20));

    cache.shared.append_packet(cached_anchor(0, 1_040_000_000));

    let (changed, waited) = waiter.join().expect("generation waiter joins");
    assert!(changed);
    assert!(waited >= Duration::from_millis(15));
    assert!(waited < Duration::from_millis(500));
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
    let deadline = Instant::now() + Duration::from_millis(100);
    while shared.consumer_waiting_readers.load(Ordering::Acquire) == 0 && Instant::now() < deadline
    {
        thread::yield_now();
    }
    assert_eq!(shared.consumer_waiting_readers.load(Ordering::Acquire), 1);
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
