use super::avio::HttpCacheRangeKind;
use super::worker::{PendingSeek, PendingTrackSelection};
use super::*;
use playback_loop::{
    DecodedVideoFrameStartAction, VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VideoDecodeRecovery,
    decoded_video_frame_start_action, initial_probe_profile, playback_read_finished,
    rebase_subtitle_cues_to_timeline_origin, subtitle_cue_timeline_nsecs,
    subtitle_timestamp_to_timeline_nsecs, trim_overlapping_subtitle_cues_at,
    video_decode_error_is_recoverable,
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
fn late_video_drop_keeps_frame_when_video_queue_is_empty() {
    let empty_queue = VecDeque::new();
    let queued_frame = test_queued_video_frame(1_200_000_000);
    let queued = VecDeque::from([queued_frame]);

    assert!(!should_drop_late_queued_video_frame(
        1_000_000_000,
        16_000_000,
        1_091_000_000,
        &empty_queue
    ));
    assert!(should_drop_late_queued_video_frame(
        1_000_000_000,
        16_000_000,
        1_091_000_000,
        &queued
    ));
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
    assert!(!video_decode_error_is_recoverable(
        "FFmpeg 发送解码包失败：Cannot allocate memory"
    ));
    assert!(!video_decode_error_is_recoverable(
        "FFmpeg 创建视频色彩转换器失败"
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
        duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
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
    shared.control.set_paused(true);
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
fn http_ring_cache_probe_read_does_not_restart_trimmed_range() {
    let mut state = HttpRingCacheState::new(100);
    state.append_at(100, b"abcdef");
    state.set_reader_offset(104);
    state.trim_to_capacity(2);
    let cache = HttpRingCache::from_state_for_test(state);
    let mut output = [0; 2];

    assert!(matches!(
        cache.read_cached_at(100, &mut output),
        CacheReadResult::Interrupted
    ));
    assert!(!cache.has_restart_request_for_test());
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
        Some(HttpStreamBufferProgress {
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
            .map(HttpStreamBufferProgress::from),
        Some(HttpStreamBufferProgress {
            start_fraction: 0.1,
            end_fraction: 0.106,
        })
    );
}

#[test]
fn http_ring_cache_state_playback_progress_ignores_retained_range_after_playback_restart() {
    let mut state = HttpRingCacheState::new(100).with_content_len_hint(Some(1_000));
    state.append_at(100, b"abcdef");

    state.restart_at_with_kind(990, HttpCacheRangeKind::TailMetadataProbe);
    state.append_at(990, b"tail");
    state.restart_at_with_kind(200, HttpCacheRangeKind::Playback);
    state.append_at(200, b"ghij");

    assert_eq!(
        state
            .playback_buffer_range()
            .map(HttpStreamBufferProgress::from),
        Some(HttpStreamBufferProgress {
            start_fraction: 0.2,
            end_fraction: 0.204,
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
