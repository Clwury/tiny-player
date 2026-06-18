use std::sync::{Arc, Condvar, Mutex, mpsc};

use crate::player::{
    backend::{BackendEventKind, ByteCacheState, CacheUnlinkPolicy, PlaybackCacheState},
    render_host::PlaybackSessionId,
};

use super::{
    CacheReadResult, CacheRestartRequest, FfmpegControl, HTTP_CACHE_RANGE_REQUEST_BYTES,
    HttpCacheConfig, HttpCacheRangeKind, HttpDiskCache, HttpRingCache, HttpRingCacheShared,
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
fn http_cache_status_reports_byte_level_seek_count_changes() {
    let mut state = HttpRingCacheState::new(100);
    assert!(state.append_at(100, b"abcdef"));
    assert!(state.take_stream_cache_status_report().is_some());
    assert!(state.take_stream_cache_status_report().is_none());

    state.note_seek_offset(500, HttpCacheRangeKind::Playback);

    let status = state
        .take_stream_cache_status_report()
        .expect("byte-level seek count change is reportable");
    assert_eq!(status.byte_level_seeks, 1);
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
fn http_cache_status_is_not_idle_while_side_download_is_pending() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    state.queue_read_miss_at(500);

    assert_eq!(state.append_capacity_from(106), 0);

    assert!(!state.stream_cache_status().idle);
}

#[test]
fn http_cache_status_reports_side_download_queue_as_active_work() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.prefetch_paused = true;
    assert!(state.stream_cache_status().idle);
    assert!(state.take_stream_cache_status_report().is_some());

    assert!(state.queue_read_miss_at(500));

    let status = state
        .take_stream_cache_status_report()
        .expect("side download activity is reportable");
    assert!(!status.idle);
}

#[test]
fn http_cache_probe_read_reports_queued_side_download_activity() {
    let (event_tx, event_rx) = mpsc::channel();
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.prefetch_paused = true;
    assert!(state.take_stream_cache_status_report().is_some());
    let cache = HttpRingCache {
        shared: Arc::new(HttpRingCacheShared {
            state: Mutex::new(state),
            ready: Condvar::new(),
            control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
            event_tx,
        }),
    };
    let mut output = [0; 4];

    assert!(matches!(
        cache.read_cached_at(500, &mut output),
        CacheReadResult::WouldBlock
    ));

    let event = event_rx
        .try_recv()
        .expect("queued side download status event is sent");
    assert!(matches!(
        event.kind,
        BackendEventKind::CacheStateChanged(PlaybackCacheState {
            byte: Some(ByteCacheState { idle: false, .. }),
            ..
        })
    ));
}

#[test]
fn http_cache_status_is_idle_after_eof_without_prefetch_pause() {
    let mut state = HttpRingCacheState::new(0);
    assert!(state.append_at(0, b"abcdef"));
    state.eof = true;

    assert!(state.stream_cache_status().idle);
}

#[test]
fn http_cache_shared_reports_idle_when_eof_reached() {
    let (event_tx, event_rx) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new(0)),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    {
        let mut guard = shared.state.lock().expect("state locks");
        assert!(guard.append_at(0, b"abcdef"));
        assert!(!guard.stream_cache_status().idle);
        assert!(guard.take_stream_cache_status_report().is_some());
    }

    shared.mark_eof();

    let event = event_rx.try_recv().expect("EOF status event is sent");
    assert!(matches!(
        event.kind,
        BackendEventKind::CacheStateChanged(PlaybackCacheState {
            byte: Some(ByteCacheState { idle: true, .. }),
            ..
        })
    ));
}

