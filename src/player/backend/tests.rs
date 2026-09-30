use super::{test_support::*, *};
use std::{cell::RefCell, rc::Rc};
use tiny_playback::{BackendError, BackendEventKind, PlaybackSeekMode, PlaybackTrackSelection};

fn request() -> BackendLoadRequest {
    BackendLoadRequest {
        url: "https://example.invalid/video".into(),
        http_headers: vec![("X-Test".into(), "fixture".into())],
        content_length: Some(1_234_567),
        start_position_seconds: 12.3456789,
        selected_tracks: PlaybackTrackSelection {
            audio_stream_index: Some(2),
            default_audio_stream_index: Some(1),
            subtitle_external_url: Some("https://example.invalid/subtitle.ass".into()),
            subtitle_codec: Some("ass".into()),
            ..Default::default()
        },
        cache_config: Default::default(),
    }
}

fn start(state: Rc<RefCell<FakeState>>) -> (PlaybackBackendAdapter, Option<String>) {
    PlaybackBackendAdapter::start_with(|| Ok(Box::new(FakeDriver(state))), request(), 0.0)
}

#[test]
fn startup_restores_mute_before_load_and_preserves_transport_and_resume_precision() {
    let state = Rc::new(RefCell::new(FakeState::default()));
    let original = request();
    let (adapter, error) = PlaybackBackendAdapter::start_with(
        || Ok(Box::new(FakeDriver(state.clone()))),
        original.clone(),
        0.0,
    );
    assert!(error.is_none());
    assert!(adapter.has_backend() && adapter.has_presenter());
    let state = state.borrow();
    assert_eq!(state.operations, ["presenter", "volume", "load"]);
    assert_eq!(state.commands[0], BackendCommand::SetVolume { volume: 0.0 });
    let mut expected = original.clone();
    expected.cache_config =
        crate::player::cache::engine_cache_config(expected.cache_config).normalized();
    assert_eq!(state.commands[1], BackendCommand::Load(expected));
    assert!(original.cache_config.fallback_cache_dir.is_none());
}

#[test]
fn backend_creation_failure_leaves_safe_empty_adapter() {
    let (mut adapter, error) = PlaybackBackendAdapter::start_with(
        || Err(BackendError::Ffmpeg("unavailable".into())),
        request(),
        0.5,
    );
    assert_eq!(
        error.as_deref(),
        Some("创建 FFmpeg 播放后端失败：unavailable")
    );
    assert!(!adapter.has_backend() && !adapter.has_presenter());
    assert!(adapter.command(BackendCommand::Pause).is_none());
    assert!(adapter.poll_events().is_empty());
    adapter.prewarm();
    adapter.discard_pending_frames();
    assert!(
        adapter
            .render(RenderSize {
                width: 2,
                height: 1
            })
            .unwrap()
            .is_none()
    );
    assert!(adapter.presenter_snapshot().is_none());
}

#[test]
fn presenter_creation_failure_retains_backend_without_starting_playback() {
    let state = Rc::new(RefCell::new(FakeState {
        fail_presenter: true,
        ..Default::default()
    }));
    let (adapter, error) = start(state.clone());
    assert_eq!(
        error.as_deref(),
        Some("创建视频渲染器失败：synthetic presenter failure")
    );
    assert!(adapter.has_backend() && !adapter.has_presenter());
    assert!(state.borrow().commands.is_empty());
    drop(adapter);
    assert_eq!(state.borrow().operations, ["presenter", "drop_backend"]);
}

#[test]
fn initialization_failures_keep_resources_and_release_presenter_before_backend() {
    for fail_volume in [true, false] {
        let state = Rc::new(RefCell::new(FakeState {
            fail_volume,
            fail_load: !fail_volume,
            ..Default::default()
        }));
        let (adapter, error) = start(state.clone());
        assert_eq!(
            error.as_deref(),
            Some("加载视频失败：synthetic command failure")
        );
        assert!(adapter.has_backend() && adapter.has_presenter());
        assert_eq!(
            state.borrow().commands.len(),
            if fail_volume { 1 } else { 2 }
        );
        drop(adapter);
        let mut expected = vec!["presenter", "volume"];
        if !fail_volume {
            expected.push("load");
        }
        expected.extend(["drop_presenter", "drop_backend"]);
        assert_eq!(state.borrow().operations, expected);
    }
}

