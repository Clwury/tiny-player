use super::super::{
    AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER, AUDIO_OUTPUT_ACTIVITY_STALL_AFTER,
    AUDIO_OUTPUT_STEADY_TARGET_DURATION, AudioClockMode, AudioOutput, AudioOutputActivitySnapshot,
    AudioOutputActivityWatchdogAction, AudioOutputStableSnapshot, AudioOutputUnstableSnapshot,
    AudioStageCheckpoint, BufferedReporter, DecodedAudio, DecodedAudioAdmission,
    DelayedAudioStartSilencePolicy, INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL,
    INITIAL_AUDIO_START_MIN_AMMUNITION, InitialAudioAmmunitionSnapshot,
    InitialAudioCommitCheckpoint, InitialAudioDeferObservation, InitialAudioNoPayloadDisposition,
    InitialAudioPreparePhase, InitialAudioPrepareToken, InitialAudioStartAction,
    InitialAudioTransientRetry, InitialAvStartDecision, InitialStartAdmission,
    InitialStartAdmissionInput, InitialSyncLogDecision, InitialSyncLogObservation,
    OutputGateResumeStatus, OutputServiceDemand, PlaybackOutputScheduler, PlaybackOutputState,
    PlaybackScheduler, PositionReporter, PrestartAudioOwnership, PrestartAudioOwnershipInput,
    SubtitlePipeline, abort_initial_audio_stage_for_test,
    abort_initial_av_start_for_discontinuity_change, audio_output_contiguous_start_timeline_nsecs,
    audio_output_flush_until_timeline_nsecs, classify_prestart_audio_ownership,
    commit_initial_audio_stage_with_checkpoints_for_test, commit_initial_av_start, duration_nsecs,
    expire_initial_av_start_hard_deadline, fail_initial_av_start_after_unstable_snapshot_deadline,
    initial_audio_clock_reset_required, initial_audio_no_payload_disposition,
    initial_audio_start_action, initial_audio_start_ammunition_ready, initial_start_admission,
    release_initial_seek_transition_after_clock_reset,
    service_initial_video_clock_until_audio_start, stage_pending_audio_with_checkpoint,
};
use super::{audio_snapshot, test_queued_video_frame};
use crate::player::backend::ffmpeg::{
    AudioOutputDecision, AudioOutputLifecycle, AudioOutputPushResult, FfmpegControl,
};
use crate::player::backend::{BackendEvent, BackendEventKind};
use crate::player::render_host::{PlaybackSessionId, VideoOutputQueue};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[test]
fn only_would_block_makes_an_empty_initial_audio_stage_retryable() {
    assert_eq!(
        initial_audio_no_payload_disposition(true),
        InitialAudioNoPayloadDisposition::RetryTransient
    );
    assert_eq!(
        initial_audio_no_payload_disposition(false),
        InitialAudioNoPayloadDisposition::RebufferTerminal
    );
}

#[test]
fn audio_output_flush_until_caps_total_pending_audio() {
    let snapshot = audio_snapshot(10_000_000_000, 0);
    let video_lead_until = 12_000_000_000;

    assert_eq!(
        audio_output_flush_until_timeline_nsecs(
            snapshot,
            video_lead_until,
            AUDIO_OUTPUT_STEADY_TARGET_DURATION
        ),
        10_000_000_000 + duration_nsecs(AUDIO_OUTPUT_STEADY_TARGET_DURATION)
    );
}
#[test]
fn audio_output_flush_until_stops_when_output_already_past_limit() {
    let snapshot = audio_snapshot(
        10_000_000_000,
        duration_nsecs(AUDIO_OUTPUT_STEADY_TARGET_DURATION) + 1,
    );
    let video_lead_until = 12_000_000_000;

    assert!(
        audio_output_flush_until_timeline_nsecs(
            snapshot,
            video_lead_until,
            AUDIO_OUTPUT_STEADY_TARGET_DURATION
        ) < audio_output_contiguous_start_timeline_nsecs(snapshot)
    );
}
#[test]
fn first_queued_frame_closes_input_demand_before_initial_av_commit() {
    let mut scheduler = PlaybackOutputScheduler::new();
    assert!(scheduler.snapshot().first_frame_needed);

    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    let snapshot = scheduler.snapshot();
    assert!(!snapshot.first_frame_needed);
    assert!(snapshot.initial_av_start_pending);
    assert!(!snapshot.first_frame_presented);
}

#[test]
fn initial_audio_start_requires_real_contiguous_audio_ammunition() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let audio_start_timeline_nsecs = 845_856_000_000;
    assert!(!initial_audio_start_ammunition_ready(
        &scheduler,
        None,
        audio_start_timeline_nsecs
    ));

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 1_920],
            duration_nsecs: 20_000_000,
        },
        audio_start_timeline_nsecs,
        audio_start_timeline_nsecs + 20_000_000,
    );
    assert!(initial_audio_start_ammunition_ready(
        &scheduler,
        None,
        audio_start_timeline_nsecs
    ));
}

#[test]
fn log_derived_split_ledgers_commit_without_clearing_device_head() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let video_anchor_nsecs = 986_433_000_000;
    let audio_start_timeline_nsecs: u64 = 986_453_000_000;
    let device_audio_nsecs: u64 = 592_100_000;
    let device_buffered_until_nsecs = audio_start_timeline_nsecs.saturating_add(device_audio_nsecs);
    let pending_audio_nsecs = 1_509_300_000;
    let decoded_audio_nsecs = 1_300_300_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(video_anchor_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: pending_audio_nsecs,
        },
        device_buffered_until_nsecs,
        device_buffered_until_nsecs.saturating_add(pending_audio_nsecs),
    );
    let device_snapshot = audio_snapshot(audio_start_timeline_nsecs, device_audio_nsecs);
    let ammunition = InitialAudioAmmunitionSnapshot::from_ledgers(
        &scheduler,
        Some(device_snapshot),
        decoded_audio_nsecs,
        audio_start_timeline_nsecs,
    );
    let started_at = Instant::now();
    let transaction = scheduler.begin_initial_av_start_transaction(
        video_anchor_nsecs,
        audio_start_timeline_nsecs,
        started_at,
    );
    scheduler.mark_first_frame_presented();
    let commit_at = started_at + Duration::from_millis(100);

    assert!(initial_audio_start_ammunition_ready(
        &scheduler,
        Some(device_snapshot),
        audio_start_timeline_nsecs,
    ));
    assert_eq!(ammunition.pending_audio_nsecs, pending_audio_nsecs);
    assert_eq!(ammunition.device_audio_nsecs, device_audio_nsecs);
    assert_eq!(ammunition.decoded_audio_nsecs, decoded_audio_nsecs);
    assert_eq!(ammunition.total_audio_nsecs, 3_401_700_000);
    assert_eq!(
        initial_audio_start_action(
            scheduler
                .initial_av_start_transaction()
                .unwrap()
                .decision(commit_at, true),
            ammunition,
        ),
        InitialAudioStartAction::CommitCovered
    );
    assert!(commit_at.duration_since(started_at) <= Duration::from_millis(500));
    assert!(commit_at < transaction.hard_deadline_at);
    assert!(ammunition.device_covers_target());
    assert!(
        !initial_audio_clock_reset_required(ammunition),
        "device audio counted as ammunition must survive commit"
    );
}

#[test]
fn covered_audio_commits_well_before_the_500ms_regression_bound() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 10_000_000_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(target_nsecs));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        target_nsecs,
        target_nsecs + 20_000_000,
    );
    let started_at = Instant::now();
    scheduler.begin_initial_av_start_transaction(target_nsecs, target_nsecs, started_at);
    scheduler.mark_first_frame_presented();
    let commit_at = started_at + Duration::from_millis(100);
    let transaction = scheduler.initial_av_start_transaction().unwrap();
    let ammunition =
        InitialAudioAmmunitionSnapshot::from_ledgers(&scheduler, None, 0, target_nsecs);

    assert_eq!(
        initial_audio_start_action(transaction.decision(commit_at, true), ammunition),
        InitialAudioStartAction::CommitCovered
    );
    assert!(commit_at.duration_since(started_at) <= Duration::from_millis(500));
}

#[test]
fn hard_deadline_with_200ms_never_commits_degraded_audio() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 20_000_000_000;
    let delayed_start_nsecs = target_nsecs + 500_000_000;
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 100_000_000,
        },
        delayed_start_nsecs,
        delayed_start_nsecs + 100_000_000,
    );
    let ammunition = InitialAudioAmmunitionSnapshot::from_ledgers(
        &scheduler,
        Some(audio_snapshot(target_nsecs, 0)),
        100_000_000,
        target_nsecs,
    );

    assert!(!ammunition.covers_target());
    assert_eq!(
        ammunition.total_audio_nsecs,
        duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION)
    );
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, ammunition),
        InitialAudioStartAction::FailNoAmmunition
    );
    assert_eq!(
        delayed_start_nsecs.saturating_sub(target_nsecs),
        500_000_000
    );
}

#[test]
fn defer_action_proves_combined_audio_is_below_ammunition_threshold() {
    let scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 30_000_000_000;
    let ammunition = InitialAudioAmmunitionSnapshot::from_ledgers(
        &scheduler,
        Some(audio_snapshot(target_nsecs, 0)),
        duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION) - 1,
        target_nsecs,
    );

    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Commit, ammunition),
        InitialAudioStartAction::DeferBelowThreshold
    );
    assert!(ammunition.total_audio_nsecs < duration_nsecs(INITIAL_AUDIO_START_MIN_AMMUNITION));
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, ammunition),
        InitialAudioStartAction::FailNoAmmunition
    );
}

#[test]
fn hard_deadline_is_terminal_even_when_decode_ledger_is_incomplete() {
    let scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 31_000_000_000;
    let incomplete = InitialAudioAmmunitionSnapshot::from_optional_ledgers(
        &scheduler,
        Some(audio_snapshot(target_nsecs, 0)),
        None,
        target_nsecs,
    );
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, incomplete),
        InitialAudioStartAction::FailNoAmmunition
    );
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Commit, incomplete),
        InitialAudioStartAction::WaitingForCompleteLedger
    );

    let complete = InitialAudioAmmunitionSnapshot::from_ledgers(
        &scheduler,
        Some(audio_snapshot(target_nsecs, 0)),
        0,
        target_nsecs,
    );
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, complete),
        InitialAudioStartAction::FailNoAmmunition
    );
}

