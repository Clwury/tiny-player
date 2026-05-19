use super::*;

pub(super) struct UploadedSourceFrame {
    pub(super) frame: ffi::pl_frame,
    _dovi_metadata: Option<Box<ffi::pl_dovi_metadata>>,
    wrapped_vulkan_planes: Vec<WrappedVulkanPlane>,
    gpu: ffi::pl_gpu,
}

struct WrappedVulkanPlane {
    texture: ffi::pl_tex,
    plane_index: usize,
    held_by_user: bool,
    cached: bool,
}

pub(super) struct AvVulkanFrameAccess {
    frames: *mut ffmpeg_vulkan::AVHWFramesContext,
    vulkan_frames: *mut ffmpeg_vulkan::AVVulkanFramesContext,
    vulkan_frame: *mut ffmpeg_vulkan::AVVkFrame,
    locked: bool,
}

struct WrappedVulkanTextureGuard {
    gpu: ffi::pl_gpu,
    texture: ffi::pl_tex,
}

impl WrappedVulkanTextureGuard {
    unsafe fn new(gpu: ffi::pl_gpu, params: &ffi::pl_vulkan_wrap_params) -> Result<Self> {
        let texture = unsafe { ffi::pl_vulkan_wrap(gpu, params) };
        if texture.is_null() {
            return Err(anyhow!("libplacebo 包装 Vulkan 视频帧失败"));
        }
        Ok(Self { gpu, texture })
    }

    fn into_texture(mut self) -> ffi::pl_tex {
        let texture = self.texture;
        self.texture = ptr::null();
        texture
    }
}

impl Drop for WrappedVulkanTextureGuard {
    fn drop(&mut self) {
        if !self.gpu.is_null() && !self.texture.is_null() {
            unsafe {
                ffi::pl_tex_destroy(self.gpu, &mut self.texture);
            }
        }
    }
}

const VK_QUEUE_FAMILY_IGNORED: u32 = u32::MAX;
const MAX_CACHED_VULKAN_TEXTURES: usize = 64;

fn vulkan_image_usage(usage: u32) -> ffi::VkImageUsageFlags {
    usage as ffi::VkImageUsageFlags
}

impl LibplaceboToneMapper {
    pub(super) unsafe fn upload_source_frame(
        &mut self,
        input: &RawVideoFrame,
        size: RenderSize,
    ) -> Result<UploadedSourceFrame> {
        if input.planes.len() != input.format.plane_count() {
            return Err(anyhow!("invalid raw video plane count"));
        }

        let prepared_dovi = self.dovi_cache.prepare_raw_video(input)?;
        let mut frame = unsafe { mem::zeroed::<ffi::pl_frame>() };
        frame.num_planes = input.planes.len() as i32;
        frame.repr = unsafe { source_color_repr(input.format, input.color, input.range) };
        frame.color = unsafe { source_color_space(input.color) };
        let dovi_metadata = prepared_dovi.map(|prepared| {
            apply_dovi_source_luminance_metadata(&mut frame.color, &prepared);
            Box::new(prepared.placebo)
        });
        if let Some(dovi_metadata) = dovi_metadata.as_ref() {
            frame.repr.dovi = dovi_metadata.as_ref() as *const ffi::pl_dovi_metadata;
        }
        frame.crop = rect_for_size(size);

        let RawVideoPlanes::Owned(planes) = &input.planes;
        for (plane_index, plane) in planes.iter().enumerate() {
            unsafe {
                self.upload_source_plane(
                    &mut frame,
                    input,
                    size,
                    plane_index,
                    &plane.data,
                    plane.stride,
                )?;
            }
        }
        unsafe { apply_chroma_location(&mut frame, input.format, input.chroma_site) };

        Ok(UploadedSourceFrame {
            frame,
            _dovi_metadata: dovi_metadata,
            wrapped_vulkan_planes: Vec::new(),
            gpu: self.gpu,
        })
    }

