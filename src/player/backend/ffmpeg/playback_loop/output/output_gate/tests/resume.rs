use std::time::Duration;

use crate::player::backend::ffmpeg::FfmpegControl;
use crate::player::render_host::PlaybackSessionId;

use super::super::super::DEFAULT_VIDEO_FRAME_DURATION_NSECS;
use super::super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioResumeWaterline,
    DecodedAudio, DecodedAudioAdmission, MAX_REBUFFER_AUDIO_LEAD_NSECS,
    PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION, PLAYING_PENDING_AUDIO_HARD_RESET_DURATION,
    PendingAudioPressureContext, PendingAudioRetentionAnchorSource, PendingAudioRetentionPlan,
    PendingStartAudioPressureLevel, PlaybackBlockReason, PlaybackOutputScheduler,
    PlaybackOutputState, RebufferResumeAnchor, StaleRebufferPendingAudio,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
    playing_pending_audio_limit_duration, playing_pending_audio_pressure_clear_duration,
    playing_pending_audio_warn_entry_duration, stale_rebuffer_pending_audio,
    stale_rebuffer_pending_audio_ahead,
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
        PendingStartAudioPressureLevel::Normal
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(Duration::from_millis(859)),
        PendingStartAudioPressureLevel::Normal
    );
    assert_eq!(
        PendingStartAudioPressureLevel::from_duration(playing_pending_audio_warn_entry_duration()),
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
            duration_nsecs: duration_nsecs(playing_pending_audio_warn_entry_duration()) + 1,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(playing_pending_audio_warn_entry_duration()) + 1,
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
fn startup_audio_input_backpressure_uses_first_contiguous_run() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let threshold_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: threshold_nsecs,
        },
        1_020_000_000,
        1_020_000_000 + threshold_nsecs,
    );

    assert!(scheduler.output_wait_audio_input_backpressured());
    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn disconnected_startup_audio_does_not_fake_contiguous_backpressure() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let short_run_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) / 2;

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: short_run_nsecs,
        },
        1_000_000_000,
        1_000_000_000 + short_run_nsecs,
    );
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: short_run_nsecs * 2,
        },
        3_000_000_000,
        3_000_000_000 + short_run_nsecs * 2,
    );

    assert!(!scheduler.output_wait_audio_input_backpressured());
    assert!(!scheduler.pending_start_audio_backpressured());
}

#[test]
fn disconnected_rebuffer_audio_does_not_close_audio_packet_admission() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 10_000_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(10_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_200_000_000,
        },
        15_000_000_000,
        16_200_000_000,
    );

    assert!(!scheduler.output_wait_audio_input_backpressured());
}

#[test]
fn audio_rebuffer_prefill_target_uses_loop_recovery_after_repeated_underruns() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = std::time::Instant::now();

    assert_eq!(
        scheduler.audio_rebuffer_prefill_target_nsecs(None),
        duration_nsecs(AUDIO_REBUFFER_PREFILL_TARGET)
    );

    scheduler.observe_audio_output_underrun_for_rebuffer(now, PlaybackSessionId(1));
    assert_eq!(
        scheduler.audio_rebuffer_prefill_target_nsecs(None),
        duration_nsecs(AUDIO_REBUFFER_PREFILL_TARGET)
    );

    scheduler.observe_audio_output_underrun_for_rebuffer(
        now + Duration::from_millis(500),
        PlaybackSessionId(1),
    );
    assert!(scheduler.audio_rebuffer_loop_active());
    assert_eq!(
        scheduler.audio_rebuffer_prefill_target_nsecs(None),
        duration_nsecs(AUDIO_REBUFFER_PREFILL_LOOP_TARGET)
    );
}

#[test]
fn far_ahead_rebuffer_audio_requests_video_master_realign_after_bounded_observations() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 5_640_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(5_680_000_000));

    assert!(
        scheduler
            .observe_rebuffer_far_ahead_audio_frame(
                182_000_000_000,
                5_640_000_000,
                Some(0),
                false,
                PlaybackSessionId(1),
                "test_far_ahead",
            )
            .is_none()
    );
    assert!(
        scheduler
            .observe_rebuffer_far_ahead_audio_frame(
                182_020_000_000,
                5_640_000_000,
                Some(0),
                false,
                PlaybackSessionId(1),
                "test_far_ahead",
            )
            .is_none()
    );
    assert!(
        scheduler
            .observe_rebuffer_far_ahead_audio_frame(
                182_040_000_000,
                5_640_000_000,
                Some(0),
                false,
                PlaybackSessionId(1),
                "test_far_ahead",
            )
            .is_none(),
        "repeated observations arm the continuity watchdog but cannot bypass it"
    );
    scheduler.expire_audio_reader_gap_watchdog_for_test();

    let request = scheduler
        .observe_rebuffer_far_ahead_audio_frame(
            182_060_000_000,
            5_640_000_000,
            Some(0),
            false,
            PlaybackSessionId(1),
            "test_far_ahead",
        )
        .expect("a persistent continuity gap requests realign after the watchdog expires");

    assert_eq!(request.target_timeline_nsecs, 5_680_000_000);
    assert_eq!(request.anchor_timeline_nsecs, 5_640_000_000);
    assert_eq!(request.first_video_timeline_nsecs, 5_680_000_000);
    assert_eq!(request.far_ahead_observation_count, 4);
    assert_eq!(
        scheduler
            .take_rebuffer_audio_realign_request()
            .map(|request| request.target_timeline_nsecs),
        Some(5_680_000_000)
    );
}

