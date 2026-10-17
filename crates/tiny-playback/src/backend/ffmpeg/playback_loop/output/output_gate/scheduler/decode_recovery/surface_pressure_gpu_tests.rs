use super::*;
use crate::{
    HardwareDecodeBackend, HardwareDecodeMode,
    backend::ffmpeg::{AvFrame, AvPacket, Decoder, FormatContext, VideoFrameConverter},
    libplacebo::LibplaceboToneMapper,
    render_host::{FrameBufferPool, FrameColor, RawVideoFormat},
};

#[test]
#[ignore = "requires VA-API HEVC decode and Vulkan DMA-BUF/modifier interop"]
fn vaapi_hevc_recovery_at_25fps_commits_eight_surfaces_renders_and_continues_decoding() {
    // 24 generated gray 320x180 Main 10 / HDR frames at 25 fps, one IDR and
    // dependent P frames. Use dimensions above NVIDIA's 144x144 minimum.
    // ffmpeg -f lavfi -i color=c=gray:s=320x180:r=25 -frames:v 24
    //   -c:v libx265 -preset ultrafast -crf 30 -pix_fmt yuv420p10le
    //   -x265-params pools=none:frame-threads=1:keyint=120:min-keyint=120:
    //     scenecut=0:bframes=0:aud=1:repeat-headers=1:info=0:
    //     colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc -f hevc
    let data = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/backend/ffmpeg/codec/tests/data/vaapi_hevc10_recovery.hevc"
    ));
    let aud = [0, 0, 0, 1, 0x46, 0x01];
    let mut boundaries = data
        .windows(aud.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == aud).then_some(offset))
        .collect::<Vec<_>>();
    assert_eq!(boundaries.len(), 24);
    boundaries.push(data.len());

    let input = FormatContext::with_streams_for_test(&[(
        ffi::AVMediaType::AVMEDIA_TYPE_VIDEO,
        ffi::AVCodecID::AV_CODEC_ID_HEVC,
    )]);
    let stream = input.streams().unwrap()[0];
    let decoder = Decoder::open_video(stream, HardwareDecodeMode::ForceVaapiVulkan).unwrap();
    assert_eq!(
        decoder.hardware_decode_backend(),
        Some(HardwareDecodeBackend::VaapiVulkan),
    );
    let convert_context = VideoFrameConvertContext::from_decoder(&decoder);
    let counter = decoder.mapped_surface_counter().unwrap();
    let mut converter = VideoFrameConverter::new(FrameBufferPool::default());
    let mut renderer =
        LibplaceboToneMapper::new_for_vulkan_decode(convert_context.vulkan_device().unwrap())
            .unwrap();
    let mut av_frame = AvFrame::new().unwrap();
    let session_id = PlaybackSessionId(1);
    let control = FfmpegControl::new(session_id);
    let target_nsecs = 566_680_000_000;
    let mut output = PlaybackOutputScheduler::new();
    output.begin_decode_recovery(
        3,
        target_nsecs,
        DecodeRecoverySource::CachedSafeIdrRebuild,
        &control,
        session_id,
    );
    output.mark_decode_recovery_replaying(3);
    assert!(output.maybe_enter_decode_recovery_barrier(
        Instant::now(),
        target_nsecs,
        &control,
        session_id,
    ));
    let mut scheduler = PlaybackScheduler::new(target_nsecs);
    let mut current_start_position_nsecs = target_nsecs;
    let mut rendered_frames = 0;
    let mut committed_at_surface_limit = false;
    let mut previous_timeline_nsecs = None;
    for (index, offsets) in boundaries.windows(2).enumerate() {
        if convert_context.hardware_surface_pressure() {
            assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 8);
            if output.decode_recovery_active() {
                assert_eq!(output.recovery_staging_frames(), 8);
                assert_eq!(
                    output.decode_recovery_phase(),
                    Some(DecodeRecoveryPhase::Replaying)
                );
                assert_eq!(
                    output.recovery_staging_range_nsecs(),
                    Some((target_nsecs, target_nsecs + 320_000_000)),
                );
                assert!(output.maybe_buffer_decode_recovery_for_surface_pressure(
                    convert_context.hardware_surface_pressure(),
                    session_id,
                ));
                assert!(output.commit_decode_recovery_if_buffered(
                    &control,
                    &mut scheduler,
                    None,
                    &mut current_start_position_nsecs,
                    true,
                    session_id,
                ));
                assert!(!control.is_output_rebuffer_paused());
                committed_at_surface_limit = true;
            }
            render_next_frame(&mut output, &mut renderer, &mut previous_timeline_nsecs);
            rendered_frames += 1;
            assert!(!convert_context.hardware_surface_pressure());
        }
        let mut props = AvPacket::new().unwrap();
        unsafe {
            let props = &mut *props.as_mut_ptr();
            props.pts = (target_nsecs / 1_000_000) as i64 + index as i64 * 40;
            props.dts = props.pts;
            props.duration = 40;
            props.flags = if index == 0 { ffi::AV_PKT_FLAG_KEY } else { 0 };
        }
        let packet = AvPacket::from_data_and_props(&data[offsets[0]..offsets[1]], &props).unwrap();
        decoder
            .decode_packet(packet.as_ptr(), &mut av_frame, |frame| {
                queue_decoded_frame(
                    &mut output,
                    &mut converter,
                    &convert_context,
                    frame,
                    session_id,
                )
            })
            .unwrap();
    }
    assert!(
        committed_at_surface_limit,
        "25 fps recovery must hit the 8-surface limit"
    );
    decoder
        .flush(&mut av_frame, |frame| {
            queue_decoded_frame(
                &mut output,
                &mut converter,
                &convert_context,
                frame,
                session_id,
            )
        })
        .unwrap();
    while !output.scheduled_video_queue.is_empty() {
        render_next_frame(&mut output, &mut renderer, &mut previous_timeline_nsecs);
        rendered_frames += 1;
    }
    assert_eq!(rendered_frames, 24);
    assert_eq!(
        previous_timeline_nsecs,
        Some(target_nsecs + 23 * 40_000_000)
    );
    assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 0);
    assert_eq!(
        output.decode_recovery_phase(),
        Some(DecodeRecoveryPhase::Committed)
    );
    assert_eq!(current_start_position_nsecs, target_nsecs);
    output
        .check_decode_recovery_deadline(
            Instant::now() + DECODE_RECOVERY_MAX_WALL_TIME,
            &control,
            session_id,
        )
        .unwrap();
}

