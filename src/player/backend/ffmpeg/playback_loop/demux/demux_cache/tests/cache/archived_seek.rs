use super::*;

#[test]
fn demux_packet_cache_state_rejects_timeline_gaps() {
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

    assert_eq!(state.seek_cached(2_000_000_000, PlaybackSessionId(2)), None);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.reader_nsecs, 0);
}

#[test]
fn demux_packet_cache_state_requests_low_level_seek_outside_cache() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));

    assert_eq!(state.seek_cached(3_000_000_000, PlaybackSessionId(7)), None);
    state.request_seek(3.0, PlaybackSessionId(7), 1, 3_000_000_000);

    assert_eq!(state.packets.len(), 1);
    assert_eq!(state.ranges.len(), 2);
    assert!(state.read_range().global_order.is_empty());
    assert!(state.read_range().stream_queues.is_empty());
    assert_eq!(state.read_index, 0);
    assert_eq!(state.cached_bytes, 1024);
    assert_eq!(state.reader_nsecs, 3_000_000_000);
    assert_eq!(state.session_id, PlaybackSessionId(7));
    assert_eq!(state.cached_seeks, 0);
    assert_eq!(state.low_level_seeks, 1);
    assert!(state.playback_cache_state(false).demux.seeking);
    assert_eq!(
        state.seek_request.map(|request| request.session_id),
        Some(PlaybackSessionId(7))
    );
}

#[test]
fn demux_packet_cache_forced_low_level_seek_bypasses_cached_hit() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        close_seek_range(&mut guard, 1_000_000_000);
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert_eq!(
        cache.seek_low_level(0.5, PlaybackSessionId(9), 12, "test_forced_seek"),
        DemuxSeekResult::Requested
    );

    let guard = shared.state.lock().expect("cache state");
    assert_eq!(guard.cached_seeks, 0);
    assert_eq!(guard.low_level_seeks, 1);
    assert!(guard.seeking);
    assert_eq!(guard.reader_nsecs, 500_000_000);
    let request = guard.seek_request.expect("low-level seek queued");
    assert_eq!(request.position_seconds, 0.5);
    assert_eq!(request.session_id, PlaybackSessionId(9));
    assert_eq!(request.seek_generation, 12);
}

#[test]
fn audio_restart_low_level_seek_replaces_prefetch_with_missing_audio() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    let audio_stream = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let old_range_id = {
        let mut state = shared.state.lock().expect("cache state");
        state.set_selected_streams(DemuxSelectedStreams {
            audio_stream: Some(audio_stream),
            subtitle_stream: None,
        });
        for second in 0..10 {
            if second == 5 || second == 8 {
                state.set_selected_streams(DemuxSelectedStreams {
                    audio_stream: (second == 8).then_some(audio_stream),
                    subtitle_stream: None,
                });
            }
            let start = second * 1_000_000_000;
            let end = start + 1_000_000_000;
            state.append_packet(cached_anchor(start, end));
            // The demux thread drops audio packets while the track is disabled.
            if state.selected_streams.audio_stream.is_some() {
                state.append_packet(cached_packet(1, false, Some(start), Some(end)));
            }
        }
        close_seek_range(&mut state, 10_000_000_000);
        assert!(
            state
                .resolve_cached_seek_plan_attempt(500_000_000, PlaybackSeekMode::Precise, false)
                .is_ok(),
            "a cached seek can hit before the later audio gap"
        );
        let audio_starts = state.read_range().stream_queues[&1]
            .iter()
            .map(|packet_id| state.packets[packet_id].start_nsecs.unwrap() / 1_000_000_000)
            .collect::<Vec<_>>();
        assert_eq!(audio_starts, vec![0, 1, 2, 3, 4, 8, 9]);
        state.read_range_id
    };
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert_eq!(
        cache.seek_low_level(0.5, PlaybackSessionId(2), 1, "audio_track_change"),
        DemuxSeekResult::Requested
    );

    let mut state = shared.state.lock().expect("cache state");
    assert_ne!(state.read_range_id, old_range_id);
    assert!(state.stream_reader_head_timeline(1).is_none());
    let request = state.take_seek_request().expect("demux input seek queued");
    assert_eq!(request.position_seconds, 0.5);
    for second in 0..10 {
        let start = second * 1_000_000_000;
        let end = start + 1_000_000_000;
        state.append_packet(cached_anchor(start, end));
        state.append_packet(cached_packet(1, false, Some(start), Some(end)));
    }

    let mut timing = DemuxPacketCacheReadTiming::default();
    for second in 0..10 {
        let (_, start, end) = state
            .stream_reader_head_timeline(1)
            .expect("re-read audio is available");
        assert_eq!(start, Some(second * 1_000_000_000));
        assert_eq!(end, Some((second + 1) * 1_000_000_000));
        assert!(
            state
                .take_packet_round_robin(&[1], &mut timing)
                .expect("audio packet reads")
                .is_some()
        );
    }
}

