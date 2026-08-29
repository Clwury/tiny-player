#[path = "tests/decode_gaps.rs"]
mod decode_gaps;
#[path = "tests/dovi.rs"]
mod dovi;
#[path = "tests/exact_seek.rs"]
mod exact_seek;
#[path = "tests/recovery_policy.rs"]
mod recovery_policy;
#[path = "tests/replay_journal.rs"]
mod replay_journal;
#[path = "tests/same_hardware.rs"]
mod same_hardware;
#[path = "tests/watchdog_startup.rs"]
mod watchdog_startup;

use ffmpeg_sys_next as ffi;
use std::time::{Duration, Instant};

use crate::player::render_host::{PlaybackSessionId, RenderSize};

use super::super::{
    AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, DemuxReaderWatermark,
    HardwareDecodeMode, PlaybackOutputSnapshot, PlaybackOutputState, StreamInfo,
    VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES, VideoFrameConvertContext, VideoRecoveryPointKind,
    decoded_video_frame_start_action, packet_is_video_recovery_point, packet_is_video_seek_point,
};
use super::{
    AudioTimelineGapEvidence, DecodedVideoFrameDiagnostic, DoviFrameMetadata, DoviRpuNalInspection,
    EXACT_SEEK_FRAME_DROP_TOLERANCE_NSECS, HEVC_DECODE_CHAIN_SAFE_ANCHOR_GRACE_PACKETS,
    HEVC_DECODE_CHAIN_ZERO_OUTPUT_HARD_PACKET_LIMIT,
    HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT, HEVC_DECODE_PACKET_DIAGNOSTIC_WINDOW_CAPACITY,
    HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS, HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
    HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS, HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
    HEVC_HW_REPLAY_REORDER_TAIL_PACKETS, HEVC_POST_FALLBACK_REBUFFER_RECOVERY_AFTER,
    HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS, HEVC_SAME_HARDWARE_CACHED_REBUILD_PROGRESS_TIMEOUT,
    HEVC_SAME_HARDWARE_DRAIN_GRACE, HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS,
    HEVC_SAME_HARDWARE_RECOVERY_MAX_WALL_TIME, HEVC_SAME_HARDWARE_REPLAY_PROGRESS_TIMEOUT,
    HEVC_STARTUP_IN_FLIGHT_HARD_AFTER, HEVC_STARTUP_WATCHDOG_REJECTION_LOG_INTERVAL,
    HEVC_STARTUP_WATCHDOG_RETRY_AFTER, HEVC_STARTUP_ZERO_OUTPUT_HARD_AFTER,
    HEVC_STARTUP_ZERO_OUTPUT_HARD_PACKET_LIMIT, HevcAdmittedVideoProgress,
    HevcAdmittedVideoProgressObservation, HevcDecodeChainFallback,
    HevcDecodeChainFallbackLoopAction, HevcDecodeChainFallbackReason,
    HevcDecodeChainFallbackRecord, HevcDecodeChainRecoveryAction, HevcDecodeChainResetScope,
    HevcDecodeChainWatchdog, HevcDecodeChainWatchdogInput, HevcDecodeHealthState,
    HevcDecodePacketDiagnosticWindow, HevcDecodePacketEvidenceScope, HevcDecodeRecoveryAction,
    HevcDecodedFrameGapAction, HevcDecodedFrameGapObservation, HevcHwReplayJournal,
    HevcLowLevelSeekLanding, HevcPostFallbackRebufferObservation,
    HevcPostSoftRecoverySkippedPacketObservation, HevcSameHardwareRecoveryAttempt,
    HevcSameHardwareRecoveryAttemptKind, HevcSameHardwareRecoveryPhase,
    HevcSameHardwareRecoveryTransaction, HevcSeekPrerollProgressObservation,
    HevcStartupStallObservation, HevcStreamFormat, PendingVideoDecodePacket, PlaybackBlockReason,
    PlaybackGeneration, StrippedHevcDoviDecodeAction, VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
    VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VideoDecodePacketQueues, VideoDecodePacketStatus,
    VideoDecodePipeline, VideoDecodeRecovery, VideoDecodeRecoveryScope, VideoDecodeWorkerInfo,
    VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, hevc_cra_low_level_landing_repeats,
    hevc_decode_chain_fallback_loop_action, hevc_decode_chain_fallback_record_after,
    hevc_decode_chain_recovery_record_after_reset, hevc_decode_packet_evidence_scope,
    hevc_decoder_drain_work_pending, hevc_dovi_decode_action_for_inspection,
    hevc_drain_video_result_progressed, hevc_hw_replay_packets,
    hevc_low_level_seek_would_repeat_cra, hevc_safe_anchor_can_roll_past_preserved_evidence,
    hevc_same_hardware_reopen_mode, hevc_startup_in_flight_packet_should_arm,
    hevc_startup_zero_output_timeout, hevc_zero_output_log_milestone,
    requeue_backpressured_video_decode_input, runtime_hevc_software_fallback_allowed,
    take_next_video_decode_input, video_decode_error_requires_hevc_resource_pressure_recovery,
    video_decode_pending_input_snapshot,
};

