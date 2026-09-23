use super::*;

#[test]
fn demux_packet_cache_bounds_archived_ranges_and_cleans_seek_tombstones() {
    let mut config = cache_config_for_test();
    config.total_cache_max_bytes = 0;
    config.demuxer_max_ranges = 2;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.ranges.insert(1, DemuxCachedRange::new(1, false, 1));
    state.ranges.insert(2, DemuxCachedRange::new(2, false, 2));
    state.ranges.insert(3, DemuxCachedRange::new(3, false, 3));
    state.rejected_cached_seek_ranges.insert(
        1,
        CachedSeekMiss {
            range_id: Some(1),
            target_nsecs: 1,
            reason: CachedSeekMissReason::TargetOutsideRange,
        },
    );
    assert_eq!(state.enforce_cached_range_limit(), 2);

    assert_eq!(state.ranges.len(), 2);
    assert!(state.ranges.contains_key(&0));
    assert!(state.ranges.contains_key(&3));
    assert!(!state.ranges.contains_key(&1));
    assert!(!state.ranges.contains_key(&2));
    assert!(!state.rejected_cached_seek_ranges.contains_key(&1));
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
    finish_bounded_read_trim(&mut state);

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
fn demux_packet_cache_state_prunes_truehd_audio_at_major_sync_boundary() {
    let mut config = cache_config_for_test();
    config.demuxer_max_back_bytes = 6 * 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(1_000_000_000), None));
    state.append_packet(cached_packet(1, false, Some(2_000_000_000), None));
    state.append_packet(cached_anchor(3_000_000_000, 4_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(3_000_000_000), None));
    state.append_packet(cached_packet(1, false, Some(4_000_000_000), None));
    state.append_packet(cached_packet(1, false, Some(5_000_000_000), None));
    state.append_packet(cached_anchor(6_000_000_000, 7_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(6_000_000_000), None));
    close_seek_range(&mut state, 7_000_000_000);

    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    finish_bounded_read_trim(&mut state);

    let archived_range = state
        .ranges
        .iter()
        .find(|(range_id, _)| **range_id != state.read_range_id)
        .map(|(_, range)| range)
        .expect("archived range remains after bounded trim");
    let audio_front = archived_range
        .stream_queues
        .get(&1)
        .and_then(|queue| queue.front())
        .copied()
        .expect("trimmed TrueHD queue remains");
    assert!(
        state
            .packets
            .get(&audio_front)
            .is_some_and(|packet| packet.recovery_point),
        "TrueHD trim leaves a major-sync packet at the queue head"
    );
    assert_eq!(
        state
            .packets
            .get(&audio_front)
            .and_then(|packet| packet.start_nsecs),
        Some(3_000_000_000)
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 3.0,
            end: 5.0,
        }
    );
}

#[test]
fn demux_packet_cache_state_trims_50k_truehd_queue_one_major_sync_block_at_a_time() {
    let mut config = cache_config_for_test();
    config.demuxer_max_bytes = 150 * 1024 * 1024;
    config.demuxer_max_back_bytes = 75 * 1024 * 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    for index in 0..50_000_u64 {
        let start_nsecs = index * 1_000_000;
        let packet = if index.is_multiple_of(32) {
            cached_key_packet(1, false, Some(start_nsecs), Some(start_nsecs + 1_000_000))
        } else {
            cached_packet(1, false, Some(start_nsecs), Some(start_nsecs + 1_000_000))
        };
        state.append_packet(packet);
    }
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(50_000_000_000),
        Some(51_000_000_000),
    ));
    state.set_reader_head_for_current_generation(1, 40_001);
    state.refresh_reader_tracking();
    let old_seek_end_nsecs = state.read_range().stream_boundary(1).seek_end_nsecs;
    state.backbuffer_limit_bytes = state.backward_bytes().saturating_sub(1);

    let outcome = state.trim_to_limit_for_append_with_outcome();

    assert!(outcome.performed);
    assert_eq!(outcome.steps, 1);
    assert_eq!(outcome.removed_packets, 32);
    assert_eq!(outcome.remaining_overrun_bytes, 0);
    assert_eq!(
        state.read_range().stream_queues.get(&1).map(VecDeque::len),
        Some(49_968)
    );
    let boundary = state.read_range().stream_boundary(1);
    assert_eq!(boundary.seek_start_nsecs, Some(32_000_000));
    assert_eq!(boundary.seek_end_nsecs, old_seek_end_nsecs);
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
    finish_bounded_read_trim(&mut state);

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
            start: 1.0,
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
fn demux_packet_cache_state_applies_sparse_last_pruned_without_reader_gate() {
    let mut state = DemuxPacketCacheState::new(
        60_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(3, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(50_000_000_000, 60_000_000_000));
    state.append_packet(cached_anchor(60_000_000_000, 160_000_000_000));
    close_seek_range(&mut state, 160_000_000_000);
    {
        let range = state.read_range_mut();
        range.ensure_stream_boundary(3).last_pruned_nsecs = Some(153_800_000_000);
        range.mark_seekable_dirty();
    }

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 153.9,
            end: 160.0,
        }
    );
}

