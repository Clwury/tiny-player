use crate::home::detail::test_fixture::detail_binding;
use std::{cell::RefCell, rc::Rc};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point, px, size};
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

fn episode_line_window(cx: &mut TestAppContext) -> (Entity<HomeContent>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init(cx);
        // Assert the final geometry without depending on wall-clock animation timing.
        cx.set_reduce_motion(true);
    });
    cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id": "episode-line-test", "user_id": "test",
            "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        let series = json!({"Id": "series-1", "Name": "Series", "Type": "Series"});
        let mut detail =
            DetailFixture::new_series(&serde_json::from_value(series.clone()).unwrap());
        detail.controller.state.item = Some(serde_json::from_value(series).unwrap());
        let episodes: Vec<_> = (1..=30)
            .map(|index| {
                json!({
                    "Id": format!("episode-{index}"), "Name": format!("Episode {index}"),
                    "Type": "Episode", "SeasonId": "season-1", "ParentIndexNumber": 1,
                    "IndexNumber": index, "Overview": "An episode overview. ".repeat(20)
                })
            })
            .collect();
        detail.controller.state.episodes = Some(
            serde_json::from_value(json!({"Items": episodes, "TotalRecordCount": 30})).unwrap(),
        );
        detail.controller.state.effects.episodes = LoadState::Loaded;
        detail.controller.state.selected_episode_id = Some("episode-20".into());
        page.controller
            .test_state_mut()
            .navigation
            .push_detail_route_fixture("series-1".into(), None);
        page.install_detail_fixture(Some(detail));
        page
    })
}

fn click_episode_line(cx: &mut VisualTestContext) {
    let line = cx.debug_bounds("series-detail-episode-line-text").unwrap();
    cx.simulate_click(line.center(), Modifiers::default());
    cx.run_until_parked();
    // Include deferred frame callbacks when checking that the page stays in place.
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
}

#[gpui::test]
fn episode_line_only_responds_to_clicks_on_its_text(cx: &mut TestAppContext) {
    let (page, cx) = episode_line_window(cx);
    cx.simulate_resize(size(px(900.0), px(500.0)));
    cx.run_until_parked();
    let row = cx.debug_bounds("series-detail-episode-line").unwrap();
    let text = cx.debug_bounds("series-detail-episode-line-text").unwrap();
    assert!(text.size.width > px(0.0));
    assert!(text.right() + px(10.0) < row.right());
    let blank = point((text.right() + row.right()) / 2.0, row.center().y);
    let scroll_before = page.read_with(cx, |page, _| {
        page.detail_view()
            .unwrap()
            .presentation
            .scroll_handle
            .offset()
    });

    cx.simulate_click(blank, Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert_eq!(
            detail
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY),
            0.0
        );
        assert_eq!(detail.presentation.scroll_handle.offset(), scroll_before);
    });

    click_episode_line(cx);
    page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert!(
            detail
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY)
                > 0.0
        );
        assert_eq!(detail.presentation.scroll_handle.offset(), scroll_before);
    });
}

