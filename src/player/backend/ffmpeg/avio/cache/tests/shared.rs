use std::sync::{Arc, Condvar, Mutex, mpsc};

use crate::player::{
    backend::{BackendEventKind, ByteCacheState, PlaybackCacheState},
    render_host::PlaybackSessionId,
};

use super::super::{
    CacheAppendPermit, CacheReadResult, CacheRestartRequest, FfmpegControl,
    HTTP_CACHE_RANGE_REQUEST_BYTES, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES, HttpCacheConfig,
    HttpCacheRangeKind, HttpRingCache, HttpRingCacheShared, HttpRingCacheState,
};

#[test]
fn http_cache_read_error_does_not_poison_cached_prefix() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(1_000));
    assert!(state.append_at(0, b"abcdef"));
    let cache = HttpRingCache::from_state_for_test(state);
    cache.shared.set_error_at(6, "range failed".to_string());

    let mut cached = [0; 6];
    assert!(matches!(
        cache.read_at_for_test(0, &mut cached),
        CacheReadResult::Data(6)
    ));
    assert_eq!(&cached, b"abcdef");

    let mut missing = [0; 1];
    assert!(matches!(
        cache.read_at_for_test(6, &mut missing),
        CacheReadResult::Error(error) if error == "range failed"
    ));
}

#[test]
fn http_cache_read_error_waits_while_side_range_can_recover_gap() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    cache.shared.set_error_at(500, "temporary gap".to_string());
    let request = CacheRestartRequest {
        offset: 500,
        range_kind: HttpCacheRangeKind::Playback,
    };
    cache
        .shared
        .state
        .lock()
        .expect("state locks")
        .side_download_active
        .push(request);

    let mut output = [0; 1];
    assert!(matches!(
        cache.read_cached_at(500, &mut output),
        CacheReadResult::WouldBlock
    ));
}

#[test]
fn http_cache_successful_side_append_clears_matching_read_error() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    cache.shared.set_error_at(500, "temporary gap".to_string());
    let request = CacheRestartRequest {
        offset: 500,
        range_kind: HttpCacheRangeKind::Playback,
    };
    cache
        .shared
        .state
        .lock()
        .expect("state locks")
        .side_download_active
        .push(request);

    assert!(matches!(
        cache
            .shared
            .append_side_download_or_stop(request, 500, b"x"),
        super::super::CacheAppendResult::Appended
    ));
    assert!(
        cache
            .shared
            .state
            .lock()
            .expect("state locks")
            .error
            .is_none()
    );
}

#[test]
fn http_cache_tail_side_failure_does_not_set_playback_error() {
    let (event_tx, _) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new(0).with_content_len_hint(Some(1_000))),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        offset: 900,
        range_kind: HttpCacheRangeKind::TailMetadataProbe,
    };
    shared
        .state
        .lock()
        .expect("state locks")
        .side_download_active
        .push(request);

    shared.finish_side_download_with_error(request, 900, "tail failed".to_string());

    assert!(shared.state.lock().expect("state locks").error.is_none());
}

#[test]
fn http_cache_playback_side_failure_only_sets_error_for_active_reader_range() {
    let (event_tx, _) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new(0).with_content_len_hint(Some(1_000))),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        offset: 500,
        range_kind: HttpCacheRangeKind::Playback,
    };
    {
        let mut guard = shared.state.lock().expect("state locks");
        guard.reader_offset = 500;
        guard.side_download_active.push(request);
        assert!(guard.append_retained_at_protected(500, &[0; 20], request));
    }

    shared.finish_side_download_with_error(request, 520, "playback failed".to_string());

    let guard = shared.state.lock().expect("state locks");
    let error = guard.error.as_ref().expect("active reader receives error");
    assert_eq!(error.offset, 520);
    assert_eq!(error.message, "playback failed");
}

#[test]
fn http_cache_playback_side_failure_ahead_of_reader_stays_background_only() {
    let (event_tx, _) = mpsc::channel();
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new(0).with_content_len_hint(Some(1_000))),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        offset: 500,
        range_kind: HttpCacheRangeKind::Playback,
    };
    shared
        .state
        .lock()
        .expect("state locks")
        .side_download_active
        .push(request);

    shared.finish_side_download_with_error(request, 500, "prefetch failed".to_string());

    assert!(shared.state.lock().expect("state locks").error.is_none());
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
        guard.set_reader_offset(500);
        assert!(guard.request_side_download_at(500, HttpCacheRangeKind::Playback));
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
fn http_cache_playback_status_skips_busy_state_lock() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    {
        let mut guard = cache.shared.state.lock().expect("state locks");
        assert!(guard.append_at(0, b"abcdef"));
    }
    assert!(cache.try_playback_byte_cache_status().is_some());

    let _guard = cache.shared.state.lock().expect("state locks");

    assert!(cache.try_playback_byte_cache_status().is_none());
}

#[test]
fn http_cache_shared_uses_small_range_for_initial_empty_playback_request() {
    let (event_tx, _) = mpsc::channel();
    let config = HttpCacheConfig {
        range_request_bytes: HTTP_CACHE_RANGE_REQUEST_BYTES,
        ..HttpCacheConfig::for_test(500 * 1024 * 1024)
    };
    let shared = HttpRingCacheShared {
        state: Mutex::new(HttpRingCacheState::new_with_config(0, config)),
        ready: Condvar::new(),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };

    assert_eq!(
        shared.playback_range_request_bytes(0),
        HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES
    );

    {
        let mut guard = shared.state.lock().expect("state locks");
        assert!(guard.append_at(0, b"abcdef"));
    }

    assert_eq!(
        shared.playback_range_request_bytes(6),
        HTTP_CACHE_RANGE_REQUEST_BYTES
    );
    assert_eq!(
        shared.playback_range_request_bytes(0),
        HTTP_CACHE_RANGE_REQUEST_BYTES
    );
}

#[test]
fn http_cache_shared_splices_retained_playback_range_on_capacity_check() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(64)),
    );
    {
        let mut guard = cache.shared.state.lock().expect("state locks");
        assert!(guard.append_at(0, b"abcdef"));
        assert!(guard.append_retained_at(6, b"ghijkl", HttpCacheRangeKind::Playback));
    }

    match cache.shared.append_capacity_now(6) {
        CacheAppendPermit::Restart(next_offset) => assert_eq!(next_offset, 12),
        CacheAppendPermit::Ready(_) => panic!("expected retained playback splice restart"),
        CacheAppendPermit::Full => panic!("expected retained playback splice restart"),
        CacheAppendPermit::Stopped => panic!("expected retained playback splice restart"),
    }

    let mut output = [0; 12];
    let mut guard = cache.shared.state.lock().expect("state locks");
    assert_eq!(guard.copy_available(0, &mut output), Some(12));
    assert_eq!(&output, b"abcdefghijkl");
    assert_eq!(guard.next_offset, 12);
    assert!(guard.retained_ranges.is_empty());
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
