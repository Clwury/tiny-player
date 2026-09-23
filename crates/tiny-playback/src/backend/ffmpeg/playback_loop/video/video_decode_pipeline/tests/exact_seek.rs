use super::*;

#[test]
fn hevc_decode_recovery_rejects_unkeyed_cra_after_wait_limit_until_safe_idr() {
    let mut recovery = VideoDecodeRecovery::default();
    let non_recovery_packet = crate::backend::ffmpeg::AvPacket::new().expect("packet allocates");
    recovery.begin_with_realign(false);

    for index in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
        assert!(
            recovery.should_skip_packet(&non_recovery_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC)
        );
        recovery.record_skipped_packet(Some(index * 40_000_000));
    }

    let unkeyed_cra = packet_from_data(&[
        0, 0, 0, 3, 0x2a, 0x01, 0xaa, // CRA_NUT
    ]);
    assert!(packet_is_video_recovery_point(
        &unkeyed_cra,
        ffi::AVCodecID::AV_CODEC_ID_HEVC
    ));
    assert!(!packet_is_video_seek_point(
        &unkeyed_cra,
        ffi::AVCodecID::AV_CODEC_ID_HEVC
    ));
    assert!(recovery.should_skip_packet(&unkeyed_cra, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_hevc_recovery_point_after_wait_limit(
        &unkeyed_cra,
        ffi::AVCodecID::AV_CODEC_ID_HEVC
    ));
    assert!(recovery.waiting_for_keyframe());

    let mut safe_idr = packet_from_data(&[
        0, 0, 0, 3, 0x26, 0x01, 0xaa, // IDR_W_RADL
    ]);
    unsafe {
        (*safe_idr.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }
    assert!(!recovery.should_skip_packet(&safe_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.accept_recovery_point(&safe_idr, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.waiting_for_keyframe());
}

#[test]
fn hevc_decode_recovery_accepts_keyed_cra_after_wait_limit() {
    let mut recovery = VideoDecodeRecovery::default();
    recovery.begin_with_realign(false);
    recovery.record_skipped_packet(Some(0));
    recovery.record_skipped_packet(Some(HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS));

    let mut keyed_cra = packet_from_data(&[
        0, 0, 0, 3, 0x2a, 0x01, 0xaa, // CRA_NUT
    ]);
    unsafe {
        (*keyed_cra.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }
    assert!(!packet_is_video_seek_point(
        &keyed_cra,
        ffi::AVCodecID::AV_CODEC_ID_HEVC
    ));
    assert!(!recovery.should_skip_packet(&keyed_cra, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(
        recovery.accept_hevc_recovery_point_after_wait_limit(
            &keyed_cra,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        )
    );
    assert!(!recovery.waiting_for_keyframe());
}

#[test]
fn exact_low_level_seek_decodes_from_226s_cra_and_gates_output_at_235s_target() {
    let transaction_id = 23;
    let anchor_nsecs = 226_810_000_000;
    let target_nsecs = 235_235_000_000;
    let next_cra_nsecs = 237_237_000_000;
    let mut recovery = VideoDecodeRecovery::default();
    let mut cra_packet = packet_from_data(&[
        0, 0, 0, 3, 0x2a, 0x01, 0xaa, // CRA_NUT
    ]);
    unsafe {
        (*cra_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }
    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);
    assert!(recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));

    let landing = HevcLowLevelSeekLanding {
        transaction_id,
        target_nsecs,
        seek_position_nsecs: 234_235_000_000,
        anchor_nsecs,
        anchor_kind: VideoRecoveryPointKind::Cra,
        range_id: Some(1),
        anchor_packet_id: Some(430),
    };
    recovery.enable_hevc_low_level_recovery_point(landing);
    assert!(matches!(
        recovery.recovery_scope(),
        VideoDecodeRecoveryScope::ExactLowLevelSeek { .. }
    ));
    assert!(recovery.requires_exact_seek_output());
    assert!(recovery.should_skip_nonref_for_seek_preroll(Some(anchor_nsecs), false, false,));
    assert!(recovery.should_skip_nonref_for_seek_preroll(
        Some(target_nsecs - EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS - 1),
        false,
        false,
    ));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(
        Some(target_nsecs - EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS),
        false,
        false,
    ));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(target_nsecs), false, false,));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(None, false, false));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(anchor_nsecs), true, false,));
    assert!(
        recovery
            .observe_exact_seek_packet_progress(Some(anchor_nsecs))
            .is_some()
    );
    assert!(
        recovery
            .observe_exact_seek_packet_progress(Some(anchor_nsecs))
            .is_none(),
        "duplicate packet PTS is not recovery progress"
    );
    assert!(
        recovery
            .observe_exact_seek_packet_progress(Some(anchor_nsecs - 1))
            .is_none(),
        "backward packet PTS is not recovery progress"
    );
    assert!(
        recovery
            .observe_exact_seek_packet_progress(Some(anchor_nsecs + 41_000_000))
            .is_some(),
        "forward packet PTS refreshes exact-seek recovery progress"
    );
    assert!(!recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.accept_recovery_point(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));

    assert_eq!(
        decoded_video_frame_start_action(
            target_nsecs - 1,
            target_nsecs,
            false,
            recovery.requires_exact_seek_output(),
        ),
        super::super::super::DecodedVideoFrameStartAction::DropBeforeStart
    );
    assert_eq!(
        decoded_video_frame_start_action(
            target_nsecs,
            target_nsecs,
            false,
            recovery.requires_exact_seek_output(),
        ),
        super::super::super::DecodedVideoFrameStartAction::Use { realign: false }
    );
    assert!(target_nsecs < next_cra_nsecs);

    recovery
        .finish_seek_bootstrap_after_target_frame(target_nsecs)
        .expect("target frame completes the exact low-level transaction");
    let completion = recovery
        .take_exact_seek_completion()
        .expect("exact transaction records its first eligible frame");
    assert_eq!(completion.transaction_id, transaction_id);
    assert_eq!(completion.first_eligible_frame_nsecs, target_nsecs);
    assert_eq!(completion.first_eligible_delta_nsecs, 0);
    assert!(!recovery.requires_exact_seek_output());
}

