use super::*;

fn packet(
    stream: c_int,
    pts: i64,
    dts: Option<i64>,
    pos: Option<i64>,
    key: bool,
) -> CachedDemuxPacket {
    let mut packet = demux_packet_with_data_for_stream(stream, &[1; 16]);
    unsafe {
        (*packet.as_mut_ptr()).pts = pts;
        (*packet.as_mut_ptr()).dts = dts.unwrap_or(ffi::AV_NOPTS_VALUE);
        (*packet.as_mut_ptr()).pos = pos.unwrap_or(-1);
        (*packet.as_mut_ptr()).flags = if key { ffi::AV_PKT_FLAG_KEY } else { 0 };
    }
    let start = u64::try_from(pts).unwrap() * 1_000_000_000;
    CachedDemuxPacket::from_packet(
        &packet,
        stream,
        stream == 0,
        CachedDemuxPacketRecovery {
            recovery_point: key,
            recovery_kind: if key {
                VideoRecoveryPointKind::Keyframe
            } else {
                VideoRecoveryPointKind::None
            },
            safe_seek_point: key,
        },
        Some(start),
        Some(start + 1_000_000_000),
        Some(start),
    )
    .unwrap()
}

fn video(second: i64) -> CachedDemuxPacket {
    packet(0, second, Some(second), Some(second * 100), true)
}

fn fixture() -> (Arc<DemuxPacketCacheShared>, DemuxPacketCache) {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let (shared, _) = shared_with_config_for_test(
        control,
        PlaybackCacheConfig {
            cache_pause: false,
            ..cache_config_for_test()
        },
    );
    let shared = Arc::new(shared);
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    (shared, cache)
}

fn archive_and_seek(state: &mut DemuxPacketCacheState) {
    state.request_seek(100.0, PlaybackSessionId(2), 1, 100_000_000_000);
    for second in 100..103 {
        state.append_packet(video(second));
    }
    assert!(
        state
            .seek_cached(500_000_000, PlaybackSessionId(3))
            .is_some()
    );
    state.take_seek_request().expect("cached range resume seek");
}

fn read_video(cache: &DemuxPacketCache) -> Vec<u64> {
    let mut starts = Vec::new();
    for _ in 0..32 {
        let DemuxReadResult::Packet(packet) = cache.poll_packet(0) else {
            break;
        };
        starts.push(
            packet
                .read_diagnostic()
                .unwrap()
                .packet_start_nsecs
                .unwrap()
                / 1_000_000_000,
        );
    }
    starts
}

#[test]
fn cached_video_continues_while_old_audio_is_backpressured() {
    let (shared, cache) = fixture();
    let (range_id, audio_head) = {
        let mut state = shared.state.lock().unwrap();
        state.set_stream_kind(1, StreamCacheKind::Audio);
        for second in 0..4 {
            state.append_packet(video(second));
        }
        for second in 0..8 {
            state.append_packet(packet(1, second, Some(second), Some(second * 100), true));
        }
        archive_and_seek(&mut state);
        let audio_head = state.reader_heads[&1];
        state.append_packet(video(4));
        (state.read_range_id, audio_head)
    };
    assert_eq!(read_video(&cache), vec![0, 1, 2, 3, 4]);
    let state = shared.state.lock().unwrap();
    assert_eq!(state.read_range_id, range_id);
    assert_eq!(state.reader_heads[&1], audio_head);
}

