use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};

use super::dovi::DoviFrameMetadata;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub size: RenderSize,
    pub pts: Option<FramePts>,
    pub pixels: FramePixels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FramePts {
    pub nsecs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramePixels {
    Bgra8(Vec<u8>),
    RawVideo(RawVideoFrame),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawVideoFrame {
    pub format: RawVideoFormat,
    pub color: FrameColor,
    pub range: RawVideoRange,
    pub chroma_site: RawVideoChromaSite,
    pub metadata: Option<FrameDynamicMetadata>,
    pub planes: RawVideoPlanes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDynamicMetadata {
    pub dolby_vision: Option<DoviFrameMetadata>,
}

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
    pub data: Vec<u8>,
    pub stride: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Default)]
pub struct FrameSlot {
    inner: Arc<Mutex<FrameSlotState>>,
}

#[derive(Default)]
struct FrameSlotState {
    latest_frame: Option<DecodedFrame>,
    current_size: Option<RenderSize>,
    pending_size_change: Option<RenderSize>,
}

impl FrameSlot {
    pub fn push(&self, frame: DecodedFrame) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        if state.current_size != Some(frame.size) {
            state.current_size = Some(frame.size);
            state.pending_size_change = Some(frame.size);
        }
        state.latest_frame = Some(frame);
    }

    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .latest_frame
            .take()
    }

    pub fn take_size_change(&self) -> Option<RenderSize> {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .pending_size_change
            .take()
    }

    pub fn clear(&self) {
        *self.inner.lock().expect("video frame slot poisoned") = FrameSlotState::default();
    }
}

pub fn frame_byte_len(size: RenderSize) -> Result<usize> {
    let pixels = size
        .width
        .checked_mul(size.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    usize::try_from(pixels).map_err(|_| anyhow!("video frame buffer is too large"))
}

#[cfg(test)]
pub fn packed_bgra_from_stride(data: &[u8], size: RenderSize, stride: usize) -> Result<Vec<u8>> {
    packed_frame_data_from_stride(data, size, stride, 4)
}

#[cfg(test)]
pub fn packed_frame_data_from_stride(
    data: &[u8],
    size: RenderSize,
    stride: usize,
    bytes_per_pixel: usize,
) -> Result<Vec<u8>> {
    if size.width == 0 || size.height == 0 {
        return Err(anyhow!("invalid video frame dimensions"));
    }

    let row_len = frame_row_len(size, bytes_per_pixel)?;
    if stride < row_len {
        return Err(anyhow!("invalid video frame stride"));
    }

    let height = usize::try_from(size.height).map_err(|_| anyhow!("video frame is too tall"))?;
    let required_len = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_len))
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    if data.len() < required_len {
        return Err(anyhow!("invalid video frame buffer size"));
    }

    let packed_len = row_len
        .checked_mul(height)
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    if stride == row_len {
        return Ok(data[..packed_len].to_vec());
    }

    let mut pixels = Vec::with_capacity(packed_len);
    for row in 0..height {
        let start = row * stride;
        pixels.extend_from_slice(&data[start..start + row_len]);
    }
    Ok(pixels)
}

#[cfg(test)]
pub fn raw_plane_from_stride(
    data: &[u8],
    offset: usize,
    stride: usize,
    row_len: usize,
    height: u32,
) -> Result<Vec<u8>> {
    Ok(raw_plane_slice_from_stride(data, offset, stride, row_len, height)?.to_vec())
}

#[cfg(test)]
pub fn raw_plane_slice_from_stride(
    data: &[u8],
    offset: usize,
    stride: usize,
    row_len: usize,
    height: u32,
) -> Result<&[u8]> {
    let end = raw_plane_end(data.len(), offset, stride, row_len, height)?;
    Ok(&data[offset..end])
}

