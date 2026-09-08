use std::{cell::RefCell, rc::Rc};

use gpui::{Modifiers, TestAppContext, px, size};
use serde_json::json;

use super::*;
use crate::{
    emby::{EmbyClient, MediaSource},
    theme,
};

fn playback_sources() -> Vec<MediaSource> {
    serde_json::from_value(json!([
        {"Id": "source-2160", "ItemId": "1193754", "Name": "S01E07.2160p.BDRip.H.265.FLAC", "Type": "Grouping"},
        {"Id": "source-1080", "ItemId": "824018", "Name": "S01E07 - 1080p", "Type": "Default"}
    ])).unwrap()
}

#[gpui::test]
fn latte_hero_uses_local_dark_scrims_without_fallback_names(cx: &mut TestAppContext) {
    cx.update(|cx| theme::set(theme::ColorTheme::Latte, cx));
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id": "hero-theme-test", "user_id": "test",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx)
    });
    for (item_type, logo_tag) in [
        ("Series", Some("pending-logo")),
        ("Movie", Some("pending-logo")),
        ("Episode", Some("pending-logo")),
        ("Series", None),
        ("Movie", None),
        ("Episode", None),
    ] {
        page.update(cx, |page, cx| {
            let item = json!({
                "Id": "hero-1", "Name": "Hidden title", "Type": item_type,
                "SeriesId": "hero-series", "SeriesName": "Hidden series title",
                "ImageTags": logo_tag.map(|tag| json!({"Logo": tag})), "CommunityRating": 8.5,
                "Genres": ["Drama"], "OfficialRating": "PG"
            });
            let mut detail =
                SeriesDetailState::from_user_item(&serde_json::from_value(item.clone()).unwrap())
                    .unwrap();
            detail.item = Some(serde_json::from_value(item).unwrap());
            page.navigation.push_detail("hero-1".into(), None);
            page.series_detail = Some(detail);
            cx.notify();
        });
        for height in [600.0, 900.0] {
            cx.simulate_resize(size(px(1100.0), px(height)));
            cx.run_until_parked();
            assert!(cx.debug_bounds("series-detail-title").is_none());
            assert!(cx.debug_bounds("series-detail-logo").is_none());
            let hero = cx.debug_bounds("series-detail-hero").unwrap();
            let scrim = cx.debug_bounds("series-detail-hero-bottom-scrim").unwrap();
            assert_eq!(scrim.size.height, px(120.0));
            assert!(scrim.top() > hero.top());
            cx.update(|window, cx| {
                let quads = window.painted_quads();
                let theme = theme::get(cx);
                assert!(
                    quads
                        .iter()
                        .all(|quad| { quad.background != theme.background.opacity(0.35).into() })
                );
                let media = theme::media_overlay(cx);
                assert!(quads.iter().any(|quad| {
                    quad.background == media.dialog_background.opacity(0.94).into()
                }));
                assert!(quads.iter().any(|quad| {
                    quad.background == media.dialog_background.opacity(0.86).into()
                }));
                assert_eq!(
                    quads
                        .iter()
                        .filter(|quad| quad.background.as_solid().is_none())
                        .count(),
                    2,
                );
            });
        }
    }
}

