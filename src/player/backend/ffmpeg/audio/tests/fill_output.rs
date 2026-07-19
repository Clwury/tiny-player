use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use crate::player::render_host::PlaybackSessionId;

use super::super::super::FALLBACK_AUDIO_OUTPUT_CHANNELS;
use super::super::{
    AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION, AudioShared, FfmpegControl, duration_nsecs,
    fill_audio_output,
};

fn test_audio_shared(max_samples: usize) -> AudioShared {
    AudioShared::new(
        max_samples,
        48_000,
        FALLBACK_AUDIO_OUTPUT_CHANNELS,
        Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
    )
}

#[test]
fn audio_clock_uses_queued_end_minus_pending_audio() {
    let shared = test_audio_shared(960);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);

    assert_eq!(shared.played_timeline_nsecs(), 1_000_000_000);
}
#[test]
fn audio_clock_subtracts_output_device_delay() {
    let shared = test_audio_shared(960);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);
    shared.set_output_delay_for_test(Duration::from_millis(20));

    assert_eq!(shared.played_timeline_nsecs(), 980_000_000);
}
#[test]
fn fill_audio_output_converts_samples_and_outputs_silence_on_underrun() {
    let shared = test_audio_shared(8);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.0, 1.0]),
        3
    );
    let mut output = [0.0f64; 4];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [-1.0, 0.0, 1.0, 0.0]);
    assert!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .is_empty()
    );
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
    assert!(shared.underrun_active_for_test());
}
#[test]
fn audio_clock_freezes_during_output_underrun_until_pending_recovers() {
    let shared = test_audio_shared(4_800);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);

    let mut output = [0.0f64; 1_920];
    fill_audio_output(&mut output, &shared);

    let frozen_timeline_nsecs = shared.played_timeline_nsecs();
    assert!((1_000_000_000..=1_010_000_000).contains(&frozen_timeline_nsecs));
    assert!(shared.underrun_active_for_test());

    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(2_000_000_000);
    assert_eq!(shared.played_timeline_nsecs(), frozen_timeline_nsecs);

    shared.clear_underrun_if_recovered_for_test(
        duration_nsecs(AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION) - 1,
    );
    assert!(shared.underrun_active_for_test());
    shared.clear_underrun_if_recovered_for_test(duration_nsecs(
        AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION,
    ));
    assert!(!shared.underrun_active_for_test());
    assert_ne!(shared.played_timeline_nsecs(), frozen_timeline_nsecs);
}
#[test]
fn fill_audio_output_applies_playback_volume() {
    let shared = test_audio_shared(8);
    shared.control.set_volume(0.25);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.5, 1.0]),
        3
    );
    let mut output = [0.0f64; 3];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [-0.25, 0.125, 0.25]);
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
}
#[test]
fn fill_audio_output_preserves_buffer_while_paused() {
    let shared = test_audio_shared(8);
    shared.control.set_user_paused(true);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.0, 1.0]),
        3
    );
    let mut output = [0.5f64; 4];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [0.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .len(),
        3
    );
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 0);
}
