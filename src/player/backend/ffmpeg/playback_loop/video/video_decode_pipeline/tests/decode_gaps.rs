use super::*;

#[test]
fn hevc_post_fallback_rebuffer_underfill_uses_playback_target_for_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let mut rebuffering = output_snapshot(
        PlaybackOutputState::Rebuffering,
        true,
        true,
        Some((93_080_000_000, 93_200_000_000)),
        Some(120_000_000),
    );
    rebuffering.video_bootstrap_after_seek = true;
    let now = Instant::now();

    watchdog.observe_post_fallback_rebuffer_underfill(HevcPostFallbackRebufferObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        now,
        output_snapshot: rebuffering,
        demux_watermark: demux_watermark(false),
        audio_ready: true,
        fallback_target_nsecs: 93_080_000_000,
        decode_recovery_active: false,
    });
    assert_eq!(watchdog.take_fallback(), None);

    watchdog.observe_post_fallback_rebuffer_underfill(HevcPostFallbackRebufferObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        now: now + HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER + Duration::from_millis(1),
        output_snapshot: rebuffering,
        demux_watermark: demux_watermark(false),
        audio_ready: true,
        fallback_target_nsecs: 93_080_000_000,
        decode_recovery_active: false,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 93_080_000_000,
            reason: HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill,
        })
    );
}

#[test]
fn hevc_zero_output_watchdog_does_not_recover_when_demux_underruns() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let action = watchdog.observe_packet(hevc_watchdog_input(
        1_600_000_000,
        output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((900_000_000, 1_000_000_000)),
            Some(100_000_000),
        ),
        demux_watermark(true),
        1_250_000_000,
    ));

    assert_eq!(action, HevcDecodeChainRecoveryAction::None);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_zero_output_watchdog_resets_after_decoder_or_admitted_video_progress() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            1_100_000_000,
            snapshot,
            demux_watermark(false),
            1_250_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 1);

    let mut progress = hevc_watchdog_input(
        1_133_000_000,
        snapshot,
        demux_watermark(false),
        1_250_000_000,
    );
    progress.decoded_frames = 1;
    assert_eq!(
        watchdog.observe_packet(progress),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 0);

    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            1_166_000_000,
            snapshot,
            demux_watermark(false),
            1_250_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 1);

    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 1_133_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 1_100_000_000,
        before_queue_end_nsecs: Some(1_100_000_000),
        after_queue_end_nsecs: Some(1_173_000_000),
    });
    assert_eq!(watchdog.zero_output_packets, 0);
    assert!(!watchdog.soft_recovery_attempted);
}

#[test]
fn recent_software_video_progress_defers_packet_lead_recovery() {
    let now = Instant::now();
    let mut watchdog = HevcDecodeChainWatchdog {
        last_video_progress_at: Some(now),
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Rebuffering,
        true,
        false,
        Some((1_000_000_000, 1_100_000_000)),
        Some(100_000_000),
    );
    let mut during_grace = hevc_watchdog_input(
        1_800_000_000,
        snapshot,
        demux_watermark(false),
        1_000_000_000,
    );
    during_grace.hardware_accelerated = false;
    during_grace.now = now + Duration::from_millis(1_999);

    assert_eq!(
        watchdog.observe_packet(during_grace),
        HevcDecodeChainRecoveryAction::None
    );
    assert!(!watchdog.soft_recovery_attempted);

    let mut after_grace = during_grace;
    after_grace.packet_nsecs = Some(1_833_000_000);
    after_grace.now = now + Duration::from_millis(2_001);
    assert_eq!(
        watchdog.observe_packet(after_grace),
        HevcDecodeChainRecoveryAction::SoftRecovery
    );
}

