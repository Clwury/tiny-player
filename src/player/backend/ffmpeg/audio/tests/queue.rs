use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::player::render_host::PlaybackSessionId;

use super::super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AudioBuffer,
    AudioOutput, AudioOutputServiceStage, AudioOutputStableSnapshot, AudioQueueInFlight,
    AudioQueueItem, AudioQueueShared, AudioQueueState, AudioShared, AudioTimelineState,
    FfmpegControl, audio_elements_duration, duration_nsecs, fill_audio_output,
    spawn_audio_queue_worker, stable_audio_output_snapshot_with_compose_hook_for_test,
    stable_audio_output_snapshot_with_hook_for_test,
    stable_audio_output_snapshot_with_retry_hook_for_test, write_audio_queue_item,
};

#[test]
fn six_ao_service_stages_publish_atomic_begin_end_and_elapsed() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let output = AudioOutput::stopped_for_test(control, 4_800, 48_000, 2);
    let stages = [
        AudioOutputServiceStage::StatusSnapshot,
        AudioOutputServiceStage::StableSnapshot,
        AudioOutputServiceStage::ResetClock,
        AudioOutputServiceStage::StagePending,
        AudioOutputServiceStage::PreparedSnapshot,
        AudioOutputServiceStage::ControlCommit,
    ];

    for stage in stages {
        let guard = output.begin_service_stage(stage);
        let active = output
            .service_stage_snapshots_for_test()
            .into_iter()
            .find(|snapshot| snapshot.stage == stage)
            .unwrap();
        assert!(active.active, "{} did not publish begin", stage.as_str());
        drop(guard);
        let completed = output
            .service_stage_snapshots_for_test()
            .into_iter()
            .find(|snapshot| snapshot.stage == stage)
            .unwrap();
        assert!(!completed.active, "{} did not publish end", stage.as_str());
        assert_eq!(completed.started_count, 1);
        assert_eq!(completed.completed_count, 1);
    }
}

#[test]
fn callback_reset_and_snapshot_stress_keeps_each_probe_bounded() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(2)));
    let output = Arc::new(AudioOutput::stopped_for_test(
        Arc::clone(&control),
        44_100 * 2,
        44_100,
        2,
    ));
    control.set_audio_output_lifecycle(super::super::AudioOutputLifecycle::Playing);
    let stop = Arc::new(AtomicBool::new(false));

    let callback_output = Arc::clone(&output);
    let callback_stop = Arc::clone(&stop);
    let callback = std::thread::spawn(move || {
        let mut samples = vec![0.0_f32; 256];
        while !callback_stop.load(Ordering::Acquire) {
            fill_audio_output(&mut samples, &callback_output.shared);
        }
    });
    let reset_output = Arc::clone(&output);
    let reset = std::thread::spawn(move || {
        for index in 0..500_u64 {
            reset_output.reset_clock(104_745_215_349 + index * 1_000_000);
        }
    });

    let mut max_probe = Duration::ZERO;
    for _ in 0..1_000 {
        let started_at = Instant::now();
        let _ = output.try_snapshot().unwrap();
        max_probe = max_probe.max(started_at.elapsed());

        let started_at = Instant::now();
        let _ = output.stable_snapshot().unwrap();
        max_probe = max_probe.max(started_at.elapsed());
    }
    reset.join().unwrap();
    stop.store(true, Ordering::Release);
    callback.join().unwrap();

    assert!(
        max_probe < Duration::from_millis(50),
        "coordinator AO probe exceeded bound: {max_probe:?}"
    );
}

#[test]
fn video_deadline_audio_clock_probe_never_waits_for_audio_queue_or_buffer_locks() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(3)));
    let output = Arc::new(AudioOutput::stopped_for_test(control, 4_800, 48_000, 2));
    output.reset_clock(2_000_000_000);
    output.shared.activate_for_test();
    let clock = output.clock_handle();
    let release = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let locked_output = Arc::clone(&output);
    let locked_release = Arc::clone(&release);
    let lock_holder = std::thread::spawn(move || {
        locked_output.hold_internal_locks_until_for_test(entered_tx, &locked_release);
    });
    entered_rx.recv().expect("audio locks are held");
    let started_at = Instant::now();

    for _ in 0..10_000 {
        assert_eq!(clock.played_timeline_nsecs(), Some(2_000_000_000));
    }

    let elapsed = started_at.elapsed();
    release.store(true, Ordering::Release);
    lock_holder.join().expect("audio lock holder joins");
    assert!(
        elapsed < Duration::from_millis(10),
        "lock-free audio clock probe was delayed by AO locks: {elapsed:?}"
    );
}

