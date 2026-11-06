use super::*;

fn subscriber(log: &Arc<PlaybackFileLog>, console: &str) -> impl tracing::Subscriber + Send + Sync {
    let filter = Arc::clone(log);
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(io::sink)
                .with_filter(EnvFilter::new(console)),
        )
        .with(
            PlaybackFileLayer(Arc::clone(log)).with_filter(
                dynamic_filter_fn(move |metadata, _| {
                    is_playback_log_event(metadata) && filter.enabled.load(Ordering::Acquire)
                })
                .with_max_level_hint(tracing::level_filters::LevelFilter::DEBUG),
            ),
        )
}

fn emit(sequence: u64) {
    tracing::debug!(target: "tiny_playback::backend::ffmpeg::audio", sequence, "audio diagnostic");
}

#[test]
fn playback_logging_toggles_live_independently_of_console_and_preserves_recorded_output() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    tracing::subscriber::with_default(subscriber(&log, "off"), || {
        log.begin_playback();
        emit(901);
        assert!(!path.exists(), "disabled logging must not create a file");
        log.set_enabled(true).unwrap();
        emit(902);
        log.set_enabled(false).unwrap();
        emit(903);
        log.set_enabled(true).unwrap();
        emit(904);
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("sequence=902"), "{text}");
    assert!(text.contains("sequence=904"), "{text}");
    assert!(!text.contains("sequence=901"));
    assert!(!text.contains("sequence=903"));
}

#[test]
fn each_playback_replaces_the_previous_recording_but_toggles_keep_the_current_one() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    fs::write(&path, b"previous application run\n").unwrap();
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    tracing::subscriber::with_default(subscriber(&log, "off"), || {
        log.set_enabled(true).unwrap();
        log.begin_playback();
        emit(1001);
        log.begin_playback();
        emit(1002);
        log.set_enabled(false).unwrap();
        emit(1003);
        log.set_enabled(true).unwrap();
        emit(1004);
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("sequence=1002"), "{text}");
    assert!(text.contains("sequence=1004"), "{text}");
    assert!(!text.contains("previous application run"));
    assert!(!text.contains("sequence=1001"));
    assert!(!text.contains("sequence=1003"));
}

#[test]
fn starting_playback_while_logging_is_off_defers_overwrite_until_recording_resumes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    fs::write(&path, b"previous playback\n").unwrap();
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    tracing::subscriber::with_default(subscriber(&log, "off"), || {
        log.set_enabled(true).unwrap();
        log.set_enabled(false).unwrap();
        log.begin_playback();
        emit(1101);
        assert_eq!(fs::read(&path).unwrap(), b"previous playback\n");
        log.set_enabled(true).unwrap();
        emit(1102);
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("sequence=1102"), "{text}");
    assert!(!text.contains("previous playback"));
    assert!(!text.contains("sequence=1101"));
}

#[test]
fn starting_playback_overwrites_the_log_even_without_a_debug_event() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    fs::write(&path, b"previous playback\n").unwrap();
    let log = PlaybackFileLog::new(Some(path.clone()));
    log.set_enabled(true).unwrap();
    log.begin_playback();
    log.shutdown();
    assert_eq!(fs::read(path).unwrap(), b"");
}

#[test]
fn playback_reset_discards_a_full_queue_and_events_still_being_formatted() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    fs::write(&path, b"previous playback exceeds the rotation limit\n").unwrap();
    let epoch = Arc::new(AtomicU64::new(1));
    let (sender, receiver) = mpsc::sync_channel(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let make_writer = NonBlockingLogMakeWriter {
        sender: sender.clone(),
        dropped_events: Arc::clone(&dropped),
        enabled: None,
        epoch: Arc::clone(&epoch),
    };
    {
        let mut event = make_writer.make_writer();
        event.write_all(b"previous queued event\n").unwrap();
    }
    let mut in_flight = make_writer.make_writer();
    in_flight.write_all(b"previous unfinished event\n").unwrap();
    epoch.store(2, Ordering::Release);
    make_writer.wake();
    drop(in_flight);
    assert_eq!(
        dropped.load(Ordering::Acquire),
        0,
        "obsolete events must be discarded before queue admission"
    );
    let writer = RotatingFileWriter::new(path.clone(), 8, 1).unwrap();
    let worker_dropped = Arc::clone(&dropped);
    let worker = thread::spawn(move || {
        run_non_blocking_log_worker(writer, receiver, worker_dropped, 1, epoch)
    });
    sender
        .send(NonBlockingLogCommand::Event {
            epoch: 2,
            bytes: b"current\n".to_vec(),
        })
        .unwrap();
    sender.send(NonBlockingLogCommand::Shutdown).unwrap();
    worker.join().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"current\n");
    assert!(
        !path.with_file_name("playback-debug.log.1").exists(),
        "the old recording must not consume the new playback's rotation budget"
    );
}

