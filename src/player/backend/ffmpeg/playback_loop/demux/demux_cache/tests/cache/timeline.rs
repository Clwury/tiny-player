use super::*;

#[test]
fn demux_packet_timeline_drops_unselected_stream_packets() {
    let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
    let audio_stream = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        Some(audio_stream),
        None,
        0.0,
        PlaybackSessionId(1),
    );
    let (event_tx, _event_rx) = mpsc::channel();

    let packet = demux_packet_for_stream(1);
    let cached = timeline
        .cache_packet(&packet, &event_tx)
        .expect("unselected packet is accepted as droppable");

    assert!(cached.is_none());
    assert!(!timeline.should_cache_stream(1));
    assert!(timeline.should_cache_stream(0));
    assert!(timeline.should_cache_stream(2));
}

#[test]
fn demux_packet_timeline_switches_selected_audio_stream() {
    let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
    let old_audio_stream = stream_info_for_test(2, ffi::AVCodecID::AV_CODEC_ID_EAC3);
    let new_audio_stream = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_AAC);
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        Some(old_audio_stream),
        None,
        0.0,
        PlaybackSessionId(1),
    );
    let (event_tx, _event_rx) = mpsc::channel();

    timeline.set_selected_streams(DemuxSelectedStreams {
        audio_stream: Some(new_audio_stream),
        subtitle_stream: None,
    });

    let old_packet = demux_packet_for_stream(2);
    let new_packet = demux_packet_for_stream(1);
    assert!(
        timeline
            .cache_packet(&old_packet, &event_tx)
            .expect("old stream packet can be dropped")
            .is_none()
    );
    assert!(
        timeline
            .cache_packet(&new_packet, &event_tx)
            .expect("new stream packet can be cached")
            .is_some()
    );
    assert!(!timeline.should_cache_stream(2));
    assert!(timeline.should_cache_stream(1));
}

#[test]
fn demux_packet_timeline_marks_truehd_major_sync_as_audio_recovery_point() {
    let video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
    let audio_stream = stream_info_for_test(1, ffi::AVCodecID::AV_CODEC_ID_TRUEHD);
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        Some(audio_stream),
        None,
        0.0,
        PlaybackSessionId(1),
    );
    let (event_tx, _event_rx) = mpsc::channel();
    let packet = demux_packet_with_data_for_stream(1, &[0xf8, 0x72, 0x6f, 0xba]);

    let cached = timeline
        .cache_packet(&packet, &event_tx)
        .expect("TrueHD packet caches")
        .expect("selected TrueHD packet is retained");

    assert!(cached.recovery_point);
    assert!(!cached.timeline_anchor);
}

#[test]
fn demux_packet_timeline_absolute_end_matches_next_packet_start() {
    let mut video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_MPEG4);
    video_stream.time_base = ffi::AVRational { num: 1, den: 30 };
    let mut timeline =
        DemuxPacketTimeline::new(video_stream, None, None, 0.0, PlaybackSessionId(1));
    let (event_tx, _event_rx) = mpsc::channel();
    let mut first = demux_packet_for_stream(0);
    let mut second = demux_packet_for_stream(0);
    unsafe {
        (*first.as_mut_ptr()).pts = 1;
        (*first.as_mut_ptr()).dts = 1;
        (*first.as_mut_ptr()).duration = 1;
        (*second.as_mut_ptr()).pts = 2;
        (*second.as_mut_ptr()).dts = 2;
        (*second.as_mut_ptr()).duration = 1;
    }

    let first = timeline
        .cache_packet(&first, &event_tx)
        .expect("first packet maps")
        .expect("video packet is selected");
    let second = timeline
        .cache_packet(&second, &event_tx)
        .expect("second packet maps")
        .expect("video packet is selected");

    assert_eq!(first.end_nsecs, second.start_nsecs);
    assert_eq!(first.end_nsecs, Some(33_333_334));
}

#[test]
fn demux_packet_timeline_seek_timestamps_preserve_pts_reordering_and_missing_values() {
    let mut video_stream = stream_info_for_test(0, ffi::AVCodecID::AV_CODEC_ID_HEVC);
    video_stream.time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut timeline =
        DemuxPacketTimeline::new(video_stream, None, None, 10.0, PlaybackSessionId(1));
    let (event_tx, _event_rx) = mpsc::channel();
    let mut first = demux_packet_for_stream(0);
    let mut reordered = demux_packet_for_stream(0);
    let mut dts_only = demux_packet_for_stream(0);
    let missing = demux_packet_for_stream(0);
    unsafe {
        (*first.as_mut_ptr()).pts = 100;
        (*first.as_mut_ptr()).dts = 90;
        (*reordered.as_mut_ptr()).pts = 70;
        (*reordered.as_mut_ptr()).dts = 100;
        (*dts_only.as_mut_ptr()).dts = 110;
    }

    let first = timeline
        .cache_packet(&first, &event_tx)
        .expect("first packet maps")
        .expect("video packet is selected");
    let reordered = timeline
        .cache_packet(&reordered, &event_tx)
        .expect("reordered packet maps")
        .expect("video packet is selected");
    let dts_only = timeline
        .cache_packet(&dts_only, &event_tx)
        .expect("DTS-only packet maps")
        .expect("video packet is selected");
    let missing = timeline
        .cache_packet(&missing, &event_tx)
        .expect("missing-timestamp packet remains cacheable")
        .expect("video packet is selected");

    assert_eq!(first.seek_timestamp_nsecs, Some(10_000_000_000));
    assert_eq!(reordered.seek_timestamp_nsecs, Some(9_970_000_000));
    assert!(reordered.start_nsecs > first.start_nsecs);
    assert!(reordered.seek_timestamp_nsecs < first.seek_timestamp_nsecs);
    assert_eq!(dts_only.seek_timestamp_nsecs, Some(10_010_000_000));
    assert_eq!(missing.seek_timestamp_nsecs, None);
    assert!(missing.start_nsecs.is_some());
}
