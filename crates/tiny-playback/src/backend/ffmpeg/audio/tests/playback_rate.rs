use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use super::super::{
    AudioBuffer, AudioOutput, AudioOutputLifecycle, AudioQueueItem, AudioQueueState, FfmpegControl,
};
use crate::render_host::PlaybackSessionId;

fn raw_frame(start: u64, frames: usize) -> AudioQueueItem {
    let duration = frames as u64 * 1_000_000_000 / 48_000;
    AudioQueueItem {
        samples: (0..frames)
            .flat_map(|index| {
                let value = (index as f32 * std::f32::consts::TAU * 1000.0 / 48_000.0).sin() * 0.25;
                [value, value]
            })
            .collect(),
        start_timeline_nsecs: start,
        end_timeline_nsecs: start + duration,
        duration_nsecs: duration,
        generation: 0,
    }
}

#[test]
fn output_worker_bounds_old_rate_audio_and_still_fills_underrun_after_live_update() {
    use super::super::{
        AudioQueueShared, AudioShared, AudioTimelineState, fill_audio_output,
        spawn_audio_queue_worker,
    };
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let timeline = Arc::new(AudioTimelineState::new(true));
    let shared = Arc::new(AudioShared::with_timeline(
        96_000,
        48_000,
        2,
        Arc::clone(&control),
        Arc::clone(&timeline),
    ));
    let queue = Arc::new(AudioQueueShared::with_timeline(
        Arc::clone(&control),
        timeline,
    ));
    for index in 0..120 {
        queue
            .state
            .lock()
            .unwrap()
            .push(raw_frame(index * 10_000_000, 480));
    }
    let worker = spawn_audio_queue_worker(Arc::clone(&shared), Arc::clone(&queue)).unwrap();
    fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        ready()
    }
    let initial_ready = wait_until(|| shared.snapshot().unwrap().buffered_nsecs >= 200_000_000);
    let initial_samples = shared.buffer.lock().unwrap().len();
    let raw_frames_left = queue.state.lock().unwrap().items.len();
    control.set_playback_rate(4.0);
    shared.mark_underrun(0);
    shared.ready.notify_all();
    let recovered = wait_until(|| shared.snapshot().unwrap().buffered_nsecs >= 250_000_000);
    let mut samples = [0.0_f32; 960];
    fill_audio_output(&mut samples, &shared);
    queue.shutdown();
    worker.join().unwrap();
    assert!(initial_ready);
    assert!(
        initial_samples <= 20_160,
        "only about 200 ms should be stretched ahead: {initial_samples}"
    );
    assert!(
        raw_frames_left >= 100,
        "read-ahead must retain raw PCM: {raw_frames_left}"
    );
    assert!(
        recovered,
        "AO demand limit must allow the 250 ms underrun prefill"
    );
    assert!(!shared.underrun_active_for_test());
    assert!(samples.iter().any(|sample| sample.abs() > 0.01));
    assert_eq!(queue.generation(), 0, "speed changes do not reset AO");
}

#[test]
fn queued_raw_pcm_uses_the_latest_rate_when_the_output_worker_requests_it() {
    let mut queue = AudioQueueState::new();
    for index in 0..80 {
        queue.push(raw_frame(index * 10_000_000, 480));
    }
    for _ in 0..10 {
        let frame = queue.pop_filtered(48_000, 2, 1.0, 0).unwrap().unwrap();
        assert_eq!(frame.samples.len(), 960);
        queue.finish_item(frame.samples.len(), frame.duration_nsecs);
    }
    assert_eq!(queue.pending_duration(), Duration::from_millis(700));
    queue.input_eof = true;
    let mut samples = 0;
    let mut end = 100_000_000;
    while let Some(frame) = queue.pop_filtered(48_000, 2, 4.0, 0).unwrap() {
        assert_eq!(frame.start_timeline_nsecs, end);
        end = frame.end_timeline_nsecs;
        samples += frame.samples.len();
        queue.finish_item(frame.samples.len(), frame.duration_nsecs);
    }
    let wall_seconds = samples as f64 / 96_000.0;
    assert!((wall_seconds - 0.175).abs() < 0.03, "{wall_seconds}");
    assert!(end.abs_diff(800_000_000) < 50_000_000, "{end}");
    assert_eq!(queue.pending_duration(), Duration::ZERO);
    assert_eq!(queue.queued_samples, 0);
}

