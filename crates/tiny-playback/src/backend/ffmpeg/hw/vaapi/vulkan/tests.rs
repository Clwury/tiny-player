use std::mem;

use super::*;
use crate::backend::ffmpeg::video::vulkan_video_frame_from_av_frame_with_device;
use crate::render_host::FfmpegFrameRef;
use crate::render_host::{
    FrameColor, RawVideoFormat, RenderSize, VulkanDecodeQueue, VulkanDecodeQueues,
    VulkanFrameSource,
};

fn buffer(size: usize) -> FfmpegAvBufferRef {
    let mut raw = unsafe { ffi::av_buffer_allocz(size) };
    assert!(!raw.is_null());
    let owned = FfmpegAvBufferRef::new_ref(raw).unwrap();
    unsafe { ffi::av_buffer_unref(&mut raw) };
    owned
}

fn source_frame(format: ffi::AVPixelFormat) -> AvFrame {
    let mut frame = AvFrame::new().unwrap();
    let pool = buffer(mem::size_of::<ffi::AVHWFramesContext>());
    unsafe {
        let pool_context = &mut *((*pool.as_ptr()).data as *mut ffi::AVHWFramesContext);
        pool_context.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        pool_context.sw_format = format;
        pool_context.width = 64;
        pool_context.height = 64;
        let frame = &mut *frame.as_mut_ptr();
        frame.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        frame.width = 60;
        frame.height = 48;
        frame.pts = 1_234;
        frame.best_effort_timestamp = 1_234;
        frame.duration = 40;
        frame.flags = ffi::AV_FRAME_FLAG_KEY;
        frame.color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        frame.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
        frame.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        frame.colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
        frame.chroma_location = ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
        frame.hw_frames_ctx = ffi::av_buffer_ref(pool.as_ptr());
        frame.buf[0] = ffi::av_buffer_alloc(1);
        assert!(!frame.buf[0].is_null());
        let side = ffi::av_frame_new_side_data(
            frame,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_SEI_UNREGISTERED,
            3,
        );
        assert!(!side.is_null());
        ptr::copy_nonoverlapping(b"hdr".as_ptr(), (*side).data, 3);
    }
    frame
}

#[test]
fn direct_mapping_preserves_hdr_timing_and_retains_surface_until_queued_refs_drop() {
    for format in [
        ffi::AVPixelFormat::AV_PIX_FMT_NV12,
        ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
    ] {
        let mut source = source_frame(format);
        let target = buffer(mem::size_of::<ffi::AVHWFramesContext>());
        let surface = FfmpegAvBufferRef::new_ref(unsafe { (*source.as_mut_ptr()).buf[0] }).unwrap();
        let mut map_calls = 0;
        let mut mapped = map_frame_with(
            source.as_mut_ptr(),
            target.as_ptr(),
            |dst, src, flags| unsafe {
                assert_eq!(
                    flags,
                    ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32
                );
                map_calls += 1;
                if map_calls == 1 {
                    assert_eq!((*src).format, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32);
                    assert_eq!(
                        (*dst).format,
                        ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                    );
                } else {
                    assert_eq!(
                        (*src).format,
                        ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
                    );
                    assert_eq!((*dst).format, ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32);
                    assert_eq!((*(*dst).hw_frames_ctx).data, (*target.as_ptr()).data);
                }
                // The native mapping retains the source surface in its map
                // descriptor. Supply the same reference semantics without a GPU.
                (*dst).buf[0] = ffi::av_buffer_ref((*src).buf[0]);
                (*dst).data[0] = (*(*dst).buf[0]).data;
                (*dst).width = (*src).width;
                (*dst).height = (*src).height;
                0
            },
        )
        .unwrap();
        assert_eq!(map_calls, 2);
        unsafe {
            let output = &*mapped.as_mut_ptr();
            assert_eq!(output.pts, 1_234);
            assert_eq!(output.best_effort_timestamp, 1_234);
            assert_eq!(output.duration, 40);
            assert_eq!((output.width, output.height), (60, 48));
            assert_eq!(
                output.color_trc,
                ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
            );
            assert_eq!(
                output.color_primaries,
                ffi::AVColorPrimaries::AVCOL_PRI_BT2020
            );
            assert_eq!(
                output.chroma_location,
                ffi::AVChromaLocation::AVCHROMA_LOC_LEFT
            );
            assert_ne!(output.flags & ffi::AV_FRAME_FLAG_KEY, 0);
            let side = ffi::av_frame_get_side_data(
                output,
                ffi::AVFrameSideDataType::AV_FRAME_DATA_SEI_UNREGISTERED,
            );
            assert!(!side.is_null());
            assert_eq!(
                std::slice::from_raw_parts((*side).data, (*side).size),
                b"hdr"
            );
        }
        let queued = FfmpegFrameRef::new_ref(mapped.as_mut_ptr()).unwrap();
        drop(mapped);
        drop(source);
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(surface.as_ptr()) }, 2);
        drop(queued);
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(surface.as_ptr()) }, 1);
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(target.as_ptr()) }, 1);
    }
}