#[test]
fn demux_packet_cache_trim_records_sparse_last_pruned_from_old_seek_start_like_mpv() {
    let mut config = cache_config_for_test();
    config.cache_secs = 1000.0;
    config.demuxer_readahead_secs = 1000.0;
    config.demuxer_max_back_bytes = 1;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(500_000_000, 240_000_000_000));
    state.append_packet(cached_packet(
        1,
        false,
        Some(500_000_000),
        Some(240_000_000_000),
    ));
    state.append_packet(cached_packet(2, false, Some(0), Some(141_520_000_000)));
    state.append_packet(cached_packet(
        2,
        false,
        Some(141_520_000_000),
        Some(200_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(200_000_000_000),
        Some(240_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(240_000_000_000),
        Some(260_000_000_000),
    ));
    close_seek_range(&mut state, 240_000_000_000);

    set_reader_head_for_stream_time(&mut state, 0, 500_000_000);
    set_reader_head_for_stream_time(&mut state, 1, 500_000_000);
    set_reader_head_for_stream_time(&mut state, 2, 200_000_000_000);
    state.reader_nsecs = 141_990_022_676;

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 0.5,
            end: 240.0,
        }
    );
    assert!(state.trim_to_limit());
    assert_eq!(
        state.read_range().stream_boundary(2).last_pruned_nsecs,
        Some(0)
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 0.5,
            end: 240.0,
        }
    );

    set_reader_head_for_stream_time(&mut state, 2, 240_000_000_000);
    state.reader_nsecs = 200_000_000_000;

    assert!(state.trim_to_limit());
    assert_eq!(
        state.read_range().stream_boundary(2).last_pruned_nsecs,
        Some(141_520_000_000)
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges[0],
        PlaybackCacheTimeRange {
            start: 141.62,
            end: 240.0,
        }
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
    let mut cached = CachedDemuxPacket::from_packet(
        &packet,
        0,
        true,
        CachedDemuxPacketRecovery {
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Keyframe,
            safe_seek_point: true,
        },
        Some(0),
        Some(1),
        Some(0),
    )
    .expect("packet caches");
    let directory = tempfile::tempdir().unwrap();
    let disk_cache = DemuxPacketDiskCache::new(
        1024,
        Some(directory.path().to_path_buf()),
        CacheUnlinkPolicy::WhenDone,
    )
    .expect("disk cache creates");
    assert_eq!(disk_cache.path.parent(), Some(directory.path()));

    cached
        .spill_to_disk(&disk_cache)
        .expect("packet spills to disk");
    let restored = cached
        .packet_ref(Some(&disk_cache))
        .expect("packet restores from disk");

    assert_eq!(restored.data(), Some(&b"packet-payload"[..]));
}