#[test]
fn forced_far_ahead_realign_bypasses_coordinator_stall_and_live_audio() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 5_640_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(5_680_000_000));
    scheduler.record_coordinator_tick(Duration::from_millis(65));

    assert!(
        scheduler
            .observe_rebuffer_far_ahead_audio_frame(
                182_000_000_000,
                5_640_000_000,
                Some(500_000_000),
                true,
                PlaybackSessionId(1),
                "test_coordinator_stall",
            )
            .is_some()
    );
    assert!(scheduler.rebuffer_audio_realign_request_pending());
}

#[test]
fn coordinator_stall_does_not_hide_real_audio_gap_or_duplicate_transaction() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 5_640_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(5_680_000_000));
    scheduler.record_coordinator_tick(Duration::from_millis(65));

    let request = scheduler
        .observe_rebuffer_far_ahead_audio_frame(
            182_000_000_000,
            5_640_000_000,
            Some(0),
            true,
            PlaybackSessionId(1),
            "test_real_audio_gap",
        )
        .expect("forced realign bypasses the gap watchdog");
    assert!(
        scheduler
            .observe_rebuffer_far_ahead_audio_frame(
                182_020_000_000,
                5_640_000_000,
                Some(0),
                true,
                PlaybackSessionId(1),
                "test_real_audio_gap",
            )
            .is_none(),
        "the same watchdog epoch cannot enqueue a duplicate realign"
    );

    assert_eq!(request.target_timeline_nsecs, 5_680_000_000);
    assert!(scheduler.take_rebuffer_audio_realign_request().is_some());
    assert!(scheduler.take_rebuffer_audio_realign_request().is_none());
}

#[test]
fn coordinator_stall_with_audio_output_coverage_suppresses_reader_realign() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 15_120_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_120_000_000));
    scheduler.record_coordinator_tick(Duration::from_millis(65));

    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                20_000_000_000,
                AudioResumeWaterline {
                    resume_timeline_nsecs: 15_120_000_000,
                    target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                    audio_output_pending_nsecs: Some(500_000_000),
                    audio_output_buffered_until_nsecs: Some(15_620_000_000),
                    ..AudioResumeWaterline::default()
                },
                15_120_000_000,
                PlaybackSessionId(1),
            )
            .is_none()
    );
    assert!(!scheduler.rebuffer_audio_realign_request_pending());
}

#[test]
fn reader_head_far_ahead_rebuffer_empty_audio_waits_for_real_gap_watchdog() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 15_120_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_120_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_160_000_000));
    scheduler.set_rebuffer_empty_audio_output_blocked(true);

    let waterline = AudioResumeWaterline {
        resume_timeline_nsecs: 15_120_000_000,
        target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        audio_output_pending_nsecs: Some(0),
        pending_audio_start_nsecs: Some(15_871_632_256),
        demux_audio_forward_nsecs: Some(208_000_000_000),
        ..AudioResumeWaterline::default()
    };
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                16_320_000_000,
                waterline,
                14_216_734_626,
                PlaybackSessionId(1),
            )
            .is_none(),
        "reader head alone cannot trigger realign before the no-progress watchdog expires"
    );
    scheduler.expire_audio_reader_gap_watchdog_for_test();

    let request = scheduler
        .request_output_wait_audio_reader_head_realign_if_needed(
            16_320_000_000,
            waterline,
            14_216_734_626,
            PlaybackSessionId(1),
        )
        .expect("a real continuity gap requests realign after the watchdog expires");

    assert_eq!(request.reason, "rebuffer_audio_reader_far_ahead");
    assert_eq!(request.target_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.anchor_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.first_video_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.far_ahead_audio_timeline_nsecs, 16_320_000_000);
    assert!(request.far_ahead_observation_count < 3);
    assert_eq!(
        scheduler
            .take_rebuffer_audio_realign_request()
            .map(|request| request.target_timeline_nsecs),
        Some(15_120_000_000)
    );
    scheduler.defer_audio_reader_gap_watchdog_after_input_pending(15_120_000_000);
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                16_320_000_000,
                waterline,
                14_216_734_626,
                PlaybackSessionId(1),
            )
            .is_none(),
        "in-flight progress rearms the watchdog instead of losing the request forever"
    );
    scheduler.expire_audio_reader_gap_watchdog_for_test();
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                16_320_000_000,
                waterline,
                14_216_734_626,
                PlaybackSessionId(1),
            )
            .is_some(),
        "a still-missing gap can request realign again after the rearmed watchdog expires"
    );
}

#[test]
fn reader_head_one_truehd_packet_past_resume_target_does_not_realign() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 15_120_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_120_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_160_000_000));
    scheduler.set_rebuffer_empty_audio_output_blocked(true);

    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                16_120_000_001,
                AudioResumeWaterline {
                    resume_timeline_nsecs: 15_120_000_000,
                    target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                    audio_output_pending_nsecs: Some(0),
                    ..AudioResumeWaterline::default()
                },
                14_216_734_626,
                PlaybackSessionId(1),
            )
            .is_none(),
        "one packet past the nominal target remains inside the guarded reader window"
    );
}

