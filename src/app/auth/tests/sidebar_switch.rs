use super::*;

fn pause_sidebar_switch(
    app: &Entity<TinyApp>,
    cx: &mut VisualTestContext,
) -> (CachedServer, gpui::EntityId) {
    let (requested, home_id) = app.update(cx, |app, cx| {
        let Page::Home(home) = &app.page else {
            panic!("expected home");
        };
        let home_id = home.entity_id();
        let requested = app.servers[2].clone();
        app.begin_select_server(&requested, cx);
        assert!(matches!(&app.page, Page::Home(home) if home.entity_id() == home_id));
        assert_eq!(
            app.selecting_server_id.as_deref(),
            Some(requested.id.as_str())
        );
        // Complete the request explicitly below so the pending state is deterministic.
        app.select_server_task = Task::ready(());
        (requested, home_id)
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sidebar-server-loading-login-required")
            .is_some()
    );
    assert!(cx.debug_bounds("sidebar-server-first").is_some());
    assert!(cx.debug_bounds("server-card-first").is_none());
    (requested, home_id)
}

#[gpui::test]
fn sidebar_login_preserves_home_until_success_and_stays_on_home_on_failure(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    for success in [true, false] {
        let path = temp.path().join(format!("servers-{success}.json"));
        let (app, cx) = sidebar_window(cx);
        app.update(cx, |app, _| app.cache_save_path = Some(path.clone()));
        let (requested, original_home) = pause_sidebar_switch(&app, cx);
        let result = if success {
            Ok(CachedServer {
                access_token: Some("new-token".into()),
                ..requested.clone()
            })
        } else {
            Err(anyhow::anyhow!("login rejected"))
        };
        app.update(cx, |app, cx| {
            app.finish_select_server(requested, result, cx)
        });
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            let Page::Home(home) = &app.page else {
                panic!("must stay on Home");
            };
            assert!(app.selecting_server_id.is_none());
            if success {
                assert_ne!(home.entity_id(), original_home);
                assert_eq!(home.read(cx).current_server_id(), "login-required");
                assert_eq!(
                    crate::storage::load_or_init_from(&path).unwrap().servers[2]
                        .access_token
                        .as_deref(),
                    Some("new-token")
                );
            } else {
                assert_eq!(home.entity_id(), original_home);
                assert!(app.has_app_notifications());
                assert!(app.servers[2].access_token.is_none());
                assert!(!path.exists());
            }
        });
        assert!(
            cx.debug_bounds("sidebar-server-loading-login-required")
                .is_none()
        );
        assert!(cx.debug_bounds("server-card-first").is_none());
    }
}

#[gpui::test]
fn cancelling_or_replacing_sidebar_switch_ignores_its_late_response(cx: &mut TestAppContext) {
    for action in ["current", "other", "back", "add"] {
        let (app, cx) = sidebar_window(cx);
        let (requested, original_home) = pause_sidebar_switch(&app, cx);
        let selector = match action {
            "current" => "sidebar-server-first",
            "other" => "sidebar-server-second",
            "back" => "home-back",
            _ => "sidebar-add-server",
        };
        let target = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(target.center(), Modifiers::default());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            assert!(app.selecting_server_id.is_none());
            app.finish_select_server(
                requested.clone(),
                Ok(CachedServer {
                    access_token: Some("late-token".into()),
                    ..requested
                }),
                cx,
            );
            assert!(app.servers[2].access_token.is_none());
            match &app.page {
                Page::Home(home) => {
                    if action == "other" {
                        assert_eq!(home.read(cx).current_server_id(), "second");
                    } else {
                        assert_eq!(home.entity_id(), original_home);
                    }
                }
                Page::Servers => assert_eq!(action, "back"),
                Page::Playback { .. } => panic!("unexpected playback"),
            }
            assert_eq!(app.add_server_dialog.is_some(), action == "add");
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-server-loading-login-required")
                .is_none()
        );
    }
}
