use super::*;
use crate::render_host::{DmaBufVideoFrame, FfmpegFrameRef};

pub(super) struct DmaBufSourceFrame {
    pub(super) frame: ffi::pl_frame,
    _dovi_metadata: Option<Box<ffi::pl_dovi_metadata>>,
    _textures: DmaBufTextures,
}

struct DmaBufTextures {
    gpu: ffi::pl_gpu,
    textures: Vec<ffi::pl_tex>,
    // Exported FDs and the VA surface must outlive all GPU references.
    _frame: FfmpegFrameRef,
}

impl Drop for DmaBufTextures {
    fn drop(&mut self) {
        unsafe {
            for texture in &mut self.textures {
                ffi::pl_tex_destroy(self.gpu, texture);
            }
            ffi::pl_gpu_finish(self.gpu);
        }
    }
}

impl LibplaceboToneMapper {
    pub(super) unsafe fn import_dmabuf_source_frame(
        &mut self,
        input: &DmaBufVideoFrame,
        size: RenderSize,
    ) -> Result<DmaBufSourceFrame> {
        // Bindgen uses a signed C enum on MSVC and an unsigned one on Linux.
        if unsafe { (*self.gpu).import_caps.tex } & ffi::pl_handle_type_PL_HANDLE_DMA_BUF as u64
            == 0
        {
            return Err(anyhow!("Vulkan 渲染设备不支持 DMA-BUF 纹理导入"));
        }
        let avframe = unsafe { &*input.frame.as_ptr() };
        if avframe.format != ffmpeg_ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32
            || avframe.data[0].is_null()
            || avframe.width <= 0
            || avframe.height <= 0
            || size.width == 0
            || size.height == 0
            || size.width > avframe.width as u32
            || size.height > avframe.height as u32
        {
            return Err(anyhow!("VA-API DMA-BUF 帧格式或尺寸无效"));
        }
        let descriptor = unsafe { &*avframe.data[0].cast::<ffmpeg_vulkan::AVDRMFrameDescriptor>() };
        let plane_count = input.format.plane_count();
        validate_descriptor(descriptor, plane_count)?;
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
        frame.num_planes = plane_count as i32;
        frame.repr = unsafe { source_color_repr(input.format, input.color, input.range) };
        frame.color = unsafe { source_color_space(input.color) };
        frame.crop = rect_for_size(size);
        let dovi_metadata = prepared_dovi.map(|prepared| {
            apply_dovi_source_luminance_metadata(&mut frame.color, &prepared);
            Box::new(prepared.placebo)
        });
        if let Some(metadata) = dovi_metadata.as_ref() {
            frame.repr.dovi = metadata.as_ref();
        }
        let mut textures = DmaBufTextures {
            gpu: self.gpu,
            textures: Vec::with_capacity(plane_count),
            _frame: input.frame.clone(),
        };
        let allocation = RenderSize {
            width: avframe.width as u32,
            height: avframe.height as u32,
        };
        for index in 0..plane_count {
            let layer = &descriptor.layers[index];
            let plane = &layer.planes[0];
            let object = &descriptor.objects[plane.object_index as usize];
            // VA drivers also export RG88/RG32 aliases while libplacebo's
            // canonical fourcc lookup uses GR88/GR32. Select the plane's actual
            // component layout, as mpv does, and validate both spellings.
            let name = plane_format_name(input.format, index, layer.format)?;
            let format = unsafe { ffi::pl_find_named_fmt(self.gpu, name.as_ptr()) };
            if format.is_null()
                || !unsafe { ffi::pl_fmt_has_modifier(format, object.format_modifier) }
            {
                return Err(anyhow!(
                    "Vulkan 不支持 DMA-BUF 平面 {index}：fourcc={:#x}, modifier={:#x}",
                    layer.format,
                    object.format_modifier,
                ));
            }
            let layout = input
                .format
                .plane_layout_for_color(allocation, index, input.color)?;
            let mut params = unsafe { mem::zeroed::<ffi::pl_tex_params>() };
            params.w = i32::try_from(layout.width)?;
            params.h = i32::try_from(layout.height)?;
            params.format = format;
            params.sampleable = true;
            params.import_handle = ffi::pl_handle_type_PL_HANDLE_DMA_BUF;
            params.shared_mem.handle.fd = object.fd;
            params.shared_mem.size = dma_buf_size(object.fd, object.size)?;
            params.shared_mem.offset = usize::try_from(plane.offset)?;
            params.shared_mem.stride_w = usize::try_from(plane.pitch)?;
            params.shared_mem.drm_format_mod = object.format_modifier;
            if params.shared_mem.offset >= params.shared_mem.size {
                return Err(anyhow!("DMA-BUF 平面偏移超出导出对象大小"));
            }
            let texture = unsafe { ffi::pl_tex_create(self.gpu, &params) };
            if texture.is_null() {
                return Err(anyhow!(
                    "libplacebo 导入 DMA-BUF 平面 {index} 失败：fourcc={:#x}, modifier={:#x}",
                    layer.format,
                    object.format_modifier,
                ));
            }
            textures.textures.push(texture);
            let output = &mut frame.planes[index];
            output.texture = texture;
            output.components = layout.components as i32;
            output.component_mapping = layout.component_map;
        }
        unsafe { apply_chroma_location(&mut frame, input.format, input.chroma_site) };
        Ok(DmaBufSourceFrame {
            frame,
            _dovi_metadata: dovi_metadata,
            _textures: textures,
        })
    }
}

