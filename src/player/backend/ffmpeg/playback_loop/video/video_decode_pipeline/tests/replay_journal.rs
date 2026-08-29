use super::*;

#[test]
fn hevc_hw_replay_journal_starts_only_at_safe_idr_or_bla() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let trail = hevc_packet(0x02, 1, 1_000, false);
    assert!(
        !journal
            .remember(&trail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );

    let cra = hevc_packet(0x2a, 2, 1_040, true);
    assert!(
        !journal
            .remember(&cra, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 0);

    let idr = hevc_packet(0x26, 3, 1_080, true);
    assert!(
        journal
            .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 1);
    assert_eq!(journal.anchor_nsecs, Some(1_080_000_000));
    assert!(matches!(
        journal.anchor_kind,
        Some(VideoRecoveryPointKind::Idr)
    ));

    let bla = hevc_packet(0x20, 4, 1_120, true);
    assert!(
        journal
            .remember(&bla, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 1);
    assert_eq!(journal.anchor_nsecs, Some(1_120_000_000));
    assert!(matches!(
        journal.anchor_kind,
        Some(VideoRecoveryPointKind::Bla)
    ));
}

#[test]
fn hevc_hw_replay_uses_cached_safe_anchor_verdict_after_payload_rewrite() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let mut packet = hevc_packet(0x02, 1, 1_000, false);
    packet.set_read_diagnostic(AvPacketReadDiagnostic {
        read_sequence: 7,
        cache_generation: 3,
        read_range_id: 2,
        packet_id: 41,
        stream_offset: 1,
        storage: AvPacketStorageKind::Memory,
        read_index_before: 8,
        read_index_after: 9,
        reader_head_before: Some(41),
        reader_head_after: Some(42),
        previous_read_packet_id: Some(40),
        previous_read_generation: Some(3),
        previous_expected_next_packet_id: Some(41),
        sequence_contiguous: Some(true),
        packet_start_nsecs: Some(1_000_000_000),
        packet_end_nsecs: Some(1_033_333_333),
        timeline_anchor: true,
        recovery_point: true,
        recovery_kind: VideoRecoveryPointKind::Idr,
        safe_seek_point: true,
    });

    assert!(
        journal
            .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("cached safe packet refs")
    );
    assert_eq!(journal.anchor_nsecs, Some(1_000_000_000));
    assert_eq!(journal.anchor_kind, Some(VideoRecoveryPointKind::Idr));
}

#[test]
fn problem_trace_19_rolls_safe_idr_already_covered_by_decoded_output() {
    let safe_idr_nsecs = 1_194_866_655_556;
    let decoded_output_end_nsecs = 1_195_499_988_766;
    assert!(hevc_safe_anchor_can_roll_past_preserved_evidence(
        true,
        Some(safe_idr_nsecs),
        Some(decoded_output_end_nsecs),
        false,
    ));
    assert!(
        !hevc_safe_anchor_can_roll_past_preserved_evidence(
            true,
            Some(decoded_output_end_nsecs.saturating_add(1)),
            Some(decoded_output_end_nsecs),
            false,
        ),
        "an IDR ahead of decoded output can belong to the active failure and must not replace the protected prefix"
    );
    assert!(
        !hevc_safe_anchor_can_roll_past_preserved_evidence(
            true,
            Some(safe_idr_nsecs),
            Some(decoded_output_end_nsecs),
            true,
        ),
        "a frozen recovery cutoff keeps its current replay prefix immutable"
    );
}

#[test]
fn hevc_hw_replay_rejects_preroll_that_does_not_cover_target() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let target_nsecs = 184_692_319_900;
    let mut journal = HevcHwReplayJournal::default();
    for (id, pts_millis, key) in [(0_u8, 179_900_i64, true), (1_u8, 180_166_i64, false)] {
        let packet = hevc_packet(if key { 0x26 } else { 0x02 }, id, pts_millis, key);
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }

    assert!(
        journal
            .clone_complete(target_nsecs)
            .expect("packet refs")
            .is_none(),
        "preroll has not reached the exact seek target yet"
    );
    assert!(
        journal
            .clone_replayable(target_nsecs, target_nsecs)
            .expect("packet refs")
            .is_none(),
        "a safe anchor without high-water coverage must fall back to cached seek"
    );
}

