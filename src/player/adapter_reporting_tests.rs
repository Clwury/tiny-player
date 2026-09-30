use super::*;
use crate::{
    emby::{PlaybackProgressReport, PlaybackStartReport, PlaybackStopReport},
    player::gateway::PlaybackReport,
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

#[test]
fn report_gateway_preserves_all_post_endpoints_bodies_and_http_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mock = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let status = if index == 3 {
                "500 Internal Server Error"
            } else {
                "204 No Content"
            };
            write!(
                reader.get_mut(),
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            requests.push((
                headers,
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            ));
        }
        requests
    });
    let gateway = EmbyPlaybackGateway {
        client: EmbyClient::new("report-adapter-test".into()).unwrap(),
        server: serde_json::from_value(serde_json::json!({
            "id":"gateway-test", "username":"test", "password":"", "user_id":"user", "access_token":"synthetic-token", "added_at_unix":0,
            "endpoint":{"protocol":"Http", "address":"127.0.0.1", "port":port, "path":"/emby"}
        })).unwrap(),
    };
    let mut start = PlaybackStartReport::direct_stream(
        "physical".into(),
        "source".into(),
        "playlistItem1".into(),
        vec![crate::emby::PlaybackQueueReportItem {
            id: "physical".into(),
            playlist_item_id: "playlistItem1".into(),
        }],
    );
    start.position_ticks = 12_345_678;
    start.play_session_id = Some("session".into());
    start.audio_stream_index = Some(2);
    start.subtitle_stream_index = Some(4);
    let mut progress = PlaybackProgressReport::direct_stream(
        "physical".into(),
        "source".into(),
        "playlistItem1".into(),
        3,
        1,
    );
    progress.position_ticks = 99_999_999;
    progress.is_paused = true;
    progress.volume_level = 76;
    progress.play_session_id = Some("session".into());
    let mut stop = PlaybackStopReport::direct_stream("physical".into(), "source".into());
    stop.position_ticks = 900_000_000;
    stop.play_session_id = Some("session".into());
    stop.failed = true;
    let expected = [
        serde_json::to_value(&start).unwrap(),
        serde_json::to_value(&progress).unwrap(),
        serde_json::to_value(&stop).unwrap(),
    ];
    gateway.report(&PlaybackReport::Started(start)).unwrap();
    gateway.report(&PlaybackReport::Progress(progress)).unwrap();
    gateway
        .report(&PlaybackReport::Stopped(stop.clone()))
        .unwrap();
    assert!(gateway.report(&PlaybackReport::Stopped(stop)).is_err());
    for ((headers, body), (path, expected)) in mock.join().unwrap().into_iter().take(3).zip(
        [
            "/emby/Sessions/Playing",
            "/emby/Sessions/Playing/Progress",
            "/emby/Sessions/Playing/Stopped",
        ]
        .into_iter()
        .zip(expected),
    ) {
        assert_eq!(
            headers.lines().next().unwrap(),
            format!("POST {path} HTTP/1.1")
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-emby-token: synthetic-token")
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert_eq!(body, expected);
    }
}