#[test]
fn admitted_video_progress_cancels_transient_rebuffer_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog {
        pending_fallback: Some(HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
        }),
        ..Default::default()
    };

    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 1_000_000_000,
        frame_duration_nsecs: 41_666_666,
        current_start_position_nsecs: 1_000_000_000,
        before_queue_end_nsecs: None,
        after_queue_end_nsecs: Some(1_041_666_666),
    });

    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn first_admitted_progress_preserves_high_water_before_stable_output_gap() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        zero_output_packets: 178,
        recent_zero_output_packets: 178,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(144_967_000_000),
        recent_output_high_water_nsecs: Some(144_733_333_333),
        recent_audio_timeline_gap_checked: true,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((144_067_000_000, 144_700_333_333)),
        Some(633_000_000),
    );

    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 144_700_000_000,
        frame_duration_nsecs: 33_333_333,
        current_start_position_nsecs: 69_267_000_000,
        before_queue_end_nsecs: Some(144_700_333_333),
        after_queue_end_nsecs: Some(144_733_333_333),
    });

    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.recent_zero_output_packets, 178);
    assert!(watchdog.recent_packet_lead_exceeded);

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 144_967_000_000;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(144_733_333_333);
    gap.previous_gap_nsecs = Some(233_666_667);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 144_733_333_333;
    gap.audio_played_timeline_nsecs = Some(144_040_652_981);
    gap.demux_watermark = demux_watermark(false);

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 144_733_333_333,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn software_clean_233ms_gap_bridges_despite_stale_hardware_evidence() {
    let mut watchdog = HevcDecodeChainWatchdog {
        zero_output_packets: 63,
        recent_zero_output_packets: 175,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_039_633_333_332),
        recent_output_high_water_nsecs: Some(1_036_766_666_666),
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_035_300_000_000, 1_036_766_666_666)),
        Some(1_466_666_666),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.hardware_accelerated = false;
    gap.timeline_nsecs = 1_037_000_000_000;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(1_036_766_666_666);
    gap.previous_gap_nsecs = Some(233_333_334);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 1_036_766_666_666;
    gap.demux_watermark = demux_watermark(false);

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert!(!watchdog.recent_packet_lead_exceeded);
}

#[test]
fn software_zero_output_packets_do_not_accumulate_hardware_high_water() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    let mut input = hevc_watchdog_input(
        1_100_000_000,
        snapshot,
        demux_watermark(false),
        1_000_000_000,
    );
    input.hardware_accelerated = false;

    assert_eq!(
        watchdog.observe_packet(input),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 1);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert_eq!(watchdog.recent_input_packet_high_water_nsecs, None);
    assert!(!watchdog.recent_packet_lead_exceeded);
}

