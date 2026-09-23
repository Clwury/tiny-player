use super::*;

#[test]
fn demux_packet_cache_state_mlp_range_uses_major_sync_blocks() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_MLP)),
        subtitle_stream: None,
    });
    state.append_packet(cached_anchor(0, 5_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(1_000_000_000), None));
    close_seek_range(&mut state, 5_000_000_000);
    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );

    state.append_packet(cached_key_packet(1, false, Some(2_000_000_000), None));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_rebuilds_truehd_boundaries_from_major_sync_packets() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 5_000_000_000));
    state.append_packet(cached_key_packet(1, false, Some(0), None));
    state.append_packet(cached_packet(1, false, Some(1_000_000_000), None));
    state.append_packet(cached_key_packet(1, false, Some(2_000_000_000), None));
    close_seek_range(&mut state, 5_000_000_000);

    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD)),
        subtitle_stream: None,
    });

    assert_eq!(
        state
            .read_range()
            .stream_seek_boundaries
            .get(&1)
            .map(|boundaries| boundaries.iter().copied().collect::<Vec<_>>()),
        Some(vec![1, 3])
    );
    assert_eq!(
        state.read_range().stream_boundary(1).seek_end_nsecs,
        Some(1_000_000_000)
    );
}

#[test]
fn demux_packet_cache_state_uses_per_stream_bof_for_seekable_start() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(500_000_000, 1_000_000_000));
    state.append_packet(cached_packet(
        1,
        false,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    let mut timing = DemuxPacketCacheReadTiming::default();
    assert!(
        state
            .take_packet_round_robin(&[1], &mut timing)
            .expect("audio packet reads")
            .is_some()
    );

    let cache_state = state.playback_cache_state(false);
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
            end: 2.0,
        }]
    );
    assert!(!cache_state.demux.bof_cached);
    assert_eq!(state.seek_cached(750_000_000, PlaybackSessionId(2)), None);
    assert_eq!(
        state.seek_cached(1_500_000_000, PlaybackSessionId(3)),
        Some(2.0)
    );
}

#[test]
fn demux_packet_cache_state_uses_per_stream_eof_for_seekable_end() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(8_000_000_000)));
    state.mark_eof();
    state.read_range_mut().ensure_stream_boundary(1).is_eof = false;

    let cache_state = state.playback_cache_state(false);
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 8.0,
        }]
    );
    assert!(!cache_state.demux.eof_cached);
    assert_eq!(state.seek_cached(9_000_000_000, PlaybackSessionId(2)), None);
    assert_eq!(
        state.seek_cached(7_000_000_000, PlaybackSessionId(3)),
        Some(8.0)
    );
}

#[test]
fn demux_packet_cache_state_does_not_split_seekable_range_at_audio_gap() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 5_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        1,
        false,
        Some(3_000_000_000),
        Some(5_000_000_000),
    ));
    state.append_packet(cached_anchor(5_000_000_000, 6_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 5.0,
        }]
    );
    assert_eq!(
        state.seek_cached(4_000_000_000, PlaybackSessionId(3)),
        Some(5.0)
    );
}

#[test]
fn demux_packet_cache_state_limits_seekable_range_to_audio_eager_end() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 12_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(10_000_000_000)));
    state.append_packet(cached_anchor(12_000_000_000, 13_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 10.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_limits_eof_seekable_range_to_common_av_coverage() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 10_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(12_000_000_000)));
    state.mark_eof();

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 10.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_does_not_shorten_seekable_end_for_subtitle_gaps() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(0, 12_000_000_000));
    state.append_packet(cached_packet(2, false, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        2,
        false,
        Some(10_000_000_000),
        Some(11_000_000_000),
    ));
    state.append_packet(cached_anchor(12_000_000_000, 13_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 12.0,
        }]
    );
}

#[test]
fn demux_packet_cache_state_omits_seekable_range_without_recovery_point() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_packet(0, true, Some(0), Some(12_000_000_000)));

    let cache_state = state.playback_cache_state(false);
    assert_eq!(cache_state.demux.cache_end, Some(12.0));
    assert!(cache_state.demux.seekable_ranges.is_empty());
}

#[test]
fn demux_packet_cache_state_does_not_split_one_physical_range_at_timeline_gaps() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(4_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(4_000_000_000),
        Some(5_000_000_000),
    ));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 4.0,
        }]
    );
    assert!(
        state
            .seek_cached(2_000_000_000, PlaybackSessionId(2))
            .is_some()
    );
}

