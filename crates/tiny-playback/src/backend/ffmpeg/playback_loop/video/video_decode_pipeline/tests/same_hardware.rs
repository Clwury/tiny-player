use super::*;

#[test]
fn resource_pressure_transaction_freezes_cutoff_and_skips_idr_drain_scan() {
    let now = Instant::now();
    let target_nsecs = 716_633_333_333;
    let cutoff_nsecs = 724_100_000_000;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ResourcePressure,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(
        fallback,
        10,
        Some("Cannot allocate memory".to_string()),
        now,
    );
    transaction.set_root_evidence(0, Some(cutoff_nsecs), Some(target_nsecs));

    assert!(transaction.resource_pressure());
    assert!(transaction.resource_pressure_demux_admission_stopped());
    assert!(transaction.resource_pressure_decoder_input_stopped());
    assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Flushing);
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::FlushSameHardware
    );
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(cutoff_nsecs)
    );
    assert!(transaction.claim_resource_pressure_external_release(7));
    assert!(
        !transaction.claim_resource_pressure_external_release(7),
        "repeated OOM results from one decoder epoch must not restart external release"
    );
    assert!(
        transaction.claim_resource_pressure_external_release(8),
        "a flush/reopen epoch may own a fresh bounded set of Vulkan frames"
    );

    transaction.record_resource_pressure_error(
        "Cannot allocate memory",
        Some(790_166_666_666),
        now + Duration::from_millis(1),
    );
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(cutoff_nsecs),
        "later failed packets must not advance the frozen recovery cutoff"
    );
    assert_eq!(transaction.target_nsecs, target_nsecs);

    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    assert!(
        transaction.resource_pressure_demux_admission_stopped(),
        "future demux packets remain frozen throughout resource-pressure replay"
    );
    assert!(
        !transaction.resource_pressure_decoder_input_stopped(),
        "the already-bounded journal must remain replayable"
    );

    let ordinary = HevcSameHardwareRecoveryTransaction::new(
        HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        },
        10,
        None,
        now,
    );
    assert!(
        transaction.resource_pressure(),
        "OOM reopen must release first"
    );
    assert!(
        !ordinary.resource_pressure(),
        "ordinary corruption recovery keeps the atomic open-first swap"
    );
    assert!(!ordinary.resource_pressure_demux_admission_stopped());
    assert!(!ordinary.resource_pressure_decoder_input_stopped());
}

#[test]
fn first_oom_preempts_active_ordinary_recovery_and_freezes_its_boundary() {
    let now = Instant::now();
    let ordinary_target_nsecs = 681_266_667_000;
    let oom_target_nsecs = 716_633_333_333;
    let oom_cutoff_nsecs = 724_100_000_000;
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(
        HevcDecodeChainFallback {
            target_nsecs: ordinary_target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        },
        10,
        None,
        now,
    );
    transaction.set_root_evidence(111, Some(690_633_333_333), Some(ordinary_target_nsecs));
    transaction.flush_attempts = HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    let attempt_id =
        transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 20, now);
    transaction.observe_packet(20, Some(690_633_333_333), 0);

    transaction.promote_to_resource_pressure(
        oom_target_nsecs,
        Some(oom_cutoff_nsecs),
        "Cannot allocate memory",
        now + Duration::from_millis(1),
    );
    if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
        transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
    } else {
        transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
    }

    assert!(transaction.resource_pressure());
    assert_eq!(
        transaction.reason,
        HevcDecodeChainFallbackReason::ResourcePressure
    );
    assert_eq!(transaction.target_nsecs, oom_target_nsecs);
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(oom_cutoff_nsecs)
    );
    assert_eq!(transaction.root_zero_output_packets, 0);
    assert_eq!(
        transaction.root_input_high_water_nsecs,
        Some(oom_cutoff_nsecs)
    );
    assert_eq!(
        transaction.root_output_high_water_nsecs,
        Some(oom_target_nsecs)
    );
    assert_eq!(transaction.attempt_ledger.len(), 1);
    assert_eq!(transaction.attempt_ledger[0].attempt_id, attempt_id);
    assert_eq!(
        transaction.attempt_ledger[0].outcome,
        "preempted_by_resource_pressure"
    );
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::ReopenSameHardware,
        "an OOM after the flush attempt must select release-first reopen"
    );

    transaction.promote_to_resource_pressure(
        790_166_666_666,
        Some(790_200_000_000),
        "Cannot allocate memory again",
        now + Duration::from_millis(2),
    );
    assert_eq!(transaction.target_nsecs, oom_target_nsecs);
    assert_eq!(transaction.observed_target_nsecs, oom_target_nsecs);
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(oom_cutoff_nsecs),
        "later OOM packets must not move the first OOM recovery cutoff"
    );
    assert_eq!(transaction.attempt_ledger.len(), 1);
    assert_eq!(transaction.resource_pressure_errors, 2);
}

