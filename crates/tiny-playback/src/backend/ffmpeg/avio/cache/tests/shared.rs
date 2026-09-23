use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    backend::{BackendEventKind, ByteCacheState, PlaybackCacheConfig, PlaybackCacheState},
    render_host::PlaybackSessionId,
};

use super::super::{
    CacheAppendPermit, CacheReadResult, CacheRestartRequest, FfmpegControl,
    HTTP_CACHE_RANGE_REQUEST_BYTES, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES, HttpCacheConfig,
    HttpCacheRangeKind, HttpRingCache, HttpRingCacheShared, HttpRingCacheState,
};

#[test]
fn recovery_demand_bypasses_demux_pause_but_keeps_the_byte_capacity_limit() {
    let cache =
        HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(0, 128));
    cache.update_demux_high_water_prefetch_paused(100, 100, true, false);
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Full
    ));
    cache.set_recovery_input_required(true);
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Ready(_)
    ));
    let capacity = cache.shared.state.lock().unwrap().active_memory_capacity();
    cache.shared.append_or_restart(0, &vec![0; capacity]);
    assert!(matches!(
        cache.shared.append_capacity_now(capacity as u64),
        CacheAppendPermit::Full
    ));
    cache.set_recovery_input_required(false);
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Full
    ));
}

#[test]
fn an_avio_read_unblocks_http_despite_a_stale_demux_pause() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new(0));
    cache.update_demux_high_water_prefetch_paused(100, 100, true, false);
    let reader = cache.clone();
    let worker = thread::spawn(move || {
        let mut bytes = [0; 1];
        assert!(matches!(
            reader.read_at(0, &mut bytes),
            CacheReadResult::Data(1)
        ));
        assert_eq!(bytes, [7]);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while cache.shared.active_readers.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Ready(_)
    ));
    cache.shared.append_or_restart(0, &[7]);
    worker.join().unwrap();
    assert!(matches!(
        cache.shared.append_capacity_now(1),
        CacheAppendPermit::Full
    ));
}

#[test]
fn a_new_playback_position_interrupts_retry_backoff_and_resets_the_active_range() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new(0));
    let retry = cache.clone();
    let generation = cache.shared.download_generation();
    let worker = thread::spawn(move || {
        retry
            .shared
            .wait_for_retry_delay(Duration::from_secs(2), generation, None)
    });
    cache.note_reader_offset(1_000_000, HttpCacheRangeKind::Playback);
    assert!(worker.join().unwrap());
    assert_eq!(cache.shared.take_restart_offset(), Some(1_000_000));
    assert_eq!(cache.shared.download_offset(), 1_000_000);
    assert!(cache.shared.state.lock().unwrap().restart_request.is_none());
}

#[test]
fn late_bytes_headers_and_eof_from_an_old_request_do_not_change_the_new_range() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000_000)),
    );
    let generation = cache.shared.download_generation();
    cache.note_reader_offset(500_000, HttpCacheRangeKind::Playback);
    cache.shared.take_restart_offset();
    assert!(matches!(
        cache
            .shared
            .append_download_bytes(generation, 500_000, b"old"),
        super::super::CacheAppendResult::Restart(500_000)
    ));
    cache.shared.set_content_len(generation, Some(12));
    assert!(!cache.shared.mark_eof(generation));
    assert_eq!(cache.content_len(), Some(1_000_000));
    assert!(!cache.has_cached_byte_at(500_000));
    assert!(!cache.shared.state.lock().unwrap().eof);
}