#[test]
fn audio_output_queue_uses_short_output_backpressure_limit() {
    let mut state = AudioQueueState::new();

    assert!(state.can_accept(1));

    state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION) - 1;
    assert!(state.can_accept(1));
    assert!(!state.can_accept(2));

    state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION);
    assert!(!state.can_accept(1));

    state.queued_duration_nsecs = 0;
    assert!(!state.can_accept(duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION).saturating_add(1)));
}
#[test]
fn audio_output_queue_keeps_eac3_recovery_margin() {
    let mut state = AudioQueueState::new();
    state.queued_duration_nsecs = duration_nsecs(
        AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION.saturating_add(Duration::from_millis(32)),
    );

    assert!(state.can_accept(32_000_000));
}
#[test]
fn audio_queue_write_progress_removes_in_flight_pending_duration() {
    let sample_rate = 48_000;
    let channels = 2;
    let samples = vec![0.25; 8];
    let duration_nsecs = duration_nsecs(audio_elements_duration(
        samples.len(),
        sample_rate,
        channels,
    ));
    let start_timeline_nsecs = 1_000_000_000u64;
    let end_timeline_nsecs = start_timeline_nsecs.saturating_add(duration_nsecs);
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let shared = AudioShared::new(samples.len(), sample_rate, channels, Arc::clone(&control));
    let queue = AudioQueueShared::new(control);
    {
        let mut state = queue.state.lock().unwrap();
        state.queued_samples = samples.len();
        state.queued_duration_nsecs = duration_nsecs;
    }

    let progress = write_audio_queue_item(
        &shared,
        &queue,
        AudioQueueItem {
            samples,
            start_timeline_nsecs,
            end_timeline_nsecs,
            duration_nsecs,
            generation: queue.generation(),
        },
    )
    .unwrap();

    assert_eq!(progress.samples, 8);
    assert_eq!(progress.duration_nsecs, duration_nsecs);
    assert_eq!(queue.snapshot().unwrap().pending_nsecs, 0);
}

#[test]
fn stale_finish_item_does_not_debit_the_new_epoch_queue() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let queue = AudioQueueShared::with_timeline(control, Arc::clone(&timeline));
    let stale_generation = timeline.epoch();
    let current_generation = timeline.advance_epoch();
    let duration_nsecs = 10_000_000;
    {
        let mut state = queue.state.lock().unwrap();
        state.push(AudioQueueItem {
            samples: vec![0.25; 960],
            start_timeline_nsecs: 2_000_000_000,
            end_timeline_nsecs: 2_010_000_000,
            duration_nsecs,
            generation: current_generation,
        });
    }

    queue.finish_item(stale_generation, 960, duration_nsecs);

    let snapshot = queue.snapshot().unwrap();
    assert_eq!(snapshot.generation, current_generation);
    assert_eq!(snapshot.frames, 1);
    assert_eq!(snapshot.pending_nsecs, duration_nsecs);
}

#[test]
fn epoch_advance_while_finish_holds_the_queue_lock_is_fenced() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let queue = Arc::new(AudioQueueShared::with_timeline(
        control,
        Arc::clone(&timeline),
    ));
    let stale_generation = timeline.epoch();
    let duration_nsecs = 10_000_000;
    {
        let mut state = queue.state.lock().unwrap();
        state.queued_samples = 960;
        state.queued_duration_nsecs = duration_nsecs;
        state.in_flight = Some(AudioQueueInFlight {
            generation: stale_generation,
            start_timeline_nsecs: 1_000_000_000,
            end_timeline_nsecs: 1_010_000_000,
            remaining_samples: 960,
            remaining_duration_nsecs: duration_nsecs,
        });
    }

    let queue_locked = Arc::new(Barrier::new(2));
    let epoch_advanced = Arc::new(Barrier::new(2));
    let worker_queue = Arc::clone(&queue);
    let worker_locked = Arc::clone(&queue_locked);
    let worker_advanced = Arc::clone(&epoch_advanced);
    let worker = std::thread::spawn(move || {
        worker_queue.finish_item_with_lock_checkpoint_for_test(
            stale_generation,
            960,
            duration_nsecs,
            || {
                worker_locked.wait();
                worker_advanced.wait();
            },
        );
    });

    queue_locked.wait();
    let current_generation = timeline.advance_epoch();
    epoch_advanced.wait();
    worker.join().unwrap();

    let fenced_snapshot = queue.snapshot().unwrap();
    assert_eq!(fenced_snapshot.generation, current_generation);
    assert_eq!(fenced_snapshot.pending_nsecs, duration_nsecs);
    assert_eq!(fenced_snapshot.in_flight_nsecs, duration_nsecs);

    queue.clear_current_epoch();
    {
        let mut state = queue.state.lock().unwrap();
        state.push(AudioQueueItem {
            samples: vec![0.25; 960],
            start_timeline_nsecs: 2_000_000_000,
            end_timeline_nsecs: 2_010_000_000,
            duration_nsecs,
            generation: current_generation,
        });
    }
    queue.finish_item(stale_generation, 960, duration_nsecs);
    let current_snapshot = queue.snapshot().unwrap();
    assert_eq!(current_snapshot.generation, current_generation);
    assert_eq!(current_snapshot.frames, 1);
    assert_eq!(current_snapshot.pending_nsecs, duration_nsecs);
}

