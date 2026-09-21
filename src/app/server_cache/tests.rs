use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::Duration,
};

use gpui::{AppContext as _, TestAppContext};

use super::*;
use crate::{
    server::{CachedItemCounts, Protocol, ServerEndpoint},
    storage::ServerCache,
    theme,
};

mod lifecycle;

fn mock_server(
    responses: Vec<(u16, serde_json::Value)>,
) -> (AddServerSubmission, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = thread::spawn(move || {
        responses.into_iter().map(|(status, body)| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let request = read_request(&mut stream);
            let body = body.to_string();
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            request
        }).collect()
    });
    (
        AddServerSubmission {
            endpoint: ServerEndpoint {
                protocol: Protocol::Http,
                address: "127.0.0.1".into(),
                port,
                path: String::new(),
            },
            username: "test-user".into(),
            password: "test-password".into(),
        },
        task,
    )
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0);
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]);
            let body_len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + body_len {
                break;
            }
        }
    }
    String::from_utf8(request).unwrap()
}

fn auth_response(name: &str, token: &str) -> serde_json::Value {
    serde_json::json!({
        "AccessToken": token, "ServerId": "remote-server",
        "User": {"Id": "user-1", "ServerName": name}
    })
}

#[test]
fn adding_matches_public_icons_and_editing_preserves_cached_data_without_requests() {
    let (submission, requests) = mock_server(vec![(
        200,
        serde_json::json!({"ServerName": "UHD", "Id": "server-id"}),
    )]);
    let client = EmbyClient::new("test".into()).unwrap();
    let added = prepare_server(&client, &submission, None).unwrap();
    assert_eq!(added.server_name.as_deref(), Some("UHD"));
    assert!(added.user_id.is_none());
    assert!(added.access_token.is_none());
    assert!(!added.needs_auth_refresh);
    assert!(
        added
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/UHD-emby.png")
    );

    let authenticated = CachedServer {
        user_id: Some("user".into()),
        access_token: Some("old-token".into()),
        icon_url: Some("https://example.com/old.png".into()),
        item_counts: Some(CachedItemCounts {
            movie_count: 10,
            series_count: 20,
        }),
        ..added.clone()
    };
    let mut submission = submission;
    submission.endpoint.address = "offline.example".into();
    submission.username = "edited-user".into();
    submission.password = "edited-password".into();
    let edited = prepare_server(&client, &submission, Some(&authenticated)).unwrap();
    assert_eq!(edited.id, added.id);
    assert_eq!(edited.added_at_unix, added.added_at_unix);
    assert_eq!(edited.endpoint, submission.endpoint);
    assert_eq!(edited.username, submission.username);
    assert_eq!(edited.password, submission.password);
    assert_eq!(edited.server_name, authenticated.server_name);
    assert_eq!(edited.server_id, authenticated.server_id);
    assert_eq!(edited.user_id, authenticated.user_id);
    assert_eq!(edited.access_token, authenticated.access_token);
    assert_eq!(edited.icon_url, authenticated.icon_url);
    assert_eq!(edited.item_counts, authenticated.item_counts);
    assert!(edited.needs_auth_refresh);
    assert!(!edited.can_reuse_auth());
    let requests = requests.join().unwrap();
    assert_eq!(requests.len(), 1);
    for request in requests {
        assert!(request.starts_with("GET /emby/System/Info/Public HTTP/1.1\r\n"));
        assert!(!request.contains("test-password"));
    }
}

#[test]
fn adding_matches_an_attached_emby_suffix_from_public_server_info() {
    let (submission, requests) = mock_server(vec![(
        200,
        serde_json::json!({
            "LocalAddresses": [],
            "RemoteAddresses": [],
            "ServerName": "OkEmby",
            "Version": "4.9.1.90",
            "Id": "okemby-server-id"
        }),
    )]);
    let server =
        prepare_server(&EmbyClient::new("test".into()).unwrap(), &submission, None).unwrap();
    assert_eq!(server.server_name.as_deref(), Some("OkEmby"));
    assert_eq!(
        server.icon_url.as_deref(),
        Some("https://raw.githubusercontent.com/lige47/QuanX-icon-rule/main/icon/emby/Ok-emby.png")
    );
    assert!(server.access_token.is_none());
    let requests = requests.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /emby/System/Info/Public HTTP/1.1\r\n"));
}