#[test]
fn problem_trace_29_recovery_replays_safe_idr_one_frame_after_frozen_target() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let target_nsecs = 1_080_633_000_000;
    let required_high_water_nsecs = 1_082_066_000_000;
    let mut journal = HevcHwReplayJournal::default();
    let next_safe_idr = hevc_packet(0x26, 0, 1_080_666, true);
    let covered_tail = hevc_packet(0x02, 1, 1_082_100, false);

    for packet in [&next_safe_idr, &covered_tail] {
        assert!(
            journal
                .remember(packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("recovery journal packet refs")
        );
    }

    assert!(
        journal
            .clone_complete(target_nsecs)
            .expect("exact-seek journal refs")
            .is_none(),
        "exact seek must not claim coverage before the journal anchor"
    );
    assert_eq!(
        journal
            .clone_replayable(target_nsecs, required_high_water_nsecs)
            .expect("recovery journal refs")
            .expect("the next-frame IDR is a bounded recovery boundary")
            .len(),
        2
    );
}

#[test]
fn hevc_hw_replay_rejects_forward_anchor_beyond_recoverable_gap() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let target_nsecs = 1_000_000_000_u64;
    let mut journal = HevcHwReplayJournal::default();
    let late_idr_millis = i64::try_from(
        target_nsecs.saturating_add(HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS) / 1_000_000 + 2,
    )
    .expect("timestamp fits");
    let late_idr = hevc_packet(0x26, 0, late_idr_millis, true);

    assert!(
        journal
            .remember(&late_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("late IDR packet refs")
    );
    assert!(
        journal
            .clone_replayable(target_nsecs, target_nsecs)
            .expect("recovery journal refs")
            .is_none(),
        "recovery must not skip an unbounded interval to a later IDR"
    );
}

