use super::*;

#[derive(Default)]
pub(super) struct DoviMetadataCache {
    mapping: Option<RpuDataMapping>,
    color: Option<VdrDmData>,
    rendered: Option<DoviRenderMetadata>,
    metadata_logged: bool,
}

#[derive(Clone)]
pub(super) struct DoviRenderMetadata {
    pub(super) placebo: ffi::pl_dovi_metadata,
    pub(super) rpu_payload: Vec<u8>,
    pub(super) source_min_pq: u16,
    pub(super) source_max_pq: u16,
}

struct ResolvedDoviRpu {
    rpu: DoviRpu,
    mapping: RpuDataMapping,
    color: VdrDmData,
}

impl DoviMetadataCache {
    pub(super) fn prepare_raw_video(
        &mut self,
        input: &RawVideoFrame,
    ) -> Result<Option<DoviRenderMetadata>> {
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

        if let Some(rendered) = self
            .rendered
            .as_ref()
            .filter(|rendered| rendered.rpu_payload == metadata.rpu_payload)
        {
            return Ok(Some(rendered.clone()));
        }

        let resolved = self.resolve(metadata)?;
        self.trace_metadata(&resolved, input.range);
        let rendered = DoviRenderMetadata {
            placebo: map_dovi_metadata(&resolved.rpu, &resolved.mapping, &resolved.color)?,
            rpu_payload: metadata.rpu_payload.clone(),
            source_min_pq: resolved.color.source_min_pq,
            source_max_pq: resolved.color.source_max_pq,
        };
        self.rendered = Some(rendered.clone());
        Ok(Some(rendered))
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

    pub(super) fn resolve_color(
        &mut self,
        profile: u8,
        color: Option<VdrDmData>,
    ) -> Result<VdrDmData> {
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

        tracing::debug!("using default Dolby Vision Profile 5 color metadata");
        let color = profile5_default_dovi_color();
        self.color = Some(color.clone());
        color
    }

    fn trace_metadata(&mut self, resolved: &ResolvedDoviRpu, range: RawVideoRange) {
        if self.metadata_logged {
            return;
        }
        self.metadata_logged = true;
        tracing::debug!(
            profile = resolved.rpu.dovi_profile,
            vdr_profile = resolved.rpu.header.vdr_rpu_profile,
            bl_full_range = resolved.rpu.header.bl_video_full_range_flag,
            signal_full_range = resolved.color.signal_full_range_flag,
            raw_range = ?range,
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

pub(super) fn profile5_default_dovi_color() -> VdrDmData {
    Profile5::dm_data()
}

pub(super) fn is_dovi_tool_profile5_default_color(color: &VdrDmData) -> bool {
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

pub(super) fn map_dovi_metadata(
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
        let mut pivot = 0u32;
        for (target, source) in out.pivots.iter_mut().zip(curve.pivots.iter()) {
            pivot = pivot.saturating_add(u32::from(*source));
            *target = pivot as f32 * pivot_scale;
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

pub(super) fn apply_dovi_source_luminance_metadata(
    color: &mut ffi::pl_color_space,
    metadata: &DoviRenderMetadata,
) {
    // Do not call pl_hdr_metadata_from_dovi_rpu here: libplacebo routes it
    // through dovi_tool's C API, whose Rust internals can abort across FFI on
    // some Profile 5 RPU variants. The render metadata has already resolved
    // source luminance in safe Rust code.
    if metadata.source_max_pq != 0 {
        color.hdr.min_luma = pq_code_to_nits(metadata.source_min_pq);
        color.hdr.max_luma = pq_code_to_nits(metadata.source_max_pq);
    }
}

pub(super) fn dovi_offset(value: u32) -> f32 {
    value as f32 / 268_435_456.0
}

pub(super) fn matrix_from_dovi_coeffs(
    values: [i16; 9],
    denominator_log2: i32,
) -> ffi::pl_matrix3x3 {
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

pub(super) fn dovi_matrix_coefficient(value: i16, denominator_log2: i32) -> f32 {
    f32::from(value) / 2.0_f32.powi(denominator_log2)
}

pub(super) fn dovi_coefficient(
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

pub(super) fn pow2_scale(exponent: u64) -> f32 {
    2.0_f32.powi(-(exponent as i32))
}

pub(super) fn pq_code_to_nits(value: u16) -> f32 {
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