#[test]
fn demux_packet_cache_hevc_b_frame_pts_reordering_stays_in_one_physical_range() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let mut first_cra =
        cached_runtime_packet_with_keyframe(0, true, true, Some(0), Some(33_333_333), Some(0));
    first_cra.recovery_kind = VideoRecoveryPointKind::Cra;
    first_cra.safe_seek_point = false;
    state.append_packet(first_cra);
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        false,
        Some(33_333_333),
        Some(66_666_666),
        Some(1_000_000_000),
    ));
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        false,
        Some(66_666_666),
        Some(99_999_999),
        Some(500_000_000),
    ));
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        false,
        Some(99_999_999),
        Some(133_333_332),
        Some(1_500_000_000),
    ));
    let mut next_cra = cached_runtime_packet_with_keyframe(
        0,
        true,
        true,
        Some(133_333_332),
        Some(166_666_665),
        Some(2_000_000_000),
    );
    next_cra.recovery_kind = VideoRecoveryPointKind::Cra;
    next_cra.safe_seek_point = false;
    state.append_packet(next_cra);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 1.5,
        }]
    );
    assert!(
        state
            .seek_cached(750_000_000, PlaybackSessionId(2))
            .is_some(),
        "presentation-order reordering remains cached-seekable"
    );
}

#[test]
fn demux_packet_cache_runtime_recovery_end_uses_pts_not_packet_duration() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_runtime_key_packet(
        0,
        true,
        Some(0),
        Some(500_000_000),
    ));
    state.append_packet(cached_runtime_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(1_500_000_000),
    ));
    state.append_packet(cached_runtime_key_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(2_500_000_000),
    ));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0,
        }]
    );
}

#[test]
fn demux_packet_cache_missing_pts_and_dts_does_not_extend_osc_range() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_runtime_key_packet(
        0,
        true,
        Some(0),
        Some(33_333_333),
    ));
    state.append_packet(cached_runtime_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(1_033_333_333),
    ));
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        false,
        Some(2_000_000_000),
        Some(50_000_000_000),
        None,
    ));
    state.append_packet(cached_runtime_key_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(3_033_333_333),
    ));

    let cache_state = state.playback_cache_state(false);
    assert_eq!(cache_state.demux.cache_end, Some(50.0));
    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0,
        }]
    );
}

#[test]
fn demux_packet_cache_reports_rounding_gaps_without_splitting_the_physical_range() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(100)));
    state.append_packet(cached_packet(0, true, Some(102), Some(200)));
    state.append_packet(cached_packet(0, true, Some(203), Some(300)));
    state.append_packet(cached_packet(0, true, Some(304), Some(400)));
    state.append_packet(cached_packet(0, true, Some(405), Some(500)));
    close_seek_range(&mut state, 500);

    let snapshot = state.cache_report_snapshot(false);
    assert_eq!(
        snapshot.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 0.000_000_5,
        }]
    );
    assert_eq!(snapshot.validation.rounding_gaps_merged, 4);
    assert_eq!(snapshot.validation.internal_packet_timestamp_holes, 0);

    let mut seek_generation = 1;
    for target_nsecs in 0..=500 {
        let hit = state.seek_cached_with_generation_hit(
            target_nsecs,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(seek_generation),
            seek_generation,
        );
        assert!(
            hit.is_some(),
            "normalized target {target_nsecs}ns must cached-seek"
        );
        seek_generation = seek_generation.saturating_add(1);
    }
}

#[test]
fn demux_packet_cache_keeps_thirty_three_millisecond_hole_inside_mpv_envelope() {
    const REAL_GAP_NSECS: u64 = 33_333_336;
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000 + REAL_GAP_NSECS),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    close_seek_range(&mut state, 3_000_000_000);

    let snapshot = state.cache_report_snapshot(false);
    assert_eq!(
        snapshot.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 3.0,
        }]
    );
    assert_eq!(snapshot.validation.rounding_gaps_merged, 0);
    assert_eq!(snapshot.validation.internal_packet_timestamp_holes, 1);
    assert!(
        state
            .seek_cached(1_000_000_000 + REAL_GAP_NSECS / 2, PlaybackSessionId(2),)
            .is_some(),
        "a target between discrete frame timestamps remains an in-cache seek"
    );
}

