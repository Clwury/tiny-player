use std::{mem, os::raw::c_int, ptr, sync::Arc};

use ffmpeg_sys_next as ffmpeg_ffi;

// ffmpeg-sys-next does not expose dovi_meta.h, and libplacebo's FFmpeg mapper
// is static inline. Keep the small ABI shim in one place.
#[derive(Clone, Debug)]
pub(crate) struct FfmpegDoviMetadata {
    bytes: Arc<[u8]>,
    profile5: bool,
}

impl FfmpegDoviMetadata {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let header = read_unaligned_at::<AvDoviMetadataHeader>(bytes, 0)?;
        let rpu_header = read_unaligned_at::<AvDoviRpuDataHeader>(bytes, header.header_offset)?;

        Some(Self {
            bytes: bytes.into(),
            profile5: rpu_header.vdr_rpu_profile == 0 && rpu_header.disable_residual_flag != 0,
        })
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn is_profile5(&self) -> bool {
        self.profile5
    }

    pub(crate) fn rpu_header(&self) -> Option<AvDoviRpuDataHeader> {
        let header = read_unaligned_at::<AvDoviMetadataHeader>(self.as_bytes(), 0)?;
        read_unaligned_at::<AvDoviRpuDataHeader>(self.as_bytes(), header.header_offset)
    }

    pub(crate) fn data_mapping(&self) -> Option<AvDoviDataMapping> {
        let header = read_unaligned_at::<AvDoviMetadataHeader>(self.as_bytes(), 0)?;
        read_unaligned_at::<AvDoviDataMapping>(self.as_bytes(), header.mapping_offset)
    }

    pub(crate) fn color_metadata(&self) -> Option<AvDoviColorMetadata> {
        let header = read_unaligned_at::<AvDoviMetadataHeader>(self.as_bytes(), 0)?;
        read_unaligned_at::<AvDoviColorMetadata>(self.as_bytes(), header.color_offset)
    }

    pub(crate) fn source_luminance_pq(&self) -> Option<(u16, u16)> {
        let color = self.color_metadata()?;
        Some((color.source_min_pq, color.source_max_pq))
    }
}

impl PartialEq for FfmpegDoviMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.profile5 == other.profile5 && self.as_bytes() == other.as_bytes()
    }
}

impl Eq for FfmpegDoviMetadata {}

#[repr(C)]
#[derive(Clone, Copy)]
struct AvDoviMetadataHeader {
    header_offset: usize,
    mapping_offset: usize,
    color_offset: usize,
    ext_block_offset: usize,
    ext_block_size: usize,
    num_ext_blocks: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AvDoviRpuDataHeader {
    pub(crate) rpu_type: u8,
    pub(crate) rpu_format: u16,
    pub(crate) vdr_rpu_profile: u8,
    pub(crate) vdr_rpu_level: u8,
    pub(crate) chroma_resampling_explicit_filter_flag: u8,
    pub(crate) coef_data_type: u8,
    pub(crate) coef_log2_denom: u8,
    pub(crate) vdr_rpu_normalized_idc: u8,
    pub(crate) bl_video_full_range_flag: u8,
    pub(crate) bl_bit_depth: u8,
    pub(crate) el_bit_depth: u8,
    pub(crate) vdr_bit_depth: u8,
    pub(crate) spatial_resampling_filter_flag: u8,
    pub(crate) el_spatial_resampling_filter_flag: u8,
    pub(crate) disable_residual_flag: u8,
    pub(crate) ext_mapping_idc_0_4: u8,
    pub(crate) ext_mapping_idc_5_7: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AvDoviReshapingCurve {
    pub(crate) num_pivots: u8,
    pub(crate) pivots: [u16; 9],
    pub(crate) mapping_idc: [c_int; 8],
    pub(crate) poly_order: [u8; 8],
    pub(crate) poly_coef: [[i64; 3]; 8],
    pub(crate) mmr_order: [u8; 8],
    pub(crate) mmr_constant: [i64; 8],
    pub(crate) mmr_coef: [[[i64; 7]; 3]; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AvDoviNlqParams {
    pub(crate) nlq_offset: u16,
    pub(crate) vdr_in_max: u64,
    pub(crate) linear_deadzone_slope: u64,
    pub(crate) linear_deadzone_threshold: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AvDoviDataMapping {
    pub(crate) vdr_rpu_id: u8,
    pub(crate) mapping_color_space: u8,
    pub(crate) mapping_chroma_format_idc: u8,
    pub(crate) curves: [AvDoviReshapingCurve; 3],
    pub(crate) nlq_method_idc: c_int,
    pub(crate) num_x_partitions: u32,
    pub(crate) num_y_partitions: u32,
    pub(crate) nlq: [AvDoviNlqParams; 3],
    pub(crate) nlq_pivots: [u16; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AvDoviColorMetadata {
    pub(crate) dm_metadata_id: u8,
    pub(crate) scene_refresh_flag: u8,
    pub(crate) ycc_to_rgb_matrix: [ffmpeg_ffi::AVRational; 9],
    pub(crate) ycc_to_rgb_offset: [ffmpeg_ffi::AVRational; 3],
    pub(crate) rgb_to_lms_matrix: [ffmpeg_ffi::AVRational; 9],
    pub(crate) signal_eotf: u16,
    pub(crate) signal_eotf_param0: u16,
    pub(crate) signal_eotf_param1: u16,
    pub(crate) signal_eotf_param2: u32,
    pub(crate) signal_bit_depth: u8,
    pub(crate) signal_color_space: u8,
    pub(crate) signal_chroma_format: u8,
    pub(crate) signal_full_range_flag: u8,
    pub(crate) source_min_pq: u16,
    pub(crate) source_max_pq: u16,
    pub(crate) source_diagonal: u16,
}

pub(crate) fn av_q2d(value: ffmpeg_ffi::AVRational) -> f32 {
    if value.den == 0 {
        0.0
    } else {
        value.num as f32 / value.den as f32
    }
}

fn read_unaligned_at<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(mem::size_of::<T>())?;
    if end > bytes.len() {
        return None;
    }
    Some(unsafe { ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}