#[test]
fn problem_trace_20_06_clean_idr_replays_video_only_gap() {
    let frozen_end_nsecs = 1_206_333_333_333;
    let recovery_frame_nsecs = 1_210_233_312_500;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 118,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_210_633_333_332),
        recent_output_high_water_nsecs: Some(frozen_end_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(1_210_466_645_829),
        recent_audio_timeline_gap_checked: true,
        last_decoded_video_end_nsecs: Some(frozen_end_nsecs),
        ..Default::default()
    };
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_205_366_687_500, frozen_end_nsecs)),
        Some(1_023_921_410),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
    gap.session_id = PlaybackSessionId(10);
    gap.timeline_nsecs = recovery_frame_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(frozen_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        recovery_frame_nsecs.saturating_sub(frozen_end_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = frozen_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(1_205_309_411_923);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(22_833_270_832),
        audio_forward_nsecs: Some(25_109_333_333),
        selected_min_forward_nsecs: Some(22_833_270_832),
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
            target_nsecs: frozen_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_22_33_rebuffered_seek_bridges_clean_idr_without_replay() {
    let frozen_end_nsecs = 1_352_666_645_833;
    let recovery_frame_nsecs = 1_355_233_312_500;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 78,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_355_633_333_332),
        recent_output_high_water_nsecs: Some(frozen_end_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(1_355_466_645_829),
        recent_audio_timeline_gap_checked: true,
        last_decoded_video_end_nsecs: Some(frozen_end_nsecs),
        ..Default::default()
    };
    let mut output = output_snapshot(
        PlaybackOutputState::Rebuffering,
        true,
        false,
        Some((1_351_900_000_000, frozen_end_nsecs)),
        Some(794_659_480),
    );
    // The initial A/V transaction has handed ownership to rebuffering, but
    // no frame has reached the display yet. This is the state captured by
    // the 22:33 trace and must not be mistaken for steady-state failure.
    output.first_video_frame_pending = false;
    output.first_frame_needed = false;
    output.first_frame_presented = false;
    output.initial_av_start_pending = false;
    output.output_clock_running = false;
    output.pending_start_audio_frames = 71;
    output.pending_start_audio_nsecs = 1_514_671_164;

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
    gap.session_id = PlaybackSessionId(10);
    gap.timeline_nsecs = recovery_frame_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(frozen_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(
        recovery_frame_nsecs.saturating_sub(frozen_end_nsecs),
    ));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = frozen_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(1_351_871_986_353);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(38_066_687_499),
        audio_forward_nsecs: Some(39_317_333_334),
        selected_min_forward_nsecs: Some(38_066_687_499),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(recovery_frame_nsecs - frozen_end_nsecs, 2_566_666_667);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
}

#[test]
fn decoder_output_breaks_only_consecutive_hardware_zero_output_run() {
    let mut watchdog = HevcDecodeChainWatchdog {
        zero_output_packets: 24,
        recent_zero_output_packets: 24,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_800_000_000),
        recent_output_high_water_nsecs: Some(1_000_000_000),
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    let mut input = hevc_watchdog_input(
        1_840_000_000,
        snapshot,
        demux_watermark(false),
        1_000_000_000,
    );
    input.decoded_frames = 1;

    assert_eq!(
        watchdog.observe_packet(input),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.recent_zero_output_packets, 24);
    assert_eq!(
        watchdog.recent_input_packet_high_water_nsecs,
        Some(1_800_000_000)
    );
    assert!(watchdog.recent_packet_lead_exceeded);
}

#[test]
fn problem_trace_222_zero_outputs_withholds_7_233s_recovery_idr() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let last_output_end_nsecs = 683_400_000_000_u64;
    let output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((682_400_000_000, last_output_end_nsecs)),
        Some(1_000_000_000),
    );
    let first_zero_packet_nsecs = 683_933_333_333_u64;
    for packet_index in 0..222_u64 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                first_zero_packet_nsecs.saturating_add(packet_index.saturating_mul(33_333_333)),
                output,
                demux_watermark(false),
                last_output_end_nsecs,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(watchdog.recent_zero_output_packets, 222);
    assert_eq!(
        watchdog.pending_fallback().map(|fallback| fallback.reason),
        Some(HevcDecodeChainFallbackReason::ZeroOutputRebuffer)
    );

    let recovery_idr_nsecs = 690_633_333_333;
    let mut decoder_output = hevc_watchdog_input(
        recovery_idr_nsecs,
        output,
        demux_watermark(false),
        last_output_end_nsecs,
    );
    decoder_output.decoded_frames = 1;
    assert_eq!(
        watchdog.observe_packet(decoder_output),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.recent_zero_output_packets, 222);
    assert!(watchdog.recent_input_packet_high_water_nsecs.is_some());

    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, output);
    gap.timeline_nsecs = recovery_idr_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(last_output_end_nsecs);
    gap.previous_gap_nsecs = Some(7_233_333_333);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = last_output_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(last_output_end_nsecs);
    gap.demux_watermark = demux_watermark(false);
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
            target_nsecs: last_output_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn clean_hevc_keyframe_bridges_small_decode_gap_without_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog {
        zero_output_packets: 3,
        recent_zero_output_packets: 12,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((118_816_666_667, 119_649_999_999)),
        Some(847_832_951),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 119_866_666_667;
    gap.duration_nsecs = 16_666_666;
    gap.previous_expected_next_nsecs = Some(119_649_999_999);
    gap.previous_gap_nsecs = Some(216_666_668);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 119_649_999_999;
    gap.audio_played_timeline_nsecs = Some(118_802_167_048);
    gap.demux_watermark = demux_watermark(false);
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
}

#[test]
fn clean_hevc_keyframe_bridges_rounding_edge_decode_gap_from_seek_log() {
    let mut watchdog = HevcDecodeChainWatchdog {
        zero_output_packets: 3,
        recent_zero_output_packets: 12,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Syncing,
        false,
        false,
        Some((174_066_666_667, 174_116_666_666)),
        Some(49_999_999),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 174_616_666_667;
    gap.duration_nsecs = 16_666_666;
    gap.previous_expected_next_nsecs = Some(174_116_666_666);
    gap.previous_gap_nsecs = Some(500_000_001);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 174_116_666_666;
    gap.audio_played_timeline_nsecs = Some(174_066_666_667);
    gap.demux_watermark = demux_watermark(false);
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.zero_output_packets, 0);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
}

