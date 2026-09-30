use super::*;
use crate::{
    player::{PlaybackEvent, PlaybackPage, PlaybackStateUpdate, PlaybackVolumeSettings},
    search_history::SearchHistory,
};

fn history(text: &str) -> SearchHistory {
    let mut history = SearchHistory::default();
    history.record(text);
    history
}

fn update() -> PlaybackStateUpdate {
    PlaybackStateUpdate {
        item_id: "item".into(),
        list_item_id: "item".into(),
        media_source_id: "source".into(),
        media_source_name: None,
        series_id: None,
        season_id: None,
        position_ticks: 125_000_000,
        run_time_ticks: None,
        failed: false,
        ended: false,
        selected_item_id: None,
        stop_completion: None,
    }
}

#[gpui::test]
fn unmounted_home_events_cannot_change_route_overlays_or_persisted_history(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    let (app, cx) = sidebar_window(cx);
    let (old_home, old_token) = app.update(cx, |app, cx| {
        app.cache_save_path = Some(temp.path().join("servers.json"));
        let old = app.shell.home().unwrap().clone();
        let token = app.shell.home_token().unwrap();
        app.open_home_for_server(app.server_feature.catalog().servers[1].clone(), cx);
        (old, token)
    });
    cx.run_until_parked();
    let current = app.read_with(cx, |app, _| app.shell.home().unwrap().clone());
    assert_ne!(current.entity_id(), old_home.entity_id());
    old_home.update(cx, |_, cx| {
        cx.emit(HomeEvent::BackToServers);
        cx.emit(HomeEvent::AddServer);
        cx.emit(HomeEvent::SearchHistoryChanged(history("stale")));
    });
    app.update(cx, |app, cx| {
        // Also exercise an event already delivered to the runner before unmount.
        app.dispatch_app_intent(
            AppIntent::Home {
                source: old_token.clone(),
                event: HomeEvent::BackToServers,
            },
            cx,
        );
        app.dispatch_app_intent(
            AppIntent::Home {
                source: old_token,
                event: HomeEvent::SearchHistoryChanged(history("stale")),
            },
            cx,
        );
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert_eq!(app.shell.home().unwrap().entity_id(), current.entity_id());
        assert!(app.add_server_dialog.is_none());
        assert!(app.cache.search_history.entries().is_empty());
    });
    current.update(cx, |_, cx| {
        cx.emit(HomeEvent::SearchHistoryChanged(history("current")))
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert_eq!(app.cache.search_history.entries(), ["current"])
    });
    current.update(cx, |_, cx| cx.emit(HomeEvent::BackToServers));
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert!(matches!(app.shell.page(), Page::Servers))
    });
}

#[gpui::test]
fn replaced_playback_cannot_change_volume_or_back_route_and_home_is_reused(
    cx: &mut TestAppContext,
) {
    let temp = tempfile::tempdir().unwrap();
    let (app, cx) = sidebar_window(cx);
    let home = app.read_with(cx, |app, _| app.shell.home().unwrap().clone());
    let first = cx.new(PlaybackPage::test_fixture);
    let second = cx.new(PlaybackPage::test_fixture);
    let token = app.update(cx, |app, cx| {
        app.cache_save_path = Some(temp.path().join("servers.json"));
        app.mount_playback_page(first.clone(), cx);
        app.shell.playback_token().unwrap()
    });
    first.update(cx, |_, cx| {
        cx.emit(PlaybackEvent::VolumeChanged {
            settings: PlaybackVolumeSettings {
                level: 0.42,
                unmuted_level: 0.42,
            },
        })
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert_eq!(app.cache.playback_volume.level, 0.42)
    });
    app.update(cx, |app, cx| app.mount_playback_page(second.clone(), cx));
    first.update(cx, |_, cx| {
        cx.emit(PlaybackEvent::VolumeChanged {
            settings: PlaybackVolumeSettings {
                level: 0.9,
                unmuted_level: 0.9,
            },
        });
        cx.emit(PlaybackEvent::Back { update: update() });
    });
    app.update(cx, |app, cx| {
        app.dispatch_app_intent(
            AppIntent::Playback {
                source: token,
                event: PlaybackEvent::Back { update: update() },
            },
            cx,
        )
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert!(matches!(app.shell.page(), Page::Playback { page, return_to } if page.entity_id() == second.entity_id() && return_to.entity_id() == home.entity_id()));
        assert_eq!(app.cache.playback_volume.level, 0.42);
    });
    second.update(cx, |_, cx| {
        cx.emit(PlaybackEvent::VolumeChanged {
            settings: PlaybackVolumeSettings {
                level: 0.3,
                unmuted_level: 0.3,
            },
        });
        cx.emit(PlaybackEvent::Back { update: update() });
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert!(
            matches!(app.shell.page(), Page::Home(page) if page.entity_id() == home.entity_id())
        );
        assert_eq!(app.cache.playback_volume.level, 0.3);
    });
    second.update(cx, |_, cx| {
        cx.emit(PlaybackEvent::VolumeChanged {
            settings: PlaybackVolumeSettings::default(),
        })
    });
    home.update(cx, |_, cx| {
        cx.emit(HomeEvent::SearchHistoryChanged(history("retained")))
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert_eq!(app.cache.playback_volume.level, 0.3);
        assert_eq!(app.cache.search_history.entries(), ["retained"]);
    });
}