    pub(super) unsafe fn wrap_vulkan_source_frame(
        &mut self,
        input: &VulkanVideoFrame,
        size: RenderSize,
    ) -> Result<UploadedSourceFrame> {
        if input.planes.is_empty() {
            return Err(anyhow!("Vulkan video frame has no planes"));
        }

        let raw = RawVideoFrame {
            format: input.format,
            color: input.color,
            range: input.range,
            chroma_site: input.chroma_site,
            metadata: input.metadata.clone(),
            planes: RawVideoPlanes::Owned(Vec::new()),
        };
        let prepared_dovi = self.dovi_cache.prepare_raw_video(&raw)?;
        let mut frame = unsafe { mem::zeroed::<ffi::pl_frame>() };
        let expected_plane_count = input.format.plane_count();
        let single_multiplane_image = input.planes.len() == 1 && expected_plane_count > 1;
        if !single_multiplane_image && input.planes.len() != expected_plane_count {
            return Err(anyhow!(
                "Vulkan video frame plane count does not match decoded format"
            ));
        }
        frame.num_planes =
            i32::try_from(expected_plane_count).map_err(|_| anyhow!("too many Vulkan planes"))?;
        frame.repr = unsafe { source_color_repr(input.format, input.color, input.range) };
        frame.color = unsafe { source_color_space(input.color) };
        let dovi_metadata = prepared_dovi.map(|prepared| {
            apply_dovi_source_luminance_metadata(&mut frame.color, &prepared);
            Box::new(prepared.placebo)
        });
        if let Some(dovi_metadata) = dovi_metadata.as_ref() {
            frame.repr.dovi = dovi_metadata.as_ref() as *const ffi::pl_dovi_metadata;
        }
        frame.crop = rect_for_size(size);

        let mut wrapped_vulkan_planes = Vec::with_capacity(input.planes.len());
        if single_multiplane_image {
            let plane = &input.planes[0];
            if plane.image == 0 {
                return Err(anyhow!("Vulkan video plane has a null VkImage"));
            }

            let mut wrap_params = unsafe { mem::zeroed::<ffi::pl_vulkan_wrap_params>() };
            wrap_params.image = plane.image as ffi::VkImage;
            wrap_params.width =
                i32::try_from(size.width).map_err(|_| anyhow!("video frame is too wide"))?;
            wrap_params.height =
                i32::try_from(size.height).map_err(|_| anyhow!("video frame is too tall"))?;
            wrap_params.format = plane.format as ffi::VkFormat;
            wrap_params.usage = vulkan_image_usage(input.usage);

            let key = VulkanTextureKey {
                image: plane.image,
                format: plane.format,
                usage: input.usage,
                width: wrap_params.width,
                height: wrap_params.height,
            };
            let texture = unsafe { self.cached_vulkan_texture(input, key, &wrap_params)? };
            let format = unsafe { (*texture).params.format };
            let texture_plane_count = if format.is_null() {
                0
            } else {
                usize::try_from(unsafe { (*format).num_planes }).unwrap_or(0)
            };
            if texture_plane_count < expected_plane_count {
                return Err(anyhow!("libplacebo Vulkan 多平面纹理缺少子平面"));
            }

            for plane_index in 0..expected_plane_count {
                let plane_texture = unsafe { (*texture).planes[plane_index] };
                if plane_texture.is_null() {
                    return Err(anyhow!("libplacebo Vulkan 多平面纹理子平面为空"));
                }
                let layout = input
                    .format
                    .plane_layout_for_color(size, plane_index, input.color)?;
                let mut out_plane = unsafe { mem::zeroed::<ffi::pl_plane>() };
                out_plane.texture = plane_texture;
                out_plane.flipped = false;
                out_plane.shift_x = 0.0;
                out_plane.shift_y = 0.0;
                out_plane.components = i32::try_from(layout.components)
                    .map_err(|_| anyhow!("video frame has too many components"))?;
                out_plane.component_mapping = layout.component_map;
                frame.planes[plane_index] = out_plane;
            }
            wrapped_vulkan_planes.push(WrappedVulkanPlane {
                texture,
                plane_index: 0,
                held_by_user: true,
                cached: true,
            });
        } else {
            for (plane_index, plane) in input.planes.iter().enumerate() {
                if plane.image == 0 {
                    return Err(anyhow!("Vulkan video plane has a null VkImage"));
                }

                let layout = input
                    .format
                    .plane_layout_for_color(size, plane_index, input.color)?;
                let mut wrap_params = unsafe { mem::zeroed::<ffi::pl_vulkan_wrap_params>() };
                wrap_params.image = plane.image as ffi::VkImage;
                wrap_params.width =
                    i32::try_from(layout.width).map_err(|_| anyhow!("video frame is too wide"))?;
                wrap_params.height =
                    i32::try_from(layout.height).map_err(|_| anyhow!("video frame is too tall"))?;
                wrap_params.format = plane.format as ffi::VkFormat;
                wrap_params.usage = vulkan_image_usage(input.usage);

                let key = VulkanTextureKey {
                    image: plane.image,
                    format: plane.format,
                    usage: input.usage,
                    width: wrap_params.width,
                    height: wrap_params.height,
                };
                let texture = unsafe { self.cached_vulkan_texture(input, key, &wrap_params)? };
                let mut out_plane = unsafe { mem::zeroed::<ffi::pl_plane>() };
                out_plane.texture = texture;
                out_plane.flipped = false;
                out_plane.shift_x = 0.0;
                out_plane.shift_y = 0.0;
                out_plane.components = i32::try_from(layout.components)
                    .map_err(|_| anyhow!("video frame has too many components"))?;
                out_plane.component_mapping = layout.component_map;
                frame.planes[plane_index] = out_plane;
                wrapped_vulkan_planes.push(WrappedVulkanPlane {
                    texture,
                    plane_index,
                    held_by_user: true,
                    cached: true,
                });
            }
        }

        unsafe { apply_chroma_location(&mut frame, input.format, input.chroma_site) };
        Ok(UploadedSourceFrame {
            frame,
            _dovi_metadata: dovi_metadata,
            wrapped_vulkan_planes,
            gpu: self.gpu,
        })
    }