#[test]
fn mapping_failure_releases_target_refs_and_never_returns_a_cpu_frame() {
    let mut source = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_NV12);
    let target = buffer(mem::size_of::<ffi::AVHWFramesContext>());
    for code in [ffi::AVERROR(ffi::ENOSYS), ffi::AVERROR(ffi::ENOMEM)] {
        for failing_format in [
            ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME,
            ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
        ] {
            let error =
                match map_frame_with(source.as_mut_ptr(), target.as_ptr(), |dst, src, _| unsafe {
                    if (*dst).format == failing_format as i32 {
                        return code;
                    }
                    (*dst).buf[0] = ffi::av_buffer_ref((*src).buf[0]);
                    (*dst).data[0] = (*(*dst).buf[0]).data;
                    (*dst).width = (*src).width;
                    (*dst).height = (*src).height;
                    0
                }) {
                    Ok(_) => panic!("failed GPU mapping must not return a downloaded frame"),
                    Err(error) => error,
                };
            assert!(error.contains(&format!("code={code}")), "{error}");
            assert!(error.contains("不回退"), "{error}");
            assert_eq!(unsafe { ffi::av_buffer_get_ref_count(target.as_ptr()) }, 1);
            assert_eq!(
                unsafe { ffi::av_buffer_get_ref_count((*source.as_mut_ptr()).buf[0]) },
                1
            );
        }
    }
    let mut unsupported = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_YUV444P);
    assert!(
        map_frame_with(unsupported.as_mut_ptr(), target.as_ptr(), |_, _, _| panic!(
            "unsupported layout must be rejected before mapping"
        ))
        .is_err()
    );
}

#[test]
fn surface_budget_counts_mappings_until_the_last_cross_thread_frame_reference_drops() {
    let counter = Arc::new(AtomicUsize::new(0));
    let mut frame = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_NV12);
    track_surface(frame.as_mut_ptr(), &counter).unwrap();
    assert_eq!(counter.load(Ordering::Acquire), 1);
    let queued = FfmpegFrameRef::new_ref(frame.as_mut_ptr()).unwrap();
    let render = queued.clone();
    drop(frame);
    drop(queued);
    assert_eq!(counter.load(Ordering::Acquire), 1);
    std::thread::spawn(move || drop(render)).join().unwrap();
    assert_eq!(counter.load(Ordering::Acquire), 0);
}

