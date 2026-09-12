use super::*;

#[test]
fn video_only_scheduler_scales_time_and_preserves_rate_across_seek_and_pause() {
    for rate in [0.25, 0.5, 1.1, 2.0, 4.0] {
        let mut scheduler = PlaybackScheduler::new(0);
        scheduler.set_playback_rate(rate);
        scheduler.reset(10_000_000_000);
        scheduler.set_elapsed_for_test(Duration::from_secs(2));
        scheduler.delay_by(Duration::from_secs(1));
        let expected = 10_000_000_000 + (1_000_000_000.0 * rate) as u64;
        assert!(scheduler.current_timeline_nsecs().abs_diff(expected) < 20_000_000);
        assert!(scheduler.ready_for(expected - 20_000_000));
        assert!(!scheduler.ready_for(expected + 100_000_000));
        let before = scheduler.current_timeline_nsecs();
        scheduler.set_playback_rate(1.0);
        assert!(scheduler.current_timeline_nsecs().abs_diff(before) < 20_000_000);
    }
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
fn rebuffer_pause_delay_keeps_scheduler_from_fast_forwarding_one_second() {
    let mut scheduler = PlaybackScheduler::new(5_000_000_000);
    scheduler.set_elapsed_for_test(Duration::from_millis(1_200));
    let before_pause_adjustment = scheduler.current_timeline_nsecs();

    scheduler.delay_by(Duration::from_secs(1));
    let after_pause_adjustment = scheduler.current_timeline_nsecs();

    assert!(before_pause_adjustment >= 6_190_000_000);
    assert!(after_pause_adjustment < 5_300_000_000);
    assert!(after_pause_adjustment.saturating_add(900_000_000) < before_pause_adjustment);
}

#[test]
fn audio_gap_scheduler_reanchor_cannot_drain_1_6_seconds_in_ten_milliseconds() {
    let mut scheduler = PlaybackScheduler::new(0);
    scheduler.reset(10_000_000_000);

    assert!(!scheduler.ready_for(11_600_000_000));
    thread::sleep(Duration::from_millis(10));
    assert!(!scheduler.ready_for(11_600_000_000));
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
