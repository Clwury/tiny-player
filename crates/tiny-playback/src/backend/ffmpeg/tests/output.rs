use super::*;

#[test]
fn queued_video_window_expands_for_pgs_subtitle_prefetch() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_300_000_000));

    assert_eq!(
        queued_video_limit_duration(&queue, false),
        AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, false),
        AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
    assert_eq!(
        queued_video_limit_duration(&queue, true),
        PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, true),
        PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION
    );
}

#[test]
fn queued_video_window_caps_vulkan_decoded_frames() {
    let mut queue = VecDeque::new();
    queue.push_back(test_vulkan_queued_video_frame(1_000_000_000));
    queue.push_back(test_vulkan_queued_video_frame(1_300_000_000));

    assert_eq!(
        queued_video_limit_duration(&queue, false),
        VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, false),
        VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
    assert_eq!(
        queued_video_limit_frames(&queue, false),
        VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES
    );
    assert_eq!(
        queued_video_target_frames(&queue, false),
        VULKAN_DECODED_VIDEO_QUEUE_TARGET_FRAMES
    );

    assert_eq!(
        queued_video_limit_duration(&queue, true),
        VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, true),
        VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
}

#[test]
fn queued_video_limit_uses_frame_and_duration_caps() {
    let mut frame_limited = VecDeque::new();
    for index in 0..DECODED_VIDEO_QUEUE_LIMIT_FRAMES {
        frame_limited.push_back(test_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(queued_video_limit_reached(&frame_limited, false));

    let mut duration_limited = VecDeque::new();
    let duration_limit_nsecs = duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION);
    let frames_for_duration_limit = duration_limit_nsecs
        .saturating_add(DEFAULT_VIDEO_FRAME_DURATION_NSECS - 1)
        / DEFAULT_VIDEO_FRAME_DURATION_NSECS;
    for index in 0..frames_for_duration_limit {
        duration_limited.push_back(test_queued_video_frame(
            1_000_000_000 + index * DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        ));
    }
    assert!(queued_video_limit_reached(&duration_limited, false));

    let mut under_limit = VecDeque::new();
    under_limit.push_back(test_queued_video_frame(1_000_000_000));
    under_limit.push_back(test_queued_video_frame(1_200_000_000));
    assert!(!queued_video_limit_reached(&under_limit, false));
}

#[test]
fn queued_video_limit_keeps_decode_headroom_above_one_second() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(2_200_000_000));

    assert!(!queued_video_limit_reached(&queue, false));
}

#[test]
fn queued_video_limit_uses_vulkan_frame_cap() {
    let mut queue = VecDeque::new();
    for index in 0..VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES {
        queue.push_back(test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(queued_video_limit_reached(&queue, false));

    let mut under_limit = VecDeque::new();
    for index in 0..VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES - 1 {
        under_limit.push_back(test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(!queued_video_limit_reached(&under_limit, false));
}

#[test]
fn video_output_rebuffer_enters_after_underrun_grace() {
    let mut underrun_started_at = None;
    let now = Instant::now();
    let queued_video_still_buffered =
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) * 2);
    let demux_forward_empty = Some(0);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        false,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, Some(now));

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER - Duration::from_millis(1),
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Rebuffering,
    ));
    assert_eq!(underrun_started_at, Some(now));

    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        true,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        false,
        queued_video_still_buffered,
        false,
        true,
        demux_forward_empty,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());
}

#[test]
fn video_output_rebuffer_waits_for_demux_cache_insufficient() {
    let now = Instant::now();
    let mut underrun_started_at = Some(now);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        Some(0),
        false,
        false,
        None,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());
}

#[test]
fn healthy_demux_decode_underfill_does_not_enter_cache_rebuffer() {
    let now = Instant::now();
    let mut underrun_started_at = None;

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        None,
        true,
        false,
        None,
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, None);
}

#[test]
fn video_output_rebuffer_stays_out_while_pending_audio_can_recover_underrun() {
    let now = Instant::now();
    let mut underrun_started_at = None;

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        None,
        true,
        false,
        None,
        false,
        true,
        true,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, None);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER - Duration::from_millis(1),
        true,
        None,
        true,
        false,
        None,
        false,
        true,
        true,
        PlaybackOutputState::Playing,
    ));

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER + Duration::from_secs(5),
        true,
        None,
        true,
        false,
        None,
        false,
        true,
        true,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, None);
}

#[test]
fn video_output_rebuffer_keeps_wait_timer_while_rebuffering() {
    let now = Instant::now();
    let mut underrun_started_at = Some(now);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        false,
        None,
        false,
        true,
        Some(0),
        false,
        true,
        false,
        PlaybackOutputState::Rebuffering,
    ));
    assert_eq!(underrun_started_at, Some(now));
}

#[test]
fn video_output_rebuffer_enters_immediately_after_output_underrun() {
    let mut underrun_started_at = None;
    let now = Instant::now();

    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        None,
        true,
        true,
        Some(0),
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, Some(now));
}

#[test]
fn video_output_rebuffer_ignores_demux_empty_until_output_is_actually_low() {
    let now = Instant::now();
    let mut underrun_started_at = None;

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        false,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) / 2),
        false,
        true,
        Some(0),
        false,
        true,
        false,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, None);
}

#[test]
fn output_scheduler_does_not_rebuffer_on_demux_low_water_with_buffered_video() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    let now = Instant::now();

    assert!(!scheduler.maybe_enter_video_output_rebuffer(
        now,
        false,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) / 2),
        false,
        true,
        Some(0),
        false,
        0,
        true,
        false,
        &control,
        None,
        None,
        PlaybackSessionId(7),
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) / 2),
    ));

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!control.is_output_rebuffer_paused());
}

#[test]
fn demux_reader_ready_for_output_uses_combined_watermark() {
    let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    let ready = DemuxReaderWatermark {
        video_forward_nsecs: Some(target_nsecs),
        audio_forward_nsecs: Some(target_nsecs),
        selected_min_forward_nsecs: Some(target_nsecs),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(demux_reader_ready_for_output(ready, true));

    let paused_at_readahead = DemuxReaderWatermark {
        video_forward_nsecs: None,
        audio_forward_nsecs: None,
        selected_min_forward_nsecs: None,
        idle: true,
        ..ready
    };
    assert!(demux_reader_ready_for_output(paused_at_readahead, true));

    let shallow_audio = DemuxReaderWatermark {
        audio_forward_nsecs: Some(target_nsecs - 1),
        selected_min_forward_nsecs: Some(target_nsecs - 1),
        ..ready
    };
    assert!(!demux_reader_ready_for_output(shallow_audio, true));

    let video_underrun = DemuxReaderWatermark {
        video_underrun: true,
        ..ready
    };
    assert!(!demux_reader_ready_for_output(video_underrun, true));
}

#[test]
fn output_scheduler_enters_rebuffer_and_updates_first_frame_gate() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let started_at = Instant::now();
    scheduler.set_video_output_underrun_started_at_for_test(started_at);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));

    assert!(scheduler.maybe_enter_video_output_rebuffer(
        started_at + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) * 2),
        false,
        true,
        Some(0),
        false,
        0,
        true,
        false,
        &control,
        None,
        None,
        PlaybackSessionId(7),
        Some(100_000_000),
    ));

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(!snapshot.first_video_frame_pending);
    assert!(control.is_output_rebuffer_paused());
}

