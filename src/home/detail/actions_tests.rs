use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, px, size};
use serde_json::json;

use super::*;
use crate::{emby::EmbyClient, theme};

struct MockEmby {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    fail_next: Arc<AtomicBool>,
    fail_path: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MockEmby {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let fail_next = Arc::new(AtomicBool::new(false));
        let fail_path: Arc<Mutex<Option<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn({
            let requests = requests.clone();
            let fail_next = fail_next.clone();
            let fail_path = fail_path.clone();
            let stop = stop.clone();
            move || {
                let mut data: HashMap<String, UserItemData> =
                    ["series-1", "episode-1", "episode-2"]
                        .into_iter()
                        .map(|id| (id.into(), UserItemData::default()))
                        .collect();
                while !stop.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 1024];
                    while !bytes.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let count = stream.read(&mut buffer).unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    let request = String::from_utf8(bytes).unwrap();
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains("x-emby-token: test-token")
                    );
                    let line = request.lines().next().unwrap().to_string();
                    let parts: Vec<_> = line.split_whitespace().collect();
                    let method = parts[0];
                    let path = parts[1];
                    let id = path.rsplit('/').next().unwrap();
                    requests.lock().unwrap().push(line.clone());
                    let fail_refresh = {
                        let mut fail_path = fail_path.lock().unwrap();
                        if fail_path
                            .as_ref()
                            .is_some_and(|prefix| path.starts_with(prefix))
                        {
                            fail_path.take();
                            true
                        } else {
                            false
                        }
                    };
                    let (status, body) = if fail_next.swap(false, Ordering::SeqCst) || fail_refresh
                    {
                        ("500 Internal Server Error", "rejected".into())
                    } else if path.contains("/FavoriteItems/") {
                        data.get_mut(id).unwrap().is_favorite = method == "POST";
                        ("200 OK", serde_json::to_string(&data[id]).unwrap())
                    } else if path.contains("/PlayedItems/") {
                        let played = method == "POST";
                        if id == "series-1" {
                            for value in data.values_mut() {
                                value.played = played;
                                value.played_percentage = None;
                                value.playback_position_ticks = Some(0);
                            }
                        } else {
                            let value = data.get_mut(id).unwrap();
                            value.played = played;
                            value.played_percentage = played.then_some(23.5);
                            value.playback_position_ticks = played.then_some(47_000_000);
                            data.get_mut("series-1").unwrap().played =
                                data["episode-1"].played && data["episode-2"].played;
                        }
                        ("200 OK", serde_json::to_string(&data[id]).unwrap())
                    } else if method == "GET" && path.ends_with("/Items/series-1") {
                        ("200 OK", json!({"Id":"series-1", "Name":"Series", "Type":"Series", "UserData":data["series-1"]}).to_string())
                    } else if method == "GET" && path.starts_with("/emby/Shows/series-1/Episodes?")
                    {
                        ("200 OK", json!({"Items": [
                            {"Id":"episode-1","Name":"First","Type":"Episode","SeriesId":"series-1","SeasonId":"season-1","IndexNumber":1,"UserData":data["episode-1"]},
                            {"Id":"episode-2","Name":"Second","Type":"Episode","SeriesId":"series-1","SeasonId":"season-1","IndexNumber":2,"UserData":data["episode-2"]}
                        ], "TotalRecordCount":2}).to_string())
                    } else if method == "GET" && path.contains("/Items/episode-") {
                        let mut user_data = data[id].clone();
                        // Distinct from Episodes: the selected item's response must win.
                        user_data.played_percentage = Some(37.5);
                        user_data.playback_position_ticks = Some(75_000_000);
                        ("200 OK", json!({"Id":id,"Name":"Refreshed Episode","Type":"Episode","SeriesId":"series-1","SeasonId":"season-1","Overview":"Refreshed overview","UserData":user_data}).to_string())
                    } else {
                        ("404 Not Found", "unexpected request".into())
                    };
                    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        Self {
            port,
            requests,
            fail_next,
            fail_path,
            stop,
            worker: Some(worker),
        }
    }

    fn mutations(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.starts_with("GET"))
            .cloned()
            .collect()
    }
}