fn snapshot(
    state: VideoDecodeWorkerState,
    pending_input_packets: usize,
    submitted_not_consumed_packets: usize,
) -> VideoDecodeWorkerSnapshot {
    VideoDecodeWorkerSnapshot {
        state,
        queued_frames: 0,
        queue_capacity: VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES,
        pending_input_packets,
        pending_input_capacity: VIDEO_DECODE_PENDING_INPUT_QUEUE_CAPACITY,
        submitted_not_consumed_packets,
        command_queue_capacity: 4,
        completed_packets: 0,
        ..VideoDecodeWorkerSnapshot::default()
    }
}

fn worker_info(hardware_accelerated: bool) -> VideoDecodeWorkerInfo {
    let size = RenderSize {
        width: 2,
        height: 1,
    };
    VideoDecodeWorkerInfo {
        stream_index: 0,
        time_base: ffi::AVRational { num: 1, den: 1 },
        size: Some(size),
        decoder_name: "test".to_string(),
        hardware_accelerated,
        vulkan_device: None,
        convert_context: VideoFrameConvertContext::new_for_test(size),
    }
}

fn packet_from_data(data: &[u8]) -> crate::player::backend::ffmpeg::AvPacket {
    let props = crate::player::backend::ffmpeg::AvPacket::new().expect("packet props allocate");
    crate::player::backend::ffmpeg::AvPacket::from_data_and_props(data, &props)
        .expect("packet data allocates")
}

fn output_snapshot(
    state: PlaybackOutputState,
    rebuffering: bool,
    video_output_low_water: bool,
    queued_video_range_nsecs: Option<(u64, u64)>,
    queued_video_forward_nsecs: Option<u64>,
) -> PlaybackOutputSnapshot {
    PlaybackOutputSnapshot {
        state,
        first_video_frame_pending: state.first_video_frame_pending(),
        first_frame_needed: state.first_video_frame_pending(),
        first_frame_presented: !state.first_video_frame_pending(),
        initial_av_start_pending: state.first_video_frame_pending(),
        output_clock_running: state == PlaybackOutputState::Playing,
        audio_start_target_nsecs: None,
        output_transition_deadline_ms: None,
        rebuffering,
        queued_video_frames: usize::from(queued_video_range_nsecs.is_some()),
        recovery_staging_frames: 0,
        recovery_staging_frame_budget: None,
        committed_output_high_water_nsecs: queued_video_range_nsecs.map(|(_, end)| end),
        recovery_staged_high_water_nsecs: None,
        decode_recovery_audio_ready_latched: false,
        queued_video_coverage_nsecs: queued_video_range_nsecs
            .map(|(start, end)| end.saturating_sub(start))
            .unwrap_or_default(),
        queued_video_duration_nsecs: queued_video_range_nsecs
            .map(|(start, end)| end.saturating_sub(start))
            .unwrap_or_default(),
        queued_video_range_span_nsecs: queued_video_range_nsecs
            .map(|(start, end)| end.saturating_sub(start))
            .unwrap_or_default(),
        queued_video_range_nsecs,
        queued_video_forward_nsecs,
        queued_video_contiguous_forward_nsecs: queued_video_forward_nsecs,
        queued_video_largest_gap_nsecs: None,
        video_output_low_water,
        pending_start_audio_frames: 0,
        pending_start_audio_nsecs: 0,
        video_output_rebuffer_anchor: None,
        video_bootstrap_after_seek: false,
        video_decode_underfill: false,
        rebuffer_empty_audio_output_blocked: false,
        scheduler_dropped_video_frames: 0,
        recent_coordinator_stall_nsecs: None,
        recent_coordinator_stall_age_nsecs: None,
    }
}

fn demux_watermark(video_underrun: bool) -> DemuxReaderWatermark {
    DemuxReaderWatermark {
        video_forward_nsecs: Some(2_000_000_000),
        audio_forward_nsecs: Some(2_000_000_000),
        selected_min_forward_nsecs: Some(2_000_000_000),
        video_underrun,
        underrun: video_underrun,
        ..Default::default()
    }
}

fn hevc_watchdog_input(
    packet_nsecs: u64,
    output_snapshot: PlaybackOutputSnapshot,
    demux_watermark: DemuxReaderWatermark,
    fallback_target_nsecs: u64,
) -> HevcDecodeChainWatchdogInput {
    HevcDecodeChainWatchdogInput {
        session_id: PlaybackSessionId(1),
        packet_nsecs: Some(packet_nsecs),
        safe_seek_point: false,
        decoded_frames: 0,
        decode_ok: true,
        hardware_accelerated: true,
        output_snapshot,
        demux_watermark,
        has_audio_output: true,
        synchronized_audio_timeline_gap_checked: true,
        synchronized_audio_timeline_gap: None,
        cache_sequence_contiguous: true,
        fallback_target_nsecs,
        now: Instant::now(),
    }
}

