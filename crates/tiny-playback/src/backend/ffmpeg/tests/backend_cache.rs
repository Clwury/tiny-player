use super::*;

#[test]
fn resolved_input_tracks_replace_request_selection_and_ignore_stale_sessions() {
    use crate::{PlaybackTrackSelection, backend::BackendLoadRequest};

    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(2);
    let requested = PlaybackTrackSelection {
        audio_stream_index: Some(5),
        subtitle_stream_index: Some(7),
        ..Default::default()
    };
    backend.current_request = Some(BackendLoadRequest {
        url: "file:///fixture.mkv".into(),
        http_headers: Vec::new(),
        content_length: None,
        start_position_seconds: 0.0,
        selected_tracks: requested.clone(),
        cache_config: PlaybackCacheConfig::default(),
    });
    let resolved = PlaybackTrackSelection {
        audio_stream_index: Some(1),
        ..Default::default()
    };
    for session in [1, 2] {
        backend
            .event_tx
            .send(BackendEvent::new(
                PlaybackSessionId(session),
                BackendEventKind::PlaybackTracksChanged {
                    audio: Vec::new(),
                    subtitles: Vec::new(),
                    selected: resolved.clone(),
                },
            ))
            .unwrap();
        let events = backend.poll_events();
        assert_eq!(events.len(), usize::from(session == 2));
        assert_eq!(
            backend.current_request.as_ref().unwrap().selected_tracks,
            if session == 2 {
                resolved.clone()
            } else {
                requested.clone()
            }
        );
    }
}

#[test]
fn ffmpeg_backend_maps_http_raw_input_rate_into_unified_cache_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    raw_input_rate: Some(123_456),
                    byte_level_seeks: 2,
                    ..DemuxCacheState::default()
                },
                byte: Some(ByteCacheState {
                    ranges: Vec::new(),
                    reader_fraction: None,
                    download_fraction: None,
                    cached_bytes: 0,
                    content_length: None,
                    disk_cache_enabled: false,
                    idle: false,
                    raw_input_rate: Some(123_456),
                    byte_level_seeks: 2,
                    ..ByteCacheState::default()
                }),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.raw_input_rate == Some(123_456)
                    && state.demux.byte_level_seeks == 2
                    && state
                        .byte
                        .as_ref()
                        .is_some_and(|byte| byte.raw_input_rate == Some(123_456))
        )
    }));
}

#[test]
fn ffmpeg_backend_byte_cache_update_does_not_replace_authoritative_demux_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.cache_state.demux = DemuxCacheState {
        cache_end: Some(20.0),
        reader_pts: Some(5.0),
        cache_duration: Some(15.0),
        cached_seeks: 4,
        seekable_ranges: vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 20.0,
        }],
        ..DemuxCacheState::default()
    };

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    raw_input_rate: Some(64 * 1024),
                    byte_level_seeks: 3,
                    ..DemuxCacheState::default()
                },
                byte: Some(ByteCacheState {
                    ranges: Vec::new(),
                    reader_fraction: Some(0.1),
                    download_fraction: Some(0.5),
                    cached_bytes: 4096,
                    content_length: Some(8192),
                    disk_cache_enabled: false,
                    idle: false,
                    raw_input_rate: Some(64 * 1024),
                    byte_level_seeks: 3,
                    ..ByteCacheState::default()
                }),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.cache_end == Some(20.0)
                    && state.demux.cached_seeks == 4
                    && state.demux.raw_input_rate == Some(64 * 1024)
                    && state.demux.byte_level_seeks == 3
                    && state.byte.as_ref().is_some_and(|byte| byte.cached_bytes == 4096)
        )
    }));
}

#[test]
fn ffmpeg_backend_byte_cache_update_uses_byte_state_as_metric_source() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.cache_state.demux = DemuxCacheState {
        cache_end: Some(20.0),
        reader_pts: Some(5.0),
        cache_duration: Some(15.0),
        byte_level_seeks: 2,
        ..DemuxCacheState::default()
    };

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState::default(),
                byte: Some(ByteCacheState {
                    ranges: Vec::new(),
                    reader_fraction: Some(0.1),
                    download_fraction: Some(0.5),
                    cached_bytes: 4096,
                    content_length: Some(8192),
                    disk_cache_enabled: false,
                    idle: false,
                    raw_input_rate: Some(96 * 1024),
                    byte_level_seeks: 4,
                    ..ByteCacheState::default()
                }),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.cache_end == Some(20.0)
                    && state.demux.raw_input_rate == Some(96 * 1024)
                    && state.demux.byte_level_seeks == 4
                    && state.byte.as_ref().is_some_and(|byte| byte.cached_bytes == 4096)
        )
    }));
}