#[test]
fn output_scheduler_snapshot_reports_decoded_output_watermarks() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(
        1_000_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    ));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );

    let snapshot = scheduler.snapshot_for_played_until(Some(1_000_000_000));

    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.first_video_frame_pending);
    assert!(!snapshot.rebuffering);
    assert_eq!(snapshot.queued_video_frames, 2);
    assert_eq!(
        snapshot.queued_video_duration_nsecs,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
    );
    assert_eq!(
        snapshot.queued_video_coverage_nsecs,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
    );
    assert_eq!(
        snapshot.queued_video_range_span_nsecs,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
    );
    assert_eq!(
        snapshot.queued_video_range_nsecs,
        Some((
            1_000_000_000,
            1_000_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
        ))
    );
    assert_eq!(
        snapshot.queued_video_forward_nsecs,
        Some(DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2)
    );
    assert!(snapshot.video_output_low_water);
    assert_eq!(snapshot.pending_start_audio_frames, 1);
    assert_eq!(snapshot.pending_start_audio_nsecs, 20_000_000);
    assert!(!snapshot.waiting_for_demux());
}

#[test]
fn startup_snapshot_uses_contiguous_forward_instead_of_gap_coverage() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(
        1_000_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    ));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(2_000_000_000));

    let snapshot = scheduler.snapshot();

    assert!(snapshot.first_video_frame_pending);
    assert_eq!(
        snapshot.queued_video_coverage_nsecs,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS * 3
    );
    assert!(snapshot.queued_video_range_span_nsecs > snapshot.queued_video_coverage_nsecs);
    assert_eq!(
        snapshot.queued_video_contiguous_forward_nsecs,
        Some(DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2)
    );
    assert_eq!(
        snapshot.queued_video_bootstrap_forward_nsecs(),
        DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
    );
}

#[test]
fn output_scheduler_backpressures_large_pending_start_audio() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);

    assert!(!scheduler.pending_start_audio_backpressured());

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
    );

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_allows_one_playing_pending_audio_frame_below_steady_limit() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let duration_nsecs =
        duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION))
            - 1;
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs,
    );

    assert!(!scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_backpressures_playing_pending_audio_at_steady_limit() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let duration_nsecs =
        duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs,
    );

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_allows_only_one_playing_frame_to_cross_steady_limit() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let limit_nsecs =
        duration_nsecs(AUDIO_OUTPUT_DELAY_LIMIT.saturating_add(AUDIO_OUTPUT_VIDEO_LEAD_DURATION));
    let first_duration_nsecs = limit_nsecs - 1;
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: first_duration_nsecs,
        },
        1_000_000_000,
        1_000_000_000 + first_duration_nsecs,
    );

    assert!(!scheduler.pending_start_audio_backpressured());

    let second_start_nsecs = 1_000_000_000 + first_duration_nsecs;
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 40_000_000,
        },
        second_start_nsecs,
        second_start_nsecs + 40_000_000,
    );

    assert!(scheduler.snapshot().pending_start_audio_nsecs > limit_nsecs);
    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_backpressures_extreme_playing_pending_audio() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let duration_nsecs = duration_nsecs(Duration::from_secs(86));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs,
    );

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_backpressures_startup_audio_at_resume_waterline_before_first_video() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let startup_limit = VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
        .saturating_sub(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
        .min(PENDING_START_AUDIO_BACKPRESSURE_DURATION);

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(startup_limit) - 1,
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(startup_limit) - 1,
    );

    assert!(!scheduler.pending_start_audio_backpressured());

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1,
        },
        1_000_000_000 + duration_nsecs(startup_limit) - 1,
        1_000_000_000 + duration_nsecs(startup_limit),
    );

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_snapshot_keeps_coordinator_gate_decisions() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let start_snapshot = scheduler.snapshot_for_played_until(Some(1_000_000_000));

    assert_eq!(start_snapshot.state, PlaybackOutputState::Syncing);
    assert!(start_snapshot.first_video_frame_pending);
    assert!(!start_snapshot.waiting_for_demux());
    assert!(start_snapshot.should_wait_for_demux());

    scheduler.set_state(PlaybackOutputState::Playing);
    let playing_empty = scheduler.snapshot_for_played_until(Some(1_000_000_000));
    assert!(playing_empty.waiting_for_demux());
    assert!(playing_empty.underflowing());
    assert!(!playing_empty.should_wait_for_demux());

    scheduler.set_state(PlaybackOutputState::Rebuffering);
    let rebuffering = scheduler.snapshot_for_played_until(Some(1_000_000_000));
    assert!(rebuffering.rebuffering);
    assert!(rebuffering.should_wait_for_demux());
}

#[test]
fn output_scheduler_reset_clears_queued_output_state() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    control.set_output_rebuffer_paused(true);
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_underrun_started_at_for_test(Instant::now());
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 1_000_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });

    scheduler.reset(&control);

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.queued_video_frames, 0);
    assert_eq!(snapshot.pending_start_audio_frames, 0);
    assert_eq!(snapshot.state, PlaybackOutputState::Syncing);
    assert!(snapshot.first_video_frame_pending);
    assert!(!scheduler.video_output_underrun_started_for_test());
    assert!(snapshot.video_output_rebuffer_anchor.is_none());
    assert!(!control.is_output_rebuffer_paused());
}

#[test]
fn video_output_rebuffer_requires_stable_decoded_queue_target() {
    let short_queue = test_queued_video_frames_with_duration(1_000_000_000, 16, 40_000_000);

    assert!(!video_output_rebuffer_resume_reached(&short_queue, false));
    assert!(queued_video_target_reached(&short_queue, false));

    let duration_ready = test_queued_video_frames_with_duration(1_000_000_000, 25, 40_000_000);

    assert!(video_output_rebuffer_resume_reached(&duration_ready, false));
    assert!(queued_video_target_reached(&duration_ready, false));

    let mut frame_ready = VecDeque::new();
    for index in 0..26 {
        frame_ready.push_back(test_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 40_000_000,
        ));
    }

    assert!(video_output_rebuffer_resume_reached(&frame_ready, false));
    assert!(queued_video_target_reached(&frame_ready, false));
}

