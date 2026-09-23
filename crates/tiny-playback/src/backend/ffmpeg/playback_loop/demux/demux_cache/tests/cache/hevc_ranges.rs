use super::*;

#[test]
fn hevc_cached_seek_at_649_ignores_non_key_recovery_packets() {
    let mut state = DemuxPacketCacheState::new(
        407_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        407_198_000_000,
        407_240_000_000,
    ));
    let mut non_key = cached_video_recovery_packet(
        VideoRecoveryPointKind::Bla,
        false,
        407_240_000_000,
        407_282_000_000,
    );
    non_key.demux_keyframe = false;
    state.append_packet(non_key);
    state.append_packet(cached_packet(
        0,
        true,
        Some(409_117_000_000),
        Some(409_159_000_000),
    ));
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        419_502_000_000,
        419_544_000_000,
    ));
    let hit = state
        .seek_cached_with_generation_hit(
            409_108_164_263,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("cached CRA seek hits");
    assert_eq!(hit.anchor_kind, VideoRecoveryPointKind::Cra);
    assert_eq!(hit.anchor_nsecs, 407_198_000_000);
    assert_eq!(hit.target_nsecs, 409_108_164_263);
}

#[test]
fn demux_packet_cache_state_seeks_from_nearest_previous_keyframe() {
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

    assert_eq!(
        state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_uses_safe_point_before_preroll_target() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
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

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("cached seek hits");
    assert_eq!(hit.buffered_until_nsecs, 4_000_000_000);
    assert_eq!(hit.target_nsecs, 3_500_000_000);
    assert_eq!(hit.anchor_nsecs, 2_000_000_000);
    assert_eq!(hit.anchor_packet_id, 2);
    assert_eq!(hit.video_reader_head, 2);
    assert!(hit.anchor_is_recovery_point);
    assert!(hit.anchor_is_safe_seek_point);
    assert!(hit.requires_precise_trim);
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
    assert_eq!(state.low_level_seeks, 0);
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_uses_closed_cra_recovery_anchor() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let recovery_only_packet = cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        2_000_000_000,
        3_000_000_000,
    );
    state.append_packet(recovery_only_packet);
    state.append_packet(cached_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("closed CRA interval supports cached seek");
    assert_eq!(hit.anchor_kind, VideoRecoveryPointKind::Cra);
    assert_eq!(hit.anchor_nsecs, 2_000_000_000);
    assert_eq!(hit.target_nsecs, 3_500_000_000);
    assert_eq!(hit.preroll_nsecs, 500_000_000);
    assert!(!hit.anchor_is_safe_seek_point);
    assert!(hit.requires_precise_trim);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
}

#[test]
fn failed_cra_anchor_is_excluded_from_ranges_and_future_cached_seeks() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        2_000_000_000,
        3_000_000_000,
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("CRA cached seek initially hits");
    let failed = DemuxCachedSeekInfo {
        range_id: hit.range_id,
        target_nsecs: hit.target_nsecs,
        anchor_nsecs: hit.anchor_nsecs,
        preroll_nsecs: hit.preroll_nsecs,
        anchor_packet_id: hit.anchor_packet_id,
        anchor_kind: hit.anchor_kind,
        anchor_is_safe_seek_point: hit.anchor_is_safe_seek_point,
        requires_precise_trim: hit.requires_precise_trim,
    };

    assert!(state.exclude_failed_cached_seek_range(failed));
    assert_eq!(state.failed_cached_seek_range(hit.range_id), Some(failed));
    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );
    assert!(!state.exclude_failed_cached_seek_range(failed));
    assert!(
        state
            .seek_cached_with_generation_hit(
                3_500_000_000,
                PlaybackSeekMode::Precise,
                PlaybackSessionId(3),
                1,
            )
            .is_none(),
        "a failed CRA interval cannot loop back into cached seek"
    );
}

#[test]
fn demux_packet_cache_state_precise_hevc_cached_seek_uses_latest_safe_point_before_effective_target()
 {
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
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(3_200_000_000),
        Some(4_000_000_000),
    ));
    close_seek_range(&mut state, 4_000_000_000);

    let hit = state
        .seek_cached_with_generation_hit(
            3_500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            0,
        )
        .expect("cached seek hits");
    assert_eq!(hit.anchor_nsecs, 2_000_000_000);
    assert_eq!(hit.anchor_packet_id, 0);
    assert!(hit.anchor_is_safe_seek_point);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
}

