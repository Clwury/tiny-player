use std::time::{Duration, Instant};

use super::super::{HttpCacheConfig, HttpCacheRangeKind, HttpRingCacheState};

fn buffered_state() -> HttpRingCacheState {
    let mut state = HttpRingCacheState::new_with_config(0, HttpCacheConfig::for_test(64 * 1024))
        .with_content_len_hint(Some(1_000_000));
    assert!(state.append_at(0, &[1; 16 * 1024]));
    state
}

#[test]
fn side_ranges_download_only_the_prefix_before_an_overlapping_request() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.request_side_download_at(499_000, HttpCacheRangeKind::Playback));
    let first = state.side_download_requests[0];
    let second = state.side_download_requests[1];
    assert_eq!((first.offset, first.end_offset), (499_000, 500_000));
    assert_eq!((second.offset, second.end_offset), (500_000, 508_192));
    assert!(!state.request_side_download_at(500_100, HttpCacheRangeKind::Playback));
    assert!(!state.request_side_download_at(499_500, HttpCacheRangeKind::TailMetadataProbe));
}

#[test]
fn side_ranges_reuse_cached_bytes_regardless_of_their_purpose() {
    let mut state = buffered_state();
    assert!(state.append_retained_at(500_000, &[2; 1_000], HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.request_side_download_at(499_000, HttpCacheRangeKind::Playback));
    assert_eq!(state.side_download_requests[0].end_offset, 500_000);
    assert!(!state.request_side_download_at(500_500, HttpCacheRangeKind::Playback));
    assert!(state.request_side_download_at(501_000, HttpCacheRangeKind::Playback));
    assert_eq!(state.side_download_requests[1].offset, 501_000);
}

#[test]
fn playback_seek_reuses_tail_probe_bytes_before_opening_its_main_response() {
    let mut state = buffered_state();
    assert!(state.append_retained_at(990_000, b"tail", HttpCacheRangeKind::TailMetadataProbe));
    state.finish_metadata_probe();
    state.note_seek_offset(990_000, HttpCacheRangeKind::Playback);
    assert_eq!(state.restart_request.unwrap().offset, 990_000);
    state.restart_at(990_000);
    assert_eq!(
        state.splice_retained_playback_at_active_end(990_000),
        Some(990_004)
    );
    assert_eq!(state.active_range_kind, HttpCacheRangeKind::Playback);
    let mut bytes = [0; 4];
    assert_eq!(state.copy_available(990_000, &mut bytes), Some(4));
    assert_eq!(&bytes, b"tail");
}

#[test]
fn side_dispatch_rechecks_bytes_received_since_queueing() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.append_retained_at(500_000, &[2; 1_000], HttpCacheRangeKind::Playback));
    state.side_read_demand = Some(501_000);
    let request = state.take_side_download_request().unwrap();
    assert_eq!((request.offset, request.end_offset), (501_000, 508_192));
}

#[test]
fn side_request_coverage_does_not_change_with_the_next_request_budget() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    state.config.range_request_bytes = 1;
    assert!(state.side_download_may_produce(508_191));
    assert!(!state.side_download_may_produce(508_192));
    assert!(!state.request_side_download_at(508_191, HttpCacheRangeKind::Playback));
}

#[test]
fn side_timeout_preserves_received_bytes_and_terminates_the_missing_suffix() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    state.side_read_demand = Some(500_000);
    let request = state.take_side_download_request().unwrap();
    assert!(state.append_retained_at(500_000, b"prefix", HttpCacheRangeKind::TailMetadataProbe));
    state.maintain_side_downloads(request.deadline + Duration::from_millis(1));
    assert!(state.side_download_active.is_empty());
    assert!(state.error.is_none());
    assert!(state.side_download_error_at(500_005).is_none());
    assert!(state.side_download_error_at(500_006).is_some());
    assert!(!state.request_side_download_at(500_006, HttpCacheRangeKind::TailMetadataProbe));
    let mut bytes = [0; 6];
    assert_eq!(state.copy_available(500_000, &mut bytes), Some(6));
    assert_eq!(&bytes, b"prefix");
}

#[test]
fn queued_side_timeout_includes_waiting_for_admission() {
    let mut state = buffered_state();
    state.set_reader_offset(state.next_offset);
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let request = state.side_download_requests[0];
    assert!(state.take_side_download_request().is_none());
    state.maintain_side_downloads(request.deadline);
    assert!(state.side_download_requests.is_empty());
    assert!(state.side_download_error_at(500_000).is_some());
    assert!(!state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
}

#[test]
fn side_deadline_survives_partial_responses_and_retry_progress() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let deadline = state.side_download_requests[0].deadline;
    state.side_read_demand = Some(500_000);
    let request = state.take_side_download_request().unwrap();
    assert!(state.append_retained_at(500_000, &[2; 100], HttpCacheRangeKind::TailMetadataProbe));
    state.side_read_demand = Some(500_100);
    state.maintain_side_downloads(deadline - Duration::from_millis(1));
    assert_eq!(state.side_download_active[0].deadline, deadline);
    state.maintain_side_downloads(deadline);
    assert!(state.side_download_error_at(500_100).is_some());
    assert!(state.side_download_active.is_empty());
    assert_eq!(request.deadline, deadline);
}

