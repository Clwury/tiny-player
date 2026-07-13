use std::sync::{Arc, Condvar, Mutex, mpsc};

use crate::player::{
    backend::{BackendEventKind, ByteCacheState, PlaybackCacheState},
    render_host::PlaybackSessionId,
};

use super::super::{
    CacheReadResult, FfmpegControl, HttpCacheRangeKind, HttpRingCache, HttpRingCacheShared,
    HttpRingCacheState,
};

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
fn http_cache_status_is_not_idle_while_side_download_is_pending() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    assert!(state.append_at(100, b"abcdef"));
    state.set_reader_offset(500);
    assert!(state.request_side_download_at(500, HttpCacheRangeKind::Playback));

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
