use super::*;

#[test]
fn zero_output_logging_uses_only_threshold_and_doubling_milestones() {
    let logged = (1..=64)
        .filter(|count| hevc_zero_output_log_milestone(*count))
        .collect::<Vec<_>>();
    assert_eq!(logged, vec![1, 2, 4, 8, 16, 24, 30, 32, 64]);
}

#[test]
fn post_commit_hevc_replay_status_stays_in_decode_recovery_scope() {
    assert_eq!(
        hevc_decode_packet_evidence_scope(false, false, false, true),
        HevcDecodePacketEvidenceScope::DecodeRecovery,
        "a replay PacketDone can outlive both recovery transactions"
    );
    assert_eq!(
        hevc_decode_packet_evidence_scope(false, false, false, false),
        HevcDecodePacketEvidenceScope::Playback,
        "only a fresh live packet may re-arm the playback watchdog"
    );
}

#[test]
fn exact_seek_scope_precedes_decode_recovery_and_replay_scopes() {
    assert_eq!(
        hevc_decode_packet_evidence_scope(true, true, true, true),
        HevcDecodePacketEvidenceScope::ExactSeek
    );
    assert_eq!(
        hevc_decode_packet_evidence_scope(false, true, false, false),
        HevcDecodePacketEvidenceScope::DecodeRecovery
    );
    assert_eq!(
        hevc_decode_packet_evidence_scope(false, false, true, false),
        HevcDecodePacketEvidenceScope::DecodeRecovery
    );
}

#[test]
fn raw_vulkan_oom_bypasses_the_generic_recoverable_error_classifier() {
    let raw_error = "Vulkan decoder failed: VK_ERROR_OUT_OF_DEVICE_MEMORY";
    assert!(!super::super::video_decode_error_is_recoverable(raw_error));
    assert!(video_decode_error_requires_hevc_resource_pressure_recovery(
        raw_error,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        true,
    ));
    assert!(
        !video_decode_error_requires_hevc_resource_pressure_recovery(
            raw_error,
            ffi::AVCodecID::AV_CODEC_ID_H264,
            true,
        )
    );
    assert!(
        !video_decode_error_requires_hevc_resource_pressure_recovery(
            raw_error,
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            false,
        )
    );
}

#[test]
fn repeated_cra_low_level_landing_ignores_range_and_packet_identity() {
    let first = HevcLowLevelSeekLanding {
        transaction_id: 11,
        target_nsecs: 62_521_000_000,
        seek_position_nsecs: 61_521_000_000,
        anchor_nsecs: 59_768_000_000,
        anchor_kind: VideoRecoveryPointKind::Cra,
        range_id: Some(2),
        anchor_packet_id: Some(41),
    };
    let repeated = HevcLowLevelSeekLanding {
        range_id: Some(3),
        anchor_packet_id: Some(93),
        ..first
    };
    assert!(hevc_cra_low_level_landing_repeats(first, repeated));

    let different_anchor = HevcLowLevelSeekLanding {
        anchor_nsecs: first.anchor_nsecs + 1,
        ..repeated
    };
    assert!(!hevc_cra_low_level_landing_repeats(first, different_anchor));
}

#[test]
fn same_cra_tuple_suppresses_second_low_level_seek() {
    let landing = HevcLowLevelSeekLanding {
        transaction_id: 11,
        target_nsecs: 62_521_000_000,
        seek_position_nsecs: 61_521_000_000,
        anchor_nsecs: 59_768_000_000,
        anchor_kind: VideoRecoveryPointKind::Cra,
        range_id: Some(2),
        anchor_packet_id: Some(41),
    };

    assert!(hevc_low_level_seek_would_repeat_cra(
        Some(landing),
        landing.target_nsecs,
        landing.seek_position_nsecs,
    ));
    assert!(!hevc_low_level_seek_would_repeat_cra(
        Some(landing),
        landing.target_nsecs + 1,
        landing.seek_position_nsecs,
    ));
}