#[test]
fn an_old_side_completion_cannot_remove_a_replacement_request_at_the_same_offset() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new(0));
    let old = CacheRestartRequest {
        generation: 0,
        offset: 900,
        range_kind: HttpCacheRangeKind::TailMetadataProbe,
    };
    cache
        .shared
        .state
        .lock()
        .unwrap()
        .side_download_active
        .push(old);
    cache.note_reader_offset(500_000, HttpCacheRangeKind::Playback);
    let replacement = CacheRestartRequest {
        generation: cache.shared.download_generation(),
        ..old
    };
    cache
        .shared
        .state
        .lock()
        .unwrap()
        .side_download_active
        .push(replacement);
    assert!(cache.shared.request_cancelled(old.generation, Some(old)));
    assert!(matches!(
        cache.shared.append_side_download_or_stop(old, 900, b"old"),
        super::super::CacheAppendResult::Stopped
    ));
    cache
        .shared
        .finish_side_download_with_error(old, 900, "obsolete error".into());
    cache.shared.finish_side_download(old, true);
    let guard = cache.shared.state.lock().unwrap();
    assert_eq!(guard.side_download_active, vec![replacement]);
    assert!(guard.error.is_none());
    assert!(!guard.cached_range_contains(900));
}

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
fn pending_cached_seek_does_not_interrupt_a_blocked_http_cache_reader() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    let control = Arc::clone(&cache.shared.control);
    let waiting_cache = cache.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut output = [0; 1];
        started_tx.send(()).expect("reader start signal sends");
        let result = waiting_cache.read_at_for_test(0, &mut output);
        result_tx
            .send((result, output))
            .expect("reader result signal sends");
    });
    started_rx.recv().expect("reader starts");

    let seek_generation = control.request_seek();
    assert!(
        result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "pending cached seek must not turn a blocked AVIO read into EIO"
    );

    control.finish_seek(seek_generation);
    assert!(matches!(
        cache.shared.append_or_restart(0, b"x"),
        super::super::CacheAppendResult::Appended
    ));

    let (result, output) = result_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cached data unblocks the AVIO reader");
    assert!(matches!(result, CacheReadResult::Data(1)));
    assert_eq!(output, *b"x");
    reader.join().expect("HTTP cache reader joins");
}

#[test]
fn http_cache_read_error_waits_while_side_range_can_recover_gap() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    cache.shared.set_error_at(500, "temporary gap".to_string());
    let request = CacheRestartRequest {
        generation: 0,
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
        generation: 0,
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        generation: 0,
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        generation: 0,
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    let request = CacheRestartRequest {
        generation: 0,
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    };
    {
        let mut guard = shared.state.lock().expect("state locks");
        assert!(guard.append_at(0, b"abcdef"));
        assert!(!guard.stream_cache_status().idle);
        assert!(guard.take_stream_cache_status_report().is_some());
    }

    shared.mark_eof(shared.download_generation());

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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
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
fn http_cache_config_update_defers_busy_lock_and_keeps_only_latest_generation() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    let first = PlaybackCacheConfig {
        disk_cache: false,
        http_cache_max_bytes: 128 * 1024,
        http_cache_chunk_bytes: 64 * 1024,
        ..PlaybackCacheConfig::default()
    };
    let latest = PlaybackCacheConfig {
        disk_cache: false,
        http_cache_max_bytes: 256 * 1024,
        http_cache_chunk_bytes: 64 * 1024,
        ..PlaybackCacheConfig::default()
    };
    let guard = cache.shared.state.lock().expect("state locks");
    let started_at = Instant::now();

    cache.apply_cache_config(&first);
    cache.apply_cache_config(&latest);

    assert!(
        started_at.elapsed() < Duration::from_millis(10),
        "contended config update unexpectedly waited for HTTP state lock"
    );
    drop(guard);

    let deadline = Instant::now() + Duration::from_secs(1);
    while cache.memory_capacity_for_test() != 256 * 1024 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(cache.memory_capacity_for_test(), 256 * 1024);
}