    pub(super) unsafe fn upload_source_plane(
        &mut self,
        frame: &mut ffi::pl_frame,
        input: &RawVideoFrame,
        size: RenderSize,
        plane_index: usize,
        data: &[u8],
        stride: usize,
    ) -> Result<()> {
        let layout = input
            .format
            .plane_layout_for_color(size, plane_index, input.color)?;
        let mut plane_data = unsafe { mem::zeroed::<ffi::pl_plane_data>() };
        plane_data.type_ = ffi::pl_fmt_type_PL_FMT_UNORM;
        plane_data.width =
            i32::try_from(layout.width).map_err(|_| anyhow!("video frame is too wide"))?;
        plane_data.height =
            i32::try_from(layout.height).map_err(|_| anyhow!("video frame is too tall"))?;
        for component in 0..layout.components {
            plane_data.component_size[component] = input.format.component_size();
        }
        plane_data.component_map = layout.component_map;
        plane_data.pixel_stride = layout.pixel_stride;
        plane_data.row_stride = stride;
        plane_data.pixels = data.as_ptr().cast::<c_void>();

        let mut out_plane = unsafe { mem::zeroed::<ffi::pl_plane>() };
        if !unsafe {
            ffi::pl_upload_plane(
                self.gpu,
                &mut out_plane,
                &mut self.source_textures[plane_index],
                &plane_data,
            )
        } {
            return Err(anyhow!("libplacebo 上传视频帧平面失败"));
        }
        out_plane.flipped = false;
        out_plane.shift_x = 0.0;
        out_plane.shift_y = 0.0;
        frame.planes[plane_index] = out_plane;
        Ok(())
    }

