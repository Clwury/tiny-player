use super::*;

#[test]
fn demux_packet_cache_repeated_forward_seeks_refill_150_mib_during_output_recovery() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = Arc::new(shared_for_test(control));
    let packet_at = |second: u64| {
        let mut packet = cached_anchor(second * 1_000_000_000, (second + 1) * 1_000_000_000);
        // Model a high-bitrate stream without allocating large test payloads.
        packet.byte_len = 5 * 1024 * 1024;
        packet
    };
    for second in 0..30 {
        shared.append_packet(packet_at(second));
    }
    shared.control.set_output_rebuffer_paused(true);
    shared.set_playback_recovery_demand(true, true, false);

    let (result_tx, result_rx) = mpsc::channel();
    let producer = Arc::clone(&shared);
    let worker = thread::spawn(move || {
        let mut next_second = 30;
        for target in [5, 10, 15, 20, 25, 30] {
            {
                let mut state = producer.state.lock().unwrap();
                assert!(state.should_pause_demux());
                assert!(
                    state
                        .seek_cached(target * 1_000_000_000, PlaybackSessionId(target))
                        .is_some()
                );
                assert_eq!(state.forward_bytes(), 125 * 1024 * 1024);
                assert!(!state.should_pause_demux());
            }
            producer.notify_ready();
            for _ in 0..5 {
                assert!(producer.wait_for_demux_permit().is_none());
                producer.append_packet(packet_at(next_second));
                next_second += 1;
            }
            let state = producer.state.lock().unwrap();
            assert_eq!(state.forward_bytes(), 150 * 1024 * 1024);
            assert!(state.should_pause_demux());
            // Recovery may defer ordinary trim; urgent trim still runs before
            // refill, limiting the excess to the trim band plus one packet.
            assert!(state.cached_bytes <= (200 + 8 + 5) * 1024 * 1024);
        }
        result_tx.send(()).unwrap();
    });
    // The old recovery policy imposed at least 3 seconds for these 30 packets.
    // This exercises the real worker permit and cached-seek paths, not just
    // the state-level byte-limit predicate.
    let result = result_rx.recv_timeout(Duration::from_secs(1));
    shared.state.lock().unwrap().shutdown = true;
    shared.notify_ready();
    worker.join().expect("forward refill producer completes");
    assert!(
        result.is_ok(),
        "cached seeks should refill without a per-packet recovery delay"
    );
}

#[test]
fn demux_packet_cache_network_prefetch_fills_forward_budget_despite_fast_input() {
    let config = PlaybackCacheConfig::default().resolved_for_cacheable_input(true);
    let target = seconds_to_nsecs(config.cache_secs);
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    for second in 0..20 {
        let mut packet = cached_anchor(
            seconds_to_nsecs(second as f64),
            seconds_to_nsecs((second + 1) as f64),
        );
        packet.byte_len = 1024 * 1024;
        state.append_packet(packet);
    }
    // Twenty seconds of media downloaded in half a second is a fast network,
    // not a 40 MiB/s media bitrate. It must not shorten the seekable window.
    let sample_at = Instant::now() - Duration::from_millis(500);
    for sample in &mut state.input_rate_samples {
        sample.at = sample_at;
    }
    state.refresh_readahead_hysteresis();
    assert_eq!(state.effective_readahead_nsecs(), target);
    assert!(!state.should_pause_demux());
    assert!(!state.packet_queue_snapshot().prefetch_queue_full());

    for second in 20..150 {
        assert!(
            !state.should_pause_demux(),
            "forward budget remains at {second} MiB"
        );
        let mut packet = cached_anchor(
            seconds_to_nsecs(second as f64),
            seconds_to_nsecs((second + 1) as f64),
        );
        packet.byte_len = 1024 * 1024;
        state.append_packet(packet);
    }
    assert_eq!(state.forward_bytes(), 150 * 1024 * 1024);
    assert!(state.should_pause_demux());
    assert!(state.packet_queue_snapshot().prefetch_queue_full());

    state.input_rate_samples.clear();
    assert_eq!(state.effective_readahead_nsecs(), target);
    assert!(state.should_pause_demux());
}

#[test]
fn demux_packet_cache_network_refill_reclaims_donated_backbuffer() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        PlaybackCacheConfig::default().resolved_for_cacheable_input(true),
    );
    for second in 0..150 {
        let mut packet = cached_anchor(
            seconds_to_nsecs(second as f64),
            seconds_to_nsecs((second + 1) as f64),
        );
        packet.byte_len = 1024 * 1024;
        state.append_packet(packet);
    }
    state.reader_nsecs = seconds_to_nsecs(100.0);
    state.set_read_index_for_test(100);
    state.trim_to_limit();
    assert_eq!(state.backward_bytes(), 100 * 1024 * 1024);
    assert!(!state.backbuffer_pressure());

    for second in 150..250 {
        assert!(
            !state.should_pause_demux(),
            "donated backbuffer must allow forward refill"
        );
        let mut packet = cached_anchor(
            seconds_to_nsecs(second as f64),
            seconds_to_nsecs((second + 1) as f64),
        );
        packet.byte_len = 1024 * 1024;
        state.append_packet(packet);
    }
    state.trim_to_limit();
    assert_eq!(state.forward_bytes(), 150 * 1024 * 1024);
    assert_eq!(state.backward_bytes(), 50 * 1024 * 1024);
    assert_eq!(state.cached_bytes, 200 * 1024 * 1024);
    assert!(state.should_pause_demux());
    let report = state.playback_cache_state(false);
    assert_eq!(report.demux.seekable_ranges.len(), 1);
    let range = report.demux.seekable_ranges[0];
    assert!(range.start < 100.0 && range.end > 100.0);
    assert!(range.end - 100.0 > 100.0 - range.start);
}

#[test]
fn demux_packet_cache_prefetch_snapshot_follows_explicit_time_limit() {
    let config = PlaybackCacheConfig {
        cache_secs: 30.0,
        demuxer_packet_max_readahead_secs: 2.0,
        ..PlaybackCacheConfig::default()
    };
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    assert!(!state.packet_queue_snapshot().prefetch_queue_full());
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    assert!(state.should_pause_demux());
    assert!(state.packet_queue_snapshot().prefetch_queue_full());

    state.set_read_index_for_test(1);
    state.refresh_readahead_hysteresis();
    assert!(!state.should_pause_demux());
    assert!(!state.packet_queue_snapshot().prefetch_queue_full());
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

    // A finite shared budget turns the legacy "unlimited" forward setting
    // into the remaining bounded slice. Setting the total budget to zero is
    // the explicit way to retain independent-layer semantics.
    assert!(state.memory_limit_bytes > 0);
    assert!(!state.should_pause_demux());

    let independent = PlaybackCacheConfig {
        demuxer_max_bytes: 0,
        total_cache_max_bytes: 0,
        ..PlaybackCacheConfig::default()
    };
    let independent_state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        independent,
    );
    assert_eq!(independent_state.memory_limit_bytes, 0);
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