#[test]
fn reset_counts_queued_and_in_flight_items_as_stale() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let queue = AudioQueueShared::with_timeline(control, Arc::clone(&timeline));
    let generation = timeline.epoch();
    {
        let mut state = queue.state.lock().unwrap();
        state.push(AudioQueueItem {
            samples: vec![0.25; 960],
            start_timeline_nsecs: 1_010_000_000,
            end_timeline_nsecs: 1_020_000_000,
            duration_nsecs: 10_000_000,
            generation,
        });
        state.in_flight = Some(AudioQueueInFlight {
            generation,
            start_timeline_nsecs: 1_000_000_000,
            end_timeline_nsecs: 1_010_000_000,
            remaining_samples: 960,
            remaining_duration_nsecs: 10_000_000,
        });
    }

    queue.clear_current_epoch();

    assert_eq!(timeline.stale_queue_items(), 2);
    let snapshot = queue.snapshot().unwrap();
    assert_eq!(snapshot.frames, 0);
    assert_eq!(snapshot.in_flight_frames, 0);
    assert_eq!(snapshot.pending_nsecs, 0);
}
#[test]
fn audio_ring_buffer_reuses_fixed_capacity_and_wraps() {
    let mut buffer = AudioBuffer::with_capacity(4);

    assert_eq!(buffer.push_slice(&[1.0, 2.0, 3.0]), 3);
    assert_eq!(buffer.pop_sample(), Some(1.0));
    assert_eq!(buffer.pop_sample(), Some(2.0));
    assert_eq!(buffer.push_slice(&[4.0, 5.0, 6.0]), 3);
    assert_eq!(buffer.push_slice(&[7.0]), 0);

    assert_eq!(buffer.pop_sample(), Some(3.0));
    assert_eq!(buffer.pop_sample(), Some(4.0));
    assert_eq!(buffer.pop_sample(), Some(5.0));
    assert_eq!(buffer.pop_sample(), Some(6.0));
    assert_eq!(buffer.pop_sample(), None);
}

#[test]
fn stopped_audio_queue_does_not_pull_until_activated() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let shared = Arc::new(AudioShared::with_timeline(
        4_800,
        48_000,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    ));
    let queue = Arc::new(AudioQueueShared::with_timeline(
        control,
        Arc::clone(&timeline),
    ));
    let duration_nsecs = 10_000_000;
    {
        let mut state = queue.state.lock().unwrap();
        state.push(AudioQueueItem {
            samples: vec![0.25; 960],
            start_timeline_nsecs: 1_000_000_000,
            end_timeline_nsecs: 1_010_000_000,
            duration_nsecs,
            generation: timeline.epoch(),
        });
    }
    let worker = spawn_audio_queue_worker(Arc::clone(&shared), Arc::clone(&queue)).unwrap();
    queue.ready.notify_all();
    std::thread::sleep(Duration::from_millis(10));
    let stopped_snapshot = queue.snapshot().unwrap();
    let stopped_shared_nsecs = shared.queued_duration_nsecs();

    timeline.set_active(true);
    queue.ready.notify_all();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = queue.state.lock().unwrap();
    while state.queued_duration_nsecs > 0 && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (next_state, _) = queue.ready.wait_timeout(state, remaining).unwrap();
        state = next_state;
    }
    drop(state);
    let activated_snapshot = queue.snapshot().unwrap();
    let activated_shared_nsecs = shared.queued_duration_nsecs();
    queue.shutdown();
    shared.ready.notify_all();
    worker.join().unwrap();

    assert_eq!(stopped_snapshot.frames, 1);
    assert_eq!(stopped_snapshot.in_flight_frames, 0);
    assert_eq!(stopped_shared_nsecs, 0);
    assert_eq!(activated_snapshot.pending_nsecs, 0);
    assert!(activated_shared_nsecs > 0);
}