#[test]
fn video_output_rebuffer_resume_keeps_stable_floor_under_vulkan_resource_pressure() {
    let frame_duration_nsecs = 25_000_000;
    let mut queued = VecDeque::new();
    for index in 0..VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES.saturating_sub(2) {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let unpressured_duration = video_output_rebuffer_resume_duration(&queued, false);
    let pressured_duration =
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true);

    assert_eq!(unpressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    assert_eq!(pressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);

    let resume_timeline_nsecs = queued.front().unwrap().timeline_nsecs;
    let target_nsecs = duration_nsecs(pressured_duration);
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: target_nsecs,
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + target_nsecs,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(waterline.target_nsecs, target_nsecs);
    assert_eq!(
        waterline.decoded_output.video_forward_nsecs,
        Some(frame_duration_nsecs * u64::try_from(queued.len()).unwrap())
    );
    assert!(!waterline.ready());

    for index in queued.len()..40 {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(waterline.target_nsecs, target_nsecs);
    assert!(waterline.ready());
}

#[test]
fn video_output_rebuffer_resume_rejects_subsecond_resume_timeline_budget() {
    let first_timeline_nsecs = 18_550_000_000;
    let resume_timeline_nsecs = 18_551_651_321;
    let frame_duration_nsecs = 20_000_000;
    let mut queued = VecDeque::new();
    for index in 0..VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES.saturating_sub(2) {
        let mut frame = test_vulkan_queued_video_frame(
            first_timeline_nsecs + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let buffered_until_nsecs =
        first_timeline_nsecs + frame_duration_nsecs * u64::try_from(queued.len()).unwrap();
    let resume_budget_nsecs = buffered_until_nsecs - resume_timeline_nsecs;
    let front_budget_nsecs = duration_nsecs(
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true),
    );

    assert!(resume_budget_nsecs < front_budget_nsecs);

    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: resume_budget_nsecs,
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + resume_budget_nsecs,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(
        waterline.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        waterline.decoded_output.video_forward_nsecs,
        Some(resume_budget_nsecs)
    );
    assert!(!waterline.ready());
}

#[test]
fn video_output_rebuffer_resume_keeps_stable_floor_under_resource_pressure() {
    let frame_duration_nsecs = 50_000_000;
    let mut queued = VecDeque::new();
    let mut frame = test_vulkan_queued_video_frame(1_000_000_000);
    frame.duration_nsecs = frame_duration_nsecs;
    queued.push_back(frame);

    let pressured_duration =
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true);

    assert_eq!(pressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);

    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        1_000_000_000,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(
        waterline.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        waterline.decoded_output.video_forward_nsecs,
        Some(frame_duration_nsecs)
    );
    assert!(!waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(waterline),
        PlaybackBlockReason::DecodedVideoQueue
    );

    for index in 1..20 {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        1_000_000_000,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert!(waterline.ready());
}

#[test]
fn rebuffer_resume_fallback_waits_for_stable_target_before_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let before_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER - Duration::from_millis(1)),
    );

    assert_eq!(
        before_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        before_timeout.decoded_output.video_forward_nsecs,
        Some(800_000_000)
    );
    assert!(!before_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_rejects_subsecond_decoded_window_after_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(800_000_000)
    );
    assert!(!after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_accepts_low_water_video_after_audio_stall_timeout() {
    let resume_timeline_nsecs = 13_080_000_000;
    let decoded_video_forward_nsecs = 800_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 20, 40_000_000);
    let pending = test_pending_audio(resume_timeline_nsecs + 200_000_000, 1_024_000_000);
    let demux_after_stalled_release = ready_demux_watermark(720_000_000);
    let audio_output_buffered_until_nsecs = resume_timeline_nsecs + 71_995_464;

    let mut waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        demux_after_stalled_release,
        Some(audio_output_buffered_until_nsecs),
        false,
        true,
    );
    assert_eq!(
        waterline.decoded_output.video_forward_nsecs,
        Some(decoded_video_forward_nsecs)
    );
    assert_eq!(
        waterline.decoded_output.audio_forward_nsecs,
        Some(71_995_464)
    );
    assert!(!waterline.ready());

    // The output gate releases the demux side after the stalled timeout so the
    // decoder can keep moving; the prolonged-wait fallback must still be the
    // thing that decides when the output side can leave rebuffering.
    waterline.prefetch.ready = true;
    let at_standard_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );
    assert!(!at_standard_timeout.ready());

    let after_audio_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );

    assert_eq!(
        after_audio_timeout.target_nsecs,
        decoded_video_forward_nsecs
    );
    assert!(after_audio_timeout.ready());
    assert!(after_audio_timeout.decoded_output.video_ready);
    assert!(after_audio_timeout.decoded_output.audio_ready);
}

#[test]
fn rebuffer_audio_stall_fallback_wakes_and_resumes_after_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 7, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + 5_000_000_000,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));
    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    assert_eq!(
        waterline.decoded_output.video_forward_nsecs,
        Some(280_000_000)
    );
    assert!(!waterline.ready());

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler
        .set_video_output_underrun_started_at_for_test(Instant::now() - Duration::from_millis(500));
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: resume_timeline_nsecs,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    for frame in test_queued_video_frames_with_duration(resume_timeline_nsecs, 7, 40_000_000) {
        scheduler.push_decoded_video_for_test(frame);
    }
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        resume_timeline_nsecs + 5_000_000_000,
        resume_timeline_nsecs
            + 5_000_000_000
            + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    scheduler.set_rebuffer_empty_audio_output_blocked(true);
    let watchdog_delay = scheduler
        .rebuffer_empty_audio_output_watchdog_delay()
        .expect("empty audio rebuffer watchdog delay");
    assert!(watchdog_delay <= Duration::from_millis(100));
    assert!(watchdog_delay > Duration::ZERO);

    scheduler.set_video_output_underrun_started_at_for_test(
        Instant::now()
            - VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER
            - Duration::from_millis(1),
    );
    assert_eq!(
        scheduler.rebuffer_empty_audio_output_watchdog_delay(),
        Some(Duration::ZERO)
    );

    let before_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER - Duration::from_millis(1)),
    );
    assert!(!before_timeout.ready());

    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );
    assert!(after_timeout.ready());
    assert!(after_timeout.decoded_output.video_ready);
    assert!(after_timeout.decoded_output.audio_ready);
}

