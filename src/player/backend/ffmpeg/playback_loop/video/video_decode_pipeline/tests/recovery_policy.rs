use super::*;

#[test]
fn hevc_zero_output_watchdog_bounds_stable_queue_boundary_grace() {
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
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            1_623_000_000,
            low_water,
            demux_watermark(false),
            1_250_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);

    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((900_000_000, 2_000_000_000)),
        Some(1_100_000_000),
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            3_100_000_000,
            stable_output,
            demux_watermark(false),
            1_333_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.take_fallback(), None);

    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            4_100_000_000,
            stable_output,
            demux_watermark(false),
            1_333_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.take_fallback(), None);

    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            7_600_000_000,
            stable_output,
            demux_watermark(false),
            1_333_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 2_000_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn problem_trace_9_49_waits_for_safe_idr_instead_of_resetting_vulkan() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output_end_nsecs = 589_233_333_332;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((588_100_000_000, output_end_nsecs)),
        Some(1_162_000_000),
    );

    for packet_index in 0..33_u64 {
        let input = hevc_watchdog_input(
            589_666_666_652 + packet_index * 33_333_333,
            stable_output,
            demux_watermark(false),
            588_071_260_797,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.pending_fallback(), None);
    }
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);

    let mut safe_idr = hevc_watchdog_input(
        590_833_312_500,
        stable_output,
        demux_watermark(false),
        588_071_260_797,
    );
    safe_idr.safe_seek_point = true;
    safe_idr.decoded_frames = 3;
    assert_eq!(
        watchdog.observe_packet(safe_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.zero_output_safe_anchor_nsecs, None);
    assert_eq!(
        watchdog.recent_zero_output_safe_anchor_nsecs,
        Some(590_833_312_500)
    );
}