#[gpui::test]
fn logos_adapt_to_aspect_ratio_and_stay_inside_small_heroes(cx: &mut TestAppContext) {
    use crate::{
        emby::{EmbyImageRequest, EmbyImageType},
        images::cache::CachedImageKey,
    };

    let images = tempfile::tempdir().unwrap();
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id": "logo-sizing-test", "user_id": "test",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx)
    });
    for (item_type, width, height) in [
        ("Movie", 1200, 100),
        ("Movie", 100, 100),
        ("Movie", 100, 300),
        ("Series", 100, 150),
        ("Series", 100, 300),
        ("Series", 50, 500),
    ] {
        let tag = format!("{width}-{height}");
        let path = images.path().join(format!("{tag}.png"));
        image::RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]))
            .save(&path)
            .unwrap();
        page.update(cx, |page, cx| {
            let item = json!({
                "Id": "logo-sizing", "Name": "Title", "Type": item_type,
                "ImageTags": {"Logo": tag}, "CommunityRating": 8.5
            });
            let mut detail =
                SeriesDetailState::from_user_item(&serde_json::from_value(item.clone()).unwrap())
                    .unwrap();
            detail.item = Some(serde_json::from_value(item).unwrap());
            let request =
                EmbyImageRequest::new("logo-sizing", EmbyImageType::Logo).with_tag(Some(tag));
            let key = CachedImageKey::from_request(&page.current_server, &request).unwrap();
            page.image_loader.finish_job(key, Ok(path));
            page.navigation.push_detail("logo-sizing".into(), None);
            page.series_detail = Some(detail);
            cx.notify();
        });
        for (window_width, window_height) in [(1100.0, 900.0), (1100.0, 600.0), (360.0, 400.0)] {
            cx.simulate_resize(size(px(window_width), px(window_height)));
            cx.run_until_parked();
            // Image decoding schedules the layout update for the next frame.
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
            let logo = cx.debug_bounds("series-detail-logo").unwrap();
            let hero = cx.debug_bounds("series-detail-hero").unwrap();
            assert!(logo.size.width > px(0.0) && logo.size.height > px(0.0));
            assert!(logo.left() >= hero.left() + px(24.0));
            assert!(logo.right() <= hero.right() - px(24.0));
            assert!(logo.top() >= hero.top());
            assert!(logo.bottom() <= hero.bottom() - px(24.0));
            assert!(logo.size.height <= px(200.0));
            if width <= height {
                // Portrait and square logos should use more than the old 80px height,
                // while retaining their aspect ratio even in a short window.
                assert!(logo.size.height >= px(if window_height >= 600.0 { 160.0 } else { 120.0 }));
                let ratio = logo.size.width / logo.size.height;
                assert!((ratio - width as f32 / height as f32).abs() < 0.02);
            } else {
                assert!(logo.size.height <= px(80.0));
            }
            if width > height && window_width > 1000.0 {
                // The old fixed width reduced this 12:1 logo to only 17px tall.
                assert!(
                    logo.size.width >= px(400.0),
                    "logo: {logo:?}, hero: {hero:?}"
                );
            }
            assert!(cx.debug_bounds("series-detail-title").is_none());
        }
    }
}

#[gpui::test]
fn series_logo_stays_in_place_when_episodes_finish_loading(cx: &mut TestAppContext) {
    use crate::{
        emby::{EmbyImageRequest, EmbyImageType},
        images::cache::CachedImageKey,
    };

    let images = tempfile::tempdir().unwrap();
    let logo_path = images.path().join("logo.png");
    image::RgbaImage::from_pixel(400, 100, image::Rgba([255, 255, 255, 255]))
        .save(&logo_path)
        .unwrap();
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id": "hero-layout-test",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "user_id": "test", "added_at_unix": 0
        }))
        .unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        let series = json!({
            "Id": "series-1", "Name": "Series", "Type": "Series",
            "ImageTags": {"Logo": "logo-tag"}, "ProductionYear": 2024
        });
        let mut detail =
            SeriesDetailState::new_series(&serde_json::from_value(series.clone()).unwrap());
        detail.item = Some(serde_json::from_value(series).unwrap());
        detail.selected_season_id = Some("season-1".into());
        detail.episodes_request_season_id = Some("season-1".into());
        detail.effects.episodes = LoadState::Loading;
        let request = EmbyImageRequest::new("series-1", EmbyImageType::Logo)
            .with_tag(Some("logo-tag".into()));
        let key = CachedImageKey::from_request(&page.current_server, &request).unwrap();
        page.image_loader.finish_job(key, Ok(logo_path));
        page.navigation.push_detail("series-1".into(), None);
        page.series_detail = Some(detail);
        page
    });
    cx.simulate_resize(size(px(1200.0), px(900.0)));
    cx.run_until_parked();
    let logo_before = cx.debug_bounds("series-detail-logo").unwrap();
    assert_eq!(logo_before.size, size(px(320.0), px(80.0)));
    let scrim = cx.debug_bounds("series-detail-hero-bottom-scrim").unwrap();
    assert!(logo_before.top() < scrim.top());
    assert!(logo_before.bottom() < scrim.top() + scrim.size.height * 0.45);
    let line_before = cx.debug_bounds("series-detail-episode-line").unwrap();
    assert_eq!(line_before.size.height, px(24.0));
    page.read_with(cx, |page, _| {
        assert!(page.series_detail.as_ref().unwrap().hero_line().is_none());
    });

    // Exercise the real completion path for both a populated and an empty season.
    for items in [
        json!([{
            "Id": "episode-1", "Name": "Episode 1", "Type": "Episode",
            "SeasonId": "season-1", "ParentIndexNumber": 1, "IndexNumber": 1
        }]),
        json!([]),
    ] {
        let count = items.as_array().unwrap().len();
        page.update(cx, |page, cx| {
            let detail = page.series_detail.as_mut().unwrap();
            detail.effects.episodes = LoadState::Loading;
            detail.episodes_request_season_id = Some("season-1".into());
            page.finish_series_episodes(
                page.request_identity(),
                DetailRequestRevisions {
                    detail: page.detail_generation,
                    user_data: page.user_data_request_revision(),
                },
                "series-1".into(),
                "season-1".into(),
                Ok(
                    serde_json::from_value(json!({"Items": items, "TotalRecordCount": count}))
                        .unwrap(),
                ),
                cx,
            );
        });
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.series_detail.as_ref().unwrap().hero_line().is_some(),
                count > 0
            );
        });
        assert_eq!(cx.debug_bounds("series-detail-logo").unwrap(), logo_before);
        assert_eq!(
            cx.debug_bounds("series-detail-episode-line").unwrap(),
            line_before
        );
    }
}