#[test]
fn cache_underrun_still_blocks_resume() {
    let first_video_nsecs = 88_120_000_000;
    let first_audio_nsecs = 89_685_337_825;
    let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    let queued = test_queued_video_frames_with_duration(first_video_nsecs, 39, 40_000_000);
    let pending = test_pending_audio(first_audio_nsecs, target_nsecs);
    let decision = rebuffer_audio_clock_resume_decision(
        &queued,
        &pending,
        88_067_923_112,
        None,
        Some(0),
        false,
    )
    .expect("empty audio output rebuffer decision");
    assert!(decision.allow_audio_gap_at_video_resume);

    let mut cache_underrun = ready_demux_watermark(target_nsecs);
    cache_underrun.underrun = true;
    let waterline = rebuffer_playback_resume_waterline_for_decision(
        &queued,
        &pending,
        decision,
        cache_underrun,
        audio_output_buffered_until_for_resume(decision, None),
        false,
        true,
        false,
    );

    assert!(waterline.decoded_output.video_ready);
    assert!(waterline.decoded_output.audio_ready);
    assert!(!waterline.prefetch.ready);
    assert!(!waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(waterline),
        PlaybackBlockReason::DemuxCache
    );

    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DemuxCache
    );
}

#[test]
fn rebuffer_resume_fallback_accepts_one_frame_short_decoded_window_when_audio_ready() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 24, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(960_000_000)
    );
    assert_eq!(after_timeout.target_nsecs, 960_000_000);
    assert!(after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_resumes_on_video_when_audio_stalls_past_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    // Plenty of decoded video ahead of the resume point.
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 13, 100_000_000);
    // Audio sits entirely behind the resume point, so it never covers it
    // (decoded_audio_forward = None): a structurally lagging / unavailable audio track.
    let pending = test_pending_audio(resume_timeline_nsecs - 500_000_000, 100_000_000);
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    assert!(!waterline.decoded_output.audio_ready);

    // At the standard stall timeout, audio is still not ready -> keep waiting for it.
    let at_standard = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );
    assert!(!at_standard.ready());

    // Past the longer audio-stall fallback, resume on the decoded-video window alone
    // rather than freezing forever waiting for audio that is not arriving.
    let after_audio_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );
    assert!(after_audio_timeout.ready());
    assert!(after_audio_timeout.decoded_output.video_ready);
}

#[test]
fn rebuffer_resume_fallback_waits_for_delayed_audio_start_safety_window_after_demux_recovers() {
    let first_video_nsecs = 11_800_000_000;
    let resume_timeline_nsecs = 11_815_079_780;
    let queued = test_queued_video_frames_with_duration(first_video_nsecs, 5, 40_000_000);
    let decoded_video_forward_nsecs = first_video_nsecs + 5 * 40_000_000 - resume_timeline_nsecs;
    assert!(decoded_video_forward_nsecs < duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION));
    assert!(decoded_video_forward_nsecs >= DEFAULT_VIDEO_FRAME_DURATION_NSECS);

    let pending = test_pending_audio(resume_timeline_nsecs, 1_900_000_000);
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(decoded_video_forward_nsecs)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn rebuffer_resume_fallback_rejects_short_window_that_audio_clock_would_consume() {
    let resume_timeline_nsecs = 9_120_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 7, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + 238_000_000,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(280_000_000)
    );
    assert!(!after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_waits_when_delayed_audio_start_consumes_low_water() {
    let resume_timeline_nsecs = 4_360_000_000;
    let delayed_audio_gap_nsecs = 344_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 13, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + delayed_audio_gap_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(520_000_000)
    );
    assert_eq!(
        after_timeout.decoded_output.delayed_audio_start_gap_nsecs,
        Some(delayed_audio_gap_nsecs)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn rebuffer_resume_fallback_accepts_delayed_audio_start_with_low_water_remaining() {
    let resume_timeline_nsecs = 4_360_000_000;
    let delayed_audio_gap_nsecs = 344_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 25, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + delayed_audio_gap_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_output.video_forward_nsecs,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION))
    );
    assert_eq!(
        after_timeout.decoded_output.delayed_audio_start_gap_nsecs,
        Some(delayed_audio_gap_nsecs)
    );
    assert!(after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_still_requires_ready_demux_reader() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let shallow_demux = ready_demux_watermark(500_000_000);

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        shallow_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn video_output_rebuffer_low_water_enters_before_queue_is_empty() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_200_000_000));

    assert!(video_output_rebuffer_low_water(&queued, 1_000_000_000));

    queued.push_back(test_queued_video_frame(1_400_000_000));

    assert!(!video_output_rebuffer_low_water(&queued, 1_000_000_000));
}

#[test]
fn video_decode_skips_nonref_frames_under_decode_pressure() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_800_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Syncing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        false,
        None,
        false,
    ));
    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Rebuffering,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
    assert!(video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));

    queued = test_queued_video_frames_with_duration(1_000_000_000, 25, 40_000_000);

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
}

#[test]
fn hevc_video_decode_does_not_skip_nonref_frames_under_low_water_pressure() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_800_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
}

#[test]
fn video_decode_skip_pressure_uses_short_vulkan_low_water() {
    let mut queued = test_vulkan_queued_video_frames_with_duration(1_000_000_000, 8, 40_000_000);

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));

    queued = test_vulkan_queued_video_frames_with_duration(1_000_000_000, 5, 40_000_000);

    assert!(video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
}

#[test]
fn video_decode_skip_pressure_uses_audio_low_water_for_vulkan_catchup() {
    let queued = test_vulkan_queued_video_frames_with_duration(1_000_000_000, 8, 40_000_000);

    assert!(video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        Some(duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION) - 1),
        false,
    ));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        Some(duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)),
        false,
    ));
}

#[test]
fn video_decode_skip_pressure_uses_vulkan_hysteresis_when_active() {
    let queued = test_vulkan_queued_video_frames_with_duration(1_000_000_000, 8, 40_000_000);

    assert!(!video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        false,
    ));
    assert!(video_decode_should_skip_nonref_for_pressure(
        ffi::AVCodecID::AV_CODEC_ID_H264,
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        None,
        true,
    ));
}

#[test]
fn push_queued_video_frame_keeps_timeline_order() {
    let mut queued = VecDeque::new();
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_080_000_000));
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_000_000_000));
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_040_000_000));

    let timeline = queued
        .iter()
        .map(|frame| frame.timeline_nsecs)
        .collect::<Vec<_>>();

    assert_eq!(timeline, vec![1_000_000_000, 1_040_000_000, 1_080_000_000]);
}