impl Drop for MockEmby {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn detail_window<'a>(
    cx: &'a mut TestAppContext,
    server: &MockEmby,
) -> (Entity<HomeContent>, &'a mut VisualTestContext) {
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id":"detail-actions-test", "user_id":"user-1", "access_token":"test-token",
            "endpoint":{"protocol":"Http", "address":"127.0.0.1", "port":server.port, "path":""},
            "username":"test", "password":"", "added_at_unix":0
        })).unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        let item = json!({"Id":"series-1", "Name":"Series", "Type":"Series", "UserData":{"Played":false,"IsFavorite":false}});
        let mut detail = SeriesDetailState::from_user_item(&serde_json::from_value(item.clone()).unwrap()).unwrap();
        detail.item = Some(serde_json::from_value(item).unwrap());
        detail.episodes = Some(serde_json::from_value(json!({"Items":[
            {"Id":"episode-1","Name":"First","Type":"Episode","SeriesId":"series-1","IndexNumber":1,"UserData":{"Played":false,"IsFavorite":false}},
            {"Id":"episode-2","Name":"Second","Type":"Episode","SeriesId":"series-1","IndexNumber":2,"UserData":{"Played":false,"IsFavorite":false}}
        ]})).unwrap());
        detail.selected_episode_id = Some("episode-1".into());
        detail.selected_season_id = Some("season-1".into());
        page.navigation.push_detail("series-1".into(), None);
        page.series_detail = Some(detail);
        page
    });
    cx.simulate_resize(size(px(1200.0), px(1000.0)));
    cx.run_until_parked();
    (page, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn episode_buttons_use_selected_id_and_series_menu_uses_series_id(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    let favorite = cx.debug_bounds("series-detail-favorite-button").unwrap();
    let played = cx.debug_bounds("series-detail-played-button").unwrap();
    let more = cx.debug_bounds("series-detail-more-button").unwrap();
    assert!(favorite.right() <= played.left() && played.right() <= more.left());
    assert!(cx.debug_bounds("episode-watched").is_none());
    click(cx, "series-detail-favorite-button");
    page.update(cx, |page, cx| {
        page.select_series_episode("episode-2".into(), cx)
    });
    cx.run_until_parked();
    click(cx, "series-detail-favorite-button");
    click(cx, "series-detail-played-button");
    page.read_with(cx, |page, _| {
        assert!(page.user_data_overrides["episode-1"].is_favorite);
        assert!(!page.user_data_overrides["episode-1"].played);
        assert!(page.user_data_overrides["episode-2"].is_favorite);
        assert!(page.user_data_overrides["episode-2"].played);
        assert!(!page.user_data_overrides["series-1"].is_favorite);
        assert!(!page.user_data_overrides["series-1"].played);
    });
    assert!(cx.debug_bounds("episode-watched").is_some());
    assert!(cx.debug_bounds("episode-unwatched").is_none());
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-favorite");
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-played");
    assert!(cx.debug_bounds("episode-unwatched").is_none());
    page.read_with(cx, |page, _| {
        for id in ["series-1", "episode-1", "episode-2"] {
            assert!(page.user_data_overrides[id].played);
            assert!(page.user_data_overrides[id].is_favorite);
        }
    });
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-favorite");
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-played");
    assert!(cx.debug_bounds("episode-watched").is_none());
    click(cx, "series-detail-played-button");
    click(cx, "series-detail-played-button");
    click(cx, "series-detail-favorite-button");
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
            "POST /emby/Users/user-1/FavoriteItems/episode-2 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/episode-2 HTTP/1.1",
            "POST /emby/Users/user-1/FavoriteItems/series-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/series-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/episode-2 HTTP/1.1",
            "DELETE /emby/Users/user-1/PlayedItems/episode-2 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/episode-2 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn episode_played_uses_mutation_response_without_fetching_episode_items(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    for played in [true, false] {
        let start = server.requests.lock().unwrap().len();
        click(cx, "series-detail-played-button");
        assert_eq!(
            server.requests.lock().unwrap()[start..],
            [
                format!(
                    "{} /emby/Users/user-1/PlayedItems/episode-1 HTTP/1.1",
                    if played { "POST" } else { "DELETE" }
                ),
                "GET /emby/Users/user-1/Items/series-1 HTTP/1.1".into(),
            ]
        );
        page.read_with(cx, |page, _| {
            let detail = page.series_detail.as_ref().unwrap();
            assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-1"));
            let episode = detail.selected_episode().unwrap();
            let data = episode.user_data.as_ref().unwrap();
            assert_eq!(data.played, played);
            assert_eq!(data.played_percentage, played.then_some(23.5));
            assert_eq!(data.playback_position_ticks, played.then_some(47_000_000));
            assert_eq!(
                page.user_data_overrides["episode-1"].played_percentage,
                played.then_some(23.5)
            );
            let other = &detail.episodes.as_ref().unwrap().items[1];
            assert!(!other.user_data.as_ref().unwrap().played);
            assert_eq!(other.played_percentage(), None);
        });
        assert_eq!(cx.debug_bounds("episode-watched").is_some(), played);
    }
}

