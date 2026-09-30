use crate::images::server_icons::tests::{mock_icon, stub_icon};

use gpui::{Context, ParentElement as _, Render, TestAppContext, size};

use super::*;

use crate::images::server_icon_assets::StubPreviews;

pub(crate) fn stub_previews(cx: &mut App) {
    cx.set_global(StubPreviews(stub_icon()));
}

pub(crate) fn has_preview(session: Uuid, url: &str, cx: &App) -> bool {
    cx.has_asset::<IconPreviewAsset>(&(session, url.to_owned()))
}

struct IconPreview(Option<String>);

impl Render for IconPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(server_icon(self.0.as_deref(), 32.0))
    }
}

#[gpui::test]
fn an_unmatched_server_renders_the_default_emby_icon(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|_, _| IconPreview(None));
    cx.run_until_parked();
    let bounds = cx.debug_bounds("server-icon-default").unwrap();
    assert_eq!(bounds.size, size(px(32.0), px(32.0)));
}

#[gpui::test]
fn a_failed_icon_download_renders_the_default_emby_icon(cx: &mut TestAppContext) {
    let (url, request) = mock_icon(404, b"not found");
    let (_, cx) = cx.add_window_view(|_, _| IconPreview(Some(url)));
    cx.run_until_parked();
    request.join().unwrap();
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
    let bounds = cx.debug_bounds("server-icon-default").unwrap();
    assert_eq!(bounds.size, size(px(32.0), px(32.0)));
}
