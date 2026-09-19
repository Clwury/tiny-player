use super::*;

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
fn demux_packet_cache_state_counts_discarded_overlap_in_raw_input_rate() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.refreshing_streams.insert(
        0,
        super::super::super::model::StreamResumePosition::new(&cached_anchor(
            1_000_000_000,
            2_000_000_000,
        )),
    );
    let mut packet = cached_anchor(0, 1_000_000_000);
    packet.byte_len = 4096;

    let outcome = state.append_packet(packet);

    assert!(!outcome.appended);
    assert_eq!(state.cached_bytes, 0);
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
