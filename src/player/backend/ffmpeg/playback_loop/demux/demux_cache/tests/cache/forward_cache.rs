use super::*;

#[test]
fn detached_cache_snapshots_and_reads_do_not_rescan_growing_packet_history() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    for second in 0..6 {
        state.append_packet_fast(cached_anchor(
            second * 1_000_000_000,
            (second + 1) * 1_000_000_000,
        ));
    }
    state.start_detached_append_range();
    let detached_id = state.append_range_id;
    for index in 0..4096 {
        let start = 6_000_000_000 + index * 40_000_000;
        state.append_packet_fast(cached_anchor(start, start + 40_000_000));
        let snapshot = state.packet_queue_snapshot();
        assert_eq!(snapshot.total_packets, 7 + index as usize);
        assert_eq!(
            snapshot.streams[0].cached_end_nsecs,
            Some(start + 40_000_000)
        );
    }
    let before = state.forward_bytes();
    let mut timing = DemuxPacketCacheReadTiming::default();
    for _ in 0..3 {
        state
            .take_packet_round_robin_with_trim(&[0], &mut timing, false)
            .unwrap()
            .unwrap();
        let _ = state.packet_queue_snapshot();
        let _ = state.reader_watermark();
    }
    assert!(state.forward_bytes() < before);
    assert_eq!(
        state.ranges[&detached_id].forward_stats_rebuilds.get(),
        0,
        "append/read monitoring must be independent of detached history length"
    );

    assert!(state.seek_cached(0, PlaybackSessionId(2)).is_some());
    for _ in 0..16 {
        assert_eq!(state.packet_queue_snapshot().total_packets, 4102);
        assert_eq!(state.forward_bytes(), before);
    }
    assert_eq!(
        state.ranges[&detached_id].forward_stats_rebuilds.get(),
        1,
        "a new seek generation rebuilds coverage only once"
    );
}

#[test]
fn detached_forward_coverage_excludes_overlap_and_preserves_reordered_timestamps() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    let mut video = cached_anchor(0, 1_000_000_000);
    video.byte_len = 10;
    state.append_packet_fast(video);
    let mut audio = cached_packet(1, false, Some(0), Some(1_000_000_000));
    audio.byte_len = 10;
    state.append_packet_fast(audio);
    state.start_detached_append_range();
    state
        .low_level_append_blocked_packet_generations
        .insert(state.next_packet_id, state.generation);
    for (stream, start, end, bytes) in [
        (0, Some(9_000_000_000), Some(10_000_000_000), 10), // overlap
        (0, Some(12_000_000_000), Some(13_000_000_000), 20),
        (1, Some(11_000_000_000), Some(14_000_000_000), 30),
        (0, Some(11_000_000_000), Some(12_000_000_000), 40), // reordered PTS
        (0, None, None, 5),
    ] {
        let mut packet = cached_packet(stream, stream == 0, start, end);
        packet.byte_len = bytes;
        state.append_packet_fast(packet);
        let _ = state.packet_queue_snapshot();
    }
    assert_eq!(state.forward_bytes(), 115);
    assert_eq!(state.forward_duration_nsecs(), 13_000_000_000);
    let snapshot = state.packet_queue_snapshot();
    assert_eq!(
        snapshot.total_bytes, 125,
        "stored queue diagnostics include the overlap"
    );
    assert_eq!(snapshot.streams[0].queued_packets, 5);
    assert_eq!(snapshot.streams[0].queued_bytes, 85);
    assert_eq!(snapshot.streams[1].cached_end_nsecs, Some(14_000_000_000));
    let stats = state.range_forward_stats(state.detached_append_range().unwrap());
    assert_eq!(stats.readable[&0].reader_nsecs, Some(11_000_000_000));
}

#[test]
fn detached_forward_coverage_refreshes_generation_blocks_on_cached_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet_fast(cached_anchor(0, 1_000_000_000));
    state.append_packet_fast(cached_anchor(1_000_000_000, 2_000_000_000));
    state.start_detached_append_range();
    state
        .low_level_append_blocked_packet_generations
        .insert(state.next_packet_id, state.generation + 1);
    let mut future_blocked = cached_anchor(20_000_000_000, 21_000_000_000);
    future_blocked.byte_len = 100;
    state.append_packet_fast(future_blocked);
    let before = state.forward_bytes();
    assert_eq!(
        state.packet_queue_snapshot().streams[0].cached_end_nsecs,
        Some(21_000_000_000)
    );

    assert!(state.seek_cached(0, PlaybackSessionId(2)).is_some());
    assert_eq!(state.forward_bytes(), before - 100);
    assert_eq!(
        state.packet_queue_snapshot().streams[0].cached_end_nsecs,
        Some(2_000_000_000)
    );
    assert_eq!(state.packet_queue_snapshot().total_packets, 3);
}

#[test]
fn range_forward_coverage_discards_trimmed_minimum_and_maximum_timestamps() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    // The first packet owns both extrema; removing it must shrink coverage.
    state.append_packet_fast(cached_anchor(0, 9_000_000_000));
    state.append_packet_fast(cached_anchor(2_000_000_000, 3_000_000_000));
    state.append_packet_fast(cached_anchor(3_000_000_000, 4_000_000_000));
    assert_eq!(
        state.range_forward_stats(state.read_range()).readable[&0].end_nsecs,
        Some(9_000_000_000)
    );
    set_reader_head_for_stream_time(&mut state, 0, 2_000_000_000);
    state.remove_read_range_stream_prefix_packets_for_test(0, 1);
    let stats = state.range_forward_stats(state.read_range());
    assert_eq!(stats.readable[&0].reader_nsecs, Some(2_000_000_000));
    assert_eq!(stats.readable[&0].end_nsecs, Some(4_000_000_000));
    assert_eq!(stats.stored[&0].packet_count, 2);
}