#[gpui::test]
fn failed_requests_preserve_state_and_menu_dismisses_without_mutating(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-more-button");
    assert!(cx.debug_bounds("series-detail-actions-menu").is_none());
    click(cx, "series-detail-more-button");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("series-detail-actions-menu").is_none());
    click(cx, "series-detail-more-button");
    cx.simulate_click(gpui::point(px(1150.0), px(930.0)), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("series-detail-actions-menu").is_none());
    assert!(server.mutations().is_empty());
    for selector in [
        "series-detail-favorite-button",
        "series-detail-played-button",
    ] {
        server.fail_next.store(true, Ordering::SeqCst);
        click(cx, selector);
        page.read_with(cx, |page, _| {
            assert!(!page.detail_user_data_pending());
            let detail = page.series_detail.as_ref().unwrap();
            let item = detail.selected_playback_item().unwrap();
            let data = page
                .effective_user_data(&item.id, item.user_data.as_ref())
                .unwrap();
            assert!(!data.played && !data.is_favorite);
        });
    }
}

#[gpui::test]
fn series_played_refreshes_both_endpoints_and_preserves_server_progress(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    page.update(cx, |page, cx| {
        page.select_series_episode("episode-2".into(), cx)
    });
    cx.run_until_parked();
    for played in [true, false] {
        let start = server.requests.lock().unwrap().len();
        click(cx, "series-detail-more-button");
        click(cx, "series-detail-series-played");
        let requests = server.requests.lock().unwrap()[start..].to_vec();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0],
            format!(
                "{} /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
                if played { "POST" } else { "DELETE" }
            )
        );
        assert!(requests[1].starts_with("GET /emby/Shows/series-1/Episodes?"));
        assert!(requests[1].contains("SeasonId=season-1"));
        assert_eq!(
            requests[2],
            "GET /emby/Users/user-1/Items/episode-2 HTTP/1.1"
        );
        page.update(cx, |page, cx| {
            let detail = page.series_detail.as_ref().unwrap();
            assert_eq!(detail.selected_season_id.as_deref(), Some("season-1"));
            assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
            let episodes = &detail.episodes.as_ref().unwrap().items;
            assert_eq!(episodes[0].user_data.as_ref().unwrap().played_percentage, None);
            assert_eq!(episodes[0].user_data.as_ref().unwrap().played, played);
            let selected = detail.selected_episode().unwrap();
            assert_eq!(selected.name, "Refreshed Episode");
            assert_eq!(selected.overview.as_deref(), Some("Refreshed overview"));
            assert_eq!(selected.user_data.as_ref().unwrap().played, played);
            assert_eq!(selected.played_percentage(), Some(37.5));
            assert_eq!(detail.playback_position_ticks(), Some(75_000_000));
            assert_eq!(page.user_data_overrides["episode-2"].played_percentage, Some(37.5));
            let stale = serde_json::from_value(json!({"Items":[{"Id":"episode-2","Name":"Stale","Type":"Episode","SeriesId":"series-1","UserData":{"Played":!played,"PlayedPercentage":100.0}}], "TotalRecordCount":1})).unwrap();
            page.absorb_user_items_user_data(&stale, 0);
            assert_eq!(page.user_data_overrides["episode-2"].played, played);
            assert_eq!(page.user_data_overrides["episode-2"].played_percentage, Some(37.5));
            page.finish_series_episodes(
                page.request_identity(),
                DetailRequestRevisions { detail: page.detail_generation, user_data: 0 },
                "series-1".into(), "season-1".into(),
                Ok(serde_json::from_value(json!({"Items":[]})).unwrap()), cx,
            );
            assert_eq!(page.series_detail.as_ref().unwrap().episodes.as_ref().unwrap().items.len(), 2);
        });
    }
}

