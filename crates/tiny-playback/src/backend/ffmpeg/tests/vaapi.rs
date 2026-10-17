use std::io::Write;

use super::*;
use crate::{
    BackendControl, BackendLoadRequest, BgraImage, HardwareDecodeBackend, HardwareDecodeMode,
    PlaybackSeekableCacheMode, VideoPresenter,
};

// Four seconds of generated gray 320x180 video at 10 fps, with a keyframe
// every second and B frames. These contain no audio or private media.
// H.264: libx264 -preset ultrafast -crf 30 -g 10 -bf 2 -pix_fmt yuv420p
// HEVC: libx265 -preset ultrafast -crf 30 -pix_fmt yuv420p10le
//   -x265-params pools=none:frame-threads=1:keyint=10:min-keyint=10:
//                scenecut=0:bframes=2:info=0:colorprim=bt2020:
//                transfer=smpte2084:colormatrix=bt2020nc
//   -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc
const H264: &[u8] = include_bytes!("../codec/tests/data/vaapi_h264.mkv");
const HEVC10: &[u8] = include_bytes!("../codec/tests/data/vaapi_hevc10.mkv");
// Five lossless repetitions of the same generated samples (20 seconds).
// Keep normal pause/seek checks ahead of decoder EOF even while a cold GPU
// initializes its renderer. EOF seek edge cases have separate regression tests.
const H264_LONG: &[u8] = include_bytes!("../codec/tests/data/vaapi_h264_long.mkv");
const HEVC10_LONG: &[u8] = include_bytes!("../codec/tests/data/vaapi_hevc10_long.mkv");
// 20 seconds of generated gray H.264 High 10, unsupported by the RTX 4070 Ti
// hardware decoder. libx264 ultrafast/crf=30/g=10/bf=2/yuv420p10le/profile=high10.
const H264_HIGH10: &[u8] = include_bytes!("../codec/tests/data/auto_h264_high10.mkv");

#[test]
#[ignore = "requires an NVIDIA GPU without H.264 High 10 hardware decoding and Vulkan rendering"]
fn auto_nvidia_unsupported_high10_advances_through_hardware_candidates_to_software() {
    for mode in [
        HardwareDecodeMode::Auto,
        HardwareDecodeMode::AutoWithCopyback,
    ] {
        exercise_playback_backend(
            H264_HIGH10,
            RawVideoFormat::I42010Le,
            FrameColor::Sdr,
            mode,
            None,
        );
    }
}

#[test]
#[ignore = "requires an NVIDIA GPU with Vulkan H.264 decode and Vulkan rendering"]
fn auto_nvidia_h264_prefers_vulkan_and_preserves_playback_controls() {
    for mode in [
        HardwareDecodeMode::Auto,
        HardwareDecodeMode::AutoWithCopyback,
    ] {
        exercise_playback(H264_LONG, RawVideoFormat::Nv12, FrameColor::Sdr, mode);
    }
}

#[test]
#[ignore = "requires an NVIDIA GPU with Vulkan HEVC Main 10 decode and Vulkan rendering"]
fn auto_nvidia_hevc10_prefers_vulkan_and_preserves_hdr_and_playback_controls() {
    exercise_playback(
        HEVC10_LONG,
        RawVideoFormat::P010Le,
        FrameColor::Hdr10Bt2020,
        HardwareDecodeMode::Auto,
    );
}

#[test]
#[ignore = "requires VA-API H.264 decode, a DRM render node and Vulkan rendering"]
fn vaapi_playback_h264_copyback_renders_seeks_resumes_and_reaches_eof() {
    exercise_playback(
        H264_LONG,
        RawVideoFormat::Nv12,
        FrameColor::Sdr,
        HardwareDecodeMode::ForceVaapi,
    );
}

#[test]
#[ignore = "requires VA-API HEVC Main 10 decode, a DRM render node and Vulkan rendering"]
fn vaapi_playback_hevc10_copyback_preserves_p010_hdr_and_playback_controls() {
    exercise_playback(
        HEVC10_LONG,
        RawVideoFormat::P010Le,
        FrameColor::Hdr10Bt2020,
        HardwareDecodeMode::ForceVaapi,
    );
}