#[test]
fn clean_hevc_keyframe_bridges_bounded_initial_gap_from_10_08_seek_log() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Syncing,
        false,
        false,
        Some((610_133_333_333, 610_166_666_666)),
        Some(33_333_333),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 613_166_666_667;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(610_166_666_666);
    gap.previous_gap_nsecs = Some(3_000_000_001);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 610_166_666_666;
    gap.audio_played_timeline_nsecs = Some(608_300_000_000);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(38_833_333_334),
        audio_forward_nsecs: Some(41_122_539_683),
        selected_min_forward_nsecs: Some(38_833_333_334),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn clean_hevc_keyframe_does_not_bridge_large_initial_gap_beyond_hold_limit() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Syncing,
        false,
        false,
        Some((174_066_666_667, 174_116_666_666)),
        Some(49_999_999),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 179_119_766_667;
    gap.duration_nsecs = 16_666_666;
    gap.previous_expected_next_nsecs = Some(174_116_666_666);
    gap.previous_gap_nsecs = Some(5_003_100_001);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 174_116_666_666;
    gap.audio_played_timeline_nsecs = Some(174_066_666_667);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(10_000_000_000),
        audio_forward_nsecs: Some(10_000_000_000),
        selected_min_forward_nsecs: Some(10_000_000_000),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::Admit
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn clean_hevc_keyframe_does_not_expand_playing_gap_policy() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((174_066_666_667, 174_116_666_666)),
        Some(49_999_999),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 175_116_666_667;
    gap.duration_nsecs = 16_666_666;
    gap.previous_expected_next_nsecs = Some(174_116_666_666);
    gap.previous_gap_nsecs = Some(1_000_000_001);
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = 174_116_666_666;
    gap.audio_played_timeline_nsecs = Some(174_066_666_667);
    gap.demux_watermark = demux_watermark(false);
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::Admit
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn strong_hevc_high_water_gap_drops_even_while_output_is_stable() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 24,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        recent_audio_timeline_gap_checked: true,
        ..Default::default()
    };
    let mut snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_000_000_000, 2_460_000_000)),
        Some(1_460_000_000),
    );
    snapshot.video_decode_underfill = true;
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        corrupt: true,
        decode_error_flags: 1,
        ..Default::default()
    };
    gap.demux_watermark = demux_watermark(false);

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback().map(|fallback| fallback.reason),
        Some(HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput)
    );
}

#[test]
fn hardware_high_water_enters_suspected_without_low_water() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let mut snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((1_000_000_000, 2_460_000_000)),
        Some(1_460_000_000),
    );
    snapshot.video_decode_underfill = true;

    for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                3_000_000_000 + packet_index * 1_000_000,
                snapshot,
                demux_watermark(false),
                2_460_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }

    assert_eq!(watchdog.take_fallback(), None);
    assert!(!watchdog.soft_recovery_attempted);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(
        watchdog.recent_zero_output_packets,
        HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT
    );
}

