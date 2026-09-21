use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point, px, size};
use serde_json::json;

use super::*;
use crate::emby::EmbyClient;

fn movie_window<'a>(
    cx: &'a mut TestAppContext,
    overview: &str,
) -> (Entity<HomeContent>, &'a mut VisualTestContext) {
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let server = serde_json::from_value(json!({
            "id": "overview-test", "user_id": "test",
            "endpoint": {"protocol": "Https", "address": "", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        let mut page = HomeContent::new(server, EmbyClient::new("test".into()).unwrap(), cx);
        let movie =
            json!({"Id": "movie-1", "Name": "Movie", "Type": "Movie", "Overview": overview});
        let mut detail =
            SeriesDetailState::new_movie(&serde_json::from_value(movie.clone()).unwrap());
        detail.item = Some(serde_json::from_value(movie).unwrap());
        page.navigation.push_detail("movie-1".into(), None);
        page.series_detail = Some(detail);
        page
    });
    cx.simulate_resize(size(px(1000.0), px(800.0)));
    cx.run_until_parked();
    (page, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect(selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn long_movie_overview_opens_centered_scrollable_overlay_and_restores_focus(
    cx: &mut TestAppContext,
) {
    let overview = format!(
        "{}\n\n{}",
        "A journey through distant worlds. ".repeat(40),
        "这是保留完整段落的电影剧情简介。".repeat(80)
    );
    let (page, cx) = movie_window(cx, &overview);
    let previous_focus = cx.update(|window, cx| {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        focus
    });
    assert!(
        cx.debug_bounds("movie-detail-overview")
            .unwrap()
            .size
            .height
            <= px(66.0)
    );
    click(cx, "movie-detail-overview");
    let overlay = cx.debug_bounds("movie-overview-overlay").unwrap();
    let panel = cx.debug_bounds("movie-overview-panel").unwrap();
    assert!((panel.center().x - overlay.center().x).abs() <= px(1.0));
    assert!((panel.center().y - overlay.center().y).abs() <= px(1.0));
    let scroll = cx.debug_bounds("movie-overview-scroll").unwrap();
    let text = cx.debug_bounds("movie-overview-text").unwrap();
    assert!(text.size.height > scroll.size.height);
    assert!(panel.top() >= overlay.top() + px(24.0));
    assert!(panel.bottom() <= overlay.bottom() - px(24.0));
    let detail_offset = page.read_with(cx, |page, _| {
        page.series_detail.as_ref().unwrap().scroll_handle.offset()
    });
    cx.simulate_mouse_move(scroll.center(), None, Modifiers::default());
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: scroll.center(),
        delta: gpui::ScrollDelta::Lines(point(0.0, -6.0)),
        modifiers: Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        let detail = page.series_detail.as_ref().unwrap();
        assert!(
            detail
                .overview_overlay
                .as_ref()
                .unwrap()
                .scroll_handle
                .offset()
                .y
                < px(0.0)
        );
        assert_eq!(detail.scroll_handle.offset(), detail_offset);
    });
    // Clicking inside the panel must not dismiss it through the backdrop.
    click(cx, "movie-overview-scroll");
    assert!(cx.debug_bounds("movie-overview-panel").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("movie-overview-panel").is_none());
    cx.update(|window, _| assert!(previous_focus.is_focused(window)));

    click(cx, "movie-detail-overview");
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.series_detail
                .as_ref()
                .unwrap()
                .overview_overlay
                .as_ref()
                .unwrap()
                .scroll_handle
                .offset(),
            point(px(0.0), px(0.0))
        );
    });
    click(cx, "movie-overview-close");
    assert!(cx.debug_bounds("movie-overview-panel").is_none());
    click(cx, "movie-detail-overview");
    // This also covers the detail back button beneath the backdrop.
    cx.simulate_click(point(px(32.0), px(30.0)), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("movie-overview-panel").is_none());
    page.read_with(cx, |page, _| {
        assert_eq!(page.series_detail.as_ref().unwrap().series_id, "movie-1")
    });
}

#[gpui::test]
fn movie_overview_expands_only_when_the_current_layout_exceeds_three_lines(
    cx: &mut TestAppContext,
) {
    let overview = "A movie about friendship and adventure. ".repeat(8);
    let (_, cx) = movie_window(cx, &overview);
    for (width, expands) in [(1600.0, false), (320.0, true), (1600.0, false)] {
        cx.simulate_resize(size(px(width), px(800.0)));
        cx.run_until_parked();
        click(cx, "movie-detail-overview");
        assert_eq!(
            cx.debug_bounds("movie-overview-panel").is_some(),
            expands,
            "width={width}"
        );
        if expands {
            cx.simulate_resize(size(px(320.0), px(400.0)));
            cx.run_until_parked();
            let overlay = cx.debug_bounds("movie-overview-overlay").unwrap();
            let panel = cx.debug_bounds("movie-overview-panel").unwrap();
            assert!(panel.left() >= overlay.left());
            assert!(panel.right() <= overlay.right());
            assert!(panel.top() >= overlay.top());
            assert!(panel.bottom() <= overlay.bottom());
            click(cx, "movie-overview-close");
        }
    }
}