fn queue_decoded_frame(
    output: &mut PlaybackOutputScheduler,
    converter: &mut VideoFrameConverter,
    convert_context: &VideoFrameConvertContext,
    frame: *mut ffi::AVFrame,
    session_id: PlaybackSessionId,
) -> Result<(), String> {
    assert_eq!(unsafe { (*frame).decode_error_flags }, 0);
    let timeline_nsecs = unsafe { (*frame).pts as u64 } * 1_000_000;
    let mut decoded = converter.convert_with_context(convert_context, frame, None)?;
    let FramePixels::DmaBufVideo(mapped) = &decoded.pixels else {
        panic!("VA-API recovery must preserve DMA-BUF output")
    };
    assert_eq!(mapped.format, RawVideoFormat::P010Le);
    assert_eq!(mapped.color, FrameColor::Hdr10Bt2020);
    decoded.pts = Some(FramePts {
        nsecs: timeline_nsecs,
    });
    let queued = QueuedVideoFrame {
        frame: decoded,
        timeline_nsecs,
        duration_nsecs: 40_000_000,
        source_duration_nsecs: 40_000_000,
    };
    if output.decode_recovery_active() {
        assert!(output.stage_decode_recovery_frame(queued, 3, session_id));
    } else {
        output.scheduled_video_queue.push_queued(queued);
    }
    Ok(())
}

fn render_next_frame(
    output: &mut PlaybackOutputScheduler,
    renderer: &mut LibplaceboToneMapper,
    previous_timeline_nsecs: &mut Option<u64>,
) {
    let frame = output.scheduled_video_queue.pop_front().unwrap();
    if let Some(previous) = *previous_timeline_nsecs {
        assert_eq!(frame.timeline_nsecs, previous + 40_000_000);
    }
    *previous_timeline_nsecs = Some(frame.timeline_nsecs);
    let FramePixels::DmaBufVideo(input) = &frame.frame.pixels else {
        panic!("VA-API recovery must preserve DMA-BUF output")
    };
    let size = RenderSize {
        width: 320,
        height: 180,
    };
    let pixels = renderer
        .tone_map_dmabuf_to_bgra8(input, size, size)
        .unwrap();
    assert_eq!(pixels.len(), 320 * 180 * 4);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255)
    );
}
