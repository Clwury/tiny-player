use std::time::Duration;

use crate::player::backend::ffmpeg::FfmpegControl;
use crate::player::render_host::PlaybackSessionId;

use super::super::super::DEFAULT_VIDEO_FRAME_DURATION_NSECS;
use super::super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioResumeWaterline,
    DecodedAudio, MAX_REBUFFER_AUDIO_LEAD_NSECS, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PendingAudioPressureContext,
    PendingStartAudioPressureLevel, PlaybackOutputScheduler, PlaybackOutputState,
    RebufferResumeAnchor, StaleRebufferPendingAudio, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
    playing_pending_audio_limit_duration, playing_pending_audio_pressure_clear_duration,
    stale_rebuffer_pending_audio, stale_rebuffer_pending_audio_ahead,
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
fn far_ahead_rebuffer_audio_requests_video_master_realign_after_repeated_drops() {
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
                PlaybackSessionId(1),
                "test_far_ahead",
            )
            .is_none()
    );
    let request = scheduler
        .observe_rebuffer_far_ahead_audio_frame(
            182_040_000_000,
            5_640_000_000,
            Some(0),
            PlaybackSessionId(1),
            "test_far_ahead",
        )
        .expect("third far-ahead drop requests realign");

    assert_eq!(request.target_timeline_nsecs, 5_680_000_000);
    assert_eq!(request.anchor_timeline_nsecs, 5_640_000_000);
    assert_eq!(request.first_video_timeline_nsecs, 5_680_000_000);
    assert_eq!(request.far_ahead_drop_count, 3);
    assert_eq!(
        scheduler
            .take_rebuffer_audio_realign_request()
            .map(|request| request.target_timeline_nsecs),
        Some(5_680_000_000)
    );
}

#[test]
fn reader_head_far_ahead_rebuffer_empty_audio_requests_realign_immediately() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 15_120_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_120_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(15_160_000_000));
    scheduler.set_rebuffer_empty_audio_output_blocked(true);

    let request = scheduler
        .request_rebuffer_audio_reader_head_realign_if_needed(
            16_127_979_167,
            AudioResumeWaterline {
                resume_timeline_nsecs: 15_120_000_000,
                target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                audio_output_pending_nsecs: Some(0),
                pending_audio_start_nsecs: Some(15_871_632_256),
                demux_audio_forward_nsecs: Some(208_000_000_000),
                ..AudioResumeWaterline::default()
            },
            14_216_734_626,
            PlaybackSessionId(1),
        )
        .expect("reader head far ahead requests immediate realign");

    assert_eq!(request.reason, "rebuffer_audio_reader_far_ahead");
    assert_eq!(request.target_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.anchor_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.first_video_timeline_nsecs, 15_120_000_000);
    assert_eq!(request.far_ahead_audio_timeline_nsecs, 16_127_979_167);
    assert!(request.far_ahead_drop_count < 3);
    assert_eq!(
        scheduler
            .take_rebuffer_audio_realign_request()
            .map(|request| request.target_timeline_nsecs),
        Some(15_120_000_000)
    );
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
fn audio_gap_recovery_does_not_suppress_rebuffer_when_video_queue_full_and_vo_empty() {
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

    assert!(scheduler.maybe_enter_video_output_rebuffer(
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
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Rebuffering);
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
fn demux_healthy_output_underflow_enters_decode_underfill_rebuffer() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);

    assert!(scheduler.maybe_enter_video_output_rebuffer(
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
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(snapshot.rebuffering);
    assert!(snapshot.video_decode_underfill);
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

    assert_eq!(waterline.resume_timeline_nsecs, first_video_nsecs);
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
fn startup_audio_resume_waterline_reports_low_water_before_first_video_queue() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 200_000_000,
        },
        1_000_000_000,
        1_200_000_000,
    );

    let waterline = scheduler
        .audio_resume_waterline_for_output_wait(
            Some(audio_snapshot(1_000_000_000, 0)),
            64_000_000,
            2,
            1_000_000_000,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            Some(3_000_000_000),
            Some(7),
        )
        .expect("startup audio waterline");

    assert!(scheduler.scheduled_video_queue.is_empty());
    assert!(waterline.below_target());
    assert_eq!(waterline.resume_timeline_nsecs, 1_000_000_000);
    assert_eq!(waterline.pending_audio_forward_nsecs, Some(200_000_000));
    assert_eq!(waterline.decoded_audio_forward_nsecs, Some(200_000_000));
    assert_eq!(waterline.audio_decode_queued_nsecs, 64_000_000);
    assert_eq!(waterline.audio_decode_in_flight_packets, 2);
    assert_eq!(waterline.demux_audio_forward_nsecs, Some(3_000_000_000));
    assert_eq!(waterline.demux_audio_cached_packets, Some(7));
}

#[test]
fn startup_audio_resume_waterline_waits_for_margin_before_input_suppression() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let suppression_threshold = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
        + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN);

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: suppression_threshold - 1,
        },
        1_000_000_000,
        1_000_000_000 + suppression_threshold - 1,
    );

    assert!(scheduler.scheduled_video_queue.is_empty());
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
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 200_000_000,
        },
        1_000_000_000,
        1_200_000_000,
    );

    assert!(scheduler.scheduled_video_queue.is_empty());
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
        scheduler.audio_far_ahead_reference_timeline_nsecs(5_000_000_000),
        5_000_000_000
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
        scheduler.audio_far_ahead_reference_timeline_nsecs(0),
        6_000_000_000
    );
    assert_eq!(
        scheduler.audio_far_ahead_reference_timeline_nsecs(7_000_000_000),
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
        scheduler.audio_far_ahead_reference_timeline_nsecs(0),
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
        scheduler.audio_far_ahead_reference_timeline_nsecs(0),
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