#[test]
fn problem_trace_20_00_retains_high_bitrate_replay_through_recovery_cutoff() {
    const LEGACY_BYTE_LIMIT: usize = 32 * 1024 * 1024;
    const OBSERVED_PREFIX_BYTES: usize = 33_409_248;
    const PROJECTED_CUTOFF_TAIL_BYTES: usize = 4 * 1024 * 1024;

    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let target_nsecs = 1_200_166_000_000;
    let required_high_water_nsecs = 1_201_066_000_000;
    let mut journal = HevcHwReplayJournal::default();
    let anchor = hevc_packet(0x26, 0, 1_192_133, true);
    let covered_tail = hevc_packet(0x02, 1, 1_201_100, false);

    assert!(
        journal
            .remember(&anchor, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("anchor packet refs")
    );
    let projected_cutoff_bytes = OBSERVED_PREFIX_BYTES.saturating_add(PROJECTED_CUTOFF_TAIL_BYTES);
    assert!(projected_cutoff_bytes > LEGACY_BYTE_LIMIT);
    journal.total_bytes = projected_cutoff_bytes.saturating_sub(covered_tail.byte_len());
    assert!(
        journal
            .remember(&covered_tail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
            .expect("cutoff packet refs"),
        "the byte budget must preserve the complete bounded recovery interval"
    );
    assert_eq!(journal.total_bytes, projected_cutoff_bytes);
    assert!(
        journal
            .clone_replayable(target_nsecs, required_high_water_nsecs)
            .expect("recovery journal refs")
            .is_some(),
        "the 20:00 recovery should replay instead of rebuilding from the 1194.866s cached IDR"
    );
}

#[test]
fn hevc_hw_replay_retains_problem_7_233_second_idr_interval() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let idr = hevc_packet(0x26, 0, 683_400, true);
    let recovery_packet = hevc_packet(0x02, 1, 690_633, false);

    assert!(
        journal
            .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("IDR packet refs")
    );
    assert!(
        journal
            .remember(
                &recovery_packet,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                time_base,
            )
            .expect("recovery packet refs")
    );
    assert_eq!(journal.len(), 2);
    assert!(
        journal
            .clone_replayable(690_633_000_000, 690_633_000_000)
            .expect("journal packet refs")
            .is_some()
    );
}

#[test]
fn problem_trace_6_49_replay_stops_after_frozen_cutoff_tail() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let anchor_millis = 406_733_i64;
    let required_packet_index = 153_usize;
    let journal_packet_count = 243_usize;
    let mut journal = HevcHwReplayJournal::default();

    for packet_index in 0..journal_packet_count {
        let pts_millis =
            anchor_millis + i64::try_from(packet_index).expect("packet index fits") * 33;
        let packet = hevc_packet(
            if packet_index == 0 { 0x26 } else { 0x02 },
            packet_index as u8,
            pts_millis,
            packet_index == 0,
        );
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &packet,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("journal packet refs")
        );
    }

    let required_high_water_nsecs = u64::try_from(
        anchor_millis + i64::try_from(required_packet_index).expect("required index fits") * 33,
    )
    .expect("positive cutoff")
    .saturating_mul(1_000_000);
    let replay = journal
        .clone_replayable(
            u64::try_from(anchor_millis)
                .expect("positive target")
                .saturating_mul(1_000_000),
            required_high_water_nsecs,
        )
        .expect("replay packet refs")
        .expect("journal covers frozen cutoff");

    assert_eq!(
        replay.len(),
        required_packet_index + 1 + HEVC_HW_REPLAY_REORDER_TAIL_PACKETS
    );
    assert!(replay.len() < journal_packet_count);
}

#[test]
fn hevc_hw_replay_locks_previous_safe_idr_until_recovery_starts() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let previous_idr_millis = 677_866_i64;
    let next_idr_millis = 690_633_i64;
    let previous_idr = hevc_packet(0x26, 0, previous_idr_millis, true);
    assert!(
        journal
            .remember(&previous_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
            .expect("previous IDR packet refs")
    );

    for packet_index in 1..300_i64 {
        let pts_millis =
            previous_idr_millis + (next_idr_millis - previous_idr_millis) * packet_index / 300;
        let packet = hevc_packet(0x02, packet_index as u8, pts_millis, false);
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &packet,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("preroll packet refs")
        );
    }
    let next_idr = hevc_packet(0x26, 255, next_idr_millis, true);
    assert!(
        journal
            .remember_preserving_safe_anchor(
                &next_idr,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                time_base,
            )
            .expect("next IDR packet refs")
    );

    assert_eq!(journal.anchor_nsecs, Some(677_866_000_000));
    assert_eq!(journal.len(), 301);
    assert!(
        journal
            .clone_replayable(683_400_000_000, 690_633_000_000)
            .expect("locked journal packet refs")
            .is_some(),
        "the newer 690.633s IDR must not replace the safe anchor before recovery"
    );
}