fn plane_format_name(
    format: RawVideoFormat,
    index: usize,
    fourcc: u32,
) -> Result<&'static std::ffi::CStr> {
    let (name, accepted): (_, &[[u8; 4]]) = match (format, index) {
        (RawVideoFormat::Nv12, 0) => (c"r8", &[*b"R8  "]),
        (RawVideoFormat::Nv12, 1) => (c"rg8", &[*b"GR88", *b"RG88"]),
        (RawVideoFormat::P010Le, 0) => (c"r16", &[*b"R16 "]),
        (RawVideoFormat::P010Le, 1) => (c"rg16", &[*b"GR32", *b"RG32"]),
        _ => return Err(anyhow!("不支持的 VA-API DMA-BUF 平面格式")),
    };
    if !accepted
        .iter()
        .any(|bytes| u32::from_le_bytes(*bytes) == fourcc)
    {
        return Err(anyhow!(
            "VA-API DMA-BUF 平面 {index} 的 fourcc={fourcc:#x} 与 {format:?} 不匹配"
        ));
    }
    Ok(name)
}

fn validate_descriptor(
    descriptor: &ffmpeg_vulkan::AVDRMFrameDescriptor,
    plane_count: usize,
) -> Result<()> {
    if descriptor.nb_objects <= 0
        || descriptor.nb_objects as usize > descriptor.objects.len()
        || descriptor.nb_layers as usize != plane_count
        || plane_count > descriptor.layers.len()
    {
        return Err(anyhow!("VA-API DMA-BUF 对象数或平面数无效"));
    }
    for layer in &descriptor.layers[..plane_count] {
        let plane = &layer.planes[0];
        if layer.nb_planes != 1
            || plane.object_index < 0
            || plane.object_index >= descriptor.nb_objects
            || plane.offset < 0
            || plane.pitch <= 0
            || descriptor.objects[plane.object_index as usize].fd < 0
        {
            return Err(anyhow!("VA-API DMA-BUF 平面布局无效"));
        }
    }
    Ok(())
}

fn dma_buf_size(fd: i32, reported_size: usize) -> Result<usize> {
    if reported_size > 0 {
        return Ok(reported_size);
    }
    // Some VA drivers leave object.size unset. DMA-BUF supports SEEK_END for
    // querying its size; use a duplicate so the exported FD stays owned by AVFrame.
    #[cfg(unix)]
    {
        use std::{
            io::{Seek, SeekFrom},
            os::fd::BorrowedFd,
        };
        let duplicate = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        let mut file = std::fs::File::from(duplicate);
        let size = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        if size == 0 {
            return Err(anyhow!("DMA-BUF 导出对象大小为零"));
        }
        Ok(usize::try_from(size)?)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(anyhow!("当前平台不支持 DMA-BUF 大小查询"))
    }
}

#[cfg(test)]
mod tests;
