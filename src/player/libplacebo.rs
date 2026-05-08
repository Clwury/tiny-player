use std::{ffi::CString, mem, os::raw::c_void, ptr};

use anyhow::{Context, Result, anyhow};
use dolby_vision::rpu::{
    dovi_rpu::DoviRpu,
    rpu_data_mapping::{DoviMappingMethod, RpuDataMapping},
    vdr_dm_data::VdrDmData,
};

use super::{
    dovi::DoviFrameMetadata,
    render_host::{FrameColor, RawVideoFormat, RawVideoFrame, RenderSize, frame_byte_len},
};

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/libplacebo_bindings.rs"));
}

pub struct LibplaceboToneMapper {
    vulkan: ffi::pl_vulkan,
    gpu: ffi::pl_gpu,
    renderer: ffi::pl_renderer,
    source_textures: [ffi::pl_tex; 4],
    target_texture: ffi::pl_tex,
    target_format: Option<OutputTextureFormat>,
    dovi_cache: DoviMetadataCache,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputTextureFormat {
    Bgra,
    Rgba,
}

impl OutputTextureFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Bgra => "bgra8",
            Self::Rgba => "rgba8",
        }
    }
}

struct UploadedSourceFrame {
    frame: ffi::pl_frame,
    _dovi_metadata: Option<Box<ffi::pl_dovi_metadata>>,
}

#[derive(Default)]
struct DoviMetadataCache {
    mapping: Option<RpuDataMapping>,
    color: Option<VdrDmData>,
    metadata_logged: bool,
}