#[test]
fn exact_seek_first_frame_releases_hevc_startup_watchdog() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        first_zero_output_at: Some(now - Duration::from_secs(1)),
        startup_in_flight_stall_started_at: Some(now - Duration::from_secs(1)),
        startup_watchdog_retry_not_before: Some(now),
        startup_watchdog_last_rejection_at: Some(now),
        startup_watchdog_last_rejection_reason: Some("test"),
        startup_watchdog_suppressed_rejections: 17,
        ..HevcDecodeChainWatchdog::default()
    };
    assert!(watchdog.startup_watchdog_deadline(true).is_some());

    watchdog.complete_startup_watchdog_after_first_frame();

    assert!(watchdog.startup_watchdog_deadline(true).is_none());
    assert!(watchdog.startup_watchdog_completed);
    assert!(watchdog.startup_watchdog_last_rejection_at.is_none());
    assert_eq!(watchdog.startup_watchdog_suppressed_rejections, 0);

    watchdog.reset_transient_after_progress(None, Some(184_740_000_000), now);
    assert!(watchdog.startup_watchdog_completed);
    assert!(watchdog.startup_watchdog_deadline(true).is_none());
}

#[test]
fn hevc_decode_packet_diagnostic_window_keeps_recent_packet_deltas() {
    let video_stream = StreamInfo {
        index: 0,
        stream: std::ptr::null_mut(),
        decoder: std::ptr::null(),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        time_base: ffi::AVRational { num: 1, den: 1_000 },
        start_nsecs: None,
        frame_duration_nsecs: Some(40_000_000),
    };
    let mut diagnostics = HevcDecodePacketDiagnosticWindow::default();
    for index in 0..HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY + 2 {
        let mut packet = packet_from_data(&[0, 0, 1, 0x02, index as u8]);
        unsafe {
            (*packet.as_mut_ptr()).pts = 1_000 + i64::try_from(index).unwrap() * 40;
            (*packet.as_mut_ptr()).dts = 960 + i64::try_from(index).unwrap() * 40;
            (*packet.as_mut_ptr()).duration = 40;
        }
        diagnostics.record(
            &VideoDecodePacketStatus {
                generation: u64::try_from(index).unwrap(),
                result: Ok(()),
                decoded_frames: 0,
                elapsed: Duration::from_micros(250),
                drained: false,
                drop_policy: VideoDecodeDropPolicy::None,
            },
            &packet,
            video_stream,
            u64::try_from(index + 1).unwrap(),
            true,
        );
    }

    assert_eq!(
        diagnostics.packets.len(),
        HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY
    );
    assert_eq!(diagnostics.packets.front().unwrap().ordinal, 3);
    let latest = diagnostics.packets.back().unwrap();
    assert_eq!(latest.pts_delta_nsecs, Some(40_000_000));
    assert_eq!(latest.dts_delta_nsecs, Some(40_000_000));
    assert_eq!(latest.packet.duration_nsecs, Some(40_000_000));
    assert!(latest.hardware_accelerated);
    assert_eq!(
        latest.zero_output_run_packets,
        u64::try_from(HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY + 2).unwrap()
    );
}

#[test]
fn full_pending_video_decode_input_reports_packet_queue_full() {
    let info = worker_info(false);
    let reason = VideoDecodePipeline::block_reason_for(
        snapshot(
            VideoDecodeWorkerState::NeedPacket,
            VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
            0,
        ),
        &info,
    );

    assert_eq!(reason, Some(PlaybackBlockReason::PacketQueueFull));
}

#[test]
fn in_flight_video_decode_command_queue_reports_decoder_in_flight() {
    let info = worker_info(false);
    let reason = VideoDecodePipeline::block_reason_for(
        snapshot(VideoDecodeWorkerState::Decoding, 0, 4),
        &info,
    );

    assert_eq!(reason, Some(PlaybackBlockReason::DecoderInFlight));
}

