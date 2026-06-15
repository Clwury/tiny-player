use super::avio::{CacheRestartRequest, CachedInputSource, HttpCacheRangeKind};
use super::worker::{FfmpegCommand, PendingSeek, PendingTrackSelection, drain_playback_commands};
use super::*;
use crate::player::backend::PlaybackCacheTimeRange;
use playback_loop::{
    AudioClockResumeDecision, DecodedVideoFrameStartAction, DemuxReaderWatermark,
    PendingAudioUnderrunRecoveryPlan, PendingStartAudio, PlaybackBlockReason,
    PlaybackOutputScheduler, PlaybackOutputState, RebufferResumeAnchor,
    VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VideoDecodeRecovery,
    admit_decoded_video_frame_to_vo, audio_clock_resume_decision,
    audio_clock_resume_timeline_nsecs, audio_clocked_video_wait_duration,
    audio_output_buffered_until_for_resume, decoded_audio_forward_nsecs_from,
    decoded_video_frame_start_action, decoded_video_start_prebuffer_reached,
    demux_reader_ready_for_output, discard_queued_video_before,
    discard_stale_pending_audio_before_recovery_start, initial_audio_clock_resume_decision,
    initial_probe_profile, pending_audio_underrun_recovery_plan, playback_read_finished,
    playback_resume_waterline, playback_resume_waterline_blocked_on, pop_audio_clocked_video_frame,
    pop_audio_clocked_video_frame_with_policy, push_queued_video_frame,
    queued_video_buffered_until_nsecs, queued_video_frame_ready_for_audio_clock,
    rebase_subtitle_cues_to_timeline_origin, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline, rebuffer_playback_resume_waterline_after_prolonged_wait,
    rebuffer_playback_resume_waterline_with_resource_pressure, should_block_for_demux_read,
    should_drop_late_video_frame, subtitle_cue_timeline_nsecs,
    subtitle_timestamp_to_timeline_nsecs, trim_overlapping_subtitle_cues_at,
    video_decode_error_is_recoverable, video_decode_should_skip_nonref_for_pressure,
    video_output_rebuffer_low_water, video_output_rebuffer_resume_duration,
    video_output_rebuffer_resume_duration_with_resource_pressure,
    video_output_rebuffer_resume_reached, video_output_rebuffer_should_enter,
};

#[test]
fn timestamp_mapper_uses_stream_start_when_available() {
    let mut mapper = TimestampMapper::new(Some(1_000_000_000), 0, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(1_250, time_base),
        MappedTimestamp {
            timeline_nsecs: 250_000_000,
            sink_nsecs: 250_000_000,
        }
    );
}

#[test]
fn ffmpeg_control_tracks_seek_generations() {
    let control = FfmpegControl::new(PlaybackSessionId::default());

    let first = control.request_seek();
    assert!(control.has_pending_seek());
    control.finish_seek(first);
    assert!(!control.has_pending_seek());

    let second = control.request_seek();
    assert!(control.has_pending_seek());
    control.finish_seek(first);
    assert!(control.has_pending_seek());
    control.finish_seek(second);
    assert!(!control.has_pending_seek());
}

#[test]
fn ffmpeg_control_splits_user_pause_from_cache_pause() {
    let control = FfmpegControl::new(PlaybackSessionId::default());

    assert!(!control.is_paused());
    assert!(!control.is_user_paused());
    assert!(!control.is_cache_paused());

    assert!(control.set_cache_paused(true));
    assert!(control.is_paused());
    assert!(!control.is_user_paused());
    assert!(control.is_cache_paused());

    control.set_user_paused(true);
    assert!(control.set_cache_paused(false));
    assert!(control.is_paused());
    assert!(control.is_user_paused());
    assert!(!control.is_cache_paused());

    control.set_user_paused(false);
    assert!(!control.is_paused());
}

#[test]
fn ffmpeg_control_tracks_output_rebuffer_pause_independently() {
    let control = FfmpegControl::new(PlaybackSessionId::default());

    assert!(!control.is_paused());
    assert!(!control.is_output_rebuffer_paused());
    assert!(!control.should_pause_audio_output());

    assert!(control.set_output_rebuffer_paused(true));
    assert!(!control.is_paused());
    assert!(control.should_pause_audio_output());
    assert!(control.is_output_rebuffer_paused());
    assert!(!control.is_user_paused());
    assert!(!control.is_cache_paused());

    assert!(control.set_output_rebuffer_paused(false));
    assert!(!control.is_paused());
    assert!(!control.should_pause_audio_output());
}

#[test]
fn ffmpeg_control_clears_output_rebuffer_pause_on_seek_generation() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    control.set_output_rebuffer_paused(true);

    let generation = control.request_seek();

    assert_eq!(generation, 1);
    assert!(!control.is_output_rebuffer_paused());
}

#[test]
fn drain_playback_commands_keeps_latest_live_cache_config() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let (command_tx, command_rx) = mpsc::channel();
    let mut first = PlaybackCacheConfig {
        cache_secs: 2.0,
        ..PlaybackCacheConfig::default()
    };
    let second = PlaybackCacheConfig {
        cache_secs: 4.0,
        demuxer_readahead_secs: 3.0,
        ..PlaybackCacheConfig::default()
    };
    first.demuxer_hysteresis_secs = f64::NAN;

    command_tx
        .send(FfmpegCommand::SetCacheConfig {
            session_id: PlaybackSessionId(7),
            config: first,
        })
        .unwrap();
    command_tx
        .send(FfmpegCommand::SetCacheConfig {
            session_id: PlaybackSessionId(8),
            config: second.clone(),
        })
        .unwrap();

    let drained = drain_playback_commands(&command_rx, &control);

    assert_eq!(control.session_id(), PlaybackSessionId(8));
    assert_eq!(drained.cache_config, Some(second.normalized()));
    assert!(drained.pending_seek.is_none());
    assert!(drained.pending_track_selection.is_none());
}

#[test]
fn playback_read_finished_treats_eio_near_duration_as_end() {
    assert!(playback_read_finished(
        ffi::AVERROR_EOF,
        Some(120.0),
        Some(1.0)
    ));
    assert!(playback_read_finished(
        ffi::AVERROR(ffi::EIO),
        Some(120.0),
        Some(119.0)
    ));
    assert!(!playback_read_finished(
        ffi::AVERROR(ffi::EIO),
        Some(120.0),
        Some(80.0)
    ));
    assert!(!playback_read_finished(
        ffi::AVERROR(ffi::EIO),
        None,
        Some(119.0)
    ));
}

#[test]
fn ffmpeg_command_drain_applies_pause_resume_and_keeps_latest_seek() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let (tx, rx) = mpsc::channel();
    let first_generation = control.request_seek();
    let second_generation = control.request_seek();

    tx.send(FfmpegCommand::Pause {
        session_id: PlaybackSessionId(2),
    })
    .unwrap();
    tx.send(FfmpegCommand::Seek {
        session_id: PlaybackSessionId(3),
        position_seconds: 12.0,
        mode: PlaybackSeekMode::Precise,
        generation: first_generation,
    })
    .unwrap();
    tx.send(FfmpegCommand::Resume {
        session_id: PlaybackSessionId(4),
    })
    .unwrap();
    tx.send(FfmpegCommand::Seek {
        session_id: PlaybackSessionId(5),
        position_seconds: 24.0,
        mode: PlaybackSeekMode::Fast,
        generation: second_generation,
    })
    .unwrap();

    let drained = drain_playback_commands(&rx, &control);

    assert!(!control.is_paused());
    assert_eq!(control.session_id(), PlaybackSessionId(4));
    assert_eq!(
        drained.pending_seek,
        Some(PendingSeek {
            session_id: PlaybackSessionId(5),
            position_seconds: 24.0,
            mode: PlaybackSeekMode::Fast,
            generation: second_generation,
        })
    );
}

#[test]
fn ffmpeg_command_drain_keeps_latest_track_selection() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let (tx, rx) = mpsc::channel();
    let seek_generation = control.request_seek();
    let track_generation = control.request_seek();
    let selected_tracks = crate::player::PlaybackTrackSelection {
        audio_stream_index: Some(3),
        subtitle_stream_index: Some(4),
        ..Default::default()
    };

    tx.send(FfmpegCommand::Seek {
        session_id: PlaybackSessionId(2),
        position_seconds: 10.0,
        mode: PlaybackSeekMode::Fast,
        generation: seek_generation,
    })
    .unwrap();
    tx.send(FfmpegCommand::SetTrackSelection {
        session_id: PlaybackSessionId(3),
        selected_tracks: selected_tracks.clone(),
        position_seconds: 24.0,
        generation: track_generation,
        pause_after_switch: true,
    })
    .unwrap();

    let drained = drain_playback_commands(&rx, &control);

    assert_eq!(drained.pending_seek, None);
    assert_eq!(
        drained.pending_track_selection,
        Some(PendingTrackSelection {
            session_id: PlaybackSessionId(3),
            selected_tracks,
            position_seconds: 24.0,
            generation: track_generation,
            pause_after_switch: true,
        })
    );
}

#[test]
fn ffmpeg_command_drain_applies_pause_to_pending_track_selection() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let (tx, rx) = mpsc::channel();
    let track_generation = control.request_seek();
    let selected_tracks = crate::player::PlaybackTrackSelection {
        audio_stream_index: Some(3),
        ..Default::default()
    };

    tx.send(FfmpegCommand::SetTrackSelection {
        session_id: PlaybackSessionId(2),
        selected_tracks: selected_tracks.clone(),
        position_seconds: 24.0,
        generation: track_generation,
        pause_after_switch: false,
    })
    .unwrap();
    tx.send(FfmpegCommand::Pause {
        session_id: PlaybackSessionId(2),
    })
    .unwrap();

    let drained = drain_playback_commands(&rx, &control);

    assert_eq!(
        drained.pending_track_selection,
        Some(PendingTrackSelection {
            session_id: PlaybackSessionId(2),
            selected_tracks,
            position_seconds: 24.0,
            generation: track_generation,
            pause_after_switch: true,
        })
    );
}

#[test]
fn pgs_subtitle_selection_uses_subtitle_probe_profile() {
    let mut selected_tracks = crate::player::PlaybackTrackSelection {
        subtitle_stream_index: Some(2),
        subtitle_codec: Some("PGSSUB".to_string()),
        ..Default::default()
    };
    assert_eq!(
        initial_probe_profile(&playback_input_with_selection(selected_tracks.clone())),
        InputProbeProfile::Subtitle
    );

    selected_tracks.subtitle_codec = Some("ass".to_string());
    assert_eq!(
        initial_probe_profile(&playback_input_with_selection(selected_tracks)),
        InputProbeProfile::Fast
    );
}

#[test]
fn external_subtitles_keep_fast_probe_profile() {
    let selected_tracks = crate::player::PlaybackTrackSelection {
        subtitle_stream_index: Some(2),
        subtitle_external_url: Some("https://example.test/sub.sup".to_string()),
        subtitle_codec: Some("PGSSUB".to_string()),
        ..Default::default()
    };

    assert_eq!(
        initial_probe_profile(&playback_input_with_selection(selected_tracks)),
        InputProbeProfile::Fast
    );
}

#[test]
fn subtitle_timestamps_do_not_rebase_to_first_sparse_packet() {
    assert_eq!(
        subtitle_timestamp_to_timeline_nsecs(60_000_000_000, None),
        60_000_000_000
    );
    assert_eq!(
        subtitle_timestamp_to_timeline_nsecs(65_000_000_000, Some(5_000_000_000)),
        60_000_000_000
    );
}

#[test]
fn subtitle_packet_timestamp_takes_precedence_over_zero_decoded_pts() {
    let stream = StreamInfo {
        index: 2,
        stream: ptr::null_mut(),
        decoder: ptr::null(),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        time_base: ffi::AVRational { num: 1, den: 1_000 },
        start_nsecs: None,
        frame_duration_nsecs: None,
    };

    assert_eq!(
        subtitle_cue_timeline_nsecs(Some(0), Some(60_000), stream, None),
        Some(60_000_000_000)
    );
}