#[test]
fn initial_clock_reset_completion_unconditionally_releases_seek_transition() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let generation = control.request_seek();
    control.finish_seek(generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);

    assert!(release_initial_seek_transition_after_clock_reset(&control));
    assert!(!control.is_seek_audio_paused());
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Ready
    );
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Silence,
        "Ready must retain buffered audio until the transaction enters Playing"
    );

    let mut scheduler = PlaybackOutputScheduler::new();
    assert_eq!(
        commit_initial_av_start(&mut scheduler, &control),
        super::super::OutputGateResumeStatus::Resumed
    );
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Playing);
    assert!(!control.is_seek_audio_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn playing_audio_activity_watchdog_releases_seek_then_runs_one_bounded_reanchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let started_at = Instant::now();
    let frozen = AudioOutputActivitySnapshot {
        played_timeline_nsecs: 846_233_000_000,
        shared_buffer_pending_nsecs: 92_879_818,
        callback_count: 1,
        consumed_callback_count: 0,
        silenced_callback_count: 1,
        underrun_count: 0,
    };

    assert_eq!(
        scheduler.observe_audio_output_activity(started_at, frozen, true, true),
        None
    );
    assert_eq!(
        scheduler.observe_audio_output_activity(
            started_at + AUDIO_OUTPUT_ACTIVITY_STALL_AFTER - Duration::from_nanos(1),
            frozen,
            true,
            true,
        ),
        None
    );
    let release = scheduler
        .observe_audio_output_activity(
            started_at + AUDIO_OUTPUT_ACTIVITY_STALL_AFTER,
            AudioOutputActivitySnapshot {
                callback_count: 25,
                silenced_callback_count: 25,
                ..frozen
            },
            true,
            true,
        )
        .expect("seek transition release");
    assert_eq!(
        release.action,
        AudioOutputActivityWatchdogAction::ReleaseSeekTransition
    );
    assert!(scheduler.audio_output_clock_stall_fallback_active());

    let recovery = scheduler
        .observe_audio_output_activity(
            started_at + AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER,
            AudioOutputActivitySnapshot {
                callback_count: 49,
                silenced_callback_count: 49,
                ..frozen
            },
            true,
            false,
        )
        .expect("bounded re-anchor");
    assert_eq!(
        recovery.action,
        AudioOutputActivityWatchdogAction::RecoverAndReanchor
    );
    assert_eq!(
        scheduler.observe_audio_output_activity(
            started_at + AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER + Duration::from_millis(50),
            AudioOutputActivitySnapshot {
                callback_count: 50,
                silenced_callback_count: 49,
                consumed_callback_count: 1,
                played_timeline_nsecs: frozen.played_timeline_nsecs + 20_000_000,
                ..frozen
            },
            true,
            false,
        ),
        None
    );
    assert!(!scheduler.audio_output_clock_stall_fallback_active());
}

#[test]
fn user_cache_or_rebuffer_pause_disarms_playing_audio_watchdog() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let now = Instant::now();
    let frozen = AudioOutputActivitySnapshot {
        played_timeline_nsecs: 1_000_000_000,
        shared_buffer_pending_nsecs: 100_000_000,
        callback_count: 1,
        consumed_callback_count: 0,
        silenced_callback_count: 1,
        underrun_count: 0,
    };
    scheduler.observe_audio_output_activity(now, frozen, true, false);
    assert_eq!(
        scheduler.observe_audio_output_activity(
            now + AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER,
            frozen,
            false,
            false,
        ),
        None
    );
    assert!(!scheduler.audio_output_clock_stall_fallback_active());
}

#[test]
fn exact_184_700_vulkan_pair_primes_without_demux_waterline() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let pair_observed_at = Instant::now();
    scheduler.note_initial_av_pair(pair_observed_at);
    let evaluation = initial_start_admission(InitialStartAdmissionInput {
        expected_target_nsecs: 184_700_000_000,
        first_video_nsecs: Some(184_700_000_000),
        first_audio_nsecs: Some(184_714_739_000),
        decoded_video_forward_nsecs: Some(33_333_333),
        strict_video_forward_nsecs: Some(33_333_333),
        decoded_audio_forward_nsecs: Some(626_938_758),
        contiguous_video_frames: 1,
        first_video_duration_nsecs: Some(33_333_333),
        first_following_video_gap_nsecs: None,
        first_frame_is_vulkan: true,
        first_frame_confirmed_clean: true,
        active_recovery: false,
        require_strict_fast_lookahead: false,
        cached_exact_landing_nsecs: None,
        startup_sync_elapsed: Some(Duration::from_millis(800)),
    });
    let InitialStartAdmission::Prime { pair, .. } = evaluation.admission else {
        panic!("exact Vulkan A/V pair should enter bounded prime admission");
    };

    let transaction = scheduler.begin_initial_av_start_transaction(
        pair.video_anchor_nsecs,
        pair.audio_start_target_nsecs,
        pair_observed_at + Duration::from_millis(800),
    );
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Primed);
    assert_eq!(transaction.started_at, pair_observed_at);
    assert_eq!(
        transaction.hard_deadline_at,
        pair_observed_at + Duration::from_secs(3)
    );
}

#[test]
fn startup_output_housekeeping_is_generation_driven_and_periodically_bounded() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = Instant::now();
    assert_eq!(
        scheduler.output_service_demand(now),
        OutputServiceDemand::OutputStateChanged
    );
    scheduler.mark_output_housekeeping_serviced_at(now);
    assert_eq!(
        scheduler.output_service_demand(now),
        OutputServiceDemand::None
    );

    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    assert_eq!(
        scheduler.output_service_demand(now),
        OutputServiceDemand::OutputStateChanged
    );
    scheduler.mark_output_housekeeping_serviced_at(now);
    assert_eq!(
        scheduler.output_service_demand(now + Duration::from_millis(5)),
        OutputServiceDemand::None
    );
    let deadline = scheduler.output_housekeeping_deadline().unwrap();
    assert_eq!(
        scheduler.output_service_demand(deadline - Duration::from_nanos(1)),
        OutputServiceDemand::None
    );
    assert_eq!(
        scheduler.output_service_demand(deadline),
        OutputServiceDemand::PeriodicProbe
    );

    scheduler.mark_output_housekeeping_serviced_at(deadline);
    assert_eq!(
        scheduler.output_service_demand(deadline + Duration::from_millis(5)),
        OutputServiceDemand::None
    );
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        184_714_739_000,
        184_734_739_000,
    );
    assert_eq!(
        scheduler.output_service_demand(deadline),
        OutputServiceDemand::OutputStateChanged
    );
}

#[test]
fn identical_initial_sync_state_is_suppressed_and_summarized_every_500ms() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = Instant::now();
    let observation = InitialSyncLogObservation {
        target_nsecs: 184_700_000_000,
        first_video_nsecs: Some(184_700_000_000),
        first_audio_nsecs: Some(184_714_739_000),
        decoded_video_nsecs: Some(33_333_333),
        strict_video_nsecs: Some(33_333_333),
        decoded_audio_nsecs: Some(626_938_758),
        demux_min_nsecs: Some(33_333_333),
        blocked_on: "insufficient_lookahead",
        due_kind: OutputServiceDemand::PeriodicProbe,
    };

    assert_eq!(
        scheduler.observe_initial_sync_log(observation, now),
        InitialSyncLogDecision::Changed {
            suppressed_repeats: 0
        }
    );
    assert_eq!(
        scheduler.observe_initial_sync_log(observation, now + Duration::from_millis(5)),
        InitialSyncLogDecision::Suppressed
    );
    assert_eq!(
        scheduler.observe_initial_sync_log(observation, now + Duration::from_millis(500)),
        InitialSyncLogDecision::Summary {
            repeated_observations: 2
        }
    );

    scheduler.note_output_housekeeping_change();
    assert!(matches!(
        scheduler.observe_initial_sync_log(observation, now + Duration::from_millis(501)),
        InitialSyncLogDecision::Changed { .. }
    ));
}

#[test]
fn identical_audio_defer_state_is_suppressed_and_summarized_once_per_second() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = Instant::now();
    let observation = InitialAudioDeferObservation {
        audio_start_target_nsecs: 184_714_739_000,
        pending_covers_target: false,
        device_covers_target: false,
        ammunition_at_threshold: false,
        decoded_audio_ledger_observed: true,
    };

    assert_eq!(
        scheduler.observe_initial_audio_defer_log(observation, now),
        InitialSyncLogDecision::Changed {
            suppressed_repeats: 0
        }
    );
    assert_eq!(
        scheduler.observe_initial_audio_defer_log(observation, now + Duration::from_millis(5)),
        InitialSyncLogDecision::Suppressed
    );
    assert_eq!(
        scheduler.observe_initial_audio_defer_log(
            observation,
            now + INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL,
        ),
        InitialSyncLogDecision::Summary {
            repeated_observations: 2
        }
    );

    let changed = InitialAudioDeferObservation {
        audio_start_target_nsecs: observation.audio_start_target_nsecs + 1,
        ..observation
    };
    assert!(matches!(
        scheduler.observe_initial_audio_defer_log(
            changed,
            now + INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL + Duration::from_millis(1),
        ),
        InitialSyncLogDecision::Changed { .. }
    ));
}

#[test]
fn prestart_ownership_anomalies_are_aggregated_by_transaction_and_state() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = Instant::now();

    assert_eq!(
        scheduler.observe_prestart_audio_ownership_log(
            PrestartAudioOwnership::UnexpectedCurrentEpoch,
            17,
            now,
        ),
        InitialSyncLogDecision::Changed {
            suppressed_repeats: 0
        }
    );
    assert_eq!(
        scheduler.observe_prestart_audio_ownership_log(
            PrestartAudioOwnership::UnexpectedCurrentEpoch,
            17,
            now + Duration::from_millis(5),
        ),
        InitialSyncLogDecision::Suppressed
    );
    assert_eq!(
        scheduler.observe_prestart_audio_ownership_log(
            PrestartAudioOwnership::UnexpectedCurrentEpoch,
            17,
            now + INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL,
        ),
        InitialSyncLogDecision::Summary {
            repeated_observations: 2
        }
    );
    assert!(matches!(
        scheduler.observe_prestart_audio_ownership_log(
            PrestartAudioOwnership::StaleEpoch,
            17,
            now + INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL + Duration::from_millis(1),
        ),
        InitialSyncLogDecision::Changed { .. }
    ));
    assert!(matches!(
        scheduler.observe_prestart_audio_ownership_log(
            PrestartAudioOwnership::StaleEpoch,
            18,
            now + INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL + Duration::from_millis(2),
        ),
        InitialSyncLogDecision::Changed { .. }
    ));
}