#[test]
fn repeated_oom_diagnostics_are_aggregated_once_per_second() {
    let now = Instant::now();
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(
        HevcDecodeChainFallback {
            target_nsecs: 716_633_333_333,
            reason: HevcDecodeChainFallbackReason::ResourcePressure,
        },
        0,
        Some("Cannot allocate memory".to_string()),
        now,
    );

    transaction.record_resource_pressure_error(
        "Cannot allocate memory",
        Some(724_100_000_000),
        now,
    );
    assert_eq!(transaction.resource_pressure_errors, 1);
    assert_eq!(transaction.last_resource_pressure_log_at, Some(now));
    assert_eq!(transaction.suppressed_resource_pressure_errors, 0);

    transaction.record_resource_pressure_error(
        "Cannot allocate memory",
        Some(724_133_333_333),
        now + Duration::from_millis(1),
    );
    assert_eq!(transaction.resource_pressure_errors, 2);
    assert_eq!(transaction.last_resource_pressure_log_at, Some(now));
    assert_eq!(transaction.suppressed_resource_pressure_errors, 1);

    let summary_at = now + Duration::from_secs(1);
    transaction.record_resource_pressure_error(
        "Cannot allocate memory",
        Some(724_166_666_666),
        summary_at,
    );
    assert_eq!(transaction.resource_pressure_errors, 3);
    assert_eq!(transaction.last_resource_pressure_log_at, Some(summary_at));
    assert_eq!(transaction.suppressed_resource_pressure_errors, 0);
}

#[test]
fn repeated_failure_after_one_millisecond_does_not_advance_flush_attempt() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 184_692_319_900,
        reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);

    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::DrainPendingResults
    );
    transaction.drain_recorded = true;
    transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::FlushSameHardware
    );
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            5,
            now + Duration::from_millis(1),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::None,
        "a repeated fallback before the one-second admitted-progress deadline is merged"
    );
    assert_eq!(
        transaction.phase,
        HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
    );
    assert_eq!(transaction.last_result_produced_sequence, 5);
}

#[test]
fn root_failure_count_does_not_fail_attempt_with_current_epoch_progress() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 681_266_667_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
    transaction.set_root_evidence(111, Some(690_633_333_333), Some(680_900_000_000));
    transaction.flush_attempts = 1;
    transaction.reopen_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        20,
        now,
    );

    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 20,
                frame_timeline_nsecs: 681_266_667_000,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: 681_266_667_000,
                before_queue_end_nsecs: Some(681_266_667_000),
                after_queue_end_nsecs: Some(681_826_667_000),
            },
            now,
        ),
        HevcAdmittedVideoProgress::Partial,
        "a fresh Vulkan decoder needs sustained progress before root evidence is cleared"
    );
    transaction.observe_packet(20, Some(681_860_000_000), 0);
    let attempt = transaction.active_attempt.as_ref().expect("active attempt");
    assert_eq!(transaction.root_zero_output_packets, 111);
    assert_eq!(attempt.consecutive_zero_output_packets, 1);
    assert_eq!(attempt.input_high_water_nsecs, Some(681_860_000_000));
    assert_eq!(attempt.admitted_span_after_catch_up_nsecs, 560_000_000);

    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            4,
            now + Duration::from_millis(1),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::None,
        "root evidence must not be reused as the current attempt counter"
    );
    assert_eq!(
        transaction.phase,
        HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
    );
}

