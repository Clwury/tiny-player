use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputTextureFormat {
    Bgra,
    Rgba,
}

impl OutputTextureFormat {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Bgra => "bgra8",
            Self::Rgba => "rgba8",
        }
    }
}

pub(super) unsafe fn source_color_repr(
    format: RawVideoFormat,
    color: FrameColor,
    range: RawVideoRange,
) -> ffi::pl_color_repr {
    let mut repr = unsafe { mem::zeroed::<ffi::pl_color_repr>() };
    repr.sys = match color {
        FrameColor::Sdr => ffi::pl_color_system_PL_COLOR_SYSTEM_BT_709,
        FrameColor::Hdr10Bt2020 => ffi::pl_color_system_PL_COLOR_SYSTEM_BT_2020_NC,
        FrameColor::DolbyVisionProfile5 => ffi::pl_color_system_PL_COLOR_SYSTEM_DOLBYVISION,
    };
    repr.levels = source_color_levels(range);
    repr.alpha = ffi::pl_alpha_mode_PL_ALPHA_NONE;
    repr.bits = ffi::pl_bit_encoding {
        sample_depth: format.sample_depth(),
        color_depth: format.color_depth(),
        bit_shift: format.bit_shift(),
    };
    repr
}

pub(super) unsafe fn source_color_space(color: FrameColor) -> ffi::pl_color_space {
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

pub(super) fn source_color_levels(range: RawVideoRange) -> ffi::pl_color_levels {
    match range {
        RawVideoRange::Unknown => ffi::pl_color_levels_PL_COLOR_LEVELS_UNKNOWN,
        RawVideoRange::Limited => ffi::pl_color_levels_PL_COLOR_LEVELS_LIMITED,
        RawVideoRange::Full => ffi::pl_color_levels_PL_COLOR_LEVELS_FULL,
    }
}

pub(super) unsafe fn apply_chroma_location(
    frame: &mut ffi::pl_frame,
    format: RawVideoFormat,
    chroma_site: RawVideoChromaSite,
) {
    if matches!(
        format,
        RawVideoFormat::P010Le
            | RawVideoFormat::I42010Le
            | RawVideoFormat::Nv12
            | RawVideoFormat::I420
    ) {
        unsafe { ffi::pl_frame_set_chroma_location(frame, chroma_location(chroma_site)) };
    }
}

pub(super) fn chroma_location(chroma_site: RawVideoChromaSite) -> ffi::pl_chroma_location {
    match chroma_site {
        RawVideoChromaSite::Unknown => ffi::pl_chroma_location_PL_CHROMA_UNKNOWN,
        RawVideoChromaSite::Left => ffi::pl_chroma_location_PL_CHROMA_LEFT,
        RawVideoChromaSite::Center => ffi::pl_chroma_location_PL_CHROMA_CENTER,
        RawVideoChromaSite::TopLeft => ffi::pl_chroma_location_PL_CHROMA_TOP_LEFT,
        RawVideoChromaSite::TopCenter => ffi::pl_chroma_location_PL_CHROMA_TOP_CENTER,
    }
}

pub(super) unsafe fn target_color_repr() -> ffi::pl_color_repr {
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

pub(super) unsafe fn target_color_space() -> ffi::pl_color_space {
    let mut space = unsafe { mem::zeroed::<ffi::pl_color_space>() };
    space.primaries = ffi::pl_color_primaries_PL_COLOR_PRIM_BT_709;
    space.transfer = ffi::pl_color_transfer_PL_COLOR_TRC_SRGB;
    space
}

pub(super) fn rect_for_size(size: RenderSize) -> ffi::pl_rect2df {
    ffi::pl_rect2df {
        x0: 0.0,
        y0: 0.0,
        x1: size.width as f32,
        y1: size.height as f32,
    }
}

pub(super) fn swap_red_blue_channels(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}
