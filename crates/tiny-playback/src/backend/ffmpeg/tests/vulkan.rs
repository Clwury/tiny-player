use super::super::{
    VideoHwDecodeContext,
    video::{raw_video_frame_from_av_frame, vulkan_video_frame_from_av_frame_with_device},
};
use super::*;
use crate::{ffmpeg_vulkan as vk, libplacebo::LibplaceboToneMapper, render_host::FrameBufferPool};

#[test]
fn vulkan_wrapping_uses_allocated_extent_and_visible_crop_when_enabled() {
    if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
        return;
    }

    // Allocate/upload images without invoking the video decoder. This also
    // works on drivers whose Vulkan Video decode path is broken.
    let codec = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264) };
    let hardware = VideoHwDecodeContext::try_create(codec).unwrap();
    let device = hardware.device();
    let mut renderer = LibplaceboToneMapper::new_for_vulkan_decode(Arc::clone(&device)).unwrap();
    for format in [RawVideoFormat::Nv12, RawVideoFormat::P010Le] {
        for width in [64, 96] {
            check_vulkan_crop(
                &mut renderer,
                &device,
                format,
                RenderSize { width, height: 64 },
            );
        }
    }
}

fn check_vulkan_crop(
    renderer: &mut LibplaceboToneMapper,
    device: &Arc<VulkanDecodeDevice>,
    format: RawVideoFormat,
    allocated_size: RenderSize,
) {
    let sw_format = match format {
        RawVideoFormat::Nv12 => ffi::AVPixelFormat::AV_PIX_FMT_NV12,
        RawVideoFormat::P010Le => ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
        _ => unreachable!(),
    };
    let mut frames_ptr = unsafe { ffi::av_hwframe_ctx_alloc(device.device_ref()) };
    assert!(!frames_ptr.is_null());
    let frames_ref = FfmpegAvBufferRef::new_ref(frames_ptr).unwrap();
    unsafe { ffi::av_buffer_unref(&mut frames_ptr) };
    let frames = unsafe { &mut *((*frames_ref.as_ptr()).data as *mut ffi::AVHWFramesContext) };
    frames.format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
    frames.sw_format = sw_format;
    frames.width = allocated_size.width as i32;
    frames.height = allocated_size.height as i32;
    let vk_frames = unsafe { &mut *(frames.hwctx as *mut vk::AVVulkanFramesContext) };
    vk_frames.nb_layers = 1;
    // Linear, separate images keep this regression independent of driver
    // support for uploading to optimal-tiled/multiplane video images.
    vk_frames.tiling = vk::VkImageTiling_VK_IMAGE_TILING_LINEAR;
    vk_frames.flags = vk::AVVkFrameFlags_AV_VK_FRAME_FLAG_NONE
        | vk::AVVkFrameFlags_AV_VK_FRAME_FLAG_DISABLE_MULTIPLANE;
    assert!(unsafe { ffi::av_hwframe_ctx_init(frames_ref.as_ptr()) } >= 0);

    let mut hardware_frame = AvFrame::new().unwrap();
    let hw_ptr = hardware_frame.as_mut_ptr();
    assert!(unsafe { ffi::av_hwframe_get_buffer(frames_ref.as_ptr(), hw_ptr, 0) } >= 0);
    let mut software_frame = AvFrame::new().unwrap();
    let sw_ptr = software_frame.as_mut_ptr();
    unsafe {
        (*sw_ptr).format = sw_format as c_int;
        (*sw_ptr).width = allocated_size.width as i32;
        (*sw_ptr).height = allocated_size.height as i32;
        (*sw_ptr).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*sw_ptr).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*sw_ptr).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*sw_ptr).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        (*sw_ptr).chroma_location = ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
    }
    assert!(unsafe { ffi::av_frame_get_buffer(sw_ptr, 32) } >= 0);
    for plane in 0..format.plane_count() {
        let layout = format.plane_layout(allocated_size, plane).unwrap();
        for row in 0..layout.height as usize {
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    (*sw_ptr).data[plane].add(row * (*sw_ptr).linesize[plane] as usize),
                    layout.row_len,
                )
            };
            for (column, sample) in bytes
                .chunks_exact_mut(format.component_size() as usize / 8)
                .enumerate()
            {
                // Distinct padding and a spatial pattern expose incorrect
                // normalized sampling even when the output size is unchanged.
                let code = if plane > 0 {
                    128
                } else if row >= 48 || column >= 60 {
                    235
                } else {
                    32 + ((row * 3 + column * 2) % 160) as u8
                };
                match format {
                    RawVideoFormat::Nv12 => sample[0] = code,
                    RawVideoFormat::P010Le => {
                        sample.copy_from_slice(&(u16::from(code) << 8).to_le_bytes())
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
    assert!(unsafe { ffi::av_hwframe_transfer_data(hw_ptr, sw_ptr, 0) } >= 0);
    assert!(unsafe { ffi::av_frame_copy_props(hw_ptr, sw_ptr) } >= 0);
    let pool = FrameBufferPool::default();

    // Reuse the same VkImage with different logical crops, then switch frame
    // pools above. Neither display dimensions nor cached textures may be stale.
    for size in [
        allocated_size,
        RenderSize {
            width: 60,
            height: 48,
        },
        RenderSize {
            width: 32,
            height: 32,
        },
    ] {
        unsafe {
            (*hw_ptr).width = size.width as i32;
            (*hw_ptr).height = size.height as i32;
            (*sw_ptr).width = size.width as i32;
            (*sw_ptr).height = size.height as i32;
        }
        let vulkan = vulkan_video_frame_from_av_frame_with_device(
            hw_ptr,
            Some(Arc::clone(device)),
            format,
            None,
        )
        .unwrap();
        assert_eq!(vulkan.allocated_size, allocated_size);
        assert_eq!(vulkan.planes.len(), 2);
        let raw = raw_video_frame_from_av_frame(sw_ptr, size, None, &pool)
            .unwrap()
            .unwrap();
        let expected = renderer.tone_map_to_bgra8(&raw, size, size).unwrap();
        let actual = renderer
            .tone_map_vulkan_to_bgra8(&vulkan, size, size)
            .unwrap();
        assert_eq!(actual.len(), expected.len());
        let max_error = actual
            .iter()
            .zip(&expected)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(
            max_error <= 2,
            "{format:?}, allocated={allocated_size:?}, visible={size:?}: max pixel error {max_error}"
        );
        assert!(
            renderer
                .tone_map_vulkan_to_bgra8(
                    &vulkan,
                    RenderSize {
                        width: allocated_size.width + 1,
                        height: size.height
                    },
                    size,
                )
                .is_err()
        );
    }
}