#[gpui::test]
fn clicking_episode_line_reveals_the_card_horizontally_without_scrolling_the_page(
    cx: &mut TestAppContext,
) {
    let (page, cx) = episode_line_window(cx);
    for width in [550.0, 900.0, 1100.0] {
        cx.simulate_resize(size(px(width), px(500.0)));
        for (id, selector, initial_offset, initial_y) in [
            (
                "episode-20",
                "series-detail-episode-card-episode-20",
                0.0,
                0.0,
            ),
            (
                "episode-1",
                "series-detail-episode-card-episode-1",
                2500.0,
                -64.0,
            ),
            (
                "episode-30",
                "series-detail-episode-card-episode-30",
                0.0,
                -64.0,
            ),
        ] {
            page.update(cx, |page, cx| {
                let detail = detail_binding(
                    page.controller.test_state_mut().navigation,
                    &mut page.detail_resources,
                )
                .unwrap();
                detail.controller.state.selected_episode_id = Some(id.into());
                detail
                    .presentation
                    .scroll_handle
                    .set_offset(point(px(0.0), px(initial_y)));
                detail
                    .presentation
                    .episodes_carousel
                    .set_scroll_offset(initial_offset, f32::INFINITY);
                detail.presentation.episodes_carousel.sync_previous_offset();
                cx.notify();
            });
            cx.run_until_parked();
            let (viewport, scroll_before) = page.read_with(cx, |page, _| {
                let scroll = &page.detail_view().unwrap().presentation.scroll_handle;
                (scroll.bounds(), scroll.offset())
            });
            let row_before = cx.debug_bounds("series-detail-episodes-row").unwrap();
            assert!(row_before.bottom() > viewport.bottom());

            click_episode_line(cx);

            let row = cx.debug_bounds("series-detail-episodes-row").unwrap();
            let card = cx.debug_bounds(selector).unwrap();
            assert!(
                card.left() >= row.left() - px(0.5),
                "{id}: {card:?}, {row:?}"
            );
            assert!(
                card.right() <= row.right() + px(0.5),
                "{id}: {card:?}, {row:?}"
            );
            assert_eq!(row.top(), row_before.top());
            page.read_with(cx, |page, _| {
                let detail = page.detail_view().unwrap();
                assert_eq!(detail.model.selected_episode_id.as_deref(), Some(id));
                assert_eq!(detail.presentation.scroll_handle.offset(), scroll_before);
                assert!(!detail.model.playback_loading);
            });
        }
    }
}

#[gpui::test]
fn clicking_episode_line_keeps_a_visible_card_at_its_horizontal_position(cx: &mut TestAppContext) {
    let (page, cx) = episode_line_window(cx);
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    page.update(cx, |page, cx| {
        let detail = detail_binding(
            page.controller.test_state_mut().navigation,
            &mut page.detail_resources,
        )
        .unwrap();
        detail.controller.state.selected_episode_id = Some("episode-3".into());
        detail
            .presentation
            .episodes_carousel
            .set_scroll_offset(300.0, f32::INFINITY);
        detail.presentation.episodes_carousel.sync_previous_offset();
        cx.notify();
    });
    cx.run_until_parked();
    click_episode_line(cx);
    page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert_eq!(
            detail
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY),
            300.0
        );
    });
}