#[gpui::test]
fn a_failed_series_refresh_still_fetches_the_other_endpoint(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    *server.fail_path.lock().unwrap() = Some("/emby/Shows/series-1/Episodes?".into());
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-played");
    page.read_with(cx, |page, _| {
        assert!(!page.detail_user_data_pending());
        assert!(page.user_data_overrides["series-1"].played);
        let detail = page.series_detail.as_ref().unwrap();
        assert_eq!(
            detail.selected_episode().unwrap().played_percentage(),
            Some(37.5)
        );
        assert_eq!(
            detail.episodes.as_ref().unwrap().items[1].played_percentage(),
            None
        );
        assert!(!page.user_data_overrides.contains_key("episode-2"));
    });
    *server.fail_path.lock().unwrap() = Some("/emby/Users/user-1/Items/episode-1".into());
    click(cx, "series-detail-more-button");
    click(cx, "series-detail-series-played");
    page.read_with(cx, |page, _| {
        assert!(!page.detail_user_data_pending());
        let detail = page.series_detail.as_ref().unwrap();
        for episode in &detail.episodes.as_ref().unwrap().items {
            assert!(!episode.user_data.as_ref().unwrap().played);
            assert_eq!(episode.played_percentage(), None);
        }
    });
}

#[gpui::test]
fn a_series_refresh_does_not_replace_a_newly_selected_season(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = detail_window(cx, &server);
    page.update(cx, |page, cx| {
        let request = PlayedRequest {
            item_id: "series-1".into(), series_id: Some("series-1".into()),
            whole_series: true, played: true, season_id: Some("season-1".into()),
            episode_id: Some("episode-1".into()), detail_generation: page.detail_generation,
        };
        let detail = page.series_detail.as_mut().unwrap();
        detail.selected_season_id = Some("season-2".into());
        detail.selected_episode_id = Some("episode-3".into());
        detail.episodes = Some(serde_json::from_value(json!({"Items":[{"Id":"episode-3","Name":"Third","Type":"Episode","UserData":{"Played":false}}]})).unwrap());
        let episode: MediaItem = serde_json::from_value(json!({"Id":"episode-1","Name":"Refreshed","Type":"Episode","UserData":{"Played":true,"PlayedPercentage":18.0}})).unwrap();
        page.apply_series_played_refresh(&request,
            Some(Ok(MediaItems { items: vec![episode.clone()], total_record_count: 1 })), Some(Ok(episode)), cx);
        let detail = page.series_detail.as_ref().unwrap();
        assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
        assert_eq!(detail.selected_episode().unwrap().id, "episode-3");
        assert_eq!(detail.episodes.as_ref().unwrap().items.len(), 1);
        assert_eq!(page.user_data_overrides["episode-1"].played_percentage, Some(18.0));
    });
}
