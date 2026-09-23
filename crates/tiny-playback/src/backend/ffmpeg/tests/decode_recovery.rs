use super::*;

#[test]
fn video_decode_recovery_waits_for_keyframe_after_error() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut delta_packet = AvPacket::new().expect("packet allocates");
    let mut key_packet = AvPacket::new().expect("packet allocates");
    unsafe {
        (*delta_packet.as_mut_ptr()).flags = 0;
        (*key_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    assert!(!recovery.waiting_for_keyframe());
    assert!(!recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));

    recovery.begin_with_realign(true);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert_eq!(recovery.record_skipped_packet(None), 1);
    assert_eq!(recovery.record_skipped_packet(None), 2);
    assert!(!recovery.should_skip_packet(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(recovery.accept_recovery_point(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(recovery.take_realign_on_next_frame());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_can_resume_without_realign_after_live_error() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut key_packet = AvPacket::new().expect("packet allocates");
    unsafe {
        (*key_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.begin_with_realign(false);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.accept_recovery_point(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_waits_for_hevc_seek_recovery_point_after_seek() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    unsafe {
        (*key_idr_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 0);
    assert!(!recovery.waiting_for_keyframe());

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_MPEG4, 1_000_000_000);
    assert!(!recovery.waiting_for_keyframe());

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 1_000_000_000);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.accept_recovery_point(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_hevc_requires_safe_key_recovery_point() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut non_key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    let mut key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    unsafe {
        (*non_key_idr_packet.as_mut_ptr()).flags = 0;
        (*key_idr_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.begin_with_realign(true);
    assert!(recovery.should_skip_packet(&non_key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_recovery_point(&non_key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.should_skip_packet(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.accept_recovery_point(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
}

#[test]
fn video_decode_recovery_cached_hevc_transaction_accepts_cra_only_for_that_seek() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut cra_packet = test_packet_from_data(&[0, 0, 0, 3, 0x2a, 0x01, 0xaa]);
    unsafe {
        (*cra_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 3_500_000_000);
    assert!(recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_recovery_point(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));

    recovery.enable_hevc_cached_recovery_point(7, 3_500_000_000);
    assert!(recovery.requires_exact_seek_output());
    assert!(!recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.accept_recovery_point(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.waiting_for_keyframe());
    assert!(recovery.requires_exact_seek_output());

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 4_500_000_000);
    assert!(recovery.should_skip_packet(&cra_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
}

#[test]
fn cra_cached_seek_exact_output_gate_drops_every_frame_before_target() {
    let mut recovery = VideoDecodeRecovery::default();
    let target_nsecs = 3_500_000_000;
    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);
    recovery.enable_hevc_cached_recovery_point(7, target_nsecs);

    assert_eq!(
        decoded_video_frame_start_action(
            target_nsecs - 1,
            target_nsecs,
            false,
            recovery.requires_exact_seek_output(),
        ),
        DecodedVideoFrameStartAction::DropBeforeStart
    );
    assert!(
        recovery
            .observe_seek_preroll_frame(target_nsecs - 1)
            .is_some()
    );
    assert_eq!(
        decoded_video_frame_start_action(
            target_nsecs,
            target_nsecs,
            false,
            recovery.requires_exact_seek_output(),
        ),
        DecodedVideoFrameStartAction::Use { realign: false }
    );
    recovery
        .finish_seek_bootstrap_after_target_frame(target_nsecs)
        .expect("target frame closes exact CRA output gate");
    assert!(!recovery.requires_exact_seek_output());
}

#[test]
fn cached_seek_from_zero_keeps_video_and_audio_on_the_original_timeline() {
    let target_nsecs = 54_585_421_272;
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut video_clock = TimestampMapper::new(Some(0), target_nsecs, Some(20_000_000));
    let mut audio_clock = TimestampMapper::new(Some(0), target_nsecs, None);
    let mut recovery = VideoDecodeRecovery::default();
    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, target_nsecs);
    recovery.enable_hevc_cached_recovery_point(3, target_nsecs);

    // The logged cached seek replays the IDR at zero, with 50 fps video and
    // 32 ms AC-3 frames. No replay frame may be relabeled as the seek target.
    let mut preroll_frames = 0;
    let mut first_video_nsecs = None;
    for raw_timestamp in (0..=54_600).step_by(20) {
        let mapped = video_clock.map(raw_timestamp, time_base);
        let action = decoded_video_frame_start_action(
            mapped.timeline_nsecs,
            target_nsecs,
            false,
            recovery.requires_exact_seek_output(),
        );
        if action == DecodedVideoFrameStartAction::DropBeforeStart {
            recovery
                .observe_seek_preroll_frame(mapped.timeline_nsecs)
                .expect("pre-target frame belongs to the seek transaction");
            preroll_frames += 1;
        } else {
            first_video_nsecs = Some(mapped.timeline_nsecs);
            recovery
                .finish_seek_bootstrap_after_target_frame(mapped.timeline_nsecs)
                .expect("target frame completes the seek");
            break;
        }
    }
    assert_eq!(preroll_frames, 2730);
    assert_eq!(first_video_nsecs, Some(54_600_000_000));

    let mut first_audio_nsecs = None;
    for raw_timestamp in (0..=54_592).step_by(32) {
        let mapped = audio_clock.map_contiguous(
            raw_timestamp,
            time_base,
            32_000_000,
            PENDING_AUDIO_CONTINUITY_TOLERANCE,
        );
        if mapped.timeline_nsecs >= target_nsecs {
            first_audio_nsecs = Some(mapped.timeline_nsecs);
            break;
        }
    }
    assert_eq!(first_audio_nsecs, Some(54_592_000_000));
    let completion = recovery
        .take_exact_seek_completion()
        .expect("seek records the actual first eligible video frame");
    assert_eq!(completion.first_eligible_frame_nsecs, 54_600_000_000);
    assert_eq!(completion.first_eligible_delta_nsecs, 14_578_728);
}

#[test]
fn video_decode_recovery_has_bounded_wait_for_recovery_point() {
    let mut recovery = VideoDecodeRecovery::default();
    let delta_packet = AvPacket::new().expect("packet allocates");

    recovery.begin_with_realign(true);
    for _ in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
        assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
        recovery.record_skipped_packet(None);
    }

    assert!(!recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(recovery.accept_after_wait_limit(ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_hevc_does_not_resume_after_wait_limit() {
    let mut recovery = VideoDecodeRecovery::default();
    let delta_packet = AvPacket::new().expect("packet allocates");

    recovery.begin_with_realign(true);
    for _ in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
        assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
        recovery.record_skipped_packet(None);
    }

    assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_after_wait_limit(ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.waiting_for_keyframe());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn recovered_video_frame_realigns_before_start_gate() {
    assert_eq!(
        decoded_video_frame_start_action(9_000_000_000, 10_000_000_000, false, false),
        DecodedVideoFrameStartAction::DropBeforeStart
    );
    assert_eq!(
        decoded_video_frame_start_action(9_996_000_000, 10_000_000_000, false, false),
        DecodedVideoFrameStartAction::Use { realign: false }
    );
    assert_eq!(
        decoded_video_frame_start_action(9_000_000_000, 10_000_000_000, true, false),
        DecodedVideoFrameStartAction::Use { realign: true }
    );
    assert_eq!(
        decoded_video_frame_start_action(11_000_000_000, 10_000_000_000, true, false),
        DecodedVideoFrameStartAction::Use { realign: true }
    );
    assert_eq!(
        decoded_video_frame_start_action(9_996_000_000, 10_000_000_000, false, true),
        DecodedVideoFrameStartAction::DropBeforeStart,
        "CRA cached seek must not expose a frame before its exact target"
    );
    assert_eq!(
        decoded_video_frame_start_action(10_000_000_000, 10_000_000_000, false, true),
        DecodedVideoFrameStartAction::Use { realign: false }
    );
}

#[test]
fn video_decode_error_recovery_classifies_decoder_errors() {
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 接收解码帧失败：Invalid data found when processing input"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 发送解码包失败：Invalid data found when processing input"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 发送解码包失败：Cannot allocate memory"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 接收解码帧失败：VK_ERROR_OUT_OF_DEVICE_MEMORY"
    ));
    assert!(!video_decode_error_is_recoverable(
        "FFmpeg 创建视频色彩转换器失败"
    ));
}

#[test]
fn pending_start_audio_buffers_decoded_audio_until_first_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 6],
            duration_nsecs: 30_000_000,
        },
        1_020_000_000,
        1_050_000_000,
    );

    assert_eq!(pending.len(), 2);
    assert_eq!(pending.queued_samples(), 10);
    assert_eq!(pending.buffered_duration(), Duration::from_millis(50));
}
