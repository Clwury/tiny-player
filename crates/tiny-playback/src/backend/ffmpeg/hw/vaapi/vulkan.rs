use std::{
    cell::RefCell,
    ffi::{CStr, c_char},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use ffmpeg_sys_next as ffi;

use super::super::{ffmpeg_error, vulkan_decode_device_from_ref};
use crate::backend::ffmpeg::AvFrame;
use crate::render_host::{FfmpegAvBufferRef, VulkanDecodeDevice};

const MAP_FLAGS: i32 = ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32;
const REQUIRED_EXTENSIONS: &[&str] = &[
    "VK_KHR_external_memory_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_EXT_image_drm_format_modifier",
    "VK_KHR_external_semaphore_fd",
];

// Accessed only by the owning decoder thread. Keeping both references avoids
// reusing a derived context after FFmpeg replaces a pool during reconfiguration.
pub(in crate::backend::ffmpeg::hw) struct VaapiVulkanMapper {
    device: Arc<VulkanDecodeDevice>,
    frames: RefCell<Option<MappedFrames>>,
    queued_surfaces: Arc<AtomicUsize>,
}

struct MappedFrames {
    source: FfmpegAvBufferRef,
    target: FfmpegAvBufferRef,
}

impl VaapiVulkanMapper {
    pub(in crate::backend::ffmpeg::hw) fn new(
        vaapi_device: &FfmpegAvBufferRef,
    ) -> Result<Self, String> {
        let mut device_ref = ptr::null_mut();
        let mut options = ptr::null_mut();
        // VA exports separate DRM layers. Their R8/RG8 or R16/RG16 images must
        // match the plane formats advertised by AVVulkanFramesContext.
        let option_result = unsafe {
            ffi::av_dict_set(
                &mut options,
                c"disable_multiplane".as_ptr(),
                c"1".as_ptr(),
                0,
            )
        };
        let result = if option_result < 0 {
            option_result
        } else {
            unsafe {
                ffi::av_hwdevice_ctx_create_derived_opts(
                    &mut device_ref,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                    vaapi_device.as_ptr(),
                    options,
                    0,
                )
            }
        };
        unsafe { ffi::av_dict_free(&mut options) };
        if result < 0 {
            unsafe { ffi::av_buffer_unref(&mut device_ref) };
            return Err(format!(
                "FFmpeg 从 VA-API 派生 Vulkan 设备失败：code={result}, error={}",
                ffmpeg_error(result)
            ));
        }
        let device = vulkan_decode_device_from_ref(device_ref);
        unsafe { ffi::av_buffer_unref(&mut device_ref) };
        let device = device?;
        if device.extensions == 0 || device.num_extensions <= 0 {
            return Err("VA-API/Vulkan 映射设备缺少 Vulkan 扩展列表".to_string());
        }
        let extensions = unsafe {
            std::slice::from_raw_parts(
                device.extensions as *const *const c_char,
                device.num_extensions.max(0) as usize,
            )
        };
        for required in REQUIRED_EXTENSIONS {
            if !extensions.iter().any(|extension| {
                !extension.is_null()
                    && unsafe { CStr::from_ptr(*extension) }.to_bytes() == required.as_bytes()
            }) {
                return Err(format!(
                    "VA-API/Vulkan GPU 映射缺少设备扩展 {required}；可选择 VA-API 回传内存模式"
                ));
            }
        }
        tracing::info!(
            device = device.key(),
            "created VA-API Vulkan DMA-BUF mapping device"
        );
        Ok(Self {
            device: Arc::new(device),
            frames: RefCell::new(None),
            queued_surfaces: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(in crate::backend::ffmpeg::hw) fn device(&self) -> Arc<VulkanDecodeDevice> {
        Arc::clone(&self.device)
    }

    pub(in crate::backend::ffmpeg::hw) fn surface_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.queued_surfaces)
    }

    pub(in crate::backend::ffmpeg::hw) fn map(
        &self,
        frame: *const ffi::AVFrame,
    ) -> Result<AvFrame, String> {
        let source = source_frames(frame)?;
        let target = mapping_frames_with(&mut self.frames.borrow_mut(), source, || {
            let mut target = ptr::null_mut();
            let result = unsafe {
                ffi::av_hwframe_ctx_create_derived(
                    &mut target,
                    ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
                    self.device.device_ref(),
                    source,
                    MAP_FLAGS,
                )
            };
            if result < 0 {
                unsafe { ffi::av_buffer_unref(&mut target) };
                return Err(format!(
                    "FFmpeg 创建 VA-API/Vulkan 帧映射上下文失败：code={result}, error={}",
                    ffmpeg_error(result)
                ));
            }
            let target_ref = FfmpegAvBufferRef::new_ref(target).map_err(|error| error.to_string());
            unsafe { ffi::av_buffer_unref(&mut target) };
            tracing::debug!("initialized direct VA-API to Vulkan frame mapping");
            target_ref
        })?;
        let mut mapped = map_frame_with(frame, target.as_ptr(), |dst, src, flags| unsafe {
            // Use two explicit hardware mappings: VAAPI's generic map_from
            // tries to map unknown formats to memory and can return EINVAL
            // before Vulkan's map_to ever gets a chance. DRM PRIME bypasses
            // that dispatch, synchronizes decode on export and imports the
            // DMA-BUF/modifier with external queue ownership on the second map.
            ffi::av_hwframe_map(dst, src, flags)
        })?;
        track_surface(mapped.as_mut_ptr(), &self.queued_surfaces)?;
        Ok(mapped)
    }
}

fn mapping_frames_with(
    cached: &mut Option<MappedFrames>,
    source: *mut ffi::AVBufferRef,
    create: impl FnOnce() -> Result<FfmpegAvBufferRef, String>,
) -> Result<FfmpegAvBufferRef, String> {
    if cached
        .as_ref()
        .is_none_or(|cached| unsafe { (*cached.source.as_ptr()).data != (*source).data })
    {
        let target = create()?;
        *cached = Some(MappedFrames {
            source: FfmpegAvBufferRef::new_ref(source).map_err(|error| error.to_string())?,
            target,
        });
    }
    Ok(cached.as_ref().unwrap().target.clone())
}

fn source_frames(frame: *const ffi::AVFrame) -> Result<*mut ffi::AVBufferRef, String> {
    let frame = unsafe { frame.as_ref() }.ok_or("VA-API GPU 映射输入帧为空")?;
    if frame.format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
        return Err("VA-API GPU 映射要求 VA-API 硬件帧".to_string());
    }
    let buffer = unsafe { frame.hw_frames_ctx.as_ref() }.ok_or("VA-API 帧缺少 hw_frames_ctx")?;
    let frames = unsafe { (buffer.data as *const ffi::AVHWFramesContext).as_ref() }
        .ok_or("VA-API 帧缺少 AVHWFramesContext")?;
    if frames.format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI
        || !matches!(
            frames.sw_format,
            ffi::AVPixelFormat::AV_PIX_FMT_NV12 | ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        )
        || frame.width <= 0
        || frame.height <= 0
        || frame.width > frames.width
        || frame.height > frames.height
    {
        return Err(format!(
            "VA-API/Vulkan 映射不支持该帧格式或尺寸：format={:?}, frame={}x{}, pool={}x{}",
            frames.sw_format, frame.width, frame.height, frames.width, frames.height
        ));
    }
    Ok(frame.hw_frames_ctx)
}

fn map_frame_with(
    frame: *const ffi::AVFrame,
    target: *mut ffi::AVBufferRef,
    mut map: impl FnMut(*mut ffi::AVFrame, *const ffi::AVFrame, i32) -> i32,
) -> Result<AvFrame, String> {
    source_frames(frame)?;
    if target.is_null() {
        return Err("VA-API/Vulkan 映射缺少目标帧上下文".to_string());
    }
    let mut drm = AvFrame::new()?;
    unsafe { (*drm.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32 };
    let result = map(drm.as_mut_ptr(), frame, MAP_FLAGS);
    if result < 0 {
        return Err(mapping_error("VA-API→DRM PRIME", result));
    }
    if unsafe { (*drm.as_mut_ptr()).format } != ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
        || unsafe { (*drm.as_mut_ptr()).data[0].is_null() }
    {
        return Err("FFmpeg VA-API 导出未返回 DRM PRIME 帧".to_string());
    }
    let mut mapped = AvFrame::new()?;
    unsafe {
        (*mapped.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32;
        (*mapped.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(target);
        if (*mapped.as_mut_ptr()).hw_frames_ctx.is_null() {
            return Err("FFmpeg 复制 Vulkan 映射帧上下文引用失败".to_string());
        }
    }
    let result = map(mapped.as_mut_ptr(), drm.as_mut_ptr(), MAP_FLAGS);
    if result < 0 {
        return Err(mapping_error("DRM PRIME→Vulkan", result));
    }
    if unsafe { (*mapped.as_mut_ptr()).format } != ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32
        || unsafe { (*mapped.as_mut_ptr()).data[0].is_null() }
    {
        return Err("FFmpeg VA-API 映射未输出 Vulkan 硬件帧".to_string());
    }
    let result = unsafe { ffi::av_frame_copy_props(mapped.as_mut_ptr(), frame) };
    if result < 0 {
        return Err(format!(
            "FFmpeg 复制 VA-API/Vulkan 帧属性失败：{}",
            ffmpeg_error(result)
        ));
    }
    Ok(mapped)
}

fn mapping_error(stage: &str, result: i32) -> String {
    format!(
        "FFmpeg {stage} GPU 帧映射失败：code={result}, error={}；该模式不回退内存回传，请检查设备、DMA-BUF/modifier 支持或选择回传内存模式",
        ffmpeg_error(result)
    )
}

fn track_surface(frame: *mut ffi::AVFrame, counter: &Arc<AtomicUsize>) -> Result<(), String> {
    let slot = unsafe { (*frame).buf.iter_mut().find(|buffer| buffer.is_null()) }
        .ok_or("VA-API/Vulkan 帧没有可用的 surface 计数引用槽")?;
    let owner = Box::into_raw(Box::new(Arc::clone(counter)));
    let reference = unsafe {
        ffi::av_buffer_create(ptr::null_mut(), 0, Some(release_surface), owner.cast(), 0)
    };
    if reference.is_null() {
        unsafe { drop(Box::from_raw(owner)) };
        return Err("FFmpeg 创建 VA-API surface 计数引用失败".to_string());
    }
    counter.fetch_add(1, Ordering::AcqRel);
    *slot = reference;
    Ok(())
}

unsafe extern "C" fn release_surface(opaque: *mut std::ffi::c_void, _: *mut u8) {
    let counter = unsafe { Box::from_raw(opaque.cast::<Arc<AtomicUsize>>()) };
    counter.fetch_sub(1, Ordering::AcqRel);
}

#[cfg(test)]
mod tests;
