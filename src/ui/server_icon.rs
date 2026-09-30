use gpui::{
    AnyElement, App, InteractiveElement, IntoElement, Styled, StyledImage, Window, div, img, px,
};
use uuid::Uuid;

use crate::images::server_icon_assets::{IconPreviewAsset, ServerIconAsset};

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

fn default_icon(size: f32) -> AnyElement {
    use gpui::ParentElement as _;

    div()
        .debug_selector(|| "server-icon-default".into())
        .size(px(size))
        .flex_none()
        .child(img("icons/emby.png").size_full())
        .into_any_element()
}

#[cfg(test)]
pub(crate) mod tests;
