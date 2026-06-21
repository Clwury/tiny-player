use super::super::{
    AUDIO_OUTPUT_DELAY_LIMIT, DecodedAudio, InitialOutputSyncDecision, PlaybackOutputScheduler,
    PlaybackOutputState, audio_output_contiguous_start_timeline_nsecs,
    audio_output_flush_until_timeline_nsecs, duration_nsecs,
    initial_delayed_audio_start_timeline_nsecs,
};
use super::{audio_snapshot, test_queued_video_frame};

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
            InitialOutputSyncDecision {
                video_resume_timeline_nsecs: 1_000_000_000,
                audio_start_timeline_nsecs: Some(1_022_780_000),
                delayed_audio_start_timeline_nsecs: Some(1_022_780_000),
                drop_audio_before_timeline_nsecs: None,
                stale_audio_preroll_until_nsecs: None,
                stale_audio_preroll_gap_nsecs: None,
                allow_initial_audio_gap_at_video_start: false,
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
            InitialOutputSyncDecision {
                video_resume_timeline_nsecs: 1_000_000_000,
                audio_start_timeline_nsecs: Some(1_000_001_000),
                delayed_audio_start_timeline_nsecs: None,
                drop_audio_before_timeline_nsecs: None,
                stale_audio_preroll_until_nsecs: None,
                stale_audio_preroll_gap_nsecs: None,
                allow_initial_audio_gap_at_video_start: false,
                reset_audio_to_video: false,
            },
        ),
        None
    );
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