#[test]
fn rebuffer_output_housekeeping_is_periodic_and_generation_driven() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let now = Instant::now();
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    assert_eq!(
        scheduler.output_service_demand(now),
        OutputServiceDemand::OutputStateChanged
    );

    scheduler.mark_output_housekeeping_serviced_at(now);
    let deadline = scheduler.output_housekeeping_deadline().unwrap();
    assert_eq!(
        scheduler.output_service_demand(deadline - Duration::from_nanos(1)),
        OutputServiceDemand::None
    );
    assert_eq!(
        scheduler.output_service_demand(deadline),
        OutputServiceDemand::PeriodicProbe
    );

    scheduler.mark_output_housekeeping_serviced_at(deadline);
    let generation_change_at = deadline + Duration::from_millis(1);
    assert_eq!(
        scheduler.output_service_demand(generation_change_at),
        OutputServiceDemand::None
    );
    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    assert_eq!(
        scheduler.output_service_demand(generation_change_at),
        OutputServiceDemand::OutputStateChanged
    );
}

#[test]
fn delayed_audio_start_is_a_bounded_primed_transaction() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    let now = Instant::now();
    let transaction =
        scheduler.begin_initial_av_start_transaction(184_700_000_000, 184_714_739_000, now);
    scheduler.mark_output_housekeeping_serviced_at(now);
    let relocked = scheduler.begin_initial_av_start_transaction(
        184_766_666_667,
        184_761_178_998,
        now + Duration::from_millis(5),
    );
    assert_eq!(relocked.video_anchor_nsecs, transaction.video_anchor_nsecs);
    assert_eq!(
        relocked.audio_start_target_nsecs,
        transaction.audio_start_target_nsecs
    );
    assert_eq!(relocked.hard_deadline_at, transaction.hard_deadline_at);
    scheduler.mark_first_frame_presented();

    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Primed);
    assert_eq!(
        scheduler.output_service_demand(now),
        OutputServiceDemand::None
    );
    let audio_due_at = now + Duration::from_nanos(14_739_000);
    assert_eq!(
        scheduler.output_service_demand(audio_due_at),
        OutputServiceDemand::AudioStartDue
    );
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(audio_due_at, false),
        InitialAvStartDecision::Waiting
    );
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(audio_due_at, true),
        InitialAvStartDecision::Commit
    );
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(transaction.hard_deadline_at, true),
        InitialAvStartDecision::Rebuffer
    );
    assert_eq!(
        transaction.hard_deadline_at.saturating_duration_since(now),
        Duration::from_secs(3)
    );

    scheduler.set_state(PlaybackOutputState::Playing);
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.initial_av_start_pending);
    assert!(snapshot.first_frame_presented);
    assert!(snapshot.output_clock_running);
}

#[test]
fn transient_audio_output_snapshot_contention_leaves_a_decode_window_between_retries() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(104_733_333_333));
    let mut now = Instant::now();
    scheduler.begin_initial_av_start_transaction(104_733_333_333, 104_745_215_349, now);
    scheduler.mark_first_frame_presented();
    scheduler.mark_output_housekeeping_serviced_at(now);
    now += Duration::from_nanos(11_882_016);

    let mut cached_decode_steps = 0usize;
    for _ in 0..32 {
        assert_eq!(
            scheduler.output_service_demand(now),
            OutputServiceDemand::AudioStartDue
        );
        assert!(
            scheduler.defer_initial_audio_start_retry(
                now,
                InitialAudioTransientRetry::OutputSnapshotBusy,
            )
        );
        assert_eq!(
            scheduler.output_service_demand(now),
            OutputServiceDemand::None
        );
        // The coordinator's DrainCachedInput branch owns this interval.
        cached_decode_steps = cached_decode_steps.saturating_add(1);
        now = scheduler.output_housekeeping_deadline().unwrap();
    }
    assert_eq!(cached_decode_steps, 32);
    assert!(scheduler.initial_av_start_transaction().is_some());
}

#[test]
fn missing_audio_parks_on_state_change_instead_of_the_eight_ms_retry() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let started_at = Instant::now();
    let transaction =
        scheduler.begin_initial_av_start_transaction(104_733_333_333, 104_745_215_349, started_at);
    scheduler.mark_output_housekeeping_serviced_at(started_at);

    assert!(scheduler.wait_initial_audio_start_for_state_change(transaction.transaction_id));
    let parked = scheduler.initial_av_start_transaction().unwrap();
    assert!(parked.audio_retry_waiting_for_state_change);
    assert_eq!(parked.next_audio_start_retry_at, parked.hard_deadline_at);
    assert_eq!(
        scheduler.output_service_demand(started_at + Duration::from_millis(100)),
        OutputServiceDemand::None
    );

    scheduler.note_output_housekeeping_change();
    let rearmed = scheduler.initial_av_start_transaction().unwrap();
    assert!(!rearmed.audio_retry_waiting_for_state_change);
    assert!(rearmed.next_audio_start_retry_at < rearmed.hard_deadline_at);
}

#[test]
fn field_1m45_pair_uses_exact_audio_due_and_combined_ammunition() {
    const VIDEO_NSECS: u64 = 104_733_333_333;
    const AUDIO_NSECS: u64 = 104_745_215_349;
    const PENDING_NSECS: u64 = 232_199_540;
    const DECODED_NSECS: u64 = 244_081_556;

    let session_id = PlaybackSessionId(105);
    let control = Arc::new(FfmpegControl::new(session_id));
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 44_100 * 2, 44_100, 2);
    assert_eq!(output.sample_rate(), 44_100);
    assert_eq!(output.channels(), 2);
    assert!(!output.stream_active_for_test());

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(VIDEO_NSECS));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 2],
            duration_nsecs: PENDING_NSECS,
        },
        AUDIO_NSECS,
        AUDIO_NSECS + PENDING_NSECS,
    );
    let now = Instant::now();
    let transaction = scheduler.begin_initial_av_start_transaction(VIDEO_NSECS, AUDIO_NSECS, now);
    scheduler.mark_first_frame_presented();

    assert_eq!(transaction.video_anchor_nsecs, VIDEO_NSECS);
    assert_eq!(transaction.audio_start_target_nsecs, AUDIO_NSECS);
    assert_eq!(
        transaction.audio_start_due_at.duration_since(now),
        Duration::from_nanos(AUDIO_NSECS - VIDEO_NSECS)
    );
    assert_eq!(
        scheduler.output_service_demand(transaction.audio_start_due_at),
        OutputServiceDemand::AudioStartDue
    );
    let ammunition =
        InitialAudioAmmunitionSnapshot::from_ledgers(&scheduler, None, DECODED_NSECS, AUDIO_NSECS);
    assert_eq!(ammunition.pending_audio_nsecs, PENDING_NSECS);
    assert_eq!(ammunition.decoded_audio_nsecs, DECODED_NSECS);
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Commit, ammunition),
        InitialAudioStartAction::CommitCovered
    );
}

#[test]
fn permanently_busy_ao_cannot_hide_external_hard_deadline() {
    const TARGET_NSECS: u64 = 104_745_215_349;
    let session_id = PlaybackSessionId(106);
    let control = Arc::new(FfmpegControl::new(session_id));
    let output = Arc::new(AudioOutput::stopped_for_test(
        Arc::clone(&control),
        44_100 * 2,
        44_100,
        2,
    ));
    output.reset_clock(TARGET_NSECS);
    let mut scheduler = PlaybackOutputScheduler::new();
    let started_at = Instant::now() - Duration::from_secs(4);
    scheduler.begin_initial_av_start_transaction_for_generations(
        104_733_333_333,
        TARGET_NSECS,
        control.seek_generation(),
        started_at,
    );
    scheduler.mark_first_frame_presented();

    let release = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel();
    let blocked_output = Arc::clone(&output);
    let blocked_release = Arc::clone(&release);
    let blocker = std::thread::spawn(move || {
        blocked_output.hold_internal_locks_until_for_test(entered_tx, &blocked_release)
    });
    entered_rx.recv().unwrap();
    assert!(output.try_snapshot().unwrap().is_none());
    assert!(output.reset_would_block_for_test());

    let expire_started_at = Instant::now();
    assert!(expire_initial_av_start_hard_deadline(
        &mut scheduler,
        Some(output.as_ref()),
        Instant::now(),
        &control,
        session_id,
    ));
    assert!(expire_started_at.elapsed() < Duration::from_millis(20));
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Aborted
    );
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert_eq!(
        snapshot
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs),
        Some(TARGET_NSECS)
    );
    assert!(scheduler.initial_av_start_transaction().is_none());

    release.store(true, Ordering::Release);
    blocker.join().unwrap();
}

#[test]
fn primed_transaction_survives_ordinary_packet_generation_and_aborts_on_discontinuity() {
    const TARGET_NSECS: u64 = 104_745_215_349;
    let session_id = PlaybackSessionId(107);
    let control = Arc::new(FfmpegControl::new(session_id));
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 4_096, 44_100, 2);
    output.reset_clock(TARGET_NSECS);
    let mut scheduler = PlaybackOutputScheduler::new();
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        104_733_333_333,
        TARGET_NSECS,
        control.seek_generation(),
        Instant::now(),
    );
    let (event_tx, _event_rx) = mpsc::channel();
    let audio_epoch_before_packet = output.audio_epoch();

    assert!(!abort_initial_av_start_for_discontinuity_change(
        &mut scheduler,
        &output,
        transaction,
        &control,
        session_id,
        &event_tx,
    ));
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Primed);
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .map(|current| current.transaction_id),
        Some(transaction.transaction_id)
    );
    assert_eq!(output.audio_epoch(), audio_epoch_before_packet);

    scheduler.advance_discontinuity_epoch();
    assert!(abort_initial_av_start_for_discontinuity_change(
        &mut scheduler,
        &output,
        transaction,
        &control,
        session_id,
        &event_tx,
    ));
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Aborted
    );
    assert!(scheduler.restart_pending());
    assert!(scheduler.initial_av_start_transaction().is_none());

    let rearmed = scheduler.begin_initial_av_start_transaction_for_generations(
        transaction.video_anchor_nsecs,
        transaction.audio_start_target_nsecs,
        control.seek_generation(),
        Instant::now(),
    );
    assert_ne!(rearmed.transaction_id, transaction.transaction_id);
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Collecting
    );
}

