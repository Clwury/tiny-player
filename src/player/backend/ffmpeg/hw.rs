use super::*;
use crate::player::ffmpeg_vulkan as vulkan_ffi;

const TINY_HWDEC_ENV: &str = "TINY_HWDEC";
const VULKAN_EXTRA_HW_FRAMES: c_int = 24;
const VK_QUEUE_GRAPHICS_BIT: u32 = 0x0000_0001;
const VK_QUEUE_COMPUTE_BIT: u32 = 0x0000_0002;
const VK_QUEUE_TRANSFER_BIT: u32 = 0x0000_0004;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum HardwareDecodeMode {
    #[default]
    Off,
    Auto,
    ForceVulkan,
}

impl HardwareDecodeMode {
    pub(super) fn from_env() -> Self {
        env::var(TINY_HWDEC_ENV)
            .ok()
            .and_then(|value| Self::parse(&value))
            .unwrap_or_default()
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" | "no" | "disabled" | "software" | "sw" => Some(Self::Off),
            "1" | "on" | "true" | "yes" | "auto" | "vulkan-auto" => Some(Self::Auto),
            "vulkan" | "force" | "force-vulkan" | "vulkan-force" => Some(Self::ForceVulkan),
            _ => None,
        }
    }

    pub(super) fn should_try_vulkan(self) -> bool {
        matches!(self, Self::Auto | Self::ForceVulkan)
    }

    pub(super) fn allows_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

#[derive(Clone)]
pub(super) struct VideoHwDecodeContext {
    pixel_format: ffi::AVPixelFormat,
    device: Arc<VulkanDecodeDevice>,
}

impl VideoHwDecodeContext {
    pub(super) fn try_create(codec: *const ffi::AVCodec) -> std::result::Result<Self, String> {
        let config = find_vulkan_hw_config(codec)
            .ok_or_else(|| "FFmpeg 解码器不支持 Vulkan 硬件帧输出".to_string())?;
        let mut device_ref = ptr::null_mut();
        let result = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device_ref,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(format!(
                "FFmpeg 创建 Vulkan 硬解设备失败：{}",
                ffmpeg_error(result)
            ));
        }

        let device = match vulkan_decode_device_from_ref(device_ref) {
            Ok(device) => device,
            Err(error) => {
                unsafe { ffi::av_buffer_unref(&mut device_ref) };
                return Err(error);
            }
        };
        unsafe { ffi::av_buffer_unref(&mut device_ref) };