#[test]
fn runtime_commands_preserve_payload_and_propagate_failures_without_retrying() {
    let state = Rc::new(RefCell::new(FakeState::default()));
    let mut adapter = adapter(state.clone(), false);
    let commands = vec![
        BackendCommand::Pause,
        BackendCommand::Resume,
        BackendCommand::Seek {
            position_seconds: 4.1234567,
            mode: PlaybackSeekMode::Precise,
        },
        BackendCommand::Seek {
            position_seconds: 9.1234567,
            mode: PlaybackSeekMode::Fast,
        },
        BackendCommand::SetAudioTrack {
            track_index: Some(7),
            position_seconds: 4.1234567,
        },
        BackendCommand::SetSubtitleTrack {
            track: None,
            position_seconds: 5.1234567,
        },
        BackendCommand::SetVolume { volume: 0.37 },
        BackendCommand::SetPlaybackRate { rate: 1.25 },
        BackendCommand::Stop,
    ];
    for command in &commands {
        adapter.command(command.clone()).unwrap().unwrap();
    }
    assert_eq!(state.borrow().commands, commands);
    state.borrow_mut().fail_commands = true;
    assert!(adapter.command(BackendCommand::Resume).unwrap().is_err());
    assert_eq!(state.borrow().commands.len(), commands.len() + 1);
    assert!(adapter.has_backend());
}

#[test]
fn runtime_cache_updates_add_application_fallback_without_changing_saved_settings() {
    let state = Rc::new(RefCell::new(FakeState::default()));
    let mut adapter = adapter(state.clone(), false);
    let mut saved = request();
    saved.cache_config.cache_dir = Some("custom-cache".into());
    adapter
        .command(BackendCommand::Load(saved.clone()))
        .unwrap()
        .unwrap();
    adapter
        .command(BackendCommand::SetCacheConfig(saved.cache_config.clone()))
        .unwrap()
        .unwrap();
    let mut expected = saved.clone();
    expected.cache_config = crate::player::cache::engine_cache_config(expected.cache_config);
    assert_eq!(
        state.borrow().commands,
        [
            BackendCommand::Load(expected.clone()),
            BackendCommand::SetCacheConfig(expected.cache_config),
        ]
    );
    assert!(saved.cache_config.fallback_cache_dir.is_none());
}

#[test]
fn validated_driver_events_keep_order_and_are_drained_once() {
    let state = Rc::new(RefCell::new(FakeState {
        events: vec![
            BackendEvent::new(
                tiny_playback::PlaybackSessionId(4),
                BackendEventKind::PlaybackRestart,
            ),
            BackendEvent::new(
                tiny_playback::PlaybackSessionId(4),
                BackendEventKind::PositionChanged(12.3456789),
            ),
            BackendEvent::new(
                tiny_playback::PlaybackSessionId(4),
                BackendEventKind::Pause(true),
            ),
        ],
        ..Default::default()
    }));
    let mut adapter = adapter(state.clone(), false);
    let events = adapter.poll_events();
    assert_eq!(events.len(), 3);
    assert!(
        events
            .iter()
            .all(|event| event.session_id == tiny_playback::PlaybackSessionId(4))
    );
    assert!(matches!(events[0].kind, BackendEventKind::PlaybackRestart));
    assert!(
        matches!(events[1].kind, BackendEventKind::PositionChanged(value) if value == 12.3456789)
    );
    assert!(matches!(events[2].kind, BackendEventKind::Pause(true)));
    assert!(adapter.poll_events().is_empty());
}

#[test]
fn frame_transfer_preserves_allocation_and_seek_discards_queued_frames() {
    let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let allocation = pixels.as_ptr();
    let size = RenderSize {
        width: 2,
        height: 1,
    };
    let state = Rc::new(RefCell::new(FakeState {
        frames: [
            BgraImage::new(pixels, 2, 1).unwrap(),
            BgraImage::new(vec![0; 8], 2, 1).unwrap(),
        ]
        .into(),
        ..Default::default()
    }));
    let mut adapter = adapter(state.clone(), true);
    assert_eq!(adapter.presenter_snapshot().unwrap().queued, 2);
    adapter.prewarm();
    let frame = adapter.render(size).unwrap().unwrap();
    assert_eq!(frame.size(), size);
    assert_eq!(frame.bytes().as_ptr(), allocation);
    assert_eq!(state.borrow().render_sizes, [size]);
    assert_eq!(adapter.presenter_snapshot().unwrap().queued, 1);
    adapter.discard_pending_frames();
    assert_eq!(adapter.presenter_snapshot().unwrap().queued, 0);
    assert!(adapter.render(size).unwrap().is_none());
    drop(adapter);
    assert_eq!(
        state.borrow().operations,
        [
            "prewarm",
            "render",
            "discard",
            "render",
            "drop_presenter",
            "drop_backend"
        ]
    );
    let pixels = frame.into_bytes();
    assert_eq!(pixels.as_ptr(), allocation);
}

#[test]
fn render_failure_keeps_resources_until_owner_is_released() {
    let state = Rc::new(RefCell::new(FakeState {
        fail_render: true,
        ..Default::default()
    }));
    let mut adapter = adapter(state.clone(), true);
    assert_eq!(
        adapter
            .render(RenderSize {
                width: 2,
                height: 1
            })
            .unwrap_err()
            .to_string(),
        "synthetic render failure"
    );
    assert!(adapter.has_backend() && adapter.has_presenter());
    drop(adapter);
    assert_eq!(
        state.borrow().operations,
        ["render", "drop_presenter", "drop_backend"]
    );
}