#[test]
fn cached_seek_plan_resolve_does_not_move_readers_before_atomic_commit() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_packet(
        0,
        true,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    close_seek_range(&mut state, 3_000_000_000);
    let generation_before = state.generation;
    let reader_heads_before = state.reader_heads.clone();
    let read_range_before = state.read_range_id;

    let plan = state
        .resolve_cached_seek_plan_attempt(1_500_000_000, PlaybackSeekMode::Precise, false)
        .expect("closed cached range resolves");

    assert_eq!(state.generation, generation_before);
    assert_eq!(state.reader_heads, reader_heads_before);
    assert_eq!(state.read_range_id, read_range_before);
    assert_eq!(state.cached_seeks, 0);

    let hit = state
        .commit_cached_seek_plan(plan, PlaybackSessionId(2), 1)
        .expect("unchanged plan commits atomically");
    assert_eq!(hit.target_nsecs, 1_500_000_000);
    assert_eq!(state.generation, generation_before + 1);
    assert_eq!(state.exact_seek_target_nsecs, 1_500_000_000);
    assert_eq!(state.cached_seeks, 1);
}

#[test]
fn cached_seek_plan_rejects_stale_seekability_revision_without_moving_readers() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 3_000_000_000);
    let plan = state
        .resolve_cached_seek_plan_attempt(500_000_000, PlaybackSeekMode::Precise, false)
        .expect("closed cached range resolves");
    let generation_before = state.generation;
    let reader_heads_before = state.reader_heads.clone();

    state.bump_seekability_revision();
    let miss = state
        .commit_cached_seek_plan(plan, PlaybackSessionId(2), 1)
        .expect_err("revision change invalidates the plan");

    assert_eq!(miss.reason, CachedSeekMissReason::GenerationBlocked);
    assert_eq!(state.generation, generation_before);
    assert_eq!(state.reader_heads, reader_heads_before);
    assert_eq!(state.cached_seeks, 0);
}

#[test]
fn cached_range_pts_index_orders_reordered_packets_by_presentation_time() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        true,
        Some(10_000_000_000),
        Some(10_040_000_000),
        Some(10_000_000_000),
    ));
    state.append_packet(cached_runtime_packet_with_keyframe(
        0,
        true,
        false,
        Some(10_040_000_000),
        Some(10_080_000_000),
        Some(9_970_000_000),
    ));

    let indexed_pts = state
        .read_range()
        .stream_pts_index
        .get(&0)
        .expect("video PTS index exists")
        .keys()
        .map(|(pts, _)| *pts)
        .collect::<Vec<_>>();
    assert_eq!(indexed_pts, vec![9_970_000_000, 10_000_000_000]);
}

