use super::*;

#[test]
fn dovi_packet_timeline_uses_first_packet_when_stream_start_is_missing() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut first_packet_nsecs = None;

    assert_eq!(
        dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_250, time_base),
        Some(0)
    );
    assert_eq!(
        dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_500, time_base),
        Some(250_000_000)
    );
}

#[test]
fn ffmpeg_http_headers_formats_crlf_separated_headers() {
    let headers = ffmpeg_http_headers(&[
        ("X-Emby-Token".to_string(), "token".to_string()),
        ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
    ])
    .unwrap();

    assert_eq!(
        headers,
        "X-Emby-Token: token\r\nUser-Agent: Lenna/1.0.13\r\n"
    );
}

#[test]
fn ffmpeg_http_headers_rejects_header_injection() {
    assert!(ffmpeg_http_headers(&[("Bad\nName".to_string(), "value".to_string())]).is_err());
    assert!(
        ffmpeg_http_headers(&[("X-Emby-Token".to_string(), "bad\r\nvalue".to_string())]).is_err()
    );
}

#[test]
fn detects_cacheable_http_urls() {
    assert!(should_cache_http_url("https://example.test/video.mp4"));
    assert!(should_cache_http_url("HTTP://example.test/video.mp4"));
    assert!(!should_cache_http_url("file:///tmp/video.mp4"));
    assert!(!should_cache_http_url("/tmp/video.mp4"));
}

#[test]
fn cached_input_source_keeps_http_cache_alive_between_probe_readers() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        HTTP_CACHE_CHUNK_SIZE,
    ));
    let mut source = CachedInputSource::from_cache_for_test(cache.clone());

    let first_reader = source
        .cached_avio()
        .expect("cached AVIO can be created")
        .expect("HTTP source uses cached AVIO");
    drop(first_reader);
    assert!(!cache.is_shutdown_for_test());

    let mut final_reader = source
        .cached_avio()
        .expect("cached AVIO can be recreated")
        .expect("HTTP source uses cached AVIO");
    final_reader.shutdown_cache_on_drop();
    source.release();
    drop(source);
    assert!(!cache.is_shutdown_for_test());

    drop(final_reader);
    assert!(cache.is_shutdown_for_test());
}

#[test]
fn cached_input_source_shutdowns_http_cache_when_no_reader_is_released() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        HTTP_CACHE_CHUNK_SIZE,
    ));

    drop(CachedInputSource::from_cache_for_test(cache.clone()));

    assert!(cache.is_shutdown_for_test());
}

#[test]
fn empty_http_cache_can_fall_back_to_native_ffmpeg_input() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        HTTP_CACHE_CHUNK_SIZE,
    ));
    let mut source = CachedInputSource::from_cache_for_test(cache.clone());

    assert!(source.disable_empty_startup_cache_for_native_fallback());
    assert!(cache.is_shutdown_for_test());
    assert!(
        source
            .cached_avio()
            .expect("native fallback cache lookup succeeds")
            .is_none()
    );
}

#[test]
fn populated_http_cache_is_preserved_for_probe_fallback() {
    let mut state = HttpRingCacheState::new_with_cache_capacity(0, HTTP_CACHE_CHUNK_SIZE);
    assert!(state.append_at(0, b"media"));
    let cache = HttpRingCache::from_state_for_test(state);
    let mut source = CachedInputSource::from_cache_for_test(cache.clone());

    assert!(!source.disable_empty_startup_cache_for_native_fallback());
    assert!(!cache.is_shutdown_for_test());
    assert!(
        source
            .cached_avio()
            .expect("cached probe lookup succeeds")
            .is_some()
    );
}

#[test]
fn cached_input_source_skips_http_cache_when_cache_mode_is_disabled() {
    let (event_tx, _) = mpsc::channel();
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Disabled,
        ..PlaybackCacheConfig::default()
    };
    let source = CachedInputSource::new(
        "https://example.test/video.mp4",
        &[],
        Some(1_024),
        &config,
        Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    )
    .expect("source creates without spawning HTTP cache");

    assert!(
        source
            .cached_avio()
            .expect("cache lookup succeeds")
            .is_none()
    );
}

#[test]
fn http_cache_request_header_log_redacts_credentials() {
    let headers = reqwest_header_pairs(&[
        ("X-Emby-Token".to_string(), "token".to_string()),
        ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
    ])
    .unwrap();

    assert_eq!(
        http_cache_request_headers_for_log(&headers, "bytes=128-255"),
        vec![
            "accept-encoding: identity".to_string(),
            "connection: keep-alive".to_string(),
            "range: bytes=128-255".to_string(),
            "x-emby-token: <redacted>".to_string(),
            "user-agent: Lenna/1.0.13".to_string(),
        ]
    );
}

