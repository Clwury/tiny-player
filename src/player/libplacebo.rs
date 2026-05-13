use std::{ffi::CString, mem, os::raw::c_void, ptr};

use anyhow::{Context, Result, anyhow};
use dolby_vision::rpu::{
    dovi_rpu::DoviRpu,
    profiles::{DoviProfile, profile5::Profile5},
    rpu_data_mapping::{DoviMappingMethod, RpuDataMapping},
    vdr_dm_data::VdrDmData,
};
use ffmpeg_sys_next as ffmpeg_ffi;

use super::{
    dovi::DoviFrameMetadata,
    ffmpeg_vulkan,
    render_host::{
        FrameColor, RawVideoChromaSite, RawVideoFormat, RawVideoFrame, RawVideoPlanes,
        RawVideoRange, RenderSize, VulkanDecodeDevice, VulkanDecodeQueue, VulkanVideoFrame,
        frame_byte_len,
    },
};

mod color;
mod dovi;
mod ffi;
mod upload;

use color::{
    OutputTextureFormat, apply_chroma_location, rect_for_size, source_color_repr,
    source_color_space, swap_red_blue_channels, target_color_repr, target_color_space,
};
use dovi::{DoviMetadataCache, apply_dovi_hdr_metadata};
#[cfg(test)]
use dovi::{
    DoviRenderMetadata, dovi_coefficient, dovi_matrix_coefficient,
    is_dovi_tool_profile5_default_color, map_dovi_metadata, profile5_default_dovi_color,
};

pub struct LibplaceboToneMapper {
    vulkan: ffi::pl_vulkan,
    gpu: ffi::pl_gpu,
    renderer: ffi::pl_renderer,
    source_textures: [ffi::pl_tex; 4],
    target_texture: ffi::pl_tex,
    target_format: Option<OutputTextureFormat>,
    dovi_cache: DoviMetadataCache,
    vulkan_device_key: Option<usize>,
}

impl LibplaceboToneMapper {
    pub fn new() -> Result<Self> {
        unsafe {
            let vulkan = ffi::pl_vulkan_create(ptr::null(), ptr::null());
            if vulkan.is_null() {
                return Err(anyhow!("初始化 libplacebo Vulkan 设备失败"));
            }

            let gpu = (*vulkan).gpu;
            if gpu.is_null() {
                let mut vulkan = vulkan;
                ffi::pl_vulkan_destroy(&mut vulkan);
                return Err(anyhow!("libplacebo Vulkan 设备缺少 GPU"));
            }

            Self::from_vulkan(vulkan, None)
        }
    }

    pub fn new_for_vulkan_decode(device: &VulkanDecodeDevice) -> Result<Self> {
        unsafe {
            let mut params = mem::zeroed::<ffi::pl_vulkan_import_params>();
            params.instance = device.instance as ffi::VkInstance;
            params.get_proc_addr = get_proc_addr_from_usize(device.get_proc_addr);
            params.phys_device = device.physical_device as ffi::VkPhysicalDevice;
            params.device = device.device as ffi::VkDevice;
            params.extensions = device.extensions as *const *const i8;
            params.num_extensions = device.num_extensions;
            params.features = device.features as *const ffi::VkPhysicalDeviceFeatures2;
            params.queue_graphics = pl_queue(device.queues.graphics);
            params.queue_compute = pl_queue(device.queues.graphics);
            params.queue_transfer = pl_queue(device.queues.graphics);
            params.no_compute = true;

            let vulkan = ffi::pl_vulkan_import(ptr::null(), &params);
            if vulkan.is_null() {
                return Err(anyhow!("libplacebo 导入 FFmpeg Vulkan 设备失败"));
            }
            if (*vulkan).gpu.is_null() {
                let mut vulkan = vulkan;
                ffi::pl_vulkan_destroy(&mut vulkan);
                return Err(anyhow!("libplacebo 导入的 Vulkan 设备缺少 GPU"));
            }

            Self::from_vulkan(vulkan, Some(device.key()))
        }
    }

    unsafe fn from_vulkan(
        vulkan: ffi::pl_vulkan,
        vulkan_device_key: Option<usize>,
    ) -> Result<Self> {
        unsafe {
            let gpu = (*vulkan).gpu;
            if gpu.is_null() {
                let mut vulkan = vulkan;
                ffi::pl_vulkan_destroy(&mut vulkan);
                return Err(anyhow!("libplacebo Vulkan 设备缺少 GPU"));
            }

            let renderer = ffi::pl_renderer_create(ptr::null(), gpu);
            if renderer.is_null() {
                let mut vulkan = vulkan;
                ffi::pl_vulkan_destroy(&mut vulkan);
                return Err(anyhow!("初始化 libplacebo renderer 失败"));
            }

            Ok(Self {
                vulkan,
                gpu,
                renderer,
                source_textures: [ptr::null(); 4],
                target_texture: ptr::null(),
                target_format: None,
                dovi_cache: DoviMetadataCache::default(),
                vulkan_device_key,
            })
        }
    }