#[test]
fn pgs_subtitle_timestamps_fall_back_to_playback_origin() {
    let stream = StreamInfo {
        index: 2,
        stream: ptr::null_mut(),
        decoder: ptr::null(),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        time_base: ffi::AVRational { num: 1, den: 1_000 },
        start_nsecs: None,
        frame_duration_nsecs: None,
    };

    assert_eq!(
        subtitle_cue_timeline_nsecs(
            Some(180_305_000_000),
            Some(180_305),
            stream,
            Some(1_168_000_000),
        ),
        Some(179_137_000_000)
    );
}

#[test]
fn pgs_subtitle_timestamps_do_not_use_sparse_stream_start_over_playback_origin() {
    let stream = StreamInfo {
        index: 2,
        stream: ptr::null_mut(),
        decoder: ptr::null(),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        time_base: ffi::AVRational { num: 1, den: 1_000 },
        start_nsecs: Some(136_470_000_000),
        frame_duration_nsecs: None,
    };

    assert_eq!(
        subtitle_cue_timeline_nsecs(
            Some(180_305_000_000),
            Some(180_305),
            stream,
            Some(1_168_000_000),
        ),
        Some(179_137_000_000)
    );
}

#[test]
fn pgs_frame_merge_bitstream_filter_initializes_when_available() {
    let mut codecpar = unsafe { ffi::avcodec_parameters_alloc() };
    assert!(!codecpar.is_null());
    unsafe {
        (*codecpar).codec_type = ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE;
        (*codecpar).codec_id = ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE;
    }
    let mut stream = unsafe { mem::zeroed::<ffi::AVStream>() };
    stream.codecpar = codecpar;
    let stream_info = StreamInfo {
        index: 2,
        stream: &mut stream,
        decoder: ptr::null(),
        codec_id: ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        time_base: ffi::AVRational { num: 1, den: 1_000 },
        start_nsecs: None,
        frame_duration_nsecs: None,
    };

    let filter = PgsFrameMergeBitstreamFilter::new(stream_info).unwrap();
    drop(filter);
    unsafe { ffi::avcodec_parameters_free(&mut codecpar) };
}

fn playback_input_with_selection(
    selected_tracks: crate::player::PlaybackTrackSelection,
) -> FfmpegPlaybackInput {
    FfmpegPlaybackInput {
        session_id: PlaybackSessionId::default(),
        url: "file:///tmp/video.mkv".to_string(),
        http_headers: Vec::new(),
        content_length: None,
        start_position_seconds: 0.0,
        selected_tracks,
        cache_config: PlaybackCacheConfig::default(),
    }
}

#[test]
fn ffmpeg_backend_discards_stale_session_events() {
    let mut backend = FfmpegBackend::new().unwrap();
    backend.current_session_id = PlaybackSessionId(2);

    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(1),
            BackendEventKind::Pause(true),
        ))
        .unwrap();
    backend
        .event_tx
        .send(BackendEvent::new(
            PlaybackSessionId(2),
            BackendEventKind::Buffering(true),
        ))
        .unwrap();

    let events = backend.poll_events();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].session_id, PlaybackSessionId(2));
    assert!(matches!(events[0].kind, BackendEventKind::Buffering(true)));
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

#[test]
fn timestamp_mapper_reports_dynamic_timeline_origin() {
    let mut mapper = TimestampMapper::new(None, 10_000_000_000, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(mapper.timeline_origin_nsecs(), None);
    assert_eq!(
        mapper.map(11_168, time_base),
        MappedTimestamp {
            timeline_nsecs: 10_000_000_000,
            sink_nsecs: 0,
        }
    );
    assert_eq!(mapper.timeline_origin_nsecs(), Some(1_168_000_000));
}

#[test]
fn timestamp_mapper_offsets_sink_timestamps_after_seek() {
    let mut mapper = TimestampMapper::new(Some(0), 10_000_000_000, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(10_250, time_base),
        MappedTimestamp {
            timeline_nsecs: 10_250_000_000,
            sink_nsecs: 250_000_000,
        }
    );
}

#[test]
fn timestamp_mapper_synthesizes_repeated_video_timestamps() {
    let mut mapper = TimestampMapper::new(Some(0), 0, Some(40_000_000));
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(0, time_base),
        MappedTimestamp {
            timeline_nsecs: 0,
            sink_nsecs: 0,
        }
    );
    assert_eq!(
        mapper.map(0, time_base),
        MappedTimestamp {
            timeline_nsecs: 40_000_000,
            sink_nsecs: 40_000_000,
        }
    );
}

#[test]
fn timestamp_mapper_keeps_missing_timestamps_at_seek_target() {
    let mut mapper = TimestampMapper::new(Some(0), 10_000_000_000, Some(40_000_000));
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(ffi::AV_NOPTS_VALUE, time_base),
        MappedTimestamp {
            timeline_nsecs: 10_000_000_000,
            sink_nsecs: 0,
        }
    );
    assert_eq!(
        mapper.map(0, time_base),
        MappedTimestamp {
            timeline_nsecs: 10_040_000_000,
            sink_nsecs: 40_000_000,
        }
    );
}

#[test]
fn optional_buffered_value_changed_uses_small_threshold() {
    assert!(!optional_buffered_value_changed(None, None));
    assert!(optional_buffered_value_changed(None, Some(1.0)));
    assert!(optional_buffered_value_changed(Some(1.0), None));
    assert!(!optional_buffered_value_changed(Some(1.0), Some(1.03)));
    assert!(optional_buffered_value_changed(Some(1.0), Some(1.05)));
}

#[test]
fn buffered_reporter_reports_first_video_update_after_reset() {
    let (tx, rx) = mpsc::channel();
    let mut reporter = BufferedReporter::new_with_events(false, true);
    let session_id = PlaybackSessionId(7);

    reporter.reset_to(0.0, session_id, &tx);
    assert_buffered_event(&rx, session_id, Some(0.0));

    reporter.report_video_timeline_nsecs(1_000_000_000, session_id, &tx);

    assert_buffered_event(&rx, session_id, Some(1.0));
}

#[test]
fn buffered_reporter_reports_first_audio_video_update_after_reset() {
    let (tx, rx) = mpsc::channel();
    let mut reporter = BufferedReporter::new_with_events(true, true);
    let session_id = PlaybackSessionId(8);

    reporter.reset_to(12.0, session_id, &tx);
    assert_buffered_event(&rx, session_id, Some(12.0));

    reporter.report_video_timeline_nsecs(13_000_000_000, session_id, &tx);
    assert!(rx.try_recv().is_err());

    reporter.report_audio_timeline_nsecs(13_000_000_000, session_id, &tx);

    assert_buffered_event(&rx, session_id, Some(13.0));
}

#[test]
fn buffered_reporter_can_update_without_emitting_events() {
    let (tx, rx) = mpsc::channel();
    let mut reporter = BufferedReporter::new_with_events(false, false);
    let session_id = PlaybackSessionId(9);

    reporter.reset_to(0.0, session_id, &tx);
    reporter.report_video_timeline_nsecs(2_000_000_000, session_id, &tx);

    assert_eq!(reporter.buffered_until(), Some(2.0));
    assert!(rx.try_recv().is_err());
}

fn assert_buffered_event(
    rx: &Receiver<BackendEvent>,
    expected_session_id: PlaybackSessionId,
    expected: Option<f64>,
) {
    match rx.try_recv().expect("expected buffered event") {
        BackendEvent {
            session_id,
            kind: BackendEventKind::BufferedChanged(buffered_until),
        } => {
            assert_eq!(session_id, expected_session_id);
            assert_eq!(buffered_until, expected);
        }
        event => panic!("expected buffered event, got {event:?}"),
    }
}

#[test]
fn queued_video_duration_uses_first_and_last_frame_pts() {
    let mut queue = VecDeque::new();
    assert_eq!(queued_video_duration(&queue), Duration::ZERO);

    queue.push_back(test_queued_video_frame(1_000_000_000));
    assert_eq!(queued_video_duration(&queue), Duration::ZERO);

    queue.push_back(test_queued_video_frame(1_180_000_000));
    queue.push_back(test_queued_video_frame(1_300_000_000));

    assert_eq!(queued_video_duration(&queue), Duration::from_millis(300));
}

#[test]
fn queued_video_window_expands_for_pgs_subtitle_prefetch() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_300_000_000));

    assert_eq!(
        queued_video_limit_duration(&queue, false),
        AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, false),
        AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
    assert_eq!(
        queued_video_limit_duration(&queue, true),
        PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, true),
        PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION
    );
}

#[test]
fn queued_video_window_caps_vulkan_decoded_frames() {
    let mut queue = VecDeque::new();
    queue.push_back(test_vulkan_queued_video_frame(1_000_000_000));
    queue.push_back(test_vulkan_queued_video_frame(1_300_000_000));

    assert_eq!(
        queued_video_limit_duration(&queue, false),
        VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, false),
        VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
    assert_eq!(
        queued_video_limit_frames(&queue, false),
        VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES
    );
    assert_eq!(
        queued_video_target_frames(&queue, false),
        VULKAN_DECODED_VIDEO_QUEUE_TARGET_FRAMES
    );

    assert_eq!(
        queued_video_limit_duration(&queue, true),
        VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    );
    assert_eq!(
        queued_video_target_duration(&queue, true),
        VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION
    );
}

#[test]
fn queued_video_limit_uses_frame_and_duration_caps() {
    let mut frame_limited = VecDeque::new();
    for index in 0..DECODED_VIDEO_QUEUE_LIMIT_FRAMES {
        frame_limited.push_back(test_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(queued_video_limit_reached(&frame_limited, false));

    let mut duration_limited = VecDeque::new();
    duration_limited.push_back(test_queued_video_frame(1_000_000_000));
    duration_limited.push_back(test_queued_video_frame(
        1_000_000_000 + duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION),
    ));
    assert!(queued_video_limit_reached(&duration_limited, false));

    let mut under_limit = VecDeque::new();
    under_limit.push_back(test_queued_video_frame(1_000_000_000));
    under_limit.push_back(test_queued_video_frame(1_200_000_000));
    assert!(!queued_video_limit_reached(&under_limit, false));
}

#[test]
fn queued_video_limit_keeps_decode_headroom_above_one_second() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(2_200_000_000));

    assert!(!queued_video_limit_reached(&queue, false));
}

#[test]
fn queued_video_limit_uses_vulkan_frame_cap() {
    let mut queue = VecDeque::new();
    for index in 0..VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES {
        queue.push_back(test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(queued_video_limit_reached(&queue, false));

    let mut under_limit = VecDeque::new();
    for index in 0..VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES - 1 {
        under_limit.push_back(test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 10_000_000,
        ));
    }
    assert!(!queued_video_limit_reached(&under_limit, false));
}

#[test]
fn video_output_rebuffer_enters_after_underrun_grace() {
    let mut underrun_started_at = None;
    let now = Instant::now();

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        false,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, Some(now));

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER - Duration::from_millis(1),
        true,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Rebuffering,
    ));
    assert_eq!(underrun_started_at, Some(now));

    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        true,
        true,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        false,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());
}

#[test]
fn video_output_rebuffer_waits_for_demux_cache_insufficient() {
    let now = Instant::now();
    let mut underrun_started_at = Some(now);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        false,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert!(underrun_started_at.is_none());
}

#[test]
fn video_output_rebuffer_enters_on_output_underrun_even_when_demux_ready() {
    let now = Instant::now();
    let mut underrun_started_at = None;

    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        true,
        false,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, Some(now));
}

#[test]
fn video_output_rebuffer_keeps_wait_timer_while_rebuffering() {
    let now = Instant::now();
    let mut underrun_started_at = Some(now);

    assert!(!video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        false,
        false,
        true,
        false,
        true,
        PlaybackOutputState::Rebuffering,
    ));
    assert_eq!(underrun_started_at, Some(now));
}

#[test]
fn video_output_rebuffer_enters_immediately_after_output_underrun() {
    let mut underrun_started_at = None;
    let now = Instant::now();

    assert!(video_output_rebuffer_should_enter(
        &mut underrun_started_at,
        now,
        true,
        true,
        true,
        false,
        true,
        PlaybackOutputState::Playing,
    ));
    assert_eq!(underrun_started_at, Some(now));
}