#[test]
fn problem_trace_6_49_auto_keeps_vulkan_before_next_safe_idr() {
    let output_end_nsecs = 408_566_645_833;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((407_500_000_000, output_end_nsecs)),
        Some(1_066_645_833),
    );
    let mut watchdog = HevcDecodeChainWatchdog::default();

    for packet_index in 0..HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT {
        let input = hevc_watchdog_input(
            408_999_979_163 + packet_index * 33_333_333,
            stable_output,
            demux_watermark(false),
            407_404_479_023,
        );
        assert_eq!(
            watchdog.observe_packet(input),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.pending_fallback(), None);
    }

    assert_eq!(watchdog.zero_output_safe_anchor_nsecs, None);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn problem_trace_3_41_stale_output_replays_at_crossed_clean_idr() {
    let continuous_output_end_nsecs = 220_500_020_833;
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 131,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(225_433_354_165),
        recent_output_high_water_nsecs: Some(continuous_output_end_nsecs),
        recent_zero_output_safe_anchor_nsecs: Some(225_233_333_329),
        recent_audio_timeline_gap_checked: true,
        last_decoded_video_end_nsecs: Some(continuous_output_end_nsecs),
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(2),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 9_989,
            frame_timeline_nsecs: 220_500_000_000,
            frame_duration_nsecs: 33_333_333,
            current_start_position_nsecs: 178_733_312_500,
            before_queue_end_nsecs: Some(continuous_output_end_nsecs),
            after_queue_end_nsecs: Some(220_533_333_333),
        }),
        HevcAdmittedVideoProgress::Partial
    );
    assert_eq!(watchdog.pending_fallback(), None);

    let previous_expected_next_nsecs = 220_833_333_333;
    let gap_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((219_566_687_500, previous_expected_next_nsecs)),
        Some(1_266_645_833),
    );
    let mut recovery_idr =
        decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, gap_output);
    recovery_idr.timeline_nsecs = 225_000_000_000;
    recovery_idr.duration_nsecs = 33_333_333;
    recovery_idr.previous_expected_next_nsecs = Some(previous_expected_next_nsecs);
    recovery_idr.previous_gap_nsecs = Some(4_166_666_667);
    recovery_idr.max_gap_nsecs = 200_000_000;
    recovery_idr.fallback_target_nsecs = previous_expected_next_nsecs;
    recovery_idr.audio_played_timeline_nsecs = Some(219_566_687_500);
    recovery_idr.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(40_733_333_334),
        audio_forward_nsecs: Some(43_370_666_666),
        selected_min_forward_nsecs: Some(40_733_333_334),
        ..Default::default()
    };
    recovery_idr.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(recovery_idr),
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
fn problem_trace_10_13_drains_reordered_prefix_then_replays_clean_idr_gap() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        zero_output_packets: 55,
        first_zero_output_packet_nsecs: Some(613_566_645_833),
        zero_output_safe_anchor_nsecs: Some(615_300_020_829),
        last_video_packet_nsecs: Some(615_366_645_833),
        last_decoded_video_end_nsecs: Some(613_100_020_833),
        recent_zero_output_packets: 55,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(615_366_645_833),
        recent_output_high_water_nsecs: Some(613_100_020_833),
        recent_zero_output_safe_anchor_nsecs: Some(615_300_020_829),
        recent_audio_timeline_gap_checked: true,
        ..Default::default()
    };
    let progress = [
        (613_100_000_000, 613_100_020_833, 613_133_333_333),
        (613_133_312_500, 613_133_333_333, 613_166_645_833),
        (613_266_687_500, 613_166_645_833, 613_300_020_833),
    ];

    for (generation, (frame_nsecs, before_nsecs, after_nsecs)) in (9_819_u64..).zip(progress) {
        assert_eq!(
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(12),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation,
                frame_timeline_nsecs: frame_nsecs,
                frame_duration_nsecs: 33_333_333,
                current_start_position_nsecs: 608_466_687_500,
                before_queue_end_nsecs: Some(before_nsecs),
                after_queue_end_nsecs: Some(after_nsecs),
            }),
            HevcAdmittedVideoProgress::Partial
        );
    }
    assert_eq!(watchdog.pending_fallback(), None);

    // Model the narrow race in which the packet watchdog queues its
    // speculative zero-output fallback just before the already-decoded IDR
    // is admitted. Once the zero-output run crossed a safe anchor, the
    // clean returning IDR confirms that the missing PTS interval was a
    // video-only decode failure, so preserve the recovery request.
    watchdog.pending_fallback = Some(HevcDecodeChainFallback {
        target_nsecs: 613_300_020_833,
        reason: HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
    });

    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((612_062_654_465, 613_300_020_833)),
        Some(1_237_366_368),
    );
    let mut clean_idr =
        decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    clean_idr.timeline_nsecs = 615_066_687_500;
    clean_idr.duration_nsecs = 33_333_333;
    clean_idr.previous_expected_next_nsecs = Some(613_300_020_833);
    clean_idr.previous_gap_nsecs = Some(1_766_666_667);
    clean_idr.max_gap_nsecs = 200_000_000;
    clean_idr.fallback_target_nsecs = 613_300_020_833;
    clean_idr.audio_played_timeline_nsecs = Some(612_062_654_465);
    clean_idr.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(43_166_687_498),
        audio_forward_nsecs: Some(43_818_666_667),
        selected_min_forward_nsecs: Some(43_166_687_498),
        ..Default::default()
    };
    clean_idr.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(
        watchdog.observe_decoded_frame_gap(clean_idr),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 613_300_020_833,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn problem_trace_11_23_rebuffer_headroom_bridges_crossed_clean_idr() {
    let output_end_nsecs = 682_666_645_833;
    let mut rebuffer_output = output_snapshot(
        PlaybackOutputState::Rebuffering,
        true,
        false,
        Some((681_700_000_000, output_end_nsecs)),
        Some(981_308_005),
    );
    rebuffer_output.first_frame_presented = false;
    rebuffer_output.pending_start_audio_frames = 71;
    rebuffer_output.pending_start_audio_nsecs = 1_514_648_488;
    let healthy_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(40_566_708_336),
        audio_forward_nsecs: Some(40_170_666_667),
        selected_min_forward_nsecs: Some(40_170_666_667),
        ..Default::default()
    };
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let safe_idr_packet_nsecs = 685_300_020_829;
    let mut packet_nsecs = 683_099_979_163;

    assert!(HevcDecodeChainWatchdog::rebuffer_has_video_headroom(
        rebuffer_output
    ));
    while packet_nsecs < safe_idr_packet_nsecs {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                packet_nsecs,
                rebuffer_output,
                healthy_demux,
                681_685_337_828,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(
            watchdog.pending_fallback(),
            None,
            "a retained 981ms video queue must not be treated as decoder failure"
        );
        packet_nsecs = packet_nsecs.saturating_add(33_333_333);
    }
    assert!(watchdog.recent_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);

    let mut safe_idr = hevc_watchdog_input(
        safe_idr_packet_nsecs,
        rebuffer_output,
        healthy_demux,
        681_685_337_828,
    );
    safe_idr.safe_seek_point = true;
    safe_idr.decoded_frames = 3;
    assert_eq!(
        watchdog.observe_packet(safe_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);

    let clean_idr_nsecs = 685_066_687_500;
    let gap_nsecs = clean_idr_nsecs - output_end_nsecs;
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, rebuffer_output);
    gap.session_id = PlaybackSessionId(5);
    gap.timeline_nsecs = clean_idr_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(output_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(gap_nsecs));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = output_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(681_685_337_828);
    gap.demux_watermark = healthy_demux;
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(gap_nsecs, 2_400_041_667);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitAndBridgeDecodeGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Healthy);
}

