use super::avio::HttpCacheRangeKind;
use super::worker::PendingSeek;
use super::*;
use playback_loop::{
    initial_probe_profile, playback_read_finished, rebase_subtitle_cues_to_timeline_origin,
    subtitle_cue_timeline_nsecs, subtitle_timestamp_to_timeline_nsecs,
    trim_overlapping_subtitle_cues_at,
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
            generation: second_generation,
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
    let mut reporter = BufferedReporter::new(false);
    let session_id = PlaybackSessionId(7);

    reporter.reset_to(0.0, session_id, &tx);
    assert_buffered_event(&rx, session_id, Some(0.0));

    reporter.report_video_timeline_nsecs(1_000_000_000, session_id, &tx);

    assert_buffered_event(&rx, session_id, Some(1.0));
}

#[test]
fn buffered_reporter_reports_first_audio_video_update_after_reset() {
    let (tx, rx) = mpsc::channel();
    let mut reporter = BufferedReporter::new(true);
    let session_id = PlaybackSessionId(8);

    reporter.reset_to(12.0, session_id, &tx);
    assert_buffered_event(&rx, session_id, Some(12.0));

    reporter.report_video_timeline_nsecs(13_000_000_000, session_id, &tx);
    assert!(rx.try_recv().is_err());

    reporter.report_audio_timeline_nsecs(13_000_000_000, session_id, &tx);

    assert_buffered_event(&rx, session_id, Some(13.0));
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
    }
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
    let mut buffer = AudioBuffer::with_capacity(8);
    assert_eq!(buffer.push_slice(&[-1.0, 0.0, 1.0]), 3);
    let shared = AudioShared {
        buffer: Mutex::new(buffer),
        ready: Condvar::new(),
        played_samples: AtomicU64::new(0),
        control: Arc::new(FfmpegControl::new(PlaybackSessionId::default())),
    };
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
}

#[test]
fn fill_audio_output_preserves_buffer_while_paused() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    control.set_paused(true);
    let mut buffer = AudioBuffer::with_capacity(8);
    assert_eq!(buffer.push_slice(&[-1.0, 0.0, 1.0]), 3);
    let shared = AudioShared {
        buffer: Mutex::new(buffer),
        ready: Condvar::new(),
        played_samples: AtomicU64::new(0),
        control,
    };
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
    assert_eq!(http_cache_range_header(0, None), "bytes=0-33554431");
    assert_eq!(http_cache_range_header(128, None), "bytes=128-33554559");
    assert_eq!(
        http_cache_range_header(595_453_649, Some(596_486_439)),
        "bytes=595453649-596486438"
    );
    assert_eq!(
        http_cache_range_header(10_675_366_349, Some(10_675_368_645)),
        "bytes=10675366349-10675368644"
    );
}

#[test]
fn http_cache_range_request_timeout_is_short_for_small_tail_ranges() {
    assert_eq!(
        http_cache_range_request_len(10_675_366_349, Some(10_675_368_645)),
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
        Some(HttpStreamBufferProgress {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_reports_near_tail_playback_range() {
    let mut state = HttpRingCacheState::new(980).with_content_len_hint(Some(1_000));
    state.append_at(980, b"tail");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(HttpStreamBufferProgress {
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
fn http_ring_cache_state_does_not_retain_cached_range_for_non_tail_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000_000_000));
    state.append_at(100, b"abcdef");

    state.restart_at(10_000);

    let mut output = [0; 3];
    assert_eq!(state.copy_available(102, &mut output), None);
}

#[test]
fn http_ring_cache_state_uses_content_length_hint_for_progress() {
    let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(100));

    state.append_at(0, b"abcde");

    assert_eq!(
        state.stream_buffer_progress(),
        Some(HttpStreamBufferProgress {
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
fn http_ring_cache_state_ignores_seek_outside_cached_range_until_read() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");

    state.note_seek_offset(10_000, HttpCacheRangeKind::Playback);
    state.trim_to_capacity(4);

    assert_eq!(state.base_offset, 100);
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
    control.set_paused(true);
    thread::sleep(Duration::from_millis(70));
    assert!(done_rx.try_recv().is_err());

    control.set_paused(false);
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
