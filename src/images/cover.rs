//! Local cover decoding adapter. GPUI owns the application-wide Asset cache,
//! keyed by immutable file path and target size, and polls the load future on
//! its background executor. It caches both success and failure until eviction
//! or app shutdown, preserving the existing warm-view behavior. A completion
//! only populates its own cache entry; it cannot mutate Home business state.
use std::{fs, path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Asset, ImageCacheError, RenderImage};
use image::{Frame, imageops::FilterType};

use super::{FileImageRepository, ImageRepository};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CoverImageRequest {
    pub(crate) path: Arc<Path>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) enum CoverImageAsset {}

impl Asset for CoverImageAsset {
    type Source = CoverImageRequest;
    type Output = std::result::Result<Arc<RenderImage>, ImageCacheError>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        source: Self::Source,
        _: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        async move {
            FileImageRepository
                .load(&source)
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
    }
}

impl ImageRepository<CoverImageRequest> for FileImageRepository {
    type Image = Arc<RenderImage>;

    fn load(&self, request: &CoverImageRequest) -> Result<Self::Image> {
        load_cover_image(request)
    }
}

fn load_cover_image(source: &CoverImageRequest) -> Result<Arc<RenderImage>> {
    let bytes = fs::read(&source.path)
        .with_context(|| format!("读取图片缓存失败：{}", source.path.display()))?;
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("解析图片缓存失败：{}", source.path.display()))?
        .to_rgba8();
    let (source_width, source_height) = image.dimensions();
    let (x, y, crop_width, crop_height) =
        cover_crop_bounds(source_width, source_height, source.width, source.height)?;
    let cropped = image::imageops::crop_imm(&image, x, y, crop_width, crop_height).to_image();
    let mut resized =
        image::imageops::resize(&cropped, source.width, source.height, FilterType::Lanczos3);

    for pixel in resized.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }

    Ok(Arc::new(RenderImage::new([Frame::new(resized)])))
}

fn cover_crop_bounds(
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<(u32, u32, u32, u32)> {
    if source_width == 0 || source_height == 0 || target_width == 0 || target_height == 0 {
        return Err(anyhow!("图片裁剪尺寸无效"));
    }

    let source_ratio = source_width as f64 / source_height as f64;
    let target_ratio = target_width as f64 / target_height as f64;

    if source_ratio > target_ratio {
        let crop_width = ((source_height as f64 * target_ratio).round() as u32)
            .max(1)
            .min(source_width);
        let x = (source_width - crop_width) / 2;
        Ok((x, 0, crop_width, source_height))
    } else {
        let crop_height = ((source_width as f64 / target_ratio).round() as u32)
            .max(1)
            .min(source_height);
        let y = (source_height - crop_height) / 2;
        Ok((0, y, source_width, crop_height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_wide_cover_images_horizontally() {
        assert_eq!(
            cover_crop_bounds(400, 213, 160, 213).unwrap(),
            (120, 0, 160, 213)
        );
    }

    #[test]
    fn crops_tall_cover_images_vertically() {
        assert_eq!(
            cover_crop_bounds(160, 400, 160, 213).unwrap(),
            (0, 93, 160, 213)
        );
    }

    #[test]
    fn crops_portrait_resume_image_to_fixed_landscape_frame() {
        assert_eq!(
            cover_crop_bounds(160, 213, 240, 135,).unwrap(),
            (0, 61, 160, 90)
        );
    }

    #[test]
    fn local_cover_decode_preserves_crop_size_and_bgra_channels() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.png");
        image::RgbaImage::from_pixel(4, 2, image::Rgba([11, 22, 33, 255]))
            .save(&path)
            .unwrap();
        let request = CoverImageRequest {
            path: path.into(),
            width: 2,
            height: 2,
        };
        let decoded = FileImageRepository.load(&request).unwrap();
        assert_eq!(decoded.size(0), gpui::size(2.into(), 2.into()));
        assert_eq!(decoded.frame_count(), 1);
        assert_eq!(decoded.as_bytes(0).unwrap(), [33, 22, 11, 255].repeat(4));
        assert!(
            FileImageRepository
                .load(&CoverImageRequest {
                    width: 0,
                    ..request
                })
                .is_err()
        );
    }

    #[gpui::test]
    fn cover_assets_cache_by_path_and_size_and_keep_failures_until_evicted(
        cx: &mut gpui::TestAppContext,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cover.png");
        let request = CoverImageRequest {
            path: path.clone().into(),
            width: 2,
            height: 2,
        };
        // A failed local decode is cached too, matching GPUI's existing contract.
        std::fs::write(&path, b"invalid image").unwrap();
        assert!(
            cx.update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
                .is_none()
        );
        cx.run_until_parked();
        assert!(
            cx.update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
                .unwrap()
                .is_err()
        );
        image::RgbaImage::from_pixel(4, 4, image::Rgba([11, 22, 33, 255]))
            .save(&path)
            .unwrap();
        assert!(
            cx.update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
                .unwrap()
                .is_err()
        );
        cx.update(|cx| cx.remove_asset::<CoverImageAsset>(&request));
        assert!(
            cx.update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
                .is_none()
        );
        cx.run_until_parked();
        let first = cx
            .update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
            .unwrap()
            .unwrap();
        let repeated = cx
            .update(|cx| cx.fetch_asset::<CoverImageAsset>(&request))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &repeated));
        let larger = CoverImageRequest {
            width: 4,
            height: 4,
            ..request.clone()
        };
        assert!(
            cx.update(|cx| cx.fetch_asset::<CoverImageAsset>(&larger))
                .is_none()
        );
        cx.run_until_parked();
        let second = cx
            .update(|cx| cx.fetch_asset::<CoverImageAsset>(&larger))
            .unwrap()
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(second.size(0), gpui::size(4.into(), 4.into()));
        cx.update(|cx| {
            cx.remove_asset::<CoverImageAsset>(&request);
            assert!(!cx.has_asset::<CoverImageAsset>(&request));
            assert!(cx.has_asset::<CoverImageAsset>(&larger));
        });
    }
}
