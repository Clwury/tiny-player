use super::{PlaybackStopResult, controller::*, test_support::Session};

#[test]
fn prepared_failure_closes_without_reporting_and_cannot_restart() {
    let mut session = Session::new();
    assert!(
        session
            .dispatch(ReportingIntent::Progress { force: true })
            .command
            .is_none()
    );
    assert!(session.controller.begin_periodic().is_none());
    let closed = session.dispatch(ReportingIntent::Close {
        failed: true,
        ended: false,
    });
    assert!(closed.command.is_none());
    let update = closed.update.unwrap();
    assert!(update.failed);
    assert!(update.stop_completion.is_none());
    assert_eq!(update.position_ticks, 25_000_000);
    assert!(session.dispatch(ReportingIntent::Restart).command.is_none());
    assert!(session.controller.is_closed());
}

#[test]
fn first_restart_reports_grouped_queue_tracks_and_precise_telemetry() {
    let mut session = Session::new();
    session.telemetry.drag_position = Some(3.1234567);
    session.telemetry.user_paused = true;
    let started = session.dispatch(ReportingIntent::Restart);
    assert!(started.started);
    let command = started.command.unwrap();
    let PlaybackReport::Started(report) = &command.report else {
        panic!("expected start");
    };
    assert_eq!(report.item_id, "physical");
    assert_eq!(report.media_source_id, "source");
    assert_eq!(report.play_session_id.as_deref(), Some("session"));
    assert_eq!(report.playlist_item_id, "playlistItem1");
    assert_eq!(report.playlist_index, 1);
    assert_eq!(report.playlist_length, 3);
    assert_eq!(
        report
            .now_playing_queue
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["previous", "physical", "next"]
    );
    assert_eq!(
        report.now_playing_queue[1].playlist_item_id,
        "playlistItem1"
    );
    assert_eq!(report.position_ticks, 31_234_567);
    assert_eq!(report.run_time_ticks, Some(900_000_000));
    assert_eq!(report.audio_stream_index, Some(2));
    assert_eq!(report.subtitle_stream_index, Some(4));
    assert_eq!(report.volume_level, 76);
    assert!(report.is_paused && report.can_seek && !report.is_muted);
    let restart = session.dispatch(ReportingIntent::Restart);
    assert!(!restart.started);
    assert!(matches!(
        &restart.command.unwrap().report,
        PlaybackReport::Progress(_)
    ));
}

#[test]
fn progress_deduplicates_but_forced_and_changed_snapshots_report() {
    let mut session = Session::new();
    session.dispatch(ReportingIntent::Restart);
    assert!(
        session
            .dispatch(ReportingIntent::Progress { force: false })
            .command
            .is_none()
    );
    assert!(
        session
            .dispatch(ReportingIntent::Progress { force: true })
            .command
            .is_some()
    );
    session.telemetry.position = Some(4.0);
    session.telemetry.user_paused = true;
    session.telemetry.volume = 0.0;
    session.telemetry.audio = Some(usize::MAX);
    session.telemetry.subtitle = None;
    let progress = session.dispatch(ReportingIntent::Progress { force: false });
    let command = progress.command.unwrap();
    let PlaybackReport::Progress(report) = &command.report else {
        panic!("expected progress");
    };
    assert_eq!(report.position_ticks, 40_000_000);
    assert!(report.is_paused && report.is_muted);
    assert_eq!(report.volume_level, 0);
    assert_eq!(report.audio_stream_index, None);
    assert_eq!(report.subtitle_stream_index, None);
    assert!(
        session
            .dispatch(ReportingIntent::Progress { force: false })
            .command
            .is_none()
    );
}

#[test]
fn periodic_timer_rejects_foreign_session_account_duplicates_and_closed_session() {
    let mut session = Session::new();
    session.dispatch(ReportingIntent::Restart);
    let timer = session.controller.begin_periodic().unwrap();
    assert!(session.controller.begin_periodic().is_none());
    let mut other = Session::new();
    other.dispatch(ReportingIntent::Restart);
    let foreign = other.controller.begin_periodic().unwrap();
    assert!(
        !session
            .controller
            .accept_periodic(&foreign, &session.identity)
    );
    for identity in [
        crate::effects::WorkspaceIdentity {
            local_server_id: "changed".into(),
            ..session.identity.clone()
        },
        crate::effects::WorkspaceIdentity {
            remote_server_id: None,
            ..session.identity.clone()
        },
        crate::effects::WorkspaceIdentity {
            user_id: None,
            ..session.identity.clone()
        },
    ] {
        assert!(!session.controller.accept_periodic(&timer, &identity));
    }
    assert!(
        session
            .controller
            .accept_periodic(&timer, &session.identity)
    );
    assert!(
        !session
            .controller
            .accept_periodic(&timer, &session.identity)
    );
    let next = session.controller.begin_periodic().unwrap();
    assert!(
        !session
            .controller
            .accept_periodic(&timer, &session.identity)
    );
    session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    assert!(!session.controller.accept_periodic(&next, &session.identity));
    assert!(session.controller.begin_periodic().is_none());
}