#[test]
fn missing_or_unmatched_public_names_use_the_default_icon_on_add() {
    let responses = [
        serde_json::json!({"Id": "server-id"}),
        serde_json::json!({"ServerName": null}),
        serde_json::json!({"ServerName": " "}),
        serde_json::json!({"ServerName": "我的私人影院"}),
    ];
    let (submission, requests) =
        mock_server(responses.into_iter().map(|body| (200, body)).collect());
    let client = EmbyClient::new("test".into()).unwrap();
    for _ in 0..4 {
        let server = prepare_server(&client, &submission, None).unwrap();
        assert!(server.icon_url.is_none());
        assert!(server.access_token.is_none());
    }
    for request in requests.join().unwrap() {
        assert!(request.starts_with("GET /emby/System/Info/Public HTTP/1.1\r\n"));
    }
}

fn pending_server(submission: &AddServerSubmission) -> CachedServer {
    CachedServer {
        endpoint: submission.endpoint.clone(),
        username: submission.username.clone(),
        password: submission.password.clone(),
        user_id: None,
        access_token: None,
        server_name: None,
        icon_url: None,
        ..saved_server()
    }
}

#[test]
fn authenticating_refreshes_name_icon_and_token_and_preserves_identity() {
    let (mut submission, requests) = mock_server(vec![
        (200, auth_response("Alpha TV Emby", "first-token")),
        (200, auth_response("我的私人影院", "edited-token")),
    ]);
    let client = EmbyClient::new("test-device".into()).unwrap();
    let mut added = authenticate_server(&client, &pending_server(&submission)).unwrap();
    assert_eq!(added.user_id.as_deref(), Some("user-1"));
    assert_eq!(added.access_token.as_deref(), Some("first-token"));
    assert_eq!(added.server_name.as_deref(), Some("Alpha TV Emby"));
    assert!(
        added
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/AlphaTV-emby.png")
    );
    assert_eq!(added.server_id.as_deref(), Some("remote-server"));
    added.item_counts = Some(CachedItemCounts {
        movie_count: 10,
        series_count: 20,
    });
    submission.password = "edited-password".into();
    let edited = authenticate_server(
        &client,
        &CachedServer {
            password: submission.password.clone(),
            needs_auth_refresh: true,
            ..added.clone()
        },
    )
    .unwrap();
    assert_eq!(edited.id, added.id);
    assert_eq!(edited.added_at_unix, added.added_at_unix);
    assert_eq!(edited.access_token.as_deref(), Some("edited-token"));
    assert_eq!(edited.server_name.as_deref(), Some("我的私人影院"));
    assert!(edited.icon_url.is_none());
    assert_eq!(edited.item_counts, added.item_counts);
    assert!(!edited.needs_auth_refresh);

    for (request, password) in requests
        .join()
        .unwrap()
        .iter()
        .zip(["test-password", "edited-password"])
    {
        assert!(request.starts_with("POST /emby/Users/AuthenticateByName HTTP/1.1\r\n"));
        let body: serde_json::Value =
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["Username"], "test-user");
        assert_eq!(body["Pw"], password);
    }
}