#[test]
fn exact_seek_nonref_skip_does_not_reenable_after_reordered_packet_pts() {
    let target_nsecs = 694_233_333_333;
    let mut recovery = VideoDecodeRecovery {
        recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 5,
            target_nsecs,
        },
        ..Default::default()
    };
    let before_target_nsecs = 694_100_000_000;
    let after_target_nsecs = 694_300_000_000;

    assert!(recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), false, false,));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(after_target_nsecs), false, false,));
    assert!(
        !recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), false, false,)
    );
}

#[test]
fn problem_trace_14_32_precise_vulkan_seek_decodes_full_preroll() {
    let target_nsecs = 869_966_687_500;
    let anchor_nsecs = 865_700_000_000;
    let mut recovery = VideoDecodeRecovery {
        recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 10,
            target_nsecs,
        },
        ..Default::default()
    };

    for packet_nsecs in [
        anchor_nsecs,
        target_nsecs - EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS - 1,
        target_nsecs,
    ] {
        assert!(
            !recovery.should_skip_nonref_for_seek_preroll(Some(packet_nsecs), false, true,),
            "precise Vulkan seek must preserve the complete HEVC reference chain"
        );
    }
}

#[test]
fn verified_replay_owns_an_exact_frame_start_boundary_until_target() {
    let current_start_nsecs = 339_720_000_000;
    let target_nsecs = 381_760_000_000;
    let mut recovery = VideoDecodeRecovery::default();

    recovery.begin_verified_replay_from_safe_anchor(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);

    assert_eq!(recovery.verified_replay_target_nsecs(), Some(target_nsecs));
    assert_eq!(
        recovery.frame_start_position_nsecs(current_start_nsecs),
        target_nsecs
    );
    assert!(recovery.requires_exact_seek_output());
    assert_eq!(
        decoded_video_frame_start_action(
            372_840_000_000,
            recovery.frame_start_position_nsecs(current_start_nsecs),
            false,
            recovery.requires_exact_seek_output(),
        ),
        super::super::super::DecodedVideoFrameStartAction::DropBeforeStart
    );

    recovery
        .finish_seek_bootstrap_after_target_frame(target_nsecs)
        .expect("target frame completes verified replay bootstrap");
    assert_eq!(recovery.verified_replay_target_nsecs(), None);
    assert_eq!(
        recovery.frame_start_position_nsecs(current_start_nsecs),
        current_start_nsecs
    );
}