#[test]
fn unpresented_initial_transaction_keeps_bounded_housekeeping_and_can_rearm_input() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    let now = Instant::now();
    let transaction =
        scheduler.begin_initial_av_start_transaction(184_700_000_000, 184_714_739_000, now);
    scheduler.mark_output_housekeeping_serviced_at(now);

    let housekeeping_deadline = scheduler.output_housekeeping_deadline().unwrap();
    assert_eq!(housekeeping_deadline, transaction.audio_start_due_at);
    assert_eq!(
        scheduler.output_service_demand(housekeeping_deadline),
        OutputServiceDemand::AudioStartDue
    );

    scheduler.scheduled_video_queue.clear();
    scheduler.mark_first_frame_presentation_failed();
    let snapshot = scheduler.snapshot();
    assert!(snapshot.first_frame_needed);
    assert!(!snapshot.first_frame_presented);
    assert!(snapshot.initial_av_start_pending);
}

#[test]
fn cache_pause_video_184_700_audio_184_714739_commits_to_playing() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    control.set_cache_paused(true);
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(184_700_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 859_000_000,
        },
        184_714_739_000,
        185_573_739_000,
    );
    let now = Instant::now();
    let transaction =
        scheduler.begin_initial_av_start_transaction(184_700_000_000, 184_714_739_000, now);
    scheduler.mark_first_frame_presented();
    let audio_due_at = transaction.audio_start_due_at;

    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(audio_due_at, false),
        InitialAvStartDecision::Waiting
    );
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(audio_due_at, true),
        InitialAvStartDecision::Commit
    );

    control.set_cache_paused(false);
    scheduler.commit_initial_av_start_transaction();
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.initial_av_start_pending);
    assert!(snapshot.first_frame_presented);
    assert!(snapshot.output_clock_running);
    assert!(!control.is_cache_paused());
}

#[test]
fn exact_165_266_seek_stages_audio_before_video_publish_and_enters_playing() {
    const VIDEO_TARGET_NSECS: u64 = 165_266_666_667;
    const AUDIO_TARGET_NSECS: u64 = 165_279_637_171;
    const NEXT_VIDEO_NSECS: u64 = 165_333_333_333;
    const AUDIO_FRAME_NSECS: u64 = 10_000_000;

    let session_id = PlaybackSessionId(165);
    let control = Arc::new(FfmpegControl::new(session_id));
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 96_000, 48_000, 2);
    output.reset_clock(AUDIO_TARGET_NSECS);

    let mut output_scheduler = PlaybackOutputScheduler::new();
    output_scheduler.push_decoded_video_for_test(test_queued_video_frame(VIDEO_TARGET_NSECS));
    for index in 0..10_u64 {
        output_scheduler.push_decoded_video_for_test(test_queued_video_frame(
            NEXT_VIDEO_NSECS + index * 33_333_333,
        ));
    }
    for index in 0..30_u64 {
        let start_nsecs = AUDIO_TARGET_NSECS + index * AUDIO_FRAME_NSECS;
        output_scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.25; 960],
                duration_nsecs: AUDIO_FRAME_NSECS,
            },
            start_nsecs,
            start_nsecs + AUDIO_FRAME_NSECS,
        );
    }
    output_scheduler.begin_initial_av_start_transaction_for_generations(
        VIDEO_TARGET_NSECS,
        AUDIO_TARGET_NSECS,
        seek_generation,
        Instant::now() - Duration::from_millis(100),
    );

    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let (event_tx, event_rx) = mpsc::channel();
    let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
    let mut buffered_reporter = BufferedReporter::new_with_events(true, false);
    let mut current_start_position_nsecs = VIDEO_TARGET_NSECS;
    let mut scheduler = PlaybackScheduler::new(VIDEO_TARGET_NSECS);

    let status = service_initial_video_clock_until_audio_start(
        &mut output_scheduler,
        &output,
        None,
        AUDIO_TARGET_NSECS,
        Some(0),
        &control,
        session_id,
        &vo_queue,
        &frame_presented,
        &mut position_reporter,
        &event_tx,
        &mut subtitle_pipeline,
        &mut buffered_reporter,
        &mut current_start_position_nsecs,
        &mut scheduler,
    )
    .expect("exact seek initial transaction commits");

    assert_eq!(status, OutputGateResumeStatus::Resumed);
    assert_eq!(
        output_scheduler.snapshot().state,
        PlaybackOutputState::Playing
    );
    assert!(output_scheduler.first_frame_presented);
    assert!(frame_presented.load(Ordering::Acquire));
    assert_eq!(
        output_scheduler
            .scheduled_video_queue
            .range_nsecs()
            .map(|range| range.0),
        Some(NEXT_VIDEO_NSECS)
    );
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );
    assert!(output.stream_active());
    let prepared_range = output
        .snapshot()
        .expect("audio snapshot")
        .payload_range_nsecs
        .expect("staged audio payload range");
    assert!(prepared_range.0 <= AUDIO_TARGET_NSECS);
    assert!(prepared_range.1 > AUDIO_TARGET_NSECS);

    let callback_started_at = Instant::now();
    assert!(
        output
            .transfer_next_queued_frame_for_test()
            .expect("queue worker transfer")
    );
    let mut callback_samples = vec![0.0; 960];
    output.invoke_callback_for_test(&mut callback_samples);
    let callback_elapsed = callback_started_at.elapsed();
    let activity = output.activity_snapshot().expect("audio activity snapshot");
    assert_eq!(activity.callback_count, 1);
    assert_eq!(activity.consumed_callback_count, 1);
    assert_eq!(activity.silenced_callback_count, 0);
    assert!(callback_samples.iter().any(|sample| *sample != 0.0));
    assert!(
        callback_elapsed < Duration::from_millis(100),
        "first callback must consume staged payload immediately, elapsed={callback_elapsed:?}"
    );

    let forbidden_diagnostics = event_rx
        .try_iter()
        .filter_map(|event| match event.kind {
            BackendEventKind::Diagnostic(diagnostic) => Some(diagnostic),
            _ => None,
        })
        .filter(|diagnostic| {
            diagnostic.code == "ffmpeg_initial_audio_stage_no_payload_terminal"
                || diagnostic
                    .message
                    .contains("initial_av_start_hard_deadline_external")
        })
        .count();
    assert_eq!(forbidden_diagnostics, 0);
}

#[test]
fn eighty_ms_real_audio_delay_commits_bounded_silence_and_consumes_the_first_callback() {
    const TARGET_NSECS: u64 = 165_266_666_667;
    const DELAYED_AUDIO_NSECS: u64 = TARGET_NSECS + 80_000_000;
    const VIDEO_FRAME_NSECS: u64 = 33_333_333;
    const AUDIO_FRAME_NSECS: u64 = 10_000_000;

    let session_id = PlaybackSessionId(164);
    let control = Arc::new(FfmpegControl::new(session_id));
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 96_000, 48_000, 2);
    output.reset_clock(TARGET_NSECS);

    let mut output_scheduler = PlaybackOutputScheduler::new();
    for index in 0..12_u64 {
        output_scheduler.push_decoded_video_for_test(test_queued_video_frame(
            TARGET_NSECS + index * VIDEO_FRAME_NSECS,
        ));
    }
    for index in 0..30_u64 {
        let start_nsecs = DELAYED_AUDIO_NSECS + index * AUDIO_FRAME_NSECS;
        output_scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.25; 960],
                duration_nsecs: AUDIO_FRAME_NSECS,
            },
            start_nsecs,
            start_nsecs + AUDIO_FRAME_NSECS,
        );
    }
    let transaction = output_scheduler.begin_initial_av_start_transaction_for_generations(
        TARGET_NSECS,
        TARGET_NSECS,
        seek_generation,
        Instant::now() - Duration::from_millis(100),
    );
    assert_eq!(
        transaction.committed_bounded_delayed_audio_start_nsecs,
        Some(DELAYED_AUDIO_NSECS)
    );

    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let (event_tx, event_rx) = mpsc::channel();
    let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
    let mut buffered_reporter = BufferedReporter::new_with_events(true, false);
    let mut current_start_position_nsecs = TARGET_NSECS;
    let mut scheduler = PlaybackScheduler::new(TARGET_NSECS);

    let status = service_initial_video_clock_until_audio_start(
        &mut output_scheduler,
        &output,
        None,
        TARGET_NSECS,
        Some(0),
        &control,
        session_id,
        &vo_queue,
        &frame_presented,
        &mut position_reporter,
        &event_tx,
        &mut subtitle_pipeline,
        &mut buffered_reporter,
        &mut current_start_position_nsecs,
        &mut scheduler,
    )
    .expect("bounded delayed audio commits");

    assert_eq!(status, OutputGateResumeStatus::Resumed);
    assert_eq!(
        output_scheduler.snapshot().state,
        PlaybackOutputState::Playing
    );
    assert!(frame_presented.load(Ordering::Acquire));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );
    assert_eq!(
        output
            .snapshot()
            .expect("audio snapshot")
            .payload_range_nsecs
            .map(|range| range.0),
        Some(TARGET_NSECS)
    );

    let callback_started_at = Instant::now();
    assert!(
        output
            .transfer_next_queued_frame_for_test()
            .expect("queue worker transfer")
    );
    let mut callback_samples = vec![1.0; 960];
    output.invoke_callback_for_test(&mut callback_samples);
    let callback_elapsed = callback_started_at.elapsed();
    let activity = output.activity_snapshot().expect("audio activity snapshot");
    assert_eq!(activity.callback_count, 1);
    assert_eq!(activity.consumed_callback_count, 1);
    assert_eq!(activity.silenced_callback_count, 0);
    assert!(callback_samples.iter().all(|sample| *sample == 0.0));
    assert!(
        callback_elapsed < Duration::from_millis(100),
        "bounded queued silence must be consumed immediately, elapsed={callback_elapsed:?}"
    );
    assert!(!event_rx.try_iter().any(|event| matches!(
        event.kind,
        BackendEventKind::Diagnostic(ref diagnostic)
            if diagnostic.code == "ffmpeg_initial_audio_stage_no_payload_terminal"
                || diagnostic.code == "ffmpeg_initial_audio_delay_exceeds_bound"
    )));
}