#[test]
fn missing_auth_server_name_uses_public_info_instead_of_session_id() {
    let (submission, requests) = mock_server(vec![
        (
            200,
            serde_json::json!({"AccessToken": "token", "SessionInfo": {"Id": "session-not-a-name", "UserId": "user-1"}}),
        ),
        (
            200,
            serde_json::json!({"ServerName": "UHD", "Id": "remote-server"}),
        ),
    ]);
    let server = authenticate_server(
        &EmbyClient::new("test".into()).unwrap(),
        &pending_server(&submission),
    )
    .unwrap();
    assert_eq!(server.server_name.as_deref(), Some("UHD"));
    assert_eq!(server.server_id.as_deref(), Some("remote-server"));
    assert!(
        server
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/UHD-emby.png")
    );
    let requests = requests.join().unwrap();
    assert!(requests[0].starts_with("POST /emby/Users/AuthenticateByName "));
    assert!(requests[1].starts_with("GET /emby/System/Info/Public "));
}

#[test]
fn authentication_reuses_the_public_name_without_an_extra_metadata_request() {
    let (submission, requests) = mock_server(vec![(
        200,
        serde_json::json!({
            "AccessToken": "token", "SessionInfo": {"Id": "session-not-a-name", "UserId": "user-1"}
        }),
    )]);
    let server = CachedServer {
        server_name: Some("UHD".into()),
        ..pending_server(&submission)
    };
    let authenticated =
        authenticate_server(&EmbyClient::new("test".into()).unwrap(), &server).unwrap();
    assert_eq!(authenticated.server_name.as_deref(), Some("UHD"));
    assert!(
        authenticated
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/UHD-emby.png")
    );
    assert_eq!(requests.join().unwrap().len(), 1);
}

#[test]
fn entry_after_edit_refreshes_public_metadata_when_authentication_omits_the_name() {
    let (submission, requests) = mock_server(vec![
        (
            200,
            serde_json::json!({"AccessToken": "new-token", "User": {"Id": "new-user"}}),
        ),
        (
            200,
            serde_json::json!({"ServerName": "Alpha TV", "Id": "new-server"}),
        ),
    ]);
    let server = CachedServer {
        server_name: Some("Old server".into()),
        needs_auth_refresh: true,
        ..pending_server(&submission)
    };
    let authenticated =
        authenticate_server(&EmbyClient::new("test".into()).unwrap(), &server).unwrap();
    assert_eq!(authenticated.server_name.as_deref(), Some("Alpha TV"));
    assert_eq!(authenticated.server_id.as_deref(), Some("new-server"));
    assert!(
        authenticated
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/AlphaTV-emby.png")
    );
    assert!(!authenticated.needs_auth_refresh);
    let requests = requests.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /emby/Users/AuthenticateByName "));
    assert!(requests[1].starts_with("GET /emby/System/Info/Public "));
}

#[test]
fn rejected_or_incomplete_authentication_cannot_create_a_cached_server() {
    for (status, body) in [
        (401, serde_json::json!({"Message": "Invalid credentials"})),
        (200, serde_json::json!({"AccessToken": "token"})),
        (200, auth_response("UHD", " ")),
    ] {
        let (submission, requests) = mock_server(vec![(status, body)]);
        assert!(
            authenticate_server(
                &EmbyClient::new("test".into()).unwrap(),
                &pending_server(&submission)
            )
            .is_err()
        );
        assert_eq!(requests.join().unwrap().len(), 1);
    }
}

fn saved_server() -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "local-id", "server_name": "UHD", "server_id": "remote-id",
        "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
        "username": "test", "password": "", "user_id": "user", "access_token": "saved-token",
        "icon_url": "https://example.com/icon.png", "added_at_unix": 123
    }))
    .unwrap()
}