#[test]
fn near_complete_pending_audio_prevents_reader_realign_and_clear() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 15_120_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_120_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_160_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 998_840_000,
        },
        15_120_000_000,
        16_118_840_000,
    );
    scheduler.set_rebuffer_empty_audio_output_blocked(true);

    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                16_320_000_000,
                AudioResumeWaterline {
                    resume_timeline_nsecs: 15_120_000_000,
                    target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                    audio_accepted_start_timeline_nsecs: Some(15_120_000_000),
                    audio_accepted_start_gap_nsecs: Some(0),
                    accepted_contiguous_coverage_nsecs: Some(998_840_000),
                    audio_output_pending_nsecs: Some(0),
                    pending_audio_start_nsecs: Some(15_120_000_000),
                    pending_audio_forward_nsecs: Some(998_840_000),
                    decoded_audio_forward_nsecs: Some(998_840_000),
                    ..AudioResumeWaterline::default()
                },
                14_216_734_626,
                PlaybackSessionId(1),
            )
            .is_none(),
        "continuous pending audio near the resume target must not be discarded"
    );
    assert_eq!(scheduler.snapshot().pending_start_audio_frames, 1);
}

#[test]
fn delayed_audio_within_av_tolerance_and_protected_waterline_does_not_realign() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let resume_nsecs = 62_521_000_000;
    let delayed_start_nsecs = resume_nsecs + 71_000_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(resume_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_056_000_000,
        },
        delayed_start_nsecs,
        delayed_start_nsecs + 1_056_000_000,
    );

    let coverage = scheduler.audio_realign_coverage(
        resume_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );

    assert!(coverage.ready);
    assert_eq!(
        coverage.audio_accepted_start_timeline_nsecs,
        Some(delayed_start_nsecs)
    );
    assert_eq!(coverage.start_gap_nsecs, Some(71_000_000));
    assert_eq!(coverage.contiguous_coverage_nsecs, Some(1_056_000_000));
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                resume_nsecs + 5_000_000_000,
                AudioResumeWaterline {
                    resume_timeline_nsecs: resume_nsecs,
                    target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                    audio_accepted_start_timeline_nsecs: coverage
                        .audio_accepted_start_timeline_nsecs,
                    audio_accepted_start_gap_nsecs: coverage.start_gap_nsecs,
                    accepted_contiguous_coverage_nsecs: coverage.contiguous_coverage_nsecs,
                    audio_output_pending_nsecs: Some(0),
                    ..AudioResumeWaterline::default()
                },
                resume_nsecs,
                PlaybackSessionId(1),
            )
            .is_none(),
        "71ms delayed audio with 1.056s coverage must not reader-realign"
    );
}

#[test]
fn delayed_audio_beyond_av_tolerance_requires_a_stalled_gap_before_realign() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let resume_nsecs = 62_521_000_000;
    let delayed_start_nsecs = resume_nsecs + 81_000_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(resume_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_056_000_000,
        },
        delayed_start_nsecs,
        delayed_start_nsecs + 1_056_000_000,
    );

    let coverage = scheduler.audio_realign_coverage(
        resume_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    assert!(!coverage.ready);
    assert_eq!(coverage.audio_accepted_start_timeline_nsecs, None);
    let waterline = AudioResumeWaterline {
        resume_timeline_nsecs: resume_nsecs,
        target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        audio_output_pending_nsecs: Some(0),
        ..AudioResumeWaterline::default()
    };
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                resume_nsecs + 5_000_000_000,
                waterline,
                resume_nsecs,
                PlaybackSessionId(1),
            )
            .is_none(),
        "reader head first arms the no-progress watchdog"
    );
    scheduler.expire_audio_reader_gap_watchdog_for_test();
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                resume_nsecs + 5_000_000_000,
                waterline,
                resume_nsecs,
                PlaybackSessionId(1),
            )
            .is_some(),
        "the confirmed stalled gap can realign after the watchdog expires"
    );
}

#[test]
fn delayed_audio_with_partial_resume_coverage_does_not_realign_from_reader_head() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let resume_nsecs = 62_521_000_000;
    let delayed_start_nsecs = resume_nsecs + 71_000_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(resume_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 849_000_000,
        },
        delayed_start_nsecs,
        delayed_start_nsecs + 849_000_000,
    );

    let coverage = scheduler.audio_realign_coverage(
        resume_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );

    assert!(!coverage.ready);
    assert_eq!(coverage.start_gap_nsecs, Some(71_000_000));
    assert_eq!(coverage.contiguous_coverage_nsecs, Some(849_000_000));
    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                resume_nsecs + 5_000_000_000,
                AudioResumeWaterline {
                    resume_timeline_nsecs: resume_nsecs,
                    target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                    audio_accepted_start_timeline_nsecs: coverage
                        .audio_accepted_start_timeline_nsecs,
                    audio_accepted_start_gap_nsecs: coverage.start_gap_nsecs,
                    accepted_contiguous_coverage_nsecs: coverage.contiguous_coverage_nsecs,
                    audio_output_pending_nsecs: Some(0),
                    ..AudioResumeWaterline::default()
                },
                resume_nsecs,
                PlaybackSessionId(1),
            )
            .is_none(),
        "partial continuous coverage is consumed instead of treating the producer cursor as a gap"
    );
}

