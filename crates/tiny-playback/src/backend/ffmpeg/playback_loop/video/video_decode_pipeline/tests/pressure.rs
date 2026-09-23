use super::*;
use crate::backend::ffmpeg::TimestampMapper;
use crate::backend::ffmpeg::playback_loop::video_decode_pipeline::VideoPacketAdmissionContext;
use std::collections::VecDeque;

fn pressure_context(
    played_until_nsecs: u64,
    video_clock: &TimestampMapper,
) -> VideoPacketAdmissionContext<'_> {
    VideoPacketAdmissionContext {
        session_id: PlaybackSessionId(1),
        video_stream: StreamInfo {
            index: 0,
            stream: std::ptr::null_mut(),
            decoder: std::ptr::null(),
            codec_id: ffi::AVCodecID::AV_CODEC_ID_H264,
            time_base: ffi::AVRational { num: 1, den: 1_000 },
            start_nsecs: Some(0),
            frame_duration_nsecs: Some(41_666_666),
        },
        output_snapshot: output_snapshot(
            PlaybackOutputState::Playing,
            false,
            true,
            Some((played_until_nsecs, played_until_nsecs + 247_333_333)),
            Some(247_333_333),
        ),
        demux_watermark: demux_watermark(false),
        has_audio_output: true,
        skip_nonref_for_pressure: true,
        played_until_nsecs: Some(played_until_nsecs),
        video_clock,
        presentation: None,
    }
}

fn video_packet(pts: Option<i64>, dts: i64, duration: i64) -> AvPacket {
    let mut packet = packet_from_data(&[0, 0, 1, 0x01]);
    unsafe {
        (*packet.as_mut_ptr()).pts = pts.unwrap_or(ffi::AV_NOPTS_VALUE);
        (*packet.as_mut_ptr()).dts = dts;
        (*packet.as_mut_ptr()).duration = duration;
    }
    packet
}

#[test]
fn pressure_skipping_compares_normalized_video_and_audio_timelines() {
    let clock = TimestampMapper::new(Some(10_000_000_000), 0, None);
    let context = pressure_context(1_000_000_000, &clock);
    assert!(context.should_skip_nonref_for_pressure(&video_packet(Some(10_500), 10_420, 40)));
    assert!(!context.should_skip_nonref_for_pressure(&video_packet(Some(11_100), 11_020, 40)));
}

#[test]
fn pressure_skipping_preserves_future_packets_rebased_after_seek_without_stream_start() {
    let mut clock = TimestampMapper::new(None, 20_000_000_000, None);
    let packet = video_packet(Some(2_000), 1_920, 40);
    assert!(!pressure_context(20_100_000_000, &clock).should_skip_nonref_for_pressure(&packet));
    clock.map(1_000, ffi::AVRational { num: 1, den: 1_000 });
    assert!(!pressure_context(20_100_000_000, &clock).should_skip_nonref_for_pressure(&packet));
}

#[test]
fn exact_seek_packet_policy_rejects_dts_fallback_and_latches_full_decode() {
    let clock = TimestampMapper::new(Some(10_000_000_000), 0, None);
    let context = pressure_context(1_000_000_000, &clock);
    let mut recovery = VideoDecodeRecovery {
        recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 1,
            target_nsecs: 1_000_000_000,
        },
        ..Default::default()
    };
    let before = video_packet(Some(10_500), 10_420, 40);
    assert!(recovery.should_skip_nonref_for_seek_preroll(
        context.packet_timeline_nsecs(&before),
        false,
        false
    ));
    let no_pts = video_packet(None, 10_600, 40);
    assert!(!recovery.should_skip_nonref_for_seek_preroll(
        context.packet_timeline_nsecs(&no_pts),
        false,
        false
    ));
    assert!(!recovery.should_skip_nonref_for_seek_preroll(
        context.packet_timeline_nsecs(&before),
        false,
        false
    ));
}

#[test]
fn exact_seek_packet_policy_stops_at_the_normalized_target() {
    let clock = TimestampMapper::new(Some(10_000_000_000), 0, None);
    let context = pressure_context(1_000_000_000, &clock);
    let mut recovery = VideoDecodeRecovery {
        recovery_scope: VideoDecodeRecoveryScope::ExactCachedSeek {
            transaction_id: 1,
            target_nsecs: 1_000_000_000,
        },
        ..Default::default()
    };
    for (pts, expected) in [(10_500, true), (10_995, false), (10_600, false)] {
        let packet = video_packet(Some(pts), pts - 80, 40);
        assert_eq!(
            recovery.should_skip_nonref_for_seek_preroll(
                context.packet_timeline_nsecs(&packet),
                false,
                false
            ),
            expected
        );
    }
}

