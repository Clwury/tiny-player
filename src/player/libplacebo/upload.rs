use super::*;

pub(super) struct UploadedSourceFrame {
    pub(super) frame: ffi::pl_frame,
    _dovi_metadata: Option<Box<ffi::pl_dovi_metadata>>,
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
            apply_dovi_hdr_metadata(&mut frame.color, &prepared);
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