#[test]
fn failed_cached_rebuild_drains_pending_decoder_work_before_terminal_action() {
    let now = Instant::now();
    let target_nsecs = 694_233_333_333;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 2_570, None, now);
    transaction.flush_attempts = 1;
    transaction.reopen_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        3_736,
        now,
    );
    transaction
        .active_attempt
        .as_mut()
        .expect("cached rebuild attempt")
        .hard_failure = Some("attempt packet lead reached one second");

    let pending = VideoDecodeWorkerSnapshot {
        state: VideoDecodeWorkerState::Decoding,
        submitted_not_consumed_packets: 1,
        completed_packets: 9,
        ..Default::default()
    };
    assert!(transaction.failed_attempt_needs_decoder_drain(pending, now));

    let drained = VideoDecodeWorkerSnapshot {
        state: VideoDecodeWorkerState::NeedPacket,
        ..Default::default()
    };
    assert!(!transaction.failed_attempt_needs_decoder_drain(drained, now));
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            2_570,
            now,
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::FailExplicitly,
        "ForceVulkan may fail only after pending decoder work is drained"
    );
}

#[test]
fn cached_safe_idr_rebuild_scans_past_ordinary_zero_output_limit_for_next_idr() {
    let now = Instant::now();
    let target_nsecs = 459_500_000_000;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        30,
        now,
    );

    for packet in 1..=60 {
        transaction.observe_packet(30, Some(target_nsecs + packet * 33_333_333), 0);
    }

    let attempt = transaction.active_attempt.as_ref().expect("cached rebuild");
    assert_eq!(attempt.consecutive_zero_output_packets, 60);
    assert_eq!(attempt.hard_failure, None);
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            0,
            now + Duration::from_millis(10),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::None,
        "a final cached rebuild must be allowed to reach the next IDR"
    );

    transaction.observe_packet(30, Some(target_nsecs + 2_033_333_333), 1);
    let attempt = transaction.active_attempt.as_ref().expect("cached rebuild");
    assert_eq!(attempt.consecutive_zero_output_packets, 0);
    assert_eq!(attempt.hard_failure, None);
}

#[test]
fn cached_safe_idr_rebuild_tolerates_mux_rounded_five_second_packet_lead() {
    let now = Instant::now();
    let target_nsecs = 459_500_000_000;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        30,
        now,
    );
    transaction.observe_packet(30, Some(target_nsecs.saturating_add(5_000_041_668)), 0);

    assert_eq!(
        transaction
            .active_attempt
            .as_ref()
            .expect("cached rebuild")
            .hard_failure,
        None
    );

    transaction.observe_packet(30, Some(target_nsecs.saturating_add(5_003_100_001)), 0);

    assert_eq!(
        transaction
            .active_attempt
            .as_ref()
            .expect("cached rebuild")
            .hard_failure,
        Some("cached rebuild packet lead reached five seconds")
    );
}

#[test]
fn failed_replay_does_not_wait_for_wrapper_owned_unsent_packets() {
    let now = Instant::now();
    let target_nsecs = 50_500_000_000;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 2_405, None, now);
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(
        2,
        HevcSameHardwareRecoveryAttemptKind::FlushReplay,
        3_727,
        now,
    );
    transaction
        .active_attempt
        .as_mut()
        .expect("flush replay attempt")
        .hard_failure = Some("attempt packet lead reached one second");

    let wrapper_only_pending = VideoDecodeWorkerSnapshot {
        state: VideoDecodeWorkerState::NeedPacket,
        pending_input_packets: 32,
        pending_input_capacity: 8,
        ..Default::default()
    };
    assert!(wrapper_only_pending.pending_input_full());
    assert!(
        !transaction.failed_attempt_needs_decoder_drain(wrapper_only_pending, now),
        "unsent wrapper packets are cleared by flush/reopen and cannot keep recovery draining"
    );
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            2_405,
            now,
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::ReopenSameHardware
    );
}