#[test]
fn http_cache_range_header_limits_request_size() {
    assert_eq!(
        http_cache_range_header(0, None, HTTP_CACHE_RANGE_REQUEST_BYTES),
        format!("bytes=0-{}", HTTP_CACHE_RANGE_REQUEST_BYTES - 1)
    );
    assert_eq!(
        http_cache_range_header(128, None, HTTP_CACHE_RANGE_REQUEST_BYTES),
        format!("bytes=128-{}", 128 + HTTP_CACHE_RANGE_REQUEST_BYTES - 1)
    );
    assert_eq!(
        http_cache_range_header(
            595_453_649,
            Some(596_486_439),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        "bytes=595453649-596486438"
    );
    assert_eq!(
        http_cache_range_header(
            10_675_366_349,
            Some(10_675_368_645),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        "bytes=10675366349-10675368644"
    );
    assert_eq!(http_cache_range_header(0, None, 1024), "bytes=0-1023");
}

#[test]
fn http_cache_range_request_timeout_is_short_for_small_tail_ranges() {
    let playback_request_bytes =
        http_cache_playback_range_request_bytes(HTTP_CACHE_RANGE_REQUEST_BYTES);
    assert_eq!(playback_request_bytes, HTTP_CACHE_RANGE_REQUEST_BYTES);
    assert_eq!(
        http_cache_range_request_len(
            10_675_366_349,
            Some(10_675_368_645),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        2_296
    );
    assert_eq!(
        http_cache_range_request_timeout(2_296),
        HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT
    );
    assert_eq!(
        http_cache_range_request_timeout(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES + 1),
        HTTP_CACHE_RANGE_REQUEST_TIMEOUT
    );
    assert_eq!(
        http_cache_range_request_timeout(playback_request_bytes),
        HTTP_CACHE_RANGE_REQUEST_TIMEOUT
    );
}

#[test]
fn http_cache_response_header_log_includes_response_headers() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 10-19/100"),
    );
    headers.insert(
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::HeaderValue::from_static("10"),
    );

    assert_eq!(
        http_cache_response_headers_for_log(&headers),
        vec![
            "content-length: 10".to_string(),
            "content-range: bytes 10-19/100".to_string(),
        ]
    );
}

#[test]
fn http_ring_cache_state_copies_available_bytes() {
    let mut state = HttpRingCacheState::new(10);
    state.append_at(10, b"abcdef");
    let mut output = [0; 3];

    assert_eq!(state.copy_available(12, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(state.copy_available(16, &mut output), None);
}

#[test]
fn http_ring_cache_probe_read_does_not_move_playback_reader() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.set_reader_offset(103);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(104, &mut output),
        CacheReadResult::Data(2)
    ));
    assert_eq!(&output, b"ef");
    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());

    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());
}

