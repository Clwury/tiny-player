use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    VisualTestContext, Window, div, px, size,
};

use crate::{
    emby::{EmbyClient, UserItems},
    home::{
        HomeContent, UserViewItemsRow,
        carousel::{
            HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX, HOME_MAIN_SCROLL_CONTENT_RIGHT_PADDING_PX,
            HOME_SIDEBAR_WIDTH_PX,
        },
    },
    server::CachedServer,
    theme,
    ui::{
        scrollbar::{SCROLLBAR_RIGHT_INSET_PX, SCROLLBAR_WIDTH_PX},
        titlebar::APP_TITLEBAR_HEIGHT_PX,
    },
};

struct DashboardWindow {
    content: Entity<HomeContent>,
}

fn frame(cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        window.simulate_next_frame(cx);
    });
    cx.run_until_parked();
}

fn render_count(content: &Entity<HomeContent>, cx: &VisualTestContext) -> usize {
    content.read_with(cx, |page, cx| page.dashboard.entity.read(cx).render_count)
}

fn choose_root(
    content: &Entity<HomeContent>,
    root: crate::home::navigation::HomeRoot,
    cx: &mut VisualTestContext,
) {
    cx.update(|window, cx| {
        content.update(cx, |page, cx| {
            page.select_root(root, window, cx);
            cx.notify();
        })
    });
    cx.run_until_parked();
}

#[gpui::test]
fn native_resize_settles_final_grid_before_two_frame_dashboard_warmup(cx: &mut TestAppContext) {
    use crate::home::{
        model::layout::RESIZE_SETTLE_DEBOUNCE,
        navigation::{HomeRoot, HomeRoute},
    };
    use std::time::Duration;
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| dashboard_window(cx));
    cx.simulate_resize(size(px(1600.0), px(900.0)));
    cx.run_until_parked();
    let content = root.read_with(cx, |root, _| root.content.clone());
    choose_root(&content, HomeRoot::Search, cx);
    let route = HomeRoute::Root(HomeRoot::Search);
    let before = render_count(&content, cx);
    let columns = content.read_with(cx, |page, _| page.layout.view_model().grid_columns);
    cx.simulate_resize(size(px(900.0), px(700.0)));
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert_eq!(page.layout.view_model().grid_columns, columns);
        assert!(!page.layout.view_model().auto_paginate);
        assert!(
            !page
                .layout
                .view_model()
                .dashboard_should_mount(&route, false)
        );
    });
    cx.executor()
        .advance_clock(RESIZE_SETTLE_DEBOUNCE - Duration::from_millis(1));
    cx.run_until_parked();
    assert!(!content.read_with(cx, |page, _| page.layout.view_model().auto_paginate));
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert!(page.layout.view_model().auto_paginate);
        assert!(page.layout.view_model().grid_columns < columns);
        assert!(
            !page
                .layout
                .view_model()
                .dashboard_should_mount(&route, false)
        );
    });
    assert_eq!(render_count(&content, cx), before);
    frame(cx);
    assert!(!content.read_with(cx, |page, _| {
        page.layout
            .view_model()
            .dashboard_should_mount(&route, false)
    }));
    frame(cx);
    assert!(content.read_with(cx, |page, _| {
        page.layout
            .view_model()
            .dashboard_should_mount(&route, false)
    }));
    assert!(render_count(&content, cx) > before);
}