        Ok(Self {
            pixel_format: config.pixel_format,
            device: Arc::new(device),
        })
    }

    pub(super) fn pixel_format(&self) -> ffi::AVPixelFormat {
        self.pixel_format
    }

    pub(super) fn device(&self) -> Arc<VulkanDecodeDevice> {
        self.device.clone()
    }

    pub(super) fn attach_to_decoder(
        &self,
        context: *mut ffi::AVCodecContext,
    ) -> std::result::Result<(), String> {
        let device_ref = unsafe { ffi::av_buffer_ref(self.device.device_ref()) };
        if device_ref.is_null() {
            return Err("FFmpeg 复制 Vulkan 硬解设备引用失败".to_string());
        }
        unsafe {
            (*context).hw_device_ctx = device_ref;
            (*context).extra_hw_frames = (*context).extra_hw_frames.max(VULKAN_EXTRA_HW_FRAMES);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VulkanHwConfig {
    pixel_format: ffi::AVPixelFormat,
}

fn find_vulkan_hw_config(codec: *const ffi::AVCodec) -> Option<VulkanHwConfig> {
    if codec.is_null() {
        return None;
    }

    let mut index = 0;
    loop {
        let config = unsafe { ffi::avcodec_get_hw_config(codec, index) };
        if config.is_null() {
            return None;
        }

        let config = unsafe { &*config };
        let has_device_ctx =
            config.methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0;
        if has_device_ctx
            && config.device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN
            && config.pix_fmt == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN
        {
            return Some(VulkanHwConfig {
                pixel_format: config.pix_fmt,
            });
        }
        index += 1;
    }
}

fn vulkan_decode_device_from_ref(
    device_ref: *mut ffi::AVBufferRef,
) -> std::result::Result<VulkanDecodeDevice, String> {
    if device_ref.is_null() {
        return Err("FFmpeg Vulkan 设备引用为空".to_string());
    }
    let hw_device = unsafe { (*device_ref).data as *mut ffi::AVHWDeviceContext };
    if hw_device.is_null() {
        return Err("FFmpeg Vulkan 设备缺少 AVHWDeviceContext".to_string());
    }
    let vulkan = unsafe { (*hw_device).hwctx as *const vulkan_ffi::AVVulkanDeviceContext };
    if vulkan.is_null() {
        return Err("FFmpeg Vulkan 设备缺少 AVVulkanDeviceContext".to_string());
    }

    let vulkan = unsafe { &*vulkan };
    let queues = select_decode_queues(vulkan)?;
    let buffer_ref = FfmpegAvBufferRef::new_ref(device_ref).map_err(|error| error.to_string())?;
    Ok(VulkanDecodeDevice::new(
        buffer_ref,
        vulkan.inst as usize,
        vulkan
            .get_proc_addr
            .map(|function| function as usize)
            .unwrap_or(0),
        vulkan.phys_dev as usize,
        vulkan.act_dev as usize,
        vulkan.enabled_dev_extensions as usize,
        vulkan.nb_enabled_dev_extensions,
        (&vulkan.device_features as *const vulkan_ffi::VkPhysicalDeviceFeatures2) as usize,
        queues,
    ))
}

fn select_decode_queues(
    vulkan: &vulkan_ffi::AVVulkanDeviceContext,
) -> std::result::Result<VulkanDecodeQueues, String> {
    let queue_count =
        usize::try_from(vulkan.nb_qf).map_err(|_| "FFmpeg Vulkan 队列数量无效".to_string())?;
    let queue_families = &vulkan.qf[..queue_count.min(vulkan.qf.len())];
    let graphics = find_queue(queue_families, VK_QUEUE_GRAPHICS_BIT)
        .ok_or_else(|| "FFmpeg Vulkan 设备缺少可供 libplacebo 渲染的 graphics queue".to_string())?;
    let compute = find_queue(queue_families, VK_QUEUE_COMPUTE_BIT);
    let transfer = find_queue(queue_families, VK_QUEUE_TRANSFER_BIT);

    Ok(VulkanDecodeQueues {
        graphics,
        compute,
        transfer,
    })
}

fn find_queue(
    queue_families: &[vulkan_ffi::AVVulkanDeviceQueueFamily],
    flag: u32,
) -> Option<VulkanDecodeQueue> {
    queue_families.iter().find_map(|queue| {
        let flags = queue.flags;
        if queue.idx < 0 || queue.num <= 0 || flags & flag == 0 {
            return None;
        }
        Some(VulkanDecodeQueue {
            index: u32::try_from(queue.idx).ok()?,
            count: u32::try_from(queue.num).ok()?,
        })
    })
}

pub(super) fn is_vulkan_frame(frame: *const ffi::AVFrame) -> bool {
    unsafe {
        !frame.is_null()
            && (*frame).format == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as c_int
            && !(*frame).data[0].is_null()
    }
}

pub(super) fn vulkan_sw_format(frame: *const ffi::AVFrame) -> Option<c_int> {
    let hw_frames_ctx = unsafe { (*frame).hw_frames_ctx };
    if hw_frames_ctx.is_null() {
        return None;
    }
    let frames = unsafe { (*hw_frames_ctx).data as *const ffi::AVHWFramesContext };
    if frames.is_null() {
        return None;
    }
    Some(unsafe { (*frames).sw_format as c_int })
}

pub(super) struct VulkanFrameImages {
    pub(super) usage: u32,
    pub(super) planes: Vec<VulkanVideoPlane>,
}

pub(super) fn vulkan_frame_planes(
    frame: *const ffi::AVFrame,
    raw_format: RawVideoFormat,
) -> std::result::Result<VulkanFrameImages, String> {
    if !is_vulkan_frame(frame) {
        return Err("FFmpeg 帧不是 Vulkan 硬件帧".to_string());
    }
    let vk_frame = unsafe { (*frame).data[0] as *const vulkan_ffi::AVVkFrame };
    if vk_frame.is_null() {
        return Err("FFmpeg Vulkan 帧缺少 AVVkFrame".to_string());
    }
    let hw_frames_ctx = unsafe { (*frame).hw_frames_ctx };
    if hw_frames_ctx.is_null() {
        return Err("FFmpeg Vulkan 帧缺少 hw_frames_ctx".to_string());
    }
    let frames = unsafe { (*hw_frames_ctx).data as *const ffi::AVHWFramesContext };
    if frames.is_null() {
        return Err("FFmpeg Vulkan 帧缺少 AVHWFramesContext".to_string());
    }
    let vk_frames = unsafe { (*frames).hwctx as *const vulkan_ffi::AVVulkanFramesContext };
    if vk_frames.is_null() {
        return Err("FFmpeg Vulkan 帧缺少 AVVulkanFramesContext".to_string());
    }

    let vk_frame = unsafe { &*vk_frame };
    let vk_frames = unsafe { &*vk_frames };
    let mut planes = Vec::new();
    for plane_index in 0..vk_frame.img.len() {
        let image = vk_frame.img[plane_index] as usize;
        if image == 0 {
            continue;
        }
        planes.push(VulkanVideoPlane {
            image,
            format: vk_frames.format[plane_index],
            layout: vk_frame.layout[plane_index],
            queue_family: vk_frame.queue_family[plane_index],
            semaphore: vk_frame.sem[plane_index] as usize,
            semaphore_value: vk_frame.sem_value[plane_index],
        });
    }

    if planes.is_empty() {
        return Err("FFmpeg Vulkan 帧缺少可渲染的 VkImage".to_string());
    }
    if planes.len() > raw_format.plane_count() {
        planes.truncate(raw_format.plane_count());
    }
    Ok(VulkanFrameImages {
        usage: vk_frames.usage,
        planes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_decode_mode_parses_disabled_values() {
        assert_eq!(
            HardwareDecodeMode::parse("off"),
            Some(HardwareDecodeMode::Off)
        );
        assert_eq!(
            HardwareDecodeMode::parse("software"),
            Some(HardwareDecodeMode::Off)
        );
        assert_eq!(
            HardwareDecodeMode::parse("0"),
            Some(HardwareDecodeMode::Off)
        );
    }

    #[test]
    fn hardware_decode_mode_parses_auto_values() {
        assert_eq!(
            HardwareDecodeMode::parse("auto"),
            Some(HardwareDecodeMode::Auto)
        );
        assert_eq!(
            HardwareDecodeMode::parse("on"),
            Some(HardwareDecodeMode::Auto)
        );
    }

    #[test]
    fn hardware_decode_mode_parses_force_vulkan_values() {
        assert_eq!(
            HardwareDecodeMode::parse("vulkan"),
            Some(HardwareDecodeMode::ForceVulkan)
        );
        assert_eq!(
            HardwareDecodeMode::parse("force-vulkan"),
            Some(HardwareDecodeMode::ForceVulkan)
        );
    }

    #[test]
    fn hardware_decode_mode_rejects_unknown_values() {
        assert_eq!(HardwareDecodeMode::parse("vaapi"), None);
    }

    #[test]
    fn hardware_decode_mode_fallback_policy_matches_mode() {
        assert!(!HardwareDecodeMode::Off.should_try_vulkan());
        assert!(HardwareDecodeMode::Auto.should_try_vulkan());
        assert!(HardwareDecodeMode::Auto.allows_fallback());
        assert!(HardwareDecodeMode::ForceVulkan.should_try_vulkan());
        assert!(!HardwareDecodeMode::ForceVulkan.allows_fallback());
    }

    #[test]
    fn queue_selection_prefers_matching_capability() {
        let queues = [
            vulkan_ffi::AVVulkanDeviceQueueFamily {
                idx: 2,
                num: 1,
                flags: VK_QUEUE_TRANSFER_BIT as _,
                video_caps: 0 as _,
            },
            vulkan_ffi::AVVulkanDeviceQueueFamily {
                idx: 4,
                num: 2,
                flags: VK_QUEUE_GRAPHICS_BIT as _,
                video_caps: 0 as _,
            },
        ];

        assert_eq!(
            find_queue(&queues, VK_QUEUE_GRAPHICS_BIT),
            Some(VulkanDecodeQueue { index: 4, count: 2 })
        );
    }
}
