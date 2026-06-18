use std::time::Duration;

use crate::player::render_host::PlaybackSessionId;

use super::super::super::DEFAULT_VIDEO_FRAME_DURATION_NSECS;
use super::super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, DecodedAudio,
    PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION, PLAYING_PENDING_AUDIO_HARD_RESET_DURATION,
    PendingStartAudioPressureLevel, PlaybackOutputScheduler, PlaybackOutputState,
    discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
    playing_pending_audio_limit_duration, playing_pending_audio_pressure_clear_duration,
};
use super::{audio_snapshot, resume_decision, test_queued_video_frame, waterline};

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