#[test]
fn close_is_terminal_and_preserves_queue_metadata_and_same_receipt() {
    let mut session = Session::new();
    session.dispatch(ReportingIntent::Restart);
    let closed = session.dispatch(ReportingIntent::Close {
        failed: true,
        ended: true,
    });
    let update = closed.update.unwrap();
    assert_eq!(update.item_id, "physical");
    assert_eq!(update.list_item_id, "grouped");
    assert_eq!(update.media_source_name.as_deref(), Some("HD"));
    assert_eq!(update.series_id.as_deref(), Some("series"));
    assert_eq!(update.season_id.as_deref(), Some("season"));
    assert_eq!(update.position_ticks, 900_000_000);
    assert!(update.failed && update.ended);
    assert!(update.selected_item_id.is_none());
    let completion = update.stop_completion.unwrap();
    let command = closed.command.unwrap();
    let PlaybackReport::Stopped(report) = &command.report else {
        panic!("expected stop");
    };
    assert_eq!(report.position_ticks, 900_000_000);
    assert_eq!(report.play_session_id.as_deref(), Some("session"));
    // Stop's historical empty playlist wire contract is deliberate.
    assert_eq!(report.playlist_length, 0);
    assert_eq!(report.playlist_index, -1);
    assert!(report.now_playing_queue.is_empty());
    assert!(report.failed);
    assert_eq!(completion.result(), PlaybackStopResult::Pending);
    command.finish(&session.identity, true);
    assert_eq!(completion.result(), PlaybackStopResult::Succeeded);
    session.telemetry.position = Some(1.0);
    session.queue.current_index = 2;
    let duplicate = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    assert!(duplicate.command.is_none());
    let duplicate = duplicate.update.unwrap();
    assert_eq!(duplicate.position_ticks, 900_000_000);
    assert_eq!(duplicate.list_item_id, "grouped");
    assert!(duplicate.failed && duplicate.ended);
    assert_eq!(
        duplicate.stop_completion.unwrap().result(),
        PlaybackStopResult::Succeeded
    );
    assert!(
        session
            .dispatch(ReportingIntent::Progress { force: true })
            .command
            .is_none()
    );
}

#[test]
fn discarded_stop_receipt_fails_and_terminal_results_cannot_be_overwritten() {
    let mut session = Session::new();
    session.dispatch(ReportingIntent::Restart);
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let completion = closed.update.unwrap().stop_completion.unwrap();
    drop(closed.command);
    assert_eq!(completion.result(), PlaybackStopResult::Failed);
    completion.finish(true);
    assert_eq!(completion.result(), PlaybackStopResult::Failed);
    let success = super::PlaybackStopCompletion::pending();
    success.finish(true);
    success.finish(false);
    assert_eq!(success.result(), PlaybackStopResult::Succeeded);
}

#[test]
fn ended_and_missing_runtime_preserve_zero_invalid_and_fallback_semantics() {
    for (runtime, duration, ended, expected_runtime, position) in [
        (None, Some(90.0), true, Some(900_000_000), 900_000_000),
        (Some(0), Some(90.0), true, Some(0), 0),
        (None, Some(f64::NAN), true, None, 0),
        (None, None, true, None, 25_000_000),
        (Some(1), Some(90.0), false, Some(1), 1),
        (Some(0), Some(90.0), false, Some(0), 25_000_000),
    ] {
        let mut session = Session::new();
        session.runtime = runtime;
        session.telemetry.duration = duration;
        session.queue.items.clear();
        let update = session
            .dispatch(ReportingIntent::Close {
                failed: false,
                ended,
            })
            .update
            .unwrap();
        assert_eq!(update.run_time_ticks, expected_runtime);
        assert_eq!(update.position_ticks, position);
        assert_eq!(update.list_item_id, "physical");
    }
}
