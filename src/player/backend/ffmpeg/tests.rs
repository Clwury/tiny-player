use super::worker::PendingSeek;
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
            pixels: FramePixels::Bgra8(vec![0, 0, 0, 255]),
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
    let shared = AudioShared {
        buffer: Mutex::new(AudioBuffer {
            samples: [-1.0, 0.0, 1.0].into_iter().collect(),
            max_samples: 8,
        }),
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
            .samples
            .is_empty()
    );
    assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
}

#[test]
fn fill_audio_output_preserves_buffer_while_paused() {
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId::default()));
    control.set_paused(true);
    let shared = AudioShared {
        buffer: Mutex::new(AudioBuffer {
            samples: [-1.0, 0.0, 1.0].into_iter().collect(),
            max_samples: 8,
        }),
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
            .samples
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

    state.note_seek_offset(10_000);
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
