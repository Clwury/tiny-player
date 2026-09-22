use std::{io::Write, net::TcpListener, thread};

use gpui::{Context, ParentElement as _, Render, TestAppContext, size};

use super::*;

pub(crate) struct StubPreviews(pub(crate) Arc<RenderImage>);

impl gpui::Global for StubPreviews {}

pub(crate) fn stub_previews(cx: &mut App) {
    cx.set_global(StubPreviews(
        decode_icon(include_bytes!("../../../assets/icons/emby.png")).unwrap(),
    ));
}

pub(crate) fn has_preview(session: Uuid, url: &str, cx: &App) -> bool {
    cx.has_asset::<IconPreviewAsset>(&(session, url.to_owned()))
}

fn mock_icon(status: u16, body: &[u8]) -> (String, thread::JoinHandle<String>) {
    let body = body.to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/icon.png", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut buffer = [0; 1024];
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        String::from_utf8(request).unwrap()
    });
    (url, task)
}

#[test]
fn downloads_public_icons_without_emby_credentials_and_limits_render_size() {
    let (url, request) = mock_icon(200, include_bytes!("../../../assets/icons/tiny-player.png"));
    let icon = load_icon(&url).unwrap();
    assert_eq!(icon.size(0).width.0, 96);
    assert_eq!(icon.size(0).height.0, 96);
    let request = request.join().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /icon.png http/1.1\r\n"));
    assert!(!request.contains("authorization"));
    assert!(!request.contains("x-emby-token"));
    assert!(request.contains("cache-control: no-cache, no-store"));
}

#[test]
fn cached_card_icons_load_again_after_the_server_is_offline() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("../../../assets/icons/tiny-player.png");
    let (url, request) = mock_icon(200, bytes);
    load_cached_icon_in(&url, directory.path()).unwrap();
    request.join().unwrap();
    assert_eq!(
        fs::read(icon_cache_path(&url, directory.path())).unwrap(),
        bytes
    );
    assert!(load_cached_icon_in(&url, directory.path()).is_ok());
}

#[test]
fn previews_fetch_the_url_even_when_a_card_icon_is_cached() {
    let directory = tempfile::tempdir().unwrap();
    let cached = include_bytes!("../../../assets/icons/tiny-player.png");
    let (url, request) = mock_icon(404, b"preview must reach the network");
    let path = icon_cache_path(&url, directory.path());
    fs::write(&path, cached).unwrap();
    assert!(load_cached_icon_in(&url, directory.path()).is_ok());
    assert!(load_icon(&url).is_err());
    request.join().unwrap();
    assert_eq!(fs::read(path).unwrap(), cached);
}

#[test]
fn corrupt_icon_cache_is_replaced_with_a_valid_download() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = include_bytes!("../../../assets/icons/tiny-player.png");
    let (url, request) = mock_icon(200, bytes);
    let path = icon_cache_path(&url, directory.path());
    fs::write(&path, b"incomplete image").unwrap();
    load_cached_icon_in(&url, directory.path()).unwrap();
    request.join().unwrap();
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert!(load_cached_icon_in(&url, directory.path()).is_ok());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn failed_invalid_and_oversized_downloads_do_not_populate_the_icon_cache() {
    for (status, body) in [
        (404, b"not found".to_vec()),
        (200, b"not an image".to_vec()),
        (200, vec![0; MAX_ICON_BYTES as usize + 1]),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (url, request) = mock_icon(status, &body);
        assert!(load_cached_icon_in(&url, directory.path()).is_err());
        request.join().unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[test]
fn every_catalog_url_has_a_distinct_bounded_cache_path() {
    let paths: std::collections::HashSet<_> = all_icons()
        .iter()
        .map(|icon| icon_cache_path(&icon.url, Path::new("cache")))
        .collect();
    assert_eq!(paths.len(), all_icons().len());
    assert!(
        paths
            .iter()
            .all(|path| path.file_name().unwrap().len() == 20)
    );
    assert_ne!(
        icon_cache_path("https://one.example/icon.png?rev=1", Path::new("cache")),
        icon_cache_path("https://one.example/icon.png?rev=2", Path::new("cache")),
    );
}

#[test]
fn failed_responses_and_invalid_image_data_are_rejected() {
    for (status, body) in [
        (404, b"not found".as_slice()),
        (200, b"not an image".as_slice()),
    ] {
        let (url, request) = mock_icon(status, body);
        assert!(load_icon(&url).is_err());
        request.join().unwrap();
    }
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