#[test]
fn disk_packet_eviction_reuses_space_without_overwriting_an_in_flight_read() {
    let dir = tempfile::tempdir().unwrap();
    let disk =
        DemuxPacketDiskCache::new(8, Some(dir.path().into()), CacheUnlinkPolicy::WhenDone).unwrap();
    let make_packet = |data: &[u8]| {
        let props = AvPacket::new().unwrap();
        let packet = AvPacket::from_data_and_props(data, &props).unwrap();
        CachedDemuxPacket::from_packet(
            &packet,
            0,
            true,
            CachedDemuxPacketRecovery {
                recovery_point: true,
                recovery_kind: VideoRecoveryPointKind::Keyframe,
                safe_seek_point: true,
            },
            Some(0),
            Some(1),
            Some(0),
        )
        .unwrap()
    };
    let mut first = make_packet(b"original");
    first.spill_to_disk(&disk).unwrap();
    let reader = first.read_source(Some(&disk), 0).unwrap();
    drop(first);
    let mut next = make_packet(b"replaced");
    next.spill_to_disk(&disk).unwrap();
    assert!(matches!(next.payload, CachedDemuxPacketPayload::Memory(_)));
    let (restored, _) = reader
        .packet_ref(&mut DemuxPacketCacheReadTiming::default())
        .unwrap();
    assert_eq!(restored.data(), Some(&b"original"[..]));
    next.spill_to_disk(&disk).unwrap();
    assert!(matches!(
        next.payload,
        CachedDemuxPacketPayload::Disk { .. }
    ));
    for _ in 0..100 {
        drop(next);
        next = make_packet(b"replaced");
        next.spill_to_disk(&disk).unwrap();
        assert!(matches!(
            next.payload,
            CachedDemuxPacketPayload::Disk { .. }
        ));
        assert_eq!(
            next.packet_ref(Some(&disk)).unwrap().data(),
            Some(&b"replaced"[..])
        );
    }
    assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), 8);
}

#[test]
fn reducing_disk_quota_restores_evicted_packets_before_reclaiming_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut disk =
        DemuxPacketDiskCache::new(16, Some(dir.path().into()), CacheUnlinkPolicy::WhenDone)
            .unwrap();
    let prefix = disk.write_packet(b"12345678").unwrap();
    let props = AvPacket::new().unwrap();
    let packet = AvPacket::from_data_and_props(b"retained", &props).unwrap();
    let mut cached = CachedDemuxPacket::from_packet(
        &packet,
        0,
        true,
        CachedDemuxPacketRecovery {
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Keyframe,
            safe_seek_point: true,
        },
        Some(0),
        Some(1),
        Some(0),
    )
    .unwrap();
    cached.spill_to_disk(&disk).unwrap();
    // A total of 9 bytes reserves 1 for HTTP and 8 for demux.
    disk.set_limit(&PlaybackCacheConfig {
        disk_cache_max_bytes: 9,
        ..PlaybackCacheConfig::default()
    });
    cached.restore_outside_disk_limit(&disk).unwrap();
    disk.maintain_file_size();
    assert!(matches!(
        cached.payload,
        CachedDemuxPacketPayload::Memory(_)
    ));
    assert_eq!(
        cached.packet_ref(Some(&disk)).unwrap().data(),
        Some(&b"retained"[..])
    );
    assert_eq!(std::fs::metadata(&disk.path).unwrap().len(), 8);
    assert_eq!(prefix.len, 8);
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
    // With the cache active, effective_readahead_secs() inflates to cache_secs.
    // Like mpv, the default is bounded by demuxer_max_bytes (150 MiB) instead of
    // an additional packet-time limit.
    let cached = PlaybackCacheConfig {
        demuxer_readahead_secs: 1.0,
        cache_secs: 3600.0,
        ..PlaybackCacheConfig::default()
    };
    assert_eq!(
        demux_packet_cache_readahead_nsecs(&cached, true),
        seconds_to_nsecs(3600.0)
    );
    assert_eq!(cached.demuxer_max_bytes, 150 * 1024 * 1024);

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