#[test]
fn bounded_cached_rebuild_disables_nonref_skip_across_reordered_packet_pts() {
    let target_nsecs = 694_233_333_333;
    let mut recovery = VideoDecodeRecovery {
        recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 5,
            target_nsecs,
        },
        ..Default::default()
    };
    let before_target_nsecs = 694_100_000_000;
    let after_target_nsecs = 694_300_000_000;

    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), true, true,));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(after_target_nsecs), true, true,));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(Some(before_target_nsecs), true, true,));
}

#[test]
fn exact_seek_decoder_results_are_isolated_from_playback_root_evidence() {
    let target_nsecs = 235_235_000_000;
    let recovery_scope = VideoDecodeRecoveryScope::ExactLowLevelSeek {
        transaction_id: 23,
        target_nsecs,
        seek_position_nsecs: 234_235_000_000,
        actual_anchor_nsecs: 226_810_000_000,
        actual_anchor_kind: VideoRecoveryPointKind::Cra,
    };
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog::default();

    for index in 0..50_u64 {
        assert!(watchdog.observe_exact_seek_decoder_result(
            recovery_scope,
            Some(231_000_000_000 + index * 33_333_333),
            0,
            true,
            now + Duration::from_micros(index),
        ));
    }
    assert_eq!(watchdog.exact_seek_zero_output_packets, 50);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);

    watchdog.complete_exact_seek_evidence_scope(
        23,
        target_nsecs + 33_333_333,
        false,
        false,
        now + Duration::from_millis(1),
    );
    assert_eq!(watchdog.exact_seek_transaction_id, None);
    assert_eq!(watchdog.exact_seek_zero_output_packets, 0);
    assert_eq!(
        watchdog.recent_output_high_water_nsecs,
        Some(target_nsecs + 33_333_333)
    );
    assert_eq!(
        watchdog.last_decoded_video_end_nsecs,
        Some(target_nsecs + 33_333_333)
    );

    let committed_end_nsecs = target_nsecs + 66_666_666;
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((target_nsecs + 33_333_333, committed_end_nsecs)),
        Some(33_333_333),
    );
    for index in 1..=3_u64 {
        let mut input = hevc_watchdog_input(
            committed_end_nsecs + index * 33_333_333,
            output,
            demux_watermark(false),
            committed_end_nsecs,
        );
        input.now = now + Duration::from_millis(index + 1);
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.recent_zero_output_packets, 3);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn cached_exact_seek_preroll_zero_outputs_stay_in_the_seek_scope() {
    let target_nsecs = 1_050_500_000_000;
    let recovery_scope = VideoDecodeRecoveryScope::ExactCachedSeek {
        transaction_id: 29,
        target_nsecs,
    };
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog::default();

    for index in 0..58_u64 {
        assert!(watchdog.observe_exact_seek_decoder_result(
            recovery_scope,
            Some(1_047_733_333_000 + index * 33_333_333),
            0,
            true,
            now + Duration::from_micros(index),
        ));
    }
    assert_eq!(watchdog.exact_seek_zero_output_packets, 58);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
    assert_eq!(watchdog.pending_fallback, None);
    assert_eq!(watchdog.completed_exact_seek_transaction_id, None);
    assert_eq!(watchdog.completed_exact_seek_landing_nsecs, None);

    watchdog.complete_exact_seek_evidence_scope(
        29,
        1_050_533_333_000,
        false,
        true,
        now + Duration::from_millis(1),
    );
    assert_eq!(watchdog.exact_seek_transaction_id, None);
    assert_eq!(watchdog.completed_exact_seek_transaction_id, Some(29));
    assert_eq!(
        watchdog.completed_exact_seek_landing_nsecs,
        Some(1_050_533_333_000)
    );
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert_eq!(
        watchdog.recent_output_high_water_nsecs,
        Some(1_050_533_333_000)
    );
    assert_eq!(watchdog.pending_fallback, None);

    watchdog.reset();
    assert_eq!(watchdog.completed_exact_seek_transaction_id, None);
    assert_eq!(watchdog.completed_exact_seek_landing_nsecs, None);
}