#[gpui::test]
fn resume_restores_played_version_when_server_keeps_returning_group_id(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        // No token: background requests stop locally. Inject fixtures through
        // the real completion handlers; this test never contacts a server.
        let server = serde_json::from_value(json!({
            "id": "resume-navigation-test",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "user_id": "test", "added_at_unix": 0
        }))
        .unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        page.resume_items = Some(
            serde_json::from_value(json!({
                "Items": [{
                    "Id": "824018", "Name": "Episode 7", "Type": "Episode",
                    "SeriesId": "800326", "ParentId": "823952", "ParentIndexNumber": 1,
                    "IndexNumber": 7, "UserData": {"PlaybackPositionTicks": 3_500_000_000_u64}
                }], "TotalRecordCount": 1
            }))
            .unwrap(),
        );
        page
    });
    cx.simulate_resize(size(px(1200.0), px(900.0)));
    cx.run_until_parked();
    let card = cx.debug_bounds("resume-items-row").unwrap();
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        page.finish_series_media_item(
            page.request_identity(),
            page.user_data_request_revision(),
            page.detail_generation,
            "800326".into(),
            Ok(serde_json::from_value(json!({
                "Id": "800326", "Name": "Series", "Type": "Series"
            }))
            .unwrap()),
            cx,
        );
        page.finish_series_seasons(
            page.request_identity(),
            page.detail_generation,
            "800326".into(),
            Ok(serde_json::from_value(json!({"Items": [
                {"Id": "specials", "Name": "Specials", "Type": "Season", "IndexNumber": 0},
                {"Id": "823952", "Name": "Season 1", "Type": "Season", "IndexNumber": 1}
            ], "TotalRecordCount": 2}))
            .unwrap()),
            cx,
        );
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        let episodes = serde_json::from_value(json!({"Items": [{
            "Id": "824018", "Name": "Episode 7", "Type": "Episode",
            "IndexNumber": 7, "ParentIndexNumber": 1, "SeasonId": "823952",
            "MediaSources": [{"Id": "metadata-1080", "ItemId": "824018", "Name": "Metadata 1080p", "Type": "Default"}]
        }], "TotalRecordCount": 1})).unwrap();
        page.finish_series_episodes(page.request_identity(), DetailRequestRevisions {
            detail: page.detail_generation, user_data: page.user_data_request_revision()
        }, "800326".into(), "823952".into(), Ok(episodes), cx);
        page.finish_resume_video_sources(page.request_identity(), page.detail_generation,
            "824018".into(), Ok(playback_sources()), cx);
        page.clear_all_notifications();
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        let detail = page.series_detail.as_ref().unwrap();
        assert_eq!(detail.selected_episode_id.as_deref(), Some("824018"));
        assert_eq!(detail.selected_media_source_index(), Some(0));
        assert!(detail.selected_media_source_label().contains("2160p"));
        assert_eq!(detail.selected_media_sources().unwrap().len(), 2);
        assert_eq!(detail.playback_position_seconds(), Some(350));
    });

    // Manual selection wins even when PlaybackInfo is rebuilt in another order.
    page.update(cx, |page, cx| {
        page.select_series_media_source(1, cx);
        let mut sources = playback_sources();
        sources.reverse();
        page.finish_resume_video_sources(
            page.request_identity(),
            page.detail_generation,
            "824018".into(),
            Ok(sources),
            cx,
        );
        let detail = page.series_detail.as_ref().unwrap();
        assert_eq!(detail.selected_media_source_label(), "S01E07 - 1080p");
        page.select_series_media_source(1, cx); // Now select 2160p for playback.
    });
    let opened_request = Rc::new(RefCell::new(None));
    let _subscription = cx.update(|_, cx| {
        let opened_request = opened_request.clone();
        cx.subscribe(&page, move |_, event: &HomeContentEvent, _| {
            if let HomeContentEvent::OpenPlayback(request) = event {
                opened_request.replace(Some(request.clone()));
            }
        })
    });
    page.update(cx, |page, cx| {
        let selected = selected_playback(
            page.series_detail.as_ref().unwrap(),
            &page.current_server,
            PlaybackLanguagePreferences::get(cx),
        )
        .unwrap();
        assert_eq!(selected.item_id, "1193754");
        assert_eq!(selected.media_source_id, "source-2160");
        assert!(page.selected_playback_still_current(&selected));
        page.finish_play_selected_media(
            page.request_identity(),
            page.detail_generation,
            selected,
            Ok(ResolvedPlayback {
                item_id: "1193754".into(),
                media_source_id: "source-2160".into(),
                url: "https://example.com/video.mkv".into(),
                http_headers: Vec::new(),
                content_length: None,
                play_session_id: None,
            }),
            cx,
        );
    });
    cx.run_until_parked();
    let request = opened_request.borrow().clone().unwrap();
    assert_eq!(request.emby.item_id, "1193754");
    assert_eq!(request.queue.current().unwrap().item_id, "824018");
    let version_name = request
        .queue
        .current()
        .unwrap()
        .media_sources
        .iter()
        .find(|source| source.id.as_deref() == Some(request.emby.media_source_id.as_str()))
        .unwrap()
        .name
        .clone();
    let old_generation = page.read_with(cx, |page, _| page.detail_generation);
    page.update(cx, |page, cx| {
        page.apply_playback_update(
            crate::player::PlaybackStateUpdate {
                item_id: request.emby.item_id.clone(),
                list_item_id: "824018".into(),
                media_source_id: request.emby.media_source_id.clone(),
                media_source_name: version_name,
                series_id: Some("800326".into()),
                season_id: Some("823952".into()),
                position_ticks: 4_000_000_000,
                run_time_ticks: Some(14_000_000_000),
                ended: false,
                failed: false,
                selected_item_id: None,
                stop_completion: None,
            },
            cx,
        );
        page.invalidate_pending_home_snapshot_save();
        for item_id in ["824018", "1193754"] {
            assert_eq!(page.played_video_versions[item_id].source_id, "source-2160");
            assert_eq!(
                page.user_data_overrides[item_id].playback_position_ticks,
                Some(4_000_000_000)
            );
        }

        // Round-trip the saved choice and hydrate it as on application restart.
        let snapshot =
            serde_json::from_slice(&serde_json::to_vec(&page.home_snapshot()).unwrap()).unwrap();
        page.played_video_versions.clear();
        page.hydrate_home_snapshot(snapshot);
        let episodes = page.series_detail.as_ref().unwrap().episodes.clone();
        page.open_resume_item_detail_by_id("824018".into(), cx);
        let detail = page.series_detail.as_mut().unwrap();
        detail.episodes = episodes;
        detail.choose_episode_from_loaded_episodes();
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        // Reject a response from the previous detail, even for the same Resume.Id.
        page.finish_resume_video_sources(
            page.request_identity(),
            old_generation,
            "824018".into(),
            Ok(playback_sources()),
            cx,
        );
        assert!(
            page.series_detail
                .as_ref()
                .unwrap()
                .resume_media_sources
                .is_none()
        );

        // The server still returns Resume.Id=824018 and puts its default first.
        // Its source IDs changed, so the retained version name must restore 2160p.
        let mut sources = playback_sources();
        sources.reverse();
        sources[1].id = Some("new-source-2160".into());
        sources[1].name = Some("(1998) - S01E07.2160p.BDRip.H.265.FLAC".into());
        page.finish_resume_video_sources(
            page.request_identity(),
            page.detail_generation,
            "824018".into(),
            Ok(sources),
            cx,
        );
        let detail = page.series_detail.as_ref().unwrap();
        assert_eq!(detail.selected_media_source_index(), Some(1));
        assert!(detail.selected_media_source_label().contains("2160p"));
        assert_eq!(detail.playback_position_seconds(), Some(400));
    });
}
