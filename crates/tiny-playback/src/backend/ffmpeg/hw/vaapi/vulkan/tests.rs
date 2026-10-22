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
        let surface = FfmpegAvBufferRef::new_ref(unsafe { (*source.as_mut_ptr()).buf[0] }).unwrap();
        let mut map_calls = 0;
        let mut mapped = map_frame_with(source.as_mut_ptr(), |dst, src, flags| unsafe {
            assert_eq!(
                flags,
                ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32
            );
            map_calls += 1;
            assert_eq!((*src).format, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32);
            assert_eq!(
                (*dst).format,
                ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
            );
            assert_eq!((*(*dst).hw_frames_ctx).data, (*(*src).hw_frames_ctx).data);
            // The native mapping retains the source surface in its map
            // descriptor. Supply the same reference semantics without a GPU.
            (*dst).buf[0] = ffi::av_buffer_ref((*src).buf[0]);
            (*dst).data[0] = (*(*dst).buf[0]).data;
            (*dst).width = (*src).width;
            (*dst).height = (*src).height;
            0
        })
        .unwrap();
        assert_eq!(map_calls, 1);
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
    }
}

#[test]
fn export_failure_releases_pool_refs_and_never_returns_a_cpu_frame() {
    let mut source = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_NV12);
    let pool = FfmpegAvBufferRef::new_ref(unsafe { (*source.as_mut_ptr()).hw_frames_ctx }).unwrap();
    for code in [ffi::AVERROR(ffi::ENOSYS), ffi::AVERROR(ffi::ENOMEM)] {
        let error = map_frame_with(source.as_mut_ptr(), |_, _, _| code)
            .err()
            .unwrap();
        assert!(error.contains(&format!("code={code}")), "{error}");
        assert!(error.contains("不回退"), "{error}");
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(pool.as_ptr()) }, 2);
    }
    let mut unsupported = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_YUV444P);
    assert!(
        map_frame_with(unsupported.as_mut_ptr(), |_, _, _| panic!(
            "unsupported layout must be rejected before export"
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
fn replacing_source_pools_releases_every_export_after_the_last_queued_ref() {
    for _ in 0..128 {
        let mut source = source_frame(ffi::AVPixelFormat::AV_PIX_FMT_P010LE);
        let pool =
            FfmpegAvBufferRef::new_ref(unsafe { (*source.as_mut_ptr()).hw_frames_ctx }).unwrap();
        let mut exported = map_frame_with(source.as_mut_ptr(), |dst, src, _| unsafe {
            (*dst).buf[0] = ffi::av_buffer_ref((*src).buf[0]);
            (*dst).data[0] = (*(*dst).buf[0]).data;
            (*dst).width = (*src).width;
            (*dst).height = (*src).height;
            0
        })
        .unwrap();
        let queued = FfmpegFrameRef::new_ref(exported.as_mut_ptr()).unwrap();
        drop(exported);
        drop(source);
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(pool.as_ptr()) }, 2);
        std::thread::spawn(move || drop(queued)).join().unwrap();
        assert_eq!(unsafe { ffi::av_buffer_get_ref_count(pool.as_ptr()) }, 1);
    }
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

#[test]
#[ignore = "requires VA-API NV12/P010 decode surfaces, Vulkan DMA-BUF import and /proc/self/fd"]
fn dmabuf_pool_reconfiguration_releases_gpu_resources_and_file_descriptors() {
    use crate::{libplacebo::LibplaceboToneMapper, render_host::DmaBufVideoFrame};

    fn fds(include_driver_cache: bool) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter(|entry| {
                include_driver_cache
                    || std::fs::read_link(entry.path())
                        .is_ok_and(|target| !target.to_string_lossy().contains("dmabuf"))
            })
            .count()
    }

    let codec = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_HEVC) };
    let mut after_first_shutdown = None;
    for restart in 0..3 {
        let vaapi = super::super::create_device(codec).unwrap();
        let mapper = VaapiVulkanMapper::new(&vaapi).unwrap();
        let mut renderer = LibplaceboToneMapper::new_for_vulkan_decode(mapper.device()).unwrap();
        let mut baseline = None;
        for pool in 0..128 {
            let (format, raw_format) = if pool % 2 == 0 {
                (ffi::AVPixelFormat::AV_PIX_FMT_NV12, RawVideoFormat::Nv12)
            } else {
                (
                    ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
                    RawVideoFormat::P010Le,
                )
            };
            let mut raw = unsafe { ffi::av_hwframe_ctx_alloc(vaapi.as_ptr()) };
            assert!(!raw.is_null());
            unsafe {
                let context = &mut *((*raw).data as *mut ffi::AVHWFramesContext);
                context.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
                context.sw_format = format;
                context.width = 64;
                context.height = 64;
                context.initial_pool_size = 2;
                assert_eq!(ffi::av_hwframe_ctx_init(raw), 0);
            }
            let context = FfmpegAvBufferRef::new_ref(raw).unwrap();
            unsafe { ffi::av_buffer_unref(&mut raw) };
            let mut source = AvFrame::new().unwrap();
            assert_eq!(
                unsafe { ffi::av_hwframe_get_buffer(context.as_ptr(), source.as_mut_ptr(), 0) },
                0
            );
            // Visible crop differs from pool extent, as in real decoder output.
            let size = RenderSize {
                width: 60,
                height: 48,
            };
            unsafe {
                (*source.as_mut_ptr()).width = size.width as i32;
                (*source.as_mut_ptr()).height = size.height as i32;
            }
            for attempt in 0..4 {
                let mut exported = mapper.map(source.as_mut_ptr()).unwrap();
                let input = DmaBufVideoFrame {
                    frame: FfmpegFrameRef::new_ref(exported.as_mut_ptr()).unwrap(),
                    device: mapper.device(),
                    format: raw_format,
                    color: FrameColor::Hdr10Bt2020,
                    range: crate::render_host::RawVideoRange::Limited,
                    chroma_site: crate::render_host::RawVideoChromaSite::Left,
                    metadata: None,
                };
                drop(exported);
                let descriptor = unsafe {
                    &*((*input.frame.as_ptr()).data[0]
                        as *const crate::ffmpeg_vulkan::AVDRMFrameDescriptor)
                };
                let exported_fds: Vec<_> = descriptor.objects[..descriptor.nb_objects as usize]
                    .iter()
                    .map(|object| object.fd)
                    .collect();
                if attempt == 3 {
                    // Fail after importing plane 0, exercising partial cleanup.
                    let descriptor = unsafe {
                        &mut *((*input.frame.as_ptr()).data[0]
                            as *mut crate::ffmpeg_vulkan::AVDRMFrameDescriptor)
                    };
                    descriptor.layers[1].format = 0;
                    assert!(
                        renderer
                            .tone_map_dmabuf_to_bgra8(&input, size, size)
                            .is_err()
                    );
                } else {
                    let pixels = renderer
                        .tone_map_dmabuf_to_bgra8(&input, size, size)
                        .unwrap();
                    assert_eq!(pixels.len(), 60 * 48 * 4);
                }
                drop(input);
                for fd in exported_fds {
                    assert!(
                        !std::path::Path::new(&format!("/proc/self/fd/{fd}")).exists(),
                        "exported DMA-BUF fd {fd} survived its last frame reference"
                    );
                }
                assert_eq!(mapper.queued_surfaces.load(Ordering::Acquire), 0);
            }
            drop(source);
            assert_eq!(
                unsafe { ffi::av_buffer_get_ref_count(context.as_ptr()) },
                1,
                "export retained source pool {pool}"
            );
            drop(context);
            // NVIDIA retains a bounded detached-surface DMA-BUF cache until
            // VA device shutdown. Check exported FDs above, and Vulkan resources
            // here; include the driver cache again after full shutdown below.
            let current = fds(false);
            if let Some(baseline) = baseline {
                assert!(
                    current <= baseline + 8,
                    "FD leak at restart {restart}, pool {pool}: {baseline} -> {current}"
                );
            } else {
                baseline = Some(current);
            }
            if pool % 32 == 0 {
                eprintln!("VA-API DMA-BUF restart={restart} pool={pool} fds={current}");
            }
        }
        drop(renderer);
        drop(mapper);
        drop(vaapi);
        let current = fds(true);
        if let Some(baseline) = after_first_shutdown {
            assert!(
                current <= baseline + 4,
                "FD leak across playback restarts: {baseline} -> {current}"
            );
        } else {
            after_first_shutdown = Some(current);
        }
        eprintln!("VA-API DMA-BUF restart={restart} shutdown_fds={current}");
    }
}