#[test]
fn queue_to_shared_transfer_between_snapshot_reads_never_returns_pseudo_zero() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(true));
    let shared = Arc::new(AudioShared::with_timeline(
        4_800,
        48_000,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    ));
    let queue = Arc::new(AudioQueueShared::with_timeline(
        control,
        Arc::clone(&timeline),
    ));
    let duration_nsecs = 10_000_000;
    {
        let mut state = queue.state.lock().unwrap();
        state.queued_samples = 960;
        state.queued_duration_nsecs = duration_nsecs;
    }
    let before_write = Arc::new(Barrier::new(2));
    let after_write = Arc::new(Barrier::new(2));
    let worker_shared = Arc::clone(&shared);
    let worker_queue = Arc::clone(&queue);
    let worker_before = Arc::clone(&before_write);
    let worker_after = Arc::clone(&after_write);
    let worker = std::thread::spawn(move || {
        worker_before.wait();
        write_audio_queue_item(
            &worker_shared,
            &worker_queue,
            AudioQueueItem {
                samples: vec![0.25; 960],
                start_timeline_nsecs: 1_000_000_000,
                end_timeline_nsecs: 1_010_000_000,
                duration_nsecs,
                generation: worker_queue.generation(),
            },
        )
        .unwrap();
        worker_after.wait();
    });
    let mut injected = false;
    let result =
        stable_audio_output_snapshot_with_hook_for_test(&shared, &queue, &timeline, || {
            if !injected {
                injected = true;
                before_write.wait();
                after_write.wait();
            }
        })
        .unwrap();
    worker.join().unwrap();

    let AudioOutputStableSnapshot::Stable(snapshot) = result else {
        panic!("transfer should settle into a stable retry snapshot");
    };
    assert!(snapshot.stable_version.is_some());
    assert_eq!(snapshot.total_pending_nsecs, duration_nsecs);
    assert_eq!(snapshot.shared_payload_nsecs, duration_nsecs);
    assert_eq!(snapshot.queue_pending_nsecs, 0);
}

#[test]
fn stable_snapshot_reports_unstable_after_the_bounded_retry_budget() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let shared = AudioShared::with_timeline(
        4_800,
        48_000,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    );
    let queue = AudioQueueShared::with_timeline(control, Arc::clone(&timeline));

    let snapshot =
        stable_audio_output_snapshot_with_hook_for_test(&shared, &queue, &timeline, || {
            timeline.set_active(!timeline.active())
        })
        .unwrap();

    let AudioOutputStableSnapshot::SnapshotUnstable(unstable) = snapshot else {
        panic!("continuous version changes must exhaust the stable snapshot retry budget");
    };
    assert_eq!(unstable.audio_epoch, timeline.epoch());
    assert_eq!(unstable.attempts, 8);
    assert_eq!(unstable.observed_version, timeline.version());
}

#[test]
fn stable_snapshot_yields_until_a_short_timeline_mutation_finishes() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let shared = AudioShared::with_timeline(
        4_800,
        44_100,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    );
    let queue = AudioQueueShared::with_timeline(control, Arc::clone(&timeline));
    let mutation_started = Arc::new(Barrier::new(2));
    let snapshot_observed_contention = Arc::new(AtomicBool::new(false));
    let writer_timeline = Arc::clone(&timeline);
    let writer_started = Arc::clone(&mutation_started);
    let writer_observed = Arc::clone(&snapshot_observed_contention);
    let writer = std::thread::spawn(move || {
        let mutation = writer_timeline.begin_mutation();
        writer_started.wait();
        while !writer_observed.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        drop(mutation);
    });
    mutation_started.wait();

    let snapshot =
        stable_audio_output_snapshot_with_retry_hook_for_test(&shared, &queue, &timeline, || {
            snapshot_observed_contention.store(true, Ordering::Release)
        })
        .unwrap();
    writer.join().unwrap();

    let AudioOutputStableSnapshot::Stable(snapshot) = snapshot else {
        panic!("a short callback-style mutation must settle within the contention budget");
    };
    assert_eq!(snapshot.audio_epoch, 0);
    assert_eq!(snapshot.queue_generation, 0);
    assert!(snapshot.stable_version.is_some());
}

#[test]
fn stable_snapshot_does_not_reread_timeline_after_validation() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let timeline = Arc::new(AudioTimelineState::new(false));
    let shared = AudioShared::with_timeline(
        4_800,
        48_000,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    );
    let queue = AudioQueueShared::with_timeline(control, Arc::clone(&timeline));

    let snapshot =
        stable_audio_output_snapshot_with_compose_hook_for_test(&shared, &queue, &timeline, || {
            timeline.set_active(true);
            timeline.advance_epoch();
        })
        .unwrap();
    let AudioOutputStableSnapshot::Stable(snapshot) = snapshot else {
        panic!("the pre-mutation snapshot should already be stable");
    };

    assert_eq!(snapshot.audio_epoch, 0);
    assert_eq!(snapshot.queue_generation, 0);
    assert!(!snapshot.queue_active);
    assert_eq!(timeline.epoch(), 1);
    assert!(timeline.active());
}