#[test]
fn demux_reader_ready_for_output_uses_combined_watermark() {
    let target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    let ready = DemuxReaderWatermark {
        video_forward_nsecs: Some(target_nsecs),
        audio_forward_nsecs: Some(target_nsecs),
        selected_min_forward_nsecs: Some(target_nsecs),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(demux_reader_ready_for_output(ready, true));

    let paused_at_readahead = DemuxReaderWatermark {
        video_forward_nsecs: None,
        audio_forward_nsecs: None,
        selected_min_forward_nsecs: None,
        idle: true,
        ..ready
    };
    assert!(demux_reader_ready_for_output(paused_at_readahead, true));

    let shallow_audio = DemuxReaderWatermark {
        audio_forward_nsecs: Some(target_nsecs - 1),
        selected_min_forward_nsecs: Some(target_nsecs - 1),
        ..ready
    };
    assert!(!demux_reader_ready_for_output(shallow_audio, true));

    let video_underrun = DemuxReaderWatermark {
        video_underrun: true,
        ..ready
    };
    assert!(!demux_reader_ready_for_output(video_underrun, true));
}

#[test]
fn output_scheduler_enters_rebuffer_and_updates_first_frame_gate() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    let started_at = Instant::now();
    scheduler.set_video_output_underrun_started_at_for_test(started_at);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));

    assert!(scheduler.maybe_enter_video_output_rebuffer(
        started_at + VIDEO_OUTPUT_REBUFFER_ENTER_AFTER,
        true,
        false,
        true,
        false,
        true,
        &control,
        None,
        PlaybackSessionId(7),
        Some(100_000_000),
    ));

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.state, PlaybackOutputState::Rebuffering);
    assert!(!snapshot.first_video_frame_pending);
    assert!(control.is_output_rebuffer_paused());
}

#[test]
fn output_scheduler_snapshot_reports_decoded_output_watermarks() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_decoded_video_for_test(test_queued_video_frame(
        1_000_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    ));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );

    let snapshot = scheduler.snapshot_for_played_until(Some(1_000_000_000));

    assert_eq!(snapshot.state, PlaybackOutputState::Playing);
    assert!(!snapshot.first_video_frame_pending);
    assert!(!snapshot.rebuffering);
    assert_eq!(snapshot.queued_video_frames, 2);
    assert_eq!(
        snapshot.queued_video_duration_nsecs,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS
    );
    assert_eq!(
        snapshot.queued_video_range_nsecs,
        Some((
            1_000_000_000,
            1_000_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2
        ))
    );
    assert_eq!(
        snapshot.queued_video_forward_nsecs,
        Some(DEFAULT_VIDEO_FRAME_DURATION_NSECS * 2)
    );
    assert!(snapshot.video_output_low_water);
    assert_eq!(snapshot.pending_start_audio_frames, 1);
    assert_eq!(snapshot.pending_start_audio_nsecs, 20_000_000);
    assert!(!snapshot.waiting_for_demux());
}

#[test]
fn output_scheduler_backpressures_large_pending_start_audio() {
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);

    assert!(!scheduler.pending_start_audio_backpressured());

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
    );

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_allows_startup_audio_until_first_video() {
    let mut scheduler = PlaybackOutputScheduler::new();

    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(PENDING_START_AUDIO_BACKPRESSURE_DURATION),
    );

    assert!(!scheduler.pending_start_audio_backpressured());

    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));

    assert!(scheduler.pending_start_audio_backpressured());
}

#[test]
fn output_scheduler_snapshot_keeps_coordinator_gate_decisions() {
    let mut scheduler = PlaybackOutputScheduler::new();
    let start_snapshot = scheduler.snapshot_for_played_until(Some(1_000_000_000));

    assert_eq!(start_snapshot.state, PlaybackOutputState::Syncing);
    assert!(start_snapshot.first_video_frame_pending);
    assert!(!start_snapshot.waiting_for_demux());
    assert!(start_snapshot.should_wait_for_demux());

    scheduler.set_state(PlaybackOutputState::Playing);
    let playing_empty = scheduler.snapshot_for_played_until(Some(1_000_000_000));
    assert!(playing_empty.waiting_for_demux());
    assert!(playing_empty.underflowing());
    assert!(!playing_empty.should_wait_for_demux());

    scheduler.set_state(PlaybackOutputState::Rebuffering);
    let rebuffering = scheduler.snapshot_for_played_until(Some(1_000_000_000));
    assert!(rebuffering.rebuffering);
    assert!(rebuffering.should_wait_for_demux());
}

#[test]
fn output_scheduler_reset_clears_queued_output_state() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    control.set_output_rebuffer_paused(true);
    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.push_decoded_video_for_test(test_queued_video_frame(1_000_000_000));
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    scheduler.set_state(PlaybackOutputState::Rebuffering);
    scheduler.set_video_output_underrun_started_at_for_test(Instant::now());
    scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
        timeline_nsecs: 1_000_000_000,
        reset_to_video_when_decoded_queue_misses_anchor: false,
    });

    scheduler.reset(&control);

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.queued_video_frames, 0);
    assert_eq!(snapshot.pending_start_audio_frames, 0);
    assert_eq!(snapshot.state, PlaybackOutputState::Syncing);
    assert!(snapshot.first_video_frame_pending);
    assert!(!scheduler.video_output_underrun_started_for_test());
    assert!(snapshot.video_output_rebuffer_anchor.is_none());
    assert!(!control.is_output_rebuffer_paused());
}

#[test]
fn video_output_rebuffer_requires_stable_decoded_queue_target() {
    let mut short_queue = VecDeque::new();
    short_queue.push_back(test_queued_video_frame(1_000_000_000));
    short_queue.push_back(test_queued_video_frame(
        1_000_000_000 + duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION),
    ));

    assert!(!video_output_rebuffer_resume_reached(&short_queue, false));
    assert!(queued_video_target_reached(&short_queue, false));

    let mut duration_ready = VecDeque::new();
    duration_ready.push_back(test_queued_video_frame(1_000_000_000));
    duration_ready.push_back(test_queued_video_frame(
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    ));

    assert!(video_output_rebuffer_resume_reached(&duration_ready, false));
    assert!(queued_video_target_reached(&duration_ready, false));

    let mut frame_ready = VecDeque::new();
    for index in 0..26 {
        frame_ready.push_back(test_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * 40_000_000,
        ));
    }

    assert!(video_output_rebuffer_resume_reached(&frame_ready, false));
    assert!(queued_video_target_reached(&frame_ready, false));
}

#[test]
fn video_output_rebuffer_resume_keeps_stable_floor_under_vulkan_resource_pressure() {
    let frame_duration_nsecs = 25_000_000;
    let mut queued = VecDeque::new();
    for index in 0..VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES.saturating_sub(2) {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let unpressured_duration = video_output_rebuffer_resume_duration(&queued, false);
    let pressured_duration =
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true);

    assert_eq!(unpressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);
    assert_eq!(pressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);

    let resume_timeline_nsecs = queued.front().unwrap().timeline_nsecs;
    let target_nsecs = duration_nsecs(pressured_duration);
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: target_nsecs,
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + target_nsecs,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(waterline.target_nsecs, target_nsecs);
    assert_eq!(
        waterline.decoded_video_forward_nsecs,
        Some(frame_duration_nsecs * u64::try_from(queued.len()).unwrap())
    );
    assert!(!waterline.ready());

    for index in queued.len()..40 {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(waterline.target_nsecs, target_nsecs);
    assert!(waterline.ready());
}

#[test]
fn video_output_rebuffer_resume_rejects_subsecond_resume_timeline_budget() {
    let first_timeline_nsecs = 18_550_000_000;
    let resume_timeline_nsecs = 18_551_651_321;
    let frame_duration_nsecs = 20_000_000;
    let mut queued = VecDeque::new();
    for index in 0..VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES.saturating_sub(2) {
        let mut frame = test_vulkan_queued_video_frame(
            first_timeline_nsecs + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let buffered_until_nsecs =
        first_timeline_nsecs + frame_duration_nsecs * u64::try_from(queued.len()).unwrap();
    let resume_budget_nsecs = buffered_until_nsecs - resume_timeline_nsecs;
    let front_budget_nsecs = duration_nsecs(
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true),
    );

    assert!(resume_budget_nsecs < front_budget_nsecs);

    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: resume_budget_nsecs,
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + resume_budget_nsecs,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(
        waterline.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        waterline.decoded_video_forward_nsecs,
        Some(resume_budget_nsecs)
    );
    assert!(!waterline.ready());
}

#[test]
fn video_output_rebuffer_resume_keeps_stable_floor_under_resource_pressure() {
    let frame_duration_nsecs = 50_000_000;
    let mut queued = VecDeque::new();
    let mut frame = test_vulkan_queued_video_frame(1_000_000_000);
    frame.duration_nsecs = frame_duration_nsecs;
    queued.push_back(frame);

    let pressured_duration =
        video_output_rebuffer_resume_duration_with_resource_pressure(&queued, false, true);

    assert_eq!(pressured_duration, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION);

    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        1_000_000_000,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert_eq!(
        waterline.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        waterline.decoded_video_forward_nsecs,
        Some(frame_duration_nsecs)
    );
    assert!(!waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(waterline),
        PlaybackBlockReason::DecodedVideoQueue
    );

    for index in 1..20 {
        let mut frame = test_vulkan_queued_video_frame(
            1_000_000_000 + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }

    let waterline = rebuffer_playback_resume_waterline_with_resource_pressure(
        &queued,
        &pending,
        1_000_000_000,
        ready_demux,
        None,
        false,
        true,
        true,
    );

    assert!(waterline.ready());
}

#[test]
fn rebuffer_resume_fallback_waits_for_stable_target_before_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let before_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER - Duration::from_millis(1)),
    );

    assert_eq!(
        before_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        before_timeout.decoded_video_forward_nsecs,
        Some(800_000_000)
    );
    assert!(!before_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_rejects_subsecond_decoded_window_after_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(after_timeout.decoded_video_forward_nsecs, Some(800_000_000));
    assert!(!after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_accepts_low_water_video_after_audio_stall_timeout() {
    let resume_timeline_nsecs = 13_080_000_000;
    let decoded_video_forward_nsecs = 800_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 20, 40_000_000);
    let pending = test_pending_audio(resume_timeline_nsecs + 200_000_000, 1_024_000_000);
    let demux_after_stalled_release = ready_demux_watermark(720_000_000);
    let audio_output_buffered_until_nsecs = resume_timeline_nsecs + 71_995_464;

    let mut waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        demux_after_stalled_release,
        Some(audio_output_buffered_until_nsecs),
        false,
        true,
    );
    assert_eq!(
        waterline.decoded_video_forward_nsecs,
        Some(decoded_video_forward_nsecs)
    );
    assert_eq!(waterline.decoded_audio_forward_nsecs, Some(71_995_464));
    assert!(!waterline.ready());

    // The output gate releases the demux side after the stalled timeout so the
    // decoder can keep moving; the prolonged-wait fallback must still be the
    // thing that decides when the output side can leave rebuffering.
    waterline.demux_ready = true;
    let at_standard_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );
    assert!(!at_standard_timeout.ready());

    let after_audio_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );

    assert_eq!(
        after_audio_timeout.target_nsecs,
        decoded_video_forward_nsecs
    );
    assert!(after_audio_timeout.ready());
    assert!(after_audio_timeout.decoded_video_ready);
    assert!(after_audio_timeout.decoded_audio_ready);
}

#[test]
fn rebuffer_resume_fallback_accepts_one_frame_short_decoded_window_when_audio_ready() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 24, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(after_timeout.decoded_video_forward_nsecs, Some(960_000_000));
    assert_eq!(after_timeout.target_nsecs, 960_000_000);
    assert!(after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_resumes_on_video_when_audio_stalls_past_timeout() {
    let resume_timeline_nsecs = 1_000_000_000;
    // Plenty of decoded video ahead of the resume point.
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 13, 100_000_000);
    // Audio sits entirely behind the resume point, so it never covers it
    // (decoded_audio_forward = None): a structurally lagging / unavailable audio track.
    let pending = test_pending_audio(resume_timeline_nsecs - 500_000_000, 100_000_000);
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    assert!(!waterline.decoded_audio_ready);

    // At the standard stall timeout, audio is still not ready -> keep waiting for it.
    let at_standard = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );
    assert!(!at_standard.ready());

    // Past the longer audio-stall fallback, resume on the decoded-video window alone
    // rather than freezing forever waiting for audio that is not arriving.
    let after_audio_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER),
    );
    assert!(after_audio_timeout.ready());
    assert!(after_audio_timeout.decoded_video_ready);
}