#[test]
fn delayed_drain_output_covering_target_cancels_destructive_recovery() {
    let now = Instant::now();
    let target_nsecs = 645_866_666_666;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 1_943, None, now);
    transaction.set_root_evidence(29, Some(647_266_666_666), Some(target_nsecs));

    let progress = transaction.observe_admitted_video_progress(
        HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(4),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 2_855,
            frame_timeline_nsecs: target_nsecs.saturating_add(1),
            frame_duration_nsecs: 33_333_333,
            current_start_position_nsecs: target_nsecs,
            before_queue_end_nsecs: Some(target_nsecs),
            after_queue_end_nsecs: Some(target_nsecs.saturating_add(33_333_334)),
        },
        now + Duration::from_millis(1),
    );

    assert_eq!(progress, HevcAdmittedVideoProgress::Stable);
    assert!(transaction.drain_recorded);
    assert_eq!(transaction.flush_attempts, 0);
    assert_eq!(transaction.reopen_attempts, 0);
    assert_eq!(
        transaction.root_output_high_water_nsecs,
        Some(target_nsecs.saturating_add(33_333_334))
    );
}

#[test]
fn discontinuous_future_drain_output_does_not_cancel_recovery() {
    let now = Instant::now();
    let target_nsecs = 645_866_666_666;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 1_943, None, now);

    let progress = transaction.observe_admitted_video_progress(
        HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(4),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 2_855,
            frame_timeline_nsecs: target_nsecs.saturating_add(1),
            frame_duration_nsecs: 33_333_333,
            current_start_position_nsecs: target_nsecs,
            before_queue_end_nsecs: Some(target_nsecs.saturating_sub(1_000_000_000)),
            after_queue_end_nsecs: Some(target_nsecs.saturating_add(33_333_334)),
        },
        now + Duration::from_millis(1),
    );

    assert_eq!(progress, HevcAdmittedVideoProgress::None);
    assert!(!transaction.drain_recorded);
    assert_eq!(
        transaction.phase,
        HevcSameHardwareRecoveryPhase::DrainingResults
    );
}

#[test]
fn current_epoch_uses_fixed_catch_up_barrier_before_stable_progress() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 1_000_000_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.set_root_evidence(111, Some(1_000_000_000), Some(960_000_000));
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(8, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 20, now);
    transaction.observe_packet(20, Some(1_000_000_000), 1);

    let old_epoch_progress = HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 19,
        frame_timeline_nsecs: 1_000_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 1_000_000_000,
        before_queue_end_nsecs: Some(1_000_000_000),
        after_queue_end_nsecs: Some(1_499_000_000),
    };
    assert_eq!(
        transaction.observe_admitted_video_progress(old_epoch_progress, now),
        HevcAdmittedVideoProgress::None
    );

    let current_epoch_progress = HevcAdmittedVideoProgressObservation {
        generation: 20,
        ..old_epoch_progress
    };
    assert_eq!(
        transaction.observe_admitted_video_progress(current_epoch_progress, now),
        HevcAdmittedVideoProgress::Partial
    );
    transaction.observe_packet(20, Some(2_000_000_000), 0);
    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                frame_timeline_nsecs: 1_499_000_000,
                before_queue_end_nsecs: Some(1_499_000_000),
                after_queue_end_nsecs: Some(1_500_000_000),
                ..current_epoch_progress
            },
            now + Duration::from_millis(1),
        ),
        HevcAdmittedVideoProgress::Partial,
        "new input must not move the barrier frozen at first recovered output"
    );
    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                frame_timeline_nsecs: 1_500_000_000,
                before_queue_end_nsecs: Some(1_500_000_000),
                after_queue_end_nsecs: Some(3_000_000_000),
                ..current_epoch_progress
            },
            now + Duration::from_millis(2),
        ),
        HevcAdmittedVideoProgress::Stable,
        "same-decoder flush recovery needs two seconds of contiguous admitted progress"
    );
    let attempt = transaction.active_attempt.as_ref().expect("active attempt");
    assert_eq!(attempt.catch_up_barrier_nsecs, Some(1_000_000_000));
    assert_eq!(attempt.input_high_water_nsecs, Some(2_000_000_000));
    assert_eq!(attempt.admitted_span_after_catch_up_nsecs, 2_000_000_000);
    assert_eq!(transaction.root_zero_output_packets, 111);
}

