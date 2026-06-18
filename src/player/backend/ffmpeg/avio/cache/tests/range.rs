use super::super::{
    CacheRestartRequest, HTTP_CACHE_RANGE_REQUEST_BYTES, HttpCacheConfig, HttpCacheRangeKind,
    HttpRingCacheState,
};

#[test]
fn http_cache_state_queues_tail_side_download_without_active_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));

    state.request_side_download_at(990, HttpCacheRangeKind::TailMetadataProbe);

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    assert!(state.restart_request.is_none());
    assert_eq!(
        state
            .side_download_requests
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![CacheRestartRequest {
            offset: 990,
            range_kind: HttpCacheRangeKind::TailMetadataProbe,
        }]
    );
    assert!(state.side_download_may_produce(990));
}
#[test]
fn http_cache_state_queues_playback_read_miss_without_active_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));

    state.queue_read_miss_at(500);

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    assert!(state.restart_request.is_none());
    assert_eq!(
        state
            .side_download_requests
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![CacheRestartRequest {
            offset: 500,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
    assert!(state.side_download_may_produce(500));
}
#[test]
fn http_cache_state_proactively_queues_next_playback_range() {
    let config = HttpCacheConfig {
        range_request_bytes: 100,
        ..HttpCacheConfig::for_test(1_000)
    };
    let mut state =
        HttpRingCacheState::new_with_config(0, config).with_content_len_hint(Some(1_000));

    assert!(state.append_at(0, &[0; 49]));
    assert!(state.side_download_requests.is_empty());

    assert!(state.append_at(49, &[0; 1]));

    assert_eq!(
        state
            .side_download_requests
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![CacheRestartRequest {
            offset: 100,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
}
#[test]
fn http_cache_state_demotes_active_range_when_playback_seek_leaves_it() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));

    state.note_seek_offset(500, HttpCacheRangeKind::Playback);

    assert_eq!(state.base_offset, 106);
    assert_eq!(state.next_offset, 106);
    assert_eq!(state.reader_offset, 500);
    assert_eq!(state.stream_cache_status().byte_level_seeks, 1);
    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
}
#[test]
fn http_cache_state_pauses_inactive_active_prefetch_while_side_range_can_serve_reader() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    state.queue_read_miss_at(500);

    assert_eq!(state.append_capacity_from(106), 0);
    assert!(state.prefetch_paused);
}
#[test]
fn http_cache_state_schedules_active_continuation_after_playback_side_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    state.queue_read_miss_at(500);
    let request = state
        .side_download_requests
        .pop_front()
        .expect("side download was queued");
    state.side_download_active.push(request);
    assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

    state.finish_side_download_request(request, true);

    assert!(state.side_download_active.is_empty());
    assert_eq!(
        state.restart_request,
        Some(CacheRestartRequest {
            offset: 504,
            range_kind: HttpCacheRangeKind::Playback,
        })
    );
}
#[test]
fn http_cache_state_marks_eof_instead_of_continuing_after_terminal_side_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(504));
    assert!(state.append_at(100, b"abcdef"));
    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    state.queue_read_miss_at(500);
    let request = state
        .side_download_requests
        .pop_front()
        .expect("side download was queued");
    state.side_download_active.push(request);
    assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

    state.finish_side_download_request(request, true);

    assert!(state.side_download_active.is_empty());
    assert!(state.restart_request.is_none());
    assert!(state.eof);
    assert!(state.stream_cache_status_for_test().idle);
}
#[test]
fn http_cache_state_does_not_schedule_active_continuation_for_incomplete_side_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    state.queue_read_miss_at(500);
    let request = state
        .side_download_requests
        .pop_front()
        .expect("side download was queued");
    state.side_download_active.push(request);
    assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

    state.finish_side_download_request(request, false);

    assert!(state.side_download_active.is_empty());
    assert!(state.restart_request.is_none());
}
#[test]
fn http_cache_state_does_not_schedule_stale_active_continuation_after_side_range() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(1_000));
    assert!(state.append_at(0, &vec![0; 600]));
    state.set_reader_offset(500);
    let request = CacheRestartRequest {
        offset: 500,
        range_kind: HttpCacheRangeKind::Playback,
    };
    state.side_download_active.push(request);
    assert!(state.append_retained_at(500, b"side", HttpCacheRangeKind::Playback));

    state.finish_side_download_request(request, true);

    assert!(state.side_download_active.is_empty());
    assert!(state.restart_request.is_none());
}
#[test]
fn http_cache_state_splices_proactive_playback_range_at_active_end() {
    let config = HttpCacheConfig {
        range_request_bytes: 6,
        ..HttpCacheConfig::for_test(64)
    };
    let mut state = HttpRingCacheState::new_with_config(0, config).with_content_len_hint(Some(64));
    assert!(state.append_at(0, b"abcdef"));
    assert!(state.append_retained_at(6, b"ghijkl", HttpCacheRangeKind::Playback));

    assert_eq!(state.splice_retained_playback_at_active_end(6), Some(12));

    let mut output = [0; 12];
    assert_eq!(state.copy_available(0, &mut output), Some(12));
    assert_eq!(&output, b"abcdefghijkl");
    assert_eq!(state.next_offset, 12);
    assert_eq!(state.active_request_start_offset, 6);
    assert!(state.retained_ranges.is_empty());
}
#[test]
fn http_cache_state_does_not_queue_stale_proactive_playback_continuation() {
    let config = HttpCacheConfig {
        range_request_bytes: 6,
        ..HttpCacheConfig::for_test(64)
    };
    let mut state = HttpRingCacheState::new_with_config(0, config).with_content_len_hint(Some(64));

    assert!(state.append_at(0, b"abcdefghijkl"));

    assert!(state.side_download_requests.is_empty());
}
#[test]
fn http_cache_state_does_not_queue_side_download_for_cached_offset() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));

    state.request_side_download_at(102, HttpCacheRangeKind::TailMetadataProbe);

    assert!(state.side_download_requests.is_empty());
}
#[test]
fn http_cache_state_queues_multiple_side_downloads_and_suppresses_duplicates() {
    let mut state = HttpRingCacheState::new(100)
        .with_content_len_hint(Some(HTTP_CACHE_RANGE_REQUEST_BYTES * 4));

    state.request_side_download_at(1_000, HttpCacheRangeKind::TailMetadataProbe);
    state.request_side_download_at(
        1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES / 2,
        HttpCacheRangeKind::TailMetadataProbe,
    );
    state.request_side_download_at(
        1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
        HttpCacheRangeKind::TailMetadataProbe,
    );

    assert_eq!(
        state
            .side_download_requests
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![
            CacheRestartRequest {
                offset: 1_000,
                range_kind: HttpCacheRangeKind::TailMetadataProbe,
            },
            CacheRestartRequest {
                offset: 1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
                range_kind: HttpCacheRangeKind::TailMetadataProbe,
            },
        ]
    );
}
#[test]
fn http_cache_state_uses_configured_side_download_range_request_budget() {
    let config = HttpCacheConfig {
        range_request_bytes: 1024,
        ..HttpCacheConfig::for_test(1024)
    };
    let mut state =
        HttpRingCacheState::new_with_config(100, config).with_content_len_hint(Some(10_000));

    state.request_side_download_at(1_000, HttpCacheRangeKind::Playback);
    state.request_side_download_at(1_500, HttpCacheRangeKind::Playback);
    state.request_side_download_at(2_025, HttpCacheRangeKind::Playback);

    assert_eq!(
        state
            .side_download_requests
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![
            CacheRestartRequest {
                offset: 1_000,
                range_kind: HttpCacheRangeKind::Playback,
            },
            CacheRestartRequest {
                offset: 2_025,
                range_kind: HttpCacheRangeKind::Playback,
            },
        ]
    );
}