#[test]
fn cached_resume_skips_each_stream_tail_beyond_seekable_end() {
    let (shared, cache) = fixture();
    {
        let mut state = shared.state.lock().unwrap();
        for second in 0..5 {
            state.append_packet(packet(
                0,
                second,
                Some(second),
                Some(second * 100),
                second < 3,
            ));
        }
        archive_and_seek(&mut state);
        for second in 1..6 {
            state.append_packet(video(second));
        }
    }
    assert_eq!(read_video(&cache), vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn cached_resume_uses_dts_without_discarding_reordered_video_pts() {
    let (shared, cache) = fixture();
    {
        let mut state = shared.state.lock().unwrap();
        for (pts, dts, key) in [
            (0, 0, true),
            (2, 1, false),
            (1, 2, false),
            (3, 3, true),
            (4, 4, false),
        ] {
            state.append_packet(packet(0, pts, Some(dts), None, key));
        }
        archive_and_seek(&mut state);
        for (pts, dts) in [(2, 1), (1, 2), (3, 3), (4, 4), (3, 5)] {
            state.append_packet(packet(0, pts, Some(dts), None, false));
        }
    }
    assert_eq!(read_video(&cache), vec![0, 2, 1, 3, 4, 3]);
}

#[test]
fn cached_resume_uses_position_when_dts_is_missing_or_non_monotonic() {
    for non_monotonic in [false, true] {
        let (shared, cache) = fixture();
        {
            let mut state = shared.state.lock().unwrap();
            for second in 0..4 {
                state.append_packet(packet(
                    0,
                    second,
                    non_monotonic.then_some(0),
                    Some(second * 100),
                    true,
                ));
            }
            archive_and_seek(&mut state);
            for second in 1..5 {
                state.append_packet(packet(
                    0,
                    second,
                    non_monotonic.then_some(0),
                    Some(second * 100),
                    true,
                ));
            }
        }
        assert_eq!(read_video(&cache), vec![0, 1, 2, 3, 4]);
    }
}

#[test]
fn cached_resume_finishing_video_does_not_clear_audio_or_subtitle_overlap() {
    let (shared, cache) = fixture();
    let (audio_packets, subtitle_packets) = {
        let mut state = shared.state.lock().unwrap();
        state.set_stream_kind(1, StreamCacheKind::Audio);
        state.set_stream_kind(4, StreamCacheKind::Subtitle);
        for second in 0..4 {
            state.append_packet(video(second));
        }
        for second in 0..8 {
            state.append_packet(packet(1, second, Some(second), None, true));
        }
        state.append_packet(packet(4, 1, Some(1), None, true));
        archive_and_seek(&mut state);
        state.append_packet(video(4));
        for second in 3..9 {
            state.append_packet(packet(1, second, Some(second), None, true));
        }
        state.append_packet(packet(4, 1, Some(1), None, true));
        state.append_packet(packet(4, 12, Some(12), None, true));
        (
            state.read_range().stream_queues[&1].len(),
            state.read_range().stream_queues[&4].len(),
        )
    };
    assert_eq!(audio_packets, 9);
    assert_eq!(subtitle_packets, 2);
    assert_eq!(read_video(&cache), vec![0, 1, 2, 3, 4]);
}

#[test]
fn cached_resume_keeps_refreshing_after_a_second_cached_seek() {
    let (shared, cache) = fixture();
    let (old_input_generation, new_input_generation, pending_seek_generation) = {
        let mut state = shared.state.lock().unwrap();
        for second in 0..5 {
            state.append_packet(video(second));
        }
        archive_and_seek(&mut state);
        state.append_packet(video(1));
        let old = state.demux_input_generation;
        assert!(
            state
                .seek_cached_with_generation(
                    1_500_000_000,
                    PlaybackSeekMode::Precise,
                    PlaybackSessionId(4),
                    9
                )
                .is_some()
        );
        let request = state.take_seek_request().unwrap();
        for second in 0..6 {
            state.append_packet(video(second));
        }
        (old, state.demux_input_generation, request.seek_generation)
    };
    assert!(new_input_generation > old_input_generation);
    assert_eq!(pending_seek_generation, 9);
    assert_eq!(read_video(&cache), vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_new_low_level_seek_clears_old_stream_refresh_boundaries() {
    let (shared, cache) = fixture();
    {
        let mut state = shared.state.lock().unwrap();
        for second in 0..4 {
            state.append_packet(video(second));
        }
        archive_and_seek(&mut state);
        state.request_seek(1.0, PlaybackSessionId(4), 9, 1_000_000_000);
        state.append_packet(video(1));
    }
    assert_eq!(read_video(&cache), vec![1]);
    assert!(shared.state.lock().unwrap().refreshing_streams.is_empty());
}

#[test]
fn archived_ranges_without_reliable_resume_keys_fall_back_to_low_level_seek() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let (shared, _) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    let (miss, ranges) = {
        let mut state = shared.state.lock().unwrap();
        for second in 0..4 {
            state.append_packet(packet(0, second, None, None, true));
        }
        state.request_seek(100.0, PlaybackSessionId(2), 1, 100_000_000_000);
        for second in 100..103 {
            state.append_packet(video(second));
        }
        (
            state
                .resolve_cached_seek_plan_attempt(500_000_000, PlaybackSeekMode::Precise, false)
                .err(),
            state.playback_cache_state(false).demux.seekable_ranges,
        )
    };
    assert_eq!(
        miss.unwrap().reason,
        CachedSeekMissReason::UnresumableStream
    );
    assert!(ranges.iter().all(|range| range.start >= 100.0));
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    assert_eq!(
        cache.seek(0.5, PlaybackSeekMode::Precise, PlaybackSessionId(3), 2),
        DemuxSeekResult::Requested
    );
}

#[test]
fn cached_resume_reads_through_overlap_past_time_target_and_stops_at_eof() {
    let (shared, cache) = fixture();
    let (before_resume_paused, refresh_paused, after_eof_paused, refreshing) = {
        let mut state = shared.state.lock().unwrap();
        for second in 0..5 {
            state.append_packet(video(second));
        }
        state.readahead_nsecs = 1;
        state.configured_hysteresis_nsecs = 0;
        let before = state.should_pause_demux();
        archive_and_seek(&mut state);
        let refreshing_paused = state.should_pause_demux();
        state.mark_eof();
        (
            before,
            refreshing_paused,
            state.should_pause_demux(),
            !state.refreshing_streams.is_empty(),
        )
    };
    assert!(before_resume_paused);
    assert!(!refresh_paused);
    assert!(after_eof_paused);
    assert!(!refreshing);
    assert_eq!(read_video(&cache), vec![0, 1, 2, 3, 4]);
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::Eof));
}
