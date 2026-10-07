use super::*;
use crate::player::{PlaybackQueueItem, queue::QueueAction, reporting::PlaybackReport};

fn context() -> BackendContext<'static> {
    BackendContext {
        item_id: "physical",
        media_source_id: "source",
        play_session_id: Some("session"),
        run_time_ticks: Some(900_000_000),
        has_frame: true,
    }
}
pub(super) fn controller() -> PlaybackSessionController {
    let queue = PlaybackQueue::new(
        (0..2)
            .map(|index| PlaybackQueueItem {
                item_id: format!("grouped-{index}"),
                title: "episode".into(),
                episode_label: "episode".into(),
                overview: None,
                primary_image_tag: None,
                series_id: Some("series".into()),
                season_id: Some("season".into()),
                premiere_date: None,
                run_time_ticks: Some(900_000_000),
                playback_position_ticks: None,
                media_sources: vec![],
            })
            .collect(),
        0,
    );
    PlaybackSessionController::new(
        PlaybackTimelineState {
            position: Some(2.5),
            duration: Some(90.0),
            ..Default::default()
        },
        super::super::model::source::PlaybackSourceState {
            source_protocol: None,
            source_url: "video.mkv".into(),
            content_length: None,
            playback_file_info: None,
            playback_info: None,
            playback_audio_info: None,
            tracks: super::super::model::source::PlaybackTrackState::new(
                vec![],
                vec![],
                tiny_playback::PlaybackTrackSelection {
                    audio_stream_index: Some(2),
                    subtitle_stream_index: Some(4),
                    ..Default::default()
                },
            ),
            track_preference_key: crate::player::PlaybackTrackPreferenceKey {
                item_id: "physical".into(),
                media_source_id: "source".into(),
            },
            remember_subtitle_on_start: false,
        },
        queue,
        WorkspaceIdentity::default(),
        tiny_playback::PlaybackVolumeSettings {
            level: 0.75,
            unmuted_level: 0.75,
        },
        None,
    )
}
fn report(transition: &BackendTransition, index: usize) -> &PlaybackReport {
    transition.reporting[index]
        .command
        .as_ref()
        .unwrap()
        .report()
}

#[test]
fn idle_inhibition_requires_a_live_loaded_video_session() {
    let mut session = controller();
    assert!(!session.should_inhibit_idle(true, true));
    session.reduce_backend(BackendEventKind::PlaybackRestart, context());
    assert!(session.should_inhibit_idle(true, true));
    assert!(!session.should_inhibit_idle(false, true));
    assert!(!session.should_inhibit_idle(true, false));
    session.reduce_backend(BackendEventKind::PausedForCacheChanged(true), context());
    assert!(!session.should_inhibit_idle(true, true));
    session.reduce_backend(BackendEventKind::PausedForCacheChanged(false), context());
    assert!(session.should_inhibit_idle(true, true));
    session.reduce_backend(BackendEventKind::Pause(true), context());
    assert!(!session.should_inhibit_idle(true, true));
    session.reduce_backend(BackendEventKind::Pause(false), context());
    assert!(session.should_inhibit_idle(true, true));
    session.reduce_backend(BackendEventKind::PlaybackEnded, context());
    assert!(!session.should_inhibit_idle(true, true));
}

#[test]
fn restart_reduces_timeline_before_start_report_and_only_starts_once() {
    let mut controller = controller();
    controller.timeline.pending_seek_position = Some(2.5);
    controller.controls.error = Some("prior error".into());
    let result = controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    assert!(matches!(result.action, BackendAction::Restart));
    assert!(controller.timeline.loaded);
    assert!(!controller.timeline.user_paused);
    assert!(controller.timeline.pending_seek_position.is_none());
    let PlaybackReport::Started(start) = report(&result, 0) else {
        panic!("expected start");
    };
    assert!(start.can_seek);
    assert!(!start.is_paused);
    assert_eq!(start.position_ticks, 25_000_000);
    assert_eq!(start.audio_stream_index, Some(2));
    assert_eq!(start.subtitle_stream_index, Some(4));
    let restart = controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    assert!(!restart.reporting[0].started);
    assert!(matches!(report(&restart, 0), PlaybackReport::Progress(_)));
}