#[gpui::test]
fn episode_line_without_a_loaded_card_does_not_scroll(cx: &mut TestAppContext) {
    let (page, cx) = episode_line_window(cx);
    cx.simulate_resize(size(px(900.0), px(500.0)));
    page.update(cx, |page, cx| {
        let detail = detail_binding(
            page.controller.test_state_mut().navigation,
            &mut page.detail_resources,
        )
        .unwrap();
        detail.controller.state.next_up = detail.controller.state.episodes.take();
        detail.controller.state.effects.episodes = LoadState::Loading;
        cx.notify();
    });
    cx.run_until_parked();
    let before = page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert!(detail.model.hero_line().is_some());
        detail.presentation.scroll_handle.offset()
    });
    click_episode_line(cx);
    page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert_eq!(detail.presentation.scroll_handle.offset(), before);
        assert_eq!(
            detail
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY),
            0.0
        );
    });
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
            let mut detail = DetailFixture::from_user_item(
                &serde_json::from_value(item.clone()).unwrap(),
                Default::default(),
            )
            .unwrap();
            detail.controller.state.item = Some(serde_json::from_value(item).unwrap());
            page.controller
                .test_state_mut()
                .navigation
                .push_detail_route_fixture("hero-1".into(), None);
            page.install_detail_fixture(Some(detail));
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
            let mut detail = DetailFixture::from_user_item(
                &serde_json::from_value(item.clone()).unwrap(),
                Default::default(),
            )
            .unwrap();
            detail.controller.state.item = Some(serde_json::from_value(item).unwrap());
            let request =
                EmbyImageRequest::new("logo-sizing", EmbyImageType::Logo).with_tag(Some(tag));
            let key = CachedImageKey::from_request(&page.current_server.id, &request).unwrap();
            page.images.test_install_path(key, path);
            page.controller
                .test_state_mut()
                .navigation
                .push_detail_route_fixture("logo-sizing".into(), None);
            page.install_detail_fixture(Some(detail));
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
fn hero_image_and_scrims_share_device_edges_at_different_heights_and_scroll_offsets(
    cx: &mut TestAppContext,
) {
    use crate::{
        emby::{EmbyImageRequest, EmbyImageType},
        images::cache::CachedImageKey,
    };
    use gpui::{ScaledPixels, black, linear_color_stop, linear_gradient};

    let images = tempfile::tempdir().unwrap();
    let backdrop_path = images.path().join("bright-backdrop.png");
    image::RgbaImage::from_pixel(192, 108, image::Rgba([255, 255, 255, 255]))
        .save(&backdrop_path)
        .unwrap();
    let (page, cx) = episode_line_window(cx);
    page.update(cx, |page, cx| {
        let item = detail_binding(
            page.controller.test_state_mut().navigation,
            &mut page.detail_resources,
        )
        .unwrap()
        .controller
        .state
        .item
        .as_mut()
        .unwrap();
        item.backdrop_image_tags = Some(vec!["edge-test".into()]);
        // Keep the page scrollable even at the tallest tested window size.
        item.people = Some(
            serde_json::from_value(json!([
                {"Id": "person-1", "Name": "Actor"}
            ]))
            .unwrap(),
        );
        let request = EmbyImageRequest::new("series-1", EmbyImageType::Backdrop)
            .with_tag(Some("edge-test".into()))
            .with_max_width(1024);
        let key = CachedImageKey::from_request(&page.current_server.id, &request).unwrap();
        page.images.test_install_path(key, backdrop_path);
        cx.notify();
    });
    let bottom_background = linear_gradient(
        180.0,
        linear_color_stop(black().opacity(0.0), 0.0),
        linear_color_stop(black().opacity(0.72), 1.0),
    );
    let side_background = linear_gradient(
        90.0,
        linear_color_stop(black().opacity(0.72), 0.0),
        linear_color_stop(black().opacity(0.0), 1.0),
    );

    for selection in [theme::ColorTheme::Latte, theme::ColorTheme::Mocha] {
        cx.update(|_, cx| theme::set(selection, cx));
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for height in [
                433.0, 434.0, 599.0, 600.0, 601.0, 799.0, 800.0, 801.0, 900.0, 901.0,
            ] {
                cx.simulate_resize(size(px(1100.0), px(height)));
                // Native resize restores the platform scale; override it afterwards.
                cx.update(|window, _| window.set_scale_factor(scale));
                for scroll in [0.0, 0.125, 0.25, 0.5, 0.75, 16.3, 64.5, 127.75] {
                    page.update(cx, |page, cx| {
                        page.detail_view()
                            .unwrap()
                            .presentation
                            .scroll_handle
                            .set_offset(point(px(0.0), px(-scroll)));
                        cx.notify();
                    });
                    cx.run_until_parked();
                    page.read_with(cx, |page, _| {
                        assert_eq!(
                            page.detail_view()
                                .unwrap()
                                .presentation
                                .scroll_handle
                                .offset()
                                .y,
                            px(-scroll)
                        );
                    });
                    let hero = cx.debug_bounds("series-detail-hero").unwrap();
                    let backdrop = cx.debug_bounds("series-detail-backdrop").unwrap();
                    cx.update(|window, cx| {
                        let hero_pixels = crate::ui::paint::device_bounds(window, hero);
                        let image_pixels = crate::ui::paint::device_bounds(window, backdrop);
                        assert_eq!(
                            image_pixels, hero_pixels,
                            "scale={scale}, height={height}, scroll={scroll}, image={backdrop:?}, hero={hero:?}"
                        );
                        let bottom = ScaledPixels(hero_pixels.bottom() as f32);
                        let quads = window.painted_quads();
                        for background in [side_background, bottom_background] {
                            let quad = quads.iter().find(|quad| quad.background == background).unwrap();
                            assert_eq!(
                                quad.bounds.bottom(), bottom,
                                "scale={scale}, height={height}, scroll={scroll}, hero={hero:?}, quad={quad:?}"
                            );
                            let last_pixel = point(
                                ScaledPixels(image_pixels.center().x as f32 + 0.5),
                                bottom - ScaledPixels(0.5),
                            );
                            assert!(quad.bounds.contains(&last_pixel));
                            assert!(quad.content_mask.bounds.contains(&last_pixel));
                        }
                        // The image stops with its scrims. Only the page surface
                        // may cover the very next physical pixel row.
                        for x in [hero_pixels.left() + 8, hero_pixels.center().x, hero_pixels.right() - 8] {
                            let pixel = point(ScaledPixels(x as f32 + 0.5), bottom + ScaledPixels(0.5));
                            let covering = quads.iter().rev().find(|quad| {
                                quad.bounds.contains(&pixel) && quad.content_mask.bounds.contains(&pixel)
                            }).unwrap();
                            assert_eq!(covering.background.as_solid(), Some(theme::get(cx).background));
                        }
                    });
                }
            }
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
            DetailFixture::new_series(&serde_json::from_value(series.clone()).unwrap());
        detail.controller.state.item = Some(serde_json::from_value(series).unwrap());
        detail.controller.state.selected_season_id = Some("season-1".into());
        detail.controller.state.episodes_request_season_id = Some("season-1".into());
        detail.controller.state.effects.episodes = LoadState::Loading;
        let request = EmbyImageRequest::new("series-1", EmbyImageType::Logo)
            .with_tag(Some("logo-tag".into()));
        let key = CachedImageKey::from_request(&page.current_server.id, &request).unwrap();
        page.images.test_install_path(key, logo_path);
        page.controller
            .test_state_mut()
            .navigation
            .push_detail_route_fixture("series-1".into(), None);
        page.install_detail_fixture(Some(detail));
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
        assert!(page.detail_view().unwrap().model.hero_line().is_none());
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
            let detail = detail_binding(
                page.controller.test_state_mut().navigation,
                &mut page.detail_resources,
            )
            .unwrap();
            detail.controller.state.effects.episodes = LoadState::Loading;
            detail.controller.state.episodes_request_season_id = Some("season-1".into());
            let request =
                restart_detail_request(page, crate::effects::DetailResource::Episodes, None);
            page.finish_detail_request(
                request,
                Ok(super::controller::DetailResponse::Episodes(
                    serde_json::from_value(json!({"Items": items, "TotalRecordCount": count}))
                        .unwrap(),
                )),
                cx,
            );
        });
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.detail_view().unwrap().model.hero_line().is_some(),
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
        page.controller.test_state_mut().feed.state.resume_items = Some(
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
        let request = restart_detail_request(page, crate::effects::DetailResource::Item, None);
        page.finish_detail_request(
            request,
            Ok(super::controller::DetailResponse::Item(Box::new(
                serde_json::from_value(json!({
                    "Id": "800326", "Name": "Series", "Type": "Series"
                }))
                .unwrap(),
            ))),
            cx,
        );
        let request = restart_detail_request(page, crate::effects::DetailResource::Seasons, None);
        page.finish_detail_request(
            request,
            Ok(super::controller::DetailResponse::Seasons(
                serde_json::from_value(json!({"Items": [
                {"Id": "specials", "Name": "Specials", "Type": "Season", "IndexNumber": 0},
                {"Id": "823952", "Name": "Season 1", "Type": "Season", "IndexNumber": 1}
            ], "TotalRecordCount": 2}))
                .unwrap(),
            )),
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
        let request = restart_detail_request(page, crate::effects::DetailResource::Episodes, None);
        page.finish_detail_request(request, Ok(super::controller::DetailResponse::Episodes(episodes)), cx);
        let request = restart_detail_request(page, crate::effects::DetailResource::ResumeSources, None);
        page.finish_detail_request(request, Ok(super::controller::DetailResponse::ResumeSources(playback_sources())), cx);
        page.clear_all_notifications();
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        let detail = page.detail_view().unwrap();
        assert_eq!(detail.model.selected_episode_id.as_deref(), Some("824018"));
        assert_eq!(detail.model.selected_media_source_index(), Some(0));
        assert!(detail.model.selected_media_source_label().contains("2160p"));
        assert_eq!(detail.model.selected_media_sources().unwrap().len(), 2);
        assert_eq!(detail.model.playback_position_seconds(), Some(350));
    });

    // Manual selection wins even when PlaybackInfo is rebuilt in another order.
    page.update(cx, |page, cx| {
        page.select_series_media_source(1, cx);
        let mut sources = playback_sources();
        sources.reverse();
        let request =
            restart_detail_request(page, crate::effects::DetailResource::ResumeSources, None);
        page.finish_detail_request(
            request,
            Ok(super::controller::DetailResponse::ResumeSources(sources)),
            cx,
        );
        let detail = page.detail_view().unwrap();
        assert_eq!(detail.model.selected_media_source_label(), "S01E07 - 1080p");
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
            page.detail_view().unwrap(),
            &page.current_server,
            PlaybackLanguagePreferences::get(cx),
            &SavedTrackChoices::default(),
        )
        .unwrap();
        assert_eq!(selected.item_id, "1193754");
        assert_eq!(selected.media_source_id, "source-2160");
        assert!(
            page.controller
                .test_state()
                .navigation
                .detail()
                .unwrap()
                .playback_selection_is_current(&selected)
        );
        let command = begin_prepared_playback(page, selected);
        page.finish_play_selected_media(
            command,
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
    let old_request = page.update(cx, |page, _| {
        restart_detail_request(page, crate::effects::DetailResource::ResumeSources, None)
    });
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
            assert_eq!(
                page.controller.test_state().played_video_versions[item_id].source_id,
                "source-2160"
            );
            assert_eq!(
                page.controller.test_state().user_data.overrides[item_id].playback_position_ticks,
                Some(4_000_000_000)
            );
        }

        // Round-trip the saved choice and hydrate it as on application restart.
        let snapshot =
            serde_json::from_slice(&serde_json::to_vec(&page.home_snapshot()).unwrap()).unwrap();
        page.controller
            .test_state_mut()
            .played_video_versions
            .clear();
        page.hydrate_home_snapshot(snapshot, cx);
        let episodes = page.detail_view().unwrap().model.episodes.clone();
        page.open_resume_item_detail_by_id("824018".into(), cx);
        let detail = detail_binding(
            page.controller.test_state_mut().navigation,
            &mut page.detail_resources,
        )
        .unwrap();
        detail.controller.state.episodes = episodes;
        {
            let change = detail
                .controller
                .state
                .choose_episode_from_loaded_episodes();
            detail.presentation.apply_change(change);
        }
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        // Reject a response from the previous detail, even for the same Resume.Id.
        page.finish_detail_request(
            old_request.clone(),
            Ok(super::controller::DetailResponse::ResumeSources(
                playback_sources(),
            )),
            cx,
        );
        assert!(
            page.detail_view()
                .unwrap()
                .model
                .resume_media_sources
                .is_none()
        );

        // The server still returns Resume.Id=824018 and puts its default first.
        // Its source IDs changed, so the retained version name must restore 2160p.
        let mut sources = playback_sources();
        sources.reverse();
        sources[1].id = Some("new-source-2160".into());
        sources[1].name = Some("(1998) - S01E07.2160p.BDRip.H.265.FLAC".into());
        let request =
            restart_detail_request(page, crate::effects::DetailResource::ResumeSources, None);
        page.finish_detail_request(
            request,
            Ok(super::controller::DetailResponse::ResumeSources(sources)),
            cx,
        );
        let detail = page.detail_view().unwrap();
        assert_eq!(detail.model.selected_media_source_index(), Some(1));
        assert!(detail.model.selected_media_source_label().contains("2160p"));
        assert_eq!(detail.model.playback_position_seconds(), Some(400));
    });
}