#[test]
fn problem_trace_191_exact_seek_zero_outputs_requests_recovery_instead_of_4_867s_hold() {
    let now = Instant::now();
    let transaction_id = 17;
    let target_nsecs = 765_666_666_667;
    let first_eligible_frame_nsecs = 765_766_666_667;
    let first_eligible_end_nsecs = 765_800_000_000;
    let seek_input_high_water_nsecs = 770_966_666_666;
    let mut watchdog = HevcDecodeChainWatchdog {
        exact_seek_transaction_id: Some(transaction_id),
        exact_seek_zero_output_packets: 191,
        exact_seek_input_high_water_nsecs: Some(seek_input_high_water_nsecs),
        ..Default::default()
    };

    watchdog.complete_exact_seek_evidence_scope(
        transaction_id,
        first_eligible_frame_nsecs,
        false,
        true,
        now,
    );

    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 191);
    assert!(watchdog.recent_packet_lead_exceeded);
    assert_eq!(
        watchdog.recent_input_packet_high_water_nsecs,
        Some(seek_input_high_water_nsecs)
    );
    assert_eq!(
        watchdog.recent_output_high_water_nsecs,
        Some(first_eligible_frame_nsecs)
    );
    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(17),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 11_153,
            frame_timeline_nsecs: first_eligible_frame_nsecs,
            frame_duration_nsecs: 33_333_333,
            current_start_position_nsecs: target_nsecs,
            before_queue_end_nsecs: None,
            after_queue_end_nsecs: Some(first_eligible_end_nsecs),
        }),
        HevcAdmittedVideoProgress::Partial
    );
    assert_eq!(watchdog.recent_zero_output_packets, 191);
    assert!(watchdog.recent_packet_lead_exceeded);

    let snapshot = output_snapshot(
        PlaybackOutputState::Syncing,
        false,
        false,
        Some((first_eligible_frame_nsecs, first_eligible_end_nsecs)),
        Some(33_333_333),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 770_666_666_667;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(first_eligible_end_nsecs);
    gap.previous_gap_nsecs = Some(4_866_666_667);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = first_eligible_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(target_nsecs);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(25_766_655_556),
        audio_forward_nsecs: Some(30_255_600_907),
        selected_min_forward_nsecs: Some(25_766_655_556),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: first_eligible_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn exact_seek_completion_inside_decode_recovery_preserves_root_evidence() {
    let now = Instant::now();
    let root_output_nsecs = 681_266_667_000;
    let root_input_nsecs = 690_633_333_333;
    let recovery_scope = VideoDecodeRecoveryScope::ExactCachedSeek {
        transaction_id: 31,
        target_nsecs: root_output_nsecs,
    };
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 111,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(root_input_nsecs),
        recent_output_high_water_nsecs: Some(root_output_nsecs),
        last_decoded_video_end_nsecs: Some(root_output_nsecs),
        ..HevcDecodeChainWatchdog::default()
    };

    assert!(watchdog.observe_exact_seek_decoder_result(
        recovery_scope,
        Some(714_266_667_000),
        0,
        true,
        now,
    ));
    watchdog.complete_exact_seek_evidence_scope(
        31,
        716_533_333_000,
        true,
        true,
        now + Duration::from_millis(1),
    );

    assert_eq!(watchdog.exact_seek_transaction_id, None);
    assert_eq!(watchdog.exact_seek_zero_output_packets, 0);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 111);
    assert!(watchdog.recent_packet_lead_exceeded);
    assert_eq!(
        watchdog.recent_input_packet_high_water_nsecs,
        Some(root_input_nsecs)
    );
    assert_eq!(
        watchdog.recent_output_high_water_nsecs,
        Some(root_output_nsecs),
        "an uncommitted cached-rebuild frame must not advance root output"
    );
    assert_eq!(
        watchdog.last_decoded_video_end_nsecs,
        Some(root_output_nsecs)
    );
}

#[test]
fn decode_recovery_pts_gap_is_routed_without_mutating_root_watchdog() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 30,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(690_633_333_333),
        recent_output_high_water_nsecs: Some(681_266_667_000),
        ..HevcDecodeChainWatchdog::default()
    };
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((681_000_000_000, 681_266_667_000)),
        Some(266_667_000),
    );
    let mut observation = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
    observation.previous_expected_next_nsecs = Some(681_266_667_000);
    observation.timeline_nsecs = 690_633_333_000;
    observation.previous_gap_nsecs = Some(9_366_666_000);
    observation.decode_recovery_active = true;

    assert_eq!(
        watchdog.observe_decoded_frame_gap(observation),
        HevcDecodedFrameGapAction::Admit
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 30);
    assert_eq!(watchdog.pending_fallback, None);

    observation.audio_timeline_gap = Some(AudioTimelineGapEvidence {
        previous_end_nsecs: 681_266_667_000,
        next_start_nsecs: 690_633_333_000,
    });
    assert_eq!(
        watchdog.observe_decoded_frame_gap(observation),
        HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 30);
    assert_eq!(
        watchdog.recent_output_high_water_nsecs,
        Some(681_266_667_000)
    );
}

