use super::*;

fn pause_sidebar_switch(
    app: &Entity<TinyApp>,
    cx: &mut VisualTestContext,
) -> (CachedServer, RequestToken, gpui::EntityId) {
    let (requested, token, home_id) = app.update(cx, |app, cx| {
        let Page::Home(home) = app.shell.page() else {
            panic!("expected home");
        };
        let home_id = home.entity_id();
        let requested = app.server_feature.catalog().servers[2].clone();
        app.begin_select_server(&requested, cx);
        assert!(matches!(app.shell.page(), Page::Home(home) if home.entity_id() == home_id));
        assert_eq!(
            app.server_feature.selecting_server_id(),
            Some(requested.id.as_str())
        );
        // Complete the request explicitly below so the pending state is deterministic.
        app.server_effects.auth.cancel();
        let token = app.server_feature.authentication_token().unwrap();
        (requested, token, home_id)
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("sidebar-server-loading-login-required")
            .is_some()
    );
    assert!(cx.debug_bounds("sidebar-server-first").is_some());
    assert!(cx.debug_bounds("server-card-first").is_none());
    (requested, token, home_id)
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
        let (requested, token, original_home) = pause_sidebar_switch(&app, cx);
        let result = if success {
            Ok(CachedServer {
                access_token: Some("new-token".into()),
                ..requested.clone()
            })
        } else {
            Err(anyhow::anyhow!("login rejected"))
        };
        app.update(cx, |app, cx| {
            app.finish_select_server(requested, token, result, cx)
        });
        cx.run_until_parked();
        app.read_with(cx, |app, cx| {
            let Page::Home(home) = app.shell.page() else {
                panic!("must stay on Home");
            };
            assert!(app.server_feature.selecting_server_id().is_none());
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
                assert!(
                    app.server_feature.catalog().servers[2]
                        .access_token
                        .is_none()
                );
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
fn reselecting_the_same_server_rejects_the_cancelled_attempt(cx: &mut TestAppContext) {
    let (app, cx) = sidebar_window(cx);
    let (requested, old, _) = pause_sidebar_switch(&app, cx);
    app.update(cx, |app, cx| {
        app.cancel_server_selection(cx);
        app.begin_select_server(&requested, cx);
        app.server_effects.auth.cancel();
        let current = app.server_feature.authentication_token().unwrap();
        assert_ne!(current, old);
        app.finish_select_server(
            requested.clone(),
            old,
            Err(anyhow::anyhow!("old failure")),
            cx,
        );
        assert_eq!(
            app.server_feature.selecting_server_id(),
            Some(requested.id.as_str())
        );
        assert!(!app.has_app_notifications());
        app.finish_select_server(
            requested,
            current,
            Err(anyhow::anyhow!("current failure")),
            cx,
        );
        assert!(app.server_feature.selecting_server_id().is_none());
        assert!(app.has_app_notifications());
    });
}

#[gpui::test]
fn cancelling_or_replacing_sidebar_switch_ignores_its_late_response(cx: &mut TestAppContext) {
    for action in ["current", "other", "back", "add"] {
        let (app, cx) = sidebar_window(cx);
        let (requested, token, original_home) = pause_sidebar_switch(&app, cx);
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
            assert!(app.server_feature.selecting_server_id().is_none());
            app.finish_select_server(
                requested.clone(),
                token,
                Ok(CachedServer {
                    access_token: Some("late-token".into()),
                    ..requested
                }),
                cx,
            );
            assert!(
                app.server_feature.catalog().servers[2]
                    .access_token
                    .is_none()
            );
            match app.shell.page() {
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