#[test]
fn blocked_cache_lookup_keeps_seek_audio_in_explicit_silence_state() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let generation = control.request_seek();
    let (shared, _event_rx) =
        shared_with_config_for_test(Arc::clone(&control), cache_config_for_test());
    let shared = Arc::new(shared);
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        close_seek_range(&mut guard, 3_000_000_000);
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let barrier = Arc::new(Barrier::new(2));
    let holder_shared = Arc::clone(&shared);
    let holder_barrier = Arc::clone(&barrier);
    let holder = thread::spawn(move || {
        let _guard = holder_shared.state.lock().expect("cache state");
        holder_barrier.wait();
        thread::sleep(Duration::from_millis(150));
    });
    barrier.wait();

    let started_at = Instant::now();
    let result = cache.seek(
        0.5,
        PlaybackSeekMode::Precise,
        PlaybackSessionId(9),
        generation,
    );

    holder.join().expect("lock holder exits");
    assert!(started_at.elapsed() >= Duration::from_millis(140));
    assert!(matches!(result, DemuxSeekResult::Cached(_)));
    assert!(control.is_seek_audio_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        crate::player::backend::ffmpeg::AudioOutputDecision::Silence
    );
    control.finish_seek(generation);
    assert!(control.finish_seek_audio_pause());
}

#[test]
fn demux_packet_cache_cache_only_seek_never_falls_through_to_low_level() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert_eq!(
        cache.seek_cached_only(62.521, PlaybackSeekMode::Precise, PlaybackSessionId(9), 12,),
        DemuxSeekResult::Unavailable
    );

    let guard = shared.state.lock().expect("cache state");
    assert_eq!(guard.low_level_seeks, 0);
    assert!(guard.seek_request.is_none());
}

#[test]
fn demux_packet_cache_cache_only_seek_uses_closed_cra_interval() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.cached_seek_preroll_nsecs = 500_000_000;
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            59_768_000_000,
            60_768_000_000,
        ));
        guard.append_packet(cached_packet(
            0,
            true,
            Some(60_768_000_000),
            Some(64_400_000_000),
        ));
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            64_439_000_000,
            65_439_000_000,
        ));
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    let result =
        cache.seek_cached_only(62.521, PlaybackSeekMode::Precise, PlaybackSessionId(9), 12);
    assert!(matches!(
        result,
        DemuxSeekResult::Cached(info)
            if info.anchor_kind == VideoRecoveryPointKind::Cra
                && info.anchor_nsecs == 59_768_000_000
    ));

    let guard = shared.state.lock().expect("cache state");
    assert_eq!(guard.cached_seeks, 1);
    assert_eq!(guard.low_level_seeks, 0);
}

#[test]
fn demux_packet_cache_safe_only_seek_can_use_older_idr_when_nearest_anchor_is_cra() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_codec_and_config_for_test(
        control,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        cache_config_for_test(),
    );
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Idr,
            true,
            0,
            1_000_000_000,
        ));
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            2_000_000_000,
            3_000_000_000,
        ));
        guard.append_packet(cached_packet(
            0,
            true,
            Some(3_000_000_000),
            Some(4_000_000_000),
        ));
        close_seek_range(&mut guard, 4_000_000_000);
    }
    let cache = DemuxPacketCache {
        shared: Arc::new(shared),
        handle: None,
    };

    assert!(matches!(
        cache.seek_cached_only(3.5, PlaybackSeekMode::Precise, PlaybackSessionId(2), 1),
        DemuxSeekResult::Cached(info)
            if info.anchor_kind == VideoRecoveryPointKind::Cra
                && info.anchor_nsecs == 2_000_000_000
    ));
    assert!(matches!(
        cache.seek_cached_safe_only(3.5, PlaybackSeekMode::Precise, PlaybackSessionId(3), 2),
        DemuxSeekResult::Cached(info)
            if info.anchor_kind == VideoRecoveryPointKind::Idr && info.anchor_nsecs == 0
    ));
    let guard = cache.shared.state.lock().expect("cache state");
    assert_eq!(guard.cached_seeks, 2);
    assert_eq!(guard.low_level_seeks, 0);
    assert!(guard.rejected_cached_seek_ranges.is_empty());
}