    pub fn matches_vulkan_decode_device(&self, device: &VulkanDecodeDevice) -> bool {
        self.vulkan_device_key == Some(device.key())
    }

    pub fn tone_map_to_bgra8(
        &mut self,
        input: &RawVideoFrame,
        source_size: RenderSize,
        output_size: RenderSize,
    ) -> Result<Vec<u8>> {
        if output_size.width == 0 || output_size.height == 0 {
            return Err(anyhow!("invalid video frame dimensions"));
        }

        unsafe {
            let source = self.upload_source_frame(input, source_size)?;
            let output_format = self.ensure_target_texture(output_size)?;
            let target = self.target_frame(output_size);

            if !ffi::pl_render_image(
                self.renderer,
                &source.frame,
                &target,
                &ffi::pl_render_default_params,
            ) {
                return Err(anyhow!("libplacebo 渲染 HDR 视频帧失败"));
            }

            let len = frame_byte_len(output_size)?;
            let mut pixels = Vec::<u8>::with_capacity(len);
            let mut transfer = mem::zeroed::<ffi::pl_tex_transfer_params>();
            transfer.tex = self.target_texture;
            transfer.row_pitch = usize::try_from(output_size.width)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .ok_or_else(|| anyhow!("video frame row is too large"))?;
            transfer.ptr = pixels.as_mut_ptr().cast::<c_void>();

            if !ffi::pl_tex_download(self.gpu, &transfer) {
                return Err(anyhow!("libplacebo 读取视频帧失败"));
            }
            pixels.set_len(len);

            if output_format == OutputTextureFormat::Rgba {
                swap_red_blue_channels(&mut pixels);
            }
            Ok(pixels)
        }
    }

    pub fn tone_map_vulkan_to_bgra8(
        &mut self,
        input: &VulkanVideoFrame,
        source_size: RenderSize,
        output_size: RenderSize,
    ) -> Result<Vec<u8>> {
        if output_size.width == 0 || output_size.height == 0 {
            return Err(anyhow!("invalid video frame dimensions"));
        }

        unsafe {
            let mut source = self.wrap_vulkan_source_frame(input, source_size)?;
            let output_format = self.ensure_target_texture(output_size)?;
            let target = self.target_frame(output_size);

            let rendered = ffi::pl_render_image(
                self.renderer,
                &source.frame,
                &target,
                &ffi::pl_render_default_params,
            );
            source.hold_vulkan_images_after_render(self.gpu, input)?;
            if !rendered {
                return Err(anyhow!("libplacebo 渲染 Vulkan 视频帧失败"));
            }

            let len = frame_byte_len(output_size)?;
            let mut pixels = Vec::<u8>::with_capacity(len);
            let mut transfer = mem::zeroed::<ffi::pl_tex_transfer_params>();
            transfer.tex = self.target_texture;
            transfer.row_pitch = usize::try_from(output_size.width)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .ok_or_else(|| anyhow!("video frame row is too large"))?;
            transfer.ptr = pixels.as_mut_ptr().cast::<c_void>();

            if !ffi::pl_tex_download(self.gpu, &transfer) {
                return Err(anyhow!("libplacebo 读取 Vulkan 视频帧失败"));
            }
            pixels.set_len(len);

            if output_format == OutputTextureFormat::Rgba {
                swap_red_blue_channels(&mut pixels);
            }
            Ok(pixels)
        }
    }
}

fn pl_queue(queue: VulkanDecodeQueue) -> ffi::pl_vulkan_queue {
    ffi::pl_vulkan_queue {
        index: queue.index,
        count: queue.count,
    }
}

fn get_proc_addr_from_usize(address: usize) -> ffi::PFN_vkGetInstanceProcAddr {
    if address == 0 {
        None
    } else {
        Some(unsafe {
            mem::transmute::<
                usize,
                unsafe extern "C" fn(ffi::VkInstance, *const i8) -> ffi::PFN_vkVoidFunction,
            >(address)
        })
    }
}

impl Drop for LibplaceboToneMapper {
    fn drop(&mut self) {
        unsafe {
            if !self.renderer.is_null() {
                ffi::pl_renderer_destroy(&mut self.renderer);
            }
            for texture in &mut self.source_textures {
                if !texture.is_null() {
                    ffi::pl_tex_destroy(self.gpu, texture);
                }
            }
            if !self.target_texture.is_null() {
                ffi::pl_tex_destroy(self.gpu, &mut self.target_texture);
            }
            if !self.vulkan.is_null() {
                ffi::pl_vulkan_destroy(&mut self.vulkan);
            }
        }
    }
}

#[cfg(test)]
mod tests;
