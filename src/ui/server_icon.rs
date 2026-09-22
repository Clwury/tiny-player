use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, ensure};
use gpui::{
    AnyElement, App, Asset, ImageCacheError, InteractiveElement, IntoElement, RenderImage, Styled,
    StyledImage, Window, div, img, px,
};
use image::{Frame, imageops::FilterType};
use uuid::Uuid;

use crate::{app_metadata, server::icon::all_icons};

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
        async move { cache_server_icon(&url).map_err(|error| ImageCacheError::Other(Arc::new(error))) }
    }
}

enum IconPreviewAsset {}

impl Asset for IconPreviewAsset {
    // Previews live only for this opening of the picker, separate from card assets.
    type Source = (Uuid, String);
    type Output = std::result::Result<Arc<RenderImage>, ImageCacheError>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        (_, url): Self::Source,
        _cx: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        #[cfg(test)]
        let stub = _cx
            .try_global::<tests::StubPreviews>()
            .map(|stub| stub.0.clone());
        async move {
            #[cfg(test)]
            if let Some(image) = stub {
                return Ok(image);
            }
            load_icon(&url).map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
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

pub(crate) fn icon_preview(session: Uuid, url: &str, size: f32) -> AnyElement {
    let source = (session, url.to_owned());
    img(move |window: &mut Window, cx: &mut App| window.use_asset::<IconPreviewAsset>(&source, cx))
        .size(px(size))
        .flex_none()
        .with_loading(move || default_icon(size))
        .with_fallback(move || default_icon(size))
        .into_any_element()
}

pub(crate) fn clear_icon_previews(session: Uuid, cx: &mut App) {
    for icon in all_icons() {
        cx.remove_asset::<IconPreviewAsset>(&(session, icon.url.clone()));
    }
}

pub(crate) fn reload_server_icon(url: &str, cx: &mut App) {
    cx.remove_asset::<ServerIconAsset>(&url.to_owned());
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
    decode_icon(&download_icon(url)?)
}

fn download_icon(url: &str) -> Result<Vec<u8>> {
    // Catalog icons are public resources and must never use Emby credentials.
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?
        .get(url)
        .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response.take(MAX_ICON_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_ICON_BYTES, "服务器图标过大");
    Ok(bytes)
}

pub(crate) fn cache_server_icon(url: &str) -> Result<Arc<RenderImage>> {
    load_cached_icon_in(url, &app_metadata::cache_dir()?.join("server-icons"))
}

fn icon_cache_path(url: &str, directory: &Path) -> PathBuf {
    // Stable FNV-1a keys keep filenames bounded and include the entire URL.
    let key = url.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    directory.join(format!("{key:016x}.img"))
}

fn load_cached_icon_in(url: &str, directory: &Path) -> Result<Arc<RenderImage>> {
    let path = icon_cache_path(url, directory);
    if let Ok(file) = fs::File::open(&path) {
        let mut bytes = Vec::new();
        if file
            .take(MAX_ICON_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_ok()
            && bytes.len() as u64 <= MAX_ICON_BYTES
            && let Ok(image) = decode_icon(&bytes)
        {
            return Ok(image);
        }
    }

    let bytes = download_icon(url)?;
    // Reject broken downloads before replacing a cache entry or a card icon.
    let image = decode_icon(&bytes)?;
    fs::create_dir_all(directory).context("创建服务器图标缓存目录失败")?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = fs::write(&temporary, bytes).and_then(|_| fs::rename(&temporary, &path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("保存服务器图标缓存失败")?;
    Ok(image)
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
pub(crate) mod tests;