#[test]
fn problem_trace_28_short_flush_recovery_escalates_instead_of_restarting_loop() {
    let now = Instant::now();
    let target_nsecs = 1_081_499_988_889;
    let generation = 12_273;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 7_478, None, now);
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::FlushReplay,
        generation,
        now,
    );
    transaction.observe_packet(generation, Some(target_nsecs), 1);
    transaction
        .active_attempt
        .as_mut()
        .expect("flush replay attempt")
        .output_commit_observed = true;

    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(28),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation,
                frame_timeline_nsecs: target_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: 1_080_633_322_222,
                before_queue_end_nsecs: Some(target_nsecs),
                after_queue_end_nsecs: Some(target_nsecs.saturating_add(533_333_328)),
            },
            now + Duration::from_millis(46),
        ),
        HevcAdmittedVideoProgress::Partial,
        "the 16-frame replay burst must not complete the same-decoder transaction"
    );

    for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        transaction.observe_packet(
            generation,
            Some(
                target_nsecs
                    .saturating_add(533_333_328)
                    .saturating_add(packet_index.saturating_mul(33_333_333)),
            ),
            0,
        );
    }
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            7_478,
            now + Duration::from_millis(150),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::ReopenSameHardware,
        "recurrence must reopen Vulkan instead of launching another flush replay"
    );
    assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);
}

#[test]
fn problem_trace_29_short_vulkan_prefix_escalates_to_bounded_cached_rebuild() {
    let now = Instant::now();
    let target_nsecs = 1_146_366_655_555;
    let generation = 6_722;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 308, None, now);
    transaction.flush_attempts = 1;
    transaction.reopen_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        5,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        generation,
        now,
    );
    transaction.observe_packet(generation, Some(target_nsecs), 1);
    transaction
        .active_attempt
        .as_mut()
        .expect("Vulkan reopen attempt")
        .output_commit_observed = true;

    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(15),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation,
                frame_timeline_nsecs: target_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: 1_144_766_655_556,
                before_queue_end_nsecs: Some(target_nsecs),
                after_queue_end_nsecs: Some(target_nsecs.saturating_add(499_999_995)),
            },
            now + Duration::from_millis(266),
        ),
        HevcAdmittedVideoProgress::Partial,
        "the reopened decoder's 500ms prefix must not complete the transaction"
    );

    for packet_index in 1..=HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        transaction.observe_packet(
            generation,
            Some(
                target_nsecs
                    .saturating_add(499_999_995)
                    .saturating_add(packet_index.saturating_mul(33_333_333)),
            ),
            0,
        );
    }
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            308,
            now + Duration::from_millis(430),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::RebuildFromCachedSeek,
        "ForceVulkan must use the final authoritative safe-IDR rebuild"
    );
    assert_eq!(
        transaction.phase,
        HevcSameHardwareRecoveryPhase::RebuildingFromCache
    );
    assert_eq!(
        transaction
            .attempt_ledger
            .last()
            .expect("Vulkan reopen ledger")
            .outcome,
        "escalated_to_cache_rebuild"
    );

    let rebuild_generation = generation + 100;
    transaction
        .begin_cached_rebuild(5, rebuild_generation, now + Duration::from_millis(500))
        .expect("one cached rebuild is allowed");
    let damaged_prefix_start_nsecs = target_nsecs.saturating_add(733_333_326);
    let damaged_prefix_end_nsecs = damaged_prefix_start_nsecs.saturating_add(499_999_995);
    transaction.observe_packet(rebuild_generation, Some(damaged_prefix_start_nsecs), 1);
    transaction
        .active_attempt
        .as_mut()
        .expect("cached rebuild attempt")
        .output_commit_observed = true;
    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(15),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: rebuild_generation,
                frame_timeline_nsecs: damaged_prefix_start_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: target_nsecs,
                before_queue_end_nsecs: Some(target_nsecs),
                after_queue_end_nsecs: Some(damaged_prefix_end_nsecs),
            },
            now + Duration::from_millis(700),
        ),
        HevcAdmittedVideoProgress::Partial,
        "the cached rebuild must remain armed across the damaged GOP"
    );

    for packet_index in 1..=120_u64 {
        transaction.observe_packet(
            rebuild_generation,
            Some(damaged_prefix_end_nsecs.saturating_add(packet_index.saturating_mul(33_333_333))),
            0,
        );
    }
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            308,
            now + Duration::from_secs(4),
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::None,
        "the final cached rebuild must wait for the next IDR instead of restarting"
    );

    let next_idr_nsecs = 1_152_433_322_222;
    transaction.observe_packet(rebuild_generation, Some(next_idr_nsecs), 1);
    assert_eq!(
        transaction.observe_admitted_video_progress(
            HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(15),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: rebuild_generation,
                frame_timeline_nsecs: next_idr_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: target_nsecs,
                before_queue_end_nsecs: Some(damaged_prefix_end_nsecs),
                after_queue_end_nsecs: Some(next_idr_nsecs.saturating_add(2_000_000_000)),
            },
            now + Duration::from_secs(5),
        ),
        HevcAdmittedVideoProgress::Stable,
        "two clean seconds after the next IDR prove recovery"
    );
}

