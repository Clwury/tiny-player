use std::io::Write;

use crate::render_host::{DecodedFrame, FramePixels, RawVideoFormat, RawVideoPlanes};
use crate::{BackendControl, BackendLoadRequest, HardwareDecodeMode, PlaybackSeekableCacheMode};

use super::super::seconds_to_nsecs;
use super::*;

fn video_fixture() -> tempfile::NamedTempFile {
    let mut file = tempfile::Builder::new().suffix(".y4m").tempfile().unwrap();
    file.write_all(b"YUV4MPEG2 W16 H16 F10:1 Ip A1:1 C420jpeg\n")
        .unwrap();
    for frame_index in 0..60_u8 {
        file.write_all(b"FRAME\n").unwrap();
        file.write_all(&[16 + frame_index * 3; 16 * 16]).unwrap();
        file.write_all(&[128; 16 * 16 / 2]).unwrap();
    }
    file.flush().unwrap();
    file
}

fn open_video(
    file: &tempfile::NamedTempFile,
    seekable_cache: PlaybackSeekableCacheMode,
) -> FfmpegBackend {
    let mut backend = FfmpegBackend::new().unwrap();
    backend
        .load(BackendLoadRequest {
            url: file.path().to_str().unwrap().to_string(),
            http_headers: Vec::new(),
            content_length: None,
            start_position_seconds: 0.0,
            selected_tracks: Default::default(),
            cache_config: PlaybackCacheConfig {
                hardware_decode: HardwareDecodeMode::Off,
                demuxer_cache_wait: false,
                seekable_cache,
                ..Default::default()
            },
        })
        .unwrap();
    wait_for_frame(&mut backend, 0.0);
    backend.pause().unwrap();
    backend
}

fn wait_for_frame(backend: &mut FfmpegBackend, position_seconds: f64) -> DecodedFrame {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        for event in backend.poll_events() {
            match event.kind {
                BackendEventKind::LoadFailed(error) | BackendEventKind::Fatal(error) => {
                    panic!("fixture playback failed: {error}");
                }
                _ => {}
            }
        }
        if backend.loaded
            && backend.video_output_queue.snapshot().active_session_id == backend.current_session_id
            && let Some(frame) = backend.video_output_queue.take_frame()
            && frame.pts.unwrap().nsecs >= seconds_to_nsecs(position_seconds)
        {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "no video frame after seeking to {position_seconds} while paused: loaded={}, session={:?}, queue={:?}, cache={:?}",
            backend.loaded,
            backend.current_session_id,
            backend.video_output_queue.snapshot(),
            backend.cache_state
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_target_frame(frame: &DecodedFrame, position_seconds: f64) {
    assert_eq!(frame.pts.unwrap().nsecs, seconds_to_nsecs(position_seconds));
    let FramePixels::RawVideo(raw) = &frame.pixels else {
        panic!("software fixture must produce a raw video frame");
    };
    assert_eq!(raw.format, RawVideoFormat::I420);
    let RawVideoPlanes::Owned(planes) = &raw.planes;
    let expected_luma = 16 + (position_seconds * 10.0).round() as u8 * 3;
    assert_eq!(planes[0].data[0], expected_luma);
    assert_eq!(planes[1].data[0], 128);
    assert_eq!(planes[2].data[0], 128);
}

fn assert_remains_paused(backend: &mut FfmpegBackend, position_seconds: f64) {
    thread::sleep(Duration::from_millis(100));
    assert!(
        backend
            .poll_events()
            .iter()
            .all(|event| !matches!(event.kind, BackendEventKind::PlaybackEnded)),
        "paused preview must not end playback"
    );
    assert!(backend.loaded);
    assert!(backend.user_paused);
    assert!(backend.paused);
    assert!(backend.worker.as_ref().unwrap().is_paused());
    assert_eq!(backend.position_seconds, Some(position_seconds));
    assert!(backend.video_output_queue.take_frame().is_none());
}

#[test]
fn paused_seek_displays_forward_and_backward_target_frames_without_resuming() {
    let file = video_fixture();
    for seekable_cache in [
        PlaybackSeekableCacheMode::Auto,
        PlaybackSeekableCacheMode::Disabled,
    ] {
        let mut backend = open_video(&file, seekable_cache);
        for position_seconds in [3.0, 1.0, 4.2] {
            backend.seek_to(position_seconds).unwrap();
            let frame = wait_for_frame(&mut backend, position_seconds);
            assert_target_frame(&frame, position_seconds);
            assert_remains_paused(&mut backend, position_seconds);
        }
        backend.stop().unwrap();
    }
}

#[test]
fn consecutive_paused_seeks_show_the_latest_target_and_resume_from_that_frame() {
    let file = video_fixture();
    let mut backend = open_video(&file, PlaybackSeekableCacheMode::Auto);
    for position_seconds in [3.1, 1.7, 4.8] {
        backend.seek_to(position_seconds).unwrap();
    }
    let frame = wait_for_frame(&mut backend, 4.8);
    assert_target_frame(&frame, 4.8);
    assert_remains_paused(&mut backend, 4.8);

    backend.resume().unwrap();
    let next = wait_for_frame(&mut backend, 4.9);
    assert!(next.pts.unwrap().nsecs > frame.pts.unwrap().nsecs);
    assert!(!backend.user_paused);
    backend.stop().unwrap();
}

#[test]
fn paused_seek_near_eof_can_seek_again_and_hold_the_final_frame() {
    let file = video_fixture();
    let mut backend = open_video(&file, PlaybackSeekableCacheMode::Disabled);
    for position_seconds in [5.0, 1.2, 5.9, 2.0, 5.9] {
        backend.seek_to(position_seconds).unwrap();
        let frame = wait_for_frame(&mut backend, position_seconds);
        assert_target_frame(&frame, position_seconds);
        assert_remains_paused(&mut backend, position_seconds);
    }
    backend.stop().unwrap();
}

#[test]
fn paused_seek_to_exact_duration_displays_final_frame_and_finishes_buffering() {
    let file = video_fixture();
    for cache in [
        PlaybackSeekableCacheMode::Auto,
        PlaybackSeekableCacheMode::Disabled,
    ] {
        for mode in [PlaybackSeekMode::Precise, PlaybackSeekMode::Fast] {
            let mut backend = open_video(&file, cache);
            for position in [5.99, 6.0] {
                backend.seek_to_with_mode(position, mode).unwrap();
                let frame = wait_for_frame(&mut backend, 5.9);
                assert_target_frame(&frame, 5.9);
                assert_remains_paused(&mut backend, position);
            }
            backend.seek_to(2.0).unwrap();
            assert_target_frame(&wait_for_frame(&mut backend, 2.0), 2.0);
            backend.resume().unwrap();
            assert!(wait_for_frame(&mut backend, 2.1).pts.unwrap().nsecs > 2_000_000_000);
            backend.stop().unwrap();
        }
    }
}

#[test]
fn repeated_paused_seeks_from_eof_keep_decoder_generations_ready() {
    let file = video_fixture();
    let mut backend = open_video(&file, PlaybackSeekableCacheMode::Disabled);
    for _ in 0..12 {
        backend.seek_to(6.0).unwrap();
        assert_target_frame(&wait_for_frame(&mut backend, 5.9), 5.9);
        backend.seek_to(1.2).unwrap();
        assert_target_frame(&wait_for_frame(&mut backend, 1.2), 1.2);
    }
    backend.stop().unwrap();
}