#[test]
fn reopen_replay_includes_live_packets_consumed_after_flush_replay() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let anchor_millis = 677_866_i64;
    let target_nsecs = 681_266_000_000_u64;
    let now = Instant::now();
    let mut journal = HevcHwReplayJournal::default();

    for packet_index in 0..217_i64 {
        let pts_millis = anchor_millis + packet_index * 33;
        let packet = hevc_packet(
            if packet_index == 0 { 0x26 } else { 0x02 },
            packet_index as u8,
            pts_millis,
            packet_index == 0,
        );
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &packet,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("flush journal packet refs")
        );
    }
    let flush_cutoff_nsecs = u64::try_from(anchor_millis + 216 * 33)
        .expect("positive cutoff")
        .saturating_mul(1_000_000);
    let first_replay = journal
        .clone_replayable(target_nsecs, flush_cutoff_nsecs)
        .expect("first replay refs")
        .expect("flush replay coverage");
    assert_eq!(first_replay.len(), 217);

    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.set_root_evidence(111, Some(flush_cutoff_nsecs), Some(target_nsecs));
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
    transaction.observe_packet(10, Some(flush_cutoff_nsecs), 1);

    let mut reopen_cutoff_nsecs = flush_cutoff_nsecs;
    for packet_index in 217..237_i64 {
        let pts_millis = anchor_millis + packet_index * 33;
        reopen_cutoff_nsecs = u64::try_from(pts_millis)
            .expect("positive packet timestamp")
            .saturating_mul(1_000_000);
        let packet = hevc_packet(0x02, packet_index as u8, pts_millis, false);
        assert!(
            journal
                .remember_preserving_safe_anchor(
                    &packet,
                    ffi::AVCodecID::AV_CODEC_ID_HEVC,
                    time_base,
                )
                .expect("live packet refs")
        );
        transaction.observe_packet(10, Some(reopen_cutoff_nsecs), 0);
    }
    assert_eq!(
        transaction.advance_after_attempt_failure(
            "flush replay exhausted",
            now + Duration::from_secs(1),
            HardwareDecodeMode::Auto,
        ),
        HevcDecodeRecoveryAction::ReopenSameHardware
    );
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(reopen_cutoff_nsecs)
    );

    let reopen_replay = journal
        .clone_replayable(
            target_nsecs,
            transaction
                .replay_required_high_water_nsecs
                .expect("reopen cutoff"),
        )
        .expect("second replay refs")
        .expect("reopen replay covers live cutoff");
    assert_eq!(reopen_replay.len(), 237);
    assert!(reopen_replay.len() > first_replay.len());
    assert_eq!(journal.anchor_nsecs, Some(677_866_000_000));
    assert_eq!(journal.high_water_nsecs, Some(reopen_cutoff_nsecs));
}

#[test]
fn hevc_hw_replay_preserves_demux_order_and_uses_fresh_generations() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    for id in 0..8_u8 {
        let packet = hevc_packet(
            if id == 0 { 0x26 } else { 0x02 },
            id,
            i64::from(id) * 40,
            id == 0,
        );
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }

    let mut playback_generation = PlaybackGeneration::default();
    let replay = hevc_hw_replay_packets(
        journal
            .clone_complete(160_000_000)
            .expect("packet refs")
            .expect("safe journal covering target"),
        &mut playback_generation,
    );

    assert_eq!(journal.len(), 8, "replay must preserve the source journal");
    assert_eq!(
        journal
            .clone_complete(160_000_000)
            .expect("packet refs")
            .expect("journal remains reusable")
            .len(),
        8
    );
    assert_eq!(replay.len(), 8);
    for (index, pending) in replay.iter().enumerate() {
        assert_eq!(pending.generation, index as u64 + 1);
        assert_eq!(
            pending.packet.data().and_then(|data| data.last()),
            Some(&(index as u8))
        );
        assert!(pending.realign_after_decode_recovery);
        assert!(!pending.hevc_startup_in_flight_watchdog);
        assert!(pending.from_hevc_hw_replay);
        assert!(pending.hevc_decode_recovery_evidence_scoped);
    }
}