#[test]
fn http_ring_cache_probe_read_queues_trimmed_range_without_active_restart() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.set_reader_offset(104);
    state.trim_to_capacity(2);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(100, &mut output),
        CacheReadResult::WouldBlock
    ));
    assert!(!cache.has_restart_request_for_test());
    assert_eq!(
        cache.side_download_requests_for_test(),
        vec![CacheRestartRequest {
            generation: 0,
            offset: 100,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
}

#[test]
fn http_ring_cache_probe_read_queues_side_download_without_active_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");
    state.set_reader_offset(103);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(500, &mut output),
        CacheReadResult::WouldBlock
    ));
    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());
    assert_eq!(
        cache.side_download_requests_for_test(),
        vec![CacheRestartRequest {
            generation: 0,
            offset: 500,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
}

#[test]
fn http_ring_cache_read_at_returns_large_partial_buffer_without_waiting_for_full_request() {
    let mut state = HttpRingCacheState::new(0);
    let cached = vec![0x5a; HTTP_CACHE_PARTIAL_READ_MIN_BYTES];
    state.append_at(0, &cached);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = vec![0; HTTP_CACHE_PARTIAL_READ_MIN_BYTES * 2];

    assert!(matches!(
        cache.read_at_for_test(0, &mut output),
        CacheReadResult::Data(HTTP_CACHE_PARTIAL_READ_MIN_BYTES)
    ));
    assert_eq!(&output[..HTTP_CACHE_PARTIAL_READ_MIN_BYTES], &cached);
}

#[test]
fn http_ring_cache_read_at_returns_small_partial_buffer_without_waiting() {
    let mut state = HttpRingCacheState::new(0);
    let cached = vec![0x5a; HTTP_CACHE_PARTIAL_READ_MIN_BYTES / 2];
    state.append_at(0, &cached);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = vec![0; HTTP_CACHE_PARTIAL_READ_MIN_BYTES];

    assert!(matches!(
        cache.read_at_for_test(0, &mut output),
        CacheReadResult::Data(read) if read == cached.len()
    ));
    assert_eq!(&output[..cached.len()], &cached);
}

#[test]
fn http_ring_cache_state_reports_buffered_ahead_for_active_playback_range() {
    let mut state = HttpRingCacheState::new(10);
    state.append_at(10, b"abcdef");

    assert_eq!(state.buffered_ahead_from(10), 6);
    assert_eq!(state.buffered_ahead_from(13), 3);
    assert_eq!(state.buffered_ahead_from(16), 0);
    assert_eq!(state.buffered_ahead_from(9), 0);
    assert_eq!(state.buffered_ahead_from(17), 0);
}

#[test]
fn http_ring_cache_state_retains_cached_range_across_tail_metadata_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(state.copy_available(990, &mut output), Some(3));
    assert_eq!(&output, b"tai");
}

#[test]
fn http_ring_cache_state_hides_tail_metadata_range_from_progress() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_reports_playback_progress_while_tail_metadata_active() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    assert_eq!(
        state
            .playback_buffer_range()
            .map(PlaybackCacheByteRange::from),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_keeps_retained_ranges_after_playback_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");
    state.restart_at_with_kind(200, HttpCacheRangeKind::Playback);
    state.append_at(200, b"ghij");

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(
        state
            .playback_buffer_range()
            .map(PlaybackCacheByteRange::from),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.2,
            end_fraction: 0.204,
        })
    );
    assert_eq!(
        state.stream_cache_status_for_test().ranges,
        vec![
            PlaybackCacheByteRange {
                start_fraction: 0.1,
                end_fraction: 0.106,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.2,
                end_fraction: 0.204,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.99,
                end_fraction: 0.994,
            },
        ]
    );
}

#[test]
fn http_ring_cache_state_reports_near_tail_playback_range() {
    let mut state = HttpRingCacheState::new(980).with_content_len_hint(Some(1_000));
    state.append_at(980, b"tail");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.98,
            end_fraction: 0.984,
        })
    );
}

#[test]
fn http_ring_cache_state_classifies_far_tail_seek_as_metadata_probe() {
    let content_len = HTTP_CACHE_RANGE_REQUEST_BYTES * 4;
    let mut state = HttpRingCacheState::new(HTTP_CACHE_RANGE_REQUEST_BYTES)
        .with_content_len_hint(Some(content_len));
    state.append_at(HTTP_CACHE_RANGE_REQUEST_BYTES, b"abcdef");

    assert!(state.is_tail_metadata_probe_seek(content_len - 1024));
}

#[test]
fn http_ring_cache_state_treats_near_tail_active_range_as_playback() {
    let content_len = HTTP_CACHE_RANGE_REQUEST_BYTES * 4;
    let start_offset = content_len - HTTP_CACHE_RANGE_REQUEST_BYTES / 2;
    let mut state = HttpRingCacheState::new(start_offset).with_content_len_hint(Some(content_len));
    state.append_at(start_offset, b"abcdef");

    assert!(!state.is_tail_metadata_probe_seek(content_len - 1024));
}

#[test]
fn http_ring_cache_state_retains_cached_range_for_non_tail_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000_000_000));
    state.append_at(100, b"abcdef");

    state.restart_at(10_000);

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
}

#[test]
fn http_ring_cache_state_appends_side_range_without_restarting_active_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    assert!(state.append_retained_at(900, b"tail", HttpCacheRangeKind::TailMetadataProbe));

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(900, &mut output), Some(4));
    assert_eq!(&output, b"tail");
    assert_eq!(
        state.stream_cache_status_for_test().ranges,
        vec![
            PlaybackCacheByteRange {
                start_fraction: 0.1,
                end_fraction: 0.106,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.9,
                end_fraction: 0.904,
            },
        ]
    );
}