#[test]
fn problem_trace_8_14_keeps_vulkan_and_replays_large_clean_idr_gap() {
    let output_end_nsecs = 494_966_645_833;
    let stable_output = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        false,
        Some((493_800_000_000, output_end_nsecs)),
        Some(1_166_645_833),
    );
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let next_idr_nsecs = 500_066_687_500;
    let mut packet_nsecs = 495_433_312_499;

    while packet_nsecs < next_idr_nsecs {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                packet_nsecs,
                stable_output,
                DemuxReaderWatermark {
                    video_forward_nsecs: Some(35_900_000_002),
                    audio_forward_nsecs: Some(39_552_000_000),
                    selected_min_forward_nsecs: Some(35_900_000_002),
                    ..Default::default()
                },
                output_end_nsecs,
            )),
            HevcDecodeChainRecoveryAction::None
        );
        assert_eq!(watchdog.pending_fallback(), None);
        packet_nsecs = packet_nsecs.saturating_add(33_333_333);
    }

    let mut clean_idr = hevc_watchdog_input(
        next_idr_nsecs,
        stable_output,
        demux_watermark(false),
        output_end_nsecs,
    );
    clean_idr.safe_seek_point = true;
    clean_idr.decoded_frames = 1;
    assert_eq!(
        watchdog.observe_packet(clean_idr),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.pending_fallback(), None);

    let gap_nsecs = next_idr_nsecs - output_end_nsecs;
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, stable_output);
    gap.timeline_nsecs = next_idr_nsecs;
    gap.duration_nsecs = 33_333_333;
    gap.previous_expected_next_nsecs = Some(output_end_nsecs);
    gap.previous_gap_nsecs = Some(i128::from(gap_nsecs));
    gap.max_gap_nsecs = 200_000_000;
    gap.fallback_target_nsecs = output_end_nsecs;
    gap.audio_played_timeline_nsecs = Some(493_704_974_648);
    gap.demux_watermark = DemuxReaderWatermark {
        video_forward_nsecs: Some(35_900_000_002),
        audio_forward_nsecs: Some(39_552_000_000),
        selected_min_forward_nsecs: Some(35_900_000_002),
        ..Default::default()
    };
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        key_frame: true,
        ..Default::default()
    };

    assert_eq!(gap_nsecs, 5_100_041_667);
    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: output_end_nsecs,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
}

