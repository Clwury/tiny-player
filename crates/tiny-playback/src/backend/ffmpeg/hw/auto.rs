use std::collections::VecDeque;
#[cfg(target_os = "linux")]
use std::ffi::CString;

use super::*;

const NVIDIA_VENDOR_ID: u32 = 0x10de;

/// A playback-local candidate list. Failed candidates are consumed permanently,
/// including across seeks and decoder recovery; user preferences remain Auto.
#[derive(Clone)]
pub(in crate::backend::ffmpeg) struct AutoDecodePlan {
    requested_mode: HardwareDecodeMode,
    remaining: VecDeque<HardwareDecodeMode>,
    vulkan: Result<Arc<VulkanDecodeDevice>, String>,
    #[cfg(target_os = "linux")]
    vaapi_path: Result<Option<CString>, String>,
}

impl AutoDecodePlan {
    pub(in crate::backend::ffmpeg) fn detect(requested_mode: HardwareDecodeMode) -> Self {
        debug_assert!(requested_mode.allows_fallback());
        #[cfg(target_os = "linux")]
        {
            let explicit_path = env::var_os("TINY_VAAPI_DEVICE")
                .filter(|path| !path.is_empty())
                .map(|path| CString::new(path.as_encoded_bytes()))
                .transpose()
                .map_err(|_| "TINY_VAAPI_DEVICE 包含无效的设备路径".to_string());
            let vulkan = explicit_path
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|path| create_auto_vulkan_device(path.as_deref()));
            let info = vulkan.as_ref().ok().and_then(|device| device_info(device));
            let vendor_id = info.as_ref().map(|info| info.vendor_id).or_else(|| {
                explicit_path
                    .as_ref()
                    .ok()
                    .and_then(|path| path.as_deref())
                    .and_then(render_node_vendor)
            });
            // Explicit nodes also select the Vulkan device through a derived
            // DRM context. Otherwise use the selected Vulkan GPU's render node,
            // never whichever card libva happens to find first.
            let vaapi_path = explicit_path.and_then(|path| match path {
                Some(path) => Ok(Some(path)),
                None if vulkan.is_err() => Ok(None),
                None => info
                    .and_then(|info| info.render_node)
                    .map(Some)
                    .ok_or_else(|| {
                        "无法匹配已选 Vulkan GPU 的 DRM render node，跳过 Auto VA-API 候选"
                            .to_string()
                    }),
            });
            tracing::info!(
                ?requested_mode,
                vendor_id,
                vaapi_device = ?vaapi_path.as_ref().ok(),
                vulkan_available = vulkan.is_ok(),
                "selected GPU for automatic hardware decoding"
            );
            Self {
                requested_mode,
                remaining: candidates(true, vendor_id, requested_mode).into(),
                vulkan,
                vaapi_path,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                requested_mode,
                remaining: candidates(false, None, requested_mode).into(),
                vulkan: create_vulkan_device(),
            }
        }
    }

    pub(in crate::backend::ffmpeg) fn requested_mode(&self) -> HardwareDecodeMode {
        self.requested_mode
    }

    pub(in crate::backend::ffmpeg) fn next(&mut self) -> Option<HardwareDecodeMode> {
        self.remaining.pop_front()
    }

    pub(in crate::backend::ffmpeg) fn has_next(&self) -> bool {
        !self.remaining.is_empty()
    }

    #[cfg(test)]
    pub(in crate::backend::ffmpeg) fn unavailable_for_test(
        requested_mode: HardwareDecodeMode,
    ) -> Self {
        Self {
            requested_mode,
            remaining: candidates(cfg!(target_os = "linux"), None, requested_mode).into(),
            vulkan: Err("test Vulkan device unavailable".to_string()),
            #[cfg(target_os = "linux")]
            vaapi_path: Err("test VA-API device unavailable".to_string()),
        }
    }

    pub(super) fn vulkan_device(&self) -> Result<Arc<VulkanDecodeDevice>, String> {
        self.vulkan.clone()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn vaapi_device_path(&self) -> Result<Option<&std::ffi::CStr>, String> {
        self.vaapi_path
            .as_ref()
            .map(|path| path.as_deref())
            .map_err(Clone::clone)
    }
}