    unsafe fn cached_vulkan_texture(
        &mut self,
        input: &VulkanVideoFrame,
        key: VulkanTextureKey,
        params: &ffi::pl_vulkan_wrap_params,
    ) -> Result<ffi::pl_tex> {
        self.vulkan_texture_generation = self.vulkan_texture_generation.wrapping_add(1).max(1);
        let generation = self.vulkan_texture_generation;
        if let Some(entry) = self
            .vulkan_texture_cache
            .iter_mut()
            .find(|entry| entry.key == key)
        {
            entry.last_used = generation;
            return Ok(entry.texture);
        }

        if let Some(index) = self
            .vulkan_texture_cache
            .iter()
            .position(|entry| entry.key.image == key.image)
        {
            self.vulkan_texture_cache.swap_remove(index);
        }

        let hw_frames_ref = unsafe { vulkan_hw_frames_ref(input)? };
        let texture = unsafe { WrappedVulkanTextureGuard::new(self.gpu, params)? }.into_texture();
        self.vulkan_texture_cache.push(VulkanTextureCacheEntry {
            key,
            texture,
            gpu: self.gpu,
            last_used: generation,
            _hw_frames_ref: hw_frames_ref,
        });
        self.prune_vulkan_texture_cache();
        Ok(texture)
    }

    fn prune_vulkan_texture_cache(&mut self) {
        while self.vulkan_texture_cache.len() > MAX_CACHED_VULKAN_TEXTURES {
            let Some(index) = self
                .vulkan_texture_cache
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
            else {
                return;
            };
            self.vulkan_texture_cache.swap_remove(index);
        }
    }

    pub(super) unsafe fn ensure_target_texture(
        &mut self,
        size: RenderSize,
    ) -> Result<OutputTextureFormat> {
        if let Some(format) = self.target_format
            && unsafe { self.recreate_target_texture(size, format)? }
        {
            return Ok(format);
        }

        for format in [OutputTextureFormat::Bgra, OutputTextureFormat::Rgba] {
            if unsafe { self.recreate_target_texture(size, format)? } {
                self.target_format = Some(format);
                return Ok(format);
            }
        }

        Err(anyhow!("libplacebo 找不到可读回的视频输出格式"))
    }

    pub(super) unsafe fn recreate_target_texture(
        &mut self,
        size: RenderSize,
        output_format: OutputTextureFormat,
    ) -> Result<bool> {
        let Some(format) = (unsafe { self.find_named_format(output_format.name())? }) else {
            return Ok(false);
        };

        let mut params = unsafe { mem::zeroed::<ffi::pl_tex_params>() };
        params.w = i32::try_from(size.width).map_err(|_| anyhow!("video frame is too wide"))?;
        params.h = i32::try_from(size.height).map_err(|_| anyhow!("video frame is too tall"))?;
        params.format = format;
        params.renderable = true;
        params.host_readable = true;

        Ok(unsafe { ffi::pl_tex_recreate(self.gpu, &mut self.target_texture, &params) })
    }

    pub(super) unsafe fn find_named_format(&self, name: &str) -> Result<Option<ffi::pl_fmt>> {
        let name = CString::new(name)?;
        let format = unsafe { ffi::pl_find_named_fmt(self.gpu, name.as_ptr()) };
        if format.is_null() {
            Ok(None)
        } else {
            Ok(Some(format))
        }
    }

    pub(super) unsafe fn target_frame(&self, size: RenderSize) -> ffi::pl_frame {
        let mut frame = unsafe { mem::zeroed::<ffi::pl_frame>() };
        frame.num_planes = 1;
        frame.planes[0].texture = self.target_texture;
        frame.planes[0].components = 4;
        frame.planes[0].component_mapping = [0, 1, 2, 3];
        frame.repr = unsafe { target_color_repr() };
        frame.color = unsafe { target_color_space() };
        frame.crop = rect_for_size(size);
        frame
    }
}