#[test]
fn forward_cache_uses_av_tail_when_subtitles_end_early() {
    let mut state = DemuxPacketCacheState::new(
        1_302_885_124_999,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    // Reproduce the reported tail: subtitles stop at 22:49.896, while video
    // and audio remain cached through 23:42.671 and 23:44.868 respectively.
    state.append_packet(cached_anchor(1_302_885_124_999, 1_422_670_541_665));
    state.append_packet(cached_packet(
        1,
        false,
        Some(1_304_217_000_000),
        Some(1_424_868_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(1_309_586_000_000),
        Some(1_369_896_000_000),
    ));

    for eof in [false, true] {
        if eof {
            state.mark_eof();
        }
        let cache = state.playback_cache_state(false).demux;
        assert_eq!(cache.eof, eof);
        assert_eq!(cache.cache_end, Some(1422.670541665));
        assert_eq!(cache.reader_pts, Some(1302.885124999));
        assert_eq!(cache.cache_duration, Some(119.785416666));
        assert_eq!(state.forward_duration_nsecs(), 119_785_416_666);
        let subtitle = cache
            .streams
            .iter()
            .find(|stream| stream.kind == StreamCacheKind::Subtitle)
            .expect("subtitle cache remains independently reported");
        assert_eq!(subtitle.cache_end, Some(1369.896));
        assert_eq!(subtitle.cache_duration, Some(60.31));
        let prepared = state
            .cache_report_snapshot_from_prepared(false)
            .expect("consumer can reuse prepared seekable ranges");
        assert_eq!(prepared.demux.cache_end, cache.cache_end);
    }
}

#[test]
fn forward_cache_prefetch_hysteresis_ignores_sparse_subtitle_cues() {
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
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_packet(2, false, Some(0), Some(500_000_000)));
    for second in 0..3 {
        let start = second * 1_000_000_000;
        let end = start + 1_000_000_000;
        state.append_packet(cached_anchor(start, end));
        state.append_packet(cached_packet(1, false, Some(start), Some(end)));
    }

    assert_eq!(state.forward_duration_nsecs(), 3_000_000_000);
    assert!(state.hysteresis_active);
    assert!(state.should_pause_demux());
    assert!(state.playback_cache_state(false).demux.idle);

    let mut timing = DemuxPacketCacheReadTiming::default();
    for stream_index in [0, 1] {
        state
            .take_packet_round_robin(&[stream_index], &mut timing)
            .expect("media packet reads")
            .expect("media packet exists");
    }
    state.refresh_readahead_hysteresis();

    assert_eq!(state.forward_duration_nsecs(), 2_000_000_000);
    assert!(!state.hysteresis_active);
    assert!(!state.should_pause_demux());
    assert!(!state.playback_cache_state(false).demux.idle);
}

#[test]
fn forward_cache_does_not_follow_subtitles_after_av_is_consumed_at_eof() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(10_000_000_000)));
    state.append_packet(cached_packet(2, false, Some(0), Some(30_000_000_000)));
    state.mark_eof();

    let mut timing = DemuxPacketCacheReadTiming::default();
    for stream_index in [0, 1] {
        state
            .take_packet_round_robin(&[stream_index], &mut timing)
            .expect("media packet reads")
            .expect("media packet exists");
    }

    let cache = state.playback_cache_state(false).demux;
    assert!(cache.eof);
    assert_eq!(cache.cache_end, Some(10.0));
    assert_eq!(cache.reader_pts, Some(10.0));
    assert_eq!(cache.cache_duration, Some(0.0));
    assert_eq!(state.forward_duration_nsecs(), 0);
    let subtitle = cache
        .streams
        .iter()
        .find(|stream| stream.kind == StreamCacheKind::Subtitle)
        .expect("unconsumed subtitle remains cached");
    assert_eq!(subtitle.cache_end, Some(30.0));
}

#[test]
fn forward_cache_does_not_bridge_to_future_archived_range() {
    let mut state = DemuxPacketCacheState::new(
        100_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(100_000_000_000, 110_000_000_000));
    state.mark_eof();
    state.request_seek(20.0, PlaybackSessionId(2), 1, 20_000_000_000);
    state.append_packet(cached_anchor(20_000_000_000, 30_000_000_000));
    state.append_packet(cached_packet(
        2,
        false,
        Some(20_000_000_000),
        Some(21_000_000_000),
    ));
    close_seek_range(&mut state, 30_000_000_000);

    let cache = state.playback_cache_state(false).demux;
    assert_eq!(
        cache.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 20.0,
                end: 30.0,
            },
            PlaybackCacheTimeRange {
                start: 100.0,
                end: 110.0,
            },
        ]
    );
    assert_eq!(cache.cache_end, Some(30.0));
    assert_eq!(cache.cache_duration, Some(10.0));
    assert_eq!(state.forward_duration_nsecs(), 10_000_000_000);
}