#[test]
fn rebuffer_resume_fallback_waits_for_delayed_audio_start_safety_window_after_demux_recovers() {
    let first_video_nsecs = 11_800_000_000;
    let resume_timeline_nsecs = 11_815_079_780;
    let queued = test_queued_video_frames_with_duration(first_video_nsecs, 5, 40_000_000);
    let decoded_video_forward_nsecs = first_video_nsecs + 5 * 40_000_000 - resume_timeline_nsecs;
    assert!(decoded_video_forward_nsecs < duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION));
    assert!(decoded_video_forward_nsecs >= DEFAULT_VIDEO_FRAME_DURATION_NSECS);

    let pending = test_pending_audio(resume_timeline_nsecs, 1_900_000_000);
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_video_forward_nsecs,
        Some(decoded_video_forward_nsecs)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn rebuffer_resume_fallback_rejects_short_window_that_audio_clock_would_consume() {
    let resume_timeline_nsecs = 9_120_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 7, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + 238_000_000,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(after_timeout.decoded_video_forward_nsecs, Some(280_000_000));
    assert!(!after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_waits_when_delayed_audio_start_consumes_low_water() {
    let resume_timeline_nsecs = 4_360_000_000;
    let delayed_audio_gap_nsecs = 344_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 13, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + delayed_audio_gap_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(after_timeout.decoded_video_forward_nsecs, Some(520_000_000));
    assert_eq!(
        after_timeout.delayed_audio_start_gap_nsecs,
        Some(delayed_audio_gap_nsecs)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn rebuffer_resume_fallback_accepts_delayed_audio_start_with_low_water_remaining() {
    let resume_timeline_nsecs = 4_360_000_000;
    let delayed_audio_gap_nsecs = 344_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 25, 40_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs + delayed_audio_gap_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = ready_demux_watermark(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION));

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        ready_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert_eq!(
        after_timeout.decoded_video_forward_nsecs,
        Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION))
    );
    assert_eq!(
        after_timeout.delayed_audio_start_gap_nsecs,
        Some(delayed_audio_gap_nsecs)
    );
    assert!(after_timeout.ready());
}

#[test]
fn rebuffer_resume_fallback_still_requires_ready_demux_reader() {
    let resume_timeline_nsecs = 1_000_000_000;
    let queued = test_queued_video_frames_with_duration(resume_timeline_nsecs, 8, 100_000_000);
    let pending = test_pending_audio(
        resume_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let shallow_demux = ready_demux_watermark(500_000_000);

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        resume_timeline_nsecs,
        shallow_demux,
        None,
        false,
        true,
    );
    let after_timeout = rebuffer_playback_resume_waterline_after_prolonged_wait(
        waterline,
        Some(VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER),
    );

    assert_eq!(
        after_timeout.target_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
    );
    assert!(!after_timeout.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(after_timeout),
        PlaybackBlockReason::DecodedVideoQueue
    );
}

#[test]
fn video_output_rebuffer_low_water_enters_before_queue_is_empty() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_200_000_000));

    assert!(video_output_rebuffer_low_water(&queued, 1_000_000_000));

    queued.push_back(test_queued_video_frame(1_400_000_000));

    assert!(!video_output_rebuffer_low_water(&queued, 1_000_000_000));
}

#[test]
fn video_decode_skips_nonref_frames_under_decode_pressure() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_800_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Syncing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));
    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        false,
        false,
    ));
    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Rebuffering,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));
    assert!(video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));

    queued.push_back(test_queued_video_frame(2_000_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));
}

#[test]
fn video_decode_skip_pressure_uses_short_vulkan_low_water() {
    let mut queued = VecDeque::new();
    queued.push_back(test_vulkan_queued_video_frame(1_000_000_000));
    queued.push_back(test_vulkan_queued_video_frame(1_300_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));

    queued.pop_back();
    queued.push_back(test_vulkan_queued_video_frame(1_200_000_000));

    assert!(video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));
}

#[test]
fn video_decode_skip_pressure_uses_vulkan_hysteresis_when_active() {
    let mut queued = VecDeque::new();
    queued.push_back(test_vulkan_queued_video_frame(1_000_000_000));
    queued.push_back(test_vulkan_queued_video_frame(1_300_000_000));

    assert!(!video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        false,
    ));
    assert!(video_decode_should_skip_nonref_for_pressure(
        PlaybackOutputState::Playing,
        &queued,
        Some(1_000_000_000),
        true,
        true,
    ));
}

#[test]
fn push_queued_video_frame_keeps_timeline_order() {
    let mut queued = VecDeque::new();
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_080_000_000));
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_000_000_000));
    push_queued_video_frame(&mut queued, test_queued_video_frame(1_040_000_000));

    let timeline = queued
        .iter()
        .map(|frame| frame.timeline_nsecs)
        .collect::<Vec<_>>();

    assert_eq!(timeline, vec![1_000_000_000, 1_040_000_000, 1_080_000_000]);
}

#[test]
fn decoded_video_start_requires_initial_prebuffer_waterline() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));

    assert!(!decoded_video_start_prebuffer_reached(&queued, false));

    queued.push_back(test_queued_video_frame(
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_START_PREBUFFER_DURATION),
    ));

    assert!(decoded_video_start_prebuffer_reached(&queued, false));
}

#[test]
fn playback_resume_waterline_requires_decoded_audio_and_demux_streams() {
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    ));
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        1_000_000_000,
        1_000_000_000 + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    let ready_waterline =
        playback_resume_waterline(&queued, &pending, 1_000_000_000, ready_demux, false, true);
    assert!(ready_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(ready_waterline),
        PlaybackBlockReason::OutputGate
    );

    let missing_audio = PendingStartAudio::default();
    let missing_audio_waterline = playback_resume_waterline(
        &queued,
        &missing_audio,
        1_000_000_000,
        ready_demux,
        false,
        true,
    );
    assert!(!missing_audio_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(missing_audio_waterline),
        PlaybackBlockReason::DecodedAudioQueue
    );

    let shallow_demux = DemuxReaderWatermark {
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1),
        ..ready_demux
    };
    let shallow_demux_waterline =
        playback_resume_waterline(&queued, &pending, 1_000_000_000, shallow_demux, false, true);
    assert!(!shallow_demux_waterline.ready());
    assert_eq!(
        playback_resume_waterline_blocked_on(shallow_demux_waterline),
        PlaybackBlockReason::DemuxCache
    );
}

#[test]
fn playback_resume_waterline_uses_resume_timeline_for_audio_offset() {
    let resume_timeline_nsecs = 1_020_000_000;
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(
        resume_timeline_nsecs + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    ));
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        },
        resume_timeline_nsecs,
        resume_timeline_nsecs + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        !playback_resume_waterline(&queued, &pending, 1_000_000_000, ready_demux, false, true)
            .ready()
    );
    assert!(
        playback_resume_waterline(
            &queued,
            &pending,
            resume_timeline_nsecs,
            ready_demux,
            false,
            true,
        )
        .ready()
    );
}

#[test]
fn playback_resume_waterline_tolerates_small_audio_timestamp_gaps() {
    let start_nsecs = 1_000_000_000;
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(start_nsecs));
    queued.push_back(test_queued_video_frame(
        start_nsecs + duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    ));
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 750_000_000,
        },
        start_nsecs,
        start_nsecs + 750_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 750_000_000,
        },
        start_nsecs + 751_000_000,
        start_nsecs + 1_501_000_000,
    );
    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        playback_resume_waterline(&queued, &pending, start_nsecs, ready_demux, false, true).ready()
    );
}

#[test]
fn initial_resume_keeps_video_start_for_small_audio_offset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_004_000_000,
        1_024_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        initial_audio_clock_resume_decision(&queued, &pending, 1_000_000_000),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: false,
        })
    );
}

#[test]
fn initial_resume_uses_audio_start_for_large_audio_offset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        initial_audio_clock_resume_decision(&queued, &pending, 1_000_000_000),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_020_000_000,
            reset_audio_to_video: false,
        })
    );
}

#[test]
fn rebuffer_resume_sync_uses_current_audio_and_pending_audio() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_400_000_000,
        1_420_000_000,
    );
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(40_000_000));
    queued.push_back(test_queued_video_frame(80_000_000));

    assert_eq!(
        audio_clock_resume_timeline_nsecs(&queued, &pending, 1_300_000_000),
        Some(1_400_000_000)
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_400_000_000), 2);
    assert!(queued.is_empty());
}

#[test]
fn rebuffer_resume_preserves_video_when_output_audio_covers_pending_gap() {
    let resume_timeline_nsecs = 1_010_000_000;
    let output_audio_until_nsecs = 1_500_000_000;
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 650_000_000,
        },
        output_audio_until_nsecs,
        2_150_000_000,
    );
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(2_050_000_000));

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            resume_timeline_nsecs,
            Some(output_audio_until_nsecs),
            false,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: resume_timeline_nsecs,
            reset_audio_to_video: false,
        })
    );
    assert_eq!(
        discard_queued_video_before(&mut queued, resume_timeline_nsecs),
        0
    );
    assert_eq!(
        decoded_audio_forward_nsecs_from(
            &pending,
            resume_timeline_nsecs,
            Some(output_audio_until_nsecs),
        ),
        Some(1_140_000_000)
    );

    let ready_demux = DemuxReaderWatermark {
        video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        audio_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        selected_min_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    };

    assert!(
        rebuffer_playback_resume_waterline(
            &queued,
            &pending,
            resume_timeline_nsecs,
            ready_demux,
            Some(output_audio_until_nsecs),
            false,
            true,
        )
        .ready()
    );
}

#[test]
fn rebuffer_resume_uses_pending_audio_when_output_audio_is_missing() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_400_000_000,
        1_420_000_000,
    );
    let queued = VecDeque::from([test_queued_video_frame(1_000_000_000)]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(&queued, &pending, 1_010_000_000, None, false),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_400_000_000,
            reset_audio_to_video: false,
        })
    );
}

#[test]
fn rebuffer_resume_resets_to_decoded_video_when_anchor_is_uncovered() {
    let pending = PendingStartAudio::default();
    let mut queued = VecDeque::from([
        test_queued_video_frame(1_000_000_000),
        test_queued_video_frame(1_040_000_000),
    ]);

    let decision = rebuffer_audio_clock_resume_decision(
        &queued,
        &pending,
        1_400_000_000,
        Some(1_900_000_000),
        true,
    );

    assert_eq!(
        decision,
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: true,
        })
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_000_000_000), 0);
    assert_eq!(
        audio_output_buffered_until_for_resume(decision.unwrap(), Some(1_900_000_000)),
        None
    );
}

#[test]
fn rebuffer_resume_keeps_anchor_when_decoded_video_covers_it() {
    let pending = PendingStartAudio::default();
    let queued = test_queued_video_frames_with_duration(
        1_360_000_000,
        30,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );

    assert_eq!(
        rebuffer_audio_clock_resume_decision(&queued, &pending, 1_370_000_000, None, true),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_370_000_000,
            reset_audio_to_video: false,
        })
    );
}

#[test]
fn rebuffer_resume_resets_to_video_when_anchor_window_is_short() {
    let pending = test_pending_audio(1_370_000_000, 900_000_000);
    let queued = VecDeque::from([
        test_queued_video_frame(1_240_000_000),
        test_queued_video_frame(1_280_000_000),
        test_queued_video_frame(1_320_000_000),
        test_queued_video_frame(1_360_000_000),
        test_queued_video_frame(1_400_000_000),
    ]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            1_370_000_000,
            Some(1_370_000_000),
            true,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_240_000_000,
            reset_audio_to_video: true,
        })
    );
    assert_eq!(
        decoded_audio_forward_nsecs_from(&pending, 1_240_000_000, None),
        None
    );
}

#[test]
fn rebuffer_resume_waterline_accepts_delayed_audio_start_after_video_reset() {
    let pending = test_pending_audio(1_370_000_000, 900_000_000);
    let queued = test_queued_video_frames_with_duration(
        1_240_000_000,
        35,
        DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    );

    let waterline = rebuffer_playback_resume_waterline(
        &queued,
        &pending,
        1_240_000_000,
        ready_demux_watermark(1_500_000_000),
        None,
        false,
        true,
    );

    assert!(waterline.ready());
    assert_eq!(waterline.decoded_audio_forward_nsecs, Some(1_030_000_000));
}