#[test]
fn hevc_cached_seek_uses_nearest_recovery_point_with_required_preroll() {
    for safe_kind in [VideoRecoveryPointKind::Idr, VideoRecoveryPointKind::Bla] {
        let mut state = DemuxPacketCacheState::new(
            0,
            0,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            PlaybackSessionId(1),
            cache_config_for_test(),
        );
        state.append_packet(cached_video_recovery_packet(
            safe_kind,
            true,
            0,
            1_000_000_000,
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(1_000_000_000),
            Some(2_000_000_000),
        ));
        state.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            2_000_000_000,
            3_000_000_000,
        ));
        state.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));
        state.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            4_000_000_000,
            5_000_000_000,
        ));

        for (target, mode, expected_anchor) in [
            (3_500_000_000, PlaybackSeekMode::Precise, 2_000_000_000),
            (2_400_000_000, PlaybackSeekMode::Precise, 0),
            (2_500_000_000, PlaybackSeekMode::Precise, 2_000_000_000),
            (2_400_000_000, PlaybackSeekMode::Fast, 2_000_000_000),
        ] {
            let hit = state
                .seek_cached_with_generation_hit(target, mode, PlaybackSessionId(2), 0)
                .expect("closed HEVC range supports cached seek");
            assert_eq!(hit.anchor_nsecs, expected_anchor);
            assert_eq!(hit.anchor_is_safe_seek_point, expected_anchor == 0);
            assert_eq!(
                hit.anchor_kind,
                if expected_anchor == 0 {
                    safe_kind
                } else {
                    VideoRecoveryPointKind::Cra
                }
            );
        }
    }
}

#[test]
fn hevc_cached_seek_at_94_seconds_uses_86_second_cra_instead_of_initial_idr() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AC3)),
        subtitle_stream: None,
    });
    let anchors = [0, 56_320, 66_320, 76_320, 86_320, 96_320];
    for (index, start_ms) in anchors.into_iter().enumerate() {
        state.append_packet(cached_video_recovery_packet(
            if index == 0 {
                VideoRecoveryPointKind::Idr
            } else {
                VideoRecoveryPointKind::Cra
            },
            index == 0,
            start_ms * 1_000_000,
            (start_ms + 20) * 1_000_000,
        ));
        if let Some(next_ms) = anchors.get(index + 1) {
            state.append_packet(cached_packet(
                0,
                true,
                Some((next_ms - 100) * 1_000_000),
                Some((next_ms - 80) * 1_000_000),
            ));
        }
    }
    for start_ms in (0..97_000).step_by(32) {
        state.append_packet(cached_packet(
            1,
            false,
            Some(start_ms * 1_000_000),
            Some((start_ms + 32) * 1_000_000),
        ));
    }

    let hit = state
        .seek_cached_with_generation_hit(
            93_886_876_244,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(4),
            3,
        )
        .expect("recorded target lies in a closed cached interval");

    assert_eq!(hit.anchor_kind, VideoRecoveryPointKind::Cra);
    assert_eq!(hit.anchor_nsecs, 86_320_000_000);
    assert_eq!(hit.target_nsecs - hit.anchor_nsecs, 7_566_876_244);
    assert!(hit.requires_precise_trim);
    assert_eq!(
        state.stream_reader_head_timeline(1).unwrap().1,
        Some(86_304_000_000)
    );
    assert_eq!(state.low_level_seeks, 0);
}

#[test]
fn demux_packet_cache_state_hits_hevc_cached_seek_from_first_safe_point_after_preroll() {
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

    assert_eq!(
        state.seek_cached(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
}

#[test]
fn demux_packet_cache_state_fast_hevc_cached_seek_uses_nearest_recovery_point() {
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

    assert_eq!(
        state.seek_cached_fast(3_500_000_000, PlaybackSessionId(2)),
        Some(4.0)
    );
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 2_000_000_000);
    assert_eq!(state.cached_seeks, 1);
}

#[test]
fn demux_packet_cache_state_hits_hevc_cached_seek_with_short_recovery_window() {
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
        state.seek_cached(7_500_000_000, PlaybackSessionId(2)),
        Some(8.0)
    );
    assert_eq!(state.read_index, 2);
    assert_eq!(state.reader_nsecs, 6_000_000_000);
}

#[test]
fn demux_packet_cache_state_requires_previous_keyframe() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));

    assert_eq!(state.seek_cached(1_500_000_000, PlaybackSessionId(2)), None);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 0);
}

#[test]
fn demux_packet_cache_state_requires_previous_cached_seek_anchor() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let mut key_packet = cached_packet(0, true, Some(0), Some(1_000_000_000));
    key_packet.safe_seek_point = true;
    state.append_packet(key_packet);
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));

    assert_eq!(state.seek_cached(1_500_000_000, PlaybackSessionId(2)), None);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 0);
}

#[test]
fn demux_packet_cache_state_reports_seekable_range_after_first_recovery_point() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_packet(0, true, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(2_000_000_000),
        Some(3_000_000_000),
    ));
    close_seek_range(&mut state, 3_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 1.0,
            end: 3.0,
        }]
    );
    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(2)),
        Some(3.0)
    );
    assert_eq!(
        state.seek_cached(1_500_000_000, PlaybackSessionId(2)),
        Some(3.0)
    );
    assert_eq!(state.read_index, 1);
}