#[test]
fn demux_packet_cache_safe_only_seek_rejects_cra_without_low_level_fallback() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, _event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    let shared = Arc::new(shared);
    {
        let mut guard = shared.state.lock().expect("cache state");
        guard.cached_seek_preroll_nsecs = 500_000_000;
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            59_768_000_000,
            60_768_000_000,
        ));
        guard.append_packet(cached_packet(
            0,
            true,
            Some(60_768_000_000),
            Some(64_400_000_000),
        ));
        guard.append_packet(cached_video_recovery_packet(
            VideoRecoveryPointKind::Cra,
            false,
            64_439_000_000,
            65_439_000_000,
        ));
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };

    assert_eq!(
        cache.seek_cached_safe_only(62.521, PlaybackSeekMode::Precise, PlaybackSessionId(9), 12,),
        DemuxSeekResult::Unavailable
    );

    let guard = shared.state.lock().expect("cache state");
    assert_eq!(guard.cached_seeks, 0);
    assert_eq!(guard.low_level_seeks, 0);
    assert!(guard.seek_request.is_none());
    assert!(guard.rejected_cached_seek_ranges.is_empty());
}

#[test]
fn demux_packet_cache_state_clears_seeking_after_seek_result_appends() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.request_seek(3.0, PlaybackSessionId(2), 1, 3_000_000_000);

    assert!(state.playback_cache_state(false).demux.seeking);

    let _ = state.take_seek_request().expect("low-level seek is taken");
    state.append_packet(cached_anchor(3_000_000_000, 4_000_000_000));

    assert!(!state.playback_cache_state(false).demux.seeking);
}

#[test]
fn demux_packet_cache_state_skips_far_ahead_packets_after_low_level_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.request_seek(35.0, PlaybackSessionId(2), 1, 35_000_000_000);

    let blocked = state.append_packet(cached_anchor(237_000_000_000, 238_000_000_000));

    assert!(blocked.appended);
    assert_eq!(state.read_range().global_order.len(), 1);
    assert!(state.reader_heads.is_empty());
    assert_eq!(state.next_packet_id_for_stream(0), None);
    assert_eq!(state.forward_bytes(), 0);
    assert_eq!(state.cached_bytes, 2 * 1024);
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(35_000_000_000)
    );

    let accepted = state.append_packet(cached_anchor(35_000_000_000, 36_000_000_000));

    assert!(accepted.appended);
    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(state.reader_nsecs, 35_000_000_000);
    assert_eq!(state.next_packet_id_for_stream(0), Some(2));
    assert_eq!(state.forward_bytes(), 1024);
    assert_eq!(state.low_level_append_guard_target_nsecs, None);
}

#[test]
fn demux_packet_cache_state_blocks_far_ahead_audio_reader_after_low_level_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.request_seek(35.0, PlaybackSessionId(2), 1, 35_000_000_000);

    let blocked_audio = state.append_packet(cached_packet(
        1,
        false,
        Some(237_000_000_000),
        Some(238_000_000_000),
    ));

    assert!(blocked_audio.appended);
    assert_eq!(state.next_packet_id_for_stream(1), None);
    assert_eq!(state.forward_bytes(), 0);

    let accepted_video = state.append_packet(cached_anchor(35_000_000_000, 36_000_000_000));

    assert!(accepted_video.appended);
    assert_eq!(state.next_packet_id_for_stream(0), Some(2));
    assert_eq!(state.next_packet_id_for_stream(1), None);
    assert_eq!(state.forward_bytes(), 1024);
}

#[test]
fn demux_packet_cache_state_ignores_reader_heads_from_previous_generation() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));

    assert_eq!(state.next_packet_id_for_stream(0), Some(0));

    state
        .reader_head_generations
        .insert(0, state.generation.saturating_add(1));

    assert_eq!(state.next_packet_id_for_stream(0), None);

    state.reset_reader_heads_for_read_index();

    assert_eq!(state.next_packet_id_for_stream(0), Some(0));
}

#[test]
fn demux_packet_cache_coalesces_seek_completion_state_after_seek_result_appends() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
        let _ = guard.take_seek_request().expect("low-level seek is taken");
        guard.last_cache_state_emit_at = Some(Instant::now());
    }
    let _ = event_rx.try_iter().collect::<Vec<_>>();

    shared.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.kind, BackendEventKind::CacheStateChanged(_)))
    );

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        assert!(guard.cache_state_emit_dirty());
        guard.last_cache_state_emit_at =
            Some(Instant::now() - DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL);
    }

    shared.append_packet(cached_anchor(11_000_000_000, 12_000_000_000));

    let events = event_rx.try_iter().collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if !state.demux.seeking
                    && state.demux.reader_pts == Some(10.0)
                    && state.demux.cache_end == Some(12.0)
        )
    }));
}

