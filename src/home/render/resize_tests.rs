use gpui::{
    AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    VisualTestContext, Window, div, px, size,
};

use crate::{
    emby::{EmbyClient, UserItems},
    home::{
        HomeContent, UserViewItemsRow,
        carousel::{HOME_MAIN_SCROLLBAR_WIDTH_PX, HOME_SIDEBAR_WIDTH_PX},
    },
    server::CachedServer,
    theme,
    ui::titlebar::APP_TITLEBAR_HEIGHT_PX,
};

struct DashboardWindow {
    content: Entity<HomeContent>,
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
        content.user_views = Some(serde_json::from_value(serde_json::json!({
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
        content.resume_items = Some(serde_json::from_value(serde_json::json!({
            "Items": items, "TotalRecordCount": 20
        })).unwrap());
        content.user_view_items_rows.insert("library-0".into(), UserViewItemsRow {
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

    for selector in [
        "user-views-row",
        "resume-items-row",
        "user-view-items-row-library-0",
        "view-all-library-0",
    ] {
        let bounds = cx.debug_bounds(selector).expect("visible Home row");
        assert_eq!(
            width - f32::from(bounds.right()),
            24.0 + HOME_MAIN_SCROLLBAR_WIDTH_PX,
            "right inset changed for {selector} at {width}x{height}",
        );
    }
}

#[gpui::test]
fn home_right_padding_stays_constant_through_resize_steps(cx: &mut TestAppContext) {
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