#[test]
fn problem_trace_2_37_immediate_idr_output_replays_video_only_gap() {
    let frozen_end_nsecs = 157_166_645_832;
    let recovery_packet_nsecs = 160_000_000_000;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((156_166_687_500, frozen_end_nsecs)),
        Some(1_042_241_342),
    );
    let healthy_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(43_633_291_672),
        audio_forward_nsecs: Some(44_565_333_333),
        selected_min_forward_nsecs: Some(43_633_291_672),
        ..Default::default()
    };
    let mut watchdog = HevcDecodeChainWatchdog::default();

    for packet_index in 0..84_u64 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                157_399_979_156 + packet_index * 33_333_333,
                stable_output,
                healthy_demux,
                156_117_346_911,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);

    let mut clean_idr = hevc_watchdog_input(
        recovery_packet_nsecs,
        stable_output,
        healthy_demux,
        156_117_346_911,
    );
    clean_idr.safe_seek_point = true;
    clean_idr.decoded_frames = 3;
    assert_eq!(
        watchdog.observe_packet(clean_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.recent_zero_output_safe_anchor_nsecs,
        Some(recovery_packet_nsecs)
    );

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    gap.session_id = PlaybackSessionId(8);
    gap.timeline_nsecs = recovery_packet_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(frozen_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        recovery_packet_nsecs.saturating_sub(frozen_end_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = frozen_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(156_114_255_910);
    gap.demux_watermark = healthy_demux;
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        corrupt: false,
        decode_error_flags: 0,
        ..Default::default()
    };

    assert_eq!(recovery_packet_nsecs - frozen_end_nsecs, 2_833_354_168);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: frozen_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_7_20_replays_vulkan_reference_chain_gap() {
    let frozen_end_nsecs = 439_633_333_332;
    let recovery_frame_nsecs = 442_466_687_500;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 93,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(442_766_645_832),
        recent_output_high_water_nsecs: Some(frozen_end_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(recovery_frame_nsecs),
        recent_audio_timeline_gap_checked: true,
        last_decoded_video_end_nsecs: Some(frozen_end_nsecs),
        ..Default::default()
    };
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((438_466_687_500, frozen_end_nsecs)),
        Some(1_166_645_832),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
    gap.session_id = PlaybackSessionId(3);
    gap.timeline_nsecs = recovery_frame_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(frozen_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        recovery_frame_nsecs.saturating_sub(frozen_end_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = frozen_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(438_270_000_000);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(44_000_000_000),
        audio_forward_nsecs: Some(44_000_000_000),
        selected_min_forward_nsecs: Some(44_000_000_000),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        corrupt: false,
        decode_error_flags: 0,
        ..Default::default()
    };

    assert_eq!(recovery_frame_nsecs - frozen_end_nsecs, 2_833_354_168);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: frozen_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn problem_trace_5_05_submitted_idr_survives_delayed_frame_admission() {
    let session_id = PlaybackSessionId(10);
    let frozen_end_nsecs = 305_066_645_832;
    let previous_expected_next_nsecs = 305_166_645_832;
    let safe_idr_nsecs = 307_466_687_500;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((304_000_000_000, frozen_end_nsecs)),
        Some(1_066_645_832),
    );
    let healthy_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(43_666_666_667),
        audio_forward_nsecs: Some(44_544_000_000),
        selected_min_forward_nsecs: Some(43_666_666_667),
        ..Default::default()
    };
    let mut watchdog = HevcDecodeChainWatchdog::default();

    let first_zero_output_packet_nsecs = 305_399_979_156;
    let zero_output_span_nsecs = 2_133_375_000;
    for packet_index in 0..68_u64 {
        let mut input = hevc_watchdog_input(
            first_zero_output_packet_nsecs + zero_output_span_nsecs * packet_index / 67,
            stable_output,
            healthy_demux,
            previous_expected_next_nsecs,
        );
        input.session_id = session_id;
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert!(watchdog.strong_recent_high_water_evidence());

    // In the real trace this safe IDR returned three delayed pre-IDR
    // frames before its PacketDone result. Arm the boundary when the
    // packet enters the queue, then reproduce that result ordering.
    watchdog.observe_submitted_safe_recovery_point(session_id, Some(safe_idr_nsecs), true, true);
    assert_eq!(
        watchdog.recent_zero_output_safe_anchor_nsecs,
        Some(safe_idr_nsecs)
    );

    let delayed_frames = [
        (305_066_687_500, 305_100_020_833),
        (305_100_020_833, 305_133_354_166),
        (305_133_354_166, previous_expected_next_nsecs),
    ];
    let mut before_queue_end_nsecs = Some(frozen_end_nsecs);
    for (frame_timeline_nsecs, after_queue_end_nsecs) in delayed_frames {
        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id,
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 10,
                frame_timeline_nsecs,
                frame_duration_nsecs: after_queue_end_nsecs.saturating_sub(frame_timeline_nsecs),
                current_start_position_nsecs: 303_033_333_333,
                before_queue_end_nsecs,
                after_queue_end_nsecs: Some(after_queue_end_nsecs),
            },),
            HevcAdmittedVideoProgress::Partial
        );
        before_queue_end_nsecs = Some(after_queue_end_nsecs);
    }
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.zero_output_safe_anchor_nsecs, None);
    assert_eq!(
        watchdog.recent_zero_output_safe_anchor_nsecs,
        Some(safe_idr_nsecs),
        "delayed frame admission must not erase the submitted IDR boundary"
    );

    let recovered_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((304_000_000_000, previous_expected_next_nsecs)),
        Some(1_166_645_832),
    );
    let mut safe_idr_done = hevc_watchdog_input(
        safe_idr_nsecs,
        recovered_output,
        healthy_demux,
        previous_expected_next_nsecs,
    );
    safe_idr_done.session_id = session_id;
    safe_idr_done.safe_seek_point = true;
    safe_idr_done.decoded_frames = 3;
    assert_eq!(
        watchdog.observe_packet(safe_idr_done),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.recent_zero_output_safe_anchor_nsecs,
        Some(safe_idr_nsecs),
        "PacketDone arrives too late to reconstruct the cleared zero-output run"
    );

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, recovered_output);
    gap.session_id = session_id;
    gap.timeline_nsecs = safe_idr_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        safe_idr_nsecs.saturating_sub(previous_expected_next_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = previous_expected_next_nsecs;
    gap.audio_played_timeline_nsecs = Some(304_114_255_910);
    gap.demux_watermark = healthy_demux;
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        corrupt: false,
        decode_error_flags: 0,
        ..Default::default()
    };

    assert_eq!(safe_idr_nsecs - previous_expected_next_nsecs, 2_300_041_668);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: previous_expected_next_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_6_07_replays_mux_rounded_five_second_idr_gap() {
    let previous_expected_next_nsecs = 367_466_645_832;
    let safe_idr_nsecs = 372_466_687_500;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((366_366_687_500, previous_expected_next_nsecs)),
        Some(1_099_958_332),
    );
    let healthy_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(54_200_062_504),
        audio_forward_nsecs: Some(57_322_666_666),
        selected_min_forward_nsecs: Some(54_200_062_504),
        ..Default::default()
    };
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 151,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(372_766_645_832),
        recent_output_high_water_nsecs: Some(previous_expected_next_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(safe_idr_nsecs),
        recent_audio_timeline_gap_checked: true,
        last_decoded_video_end_nsecs: Some(previous_expected_next_nsecs),
        ..Default::default()
    };

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    gap.session_id = PlaybackSessionId(5);
    gap.timeline_nsecs = safe_idr_nsecs;
    gap.duration_nsecs = 33_333_332;
    gap.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        safe_idr_nsecs.saturating_sub(previous_expected_next_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = previous_expected_next_nsecs;
    gap.audio_played_timeline_nsecs = Some(366_472_150_474);
    gap.demux_watermark = healthy_demux;
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        corrupt: false,
        decode_error_flags: 0,
        ..Default::default()
    };

    assert_eq!(safe_idr_nsecs - previous_expected_next_nsecs, 5_000_041_668);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: previous_expected_next_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_11_09_stable_output_waits_for_safe_idr() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output_end_nsecs = 668_966_645_832;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((667_923_285_688, output_end_nsecs)),
        Some(1_043_360_144),
    );
    let first_zero_output_packet_nsecs = 669_399_979_152;

    for packet_index in 0..49_u64 {
        let input = hevc_watchdog_input(
            first_zero_output_packet_nsecs + packet_index * 33_333_333,
            stable_output,
            demux_watermark(false),
            667_923_285_688,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(
        watchdog.pending_fallback(),
        None,
        "a bridgeable PTS lead must not request recovery before the mapped IDR"
    );

    let safe_anchor_nsecs = 671_066_645_816;
    let mut safe_idr = hevc_watchdog_input(
        safe_anchor_nsecs,
        stable_output,
        demux_watermark(false),
        667_923_285_688,
    );
    safe_idr.safe_seek_point = true;
    assert_eq!(
        watchdog.observe_packet(safe_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);
    assert_eq!(
        watchdog.zero_output_safe_anchor_nsecs,
        Some(safe_anchor_nsecs)
    );

    let mut recovered = hevc_watchdog_input(
        safe_anchor_nsecs + 33_333_333,
        stable_output,
        demux_watermark(false),
        667_923_285_688,
    );
    recovered.decoded_frames = 4;
    assert_eq!(
        watchdog.observe_packet(recovered),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.zero_output_safe_anchor_nsecs, None);
}

#[test]
fn problem_trace_12_18_replays_clean_3_366s_video_only_gap() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output_end_nsecs = 738_433_333_332;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((737_387_723_475, output_end_nsecs)),
        Some(1_045_609_857),
    );
    let first_zero_output_packet_nsecs = 738_866_666_652;

    for packet_index in 0..=91_u64 {
        let input = hevc_watchdog_input(
            first_zero_output_packet_nsecs + packet_index * 33_333_333,
            stable_output,
            demux_watermark(false),
            737_387_723_475,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.pending_fallback(), None);
    }

    let mut safe_idr = hevc_watchdog_input(
        741_900_000_000,
        stable_output,
        demux_watermark(false),
        737_387_723_475,
    );
    safe_idr.safe_seek_point = true;
    safe_idr.decoded_frames = 3;
    assert_eq!(
        watchdog.observe_packet(safe_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);
    assert!(watchdog.recent_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT);

    let previous_expected_next_nsecs = 738_533_333_332;
    let gap_nsecs = 3_366_666_668_u64;
    let gap_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((737_487_723_475, previous_expected_next_nsecs)),
        Some(1_045_609_857),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, gap_output);
    gap.timeline_nsecs = 741_900_000_000;
    gap.duration_nsecs = 33_333_332;
    gap.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(gap_nsecs));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = previous_expected_next_nsecs;
    gap.audio_played_timeline_nsecs = Some(737_387_723_475);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(43_600_041_660),
        audio_forward_nsecs: Some(42_794_666_667),
        selected_min_forward_nsecs: Some(42_794_666_667),
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
            target_nsecs: previous_expected_next_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_17_37_replays_instead_of_bridging_4_166s_video_gap() {
    let previous_expected_next_nsecs = 1_056_900_020_833;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 134,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_061_300_020_829),
        recent_output_high_water_nsecs: Some(previous_expected_next_nsecs),
        last_decoded_video_end_nsecs: Some(previous_expected_next_nsecs),
        ..Default::default()
    };
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_055_600_000_000, previous_expected_next_nsecs)),
        Some(1_325_645_087),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    gap.session_id = PlaybackSessionId(5);
    gap.timeline_nsecs = 1_061_066_687_500;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    gap.previous_gap_nsecs = Some(4_166_666_667);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = previous_expected_next_nsecs;
    gap.audio_played_timeline_nsecs = Some(1_055_741_042_413);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(40_333_333_333),
        audio_forward_nsecs: Some(43_114_666_666),
        selected_min_forward_nsecs: Some(40_333_333_333),
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
            target_nsecs: previous_expected_next_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn problem_trace_19_34_bridges_small_prefix_then_replays_clean_idr_gap() {
    let previous_expected_next_nsecs = 1_173_633_333_332;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 51,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_175_700_020_832),
        recent_output_high_water_nsecs: Some(previous_expected_next_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(1_175_633_333_316),
        last_decoded_video_end_nsecs: Some(previous_expected_next_nsecs),
        ..Default::default()
    };
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_172_600_000_000, previous_expected_next_nsecs)),
        Some(1_092_399_992),
    );
    let healthy_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(42_733_354_160),
        audio_forward_nsecs: Some(42_922_666_667),
        selected_min_forward_nsecs: Some(42_733_354_160),
        ..Default::default()
    };

    let mut non_key_gap =
        decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    non_key_gap.timeline_nsecs = 1_173_866_687_500;
    non_key_gap.duration_nsecs = 33_333_332;
    non_key_gap.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    non_key_gap.previous_gap_nsecs = Some(233_354_168);
    non_key_gap.max_gap_nsecs = 200_000_000;
    non_key_gap.fallback_target_nsecs = previous_expected_next_nsecs;
    non_key_gap.audio_played_timeline_nsecs = Some(1_172_540_933_340);
    non_key_gap.demux_watermark = healthy_demux;
    non_key_gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: false,
        corrupt: false,
        decode_error_flags: 0,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(non_key_gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.pending_fallback(), None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 51);

    let non_key_end_nsecs = 1_173_900_020_832;
    let mut idr_gap =
        decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    idr_gap.timeline_nsecs = 1_175_400_000_000;
    idr_gap.duration_nsecs = 33_333_332;
    idr_gap.previous_expected_next_nsecs = Some(non_key_end_nsecs);
    idr_gap.previous_gap_nsecs = Some(1_499_979_168);
    idr_gap.max_gap_nsecs = 200_000_000;
    idr_gap.fallback_target_nsecs = non_key_end_nsecs;
    idr_gap.audio_played_timeline_nsecs = Some(1_172_540_933_340);
    idr_gap.demux_watermark = healthy_demux;
    idr_gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(idr_gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: non_key_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn safe_idr_without_output_exhausts_a_small_packet_grace() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output_end_nsecs = 100_000_000_000;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((99_000_000_000, output_end_nsecs)),
        Some(1_000_000_000),
    );

    for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        let input = hevc_watchdog_input(
            output_end_nsecs + 600_000_000 + packet_index * 20_000_000,
            stable_output,
            demux_watermark(false),
            output_end_nsecs,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }

    let safe_anchor_nsecs = output_end_nsecs + 1_200_000_000;
    let mut safe_idr = hevc_watchdog_input(
        safe_anchor_nsecs,
        stable_output,
        demux_watermark(false),
        output_end_nsecs,
    );
    safe_idr.safe_seek_point = true;
    assert_eq!(
        watchdog.observe_packet(safe_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);

    for packet_index in 1..=HEVC_DECODE_CHAIN_SAFE_ANCHOR_GRACE_PACKETS {
        let input = hevc_watchdog_input(
            safe_anchor_nsecs + packet_index * 20_000_000,
            stable_output,
            demux_watermark(false),
            output_end_nsecs,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        if packet_index < HEVC_DECODE_CHAIN_SAFE_ANCHOR_GRACE_PACKETS {
            assert_eq!(watchdog.pending_fallback(), None);
        }
    }
    assert_eq!(
        watchdog.take_fallback().map(|fallback| fallback.reason),
        Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
    );
}

#[test]
fn post_soft_recovery_skips_reach_hard_fallback_without_waiting_for_idr() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_500_000_000),
        last_decoded_video_end_nsecs: Some(1_000_000_000),
        ..Default::default()
    };
    let low_water = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );

    for skipped_index in 0..5_u64 {
        watchdog.observe_post_soft_recovery_skipped_packet(
            HevcPostSoftRecoverySkippedPacketObservation {
                session_id: PlaybackSessionId(1),
                packet_nsecs: Some(1_510_000_000 + skipped_index * 10_000_000),
                cache_sequence_contiguous: true,
                hardware_accelerated: true,
                output_snapshot: low_water,
                demux_watermark: demux_watermark(false),
                has_audio_output: true,
                fallback_target_nsecs: 900_000_000,
            },
        );
        assert_eq!(watchdog.pending_fallback(), None);
    }
    watchdog.observe_post_soft_recovery_skipped_packet(
        HevcPostSoftRecoverySkippedPacketObservation {
            session_id: PlaybackSessionId(1),
            packet_nsecs: Some(1_560_000_000),
            cache_sequence_contiguous: true,
            hardware_accelerated: true,
            output_snapshot: low_water,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 900_000_000,
        },
    );

    assert_eq!(watchdog.post_soft_recovery_skipped_packets, 6);
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );

    let mut one_second_lead = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_500_000_000),
        last_decoded_video_end_nsecs: Some(1_000_000_000),
        ..Default::default()
    };
    one_second_lead.observe_post_soft_recovery_skipped_packet(
        HevcPostSoftRecoverySkippedPacketObservation {
            session_id: PlaybackSessionId(1),
            packet_nsecs: Some(2_000_000_000),
            cache_sequence_contiguous: true,
            hardware_accelerated: true,
            output_snapshot: low_water,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 900_000_000,
        },
    );
    assert_eq!(
        one_second_lead
            .take_fallback()
            .map(|fallback| fallback.reason),
        Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
    );

    let mut demux_unhealthy = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_500_000_000),
        last_decoded_video_end_nsecs: Some(1_000_000_000),
        ..Default::default()
    };
    demux_unhealthy.observe_post_soft_recovery_skipped_packet(
        HevcPostSoftRecoverySkippedPacketObservation {
            session_id: PlaybackSessionId(1),
            packet_nsecs: Some(2_000_000_000),
            cache_sequence_contiguous: true,
            hardware_accelerated: true,
            output_snapshot: low_water,
            demux_watermark: demux_watermark(true),
            has_audio_output: true,
            fallback_target_nsecs: 900_000_000,
        },
    );
    assert_eq!(demux_unhealthy.take_fallback(), None);
}