#[test]
fn problem_trace_19_output_commit_accepts_one_frame_boundary_tolerance() {
    let now = Instant::now();
    let generation = 23_478;
    let target_nsecs = 1_196_199_988_759;
    let catch_up_barrier_nsecs = 1_200_599_988_888;
    let recovered_end_nsecs = 1_202_566_655_555;
    let observation = HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(19),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation,
        frame_timeline_nsecs: catch_up_barrier_nsecs,
        frame_duration_nsecs: 33_333_333,
        current_start_position_nsecs: target_nsecs,
        before_queue_end_nsecs: Some(catch_up_barrier_nsecs),
        after_queue_end_nsecs: Some(recovered_end_nsecs),
    };

    let mut speculative = HevcSameHardwareRecoveryAttempt::new(
        3,
        11,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        generation,
        target_nsecs,
        now,
    );
    speculative.input_high_water_nsecs = Some(catch_up_barrier_nsecs);
    assert_eq!(
        speculative.observe_admitted_video_progress(observation, now),
        HevcAdmittedVideoProgress::Partial,
        "speculative recovery must retain the full two-second stability requirement"
    );

    let mut committed = HevcSameHardwareRecoveryAttempt::new(
        3,
        11,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        generation,
        target_nsecs,
        now,
    );
    committed.input_high_water_nsecs = Some(catch_up_barrier_nsecs);
    committed.output_commit_observed = true;
    assert_eq!(
        committed.observe_admitted_video_progress(observation, now),
        HevcAdmittedVideoProgress::Stable,
        "an atomically committed 30fps window one frame below two seconds must not fail later under normal VO backpressure"
    );
    assert_eq!(committed.admitted_span_after_catch_up_nsecs, 1_966_666_667);
}

#[test]
fn completed_attempt_cannot_shrink_required_replay_cutoff() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 681_266_667_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let root_cutoff_nsecs = 690_633_333_333;
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.set_root_evidence(111, Some(root_cutoff_nsecs), Some(681_000_000_000));
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
    transaction.observe_packet(10, Some(683_000_000_000), 0);

    assert_eq!(
        transaction.advance_after_attempt_failure(
            "attempt ended before root cutoff",
            now + Duration::from_millis(1),
            HardwareDecodeMode::Auto,
        ),
        HevcDecodeRecoveryAction::ReopenSameHardware
    );
    assert_eq!(
        transaction.replay_required_high_water_nsecs,
        Some(root_cutoff_nsecs)
    );
    assert_eq!(
        transaction
            .attempt_ledger
            .last()
            .expect("flush attempt ledger")
            .input_high_water_nsecs,
        Some(683_000_000_000)
    );
}

