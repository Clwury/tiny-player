use gpui::{Entity, Render, TestAppContext};

use crate::{
    server::CachedItemCounts,
    storage::{self, ServerCache},
};

use super::*;

fn app(cx: &mut TestAppContext, path: &std::path::Path) -> Entity<TinyApp> {
    cx.update(|cx| {
        theme::init(cx);
        Editor::bind_keys(cx);
    });
    cx.new(|cx| {
        let mut cache = ServerCache::empty();
        cache.servers = ["first", "second"].into_iter().map(|id| {
            serde_json::from_value(serde_json::json!({
                "id": id, "server_name": id,
                "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
                "username": id, "password": "", "added_at_unix": 0
            })).unwrap()
        }).collect();
        let mut app = TinyApp::new(cache, None, cx);
        app.cache_save_path = Some(path.to_owned());
        app.window_persistence_enabled = false;
        app
    })
}

struct Root(Entity<TinyApp>);

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.0.clone()
    }
}

#[gpui::test]
fn applying_an_icon_updates_only_the_target_and_persists_the_manual_choice(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("servers.json");
    let app = app(cx, &path);
    // Exercise the controller without rendering remote catalog previews.
    let window = cx.add_window(|_, _| gpui::Empty);
    let selected = all_icons()[2].url.clone();
    window
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                let previous_focus = cx.focus_handle();
                previous_focus.focus(window, cx);
                let server = app.servers[1].clone();
                app.open_server_icon_picker(&server, window, cx);
                let picker = app.server_icon_picker.as_mut().unwrap();
                let session = picker.session;
                picker.pending_url = Some(selected.clone());
                assert!(picker.focus.is_focused(window));
                app.cache.servers[1].item_counts = Some(CachedItemCounts {
                    movie_count: 12,
                    series_count: 34,
                });
                app.finish_select_server_icon(session, selected.clone(), Ok(()), window, cx);
                assert!(app.server_icon_picker.is_none());
                assert!(previous_focus.is_focused(window));
                assert!(app.servers[0].icon_url.is_none());
                assert!(!app.servers[0].icon_is_custom);
                assert_eq!(app.servers[1].icon_url.as_deref(), Some(selected.as_str()));
                assert!(app.servers[1].icon_is_custom);
                assert_eq!(app.servers[1].item_counts.as_ref().unwrap().movie_count, 12);
            })
        })
        .unwrap();
    let saved = storage::load_or_init_from(&path).unwrap();
    assert_eq!(
        saved.servers[1].icon_url.as_deref(),
        Some(selected.as_str())
    );
    assert!(saved.servers[1].icon_is_custom);
    assert!(saved.servers[0].icon_url.is_none());
}

#[gpui::test]
fn download_and_settings_save_failures_keep_the_original_icon_and_allow_retry(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("servers.json");
    let app = app(cx, &path);
    let window = cx.add_window(|_, _| gpui::Empty);
    window
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                let server = app.servers[0].clone();
                app.open_server_icon_picker(&server, window, cx);
                let selected = all_icons()[0].url.clone();
                let session = app.server_icon_picker.as_ref().unwrap().session;
                for result in [Err(anyhow::anyhow!("下载失败")), Ok(())] {
                    app.server_icon_picker.as_mut().unwrap().pending_url = Some(selected.clone());
                    app.finish_select_server_icon(session, selected.clone(), result, window, cx);
                    let picker = app.server_icon_picker.as_ref().unwrap();
                    assert!(picker.error.is_some());
                    assert!(picker.pending_url.is_none());
                    assert!(app.servers[0].icon_url.is_none());
                    assert!(app.cache.servers[0].icon_url.is_none());
                    assert!(!app.cache.servers[0].icon_is_custom);
                    // The second completion fails when saving settings, after a valid download.
                    std::fs::write(&path, b"not a directory").unwrap();
                    app.cache_save_path = Some(path.join("servers.json"));
                }
                app.dismiss_server_icon_picker(window, cx);
            })
        })
        .unwrap();
}