#[test]
fn initial_audio_delay_over_eighty_ms_rebuffers_without_waiting_for_hard_deadline() {
    const TARGET_NSECS: u64 = 165_266_666_667;
    const DELAYED_AUDIO_NSECS: u64 = TARGET_NSECS + 80_000_001;

    let session_id = PlaybackSessionId(166);
    let control = Arc::new(FfmpegControl::new(session_id));
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 256, 48_000, 2);
    output.reset_clock(TARGET_NSECS);

    let mut output_scheduler = PlaybackOutputScheduler::new();
    output_scheduler.push_decoded_video_for_test(test_queued_video_frame(TARGET_NSECS));
    output_scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.25; 1_920],
            duration_nsecs: 20_000_000,
        },
        DELAYED_AUDIO_NSECS,
        DELAYED_AUDIO_NSECS + 20_000_000,
    );
    let transaction = output_scheduler.begin_initial_av_start_transaction_for_generations(
        TARGET_NSECS,
        TARGET_NSECS,
        seek_generation,
        Instant::now() - Duration::from_millis(100),
    );
    assert!(Instant::now() < transaction.hard_deadline_at);

    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let (event_tx, event_rx) = mpsc::channel();
    let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
    let mut buffered_reporter = BufferedReporter::new_with_events(true, false);
    let mut current_start_position_nsecs = TARGET_NSECS;
    let mut scheduler = PlaybackScheduler::new(TARGET_NSECS);

    let status = service_initial_video_clock_until_audio_start(
        &mut output_scheduler,
        &output,
        None,
        TARGET_NSECS,
        Some(0),
        &control,
        session_id,
        &vo_queue,
        &frame_presented,
        &mut position_reporter,
        &event_tx,
        &mut subtitle_pipeline,
        &mut buffered_reporter,
        &mut current_start_position_nsecs,
        &mut scheduler,
    )
    .expect("unbounded initial audio delay enters immediate fallback");

    assert_eq!(status, OutputGateResumeStatus::Rebuffering);
    assert!(output_scheduler.playback_output_state.rebuffering());
    assert!(!frame_presented.load(Ordering::Acquire));
    assert!(event_rx.try_iter().any(|event| matches!(
        event.kind,
        BackendEventKind::Diagnostic(ref diagnostic)
            if diagnostic.code == "ffmpeg_initial_audio_delay_exceeds_bound"
    )));
}

#[test]
fn hard_deadline_with_covered_startup_audio_still_rebuffers() {
    let mut scheduler = PlaybackOutputScheduler::new();
    for index in 0..38_u64 {
        scheduler.push_decoded_video_for_test(test_queued_video_frame(
            184_700_000_000 + index * 40_000_000,
        ));
    }
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 859_000_000,
        },
        184_714_739_000,
        185_573_739_000,
    );
    let started_at = Instant::now() - Duration::from_secs(4);
    scheduler.begin_initial_av_start_transaction(184_700_000_000, 184_714_739_000, started_at);
    scheduler.mark_first_frame_presented();

    assert_eq!(
        scheduler.output_service_demand(Instant::now()),
        OutputServiceDemand::HardDeadline
    );
    assert_eq!(
        scheduler
            .initial_av_start_transaction()
            .unwrap()
            .decision(Instant::now(), true),
        InitialAvStartDecision::Rebuffer
    );
    let ammunition =
        InitialAudioAmmunitionSnapshot::from_ledgers(&scheduler, None, 0, 184_714_739_000);
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, ammunition),
        InitialAudioStartAction::FailNoAmmunition
    );
    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Primed);
}

#[test]
fn hard_deadline_without_audio_preserves_original_resume_anchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let video_anchor_nsecs = 184_700_000_000;
    let audio_target_nsecs = 184_714_739_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(video_anchor_nsecs));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(video_anchor_nsecs + 40_000_000));
    let started_at = Instant::now() - Duration::from_secs(4);
    scheduler.begin_initial_av_start_transaction(
        video_anchor_nsecs,
        audio_target_nsecs,
        started_at,
    );
    scheduler.mark_first_frame_presented();
    scheduler.scheduled_video_queue.pop_front();
    let ammunition = InitialAudioAmmunitionSnapshot::from_ledgers(
        &scheduler,
        Some(audio_snapshot(audio_target_nsecs, 0)),
        0,
        audio_target_nsecs,
    );
    assert_eq!(
        initial_audio_start_action(InitialAvStartDecision::Rebuffer, ammunition),
        InitialAudioStartAction::FailNoAmmunition
    );

    let control = FfmpegControl::new(PlaybackSessionId(1));
    scheduler.fail_initial_av_start_transaction(
        &control,
        PlaybackSessionId(1),
        "test_hard_deadline_without_audio",
    );
    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(control.is_output_rebuffer_paused());
    assert_eq!(
        snapshot
            .video_output_rebuffer_anchor
            .expect("original target anchor")
            .timeline_nsecs,
        audio_target_nsecs
    );
    let first_retained_video_nsecs = snapshot.queued_video_range_nsecs.unwrap().0;
    assert!(
        first_retained_video_nsecs.saturating_sub(audio_target_nsecs) <= 500_000_000,
        "failure must not advance to the decoder frontier"
    );
}

#[test]
fn log_64s_unstable_snapshot_deadline_terminates_at_the_original_audio_target() {
    let video_anchor_nsecs = 64_233_333_333;
    let audio_target_nsecs = 64_249_614_452;
    let pending_audio_nsecs = 185_760_000;
    let decoded_audio_nsecs = 202_000_000;
    let session_id = PlaybackSessionId(64);
    let control = Arc::new(FfmpegControl::new(session_id));
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 4_800, 44_100, 2);
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: pending_audio_nsecs,
        },
        audio_target_nsecs,
        audio_target_nsecs.saturating_add(pending_audio_nsecs),
    );
    let started_at = Instant::now() - Duration::from_secs(4);
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        video_anchor_nsecs,
        audio_target_nsecs,
        control.seek_generation(),
        started_at,
    );
    scheduler.mark_first_frame_presented();
    let decision = scheduler
        .initial_av_start_transaction()
        .unwrap()
        .decision(Instant::now(), true);
    assert_eq!(decision, InitialAvStartDecision::Rebuffer);
    let ammunition = InitialAudioAmmunitionSnapshot::from_optional_ledgers(
        &scheduler,
        None,
        Some(decoded_audio_nsecs),
        audio_target_nsecs,
    );
    assert_eq!(ammunition.pending_audio_nsecs, pending_audio_nsecs);
    assert_eq!(ammunition.decoded_audio_nsecs, decoded_audio_nsecs);
    assert!(ammunition.reaches_force_start_threshold());
    assert_eq!(
        classify_prestart_audio_ownership(PrestartAudioOwnershipInput {
            phase: transaction.audio_prepare_phase,
            token: transaction.audio_prepare_token,
            current_audio_epoch: output.audio_epoch(),
            current_seek_generation: control.seek_generation(),
            target_nsecs: audio_target_nsecs,
            snapshot: AudioOutputStableSnapshot::SnapshotUnstable(AudioOutputUnstableSnapshot {
                audio_epoch: output.audio_epoch(),
                observed_version: 426,
                attempts: 8,
            },),
        }),
        PrestartAudioOwnership::SnapshotUnstable
    );
    let (event_tx, _event_rx) = mpsc::channel();

    assert!(fail_initial_av_start_after_unstable_snapshot_deadline(
        &mut scheduler,
        &output,
        decision,
        &control,
        session_id,
        &event_tx,
    ));

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(!snapshot.initial_av_start_pending);
    assert!(scheduler.initial_av_start_transaction().is_none());
    assert!(control.is_output_rebuffer_paused());
    assert_eq!(
        snapshot
            .video_output_rebuffer_anchor
            .expect("the log's original audio target remains the fallback anchor")
            .timeline_nsecs,
        audio_target_nsecs
    );
    assert_eq!(
        scheduler.pending_start_audio.buffered_duration(),
        Duration::from_nanos(pending_audio_nsecs)
    );
}

#[test]
fn prepared_abort_before_rebuffer_preserves_the_explicit_seek_anchor() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 1_050_500_000_000;
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        target_nsecs,
        target_nsecs,
        9,
        Instant::now(),
    );
    assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 7));
    let token = prepared_token(
        transaction.transaction_id,
        transaction.discontinuity_epoch,
        9,
    );
    assert!(scheduler.finish_initial_audio_prepare(token));
    assert_eq!(
        scheduler.abort_initial_audio_prepare(
            transaction.transaction_id,
            PlaybackSessionId(1),
            "prepared_payload_did_not_cover_target",
        ),
        Some(token)
    );

    let control = FfmpegControl::new(PlaybackSessionId(1));
    scheduler.fail_initial_av_start_transaction_at_anchor(
        &control,
        PlaybackSessionId(1),
        "prepared_payload_did_not_cover_target",
        target_nsecs,
    );

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert_eq!(
        snapshot
            .video_output_rebuffer_anchor
            .expect("explicit seek anchor")
            .timeline_nsecs,
        target_nsecs
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

#[test]
fn primed_initial_transaction_never_pushes_decoded_audio_directly() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let target_nsecs = 8_840_000_000;
    scheduler.push_decoded_video_for_test(test_queued_video_frame(target_nsecs));
    scheduler.begin_initial_av_start_transaction(target_nsecs, target_nsecs, Instant::now());

    assert_eq!(scheduler.snapshot().state, PlaybackOutputState::Primed);
    assert!(scheduler.snapshot().initial_av_start_pending);
    assert!(!scheduler.decoded_audio_can_push_directly(
        target_nsecs,
        target_nsecs + 20_000_000,
        target_nsecs,
    ));
}

const PRODUCTION_STAGE_TARGET_NSECS: u64 = 1_050_500_000_000;
const PRODUCTION_STAGE_FRAME_NSECS: u64 = 10_000_000;
const PRODUCTION_STAGE_SAMPLES_PER_FRAME: usize = 20;

struct ProductionInitialAudioState {
    session_id: PlaybackSessionId,
    control: Arc<FfmpegControl>,
    output: AudioOutput,
    scheduler: PlaybackOutputScheduler,
    preparing_token: InitialAudioPrepareToken,
    event_tx: mpsc::Sender<BackendEvent>,
    _event_rx: mpsc::Receiver<BackendEvent>,
    frame_count: usize,
}