#[test]
fn demux_packet_cache_ninety_second_report_uses_incremental_boundaries_without_packet_scan() {
    const FRAME_COUNT: u64 = 90 * 30;
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
    for frame in 0..FRAME_COUNT {
        let start_nsecs = frame.saturating_mul(1_000_000_000) / 30;
        let end_nsecs = frame.saturating_add(1).saturating_mul(1_000_000_000) / 30;
        if frame == 0 {
            state.append_packet(cached_key_packet(
                0,
                true,
                Some(start_nsecs),
                Some(end_nsecs),
            ));
        } else {
            state.append_packet(cached_packet(0, true, Some(start_nsecs), Some(end_nsecs)));
        }
        state.append_packet(cached_packet(1, false, Some(start_nsecs), Some(end_nsecs)));
    }
    close_seek_range(&mut state, 90_000_000_000);

    let first = state.cache_report_snapshot(false);
    assert_eq!(first.validation.validation_packets, 0);
    assert_eq!(first.validation.validation_probes, 0);
    assert_eq!(first.validation.internal_packet_timestamp_holes, 0);
    assert_eq!(
        first.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 90.0,
        }]
    );

    let repeated = state.cache_report_snapshot(false);
    assert_eq!(repeated.demux.seekable_ranges, first.demux.seekable_ranges);
    assert_eq!(repeated.validation.validation_packets, 0);
    assert_eq!(repeated.validation.validation_probes, 0);
}

#[test]
fn demux_packet_cache_consumer_snapshot_never_validates_dirty_packets() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    let prepared = state.cache_report_snapshot(false);
    let prepared_revision = prepared.seekability_revision;

    state.append_packet(cached_anchor(2_000_000_000, 3_000_000_000));
    assert!(state.seekability_revision() > prepared_revision);
    let consumer = state
        .cache_report_snapshot_from_prepared(false)
        .expect("current generation retains a prepared snapshot");

    assert_eq!(consumer.seekability_revision, prepared_revision);
    assert_eq!(consumer.validation.validation_packets, 0);
    assert_eq!(consumer.validation.validation_probes, 0);
    assert_eq!(
        consumer.demux.seekable_ranges,
        prepared.demux.seekable_ranges
    );
}

#[test]
fn prefix_trim_keeps_every_advertised_target_cached_seekable() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    append_hevc_prefix_trim_regression_packets(&mut state);
    set_reader_head_for_stream_time(&mut state, 0, 1_500_000_000);
    state.reader_nsecs = 1_500_000_000;
    state.remove_read_range_stream_prefix_packets_for_test(0, 1);

    assert_every_advertised_range_sample_cached_seeks(state);
}

#[test]
fn generation_block_keeps_every_remaining_advertised_target_cached_seekable() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);
    state
        .low_level_append_blocked_packet_generations
        .insert(1, state.generation);
    state.read_range().mark_seekable_dirty();

    assert_every_advertised_range_sample_cached_seeks(state);
}

#[test]
fn advertised_range_does_not_require_a_packet_covering_every_target_timestamp() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    close_seek_range(&mut state, 3_000_000_000);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 3.0,
        }]
    );

    state.packets.remove(&1);
    let hit = state
        .seek_cached_with_generation_attempt(
            1_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            1,
        )
        .expect("mpv-style envelope seek does not require a covering packet");

    assert_eq!(hit.range_id, state.read_range_id);
    assert!(state.rejected_cached_seek_ranges.is_empty());
    assert!(
        !state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );
}

#[test]
fn advertised_range_generation_block_records_exact_rejection_reason() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    close_seek_range(&mut state, 3_000_000_000);
    assert!(
        !state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );

    state
        .low_level_append_blocked_packet_generations
        .insert(0, state.generation.saturating_add(1));
    let miss = state
        .seek_cached_with_generation_attempt(
            1_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            1,
        )
        .expect_err("next-generation block invalidates stale advertised target");

    assert_eq!(miss.reason, CachedSeekMissReason::GenerationBlocked);
    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );
}

#[test]
fn demux_packet_cache_state_reports_hevc_seekable_range_after_cached_preroll_from_first_anchor() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(4_000_000_000),
        Some(5_000_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(5_000_000_000),
        Some(6_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(6_000_000_000),
        Some(7_000_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(7_000_000_000),
        Some(8_000_000_000),
    ));
    close_seek_range(&mut state, 8_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 4.5,
            end: 8.0,
        }]
    );
    assert_eq!(
        state.seek_cached(7_500_000_000, PlaybackSessionId(2)),
        Some(8.0)
    );
}
