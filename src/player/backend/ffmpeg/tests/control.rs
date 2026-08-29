use super::*;

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

    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Silence
    );
    assert!(control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing));
    assert!(!control.is_paused());
    assert!(!control.is_output_rebuffer_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );

    assert!(control.set_output_rebuffer_paused(true));
    assert!(!control.is_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Silence
    );
    assert!(control.is_output_rebuffer_paused());
    assert!(!control.is_user_paused());
    assert!(!control.is_cache_paused());

    assert!(control.set_output_rebuffer_paused(false));
    assert!(!control.is_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn ffmpeg_control_clears_output_rebuffer_pause_on_seek_generation() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    control.set_output_rebuffer_paused(true);

    let generation = control.request_seek();

    assert_eq!(generation, 1);
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );
    assert!(!control.is_output_rebuffer_paused());
    assert!(control.is_seek_audio_paused());
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Silence
    );

    control.finish_seek(generation);
    assert!(control.finish_seek_audio_pause());
    assert!(control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing));
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn ffmpeg_control_does_not_release_seek_silence_for_superseded_generation() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let first = control.request_seek();
    let second = control.request_seek();

    control.finish_seek(first);
    assert!(!control.finish_seek_audio_pause());
    assert!(control.is_seek_audio_paused());
    assert!(!control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Syncing
    );

    control.finish_seek(second);
    assert!(control.finish_seek_audio_pause());
    assert!(!control.is_seek_audio_paused());
}

#[test]
fn ffmpeg_control_uses_one_state_word_for_lifecycle_and_pause_reasons() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    control.set_user_paused(true);
    control.set_cache_paused(true);
    control.set_output_rebuffer_paused(true);

    let state = control.audio_output_control_snapshot();
    assert_eq!(state.lifecycle(), AudioOutputLifecycle::Playing);
    assert!(state.paused_by_user());
    assert!(state.paused_by_cache());
    assert!(state.paused_by_rebuffer());
    assert!(!state.paused_by_seek_transition());
    assert_eq!(state.decision(), AudioOutputDecision::Silence);

    control.set_user_paused(false);
    control.set_cache_paused(false);
    control.set_output_rebuffer_paused(false);
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn audio_output_lifecycle_derives_one_callback_decision() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    for (lifecycle, expected) in [
        (AudioOutputLifecycle::Syncing, AudioOutputDecision::Silence),
        (AudioOutputLifecycle::Ready, AudioOutputDecision::Silence),
        (AudioOutputLifecycle::Playing, AudioOutputDecision::Consume),
        (AudioOutputLifecycle::Draining, AudioOutputDecision::Consume),
    ] {
        control.set_audio_output_lifecycle(lifecycle);
        let callback_state = control.audio_output_control_snapshot();
        assert_eq!(callback_state.lifecycle(), lifecycle);
        assert_eq!(callback_state.decision(), expected);
    }
}

#[test]
fn nine_coalesced_seeks_leave_only_the_latest_transition_to_release() {
    let control = FfmpegControl::new(PlaybackSessionId::default());
    let mut generations = Vec::new();
    for _ in 0..9 {
        generations.push(control.request_seek());
    }
    for generation in generations.iter().copied().take(8) {
        control.finish_seek(generation);
        assert!(!control.finish_seek_audio_pause());
        assert!(control.is_seek_audio_paused());
    }

    control.finish_seek(*generations.last().expect("latest seek generation"));
    assert!(control.finish_seek_audio_pause());
    assert!(control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing));
    assert_eq!(
        control.audio_output_lifecycle(),
        AudioOutputLifecycle::Playing
    );
    assert_eq!(
        control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Consume
    );
}

#[test]
fn ninth_rapid_seek_advances_audio_clock_and_video_presentations() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    let shared = AudioShared::new(
        9_600,
        48_000,
        FALLBACK_AUDIO_OUTPUT_CHANNELS,
        Arc::clone(&control),
    );
    let mut latest_generation = None;
    for _ in 0..9 {
        let generation = control.request_seek();
        control.finish_seek(generation);
        latest_generation = Some(generation);
    }
    let latest_generation = latest_generation.expect("nine seek generations");

    let target_nsecs = 846_233_000_000;
    shared.reset_clock(target_nsecs);
    shared.activate_for_test();
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
    assert!(control.finish_seek_audio_pause());
    assert_eq!(
        shared
            .buffer
            .lock()
            .expect("audio output buffer poisoned")
            .push_slice(&vec![0.0; 9_600]),
        9_600
    );
    shared.set_queued_end_timeline_nsecs(target_nsecs + 100_000_000);
    assert!(control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing));

    let mut queued_video_frames =
        test_queued_video_frames_with_duration(target_nsecs, 5, 10_000_000);
    let mut previous_played_nsecs = shared.played_timeline_nsecs();
    let mut presented_video_frames = 0usize;
    for _ in 0..3 {
        let mut callback_output = vec![0.0f32; 960];
        fill_audio_output(&mut callback_output, &shared);
        let played_nsecs = shared.played_timeline_nsecs();
        assert!(played_nsecs > previous_played_nsecs);
        if pop_audio_clocked_video_frame(&mut queued_video_frames, played_nsecs).is_some() {
            presented_video_frames = presented_video_frames.saturating_add(1);
        }
        previous_played_nsecs = played_nsecs;
    }

    assert_eq!(latest_generation, 9);
    assert_eq!(presented_video_frames, 3);
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
fn drain_playback_commands_coalesces_continuous_seeks_to_latest_generation() {
    let control = FfmpegControl::new(PlaybackSessionId(1));
    let (command_tx, command_rx) = mpsc::channel();

    for position_seconds in 70..80 {
        let generation = control.request_seek();
        command_tx
            .send(FfmpegCommand::Seek {
                session_id: PlaybackSessionId(1),
                position_seconds: f64::from(position_seconds),
                mode: PlaybackSeekMode::Fast,
                generation,
                queued_at: Instant::now(),
            })
            .unwrap();
    }

    let started_at = Instant::now();
    let drained = drain_playback_commands(&command_rx, &control);
    let pending_seek = drained.pending_seek.expect("latest seek is retained");

    assert!(started_at.elapsed() < Duration::from_millis(20));
    assert_eq!(pending_seek.generation, control.seek_generation());
    assert_eq!(pending_seek.position_seconds, 79.0);
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
    let first_queued_at = Instant::now();
    let second_queued_at = Instant::now();

    tx.send(FfmpegCommand::Pause {
        session_id: PlaybackSessionId(2),
    })
    .unwrap();
    tx.send(FfmpegCommand::Seek {
        session_id: PlaybackSessionId(3),
        position_seconds: 12.0,
        mode: PlaybackSeekMode::Precise,
        generation: first_generation,
        queued_at: first_queued_at,
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
        queued_at: second_queued_at,
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
            queued_at: second_queued_at,
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
        queued_at: Instant::now(),
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
