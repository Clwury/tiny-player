use super::*;

fn subtitle_cache() -> DemuxPacketCacheState {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: None,
        subtitle_stream: Some(stream_info_for_test(3, ffi::AVCodecID::AV_CODEC_ID_SUBRIP)),
    });
    for second in 0..20 {
        let start = second * 1_000_000_000;
        state.append_packet(cached_anchor(start, start + 1_000_000_000));
        if second % 4 == 2 {
            state.append_packet(cached_key_packet(
                3,
                false,
                Some(start),
                Some(start + 1_000_000_000),
            ));
        }
    }
    close_seek_range(&mut state, 20_000_000_000);
    state
}

#[test]
fn large_cache_trim_preserves_subtitles_for_repeated_forward_seeks() {
    let mut config = cache_config_for_test();
    config.demuxer_max_bytes = 64 * 1024 * 1024;
    config.demuxer_max_back_bytes = 16 * 1024 * 1024;
    config.demuxer_donate_buffer = false;
    let mut state = DemuxPacketCacheState::new(
        873_000_000_000,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: None,
        subtitle_stream: Some(stream_info_for_test(3, ffi::AVCodecID::AV_CODEC_ID_SUBRIP)),
    });
    // Match the failure's 44 prefetched subtitles spanning 14:47 to 17:33.
    let subtitle_starts = (0..44_u64)
        .map(|index| 887_053_000_000 + 166_792_000_000 * index / 43)
        .collect::<Vec<_>>();
    let mut next_subtitle = 0;
    for frame in 0..5000_u64 {
        let start = 873_000_000_000 + frame * 40_000_000;
        state.append_packet(cached_packet_with_keyframe(
            0,
            true,
            frame.is_multiple_of(50),
            Some(start),
            Some(start + 40_000_000),
        ));
        if let Some(&subtitle_start) = subtitle_starts.get(next_subtitle)
            && subtitle_start <= start
        {
            state.append_packet(cached_key_packet(
                3,
                false,
                Some(subtitle_start),
                Some(subtitle_start + 3_800_000_000),
            ));
            next_subtitle += 1;
        }
    }
    close_seek_range(&mut state, 1_073_000_000_000);
    assert_eq!(next_subtitle, 44);

    set_reader_head_for_stream_time(&mut state, 0, 934_000_000_000);
    let mut timing = DemuxPacketCacheReadTiming::default();
    let mut decoded_subtitles = 0;
    while state
        .take_packet_round_robin_with_trim(&[3], &mut timing, false)
        .unwrap()
        .is_some()
    {
        decoded_subtitles += 1;
    }
    assert_eq!(decoded_subtitles, 44);
    assert_eq!(state.next_packet_id_for_stream(3), None);
    // Memory pressure enters the large-queue trim path after subtitle predecode.
    state.backbuffer_limit_bytes = state.backward_bytes() - 600 * 1024;
    assert!(state.trim_to_limit());
    assert!(!state.backbuffer_pressure());

    for (index, second) in (940..=990).step_by(5).enumerate() {
        let target = second * 1_000_000_000 + 273_000_000;
        let hit = state
            .seek_cached_with_generation_hit(
                target,
                PlaybackSeekMode::Precise,
                PlaybackSessionId(index as u64 + 2),
                index as u64 + 1,
            )
            .expect("forward seek stays cached with its subtitle packets");
        let (_, actual_start, actual_end) = state.stream_reader_head_timeline(3).unwrap();
        let expected_start = subtitle_starts
            .iter()
            .copied()
            .rev()
            .find(|start| *start <= hit.anchor_nsecs)
            .unwrap();
        assert_eq!(
            actual_start,
            Some(expected_start),
            "subtitle for seek to {second}"
        );
        assert!(
            state
                .take_packet_round_robin_with_trim(&[3], &mut timing, false)
                .unwrap()
                .is_some()
        );
        if second == 990 {
            assert!(expected_start <= target && target < actual_end.unwrap());
        }
    }
}

#[test]
fn batched_subtitle_pruning_advances_seek_start_like_individual_mpv_runs() {
    for count in [2, 3, 5] {
        let mut batched = subtitle_cache();
        let mut individual = subtitle_cache();
        batched.remove_read_range_stream_prefix_packets_for_test(3, count);
        for _ in 0..count {
            individual.remove_read_range_stream_prefix_packets_for_test(3, 1);
        }
        assert_eq!(
            batched.read_range().stream_boundary(3).last_pruned_nsecs,
            individual.read_range().stream_boundary(3).last_pruned_nsecs,
            "every removed subtitle boundary must be reflected after deleting {count} packets"
        );
        assert_eq!(
            batched.playback_cache_state(false).demux.seekable_ranges,
            individual.playback_cache_state(false).demux.seekable_ranges,
        );
    }
}

#[test]
fn seek_into_pruned_subtitles_requests_a_low_level_seek() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let shared = Arc::new(shared_for_test(control));
    {
        let mut state = shared.state.lock().unwrap();
        *state = subtitle_cache();
        assert!(state.playback_cache_state(false).demux.bof_cached);
        state.remove_read_range_stream_prefix_packets_for_test(3, 3);
        assert!(!state.playback_cache_state(false).demux.bof_cached);
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 10.1,
                end: 20.0
            }]
        );
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    assert_eq!(
        cache.seek(8.0, PlaybackSeekMode::Precise, PlaybackSessionId(2), 1),
        DemuxSeekResult::Requested
    );
    assert_eq!(shared.state.lock().unwrap().low_level_seeks, 1);
}

#[test]
fn subtitle_gaps_and_future_first_cues_do_not_reject_valid_cached_seeks() {
    let mut state = subtitle_cache();
    // Like mpv, a naturally sparse stream may start after the seek target.
    let hit = state
        .seek_cached_with_generation_hit(
            1_000_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(2),
            1,
        )
        .unwrap();
    assert_eq!(hit.target_nsecs, 1_000_000_000);
    assert_eq!(
        state.stream_reader_head_timeline(3).unwrap().1,
        Some(2_000_000_000)
    );
    state.remove_read_range_stream_prefix_packets_for_test(3, 1);
    // Once the removed cue has ended, the following gap remains seekable.
    assert!(
        state
            .seek_cached(4_000_000_000, PlaybackSessionId(3))
            .is_some()
    );
    assert_eq!(
        state.stream_reader_head_timeline(3).unwrap().1,
        Some(6_000_000_000)
    );
}