#[gpui::test]
fn newer_resize_invalidates_already_queued_dashboard_frames(cx: &mut TestAppContext) {
    use crate::home::{
        model::layout::RESIZE_SETTLE_DEBOUNCE,
        navigation::{HomeRoot, HomeRoute},
    };
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| dashboard_window(cx));
    cx.simulate_resize(size(px(1600.0), px(900.0)));
    cx.run_until_parked();
    let content = root.read_with(cx, |root, _| root.content.clone());
    choose_root(&content, HomeRoot::Search, cx);
    cx.simulate_resize(size(px(900.0), px(700.0)));
    cx.run_until_parked();
    cx.executor().advance_clock(RESIZE_SETTLE_DEBOUNCE);
    cx.run_until_parked();
    frame(cx);
    cx.simulate_resize(size(px(1000.0), px(800.0)));
    cx.run_until_parked();
    frame(cx);
    let route = HomeRoute::Root(HomeRoot::Search);
    assert!(!content.read_with(cx, |page, _| {
        page.layout
            .view_model()
            .dashboard_should_mount(&route, false)
    }));
    cx.executor().advance_clock(RESIZE_SETTLE_DEBOUNCE);
    cx.run_until_parked();
    frame(cx);
    assert!(!content.read_with(cx, |page, _| {
        page.layout
            .view_model()
            .dashboard_should_mount(&route, false)
    }));
    frame(cx);
    assert!(content.read_with(cx, |page, _| {
        page.layout
            .view_model()
            .dashboard_should_mount(&route, false)
    }));
}

#[gpui::test]
fn home_return_reuses_cached_frame_until_hidden_data_or_images_change(cx: &mut TestAppContext) {
    use crate::home::{model::LoadState, navigation::HomeRoot};
    use crate::player::PlaybackStateUpdate;
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| dashboard_window(cx));
    cx.run_until_parked();
    let content = root.read_with(cx, |root, _| root.content.clone());
    content.update(cx, |page, _| {
        page.controller.test_state_mut().feed.state.home_snapshot = LoadState::Loaded
    });
    // Establish a cached frame for the normal window bounds.
    frame(cx);
    choose_root(&content, HomeRoot::Search, cx);
    let before = render_count(&content, cx);
    choose_root(&content, HomeRoot::Home, cx);
    assert_eq!(render_count(&content, cx), before);
    choose_root(&content, HomeRoot::Search, cx);
    let before = render_count(&content, cx);
    content.update(cx, |page, cx| {
        page.apply_playback_update(
            PlaybackStateUpdate {
                item_id: "movie-0".into(),
                list_item_id: "movie-0".into(),
                media_source_id: "source".into(),
                media_source_name: None,
                series_id: None,
                season_id: None,
                position_ticks: 10_000_000,
                run_time_ticks: Some(100_000_000),
                ended: false,
                failed: false,
                selected_item_id: None,
                stop_completion: None,
            },
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(render_count(&content, cx), before);
    choose_root(&content, HomeRoot::Home, cx);
    assert!(render_count(&content, cx) > before);
    let before = render_count(&content, cx);
    content.update(cx, |page, cx| {
        page.controller
            .test_state_mut()
            .feed
            .state
            .resume_items
            .as_mut()
            .unwrap()
            .items
            .clear();
        page.layout.content_changed();
        cx.notify();
    });
    cx.run_until_parked();
    assert!(render_count(&content, cx) > before);
    choose_root(&content, HomeRoot::Search, cx);
    let before = render_count(&content, cx);
    content.update(cx, |page, cx| {
        page.image_repository =
            std::sync::Arc::new(crate::images::test_support::FakeItemImages::default());
        page.ensure_image(
            crate::emby::EmbyImageRequest::primary("movie-0", Some("arrived-while-hidden".into()))
                .with_max_width(640),
            cx,
        );
    });
    cx.run_until_parked();
    content.read_with(cx, |page, _| {
        assert!(
            page.image_path_for_primary_image("movie-0", Some("arrived-while-hidden"))
                .is_some()
        );
    });
    assert_eq!(render_count(&content, cx), before);
    choose_root(&content, HomeRoot::Home, cx);
    assert!(render_count(&content, cx) > before);
}

impl Render for DashboardWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().relative().size_full().child(
            div()
                .absolute()
                .top(px(APP_TITLEBAR_HEIGHT_PX))
                .left(px(HOME_SIDEBAR_WIDTH_PX))
                .right_0()
                .bottom_0()
                .child(self.content.clone()),
        )
    }
}