#[test]
fn demux_packet_cache_state_indexes_archived_ranges_by_range_id() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);
    state.request_seek(20.0, PlaybackSessionId(3), 2, 20_000_000_000);
    state.append_packet(cached_anchor(20_000_000_000, 21_000_000_000));

    assert_eq!(state.read_range_id, 2);
    assert_eq!(state.append_range_id, 2);
    assert_eq!(
        state
            .ranges
            .keys()
            .copied()
            .filter(|range_id| *range_id != state.read_range_id)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );

    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(4)),
        Some(1.0)
    );

    assert_eq!(state.read_range_id, 0);
    assert_ne!(state.read_range_id, state.append_range_id);
    assert_eq!(
        state
            .ranges
            .iter()
            .filter(|(range_id, _)| **range_id != state.read_range_id)
            .filter(|(range_id, _)| **range_id != state.append_range_id)
            .map(|(range_id, _)| *range_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(0)
    );
}

#[test]
fn demux_packet_cache_state_seeks_inside_archived_range_after_low_level_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));

    assert_eq!(
        state.seek_cached(500_000_000, PlaybackSessionId(3)),
        Some(1.0)
    );
    assert_eq!(state.reader_nsecs, 0);
    assert_eq!(state.session_id, PlaybackSessionId(3));
    assert_eq!(state.read_index, 0);
    assert!(!state.demux_position_detached);
    assert_eq!(state.resume_append_skip_until_nsecs, Some(1_000_000_000));
    assert_ne!(state.read_range_id, state.append_range_id);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| (range.id, range.global_order.len())),
        Some((state.append_range_id, 0))
    );
    assert_eq!(state.archived_bytes(), 1024);
    assert_eq!(state.cached_seeks, 1);
    assert_eq!(state.low_level_seeks, 2);
    assert!(state.playback_cache_state(false).demux.seeking);
    let request = state.seek_request.expect("resume seek is queued");
    assert_eq!(request.position_seconds, 1.0);
    assert_eq!(request.session_id, PlaybackSessionId(3));
}

#[test]
fn demux_packet_cache_state_skips_resume_overlap_packets_after_archived_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);
    assert_eq!(
        state.seek_cached_with_generation(
            500_000_000,
            PlaybackSeekMode::Precise,
            PlaybackSessionId(3),
            7
        ),
        Some(1.0)
    );
    assert_eq!(state.read_range().global_order.len(), 2);

    state.append_packet(cached_anchor(500_000_000, 1_000_000_000));

    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(1)
    );
    assert_eq!(state.cached_bytes, 3 * 1024);
    assert_eq!(state.resume_append_skip_until_nsecs, Some(1_000_000_000));
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(1_000_000_000)
    );
    let request = state.seek_request.expect("resume seek is queued");
    assert_eq!(request.seek_generation, 7);

    state.seek_request = None;
    let blocked_far_ahead = state.append_packet(cached_anchor(237_000_000_000, 238_000_000_000));
    assert!(blocked_far_ahead.appended);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(2)
    );
    assert_eq!(state.cached_bytes, 4 * 1024);
    assert_eq!(
        state.low_level_append_guard_target_nsecs,
        Some(1_000_000_000)
    );

    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(state.read_range().global_order.len(), 2);
    assert_eq!(
        state
            .ranges
            .get(&state.append_range_id)
            .map(|range| range.global_order.len()),
        Some(4)
    );
    assert_eq!(state.cached_bytes, 5 * 1024);
    assert_eq!(state.forward_bytes(), 2 * 1024);
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0
            },
            PlaybackCacheTimeRange {
                start: 1.0,
                end: 2.0
            },
            PlaybackCacheTimeRange {
                start: 10.0,
                end: 11.0
            },
        ]
    );
    assert_eq!(state.resume_append_skip_until_nsecs, None);

    state.set_read_index_for_test(state.read_range().global_order.len());
    assert!(state.activate_detached_append_range());
    assert_eq!(state.read_range_id, state.append_range_id);
    assert_eq!(state.read_range().global_order.len(), 4);
    assert!(state.detached_append_range().is_none());
}

