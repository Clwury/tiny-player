use super::*;

fn append_av(state: &mut DemuxPacketCacheState, start: u64, end: u64) {
    state.append_packet(cached_anchor(start, end));
    state.append_packet(cached_packet(1, false, Some(start), Some(end)));
    close_seek_range(state, end);
}

fn emitted_ranges(events: &Receiver<BackendEvent>) -> Vec<Vec<PlaybackCacheTimeRange>> {
    events
        .try_iter()
        .filter_map(|event| match event.kind {
            BackendEventKind::CacheStateChanged(state) => Some(state.demux.seekable_ranges),
            _ => None,
        })
        .collect()
}

#[test]
fn disabling_audio_discards_all_audio_queues_and_preserves_video_readers() {
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
    append_av(&mut state, 0, 10_000_000_000);
    state.request_seek(30.0, PlaybackSessionId(2), 1, 30_000_000_000);
    state.take_seek_request();
    append_av(&mut state, 30_000_000_000, 40_000_000_000);
    let mut timing = DemuxPacketCacheReadTiming::default();
    state
        .take_packet_round_robin(&[0], &mut timing)
        .unwrap()
        .unwrap();
    let video_head = state.reader_heads[&0];
    let generation = state.generation;
    let read_range_id = state.read_range_id;
    let video_ids = state
        .packets
        .iter()
        .filter_map(|(id, packet)| (packet.stream_index == 0).then_some(*id))
        .collect::<std::collections::BTreeSet<_>>();
    let video_bytes: usize = video_ids.iter().map(|id| state.packets[id].byte_len).sum();

    state.set_selected_streams(DemuxSelectedStreams::default());

    assert_eq!(state.generation, generation);
    assert_eq!(state.read_range_id, read_range_id);
    assert_eq!(state.reader_heads[&0], video_head);
    assert_eq!(state.low_level_seeks, 1);
    assert!(state.seek_request.is_none());
    assert_eq!(state.cached_bytes, video_bytes);
    assert_eq!(
        state
            .packets
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        video_ids
    );
    assert!(!state.reader_heads.contains_key(&1));
    for range in state.ranges.values() {
        assert!(!range.stream_queues.contains_key(&1));
        assert!(!range.stream_pts_index.contains_key(&1));
        assert!(!range.stream_resume_positions.contains_key(&1));
        assert!(!range.stream_boundaries.contains_key(&1));
    }
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 10.0
            },
            PlaybackCacheTimeRange {
                start: 30.0,
                end: 40.0
            },
        ]
    );
}

#[test]
fn reenabling_audio_publishes_empty_then_refilled_common_seekable_ranges() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let (shared, events) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let audio = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    cache.set_selected_streams(Some(audio), None);
    {
        let mut state = shared.state.lock().unwrap();
        append_av(&mut state, 0, 100_000_000_000);
    }
    emitted_ranges(&events);
    cache.set_selected_streams(None, None);
    assert_eq!(
        emitted_ranges(&events).last(),
        Some(&vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 100.0,
        }])
    );

    cache.set_selected_streams(Some(audio), None);
    assert_eq!(
        emitted_ranges(&events).last(),
        Some(&Vec::new()),
        "old video-only ranges cannot satisfy the newly selected audio track"
    );
    assert_eq!(
        cache.seek_low_level(5.0, PlaybackSessionId(2), 1, "audio_track_change", false),
        DemuxSeekResult::Requested
    );
    assert!(emitted_ranges(&events).iter().all(Vec::is_empty));
    {
        let mut state = shared.state.lock().unwrap();
        state.take_seek_request();
    }
    shared.append_packet(cached_anchor(5_000_000_000, 6_000_000_000));
    shared.append_packet(cached_packet(
        1,
        false,
        Some(5_000_000_000),
        Some(6_000_000_000),
    ));
    shared.append_packet(cached_seek_closer(6_000_000_000));
    {
        let state = shared.state.lock().unwrap();
        assert_eq!(
            state.playback_cache_state(false).demux.seekable_ranges,
            vec![PlaybackCacheTimeRange {
                start: 5.0,
                end: 6.0
            }]
        );
    }
    shared.append_packet(cached_anchor(6_000_000_000, 8_000_000_000));
    shared.append_packet(cached_packet(
        1,
        false,
        Some(6_000_000_000),
        Some(8_000_000_000),
    ));
    // Advance the normal cache report cadence without sleeping.
    shared.state.lock().unwrap().last_cache_state_emit_at =
        Some(Instant::now() - Duration::from_secs(1));
    shared.append_packet(cached_seek_closer(8_000_000_000));
    assert_eq!(
        emitted_ranges(&events).last(),
        Some(&vec![PlaybackCacheTimeRange {
            start: 5.0,
            end: 8.0,
        }])
    );
}

#[test]
fn audio_deselection_rejects_an_inflight_packet_from_old_stream_selection() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let shared = Arc::new(shared_for_test(control));
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let audio = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    cache.set_selected_streams(Some(audio), None);
    let inflight = cached_packet(1, false, Some(0), Some(10_000_000_000));
    cache.set_selected_streams(None, None);
    shared.append_packet(inflight);
    let state = shared.state.lock().unwrap();
    assert!(state.packets.is_empty());
    assert!(state.read_range().stream_queues.is_empty());
    assert_eq!(state.cached_bytes, 0);
}