impl ProductionInitialAudioState {
    fn new(case_id: u64, frame_count: usize) -> Self {
        let session_id = PlaybackSessionId(1_000 + case_id);
        let control = Arc::new(FfmpegControl::new(session_id));
        let seek_generation = control.request_seek();
        control.finish_seek(seek_generation);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
        let output = AudioOutput::stopped_for_test(Arc::clone(&control), 256, 1_000, 2);
        let mut scheduler = PlaybackOutputScheduler::new();
        for frame_index in 0..frame_count {
            let start_timeline_nsecs = PRODUCTION_STAGE_TARGET_NSECS.saturating_add(
                u64::try_from(frame_index)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(PRODUCTION_STAGE_FRAME_NSECS),
            );
            scheduler.push_pending_start_audio_for_test(
                DecodedAudio {
                    samples: vec![0.25; PRODUCTION_STAGE_SAMPLES_PER_FRAME],
                    duration_nsecs: PRODUCTION_STAGE_FRAME_NSECS,
                },
                start_timeline_nsecs,
                start_timeline_nsecs.saturating_add(PRODUCTION_STAGE_FRAME_NSECS),
            );
        }
        let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
            PRODUCTION_STAGE_TARGET_NSECS,
            PRODUCTION_STAGE_TARGET_NSECS,
            seek_generation,
            Instant::now(),
        );
        output.reset_clock(PRODUCTION_STAGE_TARGET_NSECS);
        let audio_epoch = output.audio_epoch();
        assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, audio_epoch));
        let preparing_token = InitialAudioPrepareToken {
            transaction_id: transaction.transaction_id,
            discontinuity_epoch: transaction.discontinuity_epoch,
            seek_generation: transaction.seek_generation,
            audio_epoch,
            target_nsecs: transaction.audio_start_target_nsecs,
            staged_range_nsecs: (
                transaction.audio_start_target_nsecs,
                transaction.audio_start_target_nsecs,
            ),
            staged_frames: 0,
            staged_samples: 0,
            staged_until_nsecs: transaction.audio_start_target_nsecs,
        };
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            session_id,
            control,
            output,
            scheduler,
            preparing_token,
            event_tx,
            _event_rx: event_rx,
            frame_count,
        }
    }

    fn stage(
        &mut self,
        observe_checkpoint: impl FnMut(AudioStageCheckpoint),
    ) -> super::super::AudioStageResult {
        let flush_until_nsecs = PRODUCTION_STAGE_TARGET_NSECS.saturating_add(
            u64::try_from(self.frame_count)
                .unwrap_or(u64::MAX)
                .saturating_mul(PRODUCTION_STAGE_FRAME_NSECS),
        );
        self.stage_until(flush_until_nsecs, observe_checkpoint)
    }

    fn stage_until(
        &mut self,
        flush_until_nsecs: u64,
        observe_checkpoint: impl FnMut(AudioStageCheckpoint),
    ) -> super::super::AudioStageResult {
        let vo_queue = VideoOutputQueue::default();
        vo_queue.begin_session(self.session_id);
        let frame_presented = AtomicBool::new(false);
        let mut position_reporter = PositionReporter::default();
        let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
        let mut buffered_reporter = BufferedReporter::new_with_events(true, false);
        stage_pending_audio_with_checkpoint(
            &mut self.scheduler.pending_start_audio,
            &self.output,
            self.preparing_token.audio_epoch,
            PRODUCTION_STAGE_TARGET_NSECS,
            flush_until_nsecs,
            AudioClockMode::AudioStarted,
            DelayedAudioStartSilencePolicy::Skip,
            &self.control,
            &mut self.scheduler.scheduled_video_queue,
            self.session_id,
            &vo_queue,
            &frame_presented,
            &mut position_reporter,
            &self.event_tx,
            &mut subtitle_pipeline,
            &mut buffered_reporter,
            observe_checkpoint,
        )
        .expect("production audio staging succeeds")
    }

    fn prepared_token(
        &mut self,
        result: super::super::AudioStageResult,
    ) -> InitialAudioPrepareToken {
        let staged_range_nsecs = result
            .staged_range_nsecs
            .expect("production stage created a payload range");
        let token = InitialAudioPrepareToken {
            staged_range_nsecs,
            staged_until_nsecs: staged_range_nsecs.1,
            staged_frames: result.staged_frames,
            staged_samples: result.staged_samples,
            ..self.preparing_token
        };
        assert!(self.scheduler.finish_initial_audio_prepare(token));
        token
    }

    fn stable_snapshot(&self) -> super::super::AudioOutputSnapshot {
        match self.output.stable_snapshot().unwrap() {
            AudioOutputStableSnapshot::Stable(snapshot) => snapshot,
            AudioOutputStableSnapshot::SnapshotUnstable(unstable) => {
                panic!("test AO snapshot unexpectedly unstable: {unstable:?}")
            }
        }
    }

    fn abort(&mut self, token: InitialAudioPrepareToken, reason: &'static str) {
        abort_initial_audio_stage_for_test(
            &mut self.scheduler,
            &self.output,
            token,
            self.session_id,
            &self.event_tx,
            reason,
        );
    }

    fn assert_lossless_retry_state(&self) {
        let snapshot = self.stable_snapshot();
        assert_eq!(snapshot.shared_payload_nsecs, 0);
        assert_eq!(snapshot.queue_pending_nsecs, 0);
        assert_eq!(snapshot.worker_in_flight_nsecs, 0);
        assert_eq!(snapshot.queue_frames, 0);
        assert_eq!(snapshot.worker_in_flight_frames, 0);
        assert!(!snapshot.queue_active);
        assert!(self.scheduler.initial_av_start_transaction().is_none());
        assert_eq!(self.scheduler.pending_start_audio.len(), self.frame_count);
        assert_eq!(
            self.scheduler.pending_start_audio.queued_samples(),
            self.frame_count
                .saturating_mul(PRODUCTION_STAGE_SAMPLES_PER_FRAME)
        );
        assert_eq!(
            self.scheduler.pending_start_audio.range_nsecs(),
            Some((
                PRODUCTION_STAGE_TARGET_NSECS,
                PRODUCTION_STAGE_TARGET_NSECS.saturating_add(
                    u64::try_from(self.frame_count)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(PRODUCTION_STAGE_FRAME_NSECS),
                ),
            ))
        );
    }
}

#[test]
fn primed_state_accepts_ordinary_audio_without_cancelling_initial_transaction() {
    let mut state = ProductionInitialAudioState::new(47, 4);
    let transaction_before = state
        .scheduler
        .initial_av_start_transaction()
        .expect("initial transaction is primed");
    let audio_epoch_before = state.output.audio_epoch();
    let start_timeline_nsecs = PRODUCTION_STAGE_TARGET_NSECS + 4 * PRODUCTION_STAGE_FRAME_NSECS;
    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(state.session_id);
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
    let mut buffered_reporter = BufferedReporter::new_with_events(true, false);

    let admission = state
        .scheduler
        .push_decoded_audio_or_buffer(
            &state.output,
            &state.control,
            DecodedAudio {
                samples: vec![0.5; PRODUCTION_STAGE_SAMPLES_PER_FRAME],
                duration_nsecs: PRODUCTION_STAGE_FRAME_NSECS,
            },
            start_timeline_nsecs,
            start_timeline_nsecs + PRODUCTION_STAGE_FRAME_NSECS,
            state.session_id,
            &vo_queue,
            &frame_presented,
            &mut position_reporter,
            &state.event_tx,
            &mut subtitle_pipeline,
            &mut buffered_reporter,
            false,
        )
        .expect("ordinary audio remains owned by the restart transaction");

    assert!(matches!(admission, DecodedAudioAdmission::Accepted));
    assert_eq!(
        state.scheduler.snapshot().state,
        PlaybackOutputState::Primed
    );
    assert_eq!(
        state
            .scheduler
            .initial_av_start_transaction()
            .map(|transaction| transaction.transaction_id),
        Some(transaction_before.transaction_id)
    );
    assert_eq!(
        state.scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Preparing
    );
    assert_eq!(state.output.audio_epoch(), audio_epoch_before);
    assert_eq!(state.scheduler.pending_start_audio.len(), 5);
}

#[test]
fn uncommitted_initial_transaction_cannot_run_underrun_recovery_or_change_ao_epoch() {
    let mut state = ProductionInitialAudioState::new(48, 4);
    assert_eq!(
        state.scheduler.snapshot().state,
        PlaybackOutputState::Primed
    );
    state
        .output
        .mark_underrun_for_test(PRODUCTION_STAGE_TARGET_NSECS);
    assert!(state.output.underrun_active());
    let audio_epoch_before = state.output.audio_epoch();
    let pending_frames_before = state.scheduler.pending_start_audio.len();

    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(state.session_id);
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let mut subtitle_pipeline = SubtitlePipeline::empty_for_test();
    let mut buffered_reporter = BufferedReporter::new_with_events(true, false);
    state
        .scheduler
        .flush_pending_start_audio_if_ready(
            &state.output,
            &state.control,
            state.session_id,
            &vo_queue,
            &frame_presented,
            &mut position_reporter,
            &state.event_tx,
            &mut subtitle_pipeline,
            &mut buffered_reporter,
        )
        .expect("restart gate suppresses ordinary underrun recovery");

    assert_eq!(state.output.audio_epoch(), audio_epoch_before);
    assert!(state.output.underrun_active());
    assert_eq!(
        state.scheduler.pending_start_audio.len(),
        pending_frames_before
    );
    assert_eq!(
        state.scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Preparing
    );
    assert!(state.scheduler.restart_pending());
}

#[test]
fn unstable_snapshot_deadline_rolls_prepared_audio_back_before_rebuffer() {
    const FRAMES: usize = 4;
    let mut state = ProductionInitialAudioState::new(49, FRAMES);
    let stage_result = state.stage(|_| {});
    let token = state.prepared_token(stage_result);
    assert_eq!(
        state.scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Prepared
    );
    assert_eq!(state.scheduler.initial_audio_prepare_token(), Some(token));

    assert!(fail_initial_av_start_after_unstable_snapshot_deadline(
        &mut state.scheduler,
        &state.output,
        InitialAvStartDecision::Rebuffer,
        &state.control,
        state.session_id,
        &state.event_tx,
    ));

    state.assert_lossless_retry_state();
    let snapshot = state.scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(state.control.is_output_rebuffer_paused());
    assert_eq!(
        snapshot
            .video_output_rebuffer_anchor
            .expect("prepared rollback retains the transaction target")
            .timeline_nsecs,
        PRODUCTION_STAGE_TARGET_NSECS
    );
}

fn prepared_token(
    transaction_id: u64,
    discontinuity_epoch: u64,
    seek_generation: u64,
) -> InitialAudioPrepareToken {
    InitialAudioPrepareToken {
        transaction_id,
        discontinuity_epoch,
        seek_generation,
        audio_epoch: 7,
        target_nsecs: 1_050_500_000_000,
        staged_range_nsecs: (1_050_500_000_000, 1_050_900_000_000),
        staged_frames: 19,
        staged_samples: 38_912,
        staged_until_nsecs: 1_050_900_000_000,
    }
}

