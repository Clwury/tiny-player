use gpui::{
    AppContext as _, Context, Entity, IntoElement, Modifiers, MouseButton, ParentElement, Pixels,
    Point, Render, Styled, TestAppContext, VisualTestContext, Window, div, point, px, size,
};

use crate::{emby::EmbyClient, server::CachedServer, theme, ui::titlebar::app_titlebar};

use super::{HomeContent, HomePage, HomeRoot, HomeRoute};

const MENU_OPTIONS: [&str; 2] = ["resume-item-mark-played", "resume-item-hide-from-resume"];

struct ResumeMenuWindow {
    page: Entity<HomePage>,
}

impl Render for ResumeMenuWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(app_titlebar(window, cx, "Resume menu test".into()))
            .child(self.page.clone())
    }
}

fn resume_menu_window(cx: &mut TestAppContext) -> (Entity<HomeContent>, &mut VisualTestContext) {
    cx.update(theme::init);
    let (root, cx) = cx.add_window_view(|_, cx| {
        let server: CachedServer = serde_json::from_value(serde_json::json!({
            "id": "resume-menu-test",
            "endpoint": { "protocol": "Https", "address": "", "port": 443, "path": "" },
            "username": "test", "password": "", "user_id": "test", "added_at_unix": 0
        }))
        .unwrap();
        let page = cx.new(|cx| {
            let home_content = cx.new(|cx| {
                // Seed local data without starting Home's network/cache effects.
                let mut content = HomeContent::new(
                    server.clone(),
                    EmbyClient::new("resume-menu-test".into()).unwrap(),
                    cx,
                );
                content.resume_items = Some(
                    serde_json::from_value(serde_json::json!({
                        "Items": [
                            { "Id": "first", "Name": "First movie", "Type": "Movie" },
                            { "Id": "second", "Name": "Second movie", "Type": "Movie" }
                        ],
                        "TotalRecordCount": 2
                    }))
                    .unwrap(),
                );
                content
            });
            HomePage {
                current_server: server.clone(),
                servers: vec![server],
                home_content,
            }
        });
        ResumeMenuWindow { page }
    });
    let content = root.read_with(cx, |root, cx| root.page.read(cx).home_content.clone());
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    (content, cx)
}

fn click(cx: &mut VisualTestContext, position: Point<Pixels>, button: MouseButton) {
    cx.simulate_mouse_move(position, None, Modifiers::default());
    cx.simulate_mouse_down(position, button, Modifiers::default());
    cx.simulate_mouse_up(position, button, Modifiers::default());
    cx.run_until_parked();
}

fn open_menu(cx: &mut VisualTestContext, selector: &'static str) {
    let card = cx.debug_bounds(selector).unwrap();
    click(cx, card.center(), MouseButton::Right);
    assert!(cx.debug_bounds("resume-item-context-menu").is_some());
}

fn assert_hovered_option(cx: &mut VisualTestContext, hovered: Option<&str>) {
    for selector in MENU_OPTIONS {
        let bounds = cx.debug_bounds(selector).unwrap();
        let painted_hover = cx.update(|window, cx| {
            window.painted_quads().iter().any(|quad| {
                quad.bounds == bounds.scale(window.scale_factor())
                    && quad.background == theme::get(cx).secondary_hover.into()
            })
        });
        assert_eq!(painted_hover, hovered == Some(selector), "{selector}");
    }
}

#[gpui::test]
fn resume_menu_items_hover_independently_across_themes(cx: &mut TestAppContext) {
    let (_, cx) = resume_menu_window(cx);
    open_menu(cx, "resume-item-card-first");

    for selection in theme::ColorTheme::ALL {
        cx.update(|_, cx| theme::set(selection, cx));
        cx.run_until_parked();
        for hovered in [None, Some(MENU_OPTIONS[0]), Some(MENU_OPTIONS[1]), None] {
            let position = hovered
                .map(|selector| cx.debug_bounds(selector).unwrap().center())
                .unwrap_or(point(px(1050.0), px(700.0)));
            cx.simulate_mouse_move(position, None, Modifiers::default());
            cx.run_until_parked();
            assert_hovered_option(cx, hovered);
        }
    }
}

#[gpui::test]
fn resume_menu_dismisses_on_sidebar_content_and_titlebar_clicks(cx: &mut TestAppContext) {
    let (content, cx) = resume_menu_window(cx);
    let home = cx.debug_bounds("home-section").unwrap().center();
    let server = cx
        .debug_bounds("sidebar-server-resume-menu-test")
        .unwrap()
        .center();

    for position in [
        home,
        server,
        point(px(12.0), px(450.0)),
        point(px(1050.0), px(700.0)),
        point(px(500.0), px(15.0)),
    ] {
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            open_menu(cx, "resume-item-card-first");
            click(cx, position, button);
            assert!(
                content.read_with(cx, |page, _| page.resume_item_context_menu.is_none()),
                "menu stayed open after {button:?} click at {position:?}"
            );
            assert!(cx.debug_bounds("resume-item-context-menu").is_none());
        }
    }

    open_menu(cx, "resume-item-card-first");
    let search = cx.debug_bounds("search-section").unwrap().center();
    click(cx, search, MouseButton::Left);
    content.read_with(cx, |page, _| {
        assert!(page.resume_item_context_menu.is_none());
        assert_eq!(
            page.navigation.current(),
            &HomeRoute::Root(HomeRoot::Search)
        );
    });
}

#[gpui::test]
fn resume_menu_keeps_inside_clicks_and_retargets_another_card(cx: &mut TestAppContext) {
    let (content, cx) = resume_menu_window(cx);
    open_menu(cx, "resume-item-card-first");
    let menu = cx.debug_bounds("resume-item-context-menu").unwrap();
    for button in [MouseButton::Left, MouseButton::Right] {
        click(cx, menu.origin + point(px(2.0), px(12.0)), button);
        assert!(cx.debug_bounds("resume-item-context-menu").is_some());
    }

    open_menu(cx, "resume-item-card-second");
    content.update(cx, |page, cx| {
        assert_eq!(
            page.resume_item_context_menu.as_ref().unwrap().item_id,
            "second"
        );
        page.resume_item_requests.insert("second".into());
        cx.notify();
    });
    cx.run_until_parked();
    for selector in MENU_OPTIONS {
        let option = cx.debug_bounds(selector).unwrap();
        click(cx, option.center(), MouseButton::Left);
        assert_hovered_option(cx, None);
        content.read_with(cx, |page, _| {
            assert_eq!(
                page.resume_item_context_menu.as_ref().unwrap().item_id,
                "second"
            );
            assert_eq!(page.resume_item_requests.len(), 1);
        });
    }
}