fn candidates(
    linux: bool,
    vendor_id: Option<u32>,
    requested_mode: HardwareDecodeMode,
) -> Vec<HardwareDecodeMode> {
    use HardwareDecodeMode::{ForceVaapi, ForceVaapiVulkan, ForceVulkan, Off};
    let mut paths = if !linux {
        vec![ForceVulkan]
    } else if vendor_id == Some(NVIDIA_VENDOR_ID) {
        vec![ForceVulkan, ForceVaapiVulkan]
    } else {
        vec![ForceVaapiVulkan, ForceVulkan]
    };
    if linux && requested_mode == HardwareDecodeMode::AutoWithCopyback {
        paths.push(ForceVaapi);
    }
    paths.push(Off);
    paths
}

#[cfg(target_os = "linux")]
fn create_auto_vulkan_device(
    path: Option<&std::ffi::CStr>,
) -> Result<Arc<VulkanDecodeDevice>, String> {
    let Some(path) = path else {
        return create_vulkan_device();
    };
    let mut drm = ptr::null_mut();
    let result = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut drm,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        unsafe { ffi::av_buffer_unref(&mut drm) };
        return Err(format!("Auto 打开 DRM 设备失败：{}", ffmpeg_error(result)));
    }
    let mut vulkan = ptr::null_mut();
    let result = unsafe {
        ffi::av_hwdevice_ctx_create_derived(
            &mut vulkan,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
            drm,
            0,
        )
    };
    unsafe { ffi::av_buffer_unref(&mut drm) };
    let device = if result < 0 {
        Err(format!(
            "Auto 从 DRM 设备创建 Vulkan 设备失败：{}",
            ffmpeg_error(result)
        ))
    } else {
        vulkan_decode_device_from_ref(vulkan).map(Arc::new)
    };
    unsafe { ffi::av_buffer_unref(&mut vulkan) };
    device
}

#[cfg(target_os = "linux")]
struct DeviceInfo {
    vendor_id: u32,
    render_node: Option<CString>,
}

#[cfg(target_os = "linux")]
fn device_info(device: &VulkanDecodeDevice) -> Option<DeviceInfo> {
    let raw = device.device_ref();
    if raw.is_null() {
        return None;
    }
    let context = unsafe { ((*raw).data as *const ffi::AVHWDeviceContext).as_ref()? };
    if context.type_ != ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN {
        return None;
    }
    let vk = unsafe { (context.hwctx as *const vulkan_ffi::AVVulkanDeviceContext).as_ref()? };
    let get_proc_addr = vk.get_proc_addr?;
    let proc = unsafe { get_proc_addr(vk.inst, c"vkGetPhysicalDeviceProperties2".as_ptr())? };
    let get_properties: vulkan_ffi::PFN_vkGetPhysicalDeviceProperties2 = Some(unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(_, _)>(proc)
    });
    let mut drm: vulkan_ffi::VkPhysicalDeviceDrmPropertiesEXT = unsafe { std::mem::zeroed() };
    drm.sType = vulkan_ffi::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT;
    let mut properties: vulkan_ffi::VkPhysicalDeviceProperties2 = unsafe { std::mem::zeroed() };
    properties.sType = vulkan_ffi::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2;
    properties.pNext = (&mut drm as *mut vulkan_ffi::VkPhysicalDeviceDrmPropertiesEXT).cast();
    unsafe { get_properties?(vk.phys_dev, &mut properties) };
    Some(DeviceInfo {
        vendor_id: properties.properties.vendorID,
        render_node: render_node_path(drm.hasRender != 0, drm.renderMinor),
    })
}

#[cfg(target_os = "linux")]
fn render_node_path(has_render: bool, minor: i64) -> Option<CString> {
    (has_render && minor >= 128)
        .then(|| CString::new(format!("/dev/dri/renderD{minor}")).expect("DRM node has no NUL"))
}