#[test]
fn rebuffer_resume_resets_when_output_audio_runs_past_decoded_video() {
    let pending = PendingStartAudio::default();
    let queued = VecDeque::from([
        test_queued_video_frame(3_440_000_000),
        test_queued_video_frame(3_480_000_000),
    ]);

    assert_eq!(
        rebuffer_audio_clock_resume_decision(
            &queued,
            &pending,
            3_441_636_173,
            Some(3_940_000_000),
            true,
        ),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 3_440_000_000,
            reset_audio_to_video: true,
        })
    );
}

#[test]
fn rebuffer_resume_keeps_video_frame_overlapping_sync_point() {
    let pending = PendingStartAudio::default();
    let mut queued = VecDeque::new();
    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_040_000_000));

    assert_eq!(
        audio_clock_resume_timeline_nsecs(&queued, &pending, 1_010_000_000),
        Some(1_010_000_000)
    );
    assert_eq!(discard_queued_video_before(&mut queued, 1_010_000_000), 0);
    assert_eq!(
        queued.front().map(|frame| frame.timeline_nsecs),
        Some(1_000_000_000)
    );
}

#[test]
fn rebuffer_resume_resets_audio_clock_when_audio_is_far_ahead() {
    let pending = PendingStartAudio::default();
    let queued = VecDeque::from([
        test_queued_video_frame(1_000_000_000),
        test_queued_video_frame(1_040_000_000),
    ]);

    assert_eq!(
        audio_clock_resume_decision(&queued, &pending, 1_700_000_001),
        Some(AudioClockResumeDecision {
            timeline_nsecs: 1_000_000_000,
            reset_audio_to_video: true,
        })
    );
}

#[test]
fn queued_video_buffered_until_uses_last_frame_end() {
    let mut queued = VecDeque::new();
    assert_eq!(queued_video_buffered_until_nsecs(&queued), None);

    queued.push_back(test_queued_video_frame(1_000_000_000));
    queued.push_back(test_queued_video_frame(1_040_000_000));

    assert_eq!(
        queued_video_buffered_until_nsecs(&queued),
        Some(1_040_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS)
    );
}

#[test]
fn demux_read_blocks_until_output_gate_finishes_rebuffering() {
    assert!(should_block_for_demux_read(PlaybackOutputState::Syncing));
    assert!(!should_block_for_demux_read(PlaybackOutputState::Playing));
    assert!(should_block_for_demux_read(
        PlaybackOutputState::Rebuffering
    ));
}

#[test]
fn audio_clock_video_frames_are_ready_with_small_present_lead() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));

    assert!(!queued_video_frame_ready_for_audio_clock(
        &queue,
        984_000_000
    ));
    assert!(queued_video_frame_ready_for_audio_clock(
        &queue,
        985_000_000
    ));
}

#[test]
fn audio_clock_video_wait_duration_tracks_present_deadline() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));

    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 984_000_000),
        Some(Duration::from_millis(1))
    );
    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 985_000_000),
        Some(Duration::ZERO)
    );
    assert_eq!(
        audio_clocked_video_wait_duration(&queue, 1_010_000_000),
        Some(Duration::ZERO)
    );
}

#[test]
fn audio_clock_video_pop_only_advances_one_early_frame() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));

    let frame = pop_audio_clocked_video_frame(&mut queue, 985_000_000).unwrap();

    assert_eq!(frame.timeline_nsecs, 1_000_000_000);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_010_000_000);
}

#[test]
fn audio_clock_video_pop_catches_up_to_latest_overdue_frame() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));

    let frame = pop_audio_clocked_video_frame(&mut queue, 1_015_000_000).unwrap();

    assert_eq!(frame.timeline_nsecs, 1_010_000_000);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn audio_clock_present_uses_snapshot_timeline_without_reading_output_clock() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));
    let vo_queue = VideoOutputQueue::default();
    let frame_presented = AtomicBool::new(false);
    let mut position_reporter = PositionReporter::default();
    let (event_tx, _event_rx) = mpsc::channel();

    let pop_result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_015_000_000);
    if let Some(frame) = pop_result.frame {
        admit_decoded_video_frame_to_vo(
            frame.frame,
            PlaybackSessionId::default(),
            frame.timeline_nsecs,
            &vo_queue,
            &frame_presented,
            &mut position_reporter,
            &event_tx,
        );
    }

    assert!(frame_presented.load(Ordering::Relaxed));
    assert_eq!(vo_queue.snapshot().queued_frames, 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn pgs_subtitle_cues_rebase_when_dynamic_playback_origin_appears() {
    let mut cues = VecDeque::from([BackendSubtitleCue {
        text: "subtitle".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: 180_305_000_000,
        end_nsecs: 184_305_000_000,
    }]);

    rebase_subtitle_cues_to_timeline_origin(&mut cues, None, Some(1_168_000_000));

    assert_eq!(cues[0].start_nsecs, 179_137_000_000);
    assert_eq!(cues[0].end_nsecs, 183_137_000_000);
}

#[test]
fn pgs_subtitle_clear_marker_trims_previous_bitmap_cue() {
    let mut cues = VecDeque::from([
        BackendSubtitleCue {
            text: "first".to_string(),
            bitmaps: Vec::new(),
            start_nsecs: 10_000_000_000,
            end_nsecs: 14_000_000_000,
        },
        BackendSubtitleCue {
            text: "second".to_string(),
            bitmaps: Vec::new(),
            start_nsecs: 15_000_000_000,
            end_nsecs: 17_000_000_000,
        },
    ]);

    trim_overlapping_subtitle_cues_at(&mut cues, 12_000_000_000);

    assert_eq!(cues.len(), 2);
    assert_eq!(cues[0].start_nsecs, 10_000_000_000);
    assert_eq!(cues[0].end_nsecs, 12_000_000_000);
    assert_eq!(cues[1].start_nsecs, 15_000_000_000);
    assert_eq!(cues[1].end_nsecs, 17_000_000_000);
}

#[test]
fn pgs_subtitle_replacement_trims_previous_cue_at_next_start() {
    let mut cues = VecDeque::from([BackendSubtitleCue {
        text: "first".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: 10_000_000_000,
        end_nsecs: 14_000_000_000,
    }]);
    let next_cue_start = 11_500_000_000;

    trim_overlapping_subtitle_cues_at(&mut cues, next_cue_start);

    cues.push_back(BackendSubtitleCue {
        text: "second".to_string(),
        bitmaps: Vec::new(),
        start_nsecs: next_cue_start,
        end_nsecs: 13_000_000_000,
    });

    assert_eq!(cues[0].start_nsecs, 10_000_000_000);
    assert_eq!(cues[0].end_nsecs, next_cue_start);
    assert_eq!(cues[1].start_nsecs, next_cue_start);
    assert_eq!(cues[1].end_nsecs, 13_000_000_000);
}

#[test]
fn late_video_drop_waits_for_grace_after_frame_end() {
    assert!(!should_drop_late_video_frame(
        1_000_000_000,
        16_000_000,
        1_090_000_000
    ));
    assert!(should_drop_late_video_frame(
        1_000_000_000,
        16_000_000,
        1_091_000_000
    ));
}

#[test]
fn audio_clock_vo_admission_drops_single_late_frame() {
    let mut frame = test_queued_video_frame(1_000_000_000);
    frame.duration_nsecs = 16_000_000;
    let mut queue = VecDeque::from([frame]);

    let result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_091_000_000);

    assert!(result.frame.is_none());
    assert_eq!(result.dropped_frames, 1);
    assert!(queue.is_empty());
}

#[test]
fn audio_clock_vo_admission_reports_superseded_due_frames() {
    let mut queue = VecDeque::new();
    queue.push_back(test_queued_video_frame(1_000_000_000));
    queue.push_back(test_queued_video_frame(1_010_000_000));
    queue.push_back(test_queued_video_frame(1_020_000_000));

    let result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_015_000_000);

    assert_eq!(result.frame.unwrap().timeline_nsecs, 1_010_000_000);
    assert_eq!(result.dropped_frames, 1);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.front().unwrap().timeline_nsecs, 1_020_000_000);
}

#[test]
fn video_frame_corruption_detection_uses_flags_and_decode_errors() {
    assert!(!frame_is_corrupt(std::ptr::null_mut()));
    assert_eq!(frame_decode_error_flags(std::ptr::null_mut()), 0);

    let mut frame = AvFrame::new().expect("frame allocates");
    assert!(!frame_is_corrupt(frame.as_mut_ptr()));

    unsafe {
        (*frame.as_mut_ptr()).flags = ffi::AV_FRAME_FLAG_CORRUPT;
    }
    assert!(frame_is_corrupt(frame.as_mut_ptr()));

    unsafe {
        (*frame.as_mut_ptr()).flags = 0;
        (*frame.as_mut_ptr()).decode_error_flags = ffi::FF_DECODE_ERROR_MISSING_REFERENCE;
    }
    assert!(frame_is_corrupt(frame.as_mut_ptr()));
    assert_eq!(
        frame_decode_error_flags(frame.as_mut_ptr()),
        ffi::FF_DECODE_ERROR_MISSING_REFERENCE
    );
}