#[test]
fn demux_packet_cache_state_reports_hevc_seekable_range_after_cached_seek_preroll() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
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
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 2.0,
        }]
    );
}

#[test]
fn hevc_cra_only_range_closes_when_second_cra_arrives() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        0,
        1_000_000_000,
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));

    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty(),
        "an open CRA segment is not yet a no-delay cached seek interval"
    );

    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        2_000_000_000,
        3_000_000_000,
    ));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 2.0,
        }]
    );
}

#[test]
fn hevc_single_unclosed_cra_does_not_publish_seekable_range() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        0,
        1_000_000_000,
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));

    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );
}

#[test]
fn hevc_eof_closes_last_cra_seekable_range() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_video_recovery_packet(
        VideoRecoveryPointKind::Cra,
        false,
        0,
        1_000_000_000,
    ));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.mark_eof();

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 2.0,
        }]
    );
}

#[test]
fn demux_packet_cache_hevc_prefix_trim_preserves_confirmed_tail_after_short_block() {
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

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.5,
            end: 1.54,
        }]
    );

    state.remove_read_range_stream_prefix_packets_for_test(0, 1);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.6,
            end: 1.54,
        }],
        "prefix trim updates the first retained HEVC seek point without discarding the confirmed tail"
    );
    assert!(
        state
            .seek_cached(1_000_000_000, PlaybackSessionId(2))
            .is_some(),
        "the reported post-trim range remains usable for cached seek"
    );
}

#[test]
fn demux_packet_cache_hevc_prefix_trim_repairs_missing_boundary_index() {
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
    state.read_range_mut().stream_seek_boundaries.remove(&0);

    state.remove_read_range_stream_prefix_packets_for_test(0, 1);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.6,
            end: 1.54,
        }]
    );
    assert_eq!(
        state
            .read_range()
            .stream_seek_boundaries
            .get(&0)
            .and_then(|boundaries| boundaries.front())
            .and_then(|packet_id| state.packets.get(packet_id))
            .and_then(|packet| packet.start_nsecs),
        Some(100_000_000),
        "prefix trim rebuilds a stale HEVC safe-point index from retained packets"
    );
}

#[test]
fn demux_packet_cache_hevc_append_keeps_prefix_trim_start_stable() {
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

    state.append_packet(cached_packet(
        0,
        true,
        Some(2_500_000_000),
        Some(2_540_000_000),
    ));
    state.append_packet(cached_key_packet(
        0,
        true,
        Some(3_000_000_000),
        Some(3_040_000_000),
    ));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.6,
            end: 2.54,
        }],
        "tail append may extend the confirmed end but must not replace the retained head start"
    );
}

#[test]
fn demux_packet_cache_hevc_prefix_trim_clears_range_without_usable_safe_point() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    append_hevc_prefix_trim_regression_packets(&mut state);
    set_reader_head_for_stream_time(&mut state, 0, 2_000_000_000);
    state.reader_nsecs = 2_000_000_000;

    state.remove_read_range_stream_prefix_packets_for_test(0, 5);

    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty(),
        "a retained HEVC safe point after the previously confirmed end is not reported as seekable"
    );
}

#[test]
fn demux_packet_cache_emits_stable_hevc_range_across_prefix_trim() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_codec_and_config_for_test(
        control,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        cache_config_for_test(),
    );
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        append_hevc_prefix_trim_regression_packets(&mut guard);
        set_reader_head_for_stream_time(&mut guard, 0, 1_500_000_000);
        guard.reader_nsecs = 1_500_000_000;
        guard.remove_read_range_stream_prefix_packets_for_test(0, 1);
        let _ = guard.cache_report_snapshot(false);
        shared.emit_cache_state_after_read(&mut guard, true);

        guard.append_packet(cached_packet(
            0,
            true,
            Some(2_500_000_000),
            Some(2_540_000_000),
        ));
        guard.append_packet(cached_key_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(3_040_000_000),
        ));
        let _ = guard.cache_report_snapshot(false);
        shared.emit_cache_state_after_read(&mut guard, true);
    }

    let emitted_ranges = event_rx
        .try_iter()
        .filter_map(|event| match event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state.demux.seekable_ranges),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        emitted_ranges,
        vec![
            vec![PlaybackCacheTimeRange {
                start: 0.6,
                end: 1.54,
            }],
            vec![PlaybackCacheTimeRange {
                start: 0.6,
                end: 2.54,
            }],
        ],
        "OSC cache events must not expose an empty or future-only range between prefix trim and tail growth"
    );
}

#[test]
fn demux_packet_cache_state_reports_full_active_seekable_range() {
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
    state.set_read_index_for_test(2);
    state.reader_nsecs = 2_000_000_000;

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 3.0,
        }]
    );
}