#[test]
fn failure_captures_drag_pause_and_seek_before_clearing_activity_and_cancels_queue() {
    for event in [
        BackendEventKind::LoadFailed("load".into()),
        BackendEventKind::Fatal("fatal".into()),
    ] {
        let mut controller = controller();
        controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
        controller.timeline.progress_drag_position = Some(41.1234567);
        controller.timeline.pending_seek_position = Some(41.0);
        controller.timeline.buffering = true;
        let command = controller
            .queue
            .begin(QueueAction::Next, false, false, false)
            .unwrap();
        let result = controller.reduce_backend(event, context());
        let PlaybackReport::Stopped(stop) = report(&result, 0) else {
            panic!("expected stop");
        };
        assert!(stop.failed && stop.can_seek);
        assert!(!stop.is_paused);
        assert_eq!(stop.position_ticks, 411_234_567);
        assert_eq!(stop.play_session_id.as_deref(), Some("session"));
        let update = result.reporting[0].update.as_ref().unwrap();
        assert_eq!(update.list_item_id, "grouped-0");
        assert_eq!(update.position_ticks, 411_234_567);
        assert!(update.failed && !update.ended);
        assert!(matches!(result.action, BackendAction::Failed));
        assert!(!controller.timeline.loaded && !controller.timeline.ended);
        assert!(controller.timeline.user_paused && controller.timeline.paused);
        assert_eq!(controller.timeline.position, Some(2.5));
        assert!(controller.timeline.progress_drag_position.is_none());
        assert!(controller.timeline.pending_seek_position.is_none());
        assert!(!controller.queue.view_model().loading);
        assert!(
            controller
                .queue
                .complete(
                    command,
                    Err(anyhow::anyhow!("late")),
                    &WorkspaceIdentity::default()
                )
                .is_none()
        );
        assert!(controller.reporting.is_closed());
        assert!(
            controller
                .reduce_backend(BackendEventKind::PlaybackRestart, context())
                .reporting
                .is_empty()
        );
    }
}

#[test]
fn end_reports_terminal_position_before_pausing_and_auto_advances_once() {
    let mut controller = controller();
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.timeline.progress_drag_position = Some(5.0);
    controller.timeline.pending_seek_position = Some(5.0);
    let result = controller.reduce_backend(BackendEventKind::PlaybackEnded, context());
    assert!(matches!(
        result.action,
        BackendAction::Ended { auto_next: true }
    ));
    let PlaybackReport::Progress(progress) = report(&result, 0) else {
        panic!("expected final progress");
    };
    let PlaybackReport::Stopped(stop) = report(&result, 1) else {
        panic!("expected stop after progress");
    };
    assert_eq!(progress.position_ticks, 900_000_000);
    assert_eq!(stop.position_ticks, 900_000_000);
    assert!(!progress.is_paused && !stop.is_paused);
    assert!(progress.can_seek && stop.can_seek);
    assert!(result.reporting[1].update.as_ref().unwrap().ended);
    assert!(
        controller.timeline.ended && controller.timeline.paused && controller.timeline.user_paused
    );
    assert_eq!(controller.timeline.position, Some(90.0));
    assert!(controller.timeline.progress_drag_position.is_none());
    for event in [
        BackendEventKind::PlaybackEnded,
        BackendEventKind::PlaybackRestart,
    ] {
        let stale = controller.reduce_backend(event, context());
        assert!(matches!(stale.action, BackendAction::None));
        assert!(stale.reporting.is_empty());
    }
}

#[test]
fn missing_duration_uses_runtime_and_pending_switch_suppresses_auto_advance() {
    let mut controller = controller();
    controller.timeline.duration = None;
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller
        .queue
        .begin(QueueAction::Next, false, false, false)
        .unwrap();
    let result = controller.reduce_backend(BackendEventKind::PlaybackEnded, context());
    assert!(matches!(
        result.action,
        BackendAction::Ended { auto_next: false }
    ));
    assert_eq!(controller.timeline.position, Some(90.0));
    let PlaybackReport::Stopped(stop) = report(&result, 1) else {
        panic!("expected stop");
    };
    assert!(!stop.can_seek);
    assert_eq!(stop.position_ticks, 900_000_000);
}

#[test]
fn cache_pause_preserves_user_intent_and_deduplicates_reporting_across_event_sequence() {
    let mut controller = controller();
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.reduce_backend(BackendEventKind::PausedForCacheChanged(true), context());
    controller.reduce_backend(BackendEventKind::CacheBufferingChanged(Some(30)), context());
    let paused = controller.reduce_backend(BackendEventKind::Pause(true), context());
    assert!(!controller.timeline.user_paused);
    assert!(controller.timeline.paused);
    assert!(paused.reporting[0].command.is_none());
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    assert!(controller.timeline.paused_for_cache);
    assert_eq!(controller.timeline.cache_buffering_percent, Some(30));
    controller.reduce_backend(BackendEventKind::PausedForCacheChanged(false), context());
    assert!(!controller.timeline.paused);
    assert_eq!(controller.timeline.cache_buffering_percent, None);
    let paused = controller.reduce_backend(BackendEventKind::Pause(true), context());
    let PlaybackReport::Progress(progress) = report(&paused, 0) else {
        panic!("expected user pause");
    };
    assert!(progress.is_paused);
    assert!(controller.timeline.user_paused);
}