#[test]
fn demux_packet_cache_state_requests_continuation_after_detached_range_exhausts() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.demux_position_detached = true;
    state.set_read_index_for_test(1);
    state.reader_nsecs = 1_000_000_000;

    state.request_continuation_seek(4);

    assert!(!state.demux_position_detached);
    assert!(state.read_range().global_order.is_empty());
    assert_eq!(state.ranges.len(), 2);
    assert_eq!(state.low_level_seeks, 1);
    let request = state.seek_request.expect("continuation seek is queued");
    assert_eq!(request.position_seconds, 1.0);
    assert_eq!(request.seek_generation, 4);
}

#[test]
fn demux_packet_cache_reports_continuation_seek_promptly_after_detached_range_exhausts() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let (shared, event_rx) = shared_with_config_for_test(control, cache_config_for_test());
    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.append_packet(cached_anchor(0, 1_000_000_000));
        close_seek_range(&mut guard, 1_000_000_000);
        guard.demux_position_detached = true;
        guard.set_read_index_for_test(2);
        guard.reader_nsecs = 1_000_000_000;
    }
    let shared = Arc::new(shared);
    let read_shared = Arc::clone(&shared);
    let read_handle = thread::spawn(move || {
        let cache = DemuxPacketCache {
            shared: read_shared,
            handle: None,
        };
        cache.read_packet_round_robin(&[0]).0
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut continuation_state = None;
    while Instant::now() < deadline {
        for event in event_rx.try_iter() {
            if let BackendEventKind::CacheStateChanged(state) = event.kind
                && state.demux.seeking
                && state.demux.low_level_seeks == 1
            {
                continuation_state = Some(state);
                break;
            }
        }
        if continuation_state.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    {
        let mut guard = shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.shutdown = true;
    }
    shared.ready.notify_all();

    assert!(matches!(
        read_handle.join().expect("read thread joins"),
        DemuxReadResult::Interrupted
    ));
    let continuation_state =
        continuation_state.expect("continuation seek emits cache state promptly");
    assert_eq!(continuation_state.demux.reader_pts, Some(1.0));
    assert_eq!(
        continuation_state.demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 1.0
        }]
    );
}

#[test]
fn demux_packet_cache_state_reports_multiple_seekable_ranges() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    state.append_packet(cached_anchor(11_000_000_000, 12_000_000_000));

    let cache_state = state.playback_cache_state(false);

    assert_eq!(
        cache_state.demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0
            },
            PlaybackCacheTimeRange {
                start: 10.0,
                end: 11.0
            }
        ]
    );
    assert_eq!(cache_state.demux.cached_seeks, 0);
    assert_eq!(cache_state.demux.low_level_seeks, 1);
    assert_eq!(cache_state.demux.total_bytes, 4096);
}

#[test]
fn demux_packet_cache_state_merges_overlapping_ranges_after_backward_seek() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(10_000_000_000, 20_000_000_000));
    state.append_packet(cached_anchor(20_000_000_000, 30_000_000_000));
    state.request_seek(0.0, PlaybackSessionId(2), 1, 0);
    state.append_packet(cached_anchor(0, 15_000_000_000));
    state.append_packet(cached_anchor(15_000_000_000, 25_000_000_000));

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 20.0
        }],
        "overlapping physical ranges must be exposed as one mpv-style seekable range"
    );
}

#[test]
fn demux_packet_cache_state_keeps_adjacent_physical_ranges_independent() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(1.0, PlaybackSessionId(2), 1, 1_000_000_000);
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    close_seek_range(&mut state, 2_000_000_000);

    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0,
            },
            PlaybackCacheTimeRange {
                start: 1.0,
                end: 2.0,
            },
        ],
        "only positive overlap is merged; touching ranges retain mpv-style boundaries"
    );
}