#[test]
fn completed_video_decode_status_reports_decoder_output_pending_when_command_queue_full() {
    let info = worker_info(false);
    let mut snapshot = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
    snapshot.completed_packets = 1;
    let reason = VideoDecodePipeline::block_reason_for(snapshot, &info);

    assert_eq!(reason, Some(PlaybackBlockReason::DecoderOutputPending));
}

#[test]
fn empty_video_decode_input_reports_decoder_input_empty() {
    let info = worker_info(false);
    let reason = VideoDecodePipeline::block_reason_for(
        snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
        &info,
    );

    assert_eq!(reason, Some(PlaybackBlockReason::DecoderInputEmpty));
}

#[test]
fn output_full_video_decode_reports_surface_or_decoded_queue() {
    let software = worker_info(false);
    let hardware = worker_info(true);

    assert_eq!(
        VideoDecodePipeline::block_reason_for(
            snapshot(VideoDecodeWorkerState::OutputFull, 0, 0),
            &software,
        ),
        Some(PlaybackBlockReason::DecodedQueueFull)
    );
    assert_eq!(
        VideoDecodePipeline::block_reason_for(
            snapshot(VideoDecodeWorkerState::OutputFull, 0, 0),
            &hardware,
        ),
        Some(PlaybackBlockReason::HwSurfacePool)
    );
}

#[test]
fn hevc_zero_output_watchdog_enters_suspected_at_24_without_flushing() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let low_water = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    for packet_index in 0..23_u64 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                1_600_000_000 + packet_index * 1_000_000,
                low_water,
                demux_watermark(false),
                1_250_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    let action = watchdog.observe_packet(hevc_watchdog_input(
        1_623_000_000,
        low_water,
        demux_watermark(false),
        1_250_000_000,
    ));

    assert_eq!(action, HevcDecodeChainRecoveryAction::None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_zero_output_watchdog_keeps_hardware_decode_when_rebuffer_has_video_headroom() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let queued_video_end_nsecs = 583_322_222;
    let action = watchdog.observe_packet(hevc_watchdog_input(
        1_375_322_222,
        output_snapshot(
            PlaybackOutputState::Rebuffering,
            true,
            false,
            Some((0, queued_video_end_nsecs)),
            Some(queued_video_end_nsecs),
        ),
        demux_watermark(false),
        0,
    ));

    assert_eq!(action, HevcDecodeChainRecoveryAction::None);
    assert!(!watchdog.soft_recovery_attempted);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_zero_output_does_not_soft_recover_after_two_packets() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);

    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            40_000_000,
            startup,
            demux_watermark(false),
            0,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            80_000_000,
            startup,
            demux_watermark(false),
            0,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_zero_output_first_frame_timeout_waits_for_hard_budget() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let now = Instant::now();
    let mut input = hevc_watchdog_input(40_000_000, startup, demux_watermark(false), 0);
    input.now = now;

    assert_eq!(
        watchdog.observe_packet(input),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
            now: now + Duration::from_millis(750),
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        }),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_zero_output_waits_until_hard_packet_budget() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT - 1 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                40_000_000 * (index + 1),
                startup,
                demux_watermark(false),
                0,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_zero_output_hard_fallbacks_after_timeout() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let now = Instant::now();
    let mut input = hevc_watchdog_input(120_000_000, startup, demux_watermark(false), 120_000_000);
    input.now = now;
    assert_eq!(
        watchdog.observe_packet(input),
        HevcDecodeChainRecoveryAction::None
    );
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
        now: now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 120_000_000,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 120_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn hevc_startup_in_flight_hard_fallbacks_after_timeout_without_packet_status() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
    let now = Instant::now();
    let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

    assert_eq!(
        watchdog.observe_startup_stall(HevcStartupStallObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            hardware_accelerated: true,
            video_decode_snapshot: in_flight,
            now,
            output_snapshot: startup,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 0,
        }),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.startup_in_flight_deadline(),
        Some(now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
    );

    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: in_flight,
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 0,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        })
    );
}