#[test]
fn startup_reader_head_gap_requests_realign_before_playback_resume() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(202_550_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_996_916_044,
        },
        202_570_884_290,
        204_567_800_334,
    );
    let waterline = AudioResumeWaterline {
        resume_timeline_nsecs: 202_550_000_000,
        target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        audio_output_pending_nsecs: Some(0),
        pending_audio_start_nsecs: Some(202_570_884_290),
        audio_decode_queued_nsecs: 1_500_000_000,
        audio_decode_in_flight_packets: 1,
        ..AudioResumeWaterline::default()
    };

    assert!(
        scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                206_000_000_000,
                waterline,
                202_549_751_669,
                PlaybackSessionId(1),
            )
            .is_none(),
        "reader packets represented by queued/in-flight decode work are not a continuity gap"
    );

    let request = scheduler
        .request_output_wait_audio_reader_head_realign_if_needed(
            212_021_405_896,
            waterline,
            202_549_751_669,
            PlaybackSessionId(1),
        )
        .expect("reader PTS span bound requests realign despite stuck in-flight work");

    assert_eq!(request.reason, "output_wait_audio_reader_continuity_gap");
    assert_eq!(request.target_timeline_nsecs, 202_550_000_000);
    assert_eq!(request.far_ahead_audio_timeline_nsecs, 212_021_405_896);
}

#[test]
fn audio_gap_recovery_suppresses_empty_audio_rebuffer_while_video_has_low_water() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let video_start_nsecs = 1_000_000_000;
    for index in 0..10 {
        scheduler.push_decoded_video_for_test(test_queued_video_frame(
            video_start_nsecs + index * DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        ));
    }
    let now = std::time::Instant::now();
    scheduler.begin_audio_gap_recovery(
        video_start_nsecs,
        now,
        PlaybackSessionId(1),
        "test_audio_gap",
    );

    assert!(!scheduler.maybe_enter_video_output_rebuffer(
        now + Duration::from_millis(100),
        true,
        Some(400_000_000),
        true,
        false,
        Some(400_000_000),
        false,
        1,
        true,
        false,
        &control,
        None,
        Some(0),
        PlaybackSessionId(1),
        Some(400_000_000),
    ));
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Playing);
}

#[test]
fn healthy_demux_prevents_cache_rebuffer_when_video_queue_full_and_vo_empty() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let video_start_nsecs = 1_000_000_000;
    for index in 0..50 {
        scheduler.push_decoded_video_for_test(test_queued_video_frame(
            video_start_nsecs + index * DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        ));
    }
    let now = std::time::Instant::now();
    scheduler.begin_audio_gap_recovery(
        video_start_nsecs,
        now,
        PlaybackSessionId(1),
        "test_audio_gap",
    );

    assert!(!scheduler.maybe_enter_video_output_rebuffer(
        now + Duration::from_millis(100),
        true,
        Some(1_600_000_000),
        true,
        false,
        Some(1_600_000_000),
        false,
        0,
        true,
        false,
        &control,
        None,
        Some(0),
        PlaybackSessionId(1),
        Some(1_600_000_000),
    ));
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Playing);
}

#[test]
fn audio_gap_recovery_requires_stable_audio_output_before_clearing() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let target_timeline_nsecs = 1_000_000_000;
    scheduler.begin_audio_gap_recovery(
        target_timeline_nsecs,
        std::time::Instant::now(),
        PlaybackSessionId(1),
        "test_audio_gap",
    );

    assert!(!scheduler.clear_audio_gap_recovery_if_audio_ready(
        Some(audio_snapshot(
            target_timeline_nsecs,
            duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION) - 1,
        )),
        Some(target_timeline_nsecs),
        PlaybackSessionId(1),
        "test",
    ));
    assert!(scheduler.audio_gap_recovery_active());

    assert!(scheduler.clear_audio_gap_recovery_if_audio_ready(
        Some(audio_snapshot(
            target_timeline_nsecs,
            duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION),
        )),
        Some(target_timeline_nsecs),
        PlaybackSessionId(1),
        "test",
    ));
    assert!(!scheduler.audio_gap_recovery_active());
}

#[test]
fn audio_sync_drop_before_requires_actual_audio_output_coverage() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let drop_before_timeline_nsecs = 1_000_000_000;
    scheduler.set_audio_sync_drop_before_timeline_nsecs(
        drop_before_timeline_nsecs,
        PlaybackSessionId(1),
        "test",
    );
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 100_000_000,
        },
        drop_before_timeline_nsecs,
        drop_before_timeline_nsecs + 100_000_000,
    );

    assert!(
        !scheduler.clear_audio_sync_drop_before_if_covered(None, PlaybackSessionId(1), "test",)
    );
    assert_eq!(
        scheduler.audio_sync_drop_before_timeline_nsecs(),
        Some(drop_before_timeline_nsecs)
    );

    assert!(scheduler.clear_audio_sync_drop_before_if_covered(
        Some(audio_snapshot(drop_before_timeline_nsecs, 100_000_000)),
        PlaybackSessionId(1),
        "test",
    ));
    assert_eq!(scheduler.audio_sync_drop_before_timeline_nsecs(), None);
}