#[test]
fn video_decode_recovery_waits_for_keyframe_after_error() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut delta_packet = AvPacket::new().expect("packet allocates");
    let mut key_packet = AvPacket::new().expect("packet allocates");
    unsafe {
        (*delta_packet.as_mut_ptr()).flags = 0;
        (*key_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    assert!(!recovery.waiting_for_keyframe());
    assert!(!recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));

    recovery.begin_with_realign(true);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert_eq!(recovery.record_skipped_packet(), 1);
    assert_eq!(recovery.record_skipped_packet(), 2);
    assert!(!recovery.should_skip_packet(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(recovery.accept_recovery_point(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(recovery.take_realign_on_next_frame());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_can_resume_without_realign_after_live_error() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut key_packet = AvPacket::new().expect("packet allocates");
    unsafe {
        (*key_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.begin_with_realign(false);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.accept_recovery_point(&key_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_waits_for_hevc_seek_recovery_point_after_seek() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    unsafe {
        (*key_idr_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 0);
    assert!(!recovery.waiting_for_keyframe());

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_MPEG4, 1_000_000_000);
    assert!(!recovery.waiting_for_keyframe());

    recovery.reset_for_timeline_start(ffi::AVCodecID::AV_CODEC_ID_HEVC, 1_000_000_000);
    assert!(recovery.waiting_for_keyframe());
    assert!(recovery.accept_recovery_point(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.take_realign_on_next_frame());
}

fn test_packet_from_data(data: &[u8]) -> AvPacket {
    let props = AvPacket::new().expect("packet props allocate");
    AvPacket::from_data_and_props(data, &props).expect("packet data allocates")
}

#[test]
fn video_decode_recovery_hevc_requires_safe_key_recovery_point() {
    let mut recovery = VideoDecodeRecovery::default();
    let mut non_key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    let mut key_idr_packet = test_packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);
    unsafe {
        (*non_key_idr_packet.as_mut_ptr()).flags = 0;
        (*key_idr_packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
    }

    recovery.begin_with_realign(true);
    assert!(recovery.should_skip_packet(&non_key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_recovery_point(&non_key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.should_skip_packet(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.accept_recovery_point(&key_idr_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
}

#[test]
fn video_decode_recovery_has_bounded_wait_for_recovery_point() {
    let mut recovery = VideoDecodeRecovery::default();
    let delta_packet = AvPacket::new().expect("packet allocates");

    recovery.begin_with_realign(true);
    for _ in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
        assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
        recovery.record_skipped_packet();
    }

    assert!(!recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(recovery.accept_after_wait_limit(ffi::AVCodecID::AV_CODEC_ID_MPEG4));
    assert!(!recovery.waiting_for_keyframe());
    assert!(recovery.take_realign_on_next_frame());
}

#[test]
fn video_decode_recovery_hevc_does_not_resume_after_wait_limit() {
    let mut recovery = VideoDecodeRecovery::default();
    let delta_packet = AvPacket::new().expect("packet allocates");

    recovery.begin_with_realign(true);
    for _ in 0..VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS {
        assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
        recovery.record_skipped_packet();
    }

    assert!(recovery.should_skip_packet(&delta_packet, ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(!recovery.accept_after_wait_limit(ffi::AVCodecID::AV_CODEC_ID_HEVC));
    assert!(recovery.waiting_for_keyframe());
    assert!(!recovery.take_realign_on_next_frame());
}

#[test]
fn recovered_video_frame_realigns_before_start_gate() {
    assert_eq!(
        decoded_video_frame_start_action(9_000_000_000, 10_000_000_000, false),
        DecodedVideoFrameStartAction::DropBeforeStart
    );
    assert_eq!(
        decoded_video_frame_start_action(9_000_000_000, 10_000_000_000, true),
        DecodedVideoFrameStartAction::Use { realign: true }
    );
    assert_eq!(
        decoded_video_frame_start_action(11_000_000_000, 10_000_000_000, true),
        DecodedVideoFrameStartAction::Use { realign: true }
    );
}

#[test]
fn video_decode_error_recovery_classifies_decoder_errors() {
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 接收解码帧失败：Invalid data found when processing input"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 发送解码包失败：Invalid data found when processing input"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 发送解码包失败：Cannot allocate memory"
    ));
    assert!(video_decode_error_is_recoverable(
        "FFmpeg 接收解码帧失败：VK_ERROR_OUT_OF_DEVICE_MEMORY"
    ));
    assert!(!video_decode_error_is_recoverable(
        "FFmpeg 创建视频色彩转换器失败"
    ));
}

fn test_queued_video_frames_with_duration(
    start_timeline_nsecs: u64,
    frame_count: usize,
    frame_duration_nsecs: u64,
) -> VecDeque<QueuedVideoFrame> {
    let mut queued = VecDeque::new();
    for index in 0..frame_count {
        let mut frame = test_queued_video_frame(
            start_timeline_nsecs + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }
    queued
}

fn test_pending_audio(start_timeline_nsecs: u64, duration_nsecs: u64) -> PendingStartAudio {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        },
        start_timeline_nsecs,
        start_timeline_nsecs + duration_nsecs,
    );
    pending
}

fn ready_demux_watermark(forward_nsecs: u64) -> DemuxReaderWatermark {
    DemuxReaderWatermark {
        video_forward_nsecs: Some(forward_nsecs),
        audio_forward_nsecs: Some(forward_nsecs),
        selected_min_forward_nsecs: Some(forward_nsecs),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    }
}

fn test_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
    QueuedVideoFrame {
        frame: DecodedFrame {
            size: RenderSize {
                width: 1,
                height: 1,
            },
            pts: Some(FramePts {
                nsecs: timeline_nsecs,
            }),
            key_frame: false,
            pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
        },
        timeline_nsecs,
        duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    }
}

fn test_vulkan_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
    let mut queued = test_queued_video_frame(timeline_nsecs);
    let mut av_frame = AvFrame::new().expect("FFmpeg frame allocates");
    unsafe {
        (*av_frame.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
        (*av_frame.as_mut_ptr()).width = 1;
        (*av_frame.as_mut_ptr()).height = 1;
    }
    let buffer_result = unsafe { ffi::av_frame_get_buffer(av_frame.as_mut_ptr(), 1) };
    assert!(buffer_result >= 0, "FFmpeg frame buffer allocates");

    queued.frame.pixels = FramePixels::VulkanVideo(VulkanVideoFrame {
        frame: FfmpegFrameRef::new_ref(av_frame.as_mut_ptr()).expect("FFmpeg frame refs"),
        device: test_vulkan_device(),
        format: RawVideoFormat::P010Le,
        usage: 0,
        color: FrameColor::Sdr,
        range: RawVideoRange::Limited,
        chroma_site: RawVideoChromaSite::Left,
        metadata: None,
        planes: Vec::new(),
    });
    queued
}

fn test_vulkan_device() -> Arc<VulkanDecodeDevice> {
    let mut buffer = unsafe { ffi::av_buffer_alloc(1) };
    assert!(!buffer.is_null(), "FFmpeg buffer allocates");
    let device_ref = FfmpegAvBufferRef::new_ref(buffer).expect("FFmpeg buffer refs");
    unsafe { ffi::av_buffer_unref(&mut buffer) };
    Arc::new(VulkanDecodeDevice::new(
        device_ref,
        0,
        0,
        0,
        1,
        0,
        0,
        0,
        VulkanDecodeQueues {
            graphics: VulkanDecodeQueue { index: 0, count: 1 },
            compute: None,
            transfer: None,
        },
    ))
}

#[test]
fn pending_start_audio_buffers_decoded_audio_until_first_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 6],
            duration_nsecs: 30_000_000,
        },
        1_020_000_000,
        1_050_000_000,
    );

    assert_eq!(pending.len(), 2);
    assert_eq!(pending.queued_samples(), 10);
}

#[test]
fn pending_start_audio_discards_frames_before_first_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        900_000_000,
        920_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );

    assert_eq!(pending.discard_before(1_000_000_000), 1);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.queued_samples(), 4);
}

#[test]
fn pending_start_audio_keeps_frame_overlapping_playback_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 32_000_000,
        },
        576_000_000,
        608_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 32_000_000,
        },
        608_000_000,
        640_000_000,
    );

    assert_eq!(pending.discard_before(576_018_171), 0);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.first_start_timeline_nsecs(), Some(576_000_000));
    assert_eq!(pending.forward_duration_from(576_018_171), Some(63_981_829));
}

#[test]
fn pending_start_audio_trims_overlapping_frame_to_playback_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 64],
            duration_nsecs: 32_000_000,
        },
        576_000_000,
        608_000_000,
    );

    let mut frame = pending
        .pop_front_until(608_000_000)
        .expect("covered frame pops");
    assert!(frame.trim_before(592_000_000, 1_000, 2));

    assert_eq!(frame.start_timeline_nsecs, 592_000_000);
    assert_eq!(frame.end_timeline_nsecs, 608_000_000);
    assert_eq!(frame.samples.len(), 32);
}

#[test]
fn pending_start_audio_reports_first_start_timeline() {
    let mut pending = PendingStartAudio::default();
    assert_eq!(pending.first_start_timeline_nsecs(), None);

    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );

    assert_eq!(pending.first_start_timeline_nsecs(), Some(1_000_000_000));
}

#[test]
fn pending_start_audio_reports_contiguous_forward_duration() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_040_000_000,
        1_060_000_000,
    );

    assert_eq!(
        pending.forward_duration_from(1_000_000_000),
        Some(20_000_000)
    );
    assert_eq!(pending.forward_duration_from(1_020_000_000), None);
}

#[test]
fn pending_start_audio_tolerates_small_timestamp_gaps() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_021_000_000,
        1_041_000_000,
    );

    assert_eq!(
        pending.forward_duration_from(1_000_000_000),
        Some(41_000_000)
    );
}

#[test]
fn pending_start_audio_pops_only_frames_covered_by_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );

    assert!(pending.pop_front_until(1_019_999_999).is_none());
    assert_eq!(pending.len(), 2);
    assert!(pending.pop_front_until(1_020_000_000).is_some());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.first_start_timeline_nsecs(), Some(1_020_000_000));
}

#[test]
fn pending_audio_underrun_recovery_waits_for_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(&pending, 1_000_000_000, 0, None, None),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_resets_to_next_audio_with_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            Some(1_400_000_000),
            Some(1_900_000_000)
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_400_000_000,
            audio_flush_until_timeline_nsecs: 1_900_000_000,
            reset_audio_to_timeline_nsecs: Some(1_400_000_000),
        })
    );
}

#[test]
fn pending_audio_underrun_recovery_waits_for_existing_audio_before_clock_reset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            50_000_000,
            Some(1_400_000_000),
            Some(1_900_000_000)
        ),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_uses_video_lead_when_available() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            Some(1_000_000_000),
            Some(1_300_000_000),
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_000_000_000,
            audio_flush_until_timeline_nsecs: 1_800_000_000,
            reset_audio_to_timeline_nsecs: None,
        })
    );
}

#[test]
fn pending_audio_underrun_recovery_resets_to_video_start_when_audio_leads_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            Some(1_400_000_000),
            Some(1_900_000_000),
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_400_000_000,
            audio_flush_until_timeline_nsecs: 1_900_000_000,
            reset_audio_to_timeline_nsecs: Some(1_400_000_000),
        })
    );
}

#[test]
fn pending_audio_underrun_recovery_waits_for_actual_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            Some(1_000_000_000),
            Some(1_040_000_000),
        ),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_discards_stale_pending_audio_before_video_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_000_000_000,
        1_500_000_000,
    );

    assert_eq!(
        discard_stale_pending_audio_before_recovery_start(
            &mut pending,
            1_800_000_000,
            0,
            Some(2_000_000_000)
        ),
        1
    );
    assert!(pending.is_empty());
}

#[test]
fn audio_sample_len_rejects_invalid_sizes() {
    assert!(audio_sample_len(-1, FALLBACK_AUDIO_OUTPUT_CHANNELS).is_err());
    assert!(audio_sample_len(1024, 0).is_err());
    assert_eq!(
        audio_sample_len(1024, FALLBACK_AUDIO_OUTPUT_CHANNELS).unwrap(),
        1024 * FALLBACK_AUDIO_OUTPUT_CHANNELS as usize
    );
}

#[test]
fn audio_ring_buffer_reuses_fixed_capacity_and_wraps() {
    let mut buffer = AudioBuffer::with_capacity(4);

    assert_eq!(buffer.push_slice(&[1.0, 2.0, 3.0]), 3);
    assert_eq!(buffer.pop_sample(), Some(1.0));
    assert_eq!(buffer.pop_sample(), Some(2.0));
    assert_eq!(buffer.push_slice(&[4.0, 5.0, 6.0]), 3);
    assert_eq!(buffer.push_slice(&[7.0]), 0);

    assert_eq!(buffer.pop_sample(), Some(3.0));
    assert_eq!(buffer.pop_sample(), Some(4.0));
    assert_eq!(buffer.pop_sample(), Some(5.0));
    assert_eq!(buffer.pop_sample(), Some(6.0));
    assert_eq!(buffer.pop_sample(), None);
}

#[test]
fn audio_samples_duration_accounts_for_interleaved_channels() {
    assert_eq!(
        audio_samples_duration(96_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        Duration::from_secs(1)
    );
    assert_eq!(
        audio_samples_duration(0, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        Duration::ZERO
    );
    assert_eq!(audio_samples_duration(1024, 0, 2), Duration::ZERO);
    assert_eq!(audio_samples_duration(1024, 48_000, 0), Duration::ZERO);
}

#[test]
fn samples_for_duration_accounts_for_interleaved_channels() {
    assert_eq!(
        samples_for_duration(1_000_000_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
        96_000
    );
    assert_eq!(samples_for_duration(0, 48_000, 2), 0);
    assert_eq!(samples_for_duration(1_000_000_000, 0, 2), 0);
    assert_eq!(samples_for_duration(1_000_000_000, 48_000, 0), 0);
}

fn test_audio_shared(max_samples: usize) -> AudioShared {
    AudioShared::new(
        max_samples,
        48_000,
        FALLBACK_AUDIO_OUTPUT_CHANNELS,
        Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
    )
}

#[test]
fn audio_clock_uses_queued_end_minus_pending_audio() {
    let shared = test_audio_shared(960);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);

    assert_eq!(shared.played_timeline_nsecs(), 1_000_000_000);
}

#[test]
fn audio_clock_subtracts_output_device_delay() {
    let shared = test_audio_shared(960);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);
    shared.set_output_delay_for_test(Duration::from_millis(20));

    assert_eq!(shared.played_timeline_nsecs(), 980_000_000);
}

#[test]
fn dovi_packet_timeline_uses_stream_start_when_available() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut first_packet_nsecs = None;

    assert_eq!(
        dovi_packet_timeline_nsecs(
            &mut first_packet_nsecs,
            Some(1_000_000_000),
            1_250,
            time_base,
        ),
        Some(250_000_000)
    );
    assert_eq!(first_packet_nsecs, None);
}

#[test]
fn dovi_packet_timeline_uses_first_packet_when_stream_start_is_missing() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut first_packet_nsecs = None;

    assert_eq!(
        dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_250, time_base),
        Some(0)
    );
    assert_eq!(
        dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_500, time_base),
        Some(250_000_000)
    );
}