#[gpui::test]
fn closed_picker_completions_cannot_change_a_new_picker_or_select_a_server(
    cx: &mut TestAppContext,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("servers.json");
    let app = app(cx, &path);
    let window = cx.add_window(|_, _| gpui::Empty);
    window
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                let first = app.servers[0].clone();
                let second = app.servers[1].clone();
                app.open_server_icon_picker(&first, window, cx);
                let old_session = app.server_icon_picker.as_ref().unwrap().session;
                app.dismiss_server_icon_picker(window, cx);
                app.open_server_icon_picker(&second, window, cx);
                app.finish_select_server_icon(
                    old_session,
                    all_icons()[0].url.clone(),
                    Ok(()),
                    window,
                    cx,
                );
                assert_eq!(
                    app.server_icon_picker.as_ref().unwrap().server_id,
                    second.id
                );
                assert!(app.servers.iter().all(|server| server.icon_url.is_none()));
                app.begin_select_server(&first, cx);
                assert!(app.selecting_server_id.is_none());
                assert!(matches!(app.page, Page::Servers));
                app.dismiss_server_icon_picker(window, cx);
            })
        })
        .unwrap();
    assert!(!path.exists());
}

#[gpui::test]
fn context_menu_opens_a_scrollable_modal_and_dismissal_releases_previews(cx: &mut TestAppContext) {
    use crate::ui::server_icon::tests::{has_preview, stub_previews};
    use gpui::{Bounds, Corners, Modifiers, ScaledPixels, ScrollStrategy, point, size};

    let directory = tempfile::tempdir().unwrap();
    let app = app(cx, &directory.path().join("servers.json"));
    cx.update(stub_previews);
    let (_, cx) = cx.add_window_view(|_, _| Root(app.clone()));
    cx.simulate_resize(size(px(960.0), px(680.0)));
    cx.run_until_parked();
    let position = cx.debug_bounds("server-card-second").unwrap().center();
    cx.simulate_mouse_down(position, MouseButton::Right, Modifiers::default());
    cx.simulate_mouse_up(position, MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    let menu_item = cx
        .debug_bounds("server-context-menu-选择图标")
        .unwrap()
        .center();
    cx.simulate_click(menu_item, Modifiers::default());
    cx.run_until_parked();
    let panel = cx.debug_bounds("server-icon-picker").unwrap();
    assert_eq!(panel.size, size(px(760.0), px(600.0)));
    assert!(cx.debug_bounds("server-context-menu").is_none());
    assert!(cx.debug_bounds("server-icon-choice-0").is_some());
    // Check the painted backdrop itself: a parent's rounded bounds do not
    // prevent a square child background from covering the transparent corners.
    for fullscreen in [false, true, false] {
        cx.update(|window, _| {
            if window.is_fullscreen() != fullscreen {
                window.toggle_fullscreen();
                window.refresh();
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            let theme = theme::get(cx);
            let scale = window.scale_factor();
            // TestWindow uses system decorations on Linux.
            let inset = if cfg!(any(target_os = "windows", target_os = "linux")) || fullscreen {
                0.0
            } else {
                1.0
            };
            let bounds = Bounds::new(
                point(px(0.0), px(0.0)),
                size(window.viewport_size().width, window.viewport_size().height),
            )
            .inset(px(inset))
            .scale(scale);
            let quads = window.painted_quads();
            let backdrop = quads
                .iter()
                .filter(|quad| quad.bounds == bounds && quad.background == theme.overlay.into())
                .collect::<Vec<_>>();
            assert!(!backdrop.is_empty());
            let radius = if cfg!(any(target_os = "windows", target_os = "linux")) || fullscreen {
                0.0
            } else {
                (f32::from(theme.radius_lg) - 1.0).max(0.0) * scale
            };
            for quad in backdrop {
                assert_eq!(quad.corner_radii, Corners::all(ScaledPixels(radius)));
            }
        });
    }
    let session = app.read_with(cx, |app, cx| {
        let picker = app.server_icon_picker.as_ref().unwrap();
        assert_eq!(picker.server_id, "second");
        assert!(has_preview(picker.session, &all_icons()[0].url, cx));
        picker.session
    });
    // Clicking the panel's own header must not dismiss it.
    cx.simulate_click(
        panel.origin + point(px(30.0), px(30.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-picker").is_some());
    app.update(cx, |app, cx| {
        let last_row = all_icons().len().div_ceil(7) - 1;
        app.server_icon_picker
            .as_ref()
            .unwrap()
            .scroll
            .scroll_to_item(last_row, ScrollStrategy::Bottom);
        cx.notify();
    });
    cx.run_until_parked();
    let last = format!("server-icon-choice-{}", all_icons().len() - 1).leak();
    let last_bounds = cx.debug_bounds(last).unwrap();
    assert!(panel.contains(&last_bounds.center()));
    assert!(cx.debug_bounds("server-icon-choice-0").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-picker").is_none());
    app.read_with(cx, |app, cx| {
        assert!(app.server_icon_picker.is_none());
        assert!(app.selecting_server_id.is_none());
        assert!(!has_preview(session, &all_icons()[0].url, cx));
        assert!(!has_preview(session, &all_icons().last().unwrap().url, cx));
    });
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let server = app.servers[0].clone();
            app.open_server_icon_picker(&server, window, cx);
            assert_ne!(app.server_icon_picker.as_ref().unwrap().session, session);
        })
    });
    cx.run_until_parked();
    // This is over a server card under the backdrop: close without entering it.
    cx.simulate_click(point(px(40.0), px(60.0)), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-picker").is_none());
    app.read_with(cx, |app, _| {
        assert!(app.selecting_server_id.is_none());
        assert!(matches!(app.page, Page::Servers));
    });
}

#[gpui::test]
fn searching_names_resets_scroll_preserves_catalog_indices_and_can_be_cleared(
    cx: &mut TestAppContext,
) {
    use crate::ui::server_icon::tests::stub_previews;
    use gpui::{Modifiers, ScrollStrategy, size};

    let directory = tempfile::tempdir().unwrap();
    let app = app(cx, &directory.path().join("servers.json"));
    cx.update(stub_previews);
    let (_, cx) = cx.add_window_view(|_, _| Root(app.clone()));
    cx.simulate_resize(size(px(960.0), px(680.0)));
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let server = app.servers[1].clone();
            app.open_server_icon_picker(&server, window, cx);
        })
    });
    cx.run_until_parked();
    let panel = cx.debug_bounds("server-icon-picker").unwrap();
    app.update(cx, |app, cx| {
        app.server_icon_picker
            .as_ref()
            .unwrap()
            .scroll
            .scroll_to_item(all_icons().len().div_ceil(7) - 1, ScrollStrategy::Bottom);
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-choice-0").is_none());
    let search = cx.debug_bounds("server-icon-search").unwrap().center();
    cx.simulate_click(search, Modifiers::default());
    cx.simulate_input("  ALPHAtV  ");
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-search-empty").is_none());
    assert!(cx.debug_bounds("server-icon-choice-0").is_none());
    let matching: Vec<_> = all_icons()
        .iter()
        .enumerate()
        .filter(|(_, icon)| icon.name.to_lowercase().contains("alphatv"))
        .collect();
    assert!(matching.len() > 1);
    for (index, _) in matching {
        let selector = format!("server-icon-choice-{index}").leak();
        let bounds = cx.debug_bounds(selector).unwrap();
        assert!(panel.contains(&bounds.center()));
    }
    app.read_with(cx, |app, _| {
        assert_eq!(
            app.server_icon_picker
                .as_ref()
                .unwrap()
                .scroll
                .0
                .borrow()
                .base_handle
                .offset()
                .y,
            px(0.0)
        );
    });
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("no-such-icon-name");
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-search-empty").is_some());
    cx.simulate_keystrokes("ctrl-a backspace");
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-search-empty").is_none());
    assert!(cx.debug_bounds("server-icon-choice-0").is_some());

    let close = cx
        .debug_bounds("close-server-icon-picker")
        .unwrap()
        .center();
    cx.simulate_click(close, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-picker").is_none());
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            let server = app.servers[1].clone();
            app.open_server_icon_picker(&server, window, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_click(search, Modifiers::default());
    cx.simulate_input("emby");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("server-icon-picker").is_none());
    app.read_with(cx, |app, _| {
        assert!(app.servers.iter().all(|server| server.icon_url.is_none()));
        assert!(app.selecting_server_id.is_none());
    });
}
