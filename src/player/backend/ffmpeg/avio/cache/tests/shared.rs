use std::sync::{Arc, Condvar, Mutex, mpsc};

use crate::player::{
    backend::{BackendEventKind, ByteCacheState, PlaybackCacheState},
    render_host::PlaybackSessionId,
};

use super::super::{
    FfmpegControl, HTTP_CACHE_RANGE_REQUEST_BYTES, HttpCacheRangeKind, HttpRingCacheShared,
    HttpRingCacheState,
};

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