#[cfg(target_os = "linux")]
fn render_node_vendor(path: &std::ffi::CStr) -> Option<u32> {
    use std::{os::unix::ffi::OsStrExt, path::Path};
    let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
    let path = std::fs::canonicalize(path).ok()?;
    let node = path.file_name()?.to_str()?;
    if !node.starts_with("renderD") {
        return None;
    }
    let vendor = std::fs::read_to_string(format!("/sys/class/drm/{node}/device/vendor")).ok()?;
    u32::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_candidate_order_matches_platform_and_selected_gpu() {
        use HardwareDecodeMode::*;
        assert_eq!(
            candidates(false, Some(NVIDIA_VENDOR_ID), Auto),
            [ForceVulkan, Off]
        );
        assert_eq!(candidates(false, None, Auto), [ForceVulkan, Off]);
        assert_eq!(
            candidates(true, Some(NVIDIA_VENDOR_ID), Auto),
            [ForceVulkan, ForceVaapiVulkan, Off]
        );
        for vendor in [Some(0x8086), Some(0x1002), None] {
            assert_eq!(
                candidates(true, vendor, Auto),
                [ForceVaapiVulkan, ForceVulkan, Off]
            );
        }
    }

    #[test]
    fn development_auto_retains_copyback_only_on_linux() {
        use HardwareDecodeMode::*;
        for vendor in [Some(NVIDIA_VENDOR_ID), Some(0x8086), Some(0x1002), None] {
            assert_eq!(
                candidates(false, vendor, AutoWithCopyback),
                [ForceVulkan, Off]
            );
            let mut expected = candidates(true, vendor, Auto);
            expected.insert(expected.len() - 1, ForceVaapi);
            assert_eq!(candidates(true, vendor, AutoWithCopyback), expected);
        }
    }

    #[test]
    fn failed_candidates_are_not_retried_when_the_plan_is_cloned_for_recovery() {
        for requested_mode in [
            HardwareDecodeMode::Auto,
            HardwareDecodeMode::AutoWithCopyback,
        ] {
            let mut plan = AutoDecodePlan {
                requested_mode,
                remaining: candidates(true, None, requested_mode).into(),
                vulkan: Err("no Vulkan device".to_string()),
                #[cfg(target_os = "linux")]
                vaapi_path: Ok(None),
            };
            assert_eq!(plan.next(), Some(HardwareDecodeMode::ForceVaapiVulkan));
            let mut recovery = plan.clone();
            assert_eq!(recovery.requested_mode(), requested_mode);
            assert_eq!(recovery.next(), Some(HardwareDecodeMode::ForceVulkan));
            if requested_mode == HardwareDecodeMode::AutoWithCopyback {
                assert_eq!(recovery.next(), Some(HardwareDecodeMode::ForceVaapi));
            }
            assert_eq!(recovery.next(), Some(HardwareDecodeMode::Off));
            assert!(!recovery.has_next());
            assert_eq!(recovery.next(), None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drm_node_matching_does_not_assume_the_first_render_device() {
        assert_eq!(
            render_node_path(true, 130).unwrap().to_bytes(),
            b"/dev/dri/renderD130"
        );
        assert!(render_node_path(false, 128).is_none());
        assert!(render_node_path(true, 0).is_none());
        assert!(render_node_path(true, -1).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires an NVIDIA GPU, Vulkan and a DRM render node"]
    fn nvidia_auto_plan_matches_the_selected_vulkan_gpu_and_vaapi_render_node() {
        let mut plan = AutoDecodePlan::detect(HardwareDecodeMode::Auto);
        let device = plan.vulkan_device().unwrap();
        let info = device_info(&device).unwrap();
        assert_eq!(info.vendor_id, NVIDIA_VENDOR_ID);
        assert_eq!(plan.next(), Some(HardwareDecodeMode::ForceVulkan));
        let render_node = plan.vaapi_device_path().unwrap().unwrap();
        assert_eq!(render_node_vendor(render_node), Some(NVIDIA_VENDOR_ID));
        assert_eq!(Some(render_node), info.render_node.as_deref());
        let explicit_device = create_auto_vulkan_device(Some(render_node)).unwrap();
        let explicit_info = device_info(&explicit_device).unwrap();
        assert_eq!(explicit_info.vendor_id, NVIDIA_VENDOR_ID);
        assert_eq!(explicit_info.render_node.as_deref(), Some(render_node));
        assert_eq!(plan.next(), Some(HardwareDecodeMode::ForceVaapiVulkan));
    }
}