#[test]
fn decode_recovery_suspends_secondary_watchdogs_but_keeps_root_evidence() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 30,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(2_000_000_000),
        recent_output_high_water_nsecs: Some(1_000_000_000),
        pending_fallback: Some(HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        }),
        post_fallback_rebuffer_underfill_started_at: Some(now),
        first_zero_output_at: Some(now),
        startup_in_flight_stall_started_at: Some(now),
        startup_watchdog_retry_not_before: Some(now),
        startup_waiting_for_input: true,
        ..HevcDecodeChainWatchdog::default()
    };

    watchdog.suspend_playback_watchdogs_for_decode_recovery();

    assert_eq!(watchdog.pending_fallback, None);
    assert_eq!(watchdog.post_fallback_rebuffer_underfill_started_at, None);
    assert_eq!(watchdog.first_zero_output_at, None);
    assert_eq!(watchdog.startup_in_flight_stall_started_at, None);
    assert_eq!(watchdog.startup_watchdog_retry_not_before, None);
    assert!(!watchdog.startup_waiting_for_input);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 30);
    assert_eq!(
        watchdog.recent_input_packet_high_water_nsecs,
        Some(2_000_000_000)
    );
    assert_eq!(watchdog.recent_output_high_water_nsecs, Some(1_000_000_000));
}

#[test]
fn active_decode_recovery_cannot_retrigger_the_playback_watchdog() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        recent_zero_output_packets: 29,
        recent_input_packet_high_water_nsecs: Some(2_000_000_000),
        recent_output_high_water_nsecs: Some(1_000_000_000),
        health_state: HevcDecodeHealthState::Suspected,
        ..HevcDecodeChainWatchdog::default()
    };

    for index in 0..222_u64 {
        watchdog.observe_packet_during_decode_recovery(
            true,
            u64::from(index == 0),
            now + Duration::from_micros(index),
        );
    }

    assert_eq!(watchdog.recent_zero_output_packets, 29);
    assert_eq!(
        watchdog.recent_input_packet_high_water_nsecs,
        Some(2_000_000_000)
    );
    assert_eq!(watchdog.recent_output_high_water_nsecs, Some(1_000_000_000));
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_seek_bootstrap_counts_preroll_and_clears_at_target_frame() {
    let mut recovery = VideoDecodeRecovery::default();
    let target_nsecs = 12_800_000_000;

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);

    let first_progress = recovery
        .observe_seek_preroll_frame(8_360_000_000)
        .expect("HEVC seek bootstrap tracks preroll");
    assert_eq!(first_progress.target_nsecs, target_nsecs);
    assert_eq!(first_progress.preroll_frames, 1);
    assert_eq!(recovery.seek_bootstrap_preroll_frames(), 1);

    let second_progress = recovery
        .observe_seek_preroll_frame(8_400_000_000)
        .expect("HEVC seek bootstrap keeps tracking preroll");
    assert_eq!(second_progress.preroll_frames, 2);
    assert_eq!(
        second_progress.first_preroll_frame_nsecs,
        Some(8_360_000_000)
    );
    assert_eq!(
        second_progress.last_preroll_frame_nsecs,
        Some(8_400_000_000)
    );

    let completed = recovery
        .finish_seek_bootstrap_after_target_frame(target_nsecs)
        .expect("first target frame completes bootstrap");
    assert_eq!(completed.preroll_frames, 2);
    assert_eq!(recovery.seek_bootstrap_preroll_frames(), 0);
    assert!(recovery.observe_seek_preroll_frame(8_440_000_000).is_none());
}