#[gpui::test]
fn opening_another_detail_cancels_delivery_and_rejects_late_image_and_error_work(
    cx: &mut TestAppContext,
) {
    use crate::effects::DetailResource;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    let (page, cx) = episode_line_window(cx);
    let delivered = Arc::new(AtomicBool::new(false));
    let task_delivered = delivered.clone();
    let request = page.update(cx, |page, cx| {
        let request = restart_detail_request(page, DetailResource::Item, None);
        let task_request = request.clone();
        let task = cx.spawn(async move |page, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            task_delivered.store(true, Ordering::SeqCst);
            page.update(cx, |page, cx| {
                page.finish_detail_request(task_request, Err(anyhow::anyhow!("late error")), cx)
            })
            .ok();
        });
        detail_binding(
            page.controller.test_state_mut().navigation,
            &mut page.detail_resources,
        )
        .unwrap()
        .tasks
        .entry(DetailResource::Item)
        .or_default()
        .replace(task);
        request
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        let item =
            serde_json::from_value(json!({"Id":"movie", "Name":"Movie", "Type":"Movie"})).unwrap();
        let mut next = DetailFixture::new_movie(&item);
        next.controller.state.effects.item = LoadState::Loaded;
        next.controller.state.effects.similar = LoadState::Loaded;
        page.open_detail_state(next.controller, cx);
        assert!(
            page.detail_resources[&page.controller.test_state().navigation.history()[0].id()]
                .tasks
                .is_empty()
        );
        assert_eq!(
            page.controller.test_state().navigation.history()[0]
                .view_model()
                .effects
                .item,
            LoadState::Idle
        );
        let images_before = format!("{:?}", page.images);
        let item = serde_json::from_value(json!({
            "Id":"series-1", "Name":"Late title", "ImageTags":{"Primary":"late-image"},
            "UserData":{"IsFavorite":true}
        }))
        .unwrap();
        page.finish_detail_request(
            request.clone(),
            Ok(super::controller::DetailResponse::Item(Box::new(item))),
            cx,
        );
        page.finish_detail_request(request, Err(anyhow::anyhow!("late error")), cx);
        assert_eq!(format!("{:?}", page.images), images_before);
        assert!(page.notifications.is_empty());
        assert!(
            !page
                .controller
                .test_state()
                .user_data
                .overrides
                .contains_key("series-1")
        );
        assert_eq!(page.detail_view().unwrap().model.title, "Movie");
    });
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(!delivered.load(Ordering::SeqCst));
}