#[test]
fn http_ring_cache_state_appends_overlapping_side_range_without_duplicate_bytes() {
    let mut state =
        HttpRingCacheState::new_with_cache_capacity(0, 16).with_content_len_hint(Some(1_000));

    assert!(state.append_retained_at(900, b"tail", HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.append_retained_at(902, b"il!!", HttpCacheRangeKind::TailMetadataProbe));

    let mut output = [0; 6];
    assert_eq!(state.copy_available(900, &mut output), Some(6));
    assert_eq!(&output, b"tail!!");
    assert_eq!(state.stream_cache_status_for_test().cached_bytes, 6);
}

#[test]
fn http_ring_cache_state_prunes_least_recently_used_retained_range() {
    let mut state =
        HttpRingCacheState::new_with_cache_capacity(0, 8).with_content_len_hint(Some(1_000));
    state.append_at(0, b"abcd");
    state.restart_at(100);
    state.append_at(100, b"efgh");
    state.restart_at(200);

    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");

    state.append_at(200, b"ijkl");

    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
    assert_eq!(state.copy_available(100, &mut output), None);
    assert_eq!(state.copy_available(200, &mut output), Some(4));
    assert_eq!(&output, b"ijkl");
}

#[test]
fn http_ring_cache_state_counts_byte_level_seeks_outside_active_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.note_seek_offset(102, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 0);

    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 1);

    state.restart_at(500);
    state.append_at(500, b"ghij");
    state.note_seek_offset(102, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 2);
}

#[test]
fn http_ring_cache_state_reports_recent_raw_input_rate() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(1_000));

    state.append_at(0, b"abcdef");
    state.append_at(6, b"ghij");

    assert_eq!(
        state.stream_cache_status_for_test().raw_input_rate,
        Some(10)
    );
}

#[test]
fn http_ring_cache_state_reports_active_forward_diagnostics() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(1_000));

    state.append_at(0, b"abcdef");
    state.append_at(6, b"ghij");
    state.set_reader_offset(4);

    let status = state.stream_cache_status_for_test();

    assert_eq!(status.active_forward_bytes, 6);
    assert_eq!(status.active_forward_est_seconds, Some(0.6));
    assert_eq!(
        status.range_request_bytes_effective,
        HTTP_CACHE_RANGE_REQUEST_BYTES
    );
}

#[test]
fn http_ring_cache_state_uses_content_length_hint_for_progress() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(100));

    state.append_at(0, b"abcde");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.0,
            end_fraction: 0.05,
        })
    );
}

#[test]
fn http_ring_cache_state_trims_oldest_bytes() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.set_reader_offset(102);
    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 102);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(102, &mut output), Some(4));
    assert_eq!(&output, b"cdef");
    assert_eq!(state.copy_available(100, &mut output), None);
}

#[test]
fn http_ring_cache_state_copies_wrapped_bytes() {
    let mut state = HttpRingCacheState::new_with_cache_capacity(0, 6);
    state.append_at(0, b"abcdef");

    state.set_reader_offset(4);
    state.append_at(6, b"ghij");

    assert_eq!(state.base_offset, 4);
    assert_eq!(state.next_offset, 10);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(4, &mut output), Some(6));
    assert_eq!(&output, b"efghij");
}

#[test]
fn http_ring_cache_state_preserves_unread_bytes_when_over_capacity() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(100, &mut output), Some(6));
    assert_eq!(&output, b"abcdef");
}

#[test]
fn http_ring_cache_state_refuses_append_when_capacity_is_unread() {
    let mut state = HttpRingCacheState::new_with_cache_capacity(0, 4);
    assert!(state.append_at(0, b"abcd"));

    assert!(!state.append_at(4, b"ef"));

    assert_eq!(state.base_offset, 0);
    assert_eq!(state.next_offset, 4);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
}

#[test]
fn http_ring_cache_state_limits_prefetch_window_from_reader() {
    let mut state = HttpRingCacheState::new(100);
    let active_capacity =
        HTTP_RING_CACHE_CAPACITY - usize::try_from(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES).unwrap();

    assert_eq!(state.append_capacity_from(100 + active_capacity as u64), 0);

    let mut state = HttpRingCacheState::new(100);
    assert_eq!(state.append_capacity_from(99 + active_capacity as u64), 1);

    state.set_reader_offset(200);
    assert_eq!(
        state.append_capacity_from(100 + active_capacity as u64),
        100
    );
}

#[test]
fn http_ring_cache_state_pauses_prefetch_until_hysteresis_resume() {
    let mut state = HttpRingCacheState::new_with_readahead_for_test(0, 1_000, 10.0, 2.0)
        .with_content_len_hint(Some(10_000));
    state.set_duration_seconds_for_test(100.0);

    assert_eq!(state.append_capacity_from(875), 0);
    assert_eq!(state.append_capacity_from(874), 0);
    assert_eq!(state.append_capacity_from(675), 200);
}

