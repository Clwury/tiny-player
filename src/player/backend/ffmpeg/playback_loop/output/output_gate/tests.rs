use std::time::Duration;

use crate::player::render_host::{
    DecodedFrame, FramePixels, FramePts, PlaybackSessionId, RenderSize,
};

use super::super::{
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, QueuedVideoFrame, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
};
use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioClockResumeDecision,
    AudioOutputSnapshot, DecodedAudio, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PendingStartAudioPressureLevel,
    PlaybackOutputScheduler, PlaybackOutputState, PlaybackResumeWaterline,
    audio_output_contiguous_start_timeline_nsecs, audio_output_flush_until_timeline_nsecs,
    discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
    initial_delayed_audio_start_timeline_nsecs, playing_pending_audio_limit_duration,
    playing_pending_audio_pressure_clear_duration,
};

fn test_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
    QueuedVideoFrame {
        frame: DecodedFrame {
            size: RenderSize {
                width: 1,
                height: 1,
            },
            pts: Some(FramePts {
                nsecs: timeline_nsecs,
            }),
            key_frame: false,
            pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
        },
        timeline_nsecs,
        duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    }
}

fn resume_decision() -> AudioClockResumeDecision {
    AudioClockResumeDecision {
        timeline_nsecs: 4_608_000_000,
        reset_audio_to_video: true,
    }
}

fn waterline(decoded_video_ready: bool) -> PlaybackResumeWaterline {
    PlaybackResumeWaterline {
        target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        decoded_video_forward_nsecs: decoded_video_ready
            .then_some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        decoded_audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        delayed_audio_start_gap_nsecs: None,
        demux_video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        demux_audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        demux_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        decoded_video_ready,
        decoded_audio_ready: true,
        demux_ready: true,
    }
}

fn audio_snapshot(played_timeline_nsecs: u64, total_pending_nsecs: u64) -> AudioOutputSnapshot {
    AudioOutputSnapshot {
        played_timeline_nsecs,
        buffered_until_timeline_nsecs: played_timeline_nsecs.saturating_add(total_pending_nsecs),
        shared_pending_nsecs: total_pending_nsecs,
        queue_pending_nsecs: 0,
        total_pending_nsecs,
        queue_frames: 0,
        queue_generation: 0,
    }
}

#[test]
fn audio_output_flush_until_caps_total_pending_audio() {
    let snapshot = audio_snapshot(10_000_000_000, 0);
    let video_lead_until = 12_000_000_000;

    assert_eq!(
        audio_output_flush_until_timeline_nsecs(snapshot, video_lead_until),
        10_000_000_000 + duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT)
    );
}

#[test]
fn audio_output_flush_until_stops_when_output_already_past_limit() {
    let snapshot = audio_snapshot(10_000_000_000, duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT) + 1);
    let video_lead_until = 12_000_000_000;

    assert!(
        audio_output_flush_until_timeline_nsecs(snapshot, video_lead_until)
            < audio_output_contiguous_start_timeline_nsecs(snapshot)
    );
}

#[test]
fn initial_delayed_audio_start_detects_video_clock_gap() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_022_780_000,
        1_042_780_000,
    );

    assert_eq!(
        initial_delayed_audio_start_timeline_nsecs(
            &scheduler,
            AudioClockResumeDecision {
                timeline_nsecs: 1_022_780_000,
                reset_audio_to_video: false,
            },
        ),
        Some(1_022_780_000)
    );
}

#[test]
fn initial_delayed_audio_start_ignores_small_continuity_gap() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_001_000,
        1_020_001_000,
    );

    assert_eq!(
        initial_delayed_audio_start_timeline_nsecs(
            &scheduler,
            AudioClockResumeDecision {
                timeline_nsecs: 1_000_000_000,
                reset_audio_to_video: false,
            },
        ),
        None
    );
}

#[test]
fn playing_pending_audio_pressure_levels_follow_steady_state_thresholds() {
    assert_eq!(
        playing_pending_audio_limit_duration(),
        AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(
            playing_pending_audio_limit_duration() - Duration::from_nanos(1)
        ),
        PendingStartAudioPressureLevel::Normal
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(playing_pending_audio_limit_duration()),
        PendingStartAudioPressureLevel::Warn
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(
            PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION
        ),
        PendingStartAudioPressureLevel::ForceRecovery
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(PLAYING_PENDING_AUDIO_HARD_RESET_DURATION),
        PendingStartAudioPressureLevel::HardReset
    );
}

#[test]
fn playing_pending_audio_pressure_uses_clear_hysteresis() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let limit = playing_pending_audio_limit_duration();
    let clear_duration = playing_pending_audio_pressure_clear_duration();
    assert!(clear_duration < limit);

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(limit) + 1,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(limit) + 1,
    );
    scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
    assert_eq!(
        scheduler.pending_start_audio_pressure_level,
        PendingStartAudioPressureLevel::Warn
    );

    scheduler.pending_start_audio.clear();
    let near_limit = limit - Duration::from_millis(1);
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(near_limit),
        },
        2_000_000_000,
        2_000_000_000 + duration_nsecs(near_limit),
    );
    scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
    assert_eq!(
        scheduler.pending_start_audio_pressure_level,
        PendingStartAudioPressureLevel::Warn
    );

    scheduler.pending_start_audio.clear();
    let cleared = clear_duration - Duration::from_nanos(1);
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(cleared),
        },
        3_000_000_000,
        3_000_000_000 + duration_nsecs(cleared),
    );
    scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");
    assert_eq!(
        scheduler.pending_start_audio_pressure_level,
        PendingStartAudioPressureLevel::Normal
    );
}

#[test]
fn pending_start_audio_can_recover_playing_audio_output() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_300_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 300_000_000,
        },
        1_000_000_000,
        1_300_000_000,
    );

    assert!(
        scheduler.pending_start_audio_can_recover_output(Some(audio_snapshot(1_000_000_000, 0)))
    );
}

#[test]
fn output_gate_keeps_pre_resume_video_until_waterline_ready() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(4_400_000_000));

    let dropped = discard_decoded_video_before_output_gate_resume_if_ready(
        &mut scheduler,
        waterline(false),
        resume_decision(),
        PlaybackSessionId(1),
        4_423_755_102,
        None,
    );

    assert_eq!(dropped, 0);
    assert_eq!(scheduler.scheduled_video_queue.len(), 1);
    assert_eq!(
        scheduler.scheduled_video_queue.range_nsecs(),
        Some((
            4_400_000_000,
            4_400_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS
        ))
    );
}

#[test]
fn output_gate_discards_pre_resume_video_once_waterline_ready() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(4_400_000_000));

    let dropped = discard_decoded_video_before_output_gate_resume_if_ready(
        &mut scheduler,
        waterline(true),
        resume_decision(),
        PlaybackSessionId(1),
        4_423_755_102,
        None,
    );

    assert_eq!(dropped, 1);
    assert!(scheduler.scheduled_video_queue.is_empty());
}

#[test]
fn decoded_audio_direct_push_requires_video_coverage_at_audio_start() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(8_840_000_000));

    assert!(scheduler.decoded_audio_can_push_directly(8_860_000_000, 9_100_000_000, 8_860_000_000));
    assert!(!scheduler.decoded_audio_can_push_directly(
        9_080_000_000,
        9_120_000_000,
        9_080_000_000
    ));
    assert!(!scheduler.decoded_audio_can_push_directly(
        8_860_000_000,
        9_100_000_000,
        9_000_000_000
    ));
}