#[test]
fn force_vulkan_orders_flush_reopen_cached_idr_then_explicit_failure() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 716_633_333_333,
        reason: HevcDecodeChainFallbackReason::ResourcePressure,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(
        fallback,
        4,
        Some("Cannot allocate memory".to_string()),
        now,
    );

    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);
    transaction.record_replay(0, false, now + Duration::from_millis(1));
    assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::ReopenSameHardware
    );

    transaction.reopen_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        20,
        now + Duration::from_millis(2),
    );
    transaction.record_replay(0, true, now + Duration::from_millis(3));
    assert_eq!(
        transaction.phase,
        HevcSameHardwareRecoveryPhase::RebuildingFromCache
    );
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::RebuildFromCachedSeek
    );
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::Auto),
        HevcDecodeRecoveryAction::RequestSoftwareFallback
    );
    assert_eq!(transaction.attempt_ledger.len(), 2);
    assert_eq!(
        transaction.attempt_ledger[0].kind,
        HevcSameHardwareRecoveryAttemptKind::FlushReplay
    );
    assert_eq!(transaction.attempt_ledger[0].outcome, "journal_incomplete");
    assert_eq!(
        transaction.attempt_ledger[1].kind,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay
    );
    assert_eq!(transaction.attempt_ledger[1].outcome, "journal_incomplete");

    transaction
        .begin_cached_rebuild(3, 30, now + Duration::from_millis(4))
        .expect("one cached rebuild is allowed");
    assert_eq!(transaction.cached_rebuild_attempts, 1);
    assert_eq!(
        transaction
            .active_attempt
            .as_ref()
            .expect("cached rebuild attempt")
            .kind,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild
    );
    transaction.fail("cached safe-IDR rebuild failed");
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::ForceVulkan),
        HevcDecodeRecoveryAction::FailExplicitly
    );
    assert_eq!(
        transaction.pending_action(HardwareDecodeMode::Auto),
        HevcDecodeRecoveryAction::RequestSoftwareFallback
    );
    let error = transaction.terminal_error(
        now + Duration::from_millis(5),
        HardwareDecodeMode::ForceVulkan,
    );
    assert!(error.contains("cached_rebuild_attempts=1"));
    assert!(error.contains("kind=cached_safe_idr_rebuild"));
}

#[test]
fn same_hardware_reopen_failure_rebuilds_cache_for_force_and_uses_software_for_auto() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 184_692_319_900,
        reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
    };
    let mut forced = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
    forced.flush_attempts = 1;
    forced.reopen_attempts = 1;
    forced.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    forced.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        20,
        now,
    );
    forced
        .active_attempt
        .as_mut()
        .expect("forced attempt")
        .hard_failure = Some("unbridged continuous decode gap");
    assert_eq!(
        forced.advance_after_repeated_failure_if_idle(4, now, HardwareDecodeMode::ForceVulkan,),
        HevcDecodeRecoveryAction::RebuildFromCachedSeek
    );
    assert_eq!(
        forced.phase,
        HevcSameHardwareRecoveryPhase::RebuildingFromCache
    );

    let mut automatic = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
    automatic.flush_attempts = 1;
    automatic.reopen_attempts = 1;
    automatic.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    automatic.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        20,
        now,
    );
    automatic
        .active_attempt
        .as_mut()
        .expect("automatic attempt")
        .hard_failure = Some("unbridged continuous decode gap");
    assert_eq!(
        automatic.advance_after_repeated_failure_if_idle(4, now, HardwareDecodeMode::Auto,),
        HevcDecodeRecoveryAction::RequestSoftwareFallback
    );
    assert_eq!(
        automatic.phase,
        HevcSameHardwareRecoveryPhase::RebuildingFromCache
    );
}

#[test]
fn same_hardware_replay_timeout_advances_flush_then_reopen_once() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 681_266_667_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 4, None, now);
    transaction.flush_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
    transaction.begin_attempt(2, HevcSameHardwareRecoveryAttemptKind::FlushReplay, 10, now);

    let timed_out = now + HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT;
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(4, timed_out, HardwareDecodeMode::Auto,),
        HevcDecodeRecoveryAction::ReopenSameHardware
    );
    assert_eq!(transaction.phase, HevcSameHardwareRecoveryPhase::Reopening);

    transaction.reopen_attempts = 1;
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
        20,
        timed_out,
    );
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            4,
            timed_out + HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT,
            HardwareDecodeMode::Auto,
        ),
        HevcDecodeRecoveryAction::RequestSoftwareFallback
    );
    assert_eq!(transaction.flush_attempts, 1);
    assert_eq!(transaction.reopen_attempts, 1);
}