#[test]
fn scheduler_commit_precedes_atomic_callback_activation() {
    let session_id = PlaybackSessionId(44);
    let control = FfmpegControl::new(session_id);
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let mut scheduler = PlaybackOutputScheduler::new();
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        1_050_500_000_000,
        1_050_500_000_000,
        seek_generation,
        Instant::now(),
    );
    assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 7));
    let token = prepared_token(
        transaction.transaction_id,
        transaction.discontinuity_epoch,
        seek_generation,
    );
    assert!(scheduler.finish_initial_audio_prepare(token));
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Prepared
    );

    assert!(scheduler.commit_initial_audio_prepare(token));
    assert!(!scheduler.restart_pending());
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Prepared
    );
    assert_eq!(scheduler.initial_audio_prepare_token(), Some(token));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Ready
    );
    assert!(
        control
            .audio_output_control_snapshot()
            .paused_by_seek_transition()
    );

    let activated = AtomicBool::new(false);
    assert!(
        control.compare_and_commit_audio_output_start(seek_generation, || {
            activated.store(true, Ordering::Release);
            true
        })
    );
    assert!(activated.load(Ordering::Acquire));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );
    assert!(
        !control
            .audio_output_control_snapshot()
            .paused_by_seek_transition()
    );
    assert!(scheduler.finalize_initial_audio_prepare(token, session_id));
    assert!(scheduler.initial_audio_prepare_token().is_none());
}

#[test]
fn finalize_mismatch_after_activation_recovers_without_reopening_abort() {
    let session_id = PlaybackSessionId(46);
    let control = FfmpegControl::new(session_id);
    let seek_generation = control.request_seek();
    control.finish_seek(seek_generation);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let mut scheduler = PlaybackOutputScheduler::new();
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        1_050_500_000_000,
        1_050_500_000_000,
        seek_generation,
        Instant::now(),
    );
    assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 7));
    let token = prepared_token(
        transaction.transaction_id,
        transaction.discontinuity_epoch,
        seek_generation,
    );
    assert!(scheduler.finish_initial_audio_prepare(token));
    assert!(scheduler.commit_initial_audio_prepare(token));
    assert!(control.compare_and_commit_audio_output_start(seek_generation, || true));

    let mismatched_token = InitialAudioPrepareToken {
        staged_samples: token.staged_samples.saturating_add(1),
        ..token
    };
    assert!(!scheduler.finalize_initial_audio_prepare(mismatched_token, session_id));

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.initial_av_start_pending);
    assert!(scheduler.initial_av_start_transaction().is_none());
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );
    assert!(
        !control
            .audio_output_control_snapshot()
            .paused_by_seek_transition()
    );
}

#[test]
fn prepared_token_must_match_the_epoch_recorded_by_preparing() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        1_050_500_000_000,
        1_050_500_000_000,
        0,
        Instant::now(),
    );
    assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 8));
    let token = prepared_token(
        transaction.transaction_id,
        transaction.discontinuity_epoch,
        0,
    );

    assert!(!scheduler.finish_initial_audio_prepare(token));
    assert_eq!(
        scheduler.initial_audio_prepare_phase(),
        InitialAudioPreparePhase::Preparing
    );
}

#[test]
fn new_seek_after_prepared_prevents_old_audio_activation() {
    let session_id = PlaybackSessionId(45);
    let control = FfmpegControl::new(session_id);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let mut scheduler = PlaybackOutputScheduler::new();
    let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
        1_050_500_000_000,
        1_050_500_000_000,
        0,
        Instant::now(),
    );
    assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 7));
    let token = prepared_token(
        transaction.transaction_id,
        transaction.discontinuity_epoch,
        0,
    );
    assert!(scheduler.finish_initial_audio_prepare(token));

    let new_seek = control.request_seek();
    assert_eq!(new_seek, 1);
    assert!(scheduler.commit_initial_audio_prepare(token));
    let activated = AtomicBool::new(false);
    assert!(!control.compare_and_commit_audio_output_start(0, || {
        activated.store(true, Ordering::Release);
        true
    }));
    assert!(!activated.load(Ordering::Acquire));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );
    scheduler.abort_initial_audio_prepare(
        token.transaction_id,
        session_id,
        "new_seek_before_activate",
    );
    assert!(scheduler.restart_pending());
}

#[test]
fn production_stage_interrupt_checkpoints_abort_losslessly() {
    const FRAMES: usize = 4;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Injection {
        Reset,
        PendingPop(usize),
        FirstEnqueue,
        StableSnapshot,
    }

    let mut injections = vec![
        Injection::Reset,
        Injection::FirstEnqueue,
        Injection::StableSnapshot,
    ];
    injections.extend((0..FRAMES).map(Injection::PendingPop));

    for (case_index, injection) in injections.into_iter().enumerate() {
        let mut state = ProductionInitialAudioState::new(case_index as u64, FRAMES);
        if injection == Injection::Reset {
            state.control.request_seek();
        }
        let checkpoint_control = Arc::clone(&state.control);
        let result = state.stage(move |checkpoint| match (injection, checkpoint) {
            (Injection::PendingPop(expected), AudioStageCheckpoint::PendingPopped(actual))
                if expected == actual =>
            {
                checkpoint_control.request_seek();
            }
            (Injection::FirstEnqueue, AudioStageCheckpoint::FirstEnqueued) => {
                checkpoint_control.request_seek();
            }
            _ => {}
        });

        let token = if injection == Injection::StableSnapshot {
            assert!(!result.interrupted, "injection={injection:?}");
            let token = state.prepared_token(result);
            let snapshot = state.stable_snapshot();
            assert_eq!(
                classify_prestart_audio_ownership(PrestartAudioOwnershipInput {
                    phase: InitialAudioPreparePhase::Prepared,
                    token: Some(token),
                    current_audio_epoch: state.output.audio_epoch(),
                    current_seek_generation: state.control.seek_generation(),
                    target_nsecs: PRODUCTION_STAGE_TARGET_NSECS,
                    snapshot: AudioOutputStableSnapshot::Stable(snapshot),
                }),
                PrestartAudioOwnership::PreparedCurrentEpoch
            );
            state.control.request_seek();
            token
        } else {
            assert!(
                result.interrupted || state.control.should_interrupt(),
                "injection={injection:?}"
            );
            state.preparing_token
        };

        state.abort(token, "injected_production_stage_interrupt");
        state.assert_lossless_retry_state();
    }
}

#[test]
fn production_stage_stops_before_a_real_audio_timeline_gap() {
    let mut state = ProductionInitialAudioState::new(50, 0);
    let first_end_nsecs =
        PRODUCTION_STAGE_TARGET_NSECS.saturating_add(PRODUCTION_STAGE_FRAME_NSECS);
    let second_start_nsecs = first_end_nsecs.saturating_add(40_000_000);
    let second_end_nsecs = second_start_nsecs.saturating_add(PRODUCTION_STAGE_FRAME_NSECS);
    for (start_timeline_nsecs, end_timeline_nsecs) in [
        (PRODUCTION_STAGE_TARGET_NSECS, first_end_nsecs),
        (second_start_nsecs, second_end_nsecs),
    ] {
        state.scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.25; PRODUCTION_STAGE_SAMPLES_PER_FRAME],
                duration_nsecs: PRODUCTION_STAGE_FRAME_NSECS,
            },
            start_timeline_nsecs,
            end_timeline_nsecs,
        );
    }
    state.frame_count = 2;

    let result = state.stage_until(second_end_nsecs, |_| {});
    assert_eq!(result.staged_frames, 1);
    assert_eq!(
        result.staged_range_nsecs,
        Some((PRODUCTION_STAGE_TARGET_NSECS, first_end_nsecs))
    );
    assert_eq!(state.scheduler.pending_start_audio.len(), 1);
    assert_eq!(
        state.scheduler.pending_start_audio.range_nsecs(),
        Some((second_start_nsecs, second_end_nsecs))
    );
    let snapshot = state.stable_snapshot();
    assert_eq!(
        snapshot.payload_range_nsecs,
        Some((PRODUCTION_STAGE_TARGET_NSECS, first_end_nsecs))
    );

    let token = state.prepared_token(result);
    state.abort(token, "timeline_gap_test_cleanup");
    assert_eq!(state.scheduler.pending_start_audio.len(), 2);
    assert_eq!(
        state.scheduler.pending_start_audio.queued_samples(),
        2 * PRODUCTION_STAGE_SAMPLES_PER_FRAME
    );
    assert_eq!(
        state.scheduler.pending_start_audio.range_nsecs(),
        Some((PRODUCTION_STAGE_TARGET_NSECS, second_end_nsecs))
    );
}

#[test]
fn production_commit_checkpoints_respect_the_atomic_point_of_no_return() {
    const FRAMES: usize = 4;

    let mut before_control = ProductionInitialAudioState::new(100, FRAMES);
    assert!(!before_control.output.stream_active_for_test());
    let result = before_control.stage(|_| {});
    let token = before_control.prepared_token(result);
    let seek_control = Arc::clone(&before_control.control);
    assert!(!commit_initial_audio_stage_with_checkpoints_for_test(
        &mut before_control.scheduler,
        &before_control.output,
        token,
        before_control.session_id,
        &before_control.event_tx,
        &before_control.control,
        move |checkpoint, _stream_active| {
            if checkpoint == InitialAudioCommitCheckpoint::SchedulerCommitted {
                seek_control.request_seek();
            }
        },
    ));
    before_control.assert_lossless_retry_state();
    assert_eq!(
        before_control.control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );

    let mut after_control = ProductionInitialAudioState::new(101, FRAMES);
    assert!(!after_control.output.stream_active_for_test());
    let result = after_control.stage(|_| {});
    let token = after_control.prepared_token(result);
    let seek_control = Arc::clone(&after_control.control);
    assert!(!commit_initial_audio_stage_with_checkpoints_for_test(
        &mut after_control.scheduler,
        &after_control.output,
        token,
        after_control.session_id,
        &after_control.event_tx,
        &after_control.control,
        move |checkpoint, _stream_active| {
            if checkpoint == InitialAudioCommitCheckpoint::ControlCommitted {
                seek_control.request_seek();
            }
        },
    ));
    assert!(!after_control.output.stream_active_for_test());
    after_control.assert_lossless_retry_state();
    assert_eq!(
        after_control.control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );

    let mut after_stream = ProductionInitialAudioState::new(109, FRAMES);
    let result = after_stream.stage(|_| {});
    let token = after_stream.prepared_token(result);
    assert!(commit_initial_audio_stage_with_checkpoints_for_test(
        &mut after_stream.scheduler,
        &after_stream.output,
        token,
        after_stream.session_id,
        &after_stream.event_tx,
        &after_stream.control,
        |checkpoint, stream_active| match checkpoint {
            InitialAudioCommitCheckpoint::SchedulerCommitted
            | InitialAudioCommitCheckpoint::ControlCommitted => assert!(!stream_active),
            InitialAudioCommitCheckpoint::StreamPlayed => assert!(stream_active),
        },
    ));
    assert!(after_stream.output.stream_active_for_test());
    assert_eq!(after_stream.output.stream_control_counts_for_test().0, 1);
    assert!(!after_stream.scheduler.restart_pending());
    assert!(
        after_stream
            .scheduler
            .initial_av_start_transaction()
            .is_none()
    );
    let committed_snapshot = after_stream.stable_snapshot();
    assert!(committed_snapshot.queue_active);
    assert_eq!(committed_snapshot.queue_frames, FRAMES);
    assert_eq!(committed_snapshot.shared_payload_nsecs, 0);
    assert_eq!(
        after_stream.control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );

    let new_seek_generation = after_stream.control.request_seek();
    let new_target_nsecs = PRODUCTION_STAGE_TARGET_NSECS.saturating_add(1_000_000_000);
    after_stream.output.reset_clock(new_target_nsecs);
    let reset_snapshot = after_stream.stable_snapshot();
    assert_eq!(reset_snapshot.total_pending_nsecs, 0);
    assert!(!reset_snapshot.queue_active);
    after_stream.control.finish_seek(new_seek_generation);
    after_stream
        .control
        .set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let new_epoch = after_stream.output.audio_epoch();
    assert!(matches!(
        after_stream.output.try_push_timed_for_epoch(
            vec![0.5; PRODUCTION_STAGE_SAMPLES_PER_FRAME],
            new_target_nsecs,
            new_target_nsecs.saturating_add(PRODUCTION_STAGE_FRAME_NSECS),
            new_epoch,
            &after_stream.control,
        ),
        Ok(AudioOutputPushResult::Queued)
    ));
    assert!(after_stream.output.activate_audio_output(
        new_epoch,
        new_seek_generation,
        &after_stream.control,
    ));
    let new_generation_snapshot = after_stream.stable_snapshot();
    assert!(new_generation_snapshot.queue_active);
    assert_eq!(new_generation_snapshot.audio_epoch, new_epoch);
    assert_eq!(new_generation_snapshot.queue_generation, new_epoch);
    assert_eq!(
        new_generation_snapshot.payload_range_nsecs,
        Some((
            new_target_nsecs,
            new_target_nsecs.saturating_add(PRODUCTION_STAGE_FRAME_NSECS),
        ))
    );
}

