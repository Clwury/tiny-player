use super::*;

const PACKET_BYTES: usize = 16 * 1024;

fn fixture(
    disk: bool,
) -> (
    tempfile::TempDir,
    PlaybackCacheConfig,
    Arc<DemuxPacketCacheShared>,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = PlaybackCacheConfig {
        total_cache_max_bytes: 0,
        demuxer_max_bytes: 96 * 1024,
        demuxer_max_back_bytes: 32 * 1024,
        disk_cache: disk,
        disk_cache_max_bytes: 512 * 1024,
        cache_dir: Some(dir.path().into()),
        unlink_files: CacheUnlinkPolicy::WhenDone,
        cache_pause: false,
        ..PlaybackCacheConfig::default()
    };
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let (shared, _) = shared_with_config_for_test(control, config.clone());
    shared.state.lock().unwrap().disk_worker_active = true;
    (dir, config, Arc::new(shared))
}

fn payload_packet(index: u8) -> CachedDemuxPacket {
    let mut packet = demux_packet_with_data_for_stream(0, &vec![index; PACKET_BYTES]);
    unsafe {
        (*packet.as_mut_ptr()).pos = i64::from(index) * PACKET_BYTES as i64;
        (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }
    CachedDemuxPacket::from_packet(
        &packet,
        0,
        true,
        CachedDemuxPacketRecovery {
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Keyframe,
            safe_seek_point: true,
        },
        Some(u64::from(index) * 1_000_000_000),
        Some((u64::from(index) + 1) * 1_000_000_000),
        Some(u64::from(index) * 1_000_000_000),
    )
    .unwrap()
}

fn append_packets(shared: &DemuxPacketCacheShared, count: u8) {
    let mut state = shared.state.lock().unwrap();
    for index in 0..count {
        assert!(state.append_packet_fast(payload_packet(index)).appended);
    }
}

fn settle(shared: &DemuxPacketCacheShared) {
    for _ in 0..100 {
        let Some(work) = shared.prepare_storage_work_for_test() else {
            return;
        };
        if !work() {
            return;
        }
    }
    panic!("storage worker must settle without cycling reads and writes");
}

fn assert_accounting(state: &DemuxPacketCacheState) {
    assert_eq!(
        state.resident_bytes,
        state
            .packets
            .values()
            .map(|p| p.resident_bytes())
            .sum::<usize>()
    );
    assert_eq!(
        state.disk_cached_bytes,
        state
            .packets
            .values()
            .map(|p| p.disk_bytes())
            .sum::<usize>()
    );
    assert_eq!(
        state.cached_bytes,
        state.packets.values().map(|p| p.byte_len).sum::<usize>()
    );
}

#[test]
fn disk_cache_extends_media_window_while_resident_payloads_stay_bounded() {
    let (_dir, _config, shared) = fixture(true);
    append_packets(&shared, 20);
    {
        let state = shared.state.lock().unwrap();
        assert!(state.storage_memory_full());
        assert_eq!(state.disk_cached_bytes, 0, "append must not wait for disk");
        assert!(state.packets.values().all(|p| p.memory_packet().is_some()));
    }
    settle(&shared);
    let state = shared.state.lock().unwrap();
    assert_accounting(&state);
    assert!(state.resident_bytes < state.resident_limit_bytes());
    assert!(state.disk_cached_bytes > state.memory_limit_bytes);
    assert!(state.forward_bytes() > state.memory_limit_bytes);
    assert!(!state.should_pause_demux());
    assert!(
        state.packets[&0].memory_packet().is_some(),
        "read head stays hot"
    );
    assert!(
        state.packets[&19].memory_packet().is_none(),
        "distant data becomes cold"
    );
    let report = state.playback_cache_state(false).demux;
    assert_eq!(report.total_bytes, 20 * PACKET_BYTES as u64);
    assert_eq!(report.storage.memory_bytes, state.resident_bytes as u64);
    assert_eq!(report.storage.disk_bytes, state.disk_cached_bytes as u64);
    assert_eq!(report.forward_limit_bytes, state.media_limits().0 as u64);
    assert_eq!(
        state.packet_queue_snapshot().prefetch_limit_bytes,
        state.media_limits().0
    );
}

#[test]
fn memory_only_cache_keeps_the_existing_forward_limit() {
    let (_dir, _, shared) = fixture(false);
    append_packets(&shared, 7);
    let state = shared.state.lock().unwrap();
    assert_eq!(state.media_limits(), (96 * 1024, 32 * 1024));
    assert!(state.should_pause_demux());
    assert_eq!(state.disk_cached_bytes, 0);
}

#[test]
fn cold_cached_seek_waits_without_advancing_and_supplies_a_memory_reference() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    let head = {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        let head = state.reader_heads[&0];
        assert!(state.packets[&head].memory_packet().is_none());
        assert!(!state.consumer_drainable_packet_available());
        assert!(
            !state
                .packet_queue_snapshot()
                .consumer_drainable_for_streams(&[0])
        );
        assert!(
            state
                .packet_queue_snapshot()
                .reader_head_lost_streams(&[0])
                .is_empty()
        );
        assert!(
            state
                .take_packet_round_robin_with_trim(
                    &[0],
                    &mut DemuxPacketCacheReadTiming::default(),
                    false
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(state.reader_heads[&0], head);
        head
    };
    assert!(shared.prepare_storage_work_for_test().unwrap()());
    assert!(
        shared
            .state
            .lock()
            .unwrap()
            .consumer_drainable_packet_available()
    );
    let mut timing = DemuxPacketCacheReadTiming::default();
    let source = shared
        .state
        .lock()
        .unwrap()
        .take_packet_round_robin_with_trim(&[0], &mut timing, false)
        .unwrap()
        .unwrap();
    let (packet, _) = source.packet_ref(&mut timing).unwrap();
    assert_eq!(packet.data().unwrap(), vec![head as u8; PACKET_BYTES]);
    assert_eq!(
        timing.disk_reads, 0,
        "decoder supply must not synchronously read the file"
    );
    assert_accounting(&shared.state.lock().unwrap());
}

#[test]
fn cold_cached_seek_keeps_supplying_packets_while_disk_appends_a_detached_range() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    let detached_id = {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        state.start_detached_append_range();
        for index in 20..24 {
            state.append_packet_fast(payload_packet(index));
        }
        state.append_range_id
    };
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    for index in 10..16 {
        settle(&shared);
        let (result, _, timing) = cache.poll_packet_round_robin_with_timing(&[0]);
        let DemuxReadResult::Packet(packet) = result else {
            panic!("disk worker must supply the next cached packet");
        };
        assert_eq!(packet.data().unwrap(), vec![index; PACKET_BYTES]);
        assert_eq!(timing.disk_reads, 0);
        assert!(!timing.lock_timed_out);
        let state = shared.state.lock().unwrap();
        assert_accounting(&state);
        assert!(state.resident_bytes <= state.resident_limit_bytes());
        assert_eq!(
            state.ranges[&detached_id].forward_stats_rebuilds.get(),
            0,
            "disk reads, writes and hot-copy eviction must reuse forward aggregates"
        );
    }
}

#[test]
fn dry_subtitle_poll_preserves_range_with_a_cold_video_head() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    let (read_range_id, head) = {
        let mut state = shared.state.lock().unwrap();
        state.set_stream_kind(4, StreamCacheKind::Subtitle);
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        let head = state.reader_heads[&0];
        assert!(state.packets[&head].memory_packet().is_none());
        assert!(state.disk_read_requests.is_empty());
        state.start_detached_append_range();
        state.append_packet_fast(payload_packet(20));
        (state.read_range_id, head)
    };
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    assert!(matches!(cache.poll_packet(4), DemuxReadResult::WouldBlock));
    {
        let state = shared.state.lock().unwrap();
        assert_eq!(state.read_range_id, read_range_id);
        assert_eq!(state.reader_heads[&0], head);
        assert!(state.disk_read_requests.is_empty());
    }
    assert!(matches!(cache.poll_packet(0), DemuxReadResult::WouldBlock));
    settle(&shared);
    let (result, _, timing) = cache.poll_packet_round_robin_with_timing(&[0]);
    let DemuxReadResult::Packet(packet) = result else {
        panic!("disk worker must restore the original cached video head");
    };
    assert_eq!(packet.data().unwrap(), vec![10; PACKET_BYTES]);
    assert_eq!(
        packet.read_diagnostic().unwrap().read_range_id,
        read_range_id
    );
    assert_eq!(timing.disk_reads, 0);
}