#[test]
fn hevc_startup_in_flight_deadline_can_be_armed_at_enqueue() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
    let now = Instant::now();
    let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);
    watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

    assert_eq!(
        watchdog.startup_in_flight_deadline(),
        Some(now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER)
    );
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: in_flight,
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 0,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        })
    );
}

#[test]
fn hevc_startup_in_flight_timeout_does_not_require_output_rebuffer_flag() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let playing_without_video =
        output_snapshot(PlaybackOutputState::Playing, false, false, None, None);
    let now = Instant::now();
    let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: in_flight,
        now,
        output_snapshot: playing_without_video,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: in_flight,
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: playing_without_video,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 0,
            reason: HevcDecodeChainFallbackReason::StartupInFlightStall,
        })
    );
}

#[test]
fn hevc_zero_output_packet_status_refreshes_in_flight_progress_deadline() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
    let now = Instant::now();
    watchdog.arm_startup_in_flight_stall(PlaybackSessionId(1), now);

    let mut input = hevc_watchdog_input(40_000_000, startup, demux_watermark(false), 0);
    input.now = now + Duration::from_millis(500);
    assert_eq!(
        watchdog.observe_packet(input),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.startup_in_flight_deadline(), None);

    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: true,
        video_decode_snapshot: snapshot(VideoDecodeWorkerState::Decoding, 0, 4),
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });

    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_in_flight_timeout_requires_hardware_decoder() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Rebuffering, true, false, None, None);
    let now = Instant::now();
    let in_flight = snapshot(VideoDecodeWorkerState::Decoding, 0, 4);

    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: false,
        video_decode_snapshot: in_flight,
        now,
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: false,
        video_decode_snapshot: in_flight,
        now: now + HEVC_STARTUP_IN_FLIGHT_HARD_AFTER + Duration::from_secs(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: 0,
    });

    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.startup_in_flight_deadline(), None);
}

#[test]
fn software_decoder_does_not_publish_hevc_startup_deadline() {
    let now = Instant::now();
    let watchdog = HevcDecodeChainWatchdog {
        first_zero_output_at: Some(now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
        ..Default::default()
    };

    assert_eq!(watchdog.startup_watchdog_deadline(false), None);
    assert!(watchdog.startup_watchdog_deadline(true).is_some());
}

#[test]
fn rejected_hevc_startup_deadline_is_rearmed_in_the_future() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        first_zero_output_at: Some(
            now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER - Duration::from_millis(1),
        ),
        ..Default::default()
    };

    assert!(watchdog.startup_watchdog_deadline(true).unwrap() <= now);
    watchdog.defer_startup_watchdog_after_no_action(now);
    assert_eq!(
        watchdog.startup_watchdog_deadline(true),
        Some(now + HEVC_STARTUP_WATCHDOG_RETRY_AFTER)
    );
}

#[test]
fn hevc_startup_watchdog_pauses_for_input_and_rearms_on_submission() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        zero_output_packets: 6,
        first_zero_output_at: Some(now - HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER),
        ..Default::default()
    };

    assert!(watchdog.startup_watchdog_deadline(true).is_some());
    assert!(watchdog.suspend_startup_watchdog_for_input_wait());
    assert_eq!(watchdog.startup_watchdog_deadline(true), None);

    watchdog.resume_startup_watchdog_after_packet_submission(now);

    assert_eq!(
        watchdog.startup_watchdog_deadline(true),
        Some(now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER)
    );
}

#[test]
fn hevc_startup_rejection_logging_is_rate_limited() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog::default();

    assert_eq!(
        watchdog.record_startup_watchdog_rejection("decoder_not_decoding", now),
        Some(0)
    );
    assert_eq!(
        watchdog.record_startup_watchdog_rejection(
            "decoder_not_decoding",
            now + Duration::from_millis(1),
        ),
        None
    );
    assert_eq!(
        watchdog.record_startup_watchdog_rejection(
            "decoder_not_decoding",
            now + HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL,
        ),
        Some(1)
    );
}