#[gpui::test]
fn returning_to_same_media_restores_its_own_model_and_scroll_resources(cx: &mut TestAppContext) {
    use crate::home::detail::controller::DetailController;
    let (page, cx) = episode_line_window(cx);
    cx.simulate_resize(size(px(1200.0), px(900.0)));
    cx.run_until_parked();
    let (first_id, first_activation, second_id) = page.update(cx, |page, cx| {
        let first = page
            .controller
            .test_state_mut()
            .navigation
            .detail_mut()
            .unwrap();
        first.state.effects.item = LoadState::Loaded;
        first.state.effects.similar = LoadState::Loaded;
        first.state.effects.seasons = LoadState::Loaded;
        first.state.effects.next_up = LoadState::Loaded;
        first.state.selected_season_id = Some("season-1".into());
        first.state.episodes_request_season_id = Some("season-1".into());
        let id = first.id();
        let activation = first.activation().cloned();
        let duplicate_model = first.view_model().clone();
        page.detail_resources
            .get_mut(&id)
            .unwrap()
            .presentation
            .episodes_carousel
            .set_scroll_offset(600.0, f32::INFINITY);
        let mut second = DetailController::new(duplicate_model, page.request_identity());
        second.state.selected_episode_id = Some("episode-2".into());
        let second_id = second.id();
        page.open_detail_state(second, cx);
        assert_eq!(page.detail_resources.len(), 2);
        assert_eq!(
            page.detail_view()
                .unwrap()
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY),
            0.0
        );
        assert_eq!(
            page.detail_view()
                .unwrap()
                .model
                .selected_episode_id
                .as_deref(),
            Some("episode-2")
        );
        (id, activation, second_id)
    });
    cx.run_until_parked();
    let back = cx.debug_bounds("series-detail-back-button").unwrap();
    cx.simulate_click(back.center(), Modifiers::default());
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        let detail = page.controller.test_state().navigation.detail().unwrap();
        assert_eq!(detail.id(), first_id);
        assert_ne!(detail.activation(), first_activation.as_ref());
        assert_eq!(
            detail.view_model().selected_episode_id.as_deref(),
            Some("episode-20")
        );
        assert_eq!(
            page.detail_view()
                .unwrap()
                .presentation
                .episodes_carousel
                .scroll_offset(f32::INFINITY),
            600.0
        );
        assert!(!page.detail_resources.contains_key(&second_id));
        assert_eq!(page.detail_resources.len(), 1);
    });
    cx.update(|window, cx| {
        page.update(cx, |page, cx| {
            assert!(page.select_root(HomeRoot::Search, window, cx));
            assert!(page.controller.test_state().navigation.detail().is_none());
            assert!(page.detail_resources.is_empty());
        });
    });
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.update(|window, cx| window.simulate_next_frame(cx));
    cx.run_until_parked();
}