#[gpui::test]
fn saves_auth_and_icon_preserves_latest_settings_and_rolls_back_failed_edits(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    cx.update(theme::init);
    let app = cx.new(|cx| {
        let mut app = TinyApp::new(ServerCache::empty(), None, cx);
        app.cache_save_path = Some(path.clone());
        app
    });
    app.update(cx, |app, _| {
        app.cache.set_window_size(1300, 900);
        let id = app.save_server(saved_server(), false).unwrap();
        assert_eq!(id, "local-id");
        let loaded = storage::load_or_init_from(&path).unwrap();
        assert_eq!(
            loaded.servers[0].access_token.as_deref(),
            Some("saved-token")
        );
        assert_eq!(loaded.servers[0].icon_url, saved_server().icon_url);
        assert_eq!(loaded.window_size().unwrap().width, 1300);

        let mut duplicate = saved_server();
        duplicate.id = "duplicate-id".into();
        assert_eq!(app.save_server(duplicate, false).unwrap(), "local-id");
        assert_eq!(app.cache.servers.len(), 1);

        // A parent that is a file makes saving fail without touching the old cache.
        app.cache_save_path = Some(path.join("invalid.json"));
        let mut edited = saved_server();
        edited.access_token = Some("new-token".into());
        edited.icon_url = None;
        assert!(app.save_server(edited, true).is_err());
        assert_eq!(
            app.cache.servers[0].access_token.as_deref(),
            Some("saved-token")
        );
        assert_eq!(app.cache.servers[0].icon_url, saved_server().icon_url);
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().servers[0].icon_url,
            saved_server().icon_url
        );
    });
}

#[gpui::test]
fn editing_preserves_cache_updates_received_while_the_dialog_was_open(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let app = cx.new(|cx| {
        let mut cache = ServerCache::empty();
        cache.servers.push(saved_server());
        let mut app = TinyApp::new(cache, None, cx);
        app.cache_save_path = Some(path.clone());
        app
    });
    app.update(cx, |app, cx| {
        let dialog = cx.new(|cx| AddServerDialogState::new_edit(&app.servers[0], cx));
        app.add_server_dialog = Some(dialog.clone());
        let edited = CachedServer {
            password: "edited-password".into(),
            ..app.servers[0].clone()
        };
        // A background refresh completes after the edited snapshot was taken.
        let counts = CachedItemCounts {
            movie_count: 123,
            series_count: 456,
        };
        app.cache.servers[0].item_counts = Some(counts.clone());
        app.servers[0].item_counts = Some(counts.clone());
        app.item_counts
            .insert(edited.id.clone(), crate::emby::ItemCounts::from(&counts));
        app.finish_save_server(dialog, Ok(edited), cx);
        assert!(app.add_server_dialog.is_none());
        assert!(app.servers[0].needs_auth_refresh);
        assert_eq!(app.servers[0].password, "edited-password");
        assert_eq!(app.servers[0].access_token.as_deref(), Some("saved-token"));
        assert_eq!(app.servers[0].item_counts.as_ref(), Some(&counts));
        assert_eq!(app.item_counts[&app.servers[0].id].movie_count, 123);
        assert_eq!(
            storage::load_or_init_from(&path).unwrap().servers[0].item_counts,
            Some(counts)
        );
    });
}

#[gpui::test]
fn failed_server_save_keeps_the_edit_dialog_and_saved_login(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let app = cx.new(|cx| {
        let mut cache = ServerCache::empty();
        cache.servers.push(saved_server());
        storage::save_to(&cache, &path).unwrap();
        let mut app = TinyApp::new(cache, None, cx);
        app.cache_save_path = Some(path.join("invalid.json"));
        app
    });
    app.update(cx, |app, cx| {
        let dialog = cx.new(|cx| AddServerDialogState::new_edit(&app.servers[0], cx));
        app.add_server_dialog = Some(dialog.clone());
        let edited = CachedServer {
            password: "edited-password".into(),
            ..app.servers[0].clone()
        };
        app.finish_save_server(dialog, Ok(edited), cx);
        assert!(app.add_server_dialog.is_some());
        assert_eq!(app.servers[0].password, saved_server().password);
        assert!(!app.servers[0].needs_auth_refresh);
        assert_eq!(app.servers[0].access_token.as_deref(), Some("saved-token"));
        assert_eq!(app.cache.servers[0].icon_url, saved_server().icon_url);
        assert!(!storage::load_or_init_from(&path).unwrap().servers[0].needs_auth_refresh);
    });
}
