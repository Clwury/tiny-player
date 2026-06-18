use std::{sync::Arc, time::Duration};

use crate::player::render_host::PlaybackSessionId;

use super::super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AudioBuffer,
    AudioQueueItem, AudioQueueShared, AudioQueueState, AudioShared, FfmpegControl,
    audio_elements_duration, duration_nsecs, write_audio_queue_item,
};

#[test]
fn audio_output_queue_uses_short_output_backpressure_limit() {
    let mut state = AudioQueueState::new();

    assert!(state.can_accept());

    state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION) - 1;
    assert!(state.can_accept());

    state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION);
    assert!(!state.can_accept());
}
#[test]
fn audio_output_queue_keeps_eac3_recovery_margin() {
    let mut state = AudioQueueState::new();
    state.queued_duration_nsecs = duration_nsecs(
        AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION.saturating_add(Duration::from_millis(32)),
    );

    assert!(state.can_accept());
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