#[test]
fn playback_reset_during_field_formatting_cannot_mix_the_previous_event_into_the_new_file() {
    struct ResetOnFormat(Arc<PlaybackFileLog>);
    impl std::fmt::Debug for ResetOnFormat {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.begin_playback();
            f.write_str("obsolete formatted event")
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    log.set_enabled(true).unwrap();
    tracing::subscriber::with_default(subscriber(&log, "off"), || {
        tracing::debug!(target: "tiny_playback::backend", field = ?ResetOnFormat(Arc::clone(&log)));
        emit(1201);
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("sequence=1201"), "{text}");
    assert!(!text.contains("obsolete formatted event"));
}

#[test]
fn playback_file_accepts_debug_and_higher_events_only_from_playback_targets() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    log.set_enabled(true).unwrap();
    tracing::subscriber::with_default(subscriber(&log, "off"), || {
        emit(801);
        tracing::debug!(target: "tiny_playback::render_host::renderer", sequence = 802);
        tracing::debug!(target: "tiny_playback::libplacebo", sequence = 803);
        tracing::debug!(target: "tiny_player::player::page", sequence = 804);
        tracing::debug!(target: "tiny_player::emby", sequence = 805);
        tracing::debug!(target: "tiny_player::home", sequence = 806);
        tracing::debug!(target: "reqwest", sequence = 807);
        tracing::debug!(target: "tiny_playback::backend_extra", sequence = 808);
        tracing::trace!(target: "tiny_playback::backend::ffmpeg", sequence = 809);
        tracing::info!(target: "tiny_playback::backend::ffmpeg", sequence = 810);
        tracing::warn!(target: "tiny_playback::backend::ffmpeg", sequence = 811);
        tracing::error!(target: "tiny_playback::backend::ffmpeg", sequence = 812);
        tracing::warn!(target: "tiny_player::emby", sequence = 813);
        tracing::error!(target: "reqwest", sequence = 814);
        tracing::info!(target: "tiny_playback::backend_extra", sequence = 815);
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    for sequence in 801..=815 {
        assert_eq!(
            text.contains(&format!("sequence={sequence}")),
            sequence <= 804 || (810..=812).contains(&sequence),
            "{text}"
        );
    }
    for (level, sequence) in [("DEBUG", 801), ("INFO", 810), ("WARN", 811), ("ERROR", 812)] {
        let line = text
            .lines()
            .find(|line| line.contains(&format!("sequence={sequence}")))
            .unwrap();
        assert!(line.contains(&format!(" {level} ")), "{line}");
    }
}

#[test]
fn playback_file_preserves_original_messages_errors_paths_and_fields() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    log.set_enabled(true).unwrap();
    #[derive(Debug)]
    enum State {
        Playing,
    }
    const URL: &str = "https://private-user:private-password@media.private.invalid:8920/PrivateMovie.mkv?api_key=private-token";
    let details = "unknown diagnostic detail ".repeat(80);
    tracing::subscriber::with_default(subscriber(&log, "debug"), || {
        let span = tracing::debug_span!(target: "tiny_player::emby", "server request", parent_only = "unrelated parent context");
        let _entered = span.enter();
        let error = io::Error::other(format!("failed to read {URL}"));
        tracing::debug!(target: "tiny_playback::backend::ffmpeg",
            error = %error,
            error_details = ?error,
            url = URL,
            stream_url = ?Some(URL),
            endpoint = "media.private.invalid:8920",
            authorization = "Bearer private-bearer",
            headers = ?[("X-Emby-Token", "private-header-token")],
            path = %Path::new("/home/private-user/PrivateMovie.mkv" ).display(),
            windows_path = r"C:\Users\private-user\PrivateMovie.mkv",
            username = "private-user",
            media_title = "PrivateMovie",
            encoded = "https%3A%2F%2Fmedia.private.invalid%2Fvideo%3Fapi_key%3Dprivate-token",
            nested = ?Some((URL, "private-nested-token")),
            user_id = 90099009900990099_u64,
            api_key = 90088008800880088_u64,
            accessToken = 90077007700770077_u64,
            arbitrary_text = "private-text\nDEBUG forged event",
            destination = "192.168.123.99",
            duration_ms = 125.25,
            frames = ?Some((3, 4)),
            output_state = ?State::Playing,
            codec = "AV_CODEC_ID_HEVC",
            diagnostic_details = %details,
            blocked_on = "decoder_input_empty",
            session_id = 7_u64,
            "error reading {URL} for private-user: private-message-token"
        );
        let diagnostic = format!("backend diagnostic: {error}");
        tracing::debug!(target: "tiny_player::player::page::backend_events",
            code = "FFMPEG_AUDIO_START",
            message = %diagnostic,
            "playback backend diagnostic event"
        );
    });
    log.shutdown();
    let text = fs::read_to_string(path).unwrap();
    for original in [
        "private-",
        "media.private.invalid",
        "PrivateMovie",
        "https",
        "8920",
        "192.168.123.99",
        "90099009900990099",
        "90088008800880088",
        "90077007700770077",
        "forged",
        r"C:\\Users",
        "/home/",
    ] {
        assert!(text.contains(original), "missing {original}: {text}");
    }
    for retained in [
        "duration_ms=125.25",
        "frames=Some((3, 4))",
        "output_state=Playing",
        "blocked_on=\"decoder_input_empty\"",
        "session_id=7",
        "AV_CODEC_ID_HEVC",
        "error reading",
        "failed to read",
        "error_details=Custom",
        "private-message-token",
        "private-header-token",
        "private-nested-token",
        "backend diagnostic:",
        "FFMPEG_AUDIO_START",
        "playback backend diagnostic event",
    ] {
        assert!(text.contains(retained), "missing {retained}: {text}");
    }
    assert!(text.contains(&details));
    assert!(!text.contains("unrelated parent context"));
    assert!(!text.contains("[redacted]"));
}

#[test]
fn disabling_logging_rejects_an_event_formatted_before_the_toggle() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let log = Arc::new(PlaybackFileLog::new(Some(path.clone())));
    log.set_enabled(true).unwrap();
    let make_writer = PlaybackLogMakeWriter(Arc::clone(&log));
    let mut event = make_writer.make_writer();
    event.write_all(b"in flight\n").unwrap();
    log.set_enabled(false).unwrap();
    drop(event);
    log.shutdown();
    assert_eq!(fs::read(path).unwrap(), b"");
}

#[test]
fn playback_log_writer_does_not_wait_for_a_contended_settings_lock() {
    let temp = tempfile::tempdir().unwrap();
    let log = Arc::new(PlaybackFileLog::new(Some(
        temp.path().join("playback-debug.log"),
    )));
    log.set_enabled(true).unwrap();
    let make_writer = PlaybackLogMakeWriter(Arc::clone(&log));
    let _held = log.output.lock().unwrap();
    log.begin_playback();
    assert!(matches!(
        make_writer.make_writer(),
        fmt::writer::EitherWriter::B(_)
    ));
}

#[test]
fn unavailable_log_files_leave_recording_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let blocked = temp.path().join("not-a-directory");
    fs::write(&blocked, b"occupied").unwrap();
    for path in [None, Some(blocked.join("playback-debug.log"))] {
        let log = PlaybackFileLog::new(path);
        assert!(log.set_enabled(true).is_err());
        assert!(!log.enabled.load(Ordering::Acquire));
        assert!(log.output.lock().unwrap().is_none());
        log.set_enabled(false).unwrap();
    }
}

#[test]
fn playback_reset_truncates_the_file_and_restarts_the_rotation_budget() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    fs::write(&path, b"previous playback longer than the limit\n").unwrap();
    let mut writer = RotatingFileWriter::new(path.clone(), 16, 1).unwrap();
    writer.reset().unwrap();
    writer.write_all(b"new\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new\n");
    assert!(!writer.backup_path(1).exists());
    writer.write_all(b"rotated recording\n").unwrap();
    writer.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"rotated recording\n");
    assert_eq!(fs::read(writer.backup_path(1)).unwrap(), b"new\n");
}

#[cfg(unix)]
#[test]
fn new_log_files_are_private_to_the_current_user() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("playback-debug.log");
    let _writer = RotatingFileWriter::new(path.clone(), 1024, 1).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
