use anyhow::{Result, anyhow};

use super::{PooledBytes, RenderSize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawVideoPlanes {
    Owned(Vec<RawVideoPlane>),
}

impl RawVideoPlanes {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Owned(planes) => planes.len(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawVideoPlane {
    pub data: PooledBytes,
    pub stride: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RawVideoFormat {
    P010Le,
    I42010Le,
    Nv12,
    I420,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawVideoPlaneLayout {
    pub width: u32,
    pub height: u32,
    pub row_len: usize,
    pub pixel_stride: usize,
    pub components: usize,
    pub component_map: [i32; 4],
}

impl RawVideoFormat {
    pub fn plane_count(self) -> usize {
        match self {
            Self::P010Le | Self::Nv12 => 2,
            Self::I42010Le | Self::I420 => 3,
        }
    }

    pub fn component_size(self) -> i32 {
        match self {
            Self::P010Le | Self::I42010Le => 16,
            Self::Nv12 | Self::I420 => 8,
        }
    }

    pub fn sample_depth(self) -> i32 {
        self.component_size()
    }

    pub fn color_depth(self) -> i32 {
        match self {
            Self::P010Le | Self::I42010Le => 10,
            Self::Nv12 | Self::I420 => 8,
        }
    }

    pub fn bit_shift(self) -> i32 {
        match self {
            Self::P010Le => 6,
            Self::I42010Le | Self::Nv12 | Self::I420 => 0,
        }
    }

    pub fn plane_layout(self, size: RenderSize, plane: usize) -> Result<RawVideoPlaneLayout> {
        if size.width == 0 || size.height == 0 {
            return Err(anyhow!("invalid video frame dimensions"));
        }

        let chroma_width = size.width.div_ceil(2);
        let chroma_height = size.height.div_ceil(2);
        let (width, height, pixel_stride, components, component_map) = match (self, plane) {
            (Self::P010Le, 0) => (size.width, size.height, 2, 1, [0, -1, -1, -1]),
            (Self::P010Le, 1) => (chroma_width, chroma_height, 4, 2, [1, 2, -1, -1]),
            (Self::I42010Le, 0) => (size.width, size.height, 2, 1, [0, -1, -1, -1]),
            (Self::I42010Le, 1) => (chroma_width, chroma_height, 2, 1, [1, -1, -1, -1]),
            (Self::I42010Le, 2) => (chroma_width, chroma_height, 2, 1, [2, -1, -1, -1]),
            (Self::Nv12, 0) => (size.width, size.height, 1, 1, [0, -1, -1, -1]),
            (Self::Nv12, 1) => (chroma_width, chroma_height, 2, 2, [1, 2, -1, -1]),
            (Self::I420, 0) => (size.width, size.height, 1, 1, [0, -1, -1, -1]),
            (Self::I420, 1) => (chroma_width, chroma_height, 1, 1, [1, -1, -1, -1]),
            (Self::I420, 2) => (chroma_width, chroma_height, 1, 1, [2, -1, -1, -1]),
            _ => return Err(anyhow!("invalid raw video plane")),
        };
        let row_len = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(pixel_stride))
            .ok_or_else(|| anyhow!("video frame row is too large"))?;

        Ok(RawVideoPlaneLayout {
            width,
            height,
            row_len,
            pixel_stride,
            components,
            component_map,
        })
    }

    pub fn plane_layout_for_color(
        self,
        size: RenderSize,
        plane: usize,
        _color: FrameColor,
    ) -> Result<RawVideoPlaneLayout> {
        self.plane_layout(size, plane)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameColor {
    Sdr,
    Hdr10Bt2020,
    DolbyVisionProfile5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawVideoRange {
    Unknown,
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawVideoChromaSite {
    Unknown,
    Left,
    Center,
    TopLeft,
    TopCenter,
}

#[cfg(test)]
mod tests {
    use super::{FrameColor, RawVideoFormat};
    use crate::player::render_host::RenderSize;

    #[test]
    fn raw_video_format_reports_p010_plane_layouts() {
        let size = RenderSize {
            width: 5,
            height: 3,
        };
        let y = RawVideoFormat::P010Le.plane_layout(size, 0).unwrap();
        let uv = RawVideoFormat::P010Le.plane_layout(size, 1).unwrap();

        assert_eq!(
            (y.width, y.height, y.row_len, y.pixel_stride),
            (5, 3, 10, 2)
        );
        assert_eq!(
            (uv.width, uv.height, uv.row_len, uv.pixel_stride),
            (3, 2, 12, 4)
        );
        assert_eq!(uv.component_map, [1, 2, -1, -1]);
        assert_eq!(RawVideoFormat::P010Le.bit_shift(), 6);
    }

    #[test]
    fn raw_video_format_reports_8_bit_yuv_plane_layouts() {
        let size = RenderSize {
            width: 5,
            height: 3,
        };
        let nv12_y = RawVideoFormat::Nv12.plane_layout(size, 0).unwrap();
        let nv12_uv = RawVideoFormat::Nv12.plane_layout(size, 1).unwrap();
        let i420_u = RawVideoFormat::I420.plane_layout(size, 1).unwrap();
        let i420_v = RawVideoFormat::I420.plane_layout(size, 2).unwrap();

        assert_eq!(RawVideoFormat::Nv12.plane_count(), 2);
        assert_eq!(RawVideoFormat::I420.plane_count(), 3);
        assert_eq!((nv12_y.width, nv12_y.height, nv12_y.row_len), (5, 3, 5));
        assert_eq!((nv12_uv.width, nv12_uv.height, nv12_uv.row_len), (3, 2, 6));
        assert_eq!(nv12_uv.component_map, [1, 2, -1, -1]);
        assert_eq!((i420_u.width, i420_u.height, i420_u.row_len), (3, 2, 3));
        assert_eq!((i420_v.width, i420_v.height, i420_v.row_len), (3, 2, 3));
        assert_eq!(RawVideoFormat::Nv12.color_depth(), 8);
    }

    #[test]
    fn raw_video_format_preserves_p010_chroma_mapping_for_dolby_vision_profile5() {
        let layout = RawVideoFormat::P010Le
            .plane_layout_for_color(
                RenderSize {
                    width: 5,
                    height: 3,
                },
                1,
                FrameColor::DolbyVisionProfile5,
            )
            .unwrap();

        assert_eq!(layout.component_map, [1, 2, -1, -1]);
    }
}
