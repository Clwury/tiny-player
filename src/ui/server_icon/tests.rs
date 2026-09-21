use std::{io::Write, net::TcpListener, thread};

use gpui::{Context, ParentElement as _, Render, TestAppContext, size};

use super::*;

fn mock_icon(status: u16, body: &'static [u8]) -> (String, thread::JoinHandle<String>) {
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
        stream.write_all(body).unwrap();
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
