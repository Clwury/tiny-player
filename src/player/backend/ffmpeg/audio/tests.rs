#[cfg(test)]
use std::{sync::Arc, time::Duration};

use crate::player::render_host::PlaybackSessionId;

use super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AudioQueueItem,
    AudioQueueShared, AudioQueueState, AudioShared, FfmpegControl,
    align_audio_elements_to_frame_boundary, audio_elements_duration,
    audio_elements_for_duration_floor, audio_elements_for_duration_round,
    audio_elements_for_frames, audio_frames_for_duration_round, duration_nsecs,
    write_audio_queue_item,
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
fn delayed_start_silence_uses_aligned_interleaved_elements_for_stereo_gaps() {
    let first_gap_frames = audio_frames_for_duration_round(22_780_000, 44_100);
    let first_gap_elements = audio_elements_for_frames(first_gap_frames, 2);
    let second_gap_frames = audio_frames_for_duration_round(24_780_000, 44_100);
    let second_gap_elements = audio_elements_for_frames(second_gap_frames, 2);

    assert_eq!(first_gap_frames, 1_005);
    assert_eq!(first_gap_elements, 2_010);
    assert_eq!(first_gap_elements % 2, 0);
    assert_eq!(second_gap_frames, 1_093);
    assert_eq!(second_gap_elements, 2_186);
    assert_eq!(second_gap_elements % 2, 0);
}

#[test]
fn audio_element_helpers_keep_interleaved_buffers_frame_aligned() {
    assert_eq!(align_audio_elements_to_frame_boundary(2_009, 2), 2_008);
    assert_eq!(align_audio_elements_to_frame_boundary(2_185, 2), 2_184);
    assert_eq!(
        audio_elements_for_duration_floor(22_780_000, 44_100, 2),
        2_008
    );
    assert_eq!(
        audio_elements_for_duration_round(22_780_000, 44_100, 2),
        2_010
    );
}