#[test]
fn mock_stream_play_and_pause_do_not_wait_for_internal_ao_locks() {
    let session_id = PlaybackSessionId(108);
    let control = Arc::new(FfmpegControl::new(session_id));
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    let output = Arc::new(AudioOutput::stopped_for_test(
        Arc::clone(&control),
        4_096,
        44_100,
        2,
    ));
    assert!(!output.stream_active_for_test());

    let release = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = mpsc::channel();
    let blocked_output = Arc::clone(&output);
    let blocked_release = Arc::clone(&release);
    let blocker = std::thread::spawn(move || {
        blocked_output.hold_internal_locks_until_for_test(entered_tx, &blocked_release)
    });
    entered_rx.recv().unwrap();

    let control_started_at = Instant::now();
    assert!(output.activate_audio_output(
        output.audio_epoch(),
        control.seek_generation(),
        &control,
    ));
    assert!(output.stream_active_for_test());
    output.deactivate();
    assert!(!output.stream_active_for_test());
    assert!(control_started_at.elapsed() < Duration::from_millis(20));
    assert_eq!(output.stream_control_counts_for_test(), (1, 1));

    release.store(true, Ordering::Release);
    blocker.join().unwrap();
}

#[test]
fn production_paused_seek_commit_preserves_user_pause_in_the_atomic_state_word() {
    let mut state = ProductionInitialAudioState::new(102, 2);
    state.control.set_user_paused(true);
    let result = state.stage(|_| {});
    let token = state.prepared_token(result);

    assert!(commit_initial_audio_stage_with_checkpoints_for_test(
        &mut state.scheduler,
        &state.output,
        token,
        state.session_id,
        &state.event_tx,
        &state.control,
        |checkpoint, stream_active| match checkpoint {
            InitialAudioCommitCheckpoint::SchedulerCommitted
            | InitialAudioCommitCheckpoint::ControlCommitted => assert!(!stream_active),
            InitialAudioCommitCheckpoint::StreamPlayed => assert!(stream_active),
        },
    ));

    let output_snapshot = state.stable_snapshot();
    assert!(output_snapshot.queue_active);
    assert_eq!(output_snapshot.audio_epoch, token.audio_epoch);
    let paused_state = state.control.audio_output_control_snapshot();
    assert_eq!(paused_state.lifecycle(), AudioOutputLifecycle::Playing);
    assert!(paused_state.paused_by_user());
    assert!(!paused_state.paused_by_seek_transition());
    assert_eq!(paused_state.decision(), AudioOutputDecision::Silence);

    state.control.set_user_paused(false);
    assert_eq!(
        state.control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn production_commit_races_repeat_ten_thousand_times_without_unwind() {
    for iteration in 0..10_000_u64 {
        let mut state = ProductionInitialAudioState::new(10_000 + iteration, 1);
        let result = state.stage(|_| {});
        let token = state.prepared_token(result);
        let seek_control = Arc::clone(&state.control);
        let interrupt_checkpoint = iteration % 3;
        let committed = commit_initial_audio_stage_with_checkpoints_for_test(
            &mut state.scheduler,
            &state.output,
            token,
            state.session_id,
            &state.event_tx,
            &state.control,
            move |checkpoint, _stream_active| match checkpoint {
                InitialAudioCommitCheckpoint::SchedulerCommitted if interrupt_checkpoint == 0 => {
                    seek_control.request_seek();
                }
                InitialAudioCommitCheckpoint::ControlCommitted if interrupt_checkpoint == 1 => {
                    seek_control.request_seek();
                }
                _ => {}
            },
        );
        assert_eq!(
            committed,
            interrupt_checkpoint == 2,
            "iteration={iteration}"
        );
        if committed {
            state.output.reset_clock(PRODUCTION_STAGE_TARGET_NSECS);
            let snapshot = state.stable_snapshot();
            assert_eq!(snapshot.total_pending_nsecs, 0, "iteration={iteration}");
            assert!(!snapshot.queue_active, "iteration={iteration}");
        } else {
            state.assert_lossless_retry_state();
        }
    }
}

#[test]
fn every_prepare_interrupt_checkpoint_has_one_terminal_state_and_conserves_frames() {
    const FRAMES: usize = 19;
    const SAMPLES_PER_FRAME: usize = 2_048;
    const SAMPLES: usize = FRAMES * SAMPLES_PER_FRAME;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Checkpoint {
        AfterReset,
        AfterPendingPop(usize),
        AfterFirstEnqueue,
        AfterSnapshot,
        AfterSchedulerCommit,
        AfterControlCommit,
        NoInterrupt,
    }

    let mut checkpoints = vec![
        Checkpoint::AfterReset,
        Checkpoint::AfterFirstEnqueue,
        Checkpoint::AfterSnapshot,
        Checkpoint::AfterSchedulerCommit,
        Checkpoint::AfterControlCommit,
        Checkpoint::NoInterrupt,
    ];
    checkpoints.extend((0..FRAMES).map(Checkpoint::AfterPendingPop));

    for (case_index, checkpoint) in checkpoints.into_iter().enumerate() {
        let session_id = PlaybackSessionId(100 + case_index as u64);
        let control = FfmpegControl::new(session_id);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
        let mut scheduler = PlaybackOutputScheduler::new();
        let transaction = scheduler.begin_initial_av_start_transaction_for_generations(
            1_050_500_000_000,
            1_050_500_000_000,
            0,
            Instant::now(),
        );
        let mut token = prepared_token(
            transaction.transaction_id,
            transaction.discontinuity_epoch,
            0,
        );
        token.staged_frames = FRAMES;
        token.staged_samples = SAMPLES;
        let mut pending_frames = FRAMES;
        let mut pending_samples = SAMPLES;
        let mut staged_frames = 0usize;
        let mut staged_samples = 0usize;
        let mut held_frames = 0usize;
        let mut held_samples = 0usize;
        let mut committed = false;
        let mut aborted = false;

        if checkpoint == Checkpoint::AfterReset {
            aborted = true;
        } else {
            assert!(scheduler.begin_initial_audio_prepare(transaction.transaction_id, 7));
            for frame_index in 0..FRAMES {
                pending_frames -= 1;
                pending_samples -= SAMPLES_PER_FRAME;
                held_frames = 1;
                held_samples = SAMPLES_PER_FRAME;
                if checkpoint == Checkpoint::AfterPendingPop(frame_index) {
                    aborted = true;
                    break;
                }
                held_frames = 0;
                held_samples = 0;
                staged_frames += 1;
                staged_samples += SAMPLES_PER_FRAME;
                if checkpoint == Checkpoint::AfterFirstEnqueue && frame_index == 0 {
                    aborted = true;
                    break;
                }
            }

            if !aborted {
                assert_eq!(staged_frames, FRAMES);
                assert_eq!(staged_samples, SAMPLES);
                assert!(scheduler.finish_initial_audio_prepare(token));
                if checkpoint == Checkpoint::AfterSnapshot {
                    aborted = true;
                } else {
                    assert!(scheduler.commit_initial_audio_prepare(token));
                    if checkpoint == Checkpoint::AfterSchedulerCommit {
                        aborted = true;
                    } else {
                        assert!(control.compare_and_commit_audio_output_start(0, || true));
                        // The control commit is the point of no return. An
                        // interrupt observed after it completes the old
                        // transaction; the following seek/reset owns cleanup.
                        assert!(scheduler.finalize_initial_audio_prepare(token, session_id));
                        committed = true;
                    }
                }
            }
        }

        if aborted {
            pending_frames = pending_frames
                .saturating_add(staged_frames)
                .saturating_add(held_frames);
            pending_samples = pending_samples
                .saturating_add(staged_samples)
                .saturating_add(held_samples);
            staged_frames = 0;
            staged_samples = 0;
            held_frames = 0;
            held_samples = 0;
            scheduler.abort_initial_audio_prepare(
                transaction.transaction_id,
                session_id,
                "injected_interrupt",
            );
        }
        assert_ne!(committed, aborted, "checkpoint={checkpoint:?}");
        assert_eq!(held_frames, 0, "checkpoint={checkpoint:?}");
        assert_eq!(held_samples, 0, "checkpoint={checkpoint:?}");
        assert_eq!(
            pending_frames.saturating_add(staged_frames),
            FRAMES,
            "checkpoint={checkpoint:?}"
        );
        assert_eq!(
            pending_samples.saturating_add(staged_samples),
            SAMPLES,
            "checkpoint={checkpoint:?}"
        );
        assert_eq!(
            control.audio_output_lifecycle() == AudioOutputLifecycle::Playing,
            committed,
            "checkpoint={checkpoint:?}"
        );
    }
}