#[test]
fn decoded_video_start_requires_initial_prebuffer_waterline() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));

    assert!(!decoded_video_start_prebuffer_reached(&queued, false));

    queued = test_queued_video_frames_with_duration(1_000_000_000, 7, 40_000_000);

    assert!(decoded_video_start_prebuffer_reached(&queued, false));
}

#[test]
fn playback_resume_waterline_requires_decoded_audio_and_demux_streams() {
    let queued = test_queued_video_frames_with_duration(1_000_000_000, 25, 40_000_000);
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let ready_waterline =
        playback_resume_waterline(&queued, &pending, 1_000_000_000, ready_demux, false, true);
    assert!(ready_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(ready_waterline),
        PlaybackBlockReason::OutputGate
    );

    let missing_audio = PendingStartAudio::default();
    let missing_audio_waterline = playback_resume_waterline(
        &queued,
        &missing_audio,
        1_000_000_000,
        ready_demux,
        false,
        true,
    );
    assert!(!missing_audio_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(missing_audio_waterline),
        PlaybackBlockReason::DecodedAudioQueue
    );

    let shallow_demux = DemuxReaderWatermark {
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1),
        ..ready_demux
    };
    let shallow_demux_waterline =
        playback_resume_waterline(&queued, &pending, 1_000_000_000, shallow_demux, false, true);
    assert!(!shallow_demux_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(shallow_demux_waterline),
        PlaybackBlockReason::DemuxCache
    );
}

#[test]
fn playback_resume_waterline_uses_resume_timeline_for_audio_offset() {
    let resume_timeline_nsecs = 1_020_000_000;
    let queued = test_queued_video_frames_with_duration(1_000_000_000, 26, 40_000_000);
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        !playback_resume_waterline(&queued, &pending, 1_000_000_000, ready_demux, false, true)
            .ready()
    );
    assert!(
        playback_resume_waterline(
            &queued,
            &pending,
            resume_timeline_nsecs,
            ready_demux,
            false,
            true,
        )
        .ready()
    );
}

#[test]
fn playback_resume_waterline_tolerates_small_audio_timestamp_gaps() {
    let start_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(start_nsecs, 25, 40_000_000);
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 750_000_000,
        },
        start_nsecs,
        start_nsecs + 750_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 750_000_000,
        },
        start_nsecs + 751_000_000,
        start_nsecs + 1_501_000_000,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        playback_resume_waterline(&queued, &pending, start_nsecs, ready_demux, false, true).ready()
    );
}

#[test]
fn initial_resume_keeps_video_start_for_small_audio_offset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_004_000_000,
        1_024_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        initial_output_sync_decision(&queued, &pending, 1_000_000_000),
        Some(InitialOutputSyncDecision {
            video_resume_timeline_nsecs: 1_000_000_000,
            audio_start_timeline_nsecs: Some(1_004_000_000),
            delayed_audio_start_timeline_nsecs: None,
            drop_audio_before_timeline_nsecs: None,
            stale_audio_preroll_until_nsecs: None,
            stale_audio_preroll_gap_nsecs: None,
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        })
    );
    assert_eq!(
        initial_audio_clock_resume_decision(&queued, &pending, 1_000_000_000),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
}

#[test]
fn initial_resume_uses_video_start_and_delays_audio_for_large_audio_offset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        initial_output_sync_decision(&queued, &pending, 1_000_000_000),
        Some(InitialOutputSyncDecision {
            video_resume_timeline_nsecs: 1_000_000_000,
            audio_start_timeline_nsecs: Some(1_020_000_000),
            delayed_audio_start_timeline_nsecs: Some(1_020_000_000),
            drop_audio_before_timeline_nsecs: None,
            stale_audio_preroll_until_nsecs: None,
            stale_audio_preroll_gap_nsecs: None,
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        })
    );
    assert_eq!(
        initial_audio_clock_resume_decision(&queued, &pending, 1_000_000_000),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: Some(1_020_000_000),
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
}