#[test]
fn demux_packet_cache_state_reports_stream_kinds() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.set_stream_kind(2, StreamCacheKind::Subtitle);
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(500_000_000)));
    state.append_packet(cached_packet(2, false, Some(0), Some(2_000_000_000)));

    let streams = state.playback_cache_state(false).demux.streams;

    assert_eq!(
        streams.iter().map(|stream| stream.kind).collect::<Vec<_>>(),
        vec![
            StreamCacheKind::Video,
            StreamCacheKind::Audio,
            StreamCacheKind::Subtitle,
        ]
    );
    assert_eq!(streams[0].cache_duration, Some(1.0));
    assert_eq!(streams[1].cache_duration, Some(0.5));
    assert_eq!(streams[2].cache_duration, Some(2.0));
}

#[test]
fn demux_packet_cache_state_omits_invalid_stream_cache_duration() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_packet(
        1,
        false,
        Some(2_000_000_000),
        Some(1_000_000_000),
    ));

    let streams = state.playback_cache_state(false).demux.streams;

    let audio = streams
        .iter()
        .find(|stream| stream.kind == StreamCacheKind::Audio)
        .expect("audio stream cache state");
    assert_eq!(audio.reader_pts, Some(2.0));
    assert_eq!(audio.cache_end, Some(1.0));
    assert_eq!(audio.cache_duration, None);
}

#[test]
fn demux_packet_cache_state_stream_windows_ignore_archived_ranges() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    close_seek_range(&mut state, 1_000_000_000);
    state.request_seek(10.0, PlaybackSessionId(2), 1, 10_000_000_000);
    state.append_packet(cached_anchor(10_000_000_000, 11_000_000_000));
    close_seek_range(&mut state, 11_000_000_000);

    let streams = state.playback_cache_state(false).demux.streams;

    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].reader_pts, Some(10.0));
    assert_eq!(streams[0].cache_end, Some(11.0));
    assert_eq!(streams[0].cache_duration, Some(1.0));
    assert_eq!(
        state.playback_cache_state(false).demux.seekable_ranges,
        vec![
            PlaybackCacheTimeRange {
                start: 0.0,
                end: 1.0
            },
            PlaybackCacheTimeRange {
                start: 10.0,
                end: 11.0
            }
        ]
    );
}

#[test]
fn demux_packet_cache_state_stream_windows_ignore_consumed_packets() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.set_stream_kind(1, StreamCacheKind::Audio);
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_packet(1, false, Some(0), Some(1_000_000_000)));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    state.append_packet(cached_packet(
        1,
        false,
        Some(1_000_000_000),
        Some(2_000_000_000),
    ));
    state.set_read_index_for_test(2);
    state.reader_nsecs = 1_000_000_000;

    let streams = state.playback_cache_state(false).demux.streams;

    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].kind, StreamCacheKind::Video);
    assert_eq!(streams[0].reader_pts, Some(1.0));
    assert_eq!(streams[0].cache_end, Some(2.0));
    assert_eq!(streams[0].cache_duration, Some(1.0));
    assert!(!streams[0].underrun);
    assert_eq!(streams[1].kind, StreamCacheKind::Audio);
    assert_eq!(streams[1].reader_pts, Some(1.0));
    assert_eq!(streams[1].cache_end, Some(2.0));
    assert_eq!(streams[1].cache_duration, Some(1.0));
    assert!(!streams[1].underrun);

    state.set_read_index_for_test(4);
    state.reader_nsecs = 2_000_000_000;
    let streams = state.playback_cache_state(false).demux.streams;
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].kind, StreamCacheKind::Video);
    assert_eq!(streams[0].reader_pts, Some(2.0));
    assert_eq!(streams[0].cache_end, Some(2.0));
    assert_eq!(streams[0].cache_duration, Some(0.0));
    assert!(streams[0].underrun);
    assert!(!streams[0].idle);
    assert_eq!(streams[1].kind, StreamCacheKind::Audio);
    assert_eq!(streams[1].reader_pts, Some(2.0));
    assert_eq!(streams[1].cache_end, Some(2.0));
    assert_eq!(streams[1].cache_duration, Some(0.0));
    assert!(streams[1].underrun);
    assert!(!streams[1].idle);
}