#[test]
fn fill_audio_output_converts_samples_and_outputs_silence_on_underrun() {
    let shared = test_audio_shared(8);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.0, 1.0]),
        3
    );
    let mut output = [0.0f64; 4];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [-1.0, 0.0, 1.0, 0.0]);
    assert!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .is_empty()
    );
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
    assert!(shared.underrun_active_for_test());
}

#[test]
fn audio_clock_freezes_during_output_underrun_until_pending_recovers() {
    let shared = test_audio_shared(4_800);
    shared.reset_clock(1_000_000_000);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(1_010_000_000);

    let mut output = [0.0f64; 1_920];
    fill_audio_output(&mut output, &shared);

    let frozen_timeline_nsecs = shared.played_timeline_nsecs();
    assert!(frozen_timeline_nsecs.abs_diff(1_000_000_000) <= 1_000);
    assert!(shared.underrun_active_for_test());

    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 960]),
        960
    );
    shared.set_queued_end_timeline_nsecs(2_000_000_000);
    assert_eq!(shared.played_timeline_nsecs(), frozen_timeline_nsecs);

    shared.clear_underrun_if_recovered_for_test(duration_nsecs(
        AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    ));
    assert!(!shared.underrun_active_for_test());
    assert_ne!(shared.played_timeline_nsecs(), frozen_timeline_nsecs);
}

#[test]
fn fill_audio_output_applies_playback_volume() {
    let shared = test_audio_shared(8);
    shared.control.set_volume(0.25);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.5, 1.0]),
        3
    );
    let mut output = [0.0f64; 3];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [-0.25, 0.125, 0.25]);
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
}

#[test]
fn fill_audio_output_preserves_buffer_while_paused() {
    let shared = test_audio_shared(8);
    shared.control.set_user_paused(true);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&[-1.0, 0.0, 1.0]),
        3
    );
    let mut output = [0.5f64; 4];

    fill_audio_output(&mut output, &shared);

    assert_eq!(output, [0.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .len(),
        3
    );
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 0);
}

#[test]
fn ffmpeg_http_headers_formats_crlf_separated_headers() {
    let headers = ffmpeg_http_headers(&[
        ("X-Emby-Token".to_string(), "token".to_string()),
        ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
    ])
    .unwrap();

    assert_eq!(
        headers,
        "X-Emby-Token: token\r\nUser-Agent: Lenna/1.0.13\r\n"
    );
}

#[test]
fn ffmpeg_http_headers_rejects_header_injection() {
    assert!(ffmpeg_http_headers(&[("Bad\nName".to_string(), "value".to_string())]).is_err());
    assert!(
        ffmpeg_http_headers(&[("X-Emby-Token".to_string(), "bad\r\nvalue".to_string())]).is_err()
    );
}

#[test]
fn detects_cacheable_http_urls() {
    assert!(should_cache_http_url("https://example.test/video.mp4"));
    assert!(should_cache_http_url("HTTP://example.test/video.mp4"));
    assert!(!should_cache_http_url("file:///tmp/video.mp4"));
    assert!(!should_cache_http_url("/tmp/video.mp4"));
}

#[test]
fn cached_input_source_keeps_http_cache_alive_between_probe_readers() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        HTTP_CACHE_CHUNK_SIZE,
    ));
    let mut source = CachedInputSource::from_cache_for_test(cache.clone());

    let first_reader = source
        .cached_avio()
        .expect("cached AVIO can be created")
        .expect("HTTP source uses cached AVIO");
    drop(first_reader);
    assert!(!cache.is_shutdown_for_test());

    let mut final_reader = source
        .cached_avio()
        .expect("cached AVIO can be recreated")
        .expect("HTTP source uses cached AVIO");
    final_reader.shutdown_cache_on_drop();
    source.release();
    drop(source);
    assert!(!cache.is_shutdown_for_test());

    drop(final_reader);
    assert!(cache.is_shutdown_for_test());
}

#[test]
fn cached_input_source_shutdowns_http_cache_when_no_reader_is_released() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        HTTP_CACHE_CHUNK_SIZE,
    ));

    drop(CachedInputSource::from_cache_for_test(cache.clone()));

    assert!(cache.is_shutdown_for_test());
}

#[test]
fn cached_input_source_skips_http_cache_when_cache_mode_is_disabled() {
    let (event_tx, _) = mpsc::channel();
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Disabled,
        ..PlaybackCacheConfig::default()
    };
    let source = CachedInputSource::new(
        "https://example.test/video.mp4",
        &[],
        Some(1_024),
        &config,
        Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
        event_tx,
    )
    .expect("source creates without spawning HTTP cache");

    assert!(
        source
            .cached_avio()
            .expect("cache lookup succeeds")
            .is_none()
    );
}

#[test]
fn http_cache_request_header_log_includes_effective_headers() {
    let headers = reqwest_header_pairs(&[
        ("X-Emby-Token".to_string(), "token".to_string()),
        ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
    ])
    .unwrap();

    assert_eq!(
        http_cache_request_headers_for_log(&headers, "bytes=128-255"),
        vec![
            "accept-encoding: identity".to_string(),
            "connection: keep-alive".to_string(),
            "range: bytes=128-255".to_string(),
            "x-emby-token: token".to_string(),
            "user-agent: Lenna/1.0.13".to_string(),
        ]
    );
}

#[test]
fn http_cache_range_header_limits_request_size() {
    assert_eq!(
        http_cache_range_header(0, None, HTTP_CACHE_RANGE_REQUEST_BYTES),
        "bytes=0-33554431"
    );
    assert_eq!(
        http_cache_range_header(128, None, HTTP_CACHE_RANGE_REQUEST_BYTES),
        "bytes=128-33554559"
    );
    assert_eq!(
        http_cache_range_header(
            595_453_649,
            Some(596_486_439),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        "bytes=595453649-596486438"
    );
    assert_eq!(
        http_cache_range_header(
            10_675_366_349,
            Some(10_675_368_645),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        "bytes=10675366349-10675368644"
    );
    assert_eq!(http_cache_range_header(0, None, 1024), "bytes=0-1023");
}

#[test]
fn http_cache_range_request_timeout_is_short_for_small_tail_ranges() {
    assert_eq!(
        http_cache_range_request_len(
            10_675_366_349,
            Some(10_675_368_645),
            HTTP_CACHE_RANGE_REQUEST_BYTES
        ),
        2_296
    );
    assert_eq!(
        http_cache_range_request_timeout(2_296),
        HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT
    );
    assert_eq!(
        http_cache_range_request_timeout(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES + 1),
        HTTP_CACHE_RANGE_REQUEST_TIMEOUT
    );
}

#[test]
fn http_cache_response_header_log_includes_response_headers() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 10-19/100"),
    );
    headers.insert(
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::HeaderValue::from_static("10"),
    );

    assert_eq!(
        http_cache_response_headers_for_log(&headers),
        vec![
            "content-length: 10".to_string(),
            "content-range: bytes 10-19/100".to_string(),
        ]
    );
}

#[test]
fn http_ring_cache_state_copies_available_bytes() {
    let mut state = HttpRingCacheState::new(10);
    state.append_at(10, b"abcdef");
    let mut output = [0; 3];

    assert_eq!(state.copy_available(12, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(state.copy_available(16, &mut output), None);
}

#[test]
fn http_ring_cache_probe_read_does_not_move_playback_reader() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.set_reader_offset(103);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(104, &mut output),
        CacheReadResult::Data(2)
    ));
    assert_eq!(&output, b"ef");
    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());

    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());
}

#[test]
fn http_ring_cache_probe_read_queues_trimmed_range_without_active_restart() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.set_reader_offset(104);
    state.trim_to_capacity(2);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(100, &mut output),
        CacheReadResult::WouldBlock
    ));
    assert!(!cache.has_restart_request_for_test());
    assert_eq!(
        cache.side_download_requests_for_test(),
        vec![CacheRestartRequest {
            offset: 100,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
}

#[test]
fn http_ring_cache_probe_read_queues_side_download_without_active_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");
    state.set_reader_offset(103);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(500, &mut output),
        CacheReadResult::WouldBlock
    ));
    assert_eq!(cache.reader_offset_for_test(), 103);
    assert!(!cache.has_restart_request_for_test());
    assert_eq!(
        cache.side_download_requests_for_test(),
        vec![CacheRestartRequest {
            offset: 500,
            range_kind: HttpCacheRangeKind::Playback,
        }]
    );
}

#[test]
fn http_ring_cache_read_at_returns_large_partial_buffer_without_waiting_for_full_request() {
    let mut state = HttpRingCacheState::new(0);
    let cached = vec![0x5a; HTTP_CACHE_PARTIAL_READ_MIN_BYTES];
    state.append_at(0, &cached);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = vec![0; HTTP_CACHE_PARTIAL_READ_MIN_BYTES * 2];

    assert!(matches!(
        cache.read_at_for_test(0, &mut output),
        CacheReadResult::Data(HTTP_CACHE_PARTIAL_READ_MIN_BYTES)
    ));
    assert_eq!(&output[..HTTP_CACHE_PARTIAL_READ_MIN_BYTES], &cached);
}

#[test]
fn http_ring_cache_read_at_returns_small_partial_buffer_without_waiting() {
    let mut state = HttpRingCacheState::new(0);
    let cached = vec![0x5a; HTTP_CACHE_PARTIAL_READ_MIN_BYTES / 2];
    state.append_at(0, &cached);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = vec![0; HTTP_CACHE_PARTIAL_READ_MIN_BYTES];

    assert!(matches!(
        cache.read_at_for_test(0, &mut output),
        CacheReadResult::Data(read) if read == cached.len()
    ));
    assert_eq!(&output[..cached.len()], &cached);
}

#[test]
fn http_ring_cache_state_reports_buffered_ahead_for_active_playback_range() {
    let mut state = HttpRingCacheState::new(10);
    state.append_at(10, b"abcdef");

    assert_eq!(state.buffered_ahead_from(10), 6);
    assert_eq!(state.buffered_ahead_from(13), 3);
    assert_eq!(state.buffered_ahead_from(16), 0);
    assert_eq!(state.buffered_ahead_from(9), 0);
    assert_eq!(state.buffered_ahead_from(17), 0);
}

#[test]
fn http_ring_cache_state_retains_cached_range_across_tail_metadata_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(state.copy_available(990, &mut output), Some(3));
    assert_eq!(&output, b"tai");
}

#[test]
fn http_ring_cache_state_hides_tail_metadata_range_from_progress() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_reports_playback_progress_while_tail_metadata_active() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");

    assert_eq!(
        state
            .playback_buffer_range()
            .map(PlaybackCacheByteRange::from),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_keeps_retained_ranges_after_playback_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");
    state.restart_at_with_kind(200, HttpCacheRangeKind::Playback);
    state.append_at(200, b"ghij");

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
    assert_eq!(
        state
            .playback_buffer_range()
            .map(PlaybackCacheByteRange::from),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.2,
            end_fraction: 0.204,
        })
    );
    assert_eq!(
        state.stream_cache_status_for_test().ranges,
        vec![
            PlaybackCacheByteRange {
                start_fraction: 0.1,
                end_fraction: 0.106,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.2,
                end_fraction: 0.204,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.99,
                end_fraction: 0.994,
            },
        ]
    );
}

#[test]
fn http_ring_cache_state_reports_near_tail_playback_range() {
    let mut state = HttpRingCacheState::new(980).with_content_len_hint(Some(1_000));
    state.append_at(980, b"tail");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.98,
            end_fraction: 0.984,
        })
    );
}

#[test]
fn http_ring_cache_state_classifies_far_tail_seek_as_metadata_probe() {
    let content_len = HTTP_CACHE_RANGE_REQUEST_BYTES * 4;
    let mut state = HttpRingCacheState::new(HTTP_CACHE_RANGE_REQUEST_BYTES)
        .with_content_len_hint(Some(content_len));
    state.append_at(HTTP_CACHE_RANGE_REQUEST_BYTES, b"abcdef");

    assert!(state.is_tail_metadata_probe_seek(content_len - 1024));
}

#[test]
fn http_ring_cache_state_treats_near_tail_active_range_as_playback() {
    let content_len = HTTP_CACHE_RANGE_REQUEST_BYTES * 4;
    let start_offset = content_len - HTTP_CACHE_RANGE_REQUEST_BYTES / 2;
    let mut state = HttpRingCacheState::new(start_offset).with_content_len_hint(Some(content_len));
    state.append_at(start_offset, b"abcdef");

    assert!(!state.is_tail_metadata_probe_seek(content_len - 1024));
}

