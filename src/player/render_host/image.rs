use std::sync::Arc;

use anyhow::{Result, anyhow};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};

use super::RenderSize;

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
        frame_byte_len, packed_bgra_from_stride, raw_plane_from_stride, render_image_from_bgra,
    };
    use crate::player::render_host::RenderSize;

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
}