#[test]
fn ffmpeg_backend_byte_cache_update_does_not_extend_demux_seekable_ranges() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.duration_seconds = Some(100.0);
    backend.cache_state.demux = DemuxCacheState {
        reader_pts: Some(25.0),
        seekable_ranges: vec![PlaybackCacheTimeRange {
            start: 20.0,
            end: 28.0,
        }],
        ..DemuxCacheState::default()
    };

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                byte: Some(ByteCacheState {
                    ranges: vec![PlaybackCacheByteRange {
                        start_fraction: 0.2,
                        end_fraction: 0.5,
                    }],
                    reader_fraction: Some(0.25),
                    download_fraction: Some(0.5),
                    cached_bytes: 4096,
                    content_length: Some(8192),
                    disk_cache_enabled: false,
                    idle: false,
                    raw_input_rate: Some(64 * 1024),
                    byte_level_seeks: 1,
                    ..ByteCacheState::default()
                }),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.seekable_ranges == vec![PlaybackCacheTimeRange {
                    start: 20.0,
                    end: 28.0,
                }]
        )
    }));
}

#[test]
fn ffmpeg_backend_byte_cache_update_can_clear_stale_raw_input_rate() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.cache_state.demux = DemuxCacheState {
        cache_end: Some(20.0),
        reader_pts: Some(5.0),
        cache_duration: Some(15.0),
        raw_input_rate: Some(64 * 1024),
        byte_level_seeks: 5,
        ..DemuxCacheState::default()
    };

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    cache_end: Some(22.0),
                    reader_pts: Some(6.0),
                    cache_duration: Some(16.0),
                    ..DemuxCacheState::default()
                },
                byte: Some(ByteCacheState {
                    ranges: Vec::new(),
                    reader_fraction: Some(0.2),
                    download_fraction: Some(0.6),
                    cached_bytes: 8192,
                    content_length: Some(16_384),
                    disk_cache_enabled: false,
                    idle: true,
                    raw_input_rate: None,
                    byte_level_seeks: 3,
                    ..ByteCacheState::default()
                }),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.cache_end == Some(22.0)
                    && state.demux.raw_input_rate.is_none()
                    && state.demux.byte_level_seeks == 5
                    && state.byte.as_ref().is_some_and(|byte| byte.raw_input_rate.is_none())
        )
    }));
}

#[test]
fn ffmpeg_backend_cache_pause_event_does_not_set_user_pause() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.user_paused = false;
    backend.paused = false;

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::PausedForCacheChanged(true),
        ))
        .unwrap();
    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::Pause(true),
        ))
        .unwrap();

    let _ = backend.poll_events();

    assert!(backend.paused);
    assert!(!backend.user_paused);
    assert!(backend.cache_state.paused_for_cache);
}

#[test]
fn ffmpeg_backend_effective_pause_follows_cache_pause_without_waiting_for_pause_event() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.user_paused = false;
    backend.paused = true;
    backend.cache_state.paused_for_cache = true;

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::PausedForCacheChanged(false),
        ))
        .unwrap();

    let _ = backend.poll_events();

    assert!(!backend.paused);
    assert!(!backend.user_paused);
    assert!(!backend.cache_state.paused_for_cache);
}

#[test]
fn ffmpeg_backend_pause_false_does_not_clear_effective_pause_while_cache_paused() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.user_paused = false;
    backend.paused = true;
    backend.cache_state.paused_for_cache = true;

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::Pause(false),
        ))
        .unwrap();

    let _ = backend.poll_events();

    assert!(backend.paused);
    assert!(!backend.user_paused);
    assert!(backend.cache_state.paused_for_cache);
}

#[test]
fn ffmpeg_backend_playback_ended_clears_cache_pause_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.user_paused = false;
    backend.paused = false;
    backend.cache_state.paused_for_cache = true;
    backend.cache_state.buffering_percent = Some(42);

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::PlaybackEnded,
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(backend.paused);
    assert!(!backend.cache_state.paused_for_cache);
    assert_eq!(backend.cache_state.buffering_percent, None);
    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.eof
                    && state.demux.idle
                    && !state.paused_for_cache
                    && state.buffering_percent.is_none()
        )
    }));
}