fn raw_plane_end(
    data_len: usize,
    offset: usize,
    stride: usize,
    row_len: usize,
    height: u32,
) -> Result<usize> {
    if height == 0 || row_len == 0 {
        return Err(anyhow!("invalid video frame dimensions"));
    }
    if stride < row_len {
        return Err(anyhow!("invalid video frame stride"));
    }

    let height = usize::try_from(height).map_err(|_| anyhow!("video frame is too tall"))?;
    let required_len = stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_len))
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    let end = offset
        .checked_add(required_len)
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    if data_len < end {
        return Err(anyhow!("invalid video frame buffer size"));
    }

    Ok(end)
}

pub fn sdr_8bit_yuv_to_bgra(raw: &RawVideoFrame, size: RenderSize) -> Result<Option<Vec<u8>>> {
    if raw.color != FrameColor::Sdr
        || !matches!(raw.format, RawVideoFormat::Nv12 | RawVideoFormat::I420)
    {
        return Ok(None);
    }

    match &raw.planes {
        RawVideoPlanes::Owned(planes) => convert_owned_sdr_yuv_to_bgra(raw, size, planes),
    }
}

fn convert_owned_sdr_yuv_to_bgra(
    raw: &RawVideoFrame,
    size: RenderSize,
    planes: &[RawVideoPlane],
) -> Result<Option<Vec<u8>>> {
    let plane = |index: usize| -> Result<PlaneView<'_>> {
        let layout = raw.format.plane_layout(size, index)?;
        let plane = planes
            .get(index)
            .ok_or_else(|| anyhow!("video frame is missing a raw plane"))?;
        plane_view_from_slice(&plane.data, 0, plane.stride, layout.row_len, layout.height)
    };
    convert_sdr_yuv_planes_to_bgra(raw.format, raw.range, size, plane)
}

fn convert_sdr_yuv_planes_to_bgra<'a>(
    format: RawVideoFormat,
    range: RawVideoRange,
    size: RenderSize,
    plane: impl Fn(usize) -> Result<PlaneView<'a>>,
) -> Result<Option<Vec<u8>>> {
    let y = plane(0)?;
    let pixels = match format {
        RawVideoFormat::Nv12 => {
            let uv = plane(1)?;
            convert_nv12_to_bgra(size, range, y, uv)?
        }
        RawVideoFormat::I420 => {
            let u = plane(1)?;
            let v = plane(2)?;
            convert_i420_to_bgra(size, range, y, u, v)?
        }
        _ => return Ok(None),
    };
    Ok(Some(pixels))
}

#[derive(Clone, Copy)]
struct PlaneView<'a> {
    data: &'a [u8],
    stride: usize,
}

fn plane_view_from_slice(
    data: &[u8],
    offset: usize,
    stride: usize,
    row_len: usize,
    height: u32,
) -> Result<PlaneView<'_>> {
    let end = raw_plane_end(data.len(), offset, stride, row_len, height)?;
    Ok(PlaneView {
        data: &data[offset..end],
        stride,
    })
}

fn convert_nv12_to_bgra(
    size: RenderSize,
    range: RawVideoRange,
    y: PlaneView<'_>,
    uv: PlaneView<'_>,
) -> Result<Vec<u8>> {
    let width = usize::try_from(size.width).map_err(|_| anyhow!("video frame is too wide"))?;
    let height = usize::try_from(size.height).map_err(|_| anyhow!("video frame is too tall"))?;
    let mut pixels = vec![0; frame_byte_len(size)?];

    for row in 0..height {
        let y_row = row * y.stride;
        let uv_row = (row / 2) * uv.stride;
        let out_row = row * width * 4;
        for col in 0..width {
            let y_value = y.data[y_row + col];
            let uv_index = uv_row + (col / 2) * 2;
            write_yuv_pixel(
                &mut pixels[out_row + col * 4..out_row + col * 4 + 4],
                y_value,
                uv.data[uv_index],
                uv.data[uv_index + 1],
                range,
            );
        }
    }

    Ok(pixels)
}