#[test]
fn post_soft_recovery_skips_wait_while_output_queue_is_stable() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        recent_input_packet_high_water_nsecs: Some(1_500_000_000),
        last_decoded_video_end_nsecs: Some(1_000_000_000),
        ..Default::default()
    };
    let mut stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_000_000_000, 2_460_000_000)),
        Some(1_460_000_000),
    );
    stable_output.video_decode_underfill = true;

    watchdog.observe_post_soft_recovery_skipped_packet(
        HevcPostSoftRecoverySkippedPacketObservation {
            session_id: PlaybackSessionId(1),
            packet_nsecs: Some(2_000_000_000),
            cache_sequence_contiguous: true,
            hardware_accelerated: true,
            output_snapshot: stable_output,
            demux_watermark: demux_watermark(false),
            has_audio_output: true,
            fallback_target_nsecs: 900_000_000,
        },
    );

    assert_eq!(watchdog.post_soft_recovery_skipped_packets, 0);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn strong_high_water_fallback_is_not_discarded_by_preexisting_progress_grace() {
    assert!(!HevcDecodeChainFallbackReason::ZeroOutputRebuffer.invalidated_by_video_progress());
    assert!(!HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput.invalidated_by_video_progress());
    assert!(HevcDecodeChainFallbackReason::StartupInFlightStall.invalidated_by_video_progress());
}