#[test]
fn hevc_hw_replay_stays_ahead_of_live_packets_after_backpressure() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    for id in 0..3_u8 {
        let packet = hevc_packet(
            if id == 0 { 0x26 } else { 0x02 },
            id,
            i64::from(id) * 40,
            id == 0,
        );
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }

    let mut generation = PlaybackGeneration::default();
    let mut replay = hevc_hw_replay_packets(
        journal
            .clone_complete(40_000_000)
            .expect("packet refs")
            .expect("safe journal covering target"),
        &mut generation,
    );
    let mut regular = VideoDecodePacketQueues::default();
    assert!(
        regular
            .push_pending_input(PendingVideoDecodePacket {
                generation: generation.advance(),
                packet: hevc_packet(0x02, 9, 120, false),
                realign_after_decode_recovery: true,
                hevc_startup_in_flight_watchdog: false,
                from_hevc_hw_replay: false,
                hevc_decode_recovery_evidence_scoped: false,
            })
            .is_ok()
    );

    let blocked_replay =
        take_next_video_decode_input(&mut regular, &mut replay).expect("first replay packet");
    assert!(blocked_replay.from_hevc_hw_replay);
    requeue_backpressured_video_decode_input(&mut regular, &mut replay, blocked_replay);

    let mut ids = Vec::new();
    while let Some(packet) = take_next_video_decode_input(&mut regular, &mut replay) {
        ids.push(
            *packet
                .packet
                .data()
                .and_then(|data| data.last())
                .expect("packet id"),
        );
    }
    assert_eq!(ids, vec![0, 1, 2, 9]);
}