#[test]
fn pressure_skipping_preserves_forward_frames_while_filling_after_cached_seek() {
    // The 1:05:06 stutter started with five queued frames after a seek to
    // 3904.125s. Low water enabled NONREF while input was already ahead of audio.
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let mut context = pressure_context(3_904_127_333_309, &video_clock);
    context.output_snapshot.queued_video_frames = 5;
    for pts in [3_904_167, 3_904_625, 3_904_792, 3_906_125, 3_906_792] {
        let packet = video_packet(Some(pts), pts - 84, 42);
        assert!(
            !context.should_skip_nonref_for_pressure(&packet),
            "PTS {pts}"
        );
    }
}

#[test]
fn pressure_skipping_requires_a_frame_to_be_late_beyond_its_duration_and_tolerance() {
    let packet = video_packet(Some(1_000), 920, 40);
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let mut context = pressure_context(1_114_999_999, &video_clock);
    assert!(!context.should_skip_nonref_for_pressure(&packet));
    context.played_until_nsecs = Some(1_115_000_000);
    assert!(context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_uses_pts_instead_of_an_earlier_dts() {
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let context = pressure_context(1_200_000_000, &video_clock);
    let packet = video_packet(Some(1_240), 1_000, 40);
    assert!(!context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_preserves_packets_without_presentation_timestamps() {
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let context = pressure_context(2_000_000_000, &video_clock);
    let packet = video_packet(None, 1_000, 40);
    assert!(!context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_rechecks_lateness_for_each_reordered_packet() {
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let context = pressure_context(1_100_000_000, &video_clock);
    for (pts, should_skip) in [(800, true), (1_100, false), (920, true), (1_140, false)] {
        let packet = video_packet(Some(pts), pts - 80, 40);
        assert_eq!(
            context.should_skip_nonref_for_pressure(&packet),
            should_skip
        );
    }
}

#[test]
fn pressure_skipping_requires_queue_pressure_and_an_audio_clock() {
    let packet = video_packet(Some(1_000), 920, 40);
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let mut context = pressure_context(2_000_000_000, &video_clock);
    context.skip_nonref_for_pressure = false;
    assert!(!context.should_skip_nonref_for_pressure(&packet));
    context.skip_nonref_for_pressure = true;
    context.played_until_nsecs = None;
    assert!(!context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_keeps_long_duration_frames_that_can_still_be_presented() {
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let context = pressure_context(1_250_000_000, &video_clock);
    let packet = video_packet(Some(1_000), 920, 300);
    assert!(!context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_uses_stream_duration_when_packet_duration_is_missing() {
    let packet = video_packet(Some(1_000), 920, 0);
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let mut context = pressure_context(1_200_000_000, &video_clock);
    context.video_stream.frame_duration_nsecs = Some(200_000_000);
    assert!(!context.should_skip_nonref_for_pressure(&packet));
    context.played_until_nsecs = Some(1_275_000_000);
    assert!(context.should_skip_nonref_for_pressure(&packet));
}

#[test]
fn pressure_skipping_preserves_packets_without_a_known_frame_duration() {
    let packet = video_packet(Some(1_000), 920, 0);
    let video_clock = TimestampMapper::new(Some(0), 0, None);
    let mut context = pressure_context(2_000_000_000, &video_clock);
    for duration in [None, Some(0)] {
        context.video_stream.frame_duration_nsecs = duration;
        assert!(!context.should_skip_nonref_for_pressure(&packet));
    }
}

#[test]
fn pending_packets_keep_their_skip_policy_when_backpressure_delays_submission() {
    let mut pending = VideoDecodePacketQueues::default();
    let mut replay = VecDeque::new();
    for (generation, skip_nonref) in [(1, true), (2, false), (3, true), (4, false)] {
        assert!(
            pending
                .push_pending_input(PendingVideoDecodePacket {
                    generation,
                    packet: video_packet(Some(1_000), 920, 40),
                    drop_policy: if skip_nonref {
                        VideoDecodeDropPolicy::SeekPreroll
                    } else {
                        VideoDecodeDropPolicy::None
                    },
                    realign_after_decode_recovery: false,
                    hevc_startup_in_flight_watchdog: false,
                    from_hevc_hw_replay: false,
                    hevc_decode_recovery_evidence_scoped: false,
                })
                .is_ok()
        );
    }
    let blocked = take_next_video_decode_input(&mut pending, &mut replay).unwrap();
    requeue_backpressured_video_decode_input(&mut pending, &mut replay, blocked);
    let mut policies = Vec::new();
    while let Some(packet) = take_next_video_decode_input(&mut pending, &mut replay) {
        policies.push((packet.generation, packet.drop_policy.skip_nonref()));
    }
    assert_eq!(policies, [(1, true), (2, false), (3, true), (4, false)]);
}