fn convert_i420_to_bgra(
    size: RenderSize,
    range: RawVideoRange,
    y: PlaneView<'_>,
    u: PlaneView<'_>,
    v: PlaneView<'_>,
) -> Result<Vec<u8>> {
    let width = usize::try_from(size.width).map_err(|_| anyhow!("video frame is too wide"))?;
    let height = usize::try_from(size.height).map_err(|_| anyhow!("video frame is too tall"))?;
    let mut pixels = vec![0; frame_byte_len(size)?];

    for row in 0..height {
        let y_row = row * y.stride;
        let uv_row = (row / 2) * u.stride;
        let out_row = row * width * 4;
        for col in 0..width {
            let uv_col = col / 2;
            write_yuv_pixel(
                &mut pixels[out_row + col * 4..out_row + col * 4 + 4],
                y.data[y_row + col],
                u.data[uv_row + uv_col],
                v.data[(row / 2) * v.stride + uv_col],
                range,
            );
        }
    }

    Ok(pixels)
}

fn write_yuv_pixel(pixel: &mut [u8], y: u8, u: u8, v: u8, range: RawVideoRange) {
    let (r, g, b) = yuv_to_rgb_bt709(y, u, v, range);
    pixel[0] = b;
    pixel[1] = g;
    pixel[2] = r;
    pixel[3] = 255;
}

fn yuv_to_rgb_bt709(y: u8, u: u8, v: u8, range: RawVideoRange) -> (u8, u8, u8) {
    let y = i32::from(y);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    if range == RawVideoRange::Full {
        return (
            clamp_u8(y + ((403 * v + 128) >> 8)),
            clamp_u8(y - ((48 * u + 120 * v + 128) >> 8)),
            clamp_u8(y + ((475 * u + 128) >> 8)),
        );
    }

    let y = (y - 16).max(0);
    (
        clamp_u8((298 * y + 459 * v + 128) >> 8),
        clamp_u8((298 * y - 55 * u - 136 * v + 128) >> 8),
        clamp_u8((298 * y + 541 * u + 128) >> 8),
    )
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
fn frame_row_len(size: RenderSize, bytes_per_pixel: usize) -> Result<usize> {
    usize::try_from(size.width)
        .ok()
        .and_then(|width| width.checked_mul(bytes_per_pixel))
        .ok_or_else(|| anyhow!("video frame row is too large"))
}

pub fn render_image_from_bgra(bgra: Vec<u8>, width: u32, height: u32) -> Result<Arc<RenderImage>> {
    if bgra.len() != width as usize * height as usize * 4 {
        return Err(anyhow!("invalid video frame buffer size"));
    }

    let image = ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, bgra)
        .ok_or_else(|| anyhow!("invalid video frame buffer dimensions"))?;
    Ok(Arc::new(RenderImage::new([Frame::new(RgbaImage::from(
        image,
    ))])))
}

#[cfg(test)]
mod tests {
    use super::{
        DecodedFrame, FrameColor, FramePixels, FrameSlot, RawVideoChromaSite, RawVideoFormat,
        RawVideoFrame, RawVideoPlane, RawVideoPlanes, RawVideoRange, RenderSize, frame_byte_len,
        packed_bgra_from_stride, raw_plane_from_stride, render_image_from_bgra,
        sdr_8bit_yuv_to_bgra,
    };

    #[test]
    fn frame_byte_len_uses_four_byte_pixels() {
        assert_eq!(
            frame_byte_len(RenderSize {
                width: 3840,
                height: 2160,
            })
            .unwrap(),
            3840 * 2160 * 4
        );
    }