#[test]
fn software_long_gop_startup_timeout_scales_with_preroll_distance() {
    let target_nsecs = 669_625_000_000;
    let first_packet_nsecs = 663_833_000_000;
    let timeout = hevc_startup_zero_output_timeout(false, target_nsecs, Some(first_packet_nsecs));

    assert_eq!(timeout, Duration::from_millis(19_584));
    assert!(timeout > HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER);
}

#[test]
fn software_long_gop_zero_output_does_not_fallback_at_hardware_timeout() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let target_nsecs = 669_625_000_000;
    let first_packet_nsecs = 663_833_000_000;
    let now = Instant::now();
    let mut first = hevc_watchdog_input(
        first_packet_nsecs,
        startup,
        demux_watermark(false),
        target_nsecs,
    );
    first.hardware_accelerated = false;
    first.now = now;
    assert_eq!(
        watchdog.observe_packet(first),
        HevcDecodeChainRecoveryAction::None
    );

    let mut target =
        hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
    target.hardware_accelerated = false;
    target.now = now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1);
    assert_eq!(
        watchdog.observe_packet(target),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.take_fallback(), None);

    let timeout = hevc_startup_zero_output_timeout(false, target_nsecs, Some(first_packet_nsecs));
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: false,
        video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
        now: now + timeout + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: target_nsecs,
    });
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn hevc_startup_zero_output_hard_fallbacks_after_packet_budget() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);

    for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                40_000_000 * (index + 1),
                startup,
                demux_watermark(false),
                0,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 0,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn hevc_startup_zero_output_waits_for_seek_target_before_packet_budget_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let target_nsecs = 83_177_300_977;
    let first_preroll_packet_nsecs = 78_882_000_000;

    for index in 0..HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                first_preroll_packet_nsecs + 40_000_000 * index,
                startup,
                demux_watermark(false),
                target_nsecs,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.take_fallback(), None);

    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            target_nsecs,
            startup,
            demux_watermark(false),
            target_nsecs,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn six_zero_output_preroll_packets_continue_to_clean_target_frame() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let target_nsecs = 184_692_319_900;
    for packet_nsecs in [
        179_900_000_000,
        180_166_666_667,
        180_200_000_000,
        180_233_333_333,
        180_266_666_666,
        180_299_999_999,
    ] {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                packet_nsecs,
                startup,
                demux_watermark(false),
                target_nsecs,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.zero_output_packets, 6);
    assert_eq!(watchdog.take_fallback(), None);
    assert!(watchdog.suspend_startup_watchdog_for_input_wait());

    watchdog.resume_startup_watchdog_after_packet_submission(Instant::now());
    let mut target =
        hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
    target.decoded_frames = 1;

    assert_eq!(
        watchdog.observe_packet(target),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_startup_zero_output_timeout_waits_for_seek_target() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let startup = output_snapshot(PlaybackOutputState::Syncing, false, false, None, None);
    let target_nsecs = 83_177_300_977;
    let now = Instant::now();
    let mut preroll = hevc_watchdog_input(
        81_200_000_000,
        startup,
        demux_watermark(false),
        target_nsecs,
    );
    preroll.now = now;

    assert_eq!(
        watchdog.observe_packet(preroll),
        HevcDecodeChainRecoveryAction::None
    );
    watchdog.observe_startup_stall(HevcStartupStallObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        hardware_accelerated: false,
        video_decode_snapshot: snapshot(VideoDecodeWorkerState::NeedPacket, 0, 0),
        now: now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(1),
        output_snapshot: startup,
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        fallback_target_nsecs: target_nsecs,
    });
    assert_eq!(watchdog.take_fallback(), None);

    let mut target =
        hevc_watchdog_input(target_nsecs, startup, demux_watermark(false), target_nsecs);
    target.now = now + HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER + Duration::from_millis(2);
    assert_eq!(
        watchdog.observe_packet(target),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}