#[test]
fn http_cache_demux_waterline_resumes_immediately_below_full_without_state_lock() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );
    let _guard = cache.shared.state.lock().expect("state locks");
    let started_at = Instant::now();

    assert!(!cache.update_demux_high_water_prefetch_paused(89, 100, false, false));
    assert!(!cache.update_demux_high_water_prefetch_paused(90, 100, false, false));
    assert!(!cache.update_demux_high_water_prefetch_paused(99, 100, false, false));
    assert!(cache.update_demux_high_water_prefetch_paused(100, 100, true, false));
    assert!(cache.shared.demux_high_water_paused.load(Ordering::Acquire));
    // A small forward seek must refill even when more than 75% remains.
    assert!(cache.update_demux_high_water_prefetch_paused(99, 100, false, false));
    assert!(!cache.update_demux_high_water_prefetch_paused(90, 100, false, false));
    assert!(!cache.shared.demux_high_water_paused.load(Ordering::Acquire));
    assert!(
        started_at.elapsed() < Duration::from_millis(10),
        "atomic demux waterline update unexpectedly blocked"
    );
}

#[test]
fn http_cache_demux_underrun_resumes_prefetch_above_high_water() {
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
    );

    assert!(cache.update_demux_high_water_prefetch_paused(100, 100, true, false));
    assert!(cache.update_demux_high_water_prefetch_paused(100, 100, true, true));
    assert!(!cache.shared.demux_high_water_paused.load(Ordering::Acquire));
}

#[test]
fn http_cache_demux_time_limit_is_honored_without_a_byte_limit() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new(0));
    assert!(cache.update_demux_high_water_prefetch_paused(10, 0, true, false));
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Full
    ));
    assert!(cache.update_demux_high_water_prefetch_paused(10, 0, false, false));
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Ready(_)
    ));
}

#[test]
fn http_cache_refills_as_soon_as_demux_has_room_after_a_forward_seek() {
    let cache =
        HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(0, 1024));
    assert!(cache.update_demux_high_water_prefetch_paused(150, 150, true, false));
    assert!(matches!(
        cache.shared.append_capacity_now(0),
        CacheAppendPermit::Full
    ));

    // Only 5 MiB was consumed by the seek. The old 75% watermark held the
    // transport paused, requiring an AVIO demand read to fetch each chunk.
    assert!(cache.update_demux_high_water_prefetch_paused(145, 150, false, false));
    let CacheAppendPermit::Ready(capacity) = cache.shared.append_capacity_now(0) else {
        panic!("forward seek should immediately resume HTTP prefetch");
    };
    cache.shared.append_or_restart(0, &vec![7; capacity]);
    assert!(matches!(
        cache.shared.append_capacity_now(capacity as u64),
        CacheAppendPermit::Full
    ));
    assert_eq!(cache.shared.active_readers.load(Ordering::Acquire), 0);

    let mut output = [0; 16];
    assert!(matches!(
        cache.read_at(0, &mut output),
        CacheReadResult::Data(16)
    ));
    assert_eq!(output, [7; 16]);
    assert!(matches!(
        cache.shared.append_capacity_now(capacity as u64),
        CacheAppendPermit::Ready(16)
    ));
    assert_eq!(cache.shared.active_readers.load(Ordering::Acquire), 0);
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
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
fn http_cache_shared_external_disk_write_preserves_trimmed_backseek_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let cache = HttpRingCache::from_state_for_test(
        HttpRingCacheState::new_with_disk_cache_for_test(0, 4, 16, directory.path())
            .with_content_len_hint(Some(8)),
    );

    assert!(matches!(
        cache.shared.append_or_restart(0, b"abcd"),
        super::super::CacheAppendResult::Appended
    ));
    cache
        .shared
        .state
        .lock()
        .expect("state locks")
        .set_reader_offset(4);
    assert!(matches!(
        cache.shared.append_or_restart(4, b"efgh"),
        super::super::CacheAppendResult::Appended
    ));

    let mut restored = [0; 4];
    assert!(matches!(
        cache.read_cached_at(0, &mut restored),
        CacheReadResult::Data(4)
    ));
    assert_eq!(&restored, b"abcd");
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
        demux_high_water_paused: AtomicBool::new(false),
        recovery_input_required: AtomicBool::new(false),
        active_readers: AtomicU64::new(0),
        cache_config_generation: AtomicU64::new(0),
        input_progress_generation: AtomicU64::new(0),
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