#[test]
fn cold_archived_seek_resumes_in_the_same_range_without_replaying_disk_packets() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    let (range_id, head_is_cold, duplicate_count, first_new_id) = {
        let mut state = shared.state.lock().unwrap();
        // Keep the whole old range archived while retaining a bounded hot set.
        state.backbuffer_limit_bytes = 96 * 1024;
        state.request_seek(100.0, PlaybackSessionId(2), 1, 100_000_000_000);
        for index in 100..103 {
            state.append_packet_fast(payload_packet(index));
        }
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(3))
                .is_some()
        );
        state.take_seek_request().unwrap();
        let head = state.reader_heads[&0];
        let head_is_cold = state.packets[&head].memory_packet().is_none();
        let first_new_id = state.next_packet_id;
        let mut duplicate_count = 0;
        for index in 17..24 {
            if !state.append_packet_fast(payload_packet(index)).appended {
                duplicate_count += 1;
            }
        }
        (
            state.read_range_id,
            head_is_cold,
            duplicate_count,
            first_new_id,
        )
    };
    assert!(head_is_cold);
    assert_eq!(duplicate_count, 3);
    {
        let state = shared.state.lock().unwrap();
        assert_eq!(state.read_range_id, state.append_range_id);
        assert_eq!(state.next_packet_id, first_new_id + 4);
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    for index in 10..24 {
        settle(&shared);
        let (result, _, timing) = cache.poll_packet_round_robin_with_timing(&[0]);
        let DemuxReadResult::Packet(packet) = result else {
            panic!("storage worker must supply continuous cached and resumed packets");
        };
        assert_eq!(packet.data().unwrap(), vec![index; PACKET_BYTES]);
        assert_eq!(packet.read_diagnostic().unwrap().read_range_id, range_id);
        assert_eq!(timing.disk_reads, 0);
        assert_accounting(&shared.state.lock().unwrap());
    }
}