#[test]
fn post_seek_video_bootstrap_blocks_rebuffer_before_first_frame() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.begin_video_bootstrap_after_seek(
        PlaybackSessionId(1),
        "hevc_decode_chain_recovery_wait_rebuffer",
    );

    assert!(!scheduler.maybe_enter_video_output_rebuffer(
        std::time::Instant::now() + Duration::from_millis(500),
        true,
        None,
        true,
        false,
        Some(1_000_000_000),
        false,
        0,
        true,
        false,
        &control,
        None,
        Some(0),
        PlaybackSessionId(1),
        None,
    ));
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Syncing);
    assert!(snapshot.first_video_frame_pending);
    assert!(snapshot.video_bootstrap_after_seek);
    assert!(!snapshot.rebuffering);
}

#[test]
fn demux_healthy_output_underflow_stays_out_of_cache_rebuffer() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);

    assert!(!scheduler.maybe_enter_video_output_rebuffer(
        std::time::Instant::now() + Duration::from_millis(500),
        true,
        None,
        true,
        false,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        false,
        0,
        true,
        false,
        &control,
        None,
        Some(0),
        PlaybackSessionId(1),
        None,
    ));
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.rebuffering);
    assert!(!snapshot.video_decode_underfill);
}

#[test]
fn audio_rebuffer_prefill_target_caps_to_video_forward_window() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = std::time::Instant::now();
    scheduler.observe_audio_output_underrun_for_rebuffer(now, PlaybackSessionId(1));
    scheduler.observe_audio_output_underrun_for_rebuffer(
        now + Duration::from_millis(500),
        PlaybackSessionId(1),
    );

    assert_eq!(
        scheduler.audio_rebuffer_prefill_target_nsecs(Some(400_000_000)),
        400_000_000
    );
}

#[test]
fn initial_start_pending_pressure_context_suppresses_steady_hard_reset() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);

    assert_eq!(
        scheduler.pending_audio_pressure_context(),
        PendingAudioPressureContext::PlayingSteady
    );

    scheduler.defer_next_pending_start_audio_flush_after_initial_start();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(PLAYING_PENDING_AUDIO_HARD_RESET_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(PLAYING_PENDING_AUDIO_HARD_RESET_DURATION),
    );

    assert_eq!(
        scheduler.pending_audio_pressure_context(),
        PendingAudioPressureContext::StartupSync
    );
    assert!(scheduler.pending_start_audio_backpressured());

    scheduler.pending_start_audio.clear();
    scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");

    assert_eq!(
        scheduler.pending_audio_pressure_context(),
        PendingAudioPressureContext::PlayingSteady
    );
}

#[test]
fn initial_start_pending_pressure_context_survives_one_shot_defer_above_clear_water() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.defer_next_pending_start_audio_flush_after_initial_start();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 859_138_298,
        },
        209_629_750_306,
        210_488_888_604,
    );

    scheduler.defer_pending_start_audio_flush_once = false;
    scheduler.report_playing_pending_start_audio_pressure(PlaybackSessionId(1), "test");

    assert_eq!(
        scheduler.pending_audio_pressure_context(),
        PendingAudioPressureContext::StartupSync
    );
}

#[test]
fn vulkan_fast_start_defers_next_aac_frame_when_pending_audio_is_905ms() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.defer_next_pending_start_audio_flush_after_initial_start();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 905_000_000,
        },
        2_000_000_000,
        2_905_000_000,
    );
    let retained_before = scheduler.pending_start_audio.len();
    assert!(scheduler.pending_start_audio_backpressured());

    let admission = scheduler.defer_decoded_audio_for_backpressure(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 21_333_333,
        },
        2_905_000_000,
        2_926_333_333,
        PlaybackSessionId(1),
        "vulkan_fast_start_regression",
    );

    let DecodedAudioAdmission::Deferred(deferred) = admission else {
        panic!("the next AAC frame must remain owned by the decode pipeline");
    };
    assert_eq!(deferred.duration_nsecs, 21_333_333);
    assert_eq!(scheduler.pending_start_audio.len(), retained_before);
    assert_eq!(
        scheduler.pending_start_audio.contiguous_range_nsecs(),
        Some((2_000_000_000, 2_905_000_000))
    );
    assert_eq!(
        scheduler.pending_audio_pressure_context(),
        PendingAudioPressureContext::StartupSync
    );
}

#[test]
fn repeated_pending_audio_backpressure_is_summarized_instead_of_logged_per_tick() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);

    for offset in [0, 21_333_333] {
        let admission = scheduler.defer_decoded_audio_for_backpressure(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 21_333_333,
            },
            2_905_000_000 + offset,
            2_926_333_333 + offset,
            PlaybackSessionId(1),
            "same_block",
        );
        assert!(matches!(admission, DecodedAudioAdmission::Deferred(_)));
    }

    let log_state = scheduler
        .pending_audio_backpressure_log_state
        .expect("backpressure series is tracked");
    assert_eq!(log_state.reason, "same_block");
    assert_eq!(log_state.suppressed_repeats, 1);
}

#[test]
fn repeated_output_gate_block_logs_only_first_and_one_second_summary() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    let now = std::time::Instant::now();

    let first = scheduler
        .observe_output_gate_block_log(
            PlaybackBlockReason::DecodedAudioQueue,
            "rebuffer_empty_audio_output",
            now,
        )
        .expect("first block is logged");
    assert_eq!(first.log_kind, "state_changed");
    assert!(
        scheduler
            .observe_output_gate_block_log(
                PlaybackBlockReason::DecodedAudioQueue,
                "rebuffer_empty_audio_output",
                now + Duration::from_millis(7),
            )
            .is_none()
    );
    let summary = scheduler
        .observe_output_gate_block_log(
            PlaybackBlockReason::DecodedAudioQueue,
            "rebuffer_empty_audio_output",
            now + Duration::from_secs(1),
        )
        .expect("one-second summary is logged");
    assert_eq!(summary.log_kind, "periodic_summary");
    assert_eq!(summary.suppressed_repeats, 1);
}

