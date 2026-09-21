use super::*;

fn append_av(state: &mut DemuxPacketCacheState, start: u64, end: u64) {
    state.append_packet(cached_anchor(start, end));
    state.append_packet(cached_packet(1, false, Some(start), Some(end)));
    close_seek_range(state, end);
}

#[test]
fn subtitle_off_preserves_audio_video_readers_and_seekable_ranges() {
    let audio = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let subtitle = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_SUBRIP);
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(audio),
        subtitle_stream: Some(subtitle),
    });
    append_av(&mut state, 0, 100_000_000_000);
    state.append_packet(cached_packet(
        2,
        false,
        Some(30_000_000_000),
        Some(32_000_000_000),
    ));
    let mut timing = DemuxPacketCacheReadTiming::default();
    state
        .take_packet_round_robin(&[0], &mut timing)
        .unwrap()
        .unwrap();
    let ranges = state.playback_cache_state(false).demux.seekable_ranges;
    let video_head = state.reader_heads.get(&0).copied();
    let audio_head = state.reader_heads.get(&1).copied();
    let generation = state.generation;
    let range_id = state.read_range_id;
    let retained_ids: std::collections::BTreeSet<_> = state
        .packets
        .iter()
        .filter_map(|(id, packet)| (packet.stream_index != 2).then_some(*id))
        .collect();

    state.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(audio),
        subtitle_stream: None,
    });

    assert_eq!(state.generation, generation);
    assert_eq!(state.read_range_id, range_id);
    assert_eq!(state.reader_heads.get(&0).copied(), video_head);
    assert_eq!(state.reader_heads.get(&1).copied(), audio_head);
    assert!(state.seek_request.is_none());
    assert_eq!(state.low_level_seeks, 0);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        ranges
    );
    assert_eq!(
        state
            .packets
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        retained_ids
    );
    assert!(!state.read_range().stream_queues.contains_key(&2));
}

#[test]
fn reenabling_subtitles_refreshes_ranges_without_waiting_for_sparse_cues() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let (shared, events) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let audio = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let subtitle = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_SUBRIP);
    cache.set_selected_streams(Some(audio), Some(subtitle));
    {
        let mut state = shared.state.lock().unwrap();
        append_av(&mut state, 0, 20_000_000_000);
        state.append_packet(cached_packet(
            2,
            false,
            Some(5_000_000_000),
            Some(7_000_000_000),
        ));
        state.request_seek(30.0, PlaybackSessionId(2), 1, 30_000_000_000);
        state.take_seek_request();
        append_av(&mut state, 30_000_000_000, 100_000_000_000);
    }
    cache.set_selected_streams(Some(audio), None);
    assert_eq!(
        shared
            .state
            .lock()
            .unwrap()
            .playback_cache_state(false)
            .demux
            .seekable_ranges
            .len(),
        2
    );
    cache.set_selected_streams(Some(audio), Some(subtitle));
    events.try_iter().for_each(drop);

    assert_eq!(
        cache.seek_low_level(
            35.0,
            PlaybackSessionId(3),
            2,
            "internal_subtitle_track_change",
            true
        ),
        DemuxSeekResult::Requested
    );
    let reports = events
        .try_iter()
        .filter_map(|event| match event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state.demux.seekable_ranges),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reports.last(), Some(&Vec::new()));
    {
        let mut state = shared.state.lock().unwrap();
        assert_eq!(state.ranges.len(), 1);
        assert!(state.packets.is_empty());
        assert_eq!(state.cached_bytes, 0);
        assert_eq!(state.resident_bytes, 0);
        assert_eq!(state.disk_cached_bytes, 0);
        assert!(state.reader_heads.is_empty());
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(3))
                .is_none()
        );
        state.take_seek_request();
    }
    shared.append_packet(cached_anchor(35_000_000_000, 36_000_000_000));
    shared.append_packet(cached_packet(
        1,
        false,
        Some(35_000_000_000),
        Some(36_000_000_000),
    ));
    shared.append_packet(cached_seek_closer(36_000_000_000));
    assert_eq!(
        shared
            .state
            .lock()
            .unwrap()
            .playback_cache_state(false)
            .demux
            .seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 35.0,
            end: 36.0
        }]
    );
    shared.append_packet(cached_anchor(36_000_000_000, 40_000_000_000));
    shared.append_packet(cached_packet(
        1,
        false,
        Some(36_000_000_000),
        Some(40_000_000_000),
    ));
    shared.state.lock().unwrap().last_cache_state_emit_at =
        Some(Instant::now() - Duration::from_secs(1));
    shared.append_packet(cached_seek_closer(40_000_000_000));
    let last = events
        .try_iter()
        .filter_map(|event| match event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state.demux.seekable_ranges),
            _ => None,
        })
        .last();
    assert_eq!(
        last,
        Some(vec![PlaybackCacheTimeRange {
            start: 35.0,
            end: 40.0
        }])
    );
}

#[test]
fn superseded_subtitle_refresh_does_not_discard_cached_media() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let shared = Arc::new(shared_for_test(Arc::clone(&control)));
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    shared
        .state
        .lock()
        .unwrap()
        .append_packet(cached_anchor(0, 10_000_000_000));
    let bytes = shared.state.lock().unwrap().cached_bytes;
    let old_generation = control.request_seek();
    control.request_seek();
    assert_eq!(
        cache.seek_low_level(
            5.0,
            PlaybackSessionId(2),
            old_generation,
            "internal_subtitle_track_change",
            true
        ),
        DemuxSeekResult::Superseded
    );
    let state = shared.state.lock().unwrap();
    assert_eq!(state.cached_bytes, bytes);
    assert_eq!(state.packets.len(), 1);
    assert!(state.seek_request.is_none());
}

#[test]
fn subtitle_off_rejects_an_inflight_packet_from_the_old_selection() {
    let shared = Arc::new(shared_for_test(Arc::new(FfmpegControl::new(
        PlaybackSessionId(1),
    ))));
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let subtitle = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_SUBRIP);
    cache.set_selected_streams(None, Some(subtitle));
    let inflight = cached_packet(2, false, Some(0), Some(10_000_000_000));
    cache.set_selected_streams(None, None);
    shared.append_packet(inflight);
    assert!(shared.state.lock().unwrap().packets.is_empty());
}