#[test]
fn initial_resume_skips_small_stale_audio_preroll_gap_before_first_video() {
    let video_start_nsecs = 424_000_000_000;
    let audio_start_nsecs = 422_399_983_688;
    let audio_duration_nsecs = 1_535_986_356;
    let audio_buffered_until_nsecs = audio_start_nsecs + audio_duration_nsecs;
    let queued = test_queued_video_frames_with_duration(
        video_start_nsecs,
        31,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(audio_start_nsecs, audio_duration_nsecs);
    let sync_decision = initial_output_sync_decision(&queued, &pending, video_start_nsecs).unwrap();

    assert_eq!(
        sync_decision,
        InitialOutputSyncDecision {
            video_resume_timeline_nsecs: video_start_nsecs,
            audio_start_timeline_nsecs: Some(audio_start_nsecs),
            delayed_audio_start_timeline_nsecs: None,
            drop_audio_before_timeline_nsecs: Some(video_start_nsecs),
            stale_audio_preroll_until_nsecs: Some(audio_buffered_until_nsecs),
            stale_audio_preroll_gap_nsecs: Some(video_start_nsecs - audio_buffered_until_nsecs),
            allow_initial_audio_gap_at_video_start: true,
            reset_audio_to_video: false,
        }
    );

    let waterline = initial_playback_resume_waterline(
        &queued,
        &pending,
        sync_decision.video_resume_timeline_nsecs,
        sync_decision.delayed_audio_start_timeline_nsecs,
        sync_decision.allow_initial_audio_gap_at_video_start,
        ready_demux_watermark(12_000_000_000),
        false,
        true,
    );

    assert!(waterline.ready());
    assert!(waterline.decoded_output.video_ready);
    assert!(waterline.decoded_output.audio_ready);
    assert_eq!(waterline.decoded_output.audio_forward_nsecs, None);
    assert_eq!(waterline.decoded_output.delayed_audio_start_gap_nsecs, None);
}

#[test]
fn initial_resume_keeps_audio_preroll_that_covers_first_video() {
    let video_start_nsecs = 1_000_000_000;
    let audio_start_nsecs = 999_980_000;
    let audio_duration_nsecs = 80_000_000;
    let queued = VecDeque::from([test_queued_video_frame(video_start_nsecs)]);
    let pending = test_pending_audio(audio_start_nsecs, audio_duration_nsecs);

    assert_eq!(
        initial_output_sync_decision(&queued, &pending, video_start_nsecs),
        Some(InitialOutputSyncDecision {
            video_resume_timeline_nsecs: video_start_nsecs,
            audio_start_timeline_nsecs: Some(audio_start_nsecs),
            delayed_audio_start_timeline_nsecs: None,
            drop_audio_before_timeline_nsecs: None,
            stale_audio_preroll_until_nsecs: None,
            stale_audio_preroll_gap_nsecs: None,
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        })
    );
}

#[test]
fn initial_resume_drops_large_stale_audio_preroll_without_immediate_audio_ready() {
    let video_start_nsecs = 1_000_000_000;
    let audio_start_nsecs = 800_000_000;
    let audio_duration_nsecs = 50_000_000;
    let audio_buffered_until_nsecs = audio_start_nsecs + audio_duration_nsecs;
    let queued = test_queued_video_frames_with_duration(
        video_start_nsecs,
        31,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(audio_start_nsecs, audio_duration_nsecs);
    let sync_decision = initial_output_sync_decision(&queued, &pending, video_start_nsecs).unwrap();

    assert_eq!(
        sync_decision,
        InitialOutputSyncDecision {
            video_resume_timeline_nsecs: video_start_nsecs,
            audio_start_timeline_nsecs: Some(audio_start_nsecs),
            delayed_audio_start_timeline_nsecs: None,
            drop_audio_before_timeline_nsecs: Some(video_start_nsecs),
            stale_audio_preroll_until_nsecs: Some(audio_buffered_until_nsecs),
            stale_audio_preroll_gap_nsecs: Some(video_start_nsecs - audio_buffered_until_nsecs),
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        }
    );

    let waterline = initial_playback_resume_waterline(
        &queued,
        &pending,
        sync_decision.video_resume_timeline_nsecs,
        sync_decision.delayed_audio_start_timeline_nsecs,
        sync_decision.allow_initial_audio_gap_at_video_start,
        ready_demux_watermark(12_000_000_000),
        false,
        true,
    );

    assert!(waterline.decoded_output.video_ready);
    assert!(!waterline.decoded_output.audio_ready);
    assert!(!waterline.ready());
}

#[test]
fn rebuffer_resume_does_not_skip_stale_audio_preroll_gap() {
    let video_start_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(
        video_start_nsecs,
        31,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(800_000_000, 50_000_000);
    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        video_start_nsecs,
        ready_demux_watermark(12_000_000_000),
        None,
        false,
        true,
    );

    assert!(waterline.decoded_output.video_ready);
    assert!(!waterline.decoded_output.audio_ready);
    assert!(!waterline.ready());
}

#[test]
fn initial_waterline_delays_audio_without_waiting_for_video_at_audio_start() {
    let video_start_nsecs = 177_600_000_000;
    let audio_start_nsecs = 186_303_979_167;
    let queued = test_queued_video_frames_with_duration(
        video_start_nsecs,
        31,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(audio_start_nsecs, 1_514_671_164);
    let sync_decision = initial_output_sync_decision(&queued, &pending, video_start_nsecs).unwrap();

    assert_eq!(
        sync_decision,
        InitialOutputSyncDecision {
            video_resume_timeline_nsecs: video_start_nsecs,
            audio_start_timeline_nsecs: Some(audio_start_nsecs),
            delayed_audio_start_timeline_nsecs: Some(audio_start_nsecs),
            drop_audio_before_timeline_nsecs: None,
            stale_audio_preroll_until_nsecs: None,
            stale_audio_preroll_gap_nsecs: None,
            allow_initial_audio_gap_at_video_start: false,
            reset_audio_to_video: false,
        }
    );

    let waterline = initial_playback_resume_waterline(
        &queued,
        &pending,
        sync_decision.video_resume_timeline_nsecs,
        sync_decision.delayed_audio_start_timeline_nsecs,
        sync_decision.allow_initial_audio_gap_at_video_start,
        ready_demux_watermark(12_000_000_000),
        false,
        true,
    );

    assert!(waterline.ready());
    assert!(waterline.decoded_output.video_ready);
    assert!(waterline.decoded_output.audio_ready);
    assert_eq!(
        waterline.decoded_output.delayed_audio_start_gap_nsecs,
        Some(audio_start_nsecs - video_start_nsecs)
    );
    assert!(
        waterline.decoded_output.video_forward_nsecs.unwrap()
            < audio_start_nsecs - video_start_nsecs
    );
}

#[test]
fn rebuffer_resume_sync_uses_current_audio_and_pending_audio() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_400_000_000,
        1_420_000_000,
    );
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(40_000_000));
    queued.push_back(test_queued_video_frame(80_000_000));

    assert_eq!(
        audio_clock_resume_timeline_nsecs(&queued, &pending, 1_300_000_000),
        Some(1_400_000_000)
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_400_000_000), 2);
    assert!(queued.is_empty());
}

#[test]
fn rebuffer_resume_preserves_video_when_output_audio_covers_pending_gap() {
    let resume_timeline_nsecs = 1_010_000_000;
    let output_audio_until_nsecs = 1_500_000_000;
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 650_000_000,
        },
        output_audio_until_nsecs,
        2_150_000_000,
    );
    let mut queued = test_queued_video_frames_with_duration(1_000_000_000, 27, 40_000_000);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            resume_timeline_nsecs,
            Some(output_audio_until_nsecs),
            Some(output_audio_until_nsecs.saturating_sub(resume_timeline_nsecs)),
            false,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: resume_timeline_nsecs,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
    assert_eq!(
        discard_queued_video_before(&mut queued, resume_timeline_nsecs),
        0
    );
    assert_eq!(
        decoded_audio_forward_nsecs_from(
            &pending,
            resume_timeline_nsecs,
            Some(output_audio_until_nsecs),
        ),
        Some(1_140_000_000)
    );

    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        rebuffer_playback_resume_waterline(
            &queued,
            &pending,
            resume_timeline_nsecs,
            ready_demux,
            Some(output_audio_until_nsecs),
            false,
            true,
        )
        .ready()
    );
}

#[test]
fn rebuffer_resume_uses_pending_audio_when_output_audio_is_missing() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_400_000_000,
        1_420_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(&queued, &pending, 1_010_000_000, None, None, false),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_400_000_000,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Audio,
        })
    );
}

#[test]
fn rebuffer_resume_keeps_video_anchor_when_audio_starts_far_after_video_resume() {
    let first_video_nsecs = 67_000_000;
    let played_until_nsecs = 33_000_000;
    let first_audio_nsecs = 6_676_961_284;
    let queued = test_queued_video_frames_with_duration(
        first_video_nsecs,
        36,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(first_audio_nsecs, 1_152_000_000);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            played_until_nsecs,
            None,
            Some(0),
            true,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: first_video_nsecs,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: true,
            resume_anchor_source: ResumeAnchorSource::AudioGapReset,
        })
    );
}