#[test]
fn entering_rebuffer_invalidates_video_clock_anchor_until_explicit_reanchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    assert!(!scheduler.video_clock_anchor_valid());

    scheduler.mark_video_clock_anchor_valid();
    assert!(scheduler.video_clock_anchor_valid());

    scheduler.set_state(PlaybackOutputState::Rebuffering);
    assert!(!scheduler.video_clock_anchor_valid());
}

#[test]
fn every_rebuffer_transition_arms_the_unified_two_second_fallback_clock() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    assert!(scheduler.rebuffer_wait_elapsed().is_none());

    scheduler.set_state(PlaybackOutputState::Rebuffering);
    assert!(scheduler.restart_fallback_deadline_armed());
    assert!(scheduler.rebuffer_wait_elapsed().is_some());

    scheduler.set_video_output_underrun_started_at_for_test(
        std::time::Instant::now() - Duration::from_secs(2),
    );
    assert!(scheduler.rebuffer_wait_elapsed().unwrap() >= Duration::from_secs(2));

    scheduler.set_state(PlaybackOutputState::Playing);
    assert!(scheduler.rebuffer_wait_elapsed().is_none());
}

#[test]
fn repeated_observation_of_same_far_ahead_frame_counts_one_rejection() {
    let mut scheduler = PlaybackOutputScheduler::new();
    for _ in 0..100 {
        scheduler.record_audio_continuity_rejection(
            4_905_000_000,
            2_905_000_000,
            None,
            PlaybackSessionId(1),
            "test_repeated_deferred_frame",
        );
    }

    let summary = scheduler
        .audio_continuity_rejection_summary
        .expect("one rejection summary is retained");
    assert_eq!(summary.first_rejected_pts_nsecs, 4_905_000_000);
    assert_eq!(summary.last_rejected_pts_nsecs, 4_905_000_000);
    assert_eq!(summary.rejected_count, 1);
    assert_eq!(summary.largest_gap_nsecs, 2_000_000_000);
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
fn audio_resume_waterline_records_decode_and_demux_diagnostics() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 1_000_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_000_000_000,
        },
        1_250_000_000,
        2_250_000_000,
    );

    let waterline = scheduler
        .audio_resume_waterline_for_output_wait(
            Some(audio_snapshot(1_000_000_000, 250_000_000)),
            64_000_000,
            3,
            1_000_000_000,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            Some(5_000_000_000),
            Some(42),
        )
        .expect("output resume waterline");

    assert!(waterline.ready);
    assert_eq!(waterline.audio_output_pending_nsecs, Some(250_000_000));
    assert_eq!(waterline.audio_decode_queued_nsecs, 64_000_000);
    assert_eq!(waterline.audio_decode_in_flight_packets, 3);
    assert_eq!(waterline.demux_audio_forward_nsecs, Some(5_000_000_000));
    assert_eq!(waterline.demux_audio_cached_packets, Some(42));
}

#[test]
fn rebuffer_audio_resume_waterline_uses_video_anchor_when_audio_output_is_empty() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let anchor_nsecs = 178_054_635_222;
    let first_video_nsecs = 178_080_000_000;
    let first_audio_nsecs = 178_120_000_000;
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: anchor_nsecs,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });
    for index in 0..36 {
        scheduler.push_decoded_video_for_test(test_queued_video_frame(
            first_video_nsecs + index * DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        ));
    }
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        first_audio_nsecs,
        first_audio_nsecs + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );

    let waterline = scheduler
        .audio_resume_waterline_for_output_wait(
            Some(audio_snapshot(anchor_nsecs, 0)),
            64_000_000,
            3,
            anchor_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            Some(5_000_000_000),
            Some(42),
        )
        .expect("output resume waterline");

    assert_eq!(waterline.resume_timeline_nsecs, anchor_nsecs);
    assert_eq!(waterline.audio_output_buffered_until_nsecs, None);
    assert_eq!(waterline.audio_output_pending_nsecs, Some(0));
}

#[test]
fn rebuffer_stale_pending_audio_ahead_is_rejected_when_audio_output_empty() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let resume_nsecs = 35_394_566_033;
    let stale_audio_start_nsecs = 237_802_666_667;
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(35_439_988_889));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        stale_audio_start_nsecs,
        stale_audio_start_nsecs + 500_000_000,
    );

    assert_eq!(
        stale_rebuffer_pending_audio_ahead(
            &scheduler,
            audio_snapshot(resume_nsecs, 0),
            resume_nsecs
        ),
        Some(stale_audio_start_nsecs)
    );
    assert_eq!(
        stale_rebuffer_pending_audio_ahead(
            &scheduler,
            audio_snapshot(resume_nsecs, 0),
            stale_audio_start_nsecs.saturating_sub(MAX_REBUFFER_AUDIO_LEAD_NSECS),
        ),
        None
    );
}