#[test]
fn low_forward_watermark_cancels_background_side_io_and_defers_new_work() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.request_side_download_at(600_000, HttpCacheRangeKind::TailMetadataProbe));
    let request = state.take_side_download_request().unwrap();
    assert!(state.take_side_download_request().is_none());
    state.set_reader_offset(state.next_offset);
    state.maintain_side_downloads(Instant::now());
    assert!(state.side_download_active.is_empty());
    assert!(state.take_side_download_request().is_none());
    assert!(state.side_download_error_at(request.offset).is_none());
    assert!(state.side_download_errors.is_empty());
}

#[test]
fn blocked_read_preempts_an_inflight_probe_even_with_low_forward_cache() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let background = state.take_side_download_request().unwrap();
    assert!(state.request_side_download_at(200_000, HttpCacheRangeKind::Playback));
    state.set_reader_offset(state.next_offset);
    state.side_read_demand = Some(200_000);
    let foreground = state.take_side_download_request().unwrap();
    assert_eq!(foreground.offset, 200_000);
    assert!(!state.side_download_active.contains(&background));
    assert_eq!(state.side_download_active, vec![foreground]);
    assert!(state.side_download_errors.is_empty());
}

#[test]
fn blocked_read_reuses_an_inflight_range_with_a_different_purpose() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let request = state.take_side_download_request().unwrap();
    state.set_reader_offset(state.next_offset);
    state.side_read_demand = Some(500_100);
    assert!(!state.request_side_download_at(500_100, HttpCacheRangeKind::Playback));
    state.maintain_side_downloads(Instant::now());
    assert_eq!(state.side_download_active, vec![request]);
    assert!(state.side_download_requests.is_empty());
}

#[test]
fn cancelled_side_completion_cannot_remove_same_generation_replacement() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let old = state.take_side_download_request().unwrap();
    state.set_reader_offset(state.next_offset);
    state.maintain_side_downloads(Instant::now());
    state.side_read_demand = Some(500_000);
    assert!(state.request_side_download_at(500_000, HttpCacheRangeKind::TailMetadataProbe));
    let replacement = state.take_side_download_request().unwrap();
    assert_eq!(old.generation, replacement.generation);
    assert_ne!(old.id, replacement.id);
    state.finish_side_download_request(old, true);
    assert_eq!(state.side_download_active, vec![replacement]);
    assert!(state.restart_request.is_none());
}

#[test]
fn ending_probe_cancels_tail_work_and_allows_a_fresh_probe_after_failure() {
    let mut state = buffered_state();
    assert!(state.request_side_download_at(990_000, HttpCacheRangeKind::TailMetadataProbe));
    let request = state.take_side_download_request().unwrap();
    state.finish_side_download_request(request, false);
    state.record_side_download_error(request, request.offset, "probe failed".into());
    assert!(!state.request_side_download_at(request.offset, request.range_kind));
    state.finish_metadata_probe();
    assert_eq!(
        state.range_kind_for_seek(990_000, true),
        HttpCacheRangeKind::Playback
    );
    assert!(!state.is_tail_metadata_probe_seek(990_000));
    state.begin_metadata_probe();
    assert!(state.request_side_download_at(request.offset, request.range_kind));
}

#[test]
fn explicit_probe_purpose_survives_read_misses_after_the_seek_target() {
    let mut state = buffered_state();
    state.note_seek_offset(500_000, HttpCacheRangeKind::TailMetadataProbe);
    assert!(state.queue_read_miss_at(500_000));
    assert_eq!(
        state.range_kind_for_miss(500_001),
        HttpCacheRangeKind::TailMetadataProbe
    );
    assert!(!state.queue_read_miss_at(500_001));
    assert!(state.restart_request.is_none());
    state.finish_metadata_probe();
    assert_eq!(
        state.range_kind_for_miss(500_001),
        HttpCacheRangeKind::Playback
    );
}

#[test]
fn probe_fallback_clears_the_previous_contexts_read_purpose_and_side_work() {
    let mut state = buffered_state();
    state.note_seek_offset(990_000, HttpCacheRangeKind::TailMetadataProbe);
    assert!(state.queue_read_miss_at(990_000));
    let old = state.take_side_download_request().unwrap();
    state.begin_metadata_probe();
    assert_eq!(
        state.range_kind_for_miss(20_000),
        HttpCacheRangeKind::Playback
    );
    assert!(state.pending_seek_range_kind.is_none());
    assert!(!state.side_download_active.contains(&old));
    assert!(state.side_download_requests.is_empty());
}