#[test]
fn http_cache_shared_reports_idle_after_last_side_download_finishes() {
    let (event_tx, event_rx) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new(100).with_content_len_hint(Some(1_000))),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = {
        let mut guard = shared.state.lock().expect("state locks");
        assert!(guard.append_at(100, b"abcdef"));
        guard.note_seek_offset(500, HttpCacheRangeKind::Playback);
        guard.queue_read_miss_at(500);
        assert_eq!(guard.append_capacity_from(106), 0);
        assert!(!guard.stream_cache_status().idle);
        assert!(guard.take_stream_cache_status_report().is_some());
        let request = guard
            .side_download_requests
            .pop_front()
            .expect("side download was queued");
        guard.side_download_active.push(request);
        request
    };

    shared.finish_side_download(request, false);

    let event = event_rx
        .try_recv()
        .expect("side completion status event is sent");
    assert!(matches!(
        event.kind,
        BackendEventKind::CacheStateChanged(PlaybackCacheState {
            byte: Some(ByteCacheState { idle: true, .. }),
            ..
        })
    ));
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

#[test]
fn http_cache_shared_dispatches_multiple_side_downloads_to_active_set() {
    let (event_tx, _) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(
            HttpRingCacheState::new(100)
                .with_content_len_hint(Some(HTTP_CACHE_RANGE_REQUEST_BYTES * 4)),
        ),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    {
        let mut guard = shared.state.lock().expect("state locks");
        guard.request_side_download_at(1_000, HttpCacheRangeKind::TailMetadataProbe);
        guard.request_side_download_at(
            1_000 + HTTP_CACHE_RANGE_REQUEST_BYTES + 1,
            HttpCacheRangeKind::TailMetadataProbe,
        );
    }

    let first = shared
        .wait_for_side_download_request()
        .expect("first request dequeues");
    let second = shared
        .wait_for_side_download_request()
        .expect("second request dequeues");

    {
        let guard = shared.state.lock().expect("state locks");
        assert!(guard.side_download_requests.is_empty());
        assert_eq!(guard.side_download_active, vec![first, second]);
    }
    shared.finish_side_download(first, true);
    let guard = shared.state.lock().expect("state locks");
    assert_eq!(guard.side_download_active, vec![second]);
}

#[test]
fn http_disk_cache_unlinks_immediately_but_keeps_open_file_usable() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let mut disk_cache = HttpDiskCache::new(
        1024,
        Some(dir.path().to_path_buf()),
        CacheUnlinkPolicy::Immediate,
    )
    .expect("disk cache creates");
    let path = disk_cache.path.clone();

    assert!(!path.exists());
    disk_cache.write_at(0, b"payload").expect("payload writes");
    let mut restored = [0; 7];

    assert_eq!(disk_cache.read_at(0, &mut restored), Some(7));
    assert_eq!(&restored, b"payload");
}

#[test]
fn http_disk_cache_prunes_least_recently_used_range() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let mut disk_cache = HttpDiskCache::new(
        8,
        Some(dir.path().to_path_buf()),
        CacheUnlinkPolicy::WhenDone,
    )
    .expect("disk cache creates");
    disk_cache.write_at(0, b"aaaa").expect("first range writes");
    disk_cache
        .write_at(10, b"bbbb")
        .expect("second range writes");
    let mut restored = [0; 1];
    assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));

    disk_cache
        .write_at(20, b"cccc")
        .expect("third range writes");

    assert!(disk_cache.read_at(10, &mut restored).is_none());
    assert_eq!(disk_cache.read_at(0, &mut restored), Some(1));
    assert_eq!(restored[0], b'a');
    assert_eq!(disk_cache.read_at(20, &mut restored), Some(1));
    assert_eq!(restored[0], b'c');
}

#[test]
fn http_disk_cache_removes_file_when_done() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let path = {
        let disk_cache = HttpDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::WhenDone,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();
        assert!(path.exists());
        path
    };

    assert!(!path.exists());
}

#[test]
fn http_disk_cache_can_leave_file_for_inspection() {
    let dir = tempfile::tempdir().expect("temp dir creates");
    let path = {
        let disk_cache = HttpDiskCache::new(
            1024,
            Some(dir.path().to_path_buf()),
            CacheUnlinkPolicy::Never,
        )
        .expect("disk cache creates");
        let path = disk_cache.path.clone();
        assert!(path.exists());
        path
    };

    assert!(path.exists());
    std::fs::remove_file(path).expect("leftover cache file removes");
}