#[test]
fn stale_rebuffer_pending_audio_behind_detects_anchor_miss_video_resume() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let resume_nsecs = 24_000_000_000;
    let pending_audio_start_nsecs = 639_999_984;
    let pending_audio_until_nsecs = 1_639_999_984;
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 605_805_324,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(resume_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: pending_audio_until_nsecs - pending_audio_start_nsecs,
        },
        pending_audio_start_nsecs,
        pending_audio_until_nsecs,
    );

    assert_eq!(
        stale_rebuffer_pending_audio(&scheduler, audio_snapshot(605_805_324, 0), resume_nsecs),
        Some(StaleRebufferPendingAudio::Behind {
            pending_start_nsecs: pending_audio_start_nsecs,
            pending_until_nsecs: Some(pending_audio_until_nsecs),
        })
    );
}

#[test]
fn startup_audio_resume_waterline_waits_for_unpresented_video_anchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 200_000_000,
        },
        1_000_000_000,
        1_200_000_000,
    );

    assert!(scheduler.scheduled_video_queue.is_empty());
    assert!(
        scheduler
            .audio_resume_waterline_for_output_wait(
                Some(audio_snapshot(1_000_000_000, 0)),
                64_000_000,
                2,
                1_000_000_000,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                Some(3_000_000_000),
                Some(7),
            )
            .is_none()
    );
}

#[test]
fn startup_audio_resume_waterline_waits_for_margin_before_input_suppression() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let suppression_threshold = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: suppression_threshold - 1,
        },
        1_000_000_000,
        1_000_000_000 + suppression_threshold - 1,
    );

    assert!(!scheduler.scheduled_video_queue.is_empty());
    assert!(scheduler.audio_resume_waterline_below_input_suppression(
        Some(audio_snapshot(1_000_000_000, 0)),
        0,
        0,
        1_000_000_000,
    ));

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1,
        },
        1_000_000_000 + suppression_threshold - 1,
        1_000_000_000 + suppression_threshold,
    );

    assert!(!scheduler.audio_resume_waterline_below_input_suppression(
        Some(audio_snapshot(1_000_000_000, 0)),
        0,
        0,
        1_000_000_000,
    ));
}

#[test]
fn startup_audio_resume_waterline_below_input_suppression_keeps_filling() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 200_000_000,
        },
        1_000_000_000,
        1_200_000_000,
    );

    assert!(!scheduler.scheduled_video_queue.is_empty());
    assert!(scheduler.audio_resume_waterline_below_input_suppression(
        Some(audio_snapshot(1_000_000_000, 0)),
        0,
        0,
        1_000_000_000,
    ));
}

#[test]
fn audio_far_ahead_reference_uses_start_position_before_first_video_frame() {
    let scheduler = PlaybackOutputScheduler::new();

    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(5_000_000_000, None),
        5_000_000_000
    );
}

#[test]
fn audio_far_ahead_reference_uses_actual_played_and_buffered_timeline() {
    let scheduler = PlaybackOutputScheduler::new();
    let stale_start_anchor_nsecs = 139_233_333_333;
    let played_timeline_nsecs = 141_432_743_662;
    let buffered_audio_nsecs = 100_589_671;

    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(
            stale_start_anchor_nsecs,
            Some(audio_snapshot(played_timeline_nsecs, buffered_audio_nsecs)),
        ),
        141_533_333_333
    );
}

#[test]
fn audio_far_ahead_reference_follows_first_queued_video_frame_during_initial_sync() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(6_000_000_000));

    // Software-decoder fallback can move the first decodable video frame past
    // the requested start position; the reference must follow the actual
    // resume point so realigned audio is not dropped again.
    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(0, None),
        6_000_000_000
    );
    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(7_000_000_000, None),
        7_000_000_000
    );
}

#[test]
fn audio_far_ahead_reference_follows_rebuffer_resume_target_mid_playback() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 93_834_465_103,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(94_200_000_000));

    // Playback started at 0:00; the far-ahead reference must follow the resume
    // target, not the session start position.
    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(0, None),
        94_200_000_000
    );
}

#[test]
fn audio_far_ahead_reference_falls_back_to_anchor_without_video_queue() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 93_834_465_103,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });

    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(0, None),
        93_834_465_103
    );
}

#[test]
fn rebuffer_audio_resume_waterline_without_video_queue_stops_filling() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_700_000_000,
        },
        0,
        1_700_000_000,
    );

    assert!(scheduler.scheduled_video_queue.is_empty());
    assert!(scheduler.waiting_for_output_resume());
    // Rebuffering with an empty video queue yields no waterline, so the audio
    // drain must not keep waiting for it to fill.
    assert!(!scheduler.audio_resume_waterline_below_input_suppression(
        Some(audio_snapshot(0, 0)),
        0,
        0,
        0,
    ));
}

#[test]
fn output_resume_discard_removes_stale_pending_audio_before_anchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 1_000_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 400_000_000,
        },
        500_000_000,
        900_000_000,
    );
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_000_000_000,
        },
        1_000_000_000,
        2_000_000_000,
    );

    let waterline = scheduler
        .discard_stale_pending_audio_before_output_resume(
            Some(audio_snapshot(1_000_000_000, 0)),
            0,
            0,
            1_000_000_000,
            44_100,
            2,
            PlaybackSessionId(1),
        )
        .expect("output resume waterline");

    assert_eq!(scheduler.pending_start_audio.len(), 1);
    assert_eq!(
        scheduler.pending_start_audio.first_start_timeline_nsecs(),
        Some(1_000_000_000)
    );
    assert!(waterline.ready);
    assert_eq!(waterline.decoded_audio_forward_nsecs, Some(1_000_000_000));
}