#[test]
fn hevc_recovery_transaction_escalates_across_target_and_reason_drift() {
    let target_nsecs = 83_177_300_977;
    let fallback = HevcDecodeChainFallback {
        target_nsecs,
        reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
    };
    let now = Instant::now();
    let hardware_record = HevcDecodeChainFallbackRecord {
        root_target_nsecs: target_nsecs,
        last_target_nsecs: target_nsecs,
        last_reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
        hardware_accelerated: true,
        recorded_at: now,
        software_suppressions: 0,
        post_low_level_suppressions: 0,
        low_level_seeks: 0,
    };
    let software_record = HevcDecodeChainFallbackRecord {
        hardware_accelerated: false,
        ..hardware_record
    };

    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(hardware_record), fallback, true),
        HevcDecodeChainFallbackLoopAction::ForceSoftware
    );
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(software_record), fallback, false),
        HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
    );

    let mut suppressed_record = software_record;
    suppressed_record.software_suppressions = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(
            Some(suppressed_record),
            HevcDecodeChainFallback {
                target_nsecs: target_nsecs + 360_000_000,
                reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            },
            false,
        ),
        HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek
    );

    suppressed_record.low_level_seeks = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(suppressed_record), fallback, false,),
        HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
    );
    suppressed_record.post_low_level_suppressions = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(suppressed_record), fallback, false,),
        HevcDecodeChainFallbackLoopAction::RecoveryExhausted
    );
}

#[test]
fn hevc_recovery_record_keeps_root_target_across_fallback_drift() {
    let now = Instant::now();
    let first = HevcDecodeChainFallback {
        target_nsecs: 123_000_000_000,
        reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
    };
    let second = HevcDecodeChainFallback {
        target_nsecs: 123_360_000_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let first_record = hevc_decode_chain_fallback_record_after(None, first, false, now);
    let second_record = hevc_decode_chain_fallback_record_after(
        Some(first_record),
        second,
        false,
        now + Duration::from_millis(100),
    );

    assert_eq!(second_record.root_target_nsecs, first.target_nsecs);
    assert_eq!(second_record.last_target_nsecs, second.target_nsecs);
    assert_eq!(second_record.last_reason, second.reason);
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(second_record), second, false,),
        HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
    );
}

#[test]
fn hevc_recovery_generation_transient_reset_preserves_fallback_record() {
    let fallback = HevcDecodeChainFallback {
        target_nsecs: 123_360_000_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let record = hevc_decode_chain_fallback_record_after(None, fallback, false, Instant::now());

    assert_eq!(
        hevc_decode_chain_recovery_record_after_reset(
            Some(record),
            HevcDecodeChainResetScope::Transient,
        ),
        Some(record)
    );
    assert_eq!(
        hevc_decode_chain_recovery_record_after_reset(
            Some(record),
            HevcDecodeChainResetScope::RecoveryTransaction,
        ),
        None
    );
}

#[test]
fn hevc_recovery_transaction_bounds_internal_seek_resets() {
    let cached_fallback = HevcDecodeChainFallback {
        target_nsecs: 123_000_000_000,
        reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
    };
    let zero_output_fallback = HevcDecodeChainFallback {
        target_nsecs: 123_360_000_000,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    };
    let now = Instant::now();
    let mut internal_seek_resets = 0;

    assert_eq!(
        hevc_decode_chain_fallback_loop_action(None, cached_fallback, true),
        HevcDecodeChainFallbackLoopAction::Proceed
    );
    internal_seek_resets += 1;
    let hardware_record = hevc_decode_chain_fallback_record_after(None, cached_fallback, true, now);
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(hardware_record), zero_output_fallback, true,),
        HevcDecodeChainFallbackLoopAction::ForceSoftware
    );

    let software_record = hevc_decode_chain_fallback_record_after(
        Some(hardware_record),
        zero_output_fallback,
        false,
        now + Duration::from_millis(1),
    );
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(Some(software_record), zero_output_fallback, false,),
        HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
    );

    let mut suppressed_record = software_record;
    suppressed_record.software_suppressions = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(
            Some(suppressed_record),
            zero_output_fallback,
            false,
        ),
        HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek
    );
    internal_seek_resets += 1;

    suppressed_record.low_level_seeks = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(
            Some(suppressed_record),
            zero_output_fallback,
            false,
        ),
        HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek
    );
    suppressed_record.post_low_level_suppressions = 1;
    assert_eq!(
        hevc_decode_chain_fallback_loop_action(
            Some(suppressed_record),
            zero_output_fallback,
            false,
        ),
        HevcDecodeChainFallbackLoopAction::RecoveryExhausted
    );
    assert_eq!(internal_seek_resets, 2);
}
