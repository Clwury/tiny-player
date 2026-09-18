use super::*;

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