#[test]
fn repeated_seek_discards_an_old_cold_read_completion() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        state
            .take_packet_round_robin_with_trim(
                &[0],
                &mut DemuxPacketCacheReadTiming::default(),
                false,
            )
            .unwrap();
    }
    let old_read = shared.prepare_storage_work_for_test().unwrap();
    {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(16_000_000_000, PlaybackSessionId(3))
                .is_some()
        );
    }
    assert!(
        !old_read(),
        "obsolete read must not repopulate the old hot window"
    );
    settle(&shared);
    let state = shared.state.lock().unwrap();
    assert!(state.packets[&10].memory_packet().is_none());
    assert!(
        state.packets[&state.reader_heads[&0]]
            .memory_packet()
            .is_some()
    );
    assert!(state.disk_read_requests.is_empty());
    assert_accounting(&state);
}

#[test]
fn enabling_disk_migrates_existing_memory_and_disable_preserves_readable_media() {
    let (_dir, mut config, shared) = fixture(false);
    append_packets(&shared, 20);
    config.disk_cache = true;
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    settle(&shared);
    let before = shared.state.lock().unwrap().disk_cached_bytes;
    assert!(before > 0);
    config.disk_cache = false;
    shared.state.lock().unwrap().apply_cache_config(config);
    settle(&shared);
    {
        let mut state = shared.state.lock().unwrap();
        assert!(!state.disk_cache_writable);
        assert_eq!(state.disk_cached_bytes, before);
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
    }
    settle(&shared);
    let mut timing = DemuxPacketCacheReadTiming::default();
    let source = shared
        .state
        .lock()
        .unwrap()
        .take_packet_round_robin_with_trim(&[0], &mut timing, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        source.packet_ref(&mut timing).unwrap().0.data().unwrap()[0],
        10
    );
    assert_eq!(timing.disk_reads, 0);
}