#[test]
fn http_ring_cache_state_retains_cached_range_for_non_tail_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000_000_000));
    state.append_at(100, b"abcdef");

    state.restart_at(10_000);

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), Some(3));
    assert_eq!(&output, b"cde");
}

#[test]
fn http_ring_cache_state_appends_side_range_without_restarting_active_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    assert!(state.append_retained_at(900, b"tail", HttpCacheRangeKind::TailMetadataProbe));

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(900, &mut output), Some(4));
    assert_eq!(&output, b"tail");
    assert_eq!(
        state.stream_cache_status_for_test().ranges,
        vec![
            PlaybackCacheByteRange {
                start_fraction: 0.1,
                end_fraction: 0.106,
            },
            PlaybackCacheByteRange {
                start_fraction: 0.9,
                end_fraction: 0.904,
            },
        ]
    );
}

#[test]
fn http_ring_cache_state_appends_overlapping_side_range_without_duplicate_bytes() {
    let mut state =
        HttpRingCacheState::new_with_cache_capacity(0, 16).with_content_len_hint(Some(1_000));

    assert!(state.append_retained_at(900, b"tail", HttpCacheRangeKind::TailMetadataProbe));
    assert!(state.append_retained_at(902, b"il!!", HttpCacheRangeKind::TailMetadataProbe));

    let mut output = [0; 6];
    assert_eq!(state.copy_available(900, &mut output), Some(6));
    assert_eq!(&output, b"tail!!");
    assert_eq!(state.stream_cache_status_for_test().cached_bytes, 6);
}

#[test]
fn http_ring_cache_state_prunes_least_recently_used_retained_range() {
    let mut state =
        HttpRingCacheState::new_with_cache_capacity(0, 8).with_content_len_hint(Some(1_000));
    state.append_at(0, b"abcd");
    state.restart_at(100);
    state.append_at(100, b"efgh");
    state.restart_at(200);

    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");

    state.append_at(200, b"ijkl");

    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
    assert_eq!(state.copy_available(100, &mut output), None);
    assert_eq!(state.copy_available(200, &mut output), Some(4));
    assert_eq!(&output, b"ijkl");
}

#[test]
fn http_ring_cache_state_counts_byte_level_seeks_outside_active_range() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.note_seek_offset(102, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 0);

    state.note_seek_offset(500, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 1);

    state.restart_at(500);
    state.append_at(500, b"ghij");
    state.note_seek_offset(102, HttpCacheRangeKind::Playback);
    assert_eq!(state.stream_cache_status_for_test().byte_level_seeks, 2);
}

#[test]
fn http_ring_cache_state_reports_recent_raw_input_rate() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(1_000));

    state.append_at(0, b"abcdef");
    state.append_at(6, b"ghij");

    assert_eq!(
        state.stream_cache_status_for_test().raw_input_rate,
        Some(10)
    );
}

#[test]
fn http_ring_cache_state_uses_content_length_hint_for_progress() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(100));

    state.append_at(0, b"abcde");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(PlaybackCacheByteRange {
            start_fraction: 0.0,
            end_fraction: 0.05,
        })
    );
}

#[test]
fn http_ring_cache_state_trims_oldest_bytes() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.set_reader_offset(102);
    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 102);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(102, &mut output), Some(4));
    assert_eq!(&output, b"cdef");
    assert_eq!(state.copy_available(100, &mut output), None);
}

#[test]
fn http_ring_cache_state_copies_wrapped_bytes() {
    let mut state = HttpRingCacheState::new_with_cache_capacity(0, 6);
    state.append_at(0, b"abcdef");

    state.set_reader_offset(4);
    state.append_at(6, b"ghij");

    assert_eq!(state.base_offset, 4);
    assert_eq!(state.next_offset, 10);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(4, &mut output), Some(6));
    assert_eq!(&output, b"efghij");
}

#[test]
fn http_ring_cache_state_preserves_unread_bytes_when_over_capacity() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 100);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(100, &mut output), Some(6));
    assert_eq!(&output, b"abcdef");
}

#[test]
fn http_ring_cache_state_refuses_append_when_capacity_is_unread() {
    let mut state = HttpRingCacheState::new_with_cache_capacity(0, 4);
    assert!(state.append_at(0, b"abcd"));

    assert!(!state.append_at(4, b"ef"));

    assert_eq!(state.base_offset, 0);
    assert_eq!(state.next_offset, 4);
    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
}

#[test]
fn http_ring_cache_state_limits_prefetch_window_from_reader() {
    let mut state = HttpRingCacheState::new(100);

    assert_eq!(
        state.append_capacity_from(100 + HTTP_RING_CACHE_CAPACITY as u64),
        0
    );

    let mut state = HttpRingCacheState::new(100);
    assert_eq!(
        state.append_capacity_from(99 + HTTP_RING_CACHE_CAPACITY as u64),
        1
    );

    state.set_reader_offset(200);
    assert_eq!(
        state.append_capacity_from(100 + HTTP_RING_CACHE_CAPACITY as u64),
        100
    );
}

#[test]
fn http_ring_cache_state_pauses_prefetch_until_hysteresis_resume() {
    let mut state = HttpRingCacheState::new_with_readahead_for_test(0, 1_000, 10.0, 2.0)
        .with_content_len_hint(Some(10_000));
    state.set_duration_seconds_for_test(100.0);

    assert_eq!(state.append_capacity_from(1_000), 0);
    assert_eq!(state.append_capacity_from(999), 0);
    assert_eq!(state.append_capacity_from(800), 200);
}

#[test]
fn http_ring_cache_state_applies_live_cache_config() {
    let mut state = HttpRingCacheState::new_with_readahead_for_test(0, 1_000, 10.0, 2.0)
        .with_content_len_hint(Some(1_000));
    state.set_duration_seconds_for_test(100.0);

    assert_eq!(state.append_capacity_from(0), 100);

    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 2.0,
        demuxer_readahead_secs: 1.0,
        demuxer_hysteresis_secs: 0.5,
        ..PlaybackCacheConfig::default()
    };
    state.apply_cache_config(&config);

    assert_eq!(state.append_capacity_from(0), 20);
}

#[test]
fn http_ring_cache_applies_live_memory_budget_from_cache_config() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        1024 * 1024,
    ));
    let config = PlaybackCacheConfig {
        http_cache_max_bytes: 128 * 1024,
        http_cache_chunk_bytes: 64 * 1024,
        ..PlaybackCacheConfig::default()
    };

    cache.apply_cache_config(&config);

    assert_eq!(cache.memory_capacity_for_test(), 128 * 1024);
}

#[test]
fn http_ring_cache_applies_live_range_request_budget_from_cache_config() {
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        1024 * 1024,
    ));
    let config = PlaybackCacheConfig {
        http_cache_chunk_bytes: 64 * 1024,
        http_cache_range_request_bytes: 2 * 1024 * 1024,
        ..PlaybackCacheConfig::default()
    };

    cache.apply_cache_config(&config);

    assert_eq!(cache.range_request_bytes_for_test(), 2 * 1024 * 1024);
}

#[test]
fn http_ring_cache_state_reads_trimmed_bytes_from_disk_cache() {
    let mut state =
        HttpRingCacheState::new_with_disk_cache_for_test(0, 4, 16).with_content_len_hint(Some(8));
    assert!(state.append_at(0, b"abcd"));

    state.set_reader_offset(4);
    assert!(state.append_at(4, b"efgh"));

    let mut output = [0; 4];
    assert_eq!(state.copy_available(0, &mut output), Some(4));
    assert_eq!(&output, b"abcd");
}

#[test]
fn http_ring_cache_state_counts_overlapping_memory_and_disk_bytes_once() {
    let mut state =
        HttpRingCacheState::new_with_disk_cache_for_test(0, 4, 16).with_content_len_hint(Some(8));

    assert!(state.append_at(0, b"abcd"));
    assert_eq!(state.stream_cache_status_for_test().cached_bytes, 4);

    state.set_reader_offset(4);
    assert!(state.append_at(4, b"efgh"));

    let status = state.stream_cache_status_for_test();
    assert_eq!(status.cached_bytes, 8);
    assert_eq!(
        status.ranges,
        vec![PlaybackCacheByteRange {
            start_fraction: 0.0,
            end_fraction: 1.0,
        }]
    );
}

#[test]
fn http_ring_cache_state_uses_active_range_for_prefetch_window() {
    let tail_offset = 100 + HTTP_RING_CACHE_CAPACITY as u64 + 1_000;
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(tail_offset + 1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(tail_offset, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(tail_offset, b"tail");
    state.set_reader_offset(102);

    assert_eq!(
        state.append_capacity_from(tail_offset + 4),
        HTTP_RING_CACHE_CAPACITY - 4
    );
}

#[test]
fn http_ring_cache_state_demotes_active_range_on_seek_outside_cached_range() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.note_seek_offset(10_000, HttpCacheRangeKind::Playback);
    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 106);
    assert_eq!(state.next_offset, 106);
    let mut output = [0; 6];
    assert_eq!(state.copy_available(100, &mut output), Some(6));
    assert_eq!(&output, b"abcdef");
}

#[test]
fn http_ring_cache_state_restart_clears_eof_for_next_range() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.eof = true;

    state.restart_at(0);

    assert_eq!(state.base_offset, 0);
    assert_eq!(state.next_offset, 0);
    assert!(!state.eof);
}

#[test]
fn content_range_parser_reads_total_size() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 100-199/12345"),
    );

    assert_eq!(content_len_from_content_range(&headers), Some(12345));
    assert_eq!(
        content_range_from_headers(&headers),
        Some(HttpContentRange {
            start: 100,
            end: 199,
            total: Some(12345),
        })
    );
}

#[test]
fn content_range_parser_reads_unknown_total_and_rejects_invalid_ranges() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 2048-4095/*"),
    );

    assert_eq!(
        content_range_from_headers(&headers),
        Some(HttpContentRange {
            start: 2048,
            end: 4095,
            total: None,
        })
    );
    assert_eq!(content_len_from_content_range(&headers), None);

    headers.insert(
        reqwest::header::CONTENT_RANGE,
        reqwest::header::HeaderValue::from_static("bytes 4095-2048/8192"),
    );
    assert_eq!(content_range_from_headers(&headers), None);
}

#[test]
fn playback_scheduler_reports_ready_for_past_frames() {
    let mut scheduler = PlaybackScheduler::new(1_000_000_000);
    let control = FfmpegControl::new(PlaybackSessionId::default());

    assert_eq!(
        scheduler.wait_until(500_000_000, &control),
        WaitStatus::Ready
    );
}

#[test]
fn playback_scheduler_holds_target_while_paused() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let waiting_control = Arc::clone(&control);
    let (done_tx, done_rx) = mpsc::channel();

    let handle = thread::spawn(move || {
        let mut scheduler = PlaybackScheduler::new(0);
        let status = scheduler.wait_until(40_000_000, &waiting_control);
        done_tx
            .send(status)
            .expect("scheduler result receiver open");
    });

    thread::sleep(Duration::from_millis(10));
    control.set_user_paused(true);
    thread::sleep(Duration::from_millis(70));
    assert!(done_rx.try_recv().is_err());

    control.set_user_paused(false);
    assert_eq!(
        done_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("scheduler should resume after unpause"),
        WaitStatus::Ready
    );
    handle.join().expect("scheduler thread should finish");
}

#[test]
fn annex_b_probe_detects_three_and_four_byte_start_codes() {
    assert!(has_annex_b_start_code(&[9, 0, 0, 1, 1]));
    assert!(has_annex_b_start_code(&[9, 0, 0, 0, 1, 1]));
    assert!(!has_annex_b_start_code(&[0, 0, 2, 1]));
}

#[test]
fn ffmpeg_raw_video_format_maps_supported_yuv_formats() {
    assert_eq!(
        ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int),
        Some(RawVideoFormat::P010Le)
    );
    assert_eq!(
        ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE as c_int),
        Some(RawVideoFormat::I42010Le)
    );
    assert_eq!(
        ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int),
        Some(RawVideoFormat::Nv12)
    );
    assert_eq!(
        ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int),
        Some(RawVideoFormat::I420)
    );
    assert_eq!(
        ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int),
        None
    );
}