#[test]
fn rebuffer_resume_waterline_allows_video_first_when_audio_gap_is_explicit() {
    let video_resume_nsecs = 67_000_000;
    let first_audio_nsecs = 6_676_961_284;
    let queued = test_queued_video_frames_with_duration(
        video_resume_nsecs,
        36,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );
    let pending = test_pending_audio(first_audio_nsecs, 1_152_000_000);
    let decision = AudioClockResumeDecision {
        timeline_nsecs: video_resume_nsecs,
        reset_audio_to_video: true,
        delayed_audio_start_timeline_nsecs: Some(first_audio_nsecs),
        allow_audio_gap_at_video_resume: true,
        resume_anchor_source: ResumeAnchorSource::AudioGapReset,
    };

    let waterline = rebuffer_playback_resume_waterline_for_decision(
        &queued,
        &pending,
        decision,
        ready_demux_watermark(12_000_000_000),
        audio_output_buffered_until_for_resume(decision, None),
        false,
        true,
        false,
    );

    assert!(waterline.ready());
    assert!(waterline.decoded_output.video_ready);
    assert!(waterline.decoded_output.audio_ready);
    assert_eq!(waterline.decoded_output.audio_forward_nsecs, None);
    assert_eq!(
        waterline.decoded_output.delayed_audio_start_gap_nsecs,
        Some(first_audio_nsecs - video_resume_nsecs)
    );
    assert!(waterline.allow_audio_gap_at_video_resume);
    assert_eq!(
        waterline.resume_anchor_source,
        ResumeAnchorSource::AudioGapReset
    );
    assert_eq!(
        waterline
            .audio_resume_waterline
            .map(|audio_waterline| audio_waterline.ready),
        Some(false)
    );
}

#[test]
fn rebuffer_audio_gap_does_not_suppress_audio_input_until_contiguous_waterline() {
    let video_resume_nsecs = 67_000_000;
    let first_audio_nsecs = 6_676_961_284;
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 33_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: true,
    });
    for frame in test_queued_video_frames_with_duration(
        video_resume_nsecs,
        36,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    ) {
        scheduler.push_decoded_video_for_test(frame);
    }
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 1_152_000_000,
        },
        first_audio_nsecs,
        first_audio_nsecs + 1_152_000_000,
    );

    assert!(scheduler.audio_resume_waterline_below_input_suppression(
        Some(test_audio_snapshot(33_000_000, 0)),
        0,
        0,
        33_000_000,
    ));
}

#[test]
fn rebuffer_resume_resets_to_decoded_video_when_anchor_is_uncovered() {
    let pending = PendingStartAudio::default();
    let mut queued = VecDeque::from([
        test_queued_video_frame(1_000_000_000),
        test_queued_video_frame(1_040_000_000),
    ]);

    let decision = rebuffer_audio_clock_resume_decision(
        &queued,
        &pending,
        1_400_000_000,
        Some(1_900_000_000),
        Some(500_000_000),
        true,
    );

    assert_eq!(
        decision,
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_000_000_000), 0);
    assert_eq!(
        audio_output_buffered_until_for_resume(decision.unwrap(), Some(1_900_000_000)),
        None
    );
}

#[test]
fn rebuffer_resume_keeps_anchor_when_decoded_video_covers_it() {
    let pending = PendingStartAudio::default();
    let queued = test_queued_video_frames_with_duration(
        1_360_000_000,
        30,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );

    assert_eq!(
        rebuffer_audio_clock_resume_decision(&queued, &pending, 1_370_000_000, None, None, true),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_370_000_000,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
}

#[test]
fn rebuffer_resume_resets_to_video_when_anchor_window_is_short() {
    let pending = test_pending_audio(1_370_000_000, 900_000_000);
    let queued = VecDeque::from([
        test_queued_video_frame(1_240_000_000),
        test_queued_video_frame(1_280_000_000),
        test_queued_video_frame(1_320_000_000),
        test_queued_video_frame(1_360_000_000),
        test_queued_video_frame(1_400_000_000),
    ]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            1_370_000_000,
            Some(1_370_000_000),
            Some(0),
            true,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_240_000_000,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
    assert_eq!(
        decoded_audio_forward_nsecs_from(&pending, 1_240_000_000, None),
        None
    );
}

#[test]
fn rebuffer_resume_waterline_accepts_delayed_audio_start_after_video_reset() {
    let pending = test_pending_audio(1_370_000_000, 900_000_000);
    let queued = test_queued_video_frames_with_duration(
        1_240_000_000,
        35,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        1_240_000_000,
        ready_demux_watermark(1_500_000_000),
        None,
        false,
        true,
    );

    assert!(waterline.ready());
    assert_eq!(
        waterline.decoded_output.audio_forward_nsecs,
        Some(1_030_000_000)
    );
}

#[test]
fn rebuffer_resume_resets_when_output_audio_runs_past_decoded_video() {
    let pending = PendingStartAudio::default();
    let queued = VecDeque::from([
        test_queued_video_frame(3_440_000_000),
        test_queued_video_frame(3_480_000_000),
    ]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            3_441_636_173,
            Some(3_940_000_000),
            Some(498_363_827),
            true,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 3_440_000_000,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
}

#[test]
fn rebuffer_resume_keeps_video_frame_overlapping_sync_point() {
    let pending = PendingStartAudio::default();
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_040_000_000));

    assert_eq!(
        audio_clock_resume_timeline_nsecs(&queued, &pending, 1_010_000_000),
        Some(1_010_000_000)
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_010_000_000), 0);
    assert_eq!(
        queued.front().map(|frame| frame.timeline_nsecs),
        Some(1_000_000_000)
    );
}

#[test]
fn rebuffer_resume_resets_audio_clock_when_audio_is_far_ahead() {
    let pending = PendingStartAudio::default();
    let queued = VecDeque::from([
        test_queued_video_frame(1_000_000_000),
        test_queued_video_frame(1_040_000_000),
    ]);

    assert_eq!(
        audio_clock_resume_decision(&queued, &pending, 1_700_000_001),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: ResumeAnchorSource::Video,
        })
    );
}

#[test]
fn queued_video_buffered_until_uses_last_frame_end() {
    let mut queued = VecDeque::new();
    assert_eq!(queued_video_buffered_until_nsecs(&queued), None);

    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_040_000_000));

    assert_eq!(
        queued_video_buffered_until_nsecs(&queued),
        Some(1_040_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS)
    );
}