#[test]
fn backend_positions_respect_seek_ownership_and_cache_snapshot_overrides_soft_seek_extent() {
    let mut controller = controller();
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.timeline.pending_seek_position = Some(80.0);
    controller.timeline.pending_seek_keeps_frame = true;
    controller.timeline.buffered_until = Some(85.0);
    for event in [
        BackendEventKind::PositionChanged(10.0),
        BackendEventKind::Buffering(true),
        BackendEventKind::BufferedChanged(Some(30.0)),
    ] {
        let transition = controller.reduce_backend(event, context());
        assert!(transition.reporting.is_empty());
    }
    assert_eq!(controller.timeline.position, Some(2.5));
    assert!(!controller.timeline.buffering);
    assert_eq!(controller.timeline.buffered_until, Some(85.0));
    controller.reduce_backend(
        BackendEventKind::CacheStateChanged(tiny_playback::PlaybackCacheState {
            demux: tiny_playback::DemuxCacheState {
                cache_end: Some(83.0),
                ..Default::default()
            },
            paused_for_cache: true,
            buffering_percent: Some(20),
            ..Default::default()
        }),
        context(),
    );
    assert_eq!(controller.timeline.buffered_until, Some(83.0));
    assert!(controller.timeline.paused);
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.reduce_backend(BackendEventKind::PositionChanged(80.0), context());
    assert_eq!(controller.timeline.position, Some(80.0));
    controller.timeline.progress_drag_position = Some(100.0);
    controller.reduce_backend(BackendEventKind::DurationChanged(85.0), context());
    assert_eq!(controller.timeline.progress_drag_position, Some(85.0));
}

#[test]
fn long_event_sequence_and_failure_before_restart_never_emit_duplicate_start_or_stop() {
    let mut controller = controller();
    for index in 0..1000 {
        controller.reduce_backend(
            BackendEventKind::PositionChanged(f64::from(index) / 100.0),
            context(),
        );
    }
    let failure = controller.reduce_backend(
        BackendEventKind::LoadFailed("unavailable".into()),
        context(),
    );
    assert!(
        failure
            .reporting
            .iter()
            .all(|report| report.command.is_none())
    );
    assert!(
        failure.reporting[0]
            .update
            .as_ref()
            .unwrap()
            .stop_completion
            .is_none()
    );
    for _ in 0..100 {
        let restart = controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
        assert!(restart.reporting.is_empty());
        let ended = controller.reduce_backend(BackendEventKind::PlaybackEnded, context());
        assert!(ended.reporting.is_empty());
    }
    assert_eq!(controller.timeline.position, Some(9.99));
}

#[test]
fn tracks_keep_external_subtitles_and_only_preserve_an_unchanged_explicit_choice() {
    for selected in [Some(4), None] {
        let mut controller = controller();
        controller.source.remember_subtitle_on_start = true;
        let external = tiny_playback::PlaybackTrack::new(8, "External", true)
            .with_external_url(Some("https://example.invalid/subtitle".into()));
        controller.source.tracks.subtitles = vec![
            tiny_playback::PlaybackTrack::new(4, "Old embedded", false),
            external.clone(),
        ];
        let fresh = tiny_playback::PlaybackTrack::new(4, "New embedded", false);
        let audio = tiny_playback::PlaybackTrack::new(1, "Resolved audio", false);
        let transition = controller.reduce_backend(
            BackendEventKind::PlaybackTracksChanged {
                audio: vec![audio.clone()],
                subtitles: vec![fresh.clone()],
                selected: tiny_playback::PlaybackTrackSelection {
                    audio_stream_index: Some(1),
                    subtitle_stream_index: selected,
                    ..Default::default()
                },
            },
            context(),
        );
        assert!(matches!(transition.action, BackendAction::TracksChanged));
        assert!(transition.reporting.is_empty());
        assert_eq!(controller.source.tracks.subtitles, vec![fresh, external]);
        assert_eq!(controller.source.tracks.audio, vec![audio]);
        assert_eq!(
            controller.source.tracks.selected_audio_stream_index,
            Some(1)
        );
        assert_eq!(
            controller.source.remember_subtitle_on_start,
            selected == Some(4)
        );
        let started = controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
        let PlaybackReport::Started(start) = report(&started, 0) else {
            panic!("expected start");
        };
        assert_eq!(start.audio_stream_index, Some(1));
        assert_eq!(
            start.subtitle_stream_index,
            selected.map(|value| value as i32)
        );
    }
}

