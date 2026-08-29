use super::*;

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
fn timestamp_mapper_preserves_authoritative_decoder_replay_pts() {
    let mut mapper = TimestampMapper::new(Some(0), 339_720_000_000, Some(40_000_000));
    let time_base = ffi::AVRational { num: 1, den: 1_000 };

    assert_eq!(
        mapper.map(381_720, time_base).timeline_nsecs,
        381_720_000_000
    );
    assert_eq!(
        mapper.map_authoritative(372_840, time_base).timeline_nsecs,
        372_840_000_000,
        "verified replay must not synthesize an old safe-anchor PTS after the pre-flush high-water"
    );
    assert_eq!(
        mapper.map_authoritative(381_760, time_base).timeline_nsecs,
        381_760_000_000
    );
    assert_eq!(
        mapper.map(381_800, time_base).timeline_nsecs,
        381_800_000_000
    );
}

#[test]
fn timestamp_mapper_keeps_aac_millisecond_timestamps_sample_contiguous() {
    let mut mapper = TimestampMapper::new(Some(0), 0, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let aac_frame_duration_nsecs = 23_219_954;

    assert_eq!(
        mapper.map_contiguous(
            0,
            time_base,
            aac_frame_duration_nsecs,
            PENDING_AUDIO_CONTINUITY_TOLERANCE
        ),
        MappedTimestamp {
            timeline_nsecs: 0,
            sink_nsecs: 0,
        }
    );
    assert_eq!(
        mapper.map_contiguous(
            23,
            time_base,
            aac_frame_duration_nsecs,
            PENDING_AUDIO_CONTINUITY_TOLERANCE
        ),
        MappedTimestamp {
            timeline_nsecs: 23_219_954,
            sink_nsecs: 23_219_954,
        }
    );
    assert_eq!(
        mapper.map_contiguous(
            46,
            time_base,
            aac_frame_duration_nsecs,
            PENDING_AUDIO_CONTINUITY_TOLERANCE
        ),
        MappedTimestamp {
            timeline_nsecs: 46_439_908,
            sink_nsecs: 46_439_908,
        }
    );
}

#[test]
fn timestamp_mapper_keeps_large_audio_timestamp_gap() {
    let mut mapper = TimestampMapper::new(Some(0), 0, None);
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let aac_frame_duration_nsecs = 23_219_954;

    mapper.map_contiguous(
        0,
        time_base,
        aac_frame_duration_nsecs,
        PENDING_AUDIO_CONTINUITY_TOLERANCE,
    );

    assert_eq!(
        mapper.map_contiguous(
            100,
            time_base,
            aac_frame_duration_nsecs,
            PENDING_AUDIO_CONTINUITY_TOLERANCE
        ),
        MappedTimestamp {
            timeline_nsecs: 100_000_000,
            sink_nsecs: 100_000_000,
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

#[test]
fn queued_video_duration_uses_frame_interval_coverage_without_counting_gaps() {
    let mut queue = VecDeque::new();
    assert_eq!(queued_video_duration(&queue), Duration::ZERO);

    queue.push_back(test_queued_video_frame(1_000_000_000));
    assert_eq!(
        queued_video_duration(&queue),
        Duration::from_nanos(DEFAULT_VIDEO_FRAME_DURATION_NSECS)
    );

    queue.push_back(test_queued_video_frame(1_180_000_000));
    queue.push_back(test_queued_video_frame(1_300_000_000));

    assert_eq!(
        queued_video_coverage_duration(&queue),
        Duration::from_nanos(DEFAULT_VIDEO_FRAME_DURATION_NSECS * 3)
    );
    assert_eq!(
        queued_video_duration(&queue),
        Duration::from_nanos(DEFAULT_VIDEO_FRAME_DURATION_NSECS * 3)
    );
    assert_eq!(
        queued_video_range_span(&queue),
        Duration::from_nanos(300_000_000 + DEFAULT_VIDEO_FRAME_DURATION_NSECS)
    );
}
