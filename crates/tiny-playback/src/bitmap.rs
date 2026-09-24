use std::{fmt, sync::Arc};

use anyhow::{Result, anyhow};

use crate::{RenderSize, render_host::frame_byte_len};

/// An owned, tightly packed BGRA8 image with straight (not premultiplied) alpha.
///
/// Rows run from top to bottom, with no padding. Video output is display-ready
/// SDR; HDR/Dolby Vision conversion is performed before this boundary. The size
/// describes the pixel buffer, which may differ from the source video size.
/// This type intentionally does not implement `Clone`: consuming it transfers
/// the pixel allocation to the presentation adapter without a full-frame copy.
pub struct BgraImage {
    size: RenderSize,
    bytes: Vec<u8>,
}

impl BgraImage {
    pub fn new(bytes: Vec<u8>, width: u32, height: u32) -> Result<Self> {
        let size = RenderSize { width, height };
        if width == 0 || height == 0 {
            return Err(anyhow!("invalid image dimensions"));
        }
        if bytes.len() != frame_byte_len(size)? {
            return Err(anyhow!("invalid image buffer size"));
        }
        Ok(Self { size, bytes })
    }

    pub fn size(&self) -> RenderSize {
        self.size
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl fmt::Debug for BgraImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BgraImage")
            .field("size", &self.size)
            .finish()
    }
}

/// An immutable bitmap shared by subtitle queues, events and presentation.
///
/// Cloning shares pixels. Equality compares image identity, not pixel contents,
/// so timeline updates and presentation caches can cheaply recognize a bitmap.
#[derive(Clone, Debug)]
pub struct SharedBgraImage(Arc<BgraImage>);

impl SharedBgraImage {
    pub fn new(image: BgraImage) -> Self {
        Self(Arc::new(image))
    }

    pub fn image(&self) -> &BgraImage {
        &self.0
    }
}

impl PartialEq for SharedBgraImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedBgraImage {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_image_preserves_channels_alpha_and_allocation() {
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let allocation = pixels.as_ptr();
        let image = BgraImage::new(pixels, 2, 1).unwrap();
        assert_eq!(
            image.size(),
            RenderSize {
                width: 2,
                height: 1
            }
        );
        assert_eq!(image.bytes(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let pixels = image.into_bytes();
        assert_eq!(pixels.as_ptr(), allocation);
    }

    #[test]
    fn image_rejects_empty_overflowing_and_mismatched_dimensions() {
        for (width, height, bytes) in [
            (0, 1, vec![]),
            (1, 0, vec![]),
            (u32::MAX, u32::MAX, vec![]),
            (1, 1, vec![0; 3]),
            (1, 1, vec![0; 5]),
        ] {
            assert!(BgraImage::new(bytes, width, height).is_err());
        }
    }

    #[test]
    fn shared_image_clones_keep_identity_and_pixels_alive() {
        let first = SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 4], 1, 1).unwrap());
        let second = first.clone();
        let other = SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 4], 1, 1).unwrap());
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(
            first.image().bytes().as_ptr(),
            second.image().bytes().as_ptr()
        );
        drop(first);
        assert_eq!(second.image().bytes(), [1, 2, 3, 4]);
    }
}
