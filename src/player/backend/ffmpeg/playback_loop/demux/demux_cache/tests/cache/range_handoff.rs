use super::*;

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
    shared
        .state
        .lock()
        .unwrap()
        .set_selected_streams(DemuxSelectedStreams {
            audio_stream: Some(stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC)),
            subtitle_stream: Some(stream_info_for_test(4, ffi::AVCodecID::AV_CODEC_ID_SUBRIP)),
        });
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    (shared, cache)
}

fn append_audio_video(state: &mut DemuxPacketCacheState, second: u64) {
    let start = second * 1_000_000_000;
    let end = start + 1_000_000_000;
    state.append_packet(cached_anchor(start, end));
    state.append_packet(cached_packet(1, false, Some(start), Some(end)));
}

#[test]
fn cached_seek_dry_subtitle_poll_preserves_active_audio_video_range() {
    let (shared, cache) = fixture();
    let (read_range_id, video_head, audio_head) = {
        let mut state = shared.state.lock().unwrap();
        state.append_packet(cached_packet(
            4,
            false,
            Some(584_000_000_000),
            Some(586_000_000_000),
        ));
        for second in 584..940 {
            append_audio_video(&mut state, second);
        }
        close_seek_range(&mut state, 940_000_000_000);
        state.request_seek(1800.0, PlaybackSessionId(2), 1, 1_800_000_000_000);
        append_audio_video(&mut state, 1800);
        close_seek_range(&mut state, 1_801_000_000_000);
        assert_eq!(
            state.seek_cached(585_500_000_000, PlaybackSessionId(3)),
            Some(940.0)
        );
        let resume = state.take_seek_request().expect("resume seek queued");
        assert_eq!(resume.position_seconds, 940.0);
        state.seeking = false;
        append_audio_video(&mut state, 940);
        assert_eq!(state.read_range_id, state.append_range_id);
        (
            state.read_range_id,
            state.reader_heads[&0],
            state.reader_heads[&1],
        )
    };

    assert!(matches!(cache.poll_packet(4), DemuxReadResult::Packet(_)));
    // Decoder backpressure can leave only the sparse subtitle stream eligible.
    for _ in 0..3 {
        assert!(matches!(cache.poll_packet(4), DemuxReadResult::WouldBlock));
        let state = shared.state.lock().unwrap();
        assert_eq!(state.read_range_id, read_range_id);
        assert_eq!(state.reader_heads[&0], video_head);
        assert_eq!(state.reader_heads[&1], audio_head);
        assert!(state.reader_nsecs < 586_000_000_000);
        assert!(state.seek_request.is_none());
    }
    for (stream, expected_head) in [(0, video_head), (1, audio_head)] {
        let DemuxReadResult::Packet(packet) = cache.poll_packet(stream) else {
            panic!("cached audio/video must remain available after a dry subtitle poll");
        };
        let diagnostic = packet.read_diagnostic().unwrap();
        assert_eq!(diagnostic.read_range_id, read_range_id);
        assert_eq!(diagnostic.packet_id, expected_head);
        assert!(diagnostic.packet_start_nsecs.unwrap() < 586_000_000_000);
    }
}

#[test]
fn range_handoff_waits_for_all_audio_video_readers_but_not_subtitles() {
    for exhausted_stream in [0, 1] {
        let (shared, cache) = fixture();
        let (read_range_id, append_range_id) = {
            let mut state = shared.state.lock().unwrap();
            append_audio_video(&mut state, 585);
            state.append_packet(cached_packet(
                4,
                false,
                Some(586_000_000_000),
                Some(587_000_000_000),
            ));
            state.start_detached_append_range();
            append_audio_video(&mut state, 586);
            (state.read_range_id, state.append_range_id)
        };
        assert!(matches!(
            cache.poll_packet(exhausted_stream),
            DemuxReadResult::Packet(_)
        ));
        assert!(matches!(
            cache.poll_packet(exhausted_stream),
            DemuxReadResult::WouldBlock
        ));
        assert_eq!(shared.state.lock().unwrap().read_range_id, read_range_id);

        assert!(matches!(
            cache.poll_packet(1 - exhausted_stream),
            DemuxReadResult::Packet(_)
        ));
        assert!(shared.state.lock().unwrap().reader_heads.contains_key(&4));
        let DemuxReadResult::Packet(packet) = cache.poll_packet(exhausted_stream) else {
            panic!("all audio/video drained; sparse subtitles must not block the handoff");
        };
        let diagnostic = packet.read_diagnostic().unwrap();
        assert_eq!(diagnostic.read_range_id, append_range_id);
        assert_eq!(diagnostic.packet_start_nsecs, Some(586_000_000_000));
    }
}

#[test]
fn dry_subset_poll_does_not_report_global_eof_with_pending_audio_video() {
    let (shared, cache) = fixture();
    {
        let mut state = shared.state.lock().unwrap();
        append_audio_video(&mut state, 585);
        let range_id = state.read_range_id;
        state.set_range_eof(range_id, true);
    }
    assert!(matches!(cache.poll_packet(4), DemuxReadResult::WouldBlock));
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::Packet(_)));
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::WouldBlock));
    assert!(matches!(cache.poll_packet(1), DemuxReadResult::Packet(_)));
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::Eof));
}

#[test]
fn dry_subset_poll_does_not_seek_past_pending_audio_video() {
    let (shared, cache) = fixture();
    let read_range_id = {
        let mut state = shared.state.lock().unwrap();
        append_audio_video(&mut state, 585);
        state.demux_position_detached = true;
        state.read_range_id
    };
    assert!(matches!(cache.poll_packet(4), DemuxReadResult::WouldBlock));
    {
        let state = shared.state.lock().unwrap();
        assert_eq!(state.read_range_id, read_range_id);
        assert!(state.demux_position_detached);
        assert!(state.seek_request.is_none());
        assert_eq!(state.low_level_seeks, 0);
    }
    for stream in [0, 1] {
        assert!(matches!(
            cache.poll_packet(stream),
            DemuxReadResult::Packet(_)
        ));
    }
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::WouldBlock));
    let state = shared.state.lock().unwrap();
    assert_eq!(state.low_level_seeks, 1);
    assert_eq!(state.seek_request.unwrap().position_seconds, 586.0);
}