struct DoviRenderMetadata {
    placebo: ffi::pl_dovi_metadata,
    rpu_payload: Vec<u8>,
    source_min_pq: u16,
    source_max_pq: u16,
    levels: DoviSourceLevels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DoviSourceLevels {
    Full,
    Limited,
}

struct ResolvedDoviRpu {
    rpu: DoviRpu,
    mapping: RpuDataMapping,
    color: VdrDmData,
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
            })
        }
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

            let mut pixels = vec![0; frame_byte_len(output_size)?];
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

            if output_format == OutputTextureFormat::Rgba {
                swap_red_blue_channels(&mut pixels);
            }
            Ok(pixels)
        }
    }

    unsafe fn upload_source_frame(
        &mut self,
        input: &RawVideoFrame,
        size: RenderSize,
    ) -> Result<UploadedSourceFrame> {
        if input.planes.len() != input.format.plane_count() {
            return Err(anyhow!("invalid raw video plane count"));
        }

        let prepared_dovi = self.dovi_cache.prepare_raw_video(input)?;
        let dovi_levels = prepared_dovi.as_ref().map(|metadata| metadata.levels);
        let mut frame = unsafe { mem::zeroed::<ffi::pl_frame>() };
        frame.num_planes = input.planes.len() as i32;
        frame.repr = unsafe { source_color_repr(input.format, input.color, dovi_levels) };
        frame.color = unsafe { source_color_space(input.color) };
        let dovi_metadata = prepared_dovi.map(|prepared| {
            apply_dovi_hdr_metadata(&mut frame.color, &prepared);
            Box::new(prepared.placebo)
        });
        if let Some(dovi_metadata) = dovi_metadata.as_ref() {
            frame.repr.dovi = dovi_metadata.as_ref() as *const ffi::pl_dovi_metadata;
        }
        frame.crop = rect_for_size(size);

        for (plane_index, plane) in input.planes.iter().enumerate() {
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
            plane_data.row_stride = plane.stride;
            plane_data.pixels = plane.data.as_ptr().cast::<c_void>();

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
        }

        Ok(UploadedSourceFrame {
            frame,
            _dovi_metadata: dovi_metadata,
        })
    }

    unsafe fn ensure_target_texture(&mut self, size: RenderSize) -> Result<OutputTextureFormat> {
        if let Some(format) = self.target_format
            && unsafe { self.recreate_target_texture(size, format)? }
        {
            return Ok(format);
        }

        for format in [OutputTextureFormat::Rgba, OutputTextureFormat::Bgra] {
            if unsafe { self.recreate_target_texture(size, format)? } {
                self.target_format = Some(format);
                return Ok(format);
            }
        }

        Err(anyhow!("libplacebo 找不到可读回的视频输出格式"))
    }

    unsafe fn recreate_target_texture(
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

    unsafe fn find_named_format(&self, name: &str) -> Result<Option<ffi::pl_fmt>> {
        let name = CString::new(name)?;
        let format = unsafe { ffi::pl_find_named_fmt(self.gpu, name.as_ptr()) };
        if format.is_null() {
            Ok(None)
        } else {
            Ok(Some(format))
        }
    }

    unsafe fn target_frame(&self, size: RenderSize) -> ffi::pl_frame {
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

unsafe fn source_color_repr(
    format: RawVideoFormat,
    color: FrameColor,
    dovi_levels: Option<DoviSourceLevels>,
) -> ffi::pl_color_repr {
    let mut repr = unsafe { mem::zeroed::<ffi::pl_color_repr>() };
    repr.sys = match color {
        FrameColor::Sdr => ffi::pl_color_system_PL_COLOR_SYSTEM_BT_709,
        FrameColor::Hdr10Bt2020 => ffi::pl_color_system_PL_COLOR_SYSTEM_BT_2020_NC,
        FrameColor::DolbyVisionProfile5 => ffi::pl_color_system_PL_COLOR_SYSTEM_DOLBYVISION,
    };
    repr.levels = match color {
        FrameColor::DolbyVisionProfile5 => {
            dovi_color_levels(dovi_levels.unwrap_or(DoviSourceLevels::Full))
        }
        FrameColor::Sdr | FrameColor::Hdr10Bt2020 => ffi::pl_color_levels_PL_COLOR_LEVELS_LIMITED,
    };
    repr.alpha = ffi::pl_alpha_mode_PL_ALPHA_NONE;
    repr.bits = ffi::pl_bit_encoding {
        sample_depth: format.sample_depth(),
        color_depth: format.color_depth(),
        bit_shift: format.bit_shift(),
    };
    repr
}

unsafe fn source_color_space(color: FrameColor) -> ffi::pl_color_space {
    let mut space = unsafe { mem::zeroed::<ffi::pl_color_space>() };
    match color {
        FrameColor::Sdr => {
            space.primaries = ffi::pl_color_primaries_PL_COLOR_PRIM_BT_709;
            space.transfer = ffi::pl_color_transfer_PL_COLOR_TRC_BT_1886;
        }
        FrameColor::Hdr10Bt2020 | FrameColor::DolbyVisionProfile5 => {
            space.primaries = ffi::pl_color_primaries_PL_COLOR_PRIM_BT_2020;
            space.transfer = ffi::pl_color_transfer_PL_COLOR_TRC_PQ;
            space.hdr = unsafe { ffi::pl_hdr_metadata_hdr10 };
        }
    }
    space
}

impl DoviMetadataCache {
    fn prepare_raw_video(&mut self, input: &RawVideoFrame) -> Result<Option<DoviRenderMetadata>> {
        let Some(metadata) = input
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.dolby_vision.as_ref())
        else {
            if input.color == FrameColor::DolbyVisionProfile5 {
                return Err(anyhow!("Dolby Vision Profile 5 缺少 RPU 元数据"));
            }
            return Ok(None);
        };

        let resolved = self.resolve(metadata)?;
        let levels = dovi_source_levels(&resolved.rpu, &resolved.color);
        self.trace_metadata(&resolved, levels);
        Ok(Some(DoviRenderMetadata {
            placebo: map_dovi_metadata(&resolved.rpu, &resolved.mapping, &resolved.color)?,
            rpu_payload: metadata.rpu_payload.clone(),
            source_min_pq: resolved.color.source_min_pq,
            source_max_pq: resolved.color.source_max_pq,
            levels,
        }))
    }

    fn resolve(&mut self, metadata: &DoviFrameMetadata) -> Result<ResolvedDoviRpu> {
        let rpu = metadata.parse_rpu()?;
        let mapping = self.resolve_mapping(rpu.rpu_data_mapping.clone())?;
        let color = self.resolve_color(rpu.dovi_profile, rpu.vdr_dm_data.clone())?;

        Ok(ResolvedDoviRpu {
            rpu,
            mapping,
            color,
        })
    }

    fn resolve_mapping(&mut self, mapping: Option<RpuDataMapping>) -> Result<RpuDataMapping> {
        if let Some(mapping) = mapping {
            self.mapping = Some(mapping.clone());
            return Ok(mapping);
        }

        self.mapping
            .clone()
            .ok_or_else(|| anyhow!("Dolby Vision RPU 缺少可复用的 reshaping metadata"))
    }

    fn resolve_color(&mut self, profile: u8, color: Option<VdrDmData>) -> Result<VdrDmData> {
        match color {
            Some(color) if !color.compressed => {
                self.color = Some(color.clone());
                Ok(color)
            }
            Some(_) | None if profile == 5 => Ok(self.profile5_fallback_color()),
            Some(_) | None => self
                .color
                .clone()
                .ok_or_else(|| anyhow!("Dolby Vision RPU 缺少可复用的 color metadata")),
        }
    }

    fn profile5_fallback_color(&mut self) -> VdrDmData {
        if let Some(color) = self.color.clone() {
            return color;
        }

        tracing::debug!("using FFmpeg default Dolby Vision Profile 5 color metadata");
        let color = ffmpeg_default_dovi_color();
        self.color = Some(color.clone());
        color
    }

    fn trace_metadata(&mut self, resolved: &ResolvedDoviRpu, levels: DoviSourceLevels) {
        if self.metadata_logged {
            return;
        }
        self.metadata_logged = true;
        tracing::debug!(
            profile = resolved.rpu.dovi_profile,
            vdr_profile = resolved.rpu.header.vdr_rpu_profile,
            bl_full_range = resolved.rpu.header.bl_video_full_range_flag,
            signal_full_range = resolved.color.signal_full_range_flag,
            levels = ?levels,
            compressed_color = resolved.color.compressed,
            dovi_tool_profile5_default_color = resolved.rpu.dovi_profile == 5
                && is_dovi_tool_profile5_default_color(&resolved.color),
            coef_type = resolved.rpu.header.coefficient_data_type,
            coef_denom = resolved.rpu.header.coefficient_log2_denom,
            ycc_to_rgb = ?[
                resolved.color.ycc_to_rgb_coef0,
                resolved.color.ycc_to_rgb_coef1,
                resolved.color.ycc_to_rgb_coef2,
                resolved.color.ycc_to_rgb_coef3,
                resolved.color.ycc_to_rgb_coef4,
                resolved.color.ycc_to_rgb_coef5,
                resolved.color.ycc_to_rgb_coef6,
                resolved.color.ycc_to_rgb_coef7,
                resolved.color.ycc_to_rgb_coef8,
            ],
            ycc_offset = ?[
                resolved.color.ycc_to_rgb_offset0,
                resolved.color.ycc_to_rgb_offset1,
                resolved.color.ycc_to_rgb_offset2,
            ],
            rgb_to_lms = ?[
                resolved.color.rgb_to_lms_coef0,
                resolved.color.rgb_to_lms_coef1,
                resolved.color.rgb_to_lms_coef2,
                resolved.color.rgb_to_lms_coef3,
                resolved.color.rgb_to_lms_coef4,
                resolved.color.rgb_to_lms_coef5,
                resolved.color.rgb_to_lms_coef6,
                resolved.color.rgb_to_lms_coef7,
                resolved.color.rgb_to_lms_coef8,
            ],
            source_min_pq = resolved.color.source_min_pq,
            source_max_pq = resolved.color.source_max_pq,
            "using Dolby Vision metadata"
        );
    }
}

fn ffmpeg_default_dovi_color() -> VdrDmData {
    VdrDmData {
        ycc_to_rgb_coef0: 9575,
        ycc_to_rgb_coef1: 0,
        ycc_to_rgb_coef2: 14742,
        ycc_to_rgb_coef3: 9575,
        ycc_to_rgb_coef4: 1754,
        ycc_to_rgb_coef5: 4383,
        ycc_to_rgb_coef6: 9575,
        ycc_to_rgb_coef7: 17372,
        ycc_to_rgb_coef8: 0,
        ycc_to_rgb_offset0: 67_108_864,
        ycc_to_rgb_offset1: 536_870_912,
        ycc_to_rgb_offset2: 536_870_912,
        rgb_to_lms_coef0: 5845,
        rgb_to_lms_coef1: 9702,
        rgb_to_lms_coef2: 837,
        rgb_to_lms_coef3: 2568,
        rgb_to_lms_coef4: 12256,
        rgb_to_lms_coef5: 1561,
        rgb_to_lms_coef6: 0,
        rgb_to_lms_coef7: 679,
        rgb_to_lms_coef8: 15705,
        signal_eotf: 39322,
        signal_eotf_param0: 15867,
        signal_eotf_param1: 228,
        signal_eotf_param2: 1_383_604,
        signal_bit_depth: 14,
        signal_color_space: 0,
        signal_chroma_format: 0,
        signal_full_range_flag: 1,
        source_min_pq: 62,
        source_max_pq: 3696,
        source_diagonal: 42,
        ..Default::default()
    }
}

fn is_dovi_tool_profile5_default_color(color: &VdrDmData) -> bool {
    [
        color.ycc_to_rgb_coef0,
        color.ycc_to_rgb_coef1,
        color.ycc_to_rgb_coef2,
        color.ycc_to_rgb_coef3,
        color.ycc_to_rgb_coef4,
        color.ycc_to_rgb_coef5,
        color.ycc_to_rgb_coef6,
        color.ycc_to_rgb_coef7,
        color.ycc_to_rgb_coef8,
    ] == [8192, 799, 1681, 8192, -933, 1091, 8192, 267, -5545]
        && [
            color.ycc_to_rgb_offset0,
            color.ycc_to_rgb_offset1,
            color.ycc_to_rgb_offset2,
        ] == [0, 134_217_728, 134_217_728]
        && [
            color.rgb_to_lms_coef0,
            color.rgb_to_lms_coef1,
            color.rgb_to_lms_coef2,
            color.rgb_to_lms_coef3,
            color.rgb_to_lms_coef4,
            color.rgb_to_lms_coef5,
            color.rgb_to_lms_coef6,
            color.rgb_to_lms_coef7,
            color.rgb_to_lms_coef8,
        ] == [17081, -349, -349, -349, 17081, -349, -349, -349, 17081]
}

fn dovi_color_levels(levels: DoviSourceLevels) -> ffi::pl_color_levels {
    match levels {
        DoviSourceLevels::Full => ffi::pl_color_levels_PL_COLOR_LEVELS_FULL,
        DoviSourceLevels::Limited => ffi::pl_color_levels_PL_COLOR_LEVELS_LIMITED,
    }
}

fn dovi_source_levels(rpu: &DoviRpu, color: &VdrDmData) -> DoviSourceLevels {
    if rpu.header.bl_video_full_range_flag || color.signal_full_range_flag == 1 {
        DoviSourceLevels::Full
    } else {
        DoviSourceLevels::Limited
    }
}

fn map_dovi_metadata(
    rpu: &DoviRpu,
    mapping: &RpuDataMapping,
    color: &VdrDmData,
) -> Result<ffi::pl_dovi_metadata> {
    let mut dovi = unsafe { mem::zeroed::<ffi::pl_dovi_metadata>() };

    dovi.nonlinear_offset = [
        dovi_offset(color.ycc_to_rgb_offset0),
        dovi_offset(color.ycc_to_rgb_offset1),
        dovi_offset(color.ycc_to_rgb_offset2),
    ];
    dovi.nonlinear = matrix_from_dovi_coeffs(
        [
            color.ycc_to_rgb_coef0,
            color.ycc_to_rgb_coef1,
            color.ycc_to_rgb_coef2,
            color.ycc_to_rgb_coef3,
            color.ycc_to_rgb_coef4,
            color.ycc_to_rgb_coef5,
            color.ycc_to_rgb_coef6,
            color.ycc_to_rgb_coef7,
            color.ycc_to_rgb_coef8,
        ],
        13,
    );
    dovi.linear = matrix_from_dovi_coeffs(
        [
            color.rgb_to_lms_coef0,
            color.rgb_to_lms_coef1,
            color.rgb_to_lms_coef2,
            color.rgb_to_lms_coef3,
            color.rgb_to_lms_coef4,
            color.rgb_to_lms_coef5,
            color.rgb_to_lms_coef6,
            color.rgb_to_lms_coef7,
            color.rgb_to_lms_coef8,
        ],
        14,
    );

    let bl_bit_depth = u32::try_from(rpu.header.bl_bit_depth_minus8 + 8)
        .context("Dolby Vision BL bit depth 无效")?;
    let pivot_scale = 1.0 / (2.0_f32.powi(bl_bit_depth as i32) - 1.0);
    let coefficient_scale = pow2_scale(rpu.header.coefficient_log2_denom);

    for (component, curve) in mapping.curves.iter().enumerate() {
        let out = &mut dovi.comp[component];
        let pivot_count = curve.pivots.len().min(out.pivots.len());
        out.num_pivots = pivot_count as u8;
        for (target, source) in out.pivots.iter_mut().zip(curve.pivots.iter()) {
            *target = f32::from(*source) * pivot_scale;
        }

        let piece_count = pivot_count.saturating_sub(1).min(out.method.len());
        match curve.mapping_idc {
            DoviMappingMethod::Polynomial => {
                if let Some(polynomial) = &curve.polynomial {
                    for piece in 0..piece_count {
                        out.method[piece] = 0;
                        let order = polynomial
                            .poly_order_minus1
                            .get(piece)
                            .map(|order| *order as usize + 1)
                            .unwrap_or(0);
                        for coefficient in 0..out.poly_coeffs[piece].len() {
                            out.poly_coeffs[piece][coefficient] = if coefficient <= order {
                                dovi_coefficient(
                                    rpu.header.coefficient_data_type,
                                    coefficient_scale,
                                    polynomial
                                        .poly_coef_int
                                        .get(piece)
                                        .and_then(|values| values.get(coefficient))
                                        .copied(),
                                    polynomial
                                        .poly_coef
                                        .get(piece)
                                        .and_then(|values| values.get(coefficient))
                                        .copied(),
                                )
                            } else {
                                0.0
                            };
                        }
                    }
                }
            }
            DoviMappingMethod::MMR => {
                if let Some(mmr) = &curve.mmr {
                    for piece in 0..piece_count {
                        out.method[piece] = 1;
                        let order = mmr
                            .mmr_order_minus1
                            .get(piece)
                            .map(|order| *order + 1)
                            .unwrap_or(0);
                        out.mmr_order[piece] = order;
                        out.mmr_constant[piece] = dovi_coefficient(
                            rpu.header.coefficient_data_type,
                            coefficient_scale,
                            mmr.mmr_constant_int.get(piece).copied(),
                            mmr.mmr_constant.get(piece).copied(),
                        );
                        for mmr_order in 0..usize::from(order).min(out.mmr_coeffs[piece].len()) {
                            for coefficient in 0..out.mmr_coeffs[piece][mmr_order].len() {
                                out.mmr_coeffs[piece][mmr_order][coefficient] = dovi_coefficient(
                                    rpu.header.coefficient_data_type,
                                    coefficient_scale,
                                    mmr.mmr_coef_int
                                        .get(piece)
                                        .and_then(|orders| orders.get(mmr_order))
                                        .and_then(|values| values.get(coefficient))
                                        .copied(),
                                    mmr.mmr_coef
                                        .get(piece)
                                        .and_then(|orders| orders.get(mmr_order))
                                        .and_then(|values| values.get(coefficient))
                                        .copied(),
                                );
                            }
                        }
                    }
                }
            }
            DoviMappingMethod::Invalid => {}
        }
    }

    Ok(dovi)
}

fn apply_dovi_hdr_metadata(color: &mut ffi::pl_color_space, metadata: &DoviRenderMetadata) {
    unsafe {
        ffi::pl_hdr_metadata_from_dovi_rpu(
            &mut color.hdr,
            metadata.rpu_payload.as_ptr(),
            metadata.rpu_payload.len(),
        );
    }

    if metadata.source_max_pq != 0 {
        color.hdr.min_luma = pq_code_to_nits(metadata.source_min_pq);
        color.hdr.max_luma = pq_code_to_nits(metadata.source_max_pq);
    }
}

fn dovi_offset(value: u32) -> f32 {
    value as f32 / 268_435_456.0
}

fn matrix_from_dovi_coeffs(values: [i16; 9], denominator_log2: i32) -> ffi::pl_matrix3x3 {
    ffi::pl_matrix3x3 {
        m: [
            [
                dovi_matrix_coefficient(values[0], denominator_log2),
                dovi_matrix_coefficient(values[1], denominator_log2),
                dovi_matrix_coefficient(values[2], denominator_log2),
            ],
            [
                dovi_matrix_coefficient(values[3], denominator_log2),
                dovi_matrix_coefficient(values[4], denominator_log2),
                dovi_matrix_coefficient(values[5], denominator_log2),
            ],
            [
                dovi_matrix_coefficient(values[6], denominator_log2),
                dovi_matrix_coefficient(values[7], denominator_log2),
                dovi_matrix_coefficient(values[8], denominator_log2),
            ],
        ],
    }
}

fn dovi_matrix_coefficient(value: i16, denominator_log2: i32) -> f32 {
    f32::from(value) / 2.0_f32.powi(denominator_log2)
}

fn dovi_coefficient(
    coefficient_data_type: u8,
    scale: f32,
    integer: Option<i64>,
    fraction: Option<u64>,
) -> f32 {
    match coefficient_data_type {
        0 => integer.unwrap_or(0) as f32 + fraction.unwrap_or(0) as f32 * scale,
        1 => f32::from_bits(fraction.unwrap_or(0) as u32),
        _ => 0.0,
    }
}

fn pow2_scale(exponent: u64) -> f32 {
    2.0_f32.powi(-(exponent as i32))
}

fn pq_code_to_nits(value: u16) -> f32 {
    let normalized = f32::from(value) / 4095.0;
    if normalized <= 0.0 {
        return 0.0;
    }

    let m1 = 2610.0 / 16384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let value = normalized.powf(1.0 / m2);
    let numerator = (value - c1).max(0.0);
    let denominator = c2 - c3 * value;
    10_000.0 * (numerator / denominator).powf(1.0 / m1)
}

unsafe fn target_color_repr() -> ffi::pl_color_repr {
    let mut repr = unsafe { mem::zeroed::<ffi::pl_color_repr>() };
    repr.sys = ffi::pl_color_system_PL_COLOR_SYSTEM_RGB;
    repr.levels = ffi::pl_color_levels_PL_COLOR_LEVELS_FULL;
    repr.alpha = ffi::pl_alpha_mode_PL_ALPHA_INDEPENDENT;
    repr.bits = ffi::pl_bit_encoding {
        sample_depth: 8,
        color_depth: 8,
        bit_shift: 0,
    };
    repr
}

unsafe fn target_color_space() -> ffi::pl_color_space {
    let mut space = unsafe { mem::zeroed::<ffi::pl_color_space>() };
    space.primaries = ffi::pl_color_primaries_PL_COLOR_PRIM_BT_709;
    space.transfer = ffi::pl_color_transfer_PL_COLOR_TRC_SRGB;
    space
}

fn rect_for_size(size: RenderSize) -> ffi::pl_rect2df {
    ffi::pl_rect2df {
        x0: 0.0,
        y0: 0.0,
        x1: size.width as f32,
        y1: size.height as f32,
    }
}

fn swap_red_blue_channels(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::render_host::{RawVideoFrame, RawVideoPlane};

    #[test]
    fn swap_red_blue_channels_swaps_red_and_blue_channels() {
        let mut pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];

        swap_red_blue_channels(&mut pixels);

        assert_eq!(pixels, [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn dolby_vision_profile5_uses_metadata_range_input() {
        let full = unsafe {
            source_color_repr(
                RawVideoFormat::P010Le,
                FrameColor::DolbyVisionProfile5,
                Some(DoviSourceLevels::Full),
            )
        };
        let limited = unsafe {
            source_color_repr(
                RawVideoFormat::P010Le,
                FrameColor::DolbyVisionProfile5,
                Some(DoviSourceLevels::Limited),
            )
        };

        assert_eq!(full.levels, ffi::pl_color_levels_PL_COLOR_LEVELS_FULL);
        assert_eq!(limited.levels, ffi::pl_color_levels_PL_COLOR_LEVELS_LIMITED);
        assert_eq!(full.sys, ffi::pl_color_system_PL_COLOR_SYSTEM_DOLBYVISION);
    }

    #[test]
    fn dolby_vision_color_matrices_use_distinct_denominators() {
        assert_eq!(dovi_matrix_coefficient(8192, 13), 1.0);
        assert_eq!(dovi_matrix_coefficient(16384, 14), 1.0);
        assert_eq!(dovi_matrix_coefficient(8192, 14), 0.5);
    }

    #[test]
    fn dolby_vision_float_coefficients_use_ieee_bits() {
        assert_eq!(
            dovi_coefficient(1, 1.0, None, Some(1.25f32.to_bits().into())),
            1.25
        );
        assert_eq!(
            dovi_coefficient(1, 1.0, None, Some((-0.5f32).to_bits().into())),
            -0.5
        );
    }

    #[test]
    fn dolby_vision_fixed_coefficients_match_ffmpeg_fixed_point() {
        assert_eq!(dovi_coefficient(0, 0.25, Some(1), Some(3)), 1.75);
        assert_eq!(dovi_coefficient(0, 0.25, Some(-1), Some(1)), -0.75);
        assert_eq!(dovi_coefficient(0, 0.25, Some(-2), Some(3)), -1.25);
    }

    #[test]
    fn dolby_vision_hdr_metadata_preserves_default_when_source_luminance_is_unknown() {
        let mut color = unsafe { source_color_space(FrameColor::DolbyVisionProfile5) };
        let min_luma = color.hdr.min_luma;
        let max_luma = color.hdr.max_luma;
        let metadata = DoviRenderMetadata {
            placebo: unsafe { mem::zeroed() },
            rpu_payload: Vec::new(),
            source_min_pq: 0,
            source_max_pq: 0,
            levels: DoviSourceLevels::Full,
        };

        apply_dovi_hdr_metadata(&mut color, &metadata);

        assert_eq!(color.hdr.min_luma, min_luma);
        assert_eq!(color.hdr.max_luma, max_luma);
    }

    #[test]
    fn dolby_vision_profile5_requires_rpu_metadata() {
        let frame = RawVideoFrame {
            format: RawVideoFormat::P010Le,
            color: FrameColor::DolbyVisionProfile5,
            metadata: None,
            planes: Vec::new(),
        };

        let mut cache = DoviMetadataCache::default();
        let error = match cache.prepare_raw_video(&frame) {
            Ok(_) => panic!("Dolby Vision Profile 5 without RPU should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("缺少 RPU 元数据"));
    }

    #[test]
    fn dolby_vision_cache_reuses_uncompressed_color_metadata() {
        let mut cache = DoviMetadataCache::default();
        let color = VdrDmData {
            compressed: false,
            ycc_to_rgb_coef0: 8192,
            source_min_pq: 7,
            source_max_pq: 3079,
            ..Default::default()
        };

        let first = cache.resolve_color(8, Some(color)).unwrap();
        let reused = cache
            .resolve_color(
                8,
                Some(VdrDmData {
                    compressed: true,
                    ..Default::default()
                }),
            )
            .unwrap();

        assert_eq!(first.ycc_to_rgb_coef0, 8192);
        assert_eq!(reused.ycc_to_rgb_coef0, 8192);
        assert_eq!(reused.source_min_pq, 7);
        assert_eq!(reused.source_max_pq, 3079);
    }

    #[test]
    fn dolby_vision_profile5_prefers_stream_color_metadata() {
        let mut cache = DoviMetadataCache::default();

        let color = cache
            .resolve_color(
                5,
                Some(VdrDmData {
                    compressed: false,
                    ycc_to_rgb_coef0: 1,
                    source_min_pq: 7,
                    source_max_pq: 3079,
                    ..Default::default()
                }),
            )
            .unwrap();

        assert_eq!(color.ycc_to_rgb_coef0, 1);
        assert_eq!(color.source_min_pq, 7);
        assert_eq!(color.source_max_pq, 3079);
    }

    #[test]
    fn dolby_vision_profile5_detects_dovi_tool_default_color_metadata() {
        let color = VdrDmData {
            compressed: false,
            ycc_to_rgb_coef0: 8192,
            ycc_to_rgb_coef1: 799,
            ycc_to_rgb_coef2: 1681,
            ycc_to_rgb_coef3: 8192,
            ycc_to_rgb_coef4: -933,
            ycc_to_rgb_coef5: 1091,
            ycc_to_rgb_coef6: 8192,
            ycc_to_rgb_coef7: 267,
            ycc_to_rgb_coef8: -5545,
            ycc_to_rgb_offset0: 0,
            ycc_to_rgb_offset1: 134_217_728,
            ycc_to_rgb_offset2: 134_217_728,
            rgb_to_lms_coef0: 17081,
            rgb_to_lms_coef1: -349,
            rgb_to_lms_coef2: -349,
            rgb_to_lms_coef3: -349,
            rgb_to_lms_coef4: 17081,
            rgb_to_lms_coef5: -349,
            rgb_to_lms_coef6: -349,
            rgb_to_lms_coef7: -349,
            rgb_to_lms_coef8: 17081,
            source_min_pq: 7,
            source_max_pq: 3079,
            ..Default::default()
        };

        assert!(is_dovi_tool_profile5_default_color(&color));
    }

    #[test]
    fn dolby_vision_profile5_uses_cached_color_for_compressed_metadata() {
        let mut cache = DoviMetadataCache::default();
        let first = cache
            .resolve_color(
                5,
                Some(VdrDmData {
                    compressed: false,
                    ycc_to_rgb_coef0: 1,
                    source_min_pq: 7,
                    source_max_pq: 3079,
                    ..Default::default()
                }),
            )
            .unwrap();
        let reused = cache
            .resolve_color(
                5,
                Some(VdrDmData {
                    compressed: true,
                    ..Default::default()
                }),
            )
            .unwrap();

        assert_eq!(first.ycc_to_rgb_coef0, 1);
        assert_eq!(reused.ycc_to_rgb_coef0, 1);
        assert_eq!(reused.source_max_pq, 3079);
    }

    #[test]
    fn dolby_vision_profile5_uses_ffmpeg_default_color_for_initial_compressed_metadata() {
        let mut cache = DoviMetadataCache::default();

        let color = cache
            .resolve_color(
                5,
                Some(VdrDmData {
                    compressed: true,
                    ..Default::default()
                }),
            )
            .unwrap();

        assert_eq!(color.ycc_to_rgb_coef0, 9575);
        assert_eq!(color.ycc_to_rgb_coef2, 14742);
        assert_eq!(color.ycc_to_rgb_offset0, 67_108_864);
        assert_eq!(color.ycc_to_rgb_offset1, 536_870_912);
        assert_eq!(color.rgb_to_lms_coef4, 12256);
        assert_eq!(color.source_min_pq, 62);
        assert_eq!(color.source_max_pq, 3696);
    }

    #[test]
    fn dolby_vision_cache_rejects_unknown_compressed_color_without_previous_metadata() {
        let mut cache = DoviMetadataCache::default();

        let error = cache
            .resolve_color(
                8,
                Some(VdrDmData {
                    compressed: true,
                    ..Default::default()
                }),
            )
            .unwrap_err();

        assert!(error.to_string().contains("可复用的 color metadata"));
    }

    #[test]
    fn libplacebo_tone_maps_p010_when_enabled() {
        if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
            return;
        }

        let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
        let y = p010_code(940);
        let uv = p010_code(512);
        let frame = p010_frame(FrameColor::Hdr10Bt2020, [y; 4], [uv, uv]);

        let pixels = tone_mapper
            .tone_map_to_bgra8(
                &frame,
                RenderSize {
                    width: 2,
                    height: 2,
                },
                RenderSize {
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();

        assert_eq!(pixels.len(), 16);
        assert!(pixels.iter().any(|value| *value > 0));
    }

    #[test]
    fn libplacebo_tone_maps_hdr10_p010_red_as_red_when_enabled() {
        if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
            return;
        }

        let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
        let frame = p010_frame(
            FrameColor::Hdr10Bt2020,
            [p010_code(294); 4],
            [p010_code(449), p010_code(736)],
        );

        let pixels = tone_mapper
            .tone_map_to_bgra8(
                &frame,
                RenderSize {
                    width: 2,
                    height: 2,
                },
                RenderSize {
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();

        assert_bgra_red_dominant(&pixels);
    }

    #[test]
    fn libplacebo_tone_maps_hdr10_i42010_red_as_red_when_enabled() {
        if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
            return;
        }

        let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
        let frame = i42010_frame(FrameColor::Hdr10Bt2020, [294; 4], [449], [736]);

        let pixels = tone_mapper
            .tone_map_to_bgra8(
                &frame,
                RenderSize {
                    width: 2,
                    height: 2,
                },
                RenderSize {
                    width: 2,
                    height: 2,
                },
            )
            .unwrap();

        assert_bgra_red_dominant(&pixels);
    }

    fn assert_bgra_red_dominant(pixels: &[u8]) {
        assert!(
            pixels[2] > pixels[0],
            "expected BGRA red > blue, got {:?}",
            &pixels[..4]
        );
    }

    fn p010_frame(color: FrameColor, y: [u16; 4], uv: [u16; 2]) -> RawVideoFrame {
        RawVideoFrame {
            format: RawVideoFormat::P010Le,
            color,
            metadata: None,
            planes: vec![
                RawVideoPlane {
                    data: y.into_iter().flat_map(u16::to_le_bytes).collect(),
                    stride: 4,
                },
                RawVideoPlane {
                    data: uv.into_iter().flat_map(u16::to_le_bytes).collect(),
                    stride: 4,
                },
            ],
        }
    }

    fn i42010_frame(color: FrameColor, y: [u16; 4], u: [u16; 1], v: [u16; 1]) -> RawVideoFrame {
        RawVideoFrame {
            format: RawVideoFormat::I42010Le,
            color,
            metadata: None,
            planes: vec![
                RawVideoPlane {
                    data: y.into_iter().flat_map(u16::to_le_bytes).collect(),
                    stride: 4,
                },
                RawVideoPlane {
                    data: u.into_iter().flat_map(u16::to_le_bytes).collect(),
                    stride: 2,
                },
                RawVideoPlane {
                    data: v.into_iter().flat_map(u16::to_le_bytes).collect(),
                    stride: 2,
                },
            ],
        }
    }

    fn p010_code(value: u16) -> u16 {
        value << 6
    }
}