#[test]
fn hevc_hw_replay_journal_invalidates_entire_gop_after_duration_limit() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let idr = hevc_packet(0x26, 0, 0, true);
    assert!(
        journal
            .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    let beyond_limit = hevc_packet(
        0x02,
        1,
        i64::try_from(HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS / 1_000_000 + 1)
            .expect("duration fits"),
        false,
    );
    assert!(
        !journal
            .remember(&beyond_limit, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 0);

    let tail = hevc_packet(0x02, 2, 6_040, false);
    assert!(
        !journal
            .remember(&tail, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert!(journal.clone_complete(0).expect("packet refs").is_none());
}

#[test]
fn frozen_resource_pressure_cutoff_keeps_completed_safe_idr_prefix_replayable() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    let idr = hevc_packet(0x26, 0, 0, true);
    assert!(
        journal
            .remember_preserving_safe_anchor(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
            .expect("IDR packet refs")
    );
    let covered = hevc_packet(0x02, 1, 9_000, false);
    assert!(
        journal
            .remember_preserving_safe_anchor(&covered, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base,)
            .expect("covered packet refs")
    );
    let beyond_limit = hevc_packet(
        0x02,
        2,
        i64::try_from(HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS / 1_000_000 + 1)
            .expect("duration fits"),
        false,
    );
    assert!(
        !journal
            .remember_preserving_safe_anchor(
                &beyond_limit,
                ffi::AVCodecID::AV_CODEC_ID_HEVC,
                time_base,
            )
            .expect("bounded packet refs")
    );
    assert!(journal.coverage_exhausted);
    assert!(journal.coverage_contiguous);
    assert!(
        journal
            .clone_replayable(1_000_000_000, 9_000_000_000)
            .expect("frozen journal refs")
            .is_some(),
        "a later limit must not poison the already-covered frozen cutoff"
    );
    assert!(
        journal
            .clone_replayable(1_000_000_000, 14_000_000_000)
            .expect("incomplete cutoff check")
            .is_none(),
        "the completed prefix must not claim coverage it never recorded"
    );
}

#[test]
fn hevc_drain_grace_counts_only_video_worker_results() {
    let before = VideoDecodeWorkerSnapshot {
        result_produced_sequence: 41,
        result_consumed_sequence: 41,
        ..VideoDecodeWorkerSnapshot::default()
    };
    assert!(
        !hevc_drain_video_result_progressed(before, before),
        "audio/output activity cannot extend the video decoder drain grace"
    );

    let produced = VideoDecodeWorkerSnapshot {
        result_produced_sequence: 42,
        ..before
    };
    assert!(hevc_drain_video_result_progressed(before, produced));

    let consumed = VideoDecodeWorkerSnapshot {
        result_consumed_sequence: 42,
        ..before
    };
    assert!(hevc_drain_video_result_progressed(before, consumed));
}

#[test]
fn empty_same_hardware_drain_skips_grace_before_flush() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 229_200_000_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut empty = HevcSameHardwareRecoveryTransaction::new(fallback, 2_585, None, now);
    let empty_snapshot = VideoDecodeWorkerSnapshot::default();
    assert!(!hevc_decoder_drain_work_pending(empty_snapshot));
    assert!(empty.record_decoder_drain_pass(false, false, now));
    assert_eq!(empty.phase, HevcSameHardwareRecoveryPhase::Flushing);

    let mut in_flight = HevcSameHardwareRecoveryTransaction::new(fallback, 2_585, None, now);
    let in_flight_snapshot = VideoDecodeWorkerSnapshot {
        submitted_not_consumed_packets: 1,
        state: VideoDecodeWorkerState::Decoding,
        ..VideoDecodeWorkerSnapshot::default()
    };
    assert!(hevc_decoder_drain_work_pending(in_flight_snapshot));
    assert!(!in_flight.record_decoder_drain_pass(
        false,
        true,
        now + HEVC_SAME_HARDWARE_DRAIN_GRACE - Duration::from_nanos(1),
    ));
    assert!(
        in_flight.record_decoder_drain_pass(false, true, now + HEVC_SAME_HARDWARE_DRAIN_GRACE,)
    );
    assert_eq!(in_flight.phase, HevcSameHardwareRecoveryPhase::Flushing);
}

#[test]
fn hevc_hw_replay_requires_safe_anchor_interval_to_cover_target() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    for (id, pts_millis) in [(0_u8, 1_000_i64), (1, 1_040)] {
        let packet = hevc_packet(if id == 0 { 0x26 } else { 0x02 }, id, pts_millis, id == 0);
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }
    assert!(
        journal
            .clone_complete(900_000_000)
            .expect("packet refs")
            .is_none()
    );

    for (id, pts_millis) in [(0_u8, 0_i64), (1, 40)] {
        let packet = hevc_packet(if id == 0 { 0x26 } else { 0x02 }, id, pts_millis, id == 0);
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }
    assert!(
        journal
            .clone_complete(100_000_000)
            .expect("packet refs")
            .is_none()
    );
}

#[test]
fn hevc_hw_replay_journal_invalidates_on_packet_or_byte_limit() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut journal = HevcHwReplayJournal::default();
    for id in 0..HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS {
        let packet = hevc_packet(
            if id == 0 { 0x26 } else { 0x02 },
            id as u8,
            i64::try_from(id).expect("packet index fits"),
            id == 0,
        );
        assert!(
            journal
                .remember(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
                .expect("packet refs")
        );
    }
    let overflow = hevc_packet(0x02, 0xff, 300, false);
    assert!(
        !journal
            .remember(&overflow, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 0);

    let idr = hevc_packet(0x26, 0, 0, true);
    assert!(
        journal
            .remember(&idr, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    journal.total_bytes = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES;
    assert!(
        !journal
            .remember(&overflow, ffi::AVCodecID::AV_CODEC_ID_HEVC, time_base)
            .expect("packet refs")
    );
    assert_eq!(journal.len(), 0);
}

#[test]
fn hevc_hw_replay_reports_matching_pending_capacity() {
    assert_eq!(
        video_decode_pending_input_snapshot(0, HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS),
        (
            HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
            HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS
        )
    );
    assert_eq!(
        video_decode_pending_input_snapshot(VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY, 0),
        (
            VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY
        )
    );
}

#[test]
fn force_vulkan_runtime_failure_does_not_silently_switch_to_software() {
    assert!(runtime_hevc_software_fallback_allowed(
        HardwareDecodeMode::Auto
    ));
    assert!(!runtime_hevc_software_fallback_allowed(
        HardwareDecodeMode::ForceVulkan
    ));
    assert!(!runtime_hevc_software_fallback_allowed(
        HardwareDecodeMode::Off
    ));
    assert_eq!(
        hevc_same_hardware_reopen_mode(),
        HardwareDecodeMode::ForceVulkan
    );
}