#[test]
fn hevc_68_packet_trace_with_sparse_output_keeps_failure_evidence() {
    assert_sparse_output_progress_does_not_erase_high_water_failure(68, 2_300_000_000);
}

#[test]
fn hevc_132_packet_trace_with_sparse_output_keeps_failure_evidence() {
    assert_sparse_output_progress_does_not_erase_high_water_failure(132, 4_400_000_000);
}

#[test]
fn synchronized_audio_gap_suppresses_packet_level_high_water_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    let synchronized_gap = AudioTimelineGapEvidence {
        previous_end_nsecs: 1_000_000_000,
        next_start_nsecs: 2_000_000_000,
    };

    for packet_index in 0..40_u64 {
        let mut input = hevc_watchdog_input(
            2_000_000_000 + packet_index * 40_000_000,
            output,
            demux_watermark(false),
            1_000_000_000,
        );
        input.synchronized_audio_timeline_gap = (packet_index == 23).then_some(synchronized_gap);
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }

    assert!(watchdog.recent_zero_output_packets >= 30);
    assert!(watchdog.recent_packet_lead_exceeded);
    assert_eq!(
        watchdog.recent_synchronized_audio_timeline_gap,
        Some(synchronized_gap)
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
}

#[test]
fn high_water_waits_until_synchronized_audio_gap_was_checked() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );

    for packet_index in 0..40_u64 {
        let mut input = hevc_watchdog_input(
            2_000_000_000 + packet_index * 40_000_000,
            output,
            demux_watermark(false),
            1_000_000_000,
        );
        input.synchronized_audio_timeline_gap_checked = false;
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
    }

    assert!(!watchdog.recent_audio_timeline_gap_checked);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_high_water_survives_scheduled_queue_drain_and_keeps_gap_boundary_target() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 960_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 0,
        before_queue_end_nsecs: Some(960_000_000),
        after_queue_end_nsecs: Some(1_000_000_000),
    });
    let drained_output = output_snapshot(PlaybackOutputState::Playing, false, true, None, None);
    for packet_index in 0..68_u64 {
        let _ = watchdog.observe_packet(hevc_watchdog_input(
            1_500_000_000 + packet_index * 10_000_000,
            drained_output,
            demux_watermark(false),
            900_000_000,
        ));
    }

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
        })
    );
}

#[test]
fn video_decode_recovery_tracks_skipped_packet_pts_span() {
    let mut recovery = VideoDecodeRecovery::default();
    recovery.begin_with_realign(false);

    assert_eq!(recovery.record_skipped_packet(Some(1_000_000_000)), 1);
    assert_eq!(recovery.skipped_packet_span_nsecs(), Some(0));
    assert_eq!(recovery.record_skipped_packet(Some(2_250_000_000)), 2);
    assert_eq!(recovery.skipped_packet_span_nsecs(), Some(1_250_000_000));

    recovery.reset();
    assert_eq!(recovery.skipped_packet_span_nsecs(), None);
}