#[test]
fn strong_hevc_gap_evidence_requests_fallback_at_output_low_water() {
    let mut watchdog = HevcDecodeChainWatchdog {
        recent_zero_output_packets: 24,
        recent_soft_recovery_attempted: true,
        recent_packet_lead_exceeded: true,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((1_000_000_000, 1_120_000_000)),
        Some(120_000_000),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.source_frame_diagnostic = DecodedVideoFrameDiagnostic {
        corrupt: true,
        decode_error_flags: 1,
        ..Default::default()
    };
    gap.demux_watermark = demux_watermark(false);

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::DropForFallback
    );
    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 252_920_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn hevc_recent_gap_evidence_clears_only_after_500ms_caught_up_progress() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 24,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_000_000_000),
        recent_output_high_water_nsecs: Some(960_000_000),
        pending_fallback: Some(HevcDecodeChainFallback {
            target_nsecs: 1_000_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        }),
        ..Default::default()
    };
    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_000_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_000_000_000),
            after_queue_end_nsecs: Some(1_499_000_000),
        }),
        HevcAdmittedVideoProgress::Partial
    );

    assert_eq!(watchdog.recent_zero_output_packets, 24);
    assert!(watchdog.recent_packet_lead_exceeded);
    assert_eq!(watchdog.healthy_admitted_progress_nsecs, 499_000_000);
    assert!(watchdog.pending_fallback.is_some());

    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_499_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_499_000_000),
            after_queue_end_nsecs: Some(1_500_000_000),
        }),
        HevcAdmittedVideoProgress::Stable
    );
    watchdog.clear_recent_gap_evidence();
    watchdog.pending_fallback = None;

    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert!(!watchdog.recent_packet_lead_exceeded);
    assert_eq!(watchdog.healthy_admitted_progress_nsecs, 0);
    assert_eq!(watchdog.pending_fallback, None);
}

#[test]
fn non_contiguous_admitted_frame_resets_healthy_recovery_window() {
    let mut watchdog = HevcDecodeChainWatchdog {
        health_state: HevcDecodeHealthState::Suspected,
        recent_zero_output_packets: 24,
        recent_packet_lead_exceeded: true,
        recent_input_packet_high_water_nsecs: Some(1_000_000_000),
        recent_output_high_water_nsecs: Some(960_000_000),
        ..Default::default()
    };
    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_000_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_000_000_000),
            after_queue_end_nsecs: Some(1_300_000_000),
        }),
        HevcAdmittedVideoProgress::Partial
    );
    assert_eq!(watchdog.healthy_admitted_progress_nsecs, 300_000_000);

    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_800_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_300_000_000),
            after_queue_end_nsecs: Some(1_840_000_000),
        }),
        HevcAdmittedVideoProgress::Partial
    );
    assert_eq!(watchdog.healthy_admitted_progress_nsecs, 0);

    assert_eq!(
        watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
            session_id: PlaybackSessionId(1),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
            generation: 1,
            frame_timeline_nsecs: 1_840_000_000,
            frame_duration_nsecs: 40_000_000,
            current_start_position_nsecs: 1_000_000_000,
            before_queue_end_nsecs: Some(1_840_000_000),
            after_queue_end_nsecs: Some(2_339_000_000),
        }),
        HevcAdmittedVideoProgress::Partial
    );
    assert_eq!(watchdog.healthy_admitted_progress_nsecs, 499_000_000);
    assert_eq!(watchdog.recent_zero_output_packets, 24);
}

#[test]
fn dropped_before_start_does_not_count_as_admitted_progress() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 900_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 1_000_000_000,
        before_queue_end_nsecs: Some(1_000_000_000),
        after_queue_end_nsecs: Some(1_040_000_000),
    });
    assert!(watchdog.last_video_progress_at.is_none());
}

#[test]
fn hevc_zero_output_watchdog_ignores_dropped_before_start_progress() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            1_100_000_000,
            snapshot,
            demux_watermark(false),
            1_250_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );

    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 1_050_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 1_100_000_000,
        before_queue_end_nsecs: Some(1_100_000_000),
        after_queue_end_nsecs: Some(1_100_000_000),
    });

    assert_eq!(watchdog.zero_output_packets, 1);
}