#[test]
fn mixed_rates_keep_ring_and_device_latency_in_their_original_media_time() {
    let mut buffer = AudioBuffer::with_capacity(9_600);
    buffer.push_timed_slice(&vec![0.25; 4_800], 0, 100_000_000);
    buffer.push_timed_slice(&vec![0.5; 4_800], 100_000_000, 500_000_000);
    assert_eq!(buffer.media_duration_nsecs(), Some(500_000_000));
    for now in [0, 40_000_000, 80_000_000] {
        for _ in 0..1_920 {
            buffer.pop_sample().unwrap();
        }
        buffer.consume_timing(1_920, now + 20_000_000, now, 48_000, 1);
    }
    // The third callback straddles 1x and 4x. Hardware is still playing the
    // earlier 1x block while the ring contains only new 4x samples.
    assert_eq!(buffer.media_duration_nsecs(), Some(320_000_000));
    assert_eq!(buffer.device_delay_nsecs(80_000_000), Some(120_000_000));
    assert_eq!(buffer.device_delay_nsecs(100_000_000), Some(100_000_000));
    assert_eq!(buffer.device_delay_nsecs(120_000_000), Some(80_000_000));
    assert_eq!(buffer.device_delay_nsecs(130_000_000), Some(40_000_000));
    assert_eq!(buffer.device_delay_nsecs(140_000_000), Some(0));
    buffer.clear();
    assert_eq!(buffer.media_duration_nsecs(), None);
    assert_eq!(buffer.device_delay_nsecs(0), None);
}

#[test]
fn eof_drains_tempo_latency_and_seek_reset_discards_it() {
    for reset in [false, true] {
        let mut queue = AudioQueueState::new();
        for index in 0..4 {
            queue.push(raw_frame(index * 20_000_000, 960));
        }
        let mut total_samples = 0;
        while let Some(frame) = queue.pop_filtered(48_000, 2, 2.0, 0).unwrap() {
            total_samples += frame.samples.len();
            queue.finish_item(frame.samples.len(), frame.duration_nsecs);
        }
        assert!(
            queue.pending_duration() > Duration::ZERO,
            "filter owns delayed PCM"
        );
        if reset {
            queue.clear();
            queue.push(raw_frame(10_000_000_000, 4_800));
        }
        queue.input_eof = true;
        let mut tail_samples = 0;
        while let Some(frame) = queue
            .pop_filtered(48_000, 2, if reset { 1.0 } else { 2.0 }, 0)
            .unwrap()
        {
            if reset {
                assert!(frame.start_timeline_nsecs >= 10_000_000_000);
            }
            tail_samples += frame.samples.len();
            queue.finish_item(frame.samples.len(), frame.duration_nsecs);
        }
        assert!(tail_samples > 0);
        if reset {
            assert_eq!(tail_samples, 9_600);
        } else {
            assert!(((total_samples + tail_samples) as f64 / 96_000.0 - 0.04).abs() < 0.015);
        }
        assert_eq!(queue.pending_duration(), Duration::ZERO);
        assert_eq!(queue.queued_samples, 0);
    }
}

#[test]
fn rate_update_preserves_playing_audio_epoch_and_uses_mixed_drain_durations() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    control.set_playback_rate(0.25);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 96_000, 48_000, 2);
    output.reset_clock(10_000_000_000);
    output.stage_filtered_audio_for_test(vec![0.25; 57_600], 10_000_000_000, 10_150_000_000);
    assert!(output.activate_current_audio_output(&control));
    assert!(output.transfer_next_queued_frame_for_test().unwrap());
    let before = output.snapshot().unwrap();
    let stream_counts = output.stream_control_counts_for_test();
    control.set_playback_rate(4.0);
    let after = output.snapshot().unwrap();
    assert_eq!(after.played_timeline_nsecs, before.played_timeline_nsecs);
    assert_eq!(after.shared_payload_nsecs, 150_000_000);
    assert_eq!(after.audio_epoch, before.audio_epoch);
    assert_eq!(output.stream_control_counts_for_test(), stream_counts);
    let wait = output
        .drain_deadline()
        .unwrap()
        .unwrap()
        .saturating_duration_since(Instant::now());
    assert!(
        (Duration::from_millis(830)..=Duration::from_millis(850)).contains(&wait),
        "{wait:?}"
    );
    assert!(!output.underrun_active());
}