#[test]
fn mapping_context_tracks_pool_identity_and_survives_failed_reconfiguration() {
    let source = buffer(mem::size_of::<ffi::AVHWFramesContext>());
    let same_source = source.clone();
    let replacement = buffer(mem::size_of::<ffi::AVHWFramesContext>());
    let mut cached = None;
    let first = mapping_frames_with(&mut cached, source.as_ptr(), || Ok(buffer(1))).unwrap();
    let reused = mapping_frames_with(&mut cached, same_source.as_ptr(), || {
        panic!("same pool must reuse its derived context")
    })
    .unwrap();
    assert_eq!(unsafe { (*first.as_ptr()).data }, unsafe {
        (*reused.as_ptr()).data
    });
    assert!(
        mapping_frames_with(&mut cached, replacement.as_ptr(), || Err(
            "unsupported replacement pool".into()
        ))
        .is_err()
    );
    let retained = mapping_frames_with(&mut cached, source.as_ptr(), || {
        panic!("failed replacement must preserve the old mapping")
    })
    .unwrap();
    assert_eq!(unsafe { (*first.as_ptr()).data }, unsafe {
        (*retained.as_ptr()).data
    });
    let new = mapping_frames_with(&mut cached, replacement.as_ptr(), || Ok(buffer(1))).unwrap();
    assert_ne!(unsafe { (*first.as_ptr()).data }, unsafe {
        (*new.as_ptr()).data
    });
    drop(cached);
    assert_eq!(
        unsafe { ffi::av_buffer_get_ref_count(replacement.as_ptr()) },
        1
    );
}

#[test]
fn mapped_images_use_import_extent_and_read_usage_instead_of_decode_pool_defaults() {
    for (format, raw_format, plane_formats) in [
        (
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            RawVideoFormat::Nv12,
            [9, 16],
        ),
        (
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
            RawVideoFormat::P010Le,
            [70, 77],
        ),
    ] {
        let mut frame = source_frame(format);
        let vk_context = buffer(mem::size_of::<crate::ffmpeg_vulkan::AVVulkanFramesContext>());
        let vk_frame = buffer(mem::size_of::<crate::ffmpeg_vulkan::AVVkFrame>());
        unsafe {
            let frame = &mut *frame.as_mut_ptr();
            frame.format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32;
            frame.buf[1] = ffi::av_buffer_ref(vk_frame.as_ptr());
            frame.data[0] = (*vk_frame.as_ptr()).data;
            let frames = &mut *((*frame.hw_frames_ctx).data as *mut ffi::AVHWFramesContext);
            frames.format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
            frames.hwctx = (*vk_context.as_ptr()).data.cast();
            let context = &mut *((*vk_context.as_ptr()).data
                as *mut crate::ffmpeg_vulkan::AVVulkanFramesContext);
            context.usage = 0xff;
            context.format[0] = plane_formats[0];
            context.format[1] = plane_formats[1];
            let images = &mut *((*vk_frame.as_ptr()).data as *mut crate::ffmpeg_vulkan::AVVkFrame);
            images.img[0] = 1 as _;
            images.img[1] = 2 as _;
        }
        let device = Arc::new(VulkanDecodeDevice::new(
            buffer(1),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            VulkanDecodeQueues {
                graphics: VulkanDecodeQueue { index: 0, count: 1 },
                compute: None,
                transfer: None,
            },
        ));
        let mapped = vulkan_video_frame_from_av_frame_with_device(
            frame.as_mut_ptr(),
            Some(device.clone()),
            raw_format,
            VulkanFrameSource::VaapiMapping,
            None,
        )
        .unwrap();
        assert_eq!(
            mapped.allocated_size,
            RenderSize {
                width: 60,
                height: 48
            }
        );
        assert_eq!(mapped.usage, 0x5);
        assert_eq!(mapped.color, FrameColor::Hdr10Bt2020);
        assert_eq!(mapped.planes.len(), 2);
        assert!(!mapped.source.cache_textures());
        assert!(!mapped.source.allows_download_fallback());
        let native = vulkan_video_frame_from_av_frame_with_device(
            frame.as_mut_ptr(),
            Some(device),
            raw_format,
            VulkanFrameSource::DecoderPool,
            None,
        )
        .unwrap();
        assert_eq!(
            native.allocated_size,
            RenderSize {
                width: 64,
                height: 64
            }
        );
        assert_eq!(native.usage, 0xff);
        assert!(native.source.cache_textures());
        assert!(native.source.allows_download_fallback());
    }
}
