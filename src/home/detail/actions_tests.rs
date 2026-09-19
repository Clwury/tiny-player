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

use gpui::{Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext, px, size};
use serde_json::json;

use super::*;
use crate::{
    emby::{EmbyClient, SortOrder, UserItemsSort, VideoItemType},
    home::{
        UserViewItemsRow, library::LibraryState, navigation::HomeRoot, paged_items::PagedItemsState,
    },
    theme,
};

#[path = "workspace_menu_tests.rs"]
mod workspace_menu_tests;

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
        Self::with_favorites(false)
    }

    fn with_favorites(is_favorite: bool) -> Self {
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
                    ["series-1", "episode-1", "episode-2", "movie-1"]
                        .into_iter()
                        .map(|id| {
                            (
                                id.into(),
                                UserItemData {
                                    is_favorite,
                                    ..UserItemData::default()
                                },
                            )
                        })
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
                    // Winsock inherits the listener's nonblocking mode; these
                    // request/response reads use a blocking socket with a timeout.
                    stream.set_nonblocking(false).unwrap();
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
                            for (_, value) in
                                data.iter_mut().filter(|(key, _)| key.as_str() != "movie-1")
                            {
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
                    } else if path.ends_with("/HideFromResume?Hide=true") {
                        ("204 No Content", String::new())
                    } else if method == "GET"
                        && path.starts_with("/emby/Users/user-1/Items?")
                        && path.contains("Filters=IsFavorite")
                    {
                        let url = url::Url::parse(&format!("http://localhost{path}")).unwrap();
                        let item_type = url
                            .query_pairs()
                            .find(|(key, _)| key == "IncludeItemTypes")
                            .unwrap()
                            .1;
                        let items: Vec<_> = [
                            ("movie-1", "Movie"), ("series-1", "Series"),
                            ("episode-1", "Episode"), ("episode-2", "Episode"),
                        ].into_iter().filter(|(id, kind)| *kind == item_type && data[*id].is_favorite)
                            .map(|(id, kind)| json!({"Id":id,"Name":id,"Type":kind,"SeriesId":(kind == "Episode").then_some("series-1"),"UserData":data[id]}))
                            .collect();
                        (
                            "200 OK",
                            json!({"TotalRecordCount":items.len(),"Items":items}).to_string(),
                        )
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

fn home_menu_window<'a>(
    cx: &'a mut TestAppContext,
    server: &MockEmby,
) -> (Entity<HomeContent>, &'a mut VisualTestContext) {
    let (page, cx) = detail_window(cx, server);
    page.update(cx, |page, cx| {
        page.navigation.select_root(HomeRoot::Home);
        page.series_detail = None;
        page.user_views = Some(serde_json::from_value(json!({"Items": [
            {"Id": "videos", "Name": "Videos", "CollectionType": "movies"}
        ], "TotalRecordCount": 1})).unwrap());
        let items = json!([
            {"Id":"movie-1", "Name":"Movie", "Type":"Movie", "UserData":{"Played":false,"IsFavorite":false}},
            {"Id":"series-1", "Name":"Series", "Type":"Series", "UserData":{"Played":false,"IsFavorite":false}},
            {"Id":"episode-1", "Name":"First", "Type":"Episode", "SeriesId":"series-1", "UserData":{"Played":false,"IsFavorite":false}},
            {"Id":"episode-2", "Name":"Second", "Type":"Episode", "SeriesId":"series-1", "UserData":{"Played":false,"IsFavorite":false}}
        ]);
        page.user_view_items_rows.insert("videos".into(), UserViewItemsRow {
            items: Some(serde_json::from_value(json!({"Items": items, "TotalRecordCount":4})).unwrap()),
            ..UserViewItemsRow::default()
        });
        page.resume_items = Some(serde_json::from_value(json!({
            "Items": [items[0].clone(), items[2].clone(), items[3].clone()], "TotalRecordCount": 3
        })).unwrap());
        cx.notify();
    });
    cx.simulate_resize(size(px(1200.0), px(1100.0)));
    cx.run_until_parked();
    (page, cx)
}

fn right_click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(bounds.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn cover_and_resume_menus_toggle_the_clicked_item_favorite_without_removing_it(
    cx: &mut TestAppContext,
) {
    let server = MockEmby::new();
    let (page, cx) = home_menu_window(cx, &server);
    for (card, option, desired) in [
        (
            "user-view-item-card-movie-1",
            "user-view-item-favorite",
            true,
        ),
        (
            "user-view-item-card-movie-1",
            "user-view-item-favorite",
            false,
        ),
        ("resume-item-card-episode-1", "resume-item-favorite", true),
        ("resume-item-card-episode-1", "resume-item-favorite", false),
    ] {
        right_click(cx, card);
        click(cx, option);
        page.read_with(cx, |page, _| {
            let id = if card.ends_with("movie-1") {
                "movie-1"
            } else {
                "episode-1"
            };
            assert_eq!(page.user_item_by_id(id).unwrap().is_favorite(), desired);
            assert_eq!(
                page.resume_item_by_id(id)
                    .unwrap()
                    .user_data
                    .unwrap()
                    .is_favorite,
                desired
            );
            assert!(page.item_context_menu.is_none());
            assert!(page.favorite_requests.is_empty());
            assert_eq!(page.resume_items.as_ref().unwrap().total_record_count, 3);
            assert!(page.favorites[VideoItemType::Movie].paged.dirty);
        });
    }
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/FavoriteItems/movie-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
            "DELETE /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn cover_menu_marks_movies_episodes_and_whole_series_played_and_updates_resume(
    cx: &mut TestAppContext,
) {
    let server = MockEmby::new();
    let (page, cx) = home_menu_window(cx, &server);
    right_click(cx, "user-view-item-card-episode-1");
    click(cx, "user-view-item-favorite");
    for (id, card, remaining) in [
        ("movie-1", "user-view-item-card-movie-1", 2),
        ("movie-1", "user-view-item-card-movie-1", 2),
        ("episode-1", "user-view-item-card-episode-1", 1),
        ("series-1", "user-view-item-card-series-1", 0),
    ] {
        right_click(cx, card);
        click(cx, "user-view-item-mark-played");
        page.read_with(cx, |page, _| {
            assert!(page.user_data_overrides[id].played);
            assert!(page.user_item_by_id(id).unwrap().user_data.unwrap().played);
            assert_eq!(page.resume_items.as_ref().unwrap().items.len(), remaining);
            assert_eq!(
                page.resume_items.as_ref().unwrap().total_record_count as usize,
                remaining
            );
            assert!(page.played_request.is_none());
            assert!(page.user_data_overrides["episode-1"].is_favorite);
        });
    }
    page.read_with(cx, |page, _| {
        assert!(page.user_data_overrides["episode-2"].played);
        assert!(page.series_user_data_revisions.contains_key("series-1"));
        let snapshot = page.home_snapshot();
        assert!(snapshot.resume_items.unwrap().data.items.is_empty());
    });
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/FavoriteItems/episode-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/episode-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/series-1 HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn resume_menu_keeps_played_and_hide_actions_after_favoriting(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = home_menu_window(cx, &server);
    right_click(cx, "resume-item-card-movie-1");
    click(cx, "resume-item-favorite");
    right_click(cx, "resume-item-card-movie-1");
    assert!(cx.debug_bounds("resume-item-favorite-取消收藏").is_some());
    click(cx, "resume-item-mark-played");
    right_click(cx, "resume-item-card-episode-1");
    click(cx, "resume-item-hide-from-resume");
    page.read_with(cx, |page, _| {
        let data = &page.user_data_overrides["movie-1"];
        assert!(data.is_favorite && data.played);
        assert_eq!(data.playback_position_ticks, Some(0));
        let resume = page.resume_items.as_ref().unwrap();
        assert_eq!(resume.items.len(), 1);
        assert_eq!(resume.total_record_count, 1);
        assert_eq!(resume.items[0].id, "episode-2");
        assert!(
            !page
                .user_item_by_id("episode-1")
                .unwrap()
                .user_data
                .unwrap()
                .played
        );
    });
    assert_eq!(
        server.mutations(),
        [
            "POST /emby/Users/user-1/FavoriteItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/PlayedItems/movie-1 HTTP/1.1",
            "POST /emby/Users/user-1/Items/episode-1/HideFromResume?Hide=true HTTP/1.1",
        ]
    );
}

#[gpui::test]
fn card_menu_failures_restore_favorites_and_keep_resume_items_with_visible_errors(
    cx: &mut TestAppContext,
) {
    let server = MockEmby::new();
    let (page, cx) = home_menu_window(cx, &server);
    right_click(cx, "resume-item-card-movie-1");
    click(cx, "resume-item-favorite");
    for (card, option, id, favorite) in [
        (
            "resume-item-card-movie-1",
            "resume-item-favorite",
            "movie-1",
            true,
        ),
        (
            "user-view-item-card-episode-1",
            "user-view-item-favorite",
            "episode-1",
            false,
        ),
        (
            "user-view-item-card-episode-1",
            "user-view-item-mark-played",
            "episode-1",
            false,
        ),
    ] {
        server.fail_next.store(true, Ordering::SeqCst);
        right_click(cx, card);
        click(cx, option);
        page.update(cx, |page, _| {
            let data = page.user_item_by_id(id).unwrap().user_data.unwrap();
            assert_eq!(data.is_favorite, favorite);
            assert!(!data.played);
            assert_eq!(page.resume_items.as_ref().unwrap().items.len(), 3);
            assert!(page.has_visible_notifications());
            assert!(!page.detail_user_data_pending());
            page.clear_all_notifications();
        });
    }
}

#[gpui::test]
fn library_cover_menu_uses_library_item_and_shows_failure_in_that_library(cx: &mut TestAppContext) {
    let server = MockEmby::new();
    let (page, cx) = home_menu_window(cx, &server);
    page.update(cx, |page, cx| {
        let view = page.user_views.as_ref().unwrap().items[0].clone();
        let items = page.user_view_items_rows["videos"]
            .items
            .as_ref()
            .unwrap()
            .items
            .clone();
        let mut paged = PagedItemsState::default();
        paged.items = items;
        paged.initial = LoadState::Loaded;
        paged.exhausted = true;
        page.libraries.insert(
            "videos".into(),
            LibraryState {
                title: "Videos".into(),
                item_types: vec![VideoItemType::Movie],
                sort_by: UserItemsSort::SortName,
                sort_order: SortOrder::Ascending,
                sort_menu_open: false,
                paged,
            },
        );
        page.open_library_for_view(&view, cx);
    });
    cx.run_until_parked();
    right_click(cx, "library-grid-item-movie-1");
    click(cx, "user-view-item-favorite");
    page.read_with(cx, |page, _| {
        assert!(page.user_item_by_id("movie-1").unwrap().is_favorite())
    });
    server.fail_next.store(true, Ordering::SeqCst);
    right_click(cx, "library-grid-item-movie-1");
    click(cx, "user-view-item-mark-played");
    page.read_with(cx, |page, _| {
        assert!(page.has_visible_notifications());
        assert!(!page.user_data_overrides["movie-1"].played);
        assert_eq!(page.resume_items.as_ref().unwrap().items.len(), 3);
    });
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
            user_data: None, notification_scope: NotificationScope::Detail,
            notification_key: "detail:played".into(),
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