#[test]
fn ffmpeg_backend_demux_cache_update_preserves_latest_byte_cache_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.cache_state.byte = Some(ByteCacheState {
        ranges: Vec::new(),
        reader_fraction: Some(0.25),
        download_fraction: Some(0.75),
        cached_bytes: 8192,
        content_length: Some(16_384),
        disk_cache_enabled: true,
        idle: false,
        raw_input_rate: Some(32 * 1024),
        byte_level_seeks: 5,
        ..ByteCacheState::default()
    });
    backend.cache_state.demux.raw_input_rate = Some(32 * 1024);
    backend.cache_state.demux.byte_level_seeks = 5;

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    cache_end: Some(12.0),
                    reader_pts: Some(10.0),
                    cache_duration: Some(2.0),
                    low_level_seeks: 2,
                    cached_seeks: 1,
                    ..DemuxCacheState::default()
                },
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.cache_end == Some(12.0)
                    && state.demux.cached_seeks == 1
                    && state.demux.low_level_seeks == 2
                    && state.demux.raw_input_rate == Some(32 * 1024)
                    && state.demux.byte_level_seeks == 5
                    && state.byte.as_ref().is_some_and(|byte| byte.cached_bytes == 8192)
        )
    }));
}

#[test]
fn ffmpeg_backend_prefers_byte_cache_raw_rate_over_demux_estimate() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    backend.cache_state.byte = Some(ByteCacheState {
        ranges: Vec::new(),
        reader_fraction: Some(0.25),
        download_fraction: Some(0.75),
        cached_bytes: 8192,
        content_length: Some(16_384),
        disk_cache_enabled: true,
        idle: false,
        raw_input_rate: Some(32 * 1024),
        byte_level_seeks: 5,
        ..ByteCacheState::default()
    });
    backend.cache_state.demux.raw_input_rate = Some(32 * 1024);
    backend.cache_state.demux.byte_level_seeks = 5;

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    cache_end: Some(12.0),
                    reader_pts: Some(10.0),
                    cache_duration: Some(2.0),
                    raw_input_rate: Some(8 * 1024),
                    ..DemuxCacheState::default()
                },
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.raw_input_rate == Some(32 * 1024)
                    && state.byte.as_ref().is_some_and(|byte| byte.raw_input_rate == Some(32 * 1024))
        )
    }));
}

#[test]
fn ffmpeg_backend_does_not_synthesize_demux_cache_state_from_buffered_event() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::BufferedChanged(Some(12.0)),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        BackendEventKind::BufferedChanged(Some(12.0))
    ));
    let cache_state = backend.cache_state().expect("cache state exists");
    assert_eq!(cache_state.demux.cache_end, None);
    assert!(cache_state.demux.seekable_ranges.is_empty());
    assert!(!cache_state.demux.bof_cached);
    assert!(!cache_state.demux.eof_cached);
}

#[test]
fn ffmpeg_backend_position_events_do_not_rewrite_authoritative_demux_cache_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);
    let demux_state = DemuxCacheState {
        cache_end: Some(10.0),
        reader_pts: Some(1.0),
        cache_duration: Some(9.0),
        seekable_ranges: vec![PlaybackCacheTimeRange {
            start: 0.0,
            end: 10.0,
        }],
        ..DemuxCacheState::default()
    };

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: demux_state.clone(),
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();
    let _ = backend.poll_events();

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::PositionChanged(5.0),
        ))
        .unwrap();
    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::DurationChanged(10.0),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.kind, BackendEventKind::CacheStateChanged(_)))
    );
    assert_eq!(
        backend.cache_state().expect("cache state exists").demux,
        demux_state
    );
}

#[test]
fn ffmpeg_backend_does_not_preserve_cached_seek_count_over_demux_state() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(1);

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    cache_end: Some(10.0),
                    reader_pts: Some(1.0),
                    cache_duration: Some(9.0),
                    cached_seeks: 3,
                    ..DemuxCacheState::default()
                },
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();
    let _ = backend.poll_events();

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::CacheStateChanged(PlaybackCacheState {
                demux: DemuxCacheState {
                    cache_end: Some(2.0),
                    reader_pts: Some(1.0),
                    cache_duration: Some(1.0),
                    cached_seeks: 0,
                    ..DemuxCacheState::default()
                },
                ..PlaybackCacheState::default()
            }),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert!(events.iter().any(|event| {
        matches!(
            &event.kind,
            BackendEventKind::CacheStateChanged(state)
                if state.demux.cache_end == Some(2.0) && state.demux.cached_seeks == 0
        )
    }));
    assert_eq!(
        backend
            .cache_state()
            .expect("cache state exists")
            .demux
            .cached_seeks,
        0
    );
}

#[test]
fn timestamp_mapper_uses_first_timestamp_without_stream_start() {
    let mut mapper = TimestampMapper::new(None, 0, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(500, time_base),
        MappedTimestamp {
            timeline_nsecs: 0,
            sink_nsecs: 0,
        }
    );
    assert_eq!(
        mapper.map(750, time_base),
        MappedTimestamp {
            timeline_nsecs: 250_000_000,
            sink_nsecs: 250_000_000,
        }
    );
}