#[test]
fn same_hardware_recovery_transaction_has_a_hard_wall_time_bound() {
    let now = Instant::now();
    let transaction = HevcSameHardwareRecoveryTransaction::new(
        HevcDecodeChainFallback {
            target_nsecs: 184_692_319_900,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        },
        0,
        None,
        now,
    );
    assert!(
        !transaction
            .expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME - Duration::from_nanos(1))
    );
    assert!(transaction.expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME));
}

#[test]
fn recent_committed_output_progress_defers_wall_time_failure_until_idle() {
    let now = Instant::now();
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 1_186_466_655_435,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        5,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        8_672,
        now,
    );
    let progress_at = now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME - Duration::from_millis(50);
    let attempt = transaction
        .active_attempt
        .as_mut()
        .expect("cached rebuild attempt");
    attempt.output_commit_observed = true;
    attempt.last_admitted_progress_at = Some(progress_at);

    assert!(
        !transaction.expired(now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME),
        "fresh committed frames must not be failed by the transaction's absolute age"
    );
    assert!(
        transaction.expired(progress_at + HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT),
        "the same transaction must still fail after admitted output really goes idle"
    );
}

#[test]
fn problem_trace_8_27_staged_idr_gets_one_bounded_commit_window() {
    let now = Instant::now();
    let target_nsecs = 507_000_020_832;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
    };
    let mut transaction = HevcSameHardwareRecoveryTransaction::new(fallback, 0, None, now);
    transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterReopen;
    transaction.begin_attempt(
        3,
        HevcSameHardwareRecoveryAttemptKind::CachedSafeIdrRebuild,
        9_001,
        now + Duration::from_secs(7),
    );

    let staged_at = now + HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME;
    transaction.observe_packet(9_001, Some(512_066_687_500), 0);
    assert_eq!(
        transaction
            .active_attempt
            .as_ref()
            .expect("cached rebuild attempt")
            .hard_failure,
        Some("cached rebuild packet lead reached five seconds"),
        "PacketDone may cross the five-second lead before the decoded IDR is admitted"
    );
    assert!(
        transaction
            .active_attempt
            .as_mut()
            .expect("cached rebuild attempt")
            .observe_staged_video_progress(9_001, 512_033_333_333, staged_at)
    );
    assert_eq!(
        transaction
            .active_attempt
            .as_ref()
            .expect("cached rebuild attempt")
            .hard_failure,
        None,
        "the already-decoded IDR must clear only the stale packet-derived failure"
    );
    assert_eq!(
        transaction.advance_after_repeated_failure_if_idle(
            1,
            staged_at,
            HardwareDecodeMode::ForceVulkan,
        ),
        HevcDecodeRecoveryAction::None
    );
    assert!(
        !transaction.expired(staged_at),
        "a clean IDR accepted at the 8:27 recovery boundary must survive until atomic commit"
    );
    assert!(
        transaction.expired(staged_at + HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT),
        "unchanged staging must not keep a failed recovery alive indefinitely"
    );
}

#[test]
fn four_worker_results_waiting_for_main_consumption_cannot_trigger_stall() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let now = Instant::now();
    let mut produced_not_consumed = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
    produced_not_consumed.submitted_sequence = 4;
    produced_not_consumed.result_produced_sequence = 4;
    produced_not_consumed.result_consumed_sequence = 0;
    produced_not_consumed.oldest_submitted_packet_nsecs = Some(184_400_000_000);
    watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: produced_not_consumed,
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: output_snapshot(PlaybackOutputState::Syncing, true, false, None, None),
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 184_692_319_900,
    });

    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.startup_in_flight_deadline(), None);
}

#[test]
fn hevc_startup_in_flight_packet_arms_only_near_target_not_during_long_preroll() {
    let target_nsecs = 184_692_319_900;
    assert!(!hevc_startup_in_flight_packet_should_arm(
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        true,
        Some(179_900_000_000),
        target_nsecs,
    ));
    assert!(!hevc_startup_in_flight_packet_should_arm(
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        true,
        Some(180_166_666_667),
        target_nsecs,
    ));
    assert!(hevc_startup_in_flight_packet_should_arm(
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        true,
        Some(184_400_000_000),
        target_nsecs,
    ));
    assert!(!hevc_startup_in_flight_packet_should_arm(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        true,
        Some(target_nsecs),
        target_nsecs,
    ));
}
