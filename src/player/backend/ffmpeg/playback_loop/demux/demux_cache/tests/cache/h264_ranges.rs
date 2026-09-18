use super::*;

fn append_h264_packet(
    state: &mut DemuxPacketCacheState,
    timeline: &mut DemuxPacketTimeline,
    pts_millis: i64,
    keyframe: bool,
    data: &[u8],
) {
    let mut packet = demux_packet_with_data_for_stream(0, data);
    unsafe {
        (*packet.as_mut_ptr()).pts = pts_millis;
        (*packet.as_mut_ptr()).dts = pts_millis;
        (*packet.as_mut_ptr()).duration = 40;
        (*packet.as_mut_ptr()).flags = if keyframe { ffi::AV_PKT_FLAG_KEY } else { 0 };
    }
    let (event_tx, _event_rx) = mpsc::channel();
    let cached = timeline
        .cache_packet(&packet, &event_tx)
        .expect("H.264 packet caches")
        .expect("video stream is selected");
    state.append_packet(cached);
}

fn h264_cache_at(position_seconds: f64) -> (DemuxPacketCacheState, DemuxPacketTimeline) {
    let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_H264);
    let state = DemuxPacketCacheState::new(
        seconds_to_nsecs(position_seconds),
        0,
        video_stream.codec_id,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let timeline = DemuxPacketTimeline::new(
        video_stream,
        None,
        None,
        position_seconds,
        PlaybackSessionId(1),
    );
    (state, timeline)
}

#[test]
fn h264_non_idr_demux_keyframes_close_seek_ranges_and_allow_cached_seek() {
    for data in [&[0, 0, 0, 1, 0x41, 0xaa][..], &[0, 0, 0, 2, 0x41, 0xaa]] {
        let (mut state, mut timeline) = h264_cache_at(638.0);
        append_h264_packet(&mut state, &mut timeline, 638_000, true, data);
        append_h264_packet(&mut state, &mut timeline, 638_960, false, data);
        assert!(
            state
                .playback_cache_state(false)
                .demux
                .seekable_ranges
                .is_empty()
        );

        append_h264_packet(&mut state, &mut timeline, 639_000, true, data);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 638.0,
                end: 638.96
            }]
        );
        let hit = state
            .seek_cached_with_generation_hit(
                638_500_000_000,
                PlaybackSeekMode::Precise,
                PlaybackSessionId(2),
                1,
            )
            .expect("demux keyframe is a cached seek anchor even without an IDR");
        assert_eq!(hit.anchor_nsecs, 638_000_000_000);
        assert_eq!(hit.anchor_kind, VideoRecoveryPointKind::Keyframe);
        assert!(!hit.anchor_is_safe_seek_point);
        assert!(!hit.anchor_is_recovery_point);

        let mut timing = DemuxPacketCacheReadTiming::default();
        let (packet, _) = state
            .take_packet_round_robin(&[0], &mut timing)
            .expect("cached video reads")
            .expect("anchor is resident")
            .packet_ref(&mut timing)
            .expect("cached packet references");
        assert!(packet.is_key());
        let diagnostic = packet.read_diagnostic().expect("cache read metadata");
        assert!(!diagnostic.recovery_point);
        assert_eq!(diagnostic.recovery_kind, VideoRecoveryPointKind::None);
        assert!(!diagnostic.safe_seek_point);
        assert_eq!(state.low_level_seeks, 0);
    }
}

#[test]
fn h264_seek_range_keeps_advancing_after_last_idr_is_trimmed() {
    let (mut state, mut timeline) = h264_cache_at(635.0);
    for second in 635..=641 {
        let data = if second <= 636 {
            &[0, 0, 0, 2, 0x65, 0xaa]
        } else {
            &[0, 0, 0, 2, 0x41, 0xaa]
        };
        append_h264_packet(&mut state, &mut timeline, second * 1000, true, data);
        append_h264_packet(
            &mut state,
            &mut timeline,
            second * 1000 + 960,
            false,
            &[0, 0, 0, 2, 0x41, 0xaa],
        );
    }
    set_reader_head_for_stream_time(&mut state, 0, 639_000_000_000);
    state.reader_nsecs = 639_000_000_000;

    for (prune_count, expected_start) in [(4, 637.0), (2, 638.0)] {
        state.remove_read_range_stream_prefix_packets_for_test(0, prune_count);
        let expected = vec![PlaybackCacheTimeRange {
            start: expected_start,
            end: 640.96,
        }];
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            expected
        );
        state.refresh_range_seek_boundaries(state.read_range_id);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            expected
        );
        assert!(
            state.read_range().stream_recovery_point_index[&0]
                .values()
                .all(|packet_id| state.packets.contains_key(packet_id))
        );
    }
    append_h264_packet(
        &mut state,
        &mut timeline,
        642_000,
        true,
        &[0, 0, 0, 2, 0x41, 0xaa],
    );
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 638.0,
            end: 641.96
        }]
    );
    assert_every_advertised_range_sample_cached_seeks(state);
}

#[test]
fn h264_eof_closes_non_idr_demux_keyframe_block() {
    let (mut state, mut timeline) = h264_cache_at(638.0);
    let data = &[0, 0, 0, 2, 0x41, 0xaa];
    append_h264_packet(&mut state, &mut timeline, 638_000, true, data);
    append_h264_packet(&mut state, &mut timeline, 638_960, false, data);
    state.set_range_eof(state.read_range_id, true);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 638.0,
            end: 638.96
        }]
    );
    assert_every_advertised_range_sample_cached_seeks(state);
}

#[test]
fn h264_non_key_non_idr_packets_do_not_advertise_cached_seek_range() {
    let (mut state, mut timeline) = h264_cache_at(638.0);
    let data = &[0, 0, 0, 2, 0x41, 0xaa];
    append_h264_packet(&mut state, &mut timeline, 638_000, false, data);
    append_h264_packet(&mut state, &mut timeline, 638_960, false, data);
    state.set_range_eof(state.read_range_id, true);
    assert!(
        state
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .is_empty()
    );
    assert!(
        state
            .resolve_cached_seek_plan_attempt(638_500_000_000, PlaybackSeekMode::Precise, false)
            .is_err()
    );
}
