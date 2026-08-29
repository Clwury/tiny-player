use super::*;

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
fn demux_packet_cache_state_does_not_advance_reader_from_sparse_subtitle_packet() {
    let mut state = DemuxPacketCacheState::new(
        55_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(3, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(55_000_000_000, 56_000_000_000));
    state.append_packet(cached_packet(
        3,
        false,
        Some(153_800_000_000),
        Some(154_120_000_000),
    ));

    let mut timing = DemuxPacketCacheReadTiming::default();
    assert!(
        state
            .take_packet_round_robin(&[3], &mut timing)
            .expect("subtitle packet reads")
            .is_some()
    );

    assert_eq!(state.reader_nsecs, 55_000_000_000);
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
    let cached = CachedDemuxPacket::from_packet(
        &packet,
        0,
        true,
        CachedDemuxPacketRecovery {
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Keyframe,
            safe_seek_point: true,
        },
        Some(0),
        Some(1_000_000_000),
        Some(0),
    )
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
    finish_bounded_read_trim(&mut state);

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
fn demux_packet_cache_state_cached_seek_rewinds_to_first_pgs_packet_at_same_pts() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: None,
        subtitle_stream: Some(stream_info_for_test(
            2,
            ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        )),
    });
    state.append_packet(cached_key_packet(0, true, Some(0), Some(10_000_000_000)));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(10_000_000_000),
        Some(20_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(20_000_000_000),
        Some(30_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(5_000_000_000),
        Some(15_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(15_000_000_000),
        Some(25_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(15_000_000_000),
        Some(25_000_000_000),
    ));
    state.append_packet(cached_packet(
        2,
        false,
        Some(25_000_000_000),
        Some(30_000_000_000),
    ));
    close_seek_range(&mut state, 30_000_000_000);

    assert_eq!(
        state.seek_cached(22_000_000_000, PlaybackSessionId(2)),
        Some(30.0)
    );

    assert_eq!(state.reader_heads.get(&0), Some(&2));
    assert_eq!(
        state.reader_heads.get(&2),
        Some(&4),
        "PGS frame merge must replay every packet from the selected display-set PTS"
    );
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

    let full_refreshes_before = state.reader_tracking_full_refresh_count;
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
    assert_eq!(
        state.reader_tracking_full_refresh_count, full_refreshes_before,
        "single-stream reader realign must not rebuild all reader tracking"
    );
}

#[test]
fn demux_packet_cache_state_realigns_truehd_audio_to_previous_major_sync() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(5_000_000_000),
        Some(6_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        1,
        false,
        Some(4_800_000_000),
        Some(5_000_000_000),
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
        .realign_stream_reader_to_timeline(1, 5_650_000_000, "test_truehd_audio_realign")
        .expect("TrueHD reader finds the previous major-sync packet");

    assert_eq!(result.new_packet_id, 1);
    assert_eq!(result.new_start_nsecs, Some(4_800_000_000));
    assert_eq!(state.next_packet_id_for_stream(1), Some(1));
}

#[test]
fn demux_packet_cache_state_truehd_reader_realigns_backwards_incrementally() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    for index in 0..9_u64 {
        let start_nsecs = index * 1_000_000_000;
        let packet = if index.is_multiple_of(4) {
            cached_key_packet(
                1,
                false,
                Some(start_nsecs),
                Some(start_nsecs + 1_000_000_000),
            )
        } else {
            cached_packet(
                1,
                false,
                Some(start_nsecs),
                Some(start_nsecs + 1_000_000_000),
            )
        };
        state.append_packet(packet);
    }
    state.set_reader_head_for_current_generation(1, 5);
    state.refresh_reader_tracking();
    let forward_bytes_before = state.forward_bytes();
    let full_refreshes_before = state.reader_tracking_full_refresh_count;

    let result = state
        .realign_stream_reader_to_timeline(1, 2_500_000_000, "test_truehd_backward_realign")
        .expect("TrueHD reader realigns to the previous major-sync block");

    assert_eq!(result.old_packet_id, Some(5));
    assert_eq!(result.new_packet_id, 1);
    assert_eq!(state.next_packet_id_for_stream(1), Some(1));
    assert_eq!(state.forward_bytes(), forward_bytes_before + 4 * 1024);
    assert!(state.active_packet_is_forward(1));
    assert_eq!(
        state.reader_tracking_full_refresh_count, full_refreshes_before,
        "backward TrueHD realign updates only the affected stream"
    );
}

#[test]
fn demux_packet_cache_state_rejects_unsafe_truehd_audio_realign() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
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

    assert!(
        state
            .realign_stream_reader_to_timeline(
                1,
                5_650_000_000,
                "test_unsafe_truehd_audio_realign",
            )
            .is_none()
    );
    assert_eq!(state.next_packet_id_for_stream(1), Some(1));
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
    finish_bounded_read_trim(&mut state);

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
    assert_eq!(state.backward_bytes(), 3 * 1024);
    assert!(state.backbuffer_pressure());

    state.append_packet(cached_anchor(6_000_000_000, 7_000_000_000));
    close_seek_range(&mut state, 7_000_000_000);

    assert!(state.backward_bytes() <= state.effective_backbuffer_limit());
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 3.0,
            end: 7.0,
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
fn demux_packet_cache_state_uses_timestamp_span_for_durationless_audio() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 5_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(2_000_000_000), None));
    state.append_packet(cached_packet(1, false, Some(4_000_000_000), None));
    close_seek_range(&mut state, 5_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 4.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_rebuilds_durationless_audio_timestamp_span() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 5_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(3_000_000_000), None));
    close_seek_range(&mut state, 5_000_000_000);

    state.set_stream_kind(1, StreamCacheKind::Audio);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 3.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_truehd_range_advances_only_after_next_major_sync() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_runtime_key_packet(
        1,
        false,
        Some(0),
        Some(500_000_000),
    ));
    state.append_packet(cached_runtime_packet(
        1,
        false,
        Some(1_000_000_000),
        Some(1_500_000_000),
    ));
    state.append_packet(cached_runtime_packet(
        1,
        false,
        Some(2_000_000_000),
        Some(2_500_000_000),
    ));
    close_seek_range(&mut state, 10_000_000_000);

    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty(),
        "an open TrueHD recovery block is not seekable yet"
    );

    state.append_packet(cached_runtime_key_packet(
        1,
        false,
        Some(3_000_000_000),
        Some(3_500_000_000),
    ));
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }]
    );

    state.append_packet(cached_runtime_packet(
        1,
        false,
        Some(4_000_000_000),
        Some(4_500_000_000),
    ));
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 2.0,
        }],
        "non-sync TrueHD packets do not advance the OSC seekable end"
    );

    state.append_packet(cached_runtime_key_packet(
        1,
        false,
        Some(5_000_000_000),
        Some(5_500_000_000),
    ));
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 4.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_truehd_eof_closes_last_major_sync_block() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });
    state.append_packet(cached_anchor(0, 3_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(2_000_000_000), None));

    assert_eq!(state.read_range().stream_boundary(1).seek_end_nsecs, None);

    state.mark_eof();

    assert_eq!(
        state.read_range().stream_boundary(1).seek_start_nsecs,
        Some(0)
    );
    assert_eq!(
        state.read_range().stream_boundary(1).seek_end_nsecs,
        Some(2_000_000_000)
    );
}
