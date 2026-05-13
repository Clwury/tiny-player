use std::{
    mem,
    ops::Deref,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};

use super::dovi::DoviFrameMetadata;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlaybackSessionId(pub u64);

impl PlaybackSessionId {
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }
}

#[derive(Clone, Default, Debug)]
pub struct FrameBufferPool {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FrameBufferPool {
    const MAX_RETAINED_BUFFERS: usize = 12;

    pub fn rent(&self, min_capacity: usize) -> PooledBytes {
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        let index = buffers
            .iter()
            .position(|buffer| buffer.capacity() >= min_capacity)
            .unwrap_or_else(|| buffers.len());
        let mut bytes = if index < buffers.len() {
            buffers.swap_remove(index)
        } else {
            Vec::with_capacity(min_capacity)
        };
        bytes.clear();
        PooledBytes {
            bytes,
            pool: Some(self.clone()),
        }
    }

    fn recycle(&self, mut bytes: Vec<u8>) {
        bytes.clear();
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        if buffers.len() < Self::MAX_RETAINED_BUFFERS {
            buffers.push(bytes);
        }
    }
}

#[derive(Debug)]
pub struct PooledBytes {
    bytes: Vec<u8>,
    pool: Option<FrameBufferPool>,
}

impl PooledBytes {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes, pool: None }
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    pub fn resize(&mut self, len: usize, value: u8) {
        self.bytes.resize(len, value);
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        self.pool = None;
        mem::take(&mut self.bytes)
    }
}

impl Clone for PooledBytes {
    fn clone(&self) -> Self {
        Self::from_vec(self.bytes.clone())
    }
}

impl Deref for PooledBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl Drop for PooledBytes {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.recycle(mem::take(&mut self.bytes));
        }
    }
}

impl From<Vec<u8>> for PooledBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl PartialEq for PooledBytes {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PooledBytes {}

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub size: RenderSize,
    pub pts: Option<FramePts>,
    pub key_frame: bool,
    pub pixels: FramePixels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FramePts {
    pub nsecs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramePixels {
    Bgra8(PooledBytes),
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
    pub data: PooledBytes,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderBackpressure {
    pub rendering: bool,
    pub pending_requests: usize,
    pub last_render_nsecs: u64,
    pub average_render_nsecs: u64,
}

impl RenderBackpressure {
    pub fn should_drop_non_key_frame(self) -> bool {
        self.is_backlogged()
    }

    pub fn is_backlogged(self) -> bool {
        self.rendering
            && self.pending_requests > 0
            && (self.average_render_nsecs >= 33_000_000 || self.last_render_nsecs >= 50_000_000)
    }
}

#[derive(Default)]
struct FrameSlotState {
    active_session_id: PlaybackSessionId,
    latest_frame: Option<DecodedFrame>,
    current_size: Option<RenderSize>,
    pending_size_change: Option<RenderSize>,
    buffer_pool: FrameBufferPool,
    render_backpressure: RenderBackpressure,
}

impl FrameSlot {
    pub fn buffer_pool(&self) -> FrameBufferPool {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .buffer_pool
            .clone()
    }

    pub fn begin_session(&self, session_id: PlaybackSessionId) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let buffer_pool = state.buffer_pool.clone();
        let render_backpressure = state.render_backpressure;
        *state = FrameSlotState {
            active_session_id: session_id,
            latest_frame: None,
            current_size: None,
            pending_size_change: None,
            buffer_pool,
            render_backpressure,
        };
    }

    pub fn push(&self, session_id: PlaybackSessionId, frame: DecodedFrame) -> bool {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        if state.active_session_id != session_id {
            return false;
        }
        if state.current_size != Some(frame.size) {
            state.current_size = Some(frame.size);
            state.pending_size_change = Some(frame.size);
        }
        state.latest_frame = Some(frame);
        true
    }

    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .latest_frame
            .take()
    }

    pub fn take_size_change(&self) -> Option<(PlaybackSessionId, RenderSize)> {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let size = state.pending_size_change.take()?;
        Some((state.active_session_id, size))
    }

    pub fn render_backpressure(&self) -> RenderBackpressure {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .render_backpressure
    }

    pub fn update_render_backpressure(&self, backpressure: RenderBackpressure) {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .render_backpressure = backpressure;
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let buffer_pool = state.buffer_pool.clone();
        *state = FrameSlotState {
            buffer_pool,
            ..FrameSlotState::default()
        };
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

#[cfg(test)]
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
        DecodedFrame, FrameColor, FramePixels, FrameSlot, PlaybackSessionId, RawVideoFormat,
        RenderBackpressure, RenderSize, frame_byte_len, packed_bgra_from_stride,
        raw_plane_from_stride, render_image_from_bgra,
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
        let session_id = PlaybackSessionId(1);
        slot.begin_session(session_id);
        let first = RenderSize {
            width: 2,
            height: 1,
        };
        let second = RenderSize {
            width: 4,
            height: 1,
        };

        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: first,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![1; 8].into()),
            }
        ));
        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: first,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![2; 8].into()),
            }
        ));

        assert_eq!(slot.take_size_change(), Some((session_id, first)));
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![2; 8].into())
        );
        assert!(slot.take_frame().is_none());

        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: second,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![3; 16].into()),
            }
        ));
        assert_eq!(slot.take_size_change(), Some((session_id, second)));
    }

    #[test]
    fn frame_slot_rejects_stale_session_frames() {
        let slot = FrameSlot::default();
        let current = PlaybackSessionId(2);
        let stale = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(current);

        assert!(!slot.push(
            stale,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![1; 8].into()),
            }
        ));

        assert!(slot.take_frame().is_none());
        assert_eq!(slot.take_size_change(), None);
    }

    #[test]
    fn render_backpressure_only_drops_non_key_frames_when_backlogged() {
        assert!(!RenderBackpressure::default().should_drop_non_key_frame());
        assert!(
            RenderBackpressure {
                rendering: true,
                pending_requests: 1,
                last_render_nsecs: 55_000_000,
                average_render_nsecs: 20_000_000,
            }
            .should_drop_non_key_frame()
        );
    }
}