fn decoded_frame_gap_observation(
    codec_id: ffi::AVCodecID,
    output_snapshot: PlaybackOutputSnapshot,
) -> HevcDecodedFrameGapObservation {
    HevcDecodedFrameGapObservation {
        session_id: PlaybackSessionId(1),
        codec_id,
        hardware_accelerated: true,
        timeline_nsecs: 257_720_000_000,
        duration_nsecs: 40_000_000,
        previous_expected_next_nsecs: Some(252_920_000_000),
        previous_gap_nsecs: Some(4_800_000_000),
        max_gap_nsecs: 200_000_000,
        fallback_target_nsecs: 252_900_000_000,
        audio_played_timeline_nsecs: Some(252_900_000_000),
        audio_timeline_gap: None,
        recovery_waiting: false,
        output_snapshot,
        demux_watermark: DemuxReaderWatermark::default(),
        source_frame_diagnostic: DecodedVideoFrameDiagnostic::default(),
        recent_cache_read_anomaly: false,
        decode_recovery_active: false,
    }
}

fn hevc_packet(nal_header: u8, id: u8, pts_millis: i64, key: bool) -> AvPacket {
    let mut packet = packet_from_data(&[0, 0, 0, 3, nal_header, 0x01, id]);
    unsafe {
        (*packet.as_mut_ptr()).pts = pts_millis;
        (*packet.as_mut_ptr()).dts = pts_millis;
        if key {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }
    }
    packet
}

fn assert_sparse_output_progress_does_not_erase_high_water_failure(
    packet_count: u64,
    packet_span_nsecs: u64,
) {
    let mut watchdog = HevcDecodeChainWatchdog::default();
    let base_nsecs = 100_000_000_000_u64;
    let mut output_end_nsecs = base_nsecs;
    for packet_index in 0..packet_count {
        let packet_offset_nsecs = if packet_count > 1 {
            packet_span_nsecs.saturating_mul(packet_index) / (packet_count - 1)
        } else {
            0
        };
        let output = output_snapshot(
            PlaybackOutputState::Playing,
            false,
            false,
            Some((base_nsecs.saturating_sub(40_000_000), output_end_nsecs)),
            Some(
                output_end_nsecs
                    .saturating_sub(base_nsecs)
                    .saturating_add(40_000_000),
            ),
        );
        let _ = watchdog.observe_packet(hevc_watchdog_input(
            base_nsecs.saturating_add(packet_offset_nsecs),
            output,
            demux_watermark(false),
            output_end_nsecs,
        ));

        // A lone admitted frame breaks only the consecutive run. It must not
        // erase the high-water evidence without 500ms of caught-up output.
        if (packet_index + 1).is_multiple_of(17) {
            let before = output_end_nsecs;
            output_end_nsecs = output_end_nsecs.saturating_add(40_000_000);
            watchdog.observe_admitted_video_progress(HevcAdmittedVideoProgressObservation {
                session_id: PlaybackSessionId(1),
                codec_id: ffi::AVCodecID::AV_CODEC_ID_HEVC,
                generation: 1,
                frame_timeline_nsecs: before,
                frame_duration_nsecs: 40_000_000,
                current_start_position_nsecs: base_nsecs,
                before_queue_end_nsecs: Some(before),
                after_queue_end_nsecs: Some(output_end_nsecs),
            });
        }
    }

    assert!(watchdog.recent_zero_output_packets >= HEVC_DECODE_CHAIN_ZERO_OUTPUT_SOFT_PACKET_LIMIT);
    assert_eq!(watchdog.health_state, HevcDecodeHealthState::Suspected);
    assert_eq!(
        watchdog.take_fallback(),
        None,
        "stable output keeps the evidence but waits for a safe boundary or the bounded lead"
    );
}

fn dovi_inspection(
    kept_nal_count: usize,
    metadata: Option<DoviFrameMetadata>,
) -> DoviRpuNalInspection {
    DoviRpuNalInspection {
        metadata,
        stream_format: HevcStreamFormat::ByteStream,
        nal_count: kept_nal_count.saturating_add(1),
        kept_nal_count,
        stripped_nal_count: 1,
        stripped_bytes: 32,
    }
}

fn dovi_metadata() -> DoviFrameMetadata {
    DoviFrameMetadata {
        profile: 5,
        profile5: true,
        rpu_nalu: vec![0x7c, 0x01],
        rpu_payload: vec![0xaa],
    }
}