#[test]
#[ignore = "requires VA-API H.264 decode and Vulkan DMA-BUF/modifier interop"]
fn vaapi_vulkan_playback_h264_maps_renders_seeks_resumes_and_reaches_eof() {
    exercise_playback(
        H264_LONG,
        RawVideoFormat::Nv12,
        FrameColor::Sdr,
        HardwareDecodeMode::ForceVaapiVulkan,
    );
}

#[test]
#[ignore = "requires VA-API HEVC Main 10 decode and Vulkan DMA-BUF/modifier interop"]
fn vaapi_vulkan_playback_hevc10_preserves_p010_hdr_and_playback_controls() {
    exercise_playback(
        HEVC10_LONG,
        RawVideoFormat::P010Le,
        FrameColor::Hdr10Bt2020,
        HardwareDecodeMode::ForceVaapiVulkan,
    );
}

fn exercise_playback(
    data: &[u8],
    format: RawVideoFormat,
    color: FrameColor,
    mode: HardwareDecodeMode,
) {
    let expected_backend = Some(match mode {
        HardwareDecodeMode::Auto | HardwareDecodeMode::AutoWithCopyback => {
            HardwareDecodeBackend::Vulkan
        }
        HardwareDecodeMode::ForceVaapiVulkan => HardwareDecodeBackend::VaapiVulkan,
        _ => HardwareDecodeBackend::Vaapi,
    });
    exercise_playback_backend(data, format, color, mode, expected_backend);
}

fn exercise_playback_backend(
    data: &[u8],
    format: RawVideoFormat,
    color: FrameColor,
    mode: HardwareDecodeMode,
    expected_backend: Option<HardwareDecodeBackend>,
) {
    let mut file = tempfile::Builder::new().suffix(".mkv").tempfile().unwrap();
    file.write_all(data).unwrap();
    file.flush().unwrap();
    for cache in [
        PlaybackSeekableCacheMode::Auto,
        PlaybackSeekableCacheMode::Disabled,
    ] {
        let mut backend = FfmpegBackend::new().unwrap();
        // Hardware-only tests need native interop diagnostics even without a
        // tracing subscriber. These tests run explicitly with --ignored.
        unsafe {
            ffi::av_log_set_level(ffi::AV_LOG_VERBOSE);
            ffi::av_log_set_callback(Some(ffi::av_log_default_callback));
        }
        backend
            .load(BackendLoadRequest {
                url: file.path().to_str().unwrap().to_string(),
                http_headers: Vec::new(),
                content_length: None,
                start_position_seconds: 0.0,
                selected_tracks: Default::default(),
                cache_config: PlaybackCacheConfig {
                    hardware_decode: mode,
                    seekable_cache: cache,
                    demuxer_cache_wait: false,
                    ..Default::default()
                },
            })
            .unwrap();

        // Keep GPU/shader initialization outside the advancing media clock.
        // Otherwise a cold renderer can let a short fixture reach EOF before
        // the first paused seek, exercising an unrelated EOF/seek race.
        backend.pause().unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if mode.allows_fallback() && expected_backend.is_none() {
                // Info may initially announce an opened hardware candidate;
                // successful first-frame negotiation is what confirms it.
                for event in backend.poll_events() {
                    match event.kind {
                        BackendEventKind::Fatal(error) | BackendEventKind::LoadFailed(error) => {
                            panic!("Auto fallback failed: {error}")
                        }
                        BackendEventKind::PlaybackInfoChanged(Some(info))
                            if !info.hardware_accelerated =>
                        {
                            assert_eq!(info.hardware_decode_backend, None)
                        }
                        _ => {}
                    }
                }
            } else {
                poll_playback_events(&mut backend, expected_backend);
            }
            // Render prewarm failures must be consumed even while waiting for
            // first-frame negotiation to advance to a later candidate.
            while let Some((ticket, device)) = backend.video_output_queue.take_vulkan_prewarm() {
                let result = crate::libplacebo::LibplaceboToneMapper::new_for_vulkan_decode(device)
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                backend
                    .video_output_queue
                    .complete_vulkan_prewarm(ticket, result);
            }
            if let Some(frame) = backend.video_output_queue.take_frame() {
                assert_eq!(
                    frame.size,
                    RenderSize {
                        width: 320,
                        height: 180
                    }
                );
                match frame.pixels {
                    FramePixels::VulkanVideo(native)
                        if mode.allows_fallback() && expected_backend.is_some() =>
                    {
                        assert_eq!(
                            native.source,
                            crate::render_host::VulkanFrameSource::DecoderPool
                        );
                        assert_eq!(native.color, color);
                        assert!(native.source.cache_textures());
                        assert!(!native.planes.is_empty());
                    }
                    FramePixels::DmaBufVideo(mapped)
                        if mode == HardwareDecodeMode::ForceVaapiVulkan =>
                    {
                        assert_eq!(mapped.format, format);
                        assert_eq!(mapped.color, color);
                        let exported = unsafe { &*mapped.frame.as_ptr() };
                        assert_eq!(
                            exported.format,
                            ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                        );
                        assert!(!exported.data[0].is_null());
                    }
                    FramePixels::RawVideo(raw)
                        if mode == HardwareDecodeMode::ForceVaapi || expected_backend.is_none() =>
                    {
                        assert_eq!(raw.format, format);
                        assert_eq!(raw.color, color);
                        let crate::render_host::RawVideoPlanes::Owned(planes) = raw.planes;
                        assert_eq!(planes.len(), format.plane_count());
                    }
                    _ => panic!("VA-API output must match the requested GPU mapping/copyback path"),
                }
                break;
            }
            assert!(Instant::now() < deadline, "VA-API produced no frame");
            thread::sleep(Duration::from_millis(5));
        }

        // A paused initial frame is consumed by the native-frame assertions
        // above; request the same target again for the BGRA presenter check.
        backend.seek_to(0.0).unwrap();
        let mut presenter = VideoPresenter::new(backend.video_output()).unwrap();
        assert_bgra_frame(wait_for_render(
            &mut backend,
            &mut presenter,
            expected_backend,
            Some(0),
        ));
        for target in [2.0, 1.0] {
            backend.seek_to(target).unwrap();
            assert_bgra_frame(wait_for_render(
                &mut backend,
                &mut presenter,
                expected_backend,
                Some(super::super::seconds_to_nsecs(target)),
            ));
            assert!(backend.user_paused);
            assert!(backend.paused);
        }
        backend.set_playback_rate(4.0).unwrap();
        backend.resume().unwrap();
        let mut ended = false;
        let mut resumed_frames = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            ended |= poll_playback_events(&mut backend, expected_backend);
            presenter.prewarm_if_needed();
            if let Some(frame) = presenter
                .render_if_needed(RenderSize {
                    width: 320,
                    height: 180,
                })
                .unwrap()
            {
                assert_bgra_frame(frame);
                resumed_frames += 1;
            }
            if ended && !presenter.snapshot().needs_animation_frame() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "VA-API playback did not drain to EOF"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(resumed_frames > 0);
        drop(presenter);
        backend.stop().unwrap();
        assert!(backend.worker.is_none());
        assert!(!backend.loaded);
    }
}