#[test]
fn demux_read_blocks_until_output_gate_finishes_rebuffering() {
    assert!(should_block_for_demux_read(PlaybackOutputState::Syncing));
    assert!(!should_block_for_demux_read(PlaybackOutputState::Playing));
    assert!(should_block_for_demux_read(
        PlaybackOutputState::Rebuffering
    ));
}

#[test]
fn audio_clock_video_frames_are_ready_with_small_present_lead() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));

    assert!(!queued_video_frame_ready_for_audio_clock(
        &queue,
        984_000_000
    ));
    assert!(queued_video_frame_ready_for_audio_clock(
        &queue,
        985_000_000
    ));
}

#[test]
fn audio_clock_video_wait_duration_tracks_present_deadline() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));

    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 984_000_000),
        Some(Duration::from_millis(1))
    );
    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 985_000_000),
        Some(Duration::ZERO)
    );
    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 1_010_000_000),
        Some(Duration::ZERO)
    );
}

#[test]
fn audio_clock_video_pop_only_advances_one_early_frame() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));

    let frame = pop_audio_clocked_video_frame(&mut queue, 985_000_000).unwrap();

    assert_eq!(frame.timeline_nsecs, 1_000_000_000);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_010_000_000);
}

#[test]
fn audio_clock_video_pop_catches_up_to_latest_overdue_frame() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));

    let frame = pop_audio_clocked_video_frame(&mut queue, 1_015_000_000).unwrap();

    assert_eq!(frame.timeline_nsecs, 1_010_000_000);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn audio_clock_present_uses_snapshot_timeline_without_reading_output_clock() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));
    let vo_queue = VideoOutputQueue::default();
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let (event_tx, _event_rx) = mpsc::channel();

    let pop_result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_015_000_000);
    if let Some(frame) = pop_result.frame {
        admit_decoded_video_frame_to_vo(
            frame.frame,
            PlaybackSessionId::default(),
            frame.timeline_nsecs,
            &vo_queue,
            &frame_presented,
            &mut position_reporter,
            &event_tx,
        );
    }

    assert!(frame_presented.load(Ordering::Relaxed));
    assert_eq!(vo_queue.snapshot().queued_frames, 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn pgs_subtitle_cues_rebase_when_dynamic_playback_origin_appears() {
    let mut cues = VecDeque::from([BackendSubtitleCue {
        text: "subtitle".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: 180_305_000_000,
        end_nsecs: 184_305_000_000,
    }]);

    rebase_subtitle_cues_to_timeline_origin(&mut cues, None, Some(1_168_000_000));

    assert_eq!(cues[0].start_nsecs, 179_137_000_000);
    assert_eq!(cues[0].end_nsecs, 183_137_000_000);
}

#[test]
fn pgs_subtitle_clear_marker_trims_previous_bitmap_cue() {
    let mut cues = VecDeque::from([
        BackendSubtitleCue {
            text: "first".to_string(),
            bitmaps: Vec::new(),
            start_nsecs: 10_000_000_000,
            end_nsecs: 14_000_000_000,
        },
        BackendSubtitleCue {
            text: "second".to_string(),
            bitmaps: Vec::new(),
            start_nsecs: 15_000_000_000,
            end_nsecs: 17_000_000_000,
        },
    ]);

    trim_overlapping_subtitle_cues_at(&mut cues, 12_000_000_000);

    assert_eq!(cues.len(), 2);
    assert_eq!(cues[0].start_nsecs, 10_000_000_000);
    assert_eq!(cues[0].end_nsecs, 12_000_000_000);
    assert_eq!(cues[1].start_nsecs, 15_000_000_000);
    assert_eq!(cues[1].end_nsecs, 17_000_000_000);
}

#[test]
fn pgs_subtitle_replacement_trims_previous_cue_at_next_start() {
    let mut cues = VecDeque::from([BackendSubtitleCue {
        text: "first".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: 10_000_000_000,
        end_nsecs: 14_000_000_000,
    }]);
    let next_cue_start = 11_500_000_000;

    trim_overlapping_subtitle_cues_at(&mut cues, next_cue_start);

    cues.push_back(BackendSubtitleCue {
        text: "second".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: next_cue_start,
        end_nsecs: 13_000_000_000,
    });

    assert_eq!(cues[0].start_nsecs, 10_000_000_000);
    assert_eq!(cues[0].end_nsecs, next_cue_start);
    assert_eq!(cues[1].start_nsecs, next_cue_start);
    assert_eq!(cues[1].end_nsecs, 13_000_000_000);
}

#[test]
fn late_video_drop_waits_for_grace_after_frame_end() {
    assert!(!should_drop_late_video_frame(
        1_000_000_000,
        16_000_000,
        1_090_000_000
    ));
    assert!(should_drop_late_video_frame(
        1_000_000_000,
        16_000_000,
        1_091_000_000
    ));
}

#[test]
fn audio_clock_vo_admission_drops_single_late_frame() {
    let mut frame = test_queued_video_frame(1_000_000_000);
    frame.duration_nsecs = 16_000_000;
    let mut queue = VecDeque::from([frame]);

    let result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_091_000_000);

    assert!(result.frame.is_none());
    assert_eq!(result.dropped_frames, 1);
    assert!(queue.is_empty());
}

#[test]
fn audio_clock_vo_admission_reports_superseded_due_frames() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));

    let result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_015_000_000);

    assert_eq!(result.frame.unwrap().timeline_nsecs, 1_010_000_000);
    assert_eq!(result.dropped_frames, 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn video_frame_corruption_detection_uses_flags_and_decode_errors() {
    assert!(!frame_is_corrupt(std::ptr::null_mut()));
    assert_eq!(frame_decode_error_flags(std::ptr::null_mut()), 0);

    let mut frame = AvFrame::new().expect("frame allocates");
    assert!(!frame_is_corrupt(frame.as_mut_ptr()));

    unsafe {
        (*frame.as_mut_ptr()).flags = ffi::AV_FRAME_FLAG_CORRUPT;
    }
    assert!(frame_is_corrupt(frame.as_mut_ptr()));

    unsafe {
        (*frame.as_mut_ptr()).flags = 0;
        (*frame.as_mut_ptr()).decode_error_flags = ffi::FF_DECODE_ERROR_MISSING_REFERENCE;
    }
    assert!(frame_is_corrupt(frame.as_mut_ptr()));
    assert_eq!(
        frame_decode_error_flags(frame.as_mut_ptr()),
        ffi::FF_DECODE_ERROR_MISSING_REFERENCE
    );
}