#[test]
fn metadata_and_video_size_update_in_session_and_failure_keeps_request_identity() {
    let mut controller = controller();
    let video = tiny_playback::PlaybackVideoInfo {
        codec: "hevc".into(),
        codec_description: None,
        profile: None,
        decoder: "hevc".into(),
        size: RenderSize {
            width: 1920,
            height: 1080,
        },
        sample_aspect_ratio: None,
        frame_rate: Some(24.0),
        pixel_format: None,
        color_range: None,
        chroma_location: None,
        color_space: None,
        color_primaries: None,
        color_transfer: None,
        bitrate: None,
        hardware_accelerated: false,
    };
    controller.reduce_backend(
        BackendEventKind::PlaybackInfoChanged(Some(video.clone())),
        context(),
    );
    controller.reduce_backend(
        BackendEventKind::PlaybackFileInfoChanged(Default::default()),
        context(),
    );
    assert_eq!(controller.source.playback_info, Some(video));
    let size = RenderSize {
        width: 3840,
        height: 2160,
    };
    let transition =
        controller.reduce_backend(BackendEventKind::VideoSizeChanged(Some(size)), context());
    assert!(matches!(transition.action,BackendAction::VideoSize(Some(actual)) if actual == size));
    assert_eq!(controller.source.playback_info.as_ref().unwrap().size, size);
    controller.reduce_backend(BackendEventKind::VideoSizeChanged(None), context());
    assert_eq!(controller.source.playback_info.as_ref().unwrap().size, size);
    controller.reduce_backend(BackendEventKind::Fatal("failure".into()), context());
    assert!(controller.source.playback_info.is_none());
    assert!(controller.source.playback_file_info.is_none());
    assert!(controller.source.playback_audio_info.is_none());
    assert_eq!(controller.source.source_url, "video.mkv");
    assert_eq!(controller.source.track_preference_key.item_id, "physical");
}

#[test]
fn paused_poll_has_one_token_and_rejects_foreign_duplicate_cancelled_and_closed_results() {
    let mut controller = controller();
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.reduce_backend(
        BackendEventKind::CacheStateChanged(tiny_playback::PlaybackCacheState {
            paused_for_cache: true,
            ..Default::default()
        }),
        context(),
    );
    assert!(controller.begin_poll(false, false).is_none());
    assert!(controller.begin_poll(true, true).is_none());
    let token = controller.begin_poll(true, false).unwrap();
    assert!(controller.begin_poll(true, false).is_none());
    let foreign = WorkspaceIdentity {
        user_id: Some("foreign".into()),
        ..Default::default()
    };
    assert!(!controller.complete_poll(&token, &foreign, true, false));
    assert!(controller.complete_poll(&token, &WorkspaceIdentity::default(), true, false));
    assert!(!controller.complete_poll(&token, &WorkspaceIdentity::default(), true, false));
    let cancelled = controller.begin_poll(true, false).unwrap();
    controller.cancel_poll();
    let current = controller.begin_poll(true, false).unwrap();
    assert!(!controller.complete_poll(&cancelled, &WorkspaceIdentity::default(), true, false));
    controller.reduce_backend(BackendEventKind::PlaybackEnded, context());
    assert!(!controller.complete_poll(&current, &WorkspaceIdentity::default(), true, false));
    assert!(controller.begin_poll(true, false).is_none());
}

#[test]
fn paused_poll_completion_rechecks_live_backend_and_activity() {
    let mut controller = controller();
    controller.reduce_backend(BackendEventKind::PlaybackRestart, context());
    controller.reduce_backend(
        BackendEventKind::CacheStateChanged(tiny_playback::PlaybackCacheState {
            paused_for_cache: true,
            ..Default::default()
        }),
        context(),
    );
    let token = controller.begin_poll(true, false).unwrap();
    controller.reduce_backend(BackendEventKind::PausedForCacheChanged(false), context());
    assert!(!controller.complete_poll(&token, &WorkspaceIdentity::default(), true, false));
    assert!(controller.begin_poll(true, false).is_none());
    controller.reduce_backend(BackendEventKind::PausedForCacheChanged(true), context());
    let token = controller.begin_poll(true, false).unwrap();
    assert!(!controller.complete_poll(&token, &WorkspaceIdentity::default(), false, false));
    let token = controller.begin_poll(true, false).unwrap();
    controller.reduce_backend(BackendEventKind::LoadFailed("failure".into()), context());
    assert!(!controller.complete_poll(&token, &WorkspaceIdentity::default(), true, false));
}
