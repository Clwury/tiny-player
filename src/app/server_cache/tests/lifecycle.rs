use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use gpui::Entity;

use super::*;

struct MockEmby {
    submission: AddServerSubmission,
    auth_count: Arc<AtomicUsize>,
    auth_passwords: Arc<Mutex<Vec<String>>>,
    reject_next_auth: Arc<AtomicBool>,
    reject_next_counts: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MockEmby {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth_count = Arc::new(AtomicUsize::new(0));
        let auth_passwords = Arc::new(Mutex::new(Vec::new()));
        let reject_next_auth = Arc::new(AtomicBool::new(false));
        let reject_next_counts = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn({
            let auth_count = auth_count.clone();
            let auth_passwords = auth_passwords.clone();
            let reject_next_auth = reject_next_auth.clone();
            let reject_next_counts = reject_next_counts.clone();
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = read_request(&mut stream);
                    let (status, body) = if request.starts_with("GET /emby/System/Info/Public ") {
                        (
                            200,
                            serde_json::json!({"ServerName": "UHD", "Id": "remote-server"}),
                        )
                    } else if request.starts_with("POST /emby/Users/AuthenticateByName ") {
                        let body: serde_json::Value =
                            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1)
                                .unwrap();
                        auth_passwords
                            .lock()
                            .unwrap()
                            .push(body["Pw"].as_str().unwrap().into());
                        let count = auth_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if reject_next_auth.swap(false, Ordering::SeqCst) {
                            (401, serde_json::json!({"Message": "Invalid credentials"}))
                        } else {
                            let name = if count == 1 { "UHD" } else { "Alpha TV" };
                            (200, auth_response(name, &format!("token-{count}")))
                        }
                    } else if request.starts_with("GET /emby/Items/Counts ") {
                        if reject_next_counts.swap(false, Ordering::SeqCst) {
                            (503, serde_json::json!({"Message": "Unavailable"}))
                        } else {
                            (
                                200,
                                serde_json::json!({"MovieCount": 15296, "SeriesCount": 13625}),
                            )
                        }
                    } else {
                        // Home requests are unrelated to the authentication lifecycle.
                        (404, serde_json::json!({}))
                    };
                    let body = body.to_string();
                    write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        Self {
            submission: AddServerSubmission {
                endpoint: ServerEndpoint {
                    protocol: Protocol::Http,
                    address: "127.0.0.1".into(),
                    port,
                    path: String::new(),
                },
                username: "test-user".into(),
                password: "first-password".into(),
            },
            auth_count,
            auth_passwords,
            reject_next_auth,
            reject_next_counts,
            stop,
            worker: Some(worker),
        }
    }

    fn auth_count(&self) -> usize {
        self.auth_count.load(Ordering::SeqCst)
    }
}

impl Drop for MockEmby {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn app_with_cache(
    cx: &mut TestAppContext,
    cache: ServerCache,
    path: &std::path::Path,
) -> Entity<TinyApp> {
    cx.new(|cx| {
        let mut app = TinyApp::new(cache, None, cx);
        app.cache_save_path = Some(path.to_path_buf());
        app
    })
}

#[gpui::test]
fn first_entry_and_entry_after_edit_authenticate_once_and_later_entries_reuse_the_cache(
    cx: &mut TestAppContext,
) {
    let mock = MockEmby::new();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    let client = EmbyClient::new("test-device".into()).unwrap();
    cx.update(theme::init);
    let app = app_with_cache(cx, ServerCache::empty(), &path);
    let pending = prepare_server(&client, &mock.submission, None).unwrap();
    app.update(cx, |app, cx| {
        let dialog = cx.new(AddServerDialogState::new);
        app.add_server_dialog = Some(dialog.clone());
        app.finish_save_server(dialog, Ok(pending.clone()), cx);
        assert!(app.add_server_dialog.is_none());
        assert!(matches!(app.page, Page::Servers));
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 0);
    let saved = storage::load_or_init_from(&path).unwrap();
    assert!(
        saved.servers[0]
            .icon_url
            .as_deref()
            .unwrap()
            .ends_with("/UHD-emby.png")
    );
    assert!(
        storage::load_or_init_from(&path).unwrap().servers[0]
            .access_token
            .is_none()
    );

    app.update(cx, |app, cx| {
        app.begin_select_server(&pending, cx);
        app.begin_select_server(&pending, cx);
        assert_eq!(
            app.selecting_server_id.as_deref(),
            Some(pending.id.as_str())
        );
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 1);
    app.read_with(cx, |app, _| {
        assert!(matches!(app.page, Page::Home(_)));
        assert!(app.selecting_server_id.is_none());
        assert_eq!(app.servers[0].access_token.as_deref(), Some("token-1"));
        assert!(!app.servers[0].needs_auth_refresh);
        assert!(
            app.servers[0]
                .icon_url
                .as_deref()
                .unwrap()
                .ends_with("/UHD-emby.png")
        );
        assert_eq!(app.item_counts[&pending.id].movie_count, 15296);
    });

    app.update(cx, |app, cx| {
        app.show_servers_page_from_home(cx);
        // An old card snapshot must still reuse the latest saved token.
        app.begin_select_server(&pending, cx);
        app.flush_scheduled_cache_save(cx);
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 1);
    drop(app);
    cx.run_until_parked();

    let cache = storage::load_or_init_from(&path).unwrap();
    let previously_authenticated = cache.servers[0].clone();
    let app = app_with_cache(cx, cache, &path);
    app.update(cx, |app, cx| {
        app.begin_select_server(&previously_authenticated, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        mock.auth_count(),
        1,
        "restarting must reuse the saved login"
    );

    let edited_submission = AddServerSubmission {
        password: "edited-password".into(),
        ..mock.submission.clone()
    };
    let edited =
        prepare_server(&client, &edited_submission, Some(&previously_authenticated)).unwrap();
    app.update(cx, |app, cx| {
        let dialog = cx.new(|cx| AddServerDialogState::new_edit(&previously_authenticated, cx));
        app.add_server_dialog = Some(dialog.clone());
        app.finish_save_server(dialog, Ok(edited), cx);
        assert_eq!(
            app.servers[0].access_token,
            previously_authenticated.access_token
        );
        assert_eq!(app.servers[0].user_id, previously_authenticated.user_id);
        assert_eq!(
            app.servers[0].server_name,
            previously_authenticated.server_name
        );
        assert!(app.servers[0].needs_auth_refresh);
        assert!(
            app.servers[0]
                .icon_url
                .as_deref()
                .unwrap()
                .ends_with("/UHD-emby.png")
        );
        assert_eq!(app.item_counts[&pending.id].movie_count, 15296);
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 1, "saving an edit must not authenticate");
    let cache = storage::load_or_init_from(&path).unwrap();
    assert!(cache.servers[0].needs_auth_refresh);
    assert_eq!(
        cache.servers[0].access_token,
        previously_authenticated.access_token
    );
    assert_eq!(
        cache.servers[0].item_counts,
        previously_authenticated.item_counts
    );
    drop(app);
    cx.run_until_parked();
    let app = app_with_cache(cx, cache, &path);

    app.update(cx, |app, cx| {
        app.begin_select_server(&previously_authenticated, cx)
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 2);
    app.read_with(cx, |app, _| {
        assert!(matches!(app.page, Page::Home(_)));
        assert_eq!(app.servers[0].id, pending.id);
        assert_eq!(app.servers[0].access_token.as_deref(), Some("token-2"));
        assert!(!app.servers[0].needs_auth_refresh);
        assert!(
            app.servers[0]
                .icon_url
                .as_deref()
                .unwrap()
                .ends_with("/AlphaTV-emby.png")
        );
    });
    app.update(cx, |app, cx| {
        app.show_servers_page_from_home(cx);
        app.begin_select_server(&previously_authenticated, cx);
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 2);
    assert_eq!(
        *mock.auth_passwords.lock().unwrap(),
        ["first-password", "edited-password"]
    );
}

#[gpui::test]
fn failed_entry_preserves_pending_and_existing_caches_and_can_be_retried(cx: &mut TestAppContext) {
    cx.update(theme::init);
    for after_edit in [false, true] {
        let mock = MockEmby::new();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let server = if after_edit {
            CachedServer {
                endpoint: mock.submission.endpoint.clone(),
                password: "edited-password".into(),
                needs_auth_refresh: true,
                item_counts: Some(CachedItemCounts {
                    movie_count: 10,
                    series_count: 20,
                }),
                ..saved_server()
            }
        } else {
            pending_server(&mock.submission)
        };
        let mut cache = ServerCache::empty();
        cache.servers.push(server.clone());
        storage::save_to(&cache, &path).unwrap();
        let app = app_with_cache(cx, cache, &path);
        mock.reject_next_auth.store(true, Ordering::SeqCst);
        app.update(cx, |app, cx| app.begin_select_server(&server, cx));
        cx.run_until_parked();
        assert_eq!(mock.auth_count(), 1);
        let before = serde_json::to_value(&server).unwrap();
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Servers));
            assert!(app.selecting_server_id.is_none());
            assert!(app.has_server_page_notifications());
            assert_eq!(serde_json::to_value(&app.servers[0]).unwrap(), before);
            assert_eq!(serde_json::to_value(&app.cache.servers[0]).unwrap(), before);
            if after_edit {
                assert_eq!(app.item_counts[&server.id].movie_count, 10);
                assert_eq!(app.item_counts[&server.id].series_count, 20);
            }
        });
        assert_eq!(
            serde_json::to_value(&storage::load_or_init_from(&path).unwrap().servers[0]).unwrap(),
            before
        );
        app.update(cx, |app, cx| app.begin_select_server(&server, cx));
        cx.run_until_parked();
        assert_eq!(mock.auth_count(), 2);
        app.read_with(cx, |app, _| {
            assert!(matches!(app.page, Page::Home(_)));
            assert!(!app.servers[0].needs_auth_refresh);
            assert_eq!(app.servers[0].access_token.as_deref(), Some("token-2"));
            assert_eq!(app.item_counts[&server.id].movie_count, 15296);
        });
        drop(app);
        cx.run_until_parked();
    }
}

#[gpui::test]
fn count_refresh_failure_after_edit_preserves_cached_totals_until_success(cx: &mut TestAppContext) {
    let mock = MockEmby::new();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("servers.json");
    cx.update(theme::init);
    let server = CachedServer {
        endpoint: mock.submission.endpoint.clone(),
        needs_auth_refresh: true,
        item_counts: Some(CachedItemCounts {
            movie_count: 10,
            series_count: 20,
        }),
        ..saved_server()
    };
    let mut cache = ServerCache::empty();
    cache.servers.push(server.clone());
    storage::save_to(&cache, &path).unwrap();
    let app = app_with_cache(cx, cache, &path);
    mock.reject_next_counts.store(true, Ordering::SeqCst);
    app.update(cx, |app, cx| {
        app.begin_select_server(&server, cx);
        assert_eq!(app.item_counts[&server.id].movie_count, 10);
    });
    cx.run_until_parked();
    assert_eq!(mock.auth_count(), 1);
    app.read_with(cx, |app, _| {
        assert!(matches!(app.page, Page::Home(_)));
        assert!(!app.servers[0].needs_auth_refresh);
        assert_eq!(app.servers[0].access_token.as_deref(), Some("token-1"));
        assert!(app.item_counts_failed.contains(&server.id));
        assert_eq!(app.item_counts[&server.id].movie_count, 10);
        assert_eq!(app.item_counts[&server.id].series_count, 20);
        assert_eq!(app.cache.servers[0].item_counts, server.item_counts);
    });
    assert_eq!(
        storage::load_or_init_from(&path).unwrap().servers[0].item_counts,
        server.item_counts
    );
    app.update(cx, |app, cx| {
        app.refresh_saved_server_counts(&server.id, cx);
        assert_eq!(app.item_counts[&server.id].movie_count, 10);
    });
    cx.run_until_parked();
    app.update(cx, |app, cx| {
        assert!(!app.item_counts_failed.contains(&server.id));
        assert_eq!(app.item_counts[&server.id].movie_count, 15296);
        assert_eq!(app.item_counts[&server.id].series_count, 13625);
        app.flush_scheduled_cache_save(cx);
    });
    assert_eq!(
        storage::load_or_init_from(&path).unwrap().servers[0]
            .item_counts
            .as_ref()
            .unwrap()
            .movie_count,
        15296
    );
}