#[test]
fn primed_maintenance_keeps_audio_at_immutable_165_279637171_transaction_target() {
    const VIDEO_TARGET_NSECS: u64 = 165_266_666_667;
    const AUDIO_TARGET_NSECS: u64 = 165_279_637_171;
    const PRESENTED_QUEUE_FRONT_NSECS: u64 = 165_333_333_333;
    let audio_coverage_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN);

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(VIDEO_TARGET_NSECS));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(PRESENTED_QUEUE_FRONT_NSECS));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 200_000],
            duration_nsecs: audio_coverage_nsecs,
        },
        AUDIO_TARGET_NSECS,
        AUDIO_TARGET_NSECS + audio_coverage_nsecs,
    );
    scheduler.begin_initial_av_start_transaction(
        VIDEO_TARGET_NSECS,
        AUDIO_TARGET_NSECS,
        std::time::Instant::now(),
    );
    scheduler.mark_first_frame_presented();
    scheduler.scheduled_video_queue.pop_front();

    assert_eq!(
        scheduler
            .scheduled_video_queue
            .range_nsecs()
            .map(|range| range.0),
        Some(PRESENTED_QUEUE_FRONT_NSECS)
    );
    assert_eq!(
        scheduler
            .pending_audio_retention_plan()
            .map(|plan| plan.anchor_timeline_nsecs),
        Some(AUDIO_TARGET_NSECS)
    );

    let waterline = scheduler
        .discard_stale_pending_audio_before_output_resume(
            Some(audio_snapshot(VIDEO_TARGET_NSECS, 0)),
            0,
            0,
            VIDEO_TARGET_NSECS,
            48_000,
            2,
            PlaybackSessionId(165),
        )
        .expect("primed transaction waterline");

    assert_eq!(
        scheduler.pending_start_audio.first_start_timeline_nsecs(),
        Some(AUDIO_TARGET_NSECS)
    );
    assert_eq!(waterline.resume_timeline_nsecs, AUDIO_TARGET_NSECS);
    assert!(scheduler.output_wait_audio_input_backpressured());
}

#[test]
fn primed_transaction_rejects_a_trim_to_the_advanced_video_queue_front() {
    const AUDIO_TARGET_NSECS: u64 = 165_279_637_171;
    const ADVANCED_VIDEO_FRONT_NSECS: u64 = 165_333_333_333;
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 9_600],
            duration_nsecs: 100_000_000,
        },
        AUDIO_TARGET_NSECS,
        AUDIO_TARGET_NSECS + 100_000_000,
    );
    scheduler.begin_initial_av_start_transaction(
        165_266_666_667,
        AUDIO_TARGET_NSECS,
        std::time::Instant::now(),
    );

    let dropped = scheduler.trim_pending_audio_to_retention_plan(
        PendingAudioRetentionPlan {
            anchor_timeline_nsecs: ADVANCED_VIDEO_FRONT_NSECS,
            source: PendingAudioRetentionAnchorSource::UnpresentedVideo,
        },
        48_000,
        2,
        PlaybackSessionId(165),
    );

    assert_eq!(dropped, 0);
    assert_eq!(
        scheduler.pending_start_audio.first_start_timeline_nsecs(),
        Some(AUDIO_TARGET_NSECS)
    );
}

#[test]
fn rebuffer_backpressure_uses_coverage_after_exact_resume_anchor() {
    const ANCHOR_NSECS: u64 = 141_533_333_333;
    const AUDIO_START_NSECS: u64 = 141_432_743_662;
    const PENDING_NSECS: u64 = 905_578_206;
    const EFFECTIVE_COVERAGE_NSECS: u64 = 804_988_535;

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.defer_next_pending_start_audio_flush_after_initial_start();
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: ANCHOR_NSECS,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(ANCHOR_NSECS));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            // Enough 44.1-kHz stereo elements for the leading-frame trim.
            samples: vec![0.0; 100_000],
            duration_nsecs: PENDING_NSECS,
        },
        AUDIO_START_NSECS,
        AUDIO_START_NSECS + PENDING_NSECS,
    );

    assert!(scheduler.pending_start_audio_backpressured());
    let waterline = scheduler
        .discard_stale_pending_audio_before_output_resume(
            Some(audio_snapshot(ANCHOR_NSECS, 0)),
            0,
            0,
            ANCHOR_NSECS,
            44_100,
            2,
            PlaybackSessionId(1),
        )
        .expect("rebuffer audio waterline");

    assert_eq!(
        scheduler.pending_start_audio.first_start_timeline_nsecs(),
        Some(ANCHOR_NSECS)
    );
    assert_eq!(
        scheduler.pending_start_audio.buffered_duration().as_nanos(),
        u128::from(EFFECTIVE_COVERAGE_NSECS)
    );
    assert_eq!(
        waterline.decoded_audio_forward_nsecs,
        Some(EFFECTIVE_COVERAGE_NSECS)
    );
    assert!(!waterline.ready);
    assert!(waterline.below_target());
    assert!(!scheduler.output_wait_audio_input_backpressured());
    assert!(scheduler.audio_resume_waterline_below_input_suppression(
        Some(audio_snapshot(ANCHOR_NSECS, 0)),
        0,
        0,
        ANCHOR_NSECS,
    ));
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