    #[test]
    fn packed_bgra_from_stride_copies_tightly_packed_frame() {
        let pixels = packed_bgra_from_stride(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            RenderSize {
                width: 2,
                height: 1,
            },
            8,
        )
        .unwrap();

        assert_eq!(pixels, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn packed_bgra_from_stride_skips_row_padding() {
        let pixels = packed_bgra_from_stride(
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88,
            ],
            RenderSize {
                width: 2,
                height: 2,
            },
            10,
        )
        .unwrap();

        assert_eq!(
            pixels,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn packed_bgra_from_stride_rejects_short_buffer() {
        assert!(
            packed_bgra_from_stride(
                &[1, 2, 3],
                RenderSize {
                    width: 1,
                    height: 1,
                },
                4,
            )
            .is_err()
        );
    }

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
    fn sdr_nv12_to_bgra_converts_full_range_neutral_gray() {
        let size = RenderSize {
            width: 2,
            height: 2,
        };
        let raw = RawVideoFrame {
            format: RawVideoFormat::Nv12,
            color: FrameColor::Sdr,
            range: RawVideoRange::Full,
            chroma_site: RawVideoChromaSite::Unknown,
            metadata: None,
            planes: RawVideoPlanes::Owned(vec![
                RawVideoPlane {
                    data: vec![128, 128, 128, 128],
                    stride: 2,
                },
                RawVideoPlane {
                    data: vec![128, 128],
                    stride: 2,
                },
            ]),
        };

        let pixels = sdr_8bit_yuv_to_bgra(&raw, size).unwrap().unwrap();

        assert_eq!(
            pixels,
            vec![
                128, 128, 128, 255, 128, 128, 128, 255, 128, 128, 128, 255, 128, 128, 128, 255,
            ]
        );
    }

    #[test]
    fn sdr_i420_to_bgra_converts_limited_range_luma() {
        let size = RenderSize {
            width: 2,
            height: 2,
        };
        let raw = RawVideoFrame {
            format: RawVideoFormat::I420,
            color: FrameColor::Sdr,
            range: RawVideoRange::Limited,
            chroma_site: RawVideoChromaSite::Unknown,
            metadata: None,
            planes: RawVideoPlanes::Owned(vec![
                RawVideoPlane {
                    data: vec![16, 235, 16, 235],
                    stride: 2,
                },
                RawVideoPlane {
                    data: vec![128],
                    stride: 1,
                },
                RawVideoPlane {
                    data: vec![128],
                    stride: 1,
                },
            ]),
        };

        let pixels = sdr_8bit_yuv_to_bgra(&raw, size).unwrap().unwrap();

        assert_eq!(
            pixels,
            vec![
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
            ]
        );
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

    #[test]
    fn raw_plane_from_stride_copies_plane_region_with_padding() {
        let plane =
            raw_plane_from_stride(&[0, 0, 1, 2, 3, 4, 99, 9, 10, 11, 12, 88, 0], 2, 5, 4, 2)
                .unwrap();

        assert_eq!(plane, [1, 2, 3, 4, 99, 9, 10, 11, 12]);
    }

    #[test]
    fn raw_plane_from_stride_rejects_short_buffer() {
        assert!(raw_plane_from_stride(&[1, 2, 3], 0, 4, 4, 1).is_err());
    }

    #[test]
    fn render_image_from_bgra_preserves_gpui_bgra_bytes() {
        let image = render_image_from_bgra(vec![1, 2, 3, 4, 5, 6, 7, 8], 2, 1).unwrap();

        assert_eq!(image.as_bytes(0), Some([1, 2, 3, 4, 5, 6, 7, 8].as_slice()));
    }

    #[test]
    fn render_image_from_bgra_rejects_wrong_buffer_size() {
        assert!(render_image_from_bgra(vec![1, 2, 3], 1, 1).is_err());
    }

    #[test]
    fn frame_slot_keeps_latest_frame_and_reports_size_changes() {
        let slot = FrameSlot::default();
        let first = RenderSize {
            width: 2,
            height: 1,
        };
        let second = RenderSize {
            width: 4,
            height: 1,
        };

        slot.push(DecodedFrame {
            size: first,
            pts: None,
            pixels: FramePixels::Bgra8(vec![1; 8]),
        });
        slot.push(DecodedFrame {
            size: first,
            pts: None,
            pixels: FramePixels::Bgra8(vec![2; 8]),
        });

        assert_eq!(slot.take_size_change(), Some(first));
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![2; 8])
        );
        assert!(slot.take_frame().is_none());

        slot.push(DecodedFrame {
            size: second,
            pts: None,
            pixels: FramePixels::Bgra8(vec![3; 16]),
        });
        assert_eq!(slot.take_size_change(), Some(second));
    }
}