#[test]
fn http_ring_cache_state_applies_live_cache_config() {
    let mut state = HttpRingCacheState::new_with_readahead_for_test(0, 1_000, 10.0, 2.0)
        .with_content_len_hint(Some(1_000));
    state.set_duration_seconds_for_test(100.0);

    assert_eq!(state.append_capacity_from(0), 100);

    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 2.0,
        demuxer_readahead_secs: 1.0,
        demuxer_hysteresis_secs: 0.5,
        ..PlaybackCacheConfig::default()
    };
    state.apply_cache_config(&config);

    assert_eq!(state.append_capacity_from(0), 20);
}

#[test]
fn http_ring_cache_applies_live_memory_budget_from_cache_config() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        1024 * 1024,
    ));
    let config = PlaybackCacheConfig {
        http_cache_max_bytes: 128 * 1024,
        http_cache_chunk_bytes: 64 * 1024,
        ..PlaybackCacheConfig::default()
    };

    cache.apply_cache_config(&config);

    assert_eq!(cache.memory_capacity_for_test(), 128 * 1024);
}

#[test]
fn http_ring_cache_applies_live_range_request_budget_from_cache_config() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        1024 * 1024,
    ));
    let config = PlaybackCacheConfig {
        http_cache_chunk_bytes: 64 * 1024,
        http_cache_range_request_bytes: 2 * 1024 * 1024,
        ..PlaybackCacheConfig::default()
    };

    cache.apply_cache_config(&config);

    assert_eq!(cache.range_request_bytes_for_test(), 2 * 1024 * 1024);
}

#[test]
fn http_ring_cache_state_reads_trimmed_bytes_from_disk_cache() {
    let mut state =
        HttpRingCacheState::new_with_disk_cache_for_test(0, 4, 16).with_content_len_hint(Some(8));
    assert!(state.append_at(0, b"abcd"));

    state.set_reader_offset(4);
    assert!(state.append_at(4, b"efgh"));

    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
}

#[test]
fn http_ring_cache_state_counts_overlapping_memory_and_disk_bytes_once() {
    let mut state =
        HttpRingCacheState::new_with_disk_cache_for_test(0, 4, 16).with_content_len_hint(Some(8));

    assert!(state.append_at(0, b"abcd"));
    assert_eq!(state.stream_cache_status_for_test().cached_bytes, 4);

    state.set_reader_offset(4);
    assert!(state.append_at(4, b"efgh"));

    let status = state.stream_cache_status_for_test();
    assert_eq!(status.cached_bytes, 8);
    assert_eq!(
        status.ranges,
        vec![PlaybackCacheByteRange {
            start_fraction: 0.0,
            end_fraction: 1.0,
        }]
    );
}

#[test]
fn http_ring_cache_state_uses_active_range_for_prefetch_window() {
    let tail_offset = 100 + HTTP_RING_CACHE_CAPACITY as u64 + 1_000;
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(tail_offset + 1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(tail_offset, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(tail_offset, b"tail");
    state.set_reader_offset(102);

    assert_eq!(
        state.append_capacity_from(tail_offset + 4),
        HTTP_RING_CACHE_CAPACITY - 4
    );
}

#[test]
fn http_ring_cache_state_demotes_active_range_on_seek_outside_cached_range() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.note_seek_offset(10_000, HttpCacheRangeKind::Playback);
    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 106);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(100, &mut output), Some(6));
    assert_eq!(&output, b"abcdef");
}

#[test]
fn http_ring_cache_state_restart_clears_eof_for_next_range() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.eof = true;

    state.restart_at(0);

    assert_eq!(state.base_offset, 0);
    assert_eq!(state.next_offset, 0);
    assert!(!state.eof);
}

#[test]
fn content_range_parser_reads_total_size() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 100-199/12345"),
    );

    assert_eq!(content_len_from_content_range(&headers), Some(12345));
    assert_eq!(
        content_range_from_headers(&headers),
        Some(HttpContentRange {
            start: 100,
            end: 199,
            total: Some(12345),
        })
    );
}

#[test]
fn content_range_parser_reads_unknown_total_and_rejects_invalid_ranges() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 2048-4095/*"),
    );

    assert_eq!(
        content_range_from_headers(&headers),
        Some(HttpContentRange {
            start: 2048,
            end: 4095,
            total: None,
        })
    );
    assert_eq!(content_len_from_content_range(&headers), None);

    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 4095-2048/8192"),
    );
    assert_eq!(content_range_from_headers(&headers), None);
}

#[test]
fn playback_scheduler_reports_ready_for_past_frames() {
    let mut scheduler = PlaybackScheduler::new(1_000_000_000);
    let control = FfmpegControl::new(PlaybackSessionId::default());

    assert_eq!(
        scheduler.wait_until(500_000_000, &control),
        WaitStatus::Ready
    );
}
