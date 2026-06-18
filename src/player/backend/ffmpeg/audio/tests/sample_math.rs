use std::time::Duration;

use super::super::super::FALLBACK_AUDIO_OUTPUT_CHANNELS;
use super::super::{
    align_audio_elements_to_frame_boundary, audio_elements_for_duration_floor,
    audio_elements_for_duration_round, audio_elements_for_frames, audio_frames_for_duration_round,
    audio_samples_duration, samples_for_duration,
};

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
#[test]
fn audio_samples_duration_accounts_for_interleaved_channels() {
    assert_eq!(
        audio_samples_duration(96_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        Duration::from_secs(1)
    );
    assert_eq!(
        audio_samples_duration(0, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        Duration::ZERO
    );
    assert_eq!(audio_samples_duration(1024, 0, 2), Duration::ZERO);
    assert_eq!(audio_samples_duration(1024, 48_000, 0), Duration::ZERO);
}
#[test]
fn samples_for_duration_accounts_for_interleaved_channels() {
    assert_eq!(
        samples_for_duration(1_000_000_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        96_000
    );
    assert_eq!(samples_for_duration(0, 48_000, 2), 0);
    assert_eq!(samples_for_duration(1_000_000_000, 0, 2), 0);
    assert_eq!(samples_for_duration(1_000_000_000, 48_000, 0), 0);
}
