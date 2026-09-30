//! GPUI image-cache adapter. Card assets belong to the app cache; preview
//! assets are keyed by picker session and explicitly evicted when it closes.
//! GPUI polls each repository future on its background executor and keeps a
//! weak reference to the originating cache entry for completion. Eviction
//! cancels delivery, so a retired preview cannot populate a reopened picker.
use super::{FileImageRepository, ImageRepository, ServerIconLoad, ServerIconRequest};
use gpui::{App, Asset, ImageCacheError, RenderImage};
use std::sync::Arc;
use uuid::Uuid;

pub(crate) enum ServerIconAsset {}

impl Asset for ServerIconAsset {
    type Source = String;
    type Output = std::result::Result<Arc<RenderImage>, ImageCacheError>;

    #[allow(clippy::manual_async_fn)]
    fn load(
        url: Self::Source,
        _: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        async move {
            FileImageRepository
                .load(&ServerIconRequest {
                    url,
                    load: ServerIconLoad::Cached,
                })
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
    }
}

pub(crate) enum IconPreviewAsset {}

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
        let stub = _cx.try_global::<StubPreviews>().map(|stub| stub.0.clone());
        async move {
            #[cfg(test)]
            if let Some(image) = stub {
                return Ok(image);
            }
            FileImageRepository
                .load(&ServerIconRequest {
                    url,
                    load: ServerIconLoad::Preview,
                })
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
    }
}

pub(crate) fn clear_icon_previews<'a>(
    session: Uuid,
    urls: impl IntoIterator<Item = &'a str>,
    cx: &mut App,
) {
    for url in urls {
        cx.remove_asset::<IconPreviewAsset>(&(session, url.to_owned()));
    }
}

pub(crate) fn reload_server_icon(url: &str, cx: &mut App) {
    cx.remove_asset::<ServerIconAsset>(&url.to_owned());
}

#[cfg(test)]
pub(crate) struct StubPreviews(pub(crate) Arc<RenderImage>);
#[cfg(test)]
impl gpui::Global for StubPreviews {}