#[test]
fn smaller_disk_quota_restores_gradually_without_losing_forward_packets() {
    let (_dir, mut config, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    config.disk_cache_max_bytes = 64 * 1024;
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    settle(&shared);
    {
        let state = shared.state.lock().unwrap();
        assert!(state.resident_bytes <= state.resident_limit_bytes());
        assert!(
            state
                .playback_cache_state(false)
                .demux
                .storage
                .disk_pending_bytes
                > 0
        );
        assert_eq!(state.packets.len(), 20);
    }
    for index in 0..20 {
        // Demand reads can bypass the resident cap by one packet per track.
        loop {
            let source = shared
                .state
                .lock()
                .unwrap()
                .take_packet_round_robin_with_trim(
                    &[0],
                    &mut DemuxPacketCacheReadTiming::default(),
                    true,
                )
                .unwrap();
            if let Some(source) = source {
                assert_eq!(
                    source
                        .packet_ref(&mut DemuxPacketCacheReadTiming::default())
                        .unwrap()
                        .0
                        .data()
                        .unwrap()[0],
                    index
                );
                break;
            }
            assert!(shared.prepare_storage_work_for_test().unwrap()());
        }
        settle(&shared);
        assert_accounting(&shared.state.lock().unwrap());
    }
    let state = shared.state.lock().unwrap();
    let disk = state.disk_cache.as_ref().unwrap().clone();
    assert!(state.resident_bytes <= state.resident_limit_bytes());
    assert!(state.disk_restore_requests.is_empty());
    drop(state);
    disk.maintain_file_size();
    assert!(
        std::fs::metadata(&disk.path).unwrap().len() <= config.effective_disk_cache_budgets().1
    );
}

#[test]
fn stale_disk_write_after_disable_keeps_the_original_packet() {
    let (_dir, mut config, shared) = fixture(true);
    append_packets(&shared, 10);
    let write = shared.prepare_storage_work_for_test().unwrap();
    config.disk_cache = false;
    shared.state.lock().unwrap().apply_cache_config(config);
    write();
    let state = shared.state.lock().unwrap();
    assert_eq!(state.disk_cached_bytes, 0);
    assert!(state.packets.values().all(|p| p.memory_packet().is_some()));
    assert_accounting(&state);
}

#[test]
fn real_storage_worker_wakes_readers_and_releases_the_session_on_shutdown() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    let weak = Arc::downgrade(&shared);
    shared.start_storage_worker().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if shared.state.lock().unwrap().disk_cached_bytes > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "disk worker did not spill");
        thread::sleep(Duration::from_millis(1));
    }
    let cache = DemuxPacketCache {
        shared: Arc::clone(&shared),
        handle: None,
    };
    let (result, _, timing) = cache.poll_packet_round_robin_with_timing(&[0]);
    assert!(matches!(result, DemuxReadResult::Packet(_)));
    assert_eq!(timing.disk_reads, 0);
    drop(cache);
    drop(shared);
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "disk worker retained a closed session"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn slow_cold_read_does_not_hold_the_cache_lock_or_advance_the_reader() {
    let (_dir, config, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    let props = {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        state
            .take_packet_round_robin_with_trim(
                &[0],
                &mut DemuxPacketCacheReadTiming::default(),
                false,
            )
            .unwrap();
        match &state.packets[&10].payload {
            CachedDemuxPacketPayload::Disk { props, .. } => Arc::clone(props),
            _ => panic!("seek target must start cold"),
        }
    };
    // Deliberately stop the I/O job at its properties-copy stage. A waiting
    // storage operation must not prevent config changes or another poll.
    let blocked_props = props.lock().unwrap();
    let work = shared.prepare_storage_work_for_test().unwrap();
    thread::scope(|scope| {
        let worker = scope.spawn(work);
        let mut state = shared
            .state
            .try_lock()
            .expect("cold read must release cache mutex");
        state.apply_cache_config(config);
        assert_eq!(state.reader_heads[&0], 10);
        assert!(
            state
                .take_packet_round_robin_with_trim(
                    &[0],
                    &mut DemuxPacketCacheReadTiming::default(),
                    false
                )
                .unwrap()
                .is_none()
        );
        drop(state);
        drop(blocked_props);
        assert!(worker.join().unwrap());
    });
    assert!(
        shared.state.lock().unwrap().packets[&10]
            .memory_packet()
            .is_some()
    );
}

#[test]
fn failed_disk_creation_retains_the_memory_only_admission_limit() {
    let (dir, mut config, shared) = fixture(false);
    let file = dir.path().join("not-a-directory");
    std::fs::write(&file, b"fixture").unwrap();
    config.cache_dir = Some(file);
    config.disk_cache = true;
    shared.state.lock().unwrap().apply_cache_config(config);
    append_packets(&shared, 7);
    {
        let state = shared.state.lock().unwrap();
        assert_eq!(
            state.disk_budget_bytes, 0,
            "pending file creation must not expand RAM-only media"
        );
        assert!(state.should_pause_demux());
    }
    settle(&shared);
    let state = shared.state.lock().unwrap();
    assert!(state.disk_cache.is_none());
    assert_eq!(state.media_limits().0, state.memory_limit_bytes);
    assert!(state.should_pause_demux());
    assert_accounting(&state);
}

#[test]
fn latest_disk_config_wins_over_an_already_prepared_update() {
    let (_dir, mut config, shared) = fixture(true);
    config.disk_cache_max_bytes = 32 * 1024;
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    let old_config = shared.prepare_storage_work_for_test().unwrap();
    config.disk_cache_max_bytes = 1024 * 1024;
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    old_config();
    settle(&shared);
    let state = shared.state.lock().unwrap();
    assert_eq!(
        state.disk_cache.as_ref().unwrap().limit(),
        config.effective_disk_cache_budgets().1
    );
    assert_eq!(
        state.disk_budget_bytes as u64,
        config.effective_disk_cache_budgets().1
    );
}

#[test]
fn default_disk_budget_expands_forward_and_backward_media_in_the_memory_ratio() {
    let (dir, _, shared) = fixture(false);
    let config = PlaybackCacheConfig {
        disk_cache: true,
        cache_dir: Some(dir.path().into()),
        ..PlaybackCacheConfig::default()
    };
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    settle(&shared);
    let mut state = shared.state.lock().unwrap();
    assert_eq!(state.resident_limit_bytes(), 200 * 1024 * 1024);
    assert_eq!(
        state.media_limits(),
        (3198 * 1024 * 1024, 1066 * 1024 * 1024)
    );
    state.apply_cache_config(PlaybackCacheConfig {
        demuxer_max_back_bytes: 0,
        ..config
    });
    drop(state);
    settle(&shared);
    assert_eq!(shared.state.lock().unwrap().media_limits().1, 0);
}

#[test]
fn full_disk_pauses_prefetch_but_consumption_reclaims_space_and_resumes_it() {
    let (_dir, mut config, shared) = fixture(true);
    config.disk_cache_max_bytes = 64 * 1024;
    shared
        .state
        .lock()
        .unwrap()
        .apply_cache_config(config.clone());
    settle(&shared);
    append_packets(&shared, 12);
    settle(&shared);
    {
        let state = shared.state.lock().unwrap();
        assert!(state.disk_write_blocked);
        assert!(state.should_pause_demux());
        assert!(
            state.disk_cache.as_ref().unwrap().file_bytes()
                <= config.effective_disk_cache_budgets().1
        );
    }
    for index in 0..10 {
        loop {
            let source = shared
                .state
                .lock()
                .unwrap()
                .take_packet_round_robin_with_trim(
                    &[0],
                    &mut DemuxPacketCacheReadTiming::default(),
                    true,
                )
                .unwrap();
            if let Some(source) = source {
                assert_eq!(
                    source
                        .packet_ref(&mut DemuxPacketCacheReadTiming::default())
                        .unwrap()
                        .0
                        .data()
                        .unwrap()[0],
                    index
                );
                break;
            }
            assert!(shared.prepare_storage_work_for_test().unwrap()());
        }
        settle(&shared);
    }
    let state = shared.state.lock().unwrap();
    assert!(!state.should_pause_demux());
    assert!(state.resident_bytes < state.resident_limit_bytes());
    assert_accounting(&state);
}

#[test]
fn corrupted_cold_packet_reports_error_without_advancing_the_read_head() {
    let (_dir, _, shared) = fixture(true);
    append_packets(&shared, 20);
    settle(&shared);
    {
        let mut state = shared.state.lock().unwrap();
        assert!(
            state
                .seek_cached(10_000_000_000, PlaybackSessionId(2))
                .is_some()
        );
        let path = state.disk_cache.as_ref().unwrap().path.clone();
        drop(state);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(0)
            .unwrap();
    }
    assert!(!shared.prepare_storage_work_for_test().unwrap()());
    let state = shared.state.lock().unwrap();
    assert!(state.error.is_some());
    assert_eq!(state.reader_heads[&0], 10);
    assert!(state.packets[&10].memory_packet().is_none());
    assert_accounting(&state);
}

#[test]
fn disk_packets_continue_to_count_resident_side_data() {
    let (_dir, _, shared) = fixture(true);
    let mut packet = demux_packet_with_data_for_stream(0, &vec![0; PACKET_BYTES]);
    let side_bytes = 24 * 1024;
    unsafe {
        let side = ffi::av_packet_new_side_data(
            packet.as_mut_ptr(),
            ffi::AVPacketSideDataType::AV_PKT_DATA_NEW_EXTRADATA,
            side_bytes,
        );
        assert!(!side.is_null());
        std::ptr::write_bytes(side, 0, side_bytes);
    }
    let cached = CachedDemuxPacket::from_packet(
        &packet,
        0,
        true,
        CachedDemuxPacketRecovery {
            recovery_point: true,
            recovery_kind: VideoRecoveryPointKind::Keyframe,
            safe_seek_point: true,
        },
        Some(100_000_000_000),
        Some(101_000_000_000),
        Some(100_000_000_000),
    )
    .unwrap();
    let properties_bytes = cached.properties_byte_len;
    assert!(properties_bytes > side_bytes);
    {
        let mut state = shared.state.lock().unwrap();
        state.append_packet_fast(payload_packet(0));
        state.append_packet_fast(cached);
    }
    settle(&shared);
    let state = shared.state.lock().unwrap();
    let cached = &state.packets[&1];
    assert!(cached.memory_packet().is_none());
    assert_eq!(cached.disk_bytes(), PACKET_BYTES);
    assert!(cached.resident_bytes() >= properties_bytes);
    assert_accounting(&state);
}
