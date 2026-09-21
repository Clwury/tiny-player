use std::{io::Read, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use gpui::{
    AnyElement, App, Asset, ImageCacheError, InteractiveElement, IntoElement, RenderImage, Styled,
    StyledImage, Window, div, img, px,
};
use image::{Frame, imageops::FilterType};

const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;

enum ServerIconAsset {}

impl Asset for ServerIconAsset {
    type Source = String;
    type Output = std::result::Result<Arc<RenderImage>, ImageCacheError>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        url: Self::Source,
        _: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        async move { load_icon(&url).map_err(|error| ImageCacheError::Other(Arc::new(error))) }
    }
}

pub(crate) fn server_icon(url: Option<&str>, size: f32) -> AnyElement {
    let Some(url) = url else {
        return default_icon(size);
    };
    let url = url.to_owned();
    img(move |window: &mut Window, cx: &mut App| window.use_asset::<ServerIconAsset>(&url, cx))
        .size(px(size))
        .flex_none()
        .with_loading(move || default_icon(size))
        .with_fallback(move || default_icon(size))
        .into_any_element()
}

fn default_icon(size: f32) -> AnyElement {
    use gpui::ParentElement as _;

    div()
        .debug_selector(|| "server-icon-default".into())
        .size(px(size))
        .flex_none()
        .child(img("icons/emby.png").size_full())
        .into_any_element()
}

fn load_icon(url: &str) -> Result<Arc<RenderImage>> {
    // Catalog icons are public resources and must never use Emby credentials.
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get(url)
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response.take(MAX_ICON_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_ICON_BYTES, "服务器图标过大");
    decode_icon(&bytes)
}

fn decode_icon(bytes: &[u8]) -> Result<Arc<RenderImage>> {
    let image = image::load_from_memory(bytes).context("解析服务器图标失败")?;
    // Keep enough detail for the 32px card icon on high density displays.
    let mut image = image.resize(96, 96, FilterType::Lanczos3).to_rgba8();
    for pixel in image.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([Frame::new(image)])))
}

#[cfg(test)]
mod tests;