fn dashboard_window(cx: &mut Context<DashboardWindow>) -> DashboardWindow {
    let server: CachedServer = serde_json::from_value(serde_json::json!({
        "id": "resize-test",
        "endpoint": { "protocol": "Https", "address": "example.com", "port": 443, "path": "" },
        "username": "test", "password": "", "user_id": "test", "added_at_unix": 0
    }))
    .unwrap();
    let client = EmbyClient::new("resize-test".into()).unwrap();
    let content = cx.new(|cx| {
        // Populate local data without starting Home's network/cache effects.
        let mut content = HomeContent::new(server, client, cx);
        content.controller.test_state_mut().feed.state.user_views = Some(serde_json::from_value(serde_json::json!({
            "Items": (0..10).map(|index| serde_json::json!({
                "Id": format!("library-{index}"), "Name": "Movies", "Type": "CollectionFolder"
            })).collect::<Vec<_>>(),
            "TotalRecordCount": 10
        })).unwrap());
        let items = (0..20)
            .map(|index| serde_json::json!({
                "Id": format!("movie-{index}"), "Name": "Movie", "Type": "Movie", "ProductionYear": 2024
            }))
            .collect::<Vec<_>>();
        content.controller.test_state_mut().feed.state.resume_items = Some(serde_json::from_value(serde_json::json!({
            "Items": items, "TotalRecordCount": 20
        })).unwrap());
        content.controller.test_state_mut().feed.state.user_view_items_rows.insert("library-0".into(), UserViewItemsRow {
            items: Some(serde_json::from_value::<UserItems>(serde_json::json!({
                "Items": items, "TotalRecordCount": 20
            })).unwrap()),
            ..Default::default()
        });
        content
    });
    DashboardWindow { content }
}

fn assert_dashboard_insets(
    cx: &mut VisualTestContext,
    root: &Entity<DashboardWindow>,
    width: f32,
    height: f32,
) {
    let viewport = root.read_with(cx, |root, cx| {
        root.content.read(cx).home_scroll_handle.bounds()
    });
    assert_eq!(f32::from(viewport.left()), HOME_SIDEBAR_WIDTH_PX);
    assert_eq!(f32::from(viewport.top()), APP_TITLEBAR_HEIGHT_PX);
    assert_eq!(f32::from(viewport.right()), width);
    assert_eq!(f32::from(viewport.bottom()), height);

    let thumb = cx.update(|window, cx| {
        let scale = window.scale_factor();
        window
            .painted_quads()
            .iter()
            .find(|quad| quad.background == theme::get(cx).scrollbar_thumb.into())
            .map(|quad| quad.bounds.map(|value| px(value.0 / scale)))
    });
    if let Some(thumb) = thumb {
        assert_eq!(thumb.size.width, px(SCROLLBAR_WIDTH_PX));
        assert_eq!(
            viewport.right() - thumb.right(),
            px(SCROLLBAR_RIGHT_INSET_PX)
        );
    }

    for selector in [
        "user-views-row",
        "resume-items-row",
        "user-view-items-row-library-0",
        "view-all-library-0",
    ] {
        let bounds = cx.debug_bounds(selector).expect("visible Home row");
        if selector != "view-all-library-0" {
            assert_eq!(
                f32::from(bounds.left() - viewport.left()),
                HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX,
                "left inset changed for {selector} at {width}x{height}",
            );
        }
        assert_eq!(
            width - f32::from(bounds.right()),
            HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX,
            "right inset changed for {selector} at {width}x{height}",
        );
        if let Some(thumb) = thumb {
            assert_eq!(
                thumb.left() - bounds.right(),
                px(HOME_MAIN_SCROLL_CONTENT_RIGHT_PADDING_PX),
                "content overlaps the scrollbar for {selector} at {width}x{height}",
            );
        }
    }
}

#[gpui::test]
fn home_horizontal_padding_stays_constant_through_resize_steps(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| dashboard_window(cx));

    // Cross several former 32px layout boundaries in both directions. The
    // viewport and all right-aligned controls must follow each native frame.
    for width in (1_100..=1_165).chain((1_050..=1_164).rev()) {
        cx.simulate_resize(size(px(width as f32), px(800.0)));
        cx.run_until_parked();
        assert_dashboard_insets(cx, &root, width as f32, 800.0);
    }

    for (width, height) in [(1_600.0, 960.0), (900.0, 720.0), (1_101.0, 801.0)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        assert_dashboard_insets(cx, &root, width, height);
    }
}