impl UploadedSourceFrame {
    pub(super) unsafe fn release_vulkan_images_for_render(
        &mut self,
        input: &VulkanVideoFrame,
    ) -> Result<AvVulkanFrameAccess> {
        let access = unsafe { AvVulkanFrameAccess::lock(input)? };
        for plane in &mut self.wrapped_vulkan_planes {
            let plane_index = plane.plane_index;
            let release = ffi::pl_vulkan_release_params {
                tex: plane.texture,
                layout: unsafe { access.layout(plane_index)? },
                qf: VK_QUEUE_FAMILY_IGNORED,
                semaphore: unsafe { access.semaphore(plane_index)? },
            };
            unsafe { ffi::pl_vulkan_release_ex(self.gpu, &release) };
            plane.held_by_user = false;
        }
        Ok(access)
    }

    pub(super) unsafe fn hold_vulkan_images_after_render(
        &mut self,
        gpu: ffi::pl_gpu,
        access: &mut AvVulkanFrameAccess,
    ) -> Result<()> {
        let mut hold_error = None;
        for plane in &mut self.wrapped_vulkan_planes {
            let plane_index = plane.plane_index;
            let next_semaphore_value = unsafe { access.next_semaphore_value(plane_index)? };
            let mut out_layout = 0;
            let hold = ffi::pl_vulkan_hold_params {
                tex: plane.texture,
                layout: 0,
                out_layout: &mut out_layout,
                qf: VK_QUEUE_FAMILY_IGNORED,
                semaphore: ffi::pl_vulkan_sem {
                    sem: unsafe { access.semaphore_handle(plane_index)? },
                    value: next_semaphore_value,
                },
            };
            if unsafe { ffi::pl_vulkan_hold_ex(gpu, &hold) } {
                unsafe {
                    access.update_plane_after_hold(
                        plane_index,
                        out_layout,
                        next_semaphore_value,
                    )?;
                }
                plane.held_by_user = true;
            } else {
                hold_error
                    .get_or_insert_with(|| anyhow!("libplacebo 释放 Vulkan 视频帧控制权失败"));
            }
        }
        unsafe { access.unlock() };
        match hold_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Drop for UploadedSourceFrame {
    fn drop(&mut self) {
        unsafe {
            for plane in &mut self.wrapped_vulkan_planes {
                if !plane.cached
                    && !plane.texture.is_null()
                    && !self.gpu.is_null()
                    && plane.held_by_user
                {
                    let mut texture = plane.texture;
                    ffi::pl_tex_destroy(self.gpu, &mut texture);
                    plane.texture = ptr::null();
                }
            }
        }
    }
}

unsafe fn vulkan_hw_frames_ref(input: &VulkanVideoFrame) -> Result<FfmpegAvBufferRef> {
    let frame = input.frame.as_ptr() as *mut ffmpeg_ffi::AVFrame;
    if frame.is_null() {
        return Err(anyhow!("Vulkan video frame reference is null"));
    }
    let hw_frames_ctx = unsafe { (*frame).hw_frames_ctx };
    FfmpegAvBufferRef::new_ref(hw_frames_ctx).context("Vulkan video frame is missing hw_frames_ctx")
}

impl AvVulkanFrameAccess {
    unsafe fn lock(input: &VulkanVideoFrame) -> Result<Self> {
        let frame = input.frame.as_ptr() as *mut ffmpeg_ffi::AVFrame;
        if frame.is_null() {
            return Err(anyhow!("Vulkan video frame reference is null"));
        }
        let hw_frames_ref = unsafe { (*frame).hw_frames_ctx };
        if hw_frames_ref.is_null() {
            return Err(anyhow!("Vulkan video frame is missing hw_frames_ctx"));
        }
        let frames = unsafe { (*hw_frames_ref).data as *mut ffmpeg_vulkan::AVHWFramesContext };
        if frames.is_null() {
            return Err(anyhow!("Vulkan video frame is missing AVHWFramesContext"));
        }
        let vulkan_frames = unsafe { (*frames).hwctx as *mut ffmpeg_vulkan::AVVulkanFramesContext };
        if vulkan_frames.is_null() {
            return Err(anyhow!(
                "Vulkan video frame is missing AVVulkanFramesContext"
            ));
        }
        let vulkan_frame = unsafe { (*frame).data[0] as *mut ffmpeg_vulkan::AVVkFrame };
        if vulkan_frame.is_null() {
            return Err(anyhow!("Vulkan video frame is missing AVVkFrame"));
        }

        let mut access = Self {
            frames,
            vulkan_frames,
            vulkan_frame,
            locked: false,
        };
        if let Some(lock_frame) = unsafe { (*vulkan_frames).lock_frame } {
            unsafe { lock_frame(frames, vulkan_frame) };
            access.locked = true;
        }
        Ok(access)
    }

    unsafe fn layout(&self, plane_index: usize) -> Result<ffi::VkImageLayout> {
        if plane_index >= unsafe { (*self.vulkan_frame).layout.len() } {
            return Err(anyhow!("invalid Vulkan video plane index"));
        }
        Ok(unsafe { (*self.vulkan_frame).layout[plane_index] as ffi::VkImageLayout })
    }

    unsafe fn semaphore(&self, plane_index: usize) -> Result<ffi::pl_vulkan_sem> {
        Ok(ffi::pl_vulkan_sem {
            sem: unsafe { self.semaphore_handle(plane_index)? },
            value: unsafe { self.semaphore_value(plane_index)? },
        })
    }

    unsafe fn semaphore_handle(&self, plane_index: usize) -> Result<ffi::VkSemaphore> {
        if plane_index >= unsafe { (*self.vulkan_frame).sem.len() } {
            return Err(anyhow!("invalid Vulkan video plane index"));
        }
        let semaphore = unsafe { (*self.vulkan_frame).sem[plane_index] as ffi::VkSemaphore };
        if semaphore.is_null() {
            return Err(anyhow!("Vulkan video plane has a null semaphore"));
        }
        Ok(semaphore)
    }

    unsafe fn semaphore_value(&self, plane_index: usize) -> Result<u64> {
        if plane_index >= unsafe { (*self.vulkan_frame).sem_value.len() } {
            return Err(anyhow!("invalid Vulkan video plane index"));
        }
        Ok(unsafe { (*self.vulkan_frame).sem_value[plane_index] })
    }

    unsafe fn next_semaphore_value(&self, plane_index: usize) -> Result<u64> {
        Ok(unsafe { self.semaphore_value(plane_index)? }.saturating_add(1))
    }

    unsafe fn update_plane_after_hold(
        &mut self,
        plane_index: usize,
        layout: ffi::VkImageLayout,
        semaphore_value: u64,
    ) -> Result<()> {
        if plane_index >= unsafe { (*self.vulkan_frame).layout.len() } {
            return Err(anyhow!("invalid Vulkan video plane index"));
        }
        unsafe {
            (*self.vulkan_frame).layout[plane_index] = layout as ffmpeg_vulkan::VkImageLayout;
            (*self.vulkan_frame).access[plane_index] = 0 as ffmpeg_vulkan::VkAccessFlagBits;
            (*self.vulkan_frame).sem_value[plane_index] = semaphore_value;
        }
        Ok(())
    }

    unsafe fn unlock(&mut self) {
        if !self.locked {
            return;
        }
        if let Some(unlock_frame) = unsafe { (*self.vulkan_frames).unlock_frame } {
            unsafe { unlock_frame(self.frames, self.vulkan_frame) };
        }
        self.locked = false;
    }
}

impl Drop for AvVulkanFrameAccess {
    fn drop(&mut self) {
        unsafe { self.unlock() };
    }
}