#[test]
fn hevc_zero_output_watchdog_resets_after_seek_preroll_progress() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((900_000_000, 1_000_000_000)),
        Some(100_000_000),
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            1_100_000_000,
            snapshot,
            demux_watermark(false),
            1_250_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(watchdog.zero_output_packets, 1);

    watchdog.observe_seek_preroll_progress(HevcSeekPrerollProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        frame_timeline_nsecs: 1_050_000_000,
        target_nsecs: 1_250_000_000,
        preroll_frames: 1,
    });

    assert_eq!(watchdog.zero_output_packets, 0);
    assert!(!watchdog.soft_recovery_attempted);
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn hevc_zero_output_pts_gap_fallback_survives_first_admitted_video_progress() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((252_760_000_000, 252_920_000_000)),
        Some(40_000_000),
    );
    for packet_index in 0..23_u64 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_000_000_000 + packet_index * 1_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            253_500_000_000,
            snapshot,
            demux_watermark(false),
            252_900_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );

    assert_eq!(
        watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            snapshot,
        )),
        HevcDecodedFrameGapAction::DropForFallback
    );
    watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        generation: 1,
        frame_timeline_nsecs: 257_720_000_000,
        frame_duration_nsecs: 40_000_000,
        current_start_position_nsecs: 252_900_000_000,
        before_queue_end_nsecs: Some(252_920_000_000),
        after_queue_end_nsecs: Some(257_760_000_000),
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 252_920_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn hevc_high_water_pts_gap_evidence_survives_preroll_below_input_high_water() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((252_760_000_000, 252_920_000_000)),
        Some(40_000_000),
    );
    for packet_index in 0..23_u64 {
        assert_eq!(
            watchdog.observe_packet(hevc_watchdog_input(
                253_000_000_000 + packet_index * 1_000_000,
                snapshot,
                demux_watermark(false),
                252_900_000_000,
            )),
            HevcDecodeChainRecoveryAction::None
        );
    }
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            253_500_000_000,
            snapshot,
            demux_watermark(false),
            252_900_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );
    assert_eq!(
        watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            snapshot,
        )),
        HevcDecodedFrameGapAction::DropForFallback
    );
    watchdog.observe_seek_preroll_progress(HevcSeekPrerollProgressObservation {
        session_id: PlaybackSessionId(1),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
        frame_timeline_nsecs: 252_880_000_000,
        target_nsecs: 252_920_000_000,
        preroll_frames: 1,
    });

    assert_eq!(
        watchdog.take_fallback(),
        Some(HevcDecodeChainFallback {
            target_nsecs: 252_920_000_000,
            reason: HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        })
    );
}

#[test]
fn hevc_large_pts_gap_without_decode_chain_evidence_does_not_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((252_760_000_000, 252_920_000_000)),
        Some(40_000_000),
    );

    assert_eq!(
        watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            snapshot,
        )),
        HevcDecodedFrameGapAction::Admit
    );

    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn synchronized_audio_video_timeline_gap_is_admitted_without_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog {
        recent_zero_output_packets: HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT,
        recent_packet_lead_exceeded: true,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((1_252_668_000_000, 1_254_127_708_333)),
        Some(1_459_708_333),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 1_254_962_000_000;
    gap.duration_nsecs = 41_708_333;
    gap.previous_expected_next_nsecs = Some(1_254_127_708_333);
    gap.previous_gap_nsecs = Some(834_291_667);
    gap.max_gap_nsecs = 200_000_000;
    gap.audio_timeline_gap = Some(AudioTimelineGapEvidence {
        previous_end_nsecs: 1_254_112_004_496,
        next_start_nsecs: 1_254_944_004_496,
    });

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::AdmitSynchronizedTimelineGap
    );
    assert_eq!(watchdog.take_fallback(), None);
    assert_eq!(watchdog.recent_zero_output_packets, 0);
    assert!(!watchdog.recent_packet_lead_exceeded);
}

#[test]
fn video_only_pts_gap_with_weak_recent_evidence_does_not_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog {
        recent_zero_output_packets: 19,
        recent_packet_lead_exceeded: true,
        ..Default::default()
    };
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((1_252_668_000_000, 1_254_127_708_333)),
        Some(1_459_708_333),
    );
    let mut gap = decoded_frame_gap_observation(ffi::AVCodecID::AV_CODEC_ID_HEVC, snapshot);
    gap.timeline_nsecs = 1_254_962_000_000;
    gap.previous_expected_next_nsecs = Some(1_254_127_708_333);
    gap.previous_gap_nsecs = Some(834_291_667);
    gap.max_gap_nsecs = 200_000_000;

    assert_eq!(
        watchdog.observe_decoded_frame_gap(gap),
        HevcDecodedFrameGapAction::Admit
    );
    assert_eq!(watchdog.take_fallback(), None);
}

#[test]
fn non_hevc_large_pts_gap_does_not_trigger_hevc_fallback() {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let snapshot = output_snapshot(
        PlaybackOutputState::Playing,
        false,
        true,
        Some((252_760_000_000, 252_920_000_000)),
        Some(40_000_000),
    );
    assert_eq!(
        watchdog.observe_packet(hevc_watchdog_input(
            253_500_000_000,
            snapshot,
            demux_watermark(false),
            252_900_000_000,
        )),
        HevcDecodeChainRecoveryAction::None
    );

    assert_eq!(
        watchdog.observe_decoded_frame_gap(decoded_frame_gap_observation(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            snapshot,
        )),
        HevcDecodedFrameGapAction::Admit
    );

    assert_eq!(watchdog.take_fallback(), None);
}