fn poll_playback_events(
    backend: &mut FfmpegBackend,
    expected_backend: Option<HardwareDecodeBackend>,
) -> bool {
    let mut ended = false;
    for event in backend.poll_events() {
        match event.kind {
            BackendEventKind::LoadFailed(error) | BackendEventKind::Fatal(error) => {
                panic!("VA-API playback failed: {error}")
            }
            BackendEventKind::PlaybackInfoChanged(Some(info)) => {
                assert_eq!(info.hardware_accelerated, expected_backend.is_some());
                assert_eq!(info.hardware_decode_backend, expected_backend);
            }
            BackendEventKind::PlaybackEnded => ended = true,
            _ => {}
        }
    }
    ended
}

fn wait_for_render(
    backend: &mut FfmpegBackend,
    presenter: &mut VideoPresenter,
    expected_backend: Option<HardwareDecodeBackend>,
    seek: Option<u64>,
) -> BgraImage {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        poll_playback_events(backend, expected_backend);
        presenter.prewarm_if_needed();
        if let Some(frame) = presenter
            .render_if_needed(RenderSize {
                width: 320,
                height: 180,
            })
            .unwrap()
        {
            let presented_target = seek.is_none_or(|target_nsecs| {
                backend.loaded
                    && backend.video_output_queue.presentation_identity().0
                        == backend.current_session_id
                    && backend
                        .video_output_queue
                        .snapshot()
                        .last_presentation
                        .is_some_and(|feedback| {
                            feedback.timeline_nsecs >= target_nsecs
                                && feedback.timeline_nsecs < target_nsecs + 100_000_000
                        })
            });
            // seek_to is asynchronous: ignore a ready frame from the previous
            // generation until the actual paused target reaches presentation.
            if presented_target {
                return frame;
            }
        }
        assert!(
            Instant::now() < deadline,
            "VA-API produced no BGRA frame: target={seek:?}, loaded={}, session={:?}, output={:?}, presenter={:?}",
            backend.loaded,
            backend.current_session_id,
            backend.video_output_queue.snapshot(),
            presenter.snapshot()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_bgra_frame(frame: BgraImage) {
    assert_eq!(
        frame.size(),
        RenderSize {
            width: 320,
            height: 180
        }
    );
    assert_eq!(frame.bytes().len(), 320 * 180 * 4);
    assert!(frame.bytes().chunks_exact(4).all(|pixel| pixel[3] == 255));
    assert!(frame.bytes().chunks_exact(4).any(|pixel| pixel[0] != 0));
}

#[test]
fn generated_vaapi_media_decodes_seeks_and_drains_in_software_without_a_gpu() {
    for (data, format, color) in [
        (H264, RawVideoFormat::I420, FrameColor::Sdr),
        (HEVC10, RawVideoFormat::I42010Le, FrameColor::Hdr10Bt2020),
    ] {
        let mut file = tempfile::Builder::new().suffix(".mkv").tempfile().unwrap();
        file.write_all(data).unwrap();
        file.flush().unwrap();
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
                    ..Default::default()
                },
            })
            .unwrap();
        let next_frame = |backend: &mut FfmpegBackend| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                poll_playback_events(backend, None);
                if let Some(frame) = backend.video_output_queue.take_frame() {
                    let FramePixels::RawVideo(raw) = &frame.pixels else {
                        panic!("expected decoded YUV")
                    };
                    assert_eq!(raw.format, format);
                    assert_eq!(raw.color, color);
                    return frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "encoded fixture produced no frame"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        next_frame(&mut backend);
        backend.pause().unwrap();
        for target in [2.0, 1.0] {
            backend.seek_to(target).unwrap();
            let frame = next_frame(&mut backend);
            assert_eq!(
                frame.pts.unwrap().nsecs,
                super::super::seconds_to_nsecs(target)
            );
            assert!(backend.user_paused);
        }
        backend.resume().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut frames = 0;
        loop {
            let ended = poll_playback_events(&mut backend, None);
            if backend.video_output_queue.take_frame().is_some() {
                frames += 1;
            }
            if ended {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "encoded fixture did not drain to EOF"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(frames > 0);
        backend.stop().unwrap();
        assert!(backend.worker.is_none());
    }
}

#[test]
fn forced_vaapi_playback_rejects_unsupported_codec_instead_of_using_software() {
    for mode in [
        HardwareDecodeMode::ForceVaapi,
        HardwareDecodeMode::ForceVaapiVulkan,
    ] {
        assert_unsupported_codec_is_rejected(mode);
    }
}

fn assert_unsupported_codec_is_rejected(mode: HardwareDecodeMode) {
    let mut file = tempfile::Builder::new().suffix(".y4m").tempfile().unwrap();
    file.write_all(b"YUV4MPEG2 W16 H16 F10:1 Ip A1:1 C420jpeg\nFRAME\n")
        .unwrap();
    file.write_all(&[128; 16 * 16 * 3 / 2]).unwrap();
    file.flush().unwrap();
    let mut backend = FfmpegBackend::new().unwrap();
    backend
        .load(BackendLoadRequest {
            url: file.path().to_str().unwrap().to_string(),
            http_headers: Vec::new(),
            content_length: None,
            start_position_seconds: 0.0,
            selected_tracks: Default::default(),
            cache_config: PlaybackCacheConfig {
                hardware_decode: mode,
                ..Default::default()
            },
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let error = 'wait: loop {
        for event in backend.poll_events() {
            match event.kind {
                BackendEventKind::LoadFailed(error) | BackendEventKind::Fatal(error) => {
                    break 'wait error;
                }
                BackendEventKind::PlaybackInfoChanged(Some(_)) => {
                    panic!("forced VA-API must not start a software-only codec")
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "unsupported VA-API input did not report failure"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert!(error.contains("VA-API"), "{error}");
    assert!(backend.video_output_queue.take_frame().is_none());
    backend.stop().unwrap();
}
