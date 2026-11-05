use std::sync::{Arc, atomic::AtomicBool, mpsc};

use super::super::{
    AudioOutput, BufferedReporter, DecodedAudio, PlaybackOutputScheduler, PlaybackOutputState,
    PositionReporter, SubtitlePipeline,
};
use super::test_queued_video_frame;
use crate::backend::ffmpeg::{AudioOutputLifecycle, FfmpegControl};
use crate::render_host::{PlaybackSessionId, VideoOutputQueue};

#[test]
fn disabling_audio_preserves_decoded_video_and_resumes_on_video_clock() {
    use crate::backend::ffmpeg::PlaybackScheduler;

    for paused in [false, true] {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_user_paused(paused);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.mark_first_frame_presented();
        scheduler.set_state(PlaybackOutputState::Playing);
        scheduler.push_decoded_video_for_test(test_queued_video_frame(401_300_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.5; 1920],
                duration_nsecs: 20_000_000,
            },
            401_300_000_000,
            401_320_000_000,
        );
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        control.set_output_rebuffer_paused(true);
        control.set_output_underrun_for_cache_pause(true);
        let before_video = scheduler.scheduled_video_queue.range_nsecs();
        let mut clock = PlaybackScheduler::new(100_000_000_000);

        scheduler.disable_audio(&mut clock, 401_276_000_000, &control);

        assert_eq!(scheduler.scheduled_video_queue.range_nsecs(), before_video);
        assert_eq!(scheduler.scheduled_video_queue.len(), 1);
        assert!(scheduler.pending_start_audio.is_empty());
        assert!(!scheduler.restart_pending());
        assert!(!scheduler.rebuffering());
        assert!(scheduler.video_clock_anchor_valid());
        assert!(clock.ready_for(401_276_000_000));
        assert!(
            !clock.ready_for(410_000_000_000),
            "use presentation time, not decode-ahead time"
        );
        assert_eq!(control.is_user_paused(), paused);
        assert!(!control.is_output_rebuffer_paused());
        assert!(!control.output_underrun_for_cache_pause());
        assert_eq!(control.seek_generation(), 0);
    }
}

#[test]
fn underrun_preserves_logged_partial_prefill_and_appends_across_short_pts_gap() {
    const PLAYED: u64 = 1_301_797_375_331;
    const AUDIO_END: u64 = 1_301_946_695_055;
    const NEXT_AUDIO: u64 = 1_301_997_062_500;
    const VIDEO_START: u64 = 1_301_835_055_556;

    let session_id = PlaybackSessionId(1302);
    let control = Arc::new(FfmpegControl::new(session_id));
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 96_000, 48_000, 2);
    output.reset_clock(PLAYED);
    output.stage_filtered_audio_for_test(vec![0.25; 14_336], PLAYED, AUDIO_END);
    output.activate_current_audio_output(&control);
    assert!(output.transfer_next_queued_frame_for_test().unwrap());
    output.mark_underrun_for_test(PLAYED);
    let epoch = output.audio_epoch();
    let mut callback = vec![0.0; 960];
    output.invoke_callback_for_test(&mut callback);
    assert!(callback.iter().all(|sample| *sample == 0.0));
    assert_eq!(
        output.snapshot().unwrap().shared_payload_nsecs,
        AUDIO_END - PLAYED
    );

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.mark_first_frame_presented();
    for index in 0..36 {
        scheduler
            .push_decoded_video_for_test(test_queued_video_frame(VIDEO_START + index * 40_000_000));
    }
    for index in 0..10 {
        let start = NEXT_AUDIO + index * 20_000_000;
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.5; 1920],
                duration_nsecs: 20_000_000,
            },
            start,
            start + 20_000_000,
        );
    }
    assert!(scheduler.pending_start_audio_can_recover_output(Some(output.snapshot().unwrap())));
    let video_range = scheduler.scheduled_video_queue.range_nsecs();
    let (event_tx, _event_rx) = mpsc::channel();
    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(true);
    let mut position = PositionReporter::default();
    let mut subtitles = SubtitlePipeline::empty_for_test();
    let mut buffered = BufferedReporter::new_with_events(true, false);
    for _ in 0..100 {
        scheduler
            .flush_pending_start_audio_if_ready(
                &output,
                &control,
                session_id,
                &vo_queue,
                &frame_presented,
                &mut position,
                &event_tx,
                &mut subtitles,
                &mut buffered,
            )
            .unwrap();
        while output.snapshot().unwrap().queue_frames > 0 {
            assert!(output.transfer_next_queued_frame_for_test().unwrap());
        }
        if scheduler.pending_start_audio.is_empty() {
            break;
        }
    }
    assert!(scheduler.pending_start_audio.is_empty());
    assert_eq!(output.audio_epoch(), epoch);
    assert_eq!(scheduler.scheduled_video_queue.range_nsecs(), video_range);
    let mut pcm = vec![0.0; 14_336 + 10 * 1920];
    for chunk in pcm.chunks_mut(960) {
        output.invoke_callback_for_test(chunk);
    }
    assert!(pcm[..14_336].iter().all(|sample| *sample == 0.25));
    assert!(pcm[14_336..].iter().all(|sample| *sample == 0.5));
    assert!(!output.underrun_active());
    assert_eq!(
        output.activity_snapshot().unwrap().consumed_callback_count,
        pcm.len().div_ceil(960) as u64
    );
    assert!(output.snapshot().unwrap().played_timeline_nsecs > AUDIO_END);
}

#[test]
fn accelerated_playback_refills_underrun_when_audio_tail_is_in_video_pts_gap() {
    const RATE: f64 = 1.331;
    const PLAYED: u64 = 2_328_983_331_435;
    const AUDIO_START: u64 = 2_329_034_941_270;
    const AUDIO_END: u64 = 2_329_076_923_605;
    const VIDEO_START: u64 = 2_329_035_000_000;
    const PENDING_FRAMES: usize = 1017;

    let session_id = PlaybackSessionId(2329);
    let control = Arc::new(FfmpegControl::new(session_id));
    control.set_playback_rate(RATE);
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 88_200, 44_100, 2);
    output.reset_clock(PLAYED);
    // Reproduce the already-stretched AO tail from the log. Newly decoded
    // pending frames below now retain original PCM until the AO worker.
    output.stage_filtered_audio_for_test(vec![0.25; 2782], AUDIO_START, AUDIO_END);
    output.activate_current_audio_output(&control);
    assert!(output.transfer_next_queued_frame_for_test().unwrap());
    output.mark_underrun_for_test(PLAYED);
    let epoch = output.audio_epoch();

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.mark_first_frame_presented();
    for index in 0..34_u64 {
        let pts = VIDEO_START + ((index * 1001 + 12) / 24) * 1_000_000;
        let mut frame = test_queued_video_frame(pts);
        frame.duration_nsecs = 41_708_333;
        frame.source_duration_nsecs = 41_708_333;
        scheduler.push_decoded_video_for_test(frame);
    }
    let media_offset =
        |index: usize| (index as f64 * 40.0 * 1_000_000_000.0 / 44_100.0).round() as u64;
    for index in 0..PENDING_FRAMES {
        let start = AUDIO_END + media_offset(index);
        let end = AUDIO_END + media_offset(index + 1);
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.25; 80],
                duration_nsecs: end - start,
            },
            start,
            end,
        );
    }

    let initial = output.snapshot().unwrap();
    assert_eq!(initial.played_timeline_nsecs, PLAYED);
    assert_eq!(initial.buffered_until_timeline_nsecs, AUDIO_END);
    assert_eq!(initial.shared_payload_nsecs, 41_982_335);
    assert!(
        scheduler.pending_start_audio_can_recover_output(Some(initial)),
        "continuous cached video must allow audio prefill at a quantized PTS gap"
    );
    let video_range = scheduler.scheduled_video_queue.range_nsecs();
    let (event_tx, _event_rx) = mpsc::channel();
    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(true);
    let mut position = PositionReporter::default();
    let mut subtitles = SubtitlePipeline::empty_for_test();
    let mut buffered = BufferedReporter::new_with_events(true, false);
    let mut callback = [0.0; 1024];
    let mut recovered = false;
    for _ in 0..PENDING_FRAMES {
        scheduler
            .flush_pending_start_audio_if_ready(
                &output,
                &control,
                session_id,
                &vo_queue,
                &frame_presented,
                &mut position,
                &event_tx,
                &mut subtitles,
                &mut buffered,
            )
            .unwrap();
        // A preempted 2ms staging pass may yield before enqueuing anything.
        // Retry under the same bounded loop as ordinary partial prefill.
        while output.snapshot().unwrap().queue_frames > 0 {
            if !output.transfer_next_queued_frame_for_test().unwrap() {
                break;
            }
        }
        assert_eq!(output.audio_epoch(), epoch);
        assert_eq!(scheduler.scheduled_video_queue.range_nsecs(), video_range);
        assert_eq!(scheduler.scheduled_video_queue.len(), 34);
        output.invoke_callback_for_test(&mut callback);
        if !output.underrun_active() {
            assert!(callback.iter().all(|sample| (*sample - 0.25).abs() < 0.001));
            recovered = true;
            break;
        }
        assert!(callback.iter().all(|sample| *sample == 0.0));
        assert_eq!(output.snapshot().unwrap().played_timeline_nsecs, PLAYED);
    }
    assert!(
        recovered,
        "native callback must resume after bounded prefill passes"
    );
    assert!(output.activity_snapshot().unwrap().consumed_callback_count > 0);
    assert!(output.snapshot().unwrap().played_timeline_nsecs > PLAYED);
}

#[test]
fn tempo_underrun_refills_past_video_tail_and_frozen_clock_budget() {
    for (final_rate, reset_before_refill) in [(1.4641, false), (1.0, false), (1.0, true)] {
        let session_id = PlaybackSessionId(7);
        let control = Arc::new(FfmpegControl::new(session_id));
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
        let output = AudioOutput::stopped_for_test(Arc::clone(&control), 88_200, 44_100, 2);
        output.reset_clock(1_000_000_000);
        output.activate_current_audio_output(&control);
        let enqueue = |start, rate| {
            control.set_playback_rate(rate);
            assert!(matches!(
                output
                    .try_push_timed(vec![0.25; 2822], start, start + 32_000_000, &control)
                    .unwrap(),
                crate::backend::ffmpeg::AudioOutputPushResult::Queued
            ));
            while output.transfer_next_queued_frame_for_test().unwrap() {}
        };

        enqueue(1_000_000_000, 1.0);
        output.invoke_callback_for_test(&mut [0.0; 2822]);
        let played = output.snapshot().unwrap().played_timeline_nsecs;
        // Retain a timestamp offset as in the reported frozen-clock snapshot:
        // the accepted audio ends 577 ms ahead, leaving less than one 32 ms
        // frame in the ordinary 600 ms admission budget.
        let audio_start = played + 257_000_000;
        for index in 0..10 {
            enqueue(
                audio_start + index * 32_000_000,
                if index < 6 { 1.4641 } else { final_rate },
            );
        }
        if reset_before_refill {
            // The watchdog discards the filter graph. A plain 1x tail must
            // also recover if it meets the same video/clock admission limits.
            output.reset_clock(played);
            output.activate_current_audio_output(&control);
            for index in 0..6 {
                enqueue(played + 385_000_000 + index * 32_000_000, 1.0);
            }
        }
        output.mark_underrun_for_test(played);
        let initial = output.snapshot().unwrap();
        assert!(initial.shared_payload_nsecs < 250_000_000, "{initial:?}");
        if reset_before_refill {
            assert_eq!(initial.tempo_pending_nsecs, 0);
        } else {
            assert!(initial.total_pending_nsecs > 250_000_000, "{initial:?}");
            assert!(initial.tempo_pending_nsecs > 0);
        }
        assert_eq!(
            initial.total_pending_nsecs - initial.driver_delay_nsecs,
            initial.shared_payload_nsecs + initial.tempo_pending_nsecs
        );
        assert_eq!(initial.buffered_until_timeline_nsecs, played + 577_000_000);
        let epoch = output.audio_epoch();

        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Playing);
        scheduler.mark_first_frame_presented();
        for index in 0..12 {
            let mut frame = test_queued_video_frame(played + index * 40_000_000);
            frame.duration_nsecs = 40_000_000;
            frame.source_duration_nsecs = 40_000_000;
            scheduler.push_decoded_video_for_test(frame);
        }
        let video_range = scheduler.scheduled_video_queue.range_nsecs();
        assert!(initial.buffered_until_timeline_nsecs > video_range.unwrap().1);
        for index in 0..40 {
            let start = initial.buffered_until_timeline_nsecs + index * 32_000_000;
            scheduler.push_pending_start_audio_for_test(
                DecodedAudio {
                    samples: vec![0.25; 2822],
                    duration_nsecs: 32_000_000,
                },
                start,
                start + 32_000_000,
            );
        }
        assert!(
            scheduler.pending_start_audio_can_recover_output(Some(initial)),
            "tempo latency must not leave prefill dependent on the frozen video clock"
        );

        let (event_tx, _event_rx) = mpsc::channel();
        let vo_queue = VideoOutputQueue::default();
        vo_queue.begin_session(session_id);
        let frame_presented = AtomicBool::new(true);
        let mut position = PositionReporter::default();
        let mut subtitles = SubtitlePipeline::empty_for_test();
        let mut buffered = BufferedReporter::new_with_events(true, false);
        let mut callback = [0.0; 1024];
        for _ in 0..100 {
            scheduler
                .flush_pending_start_audio_if_ready(
                    &output,
                    &control,
                    session_id,
                    &vo_queue,
                    &frame_presented,
                    &mut position,
                    &event_tx,
                    &mut subtitles,
                    &mut buffered,
                )
                .unwrap();
            while output.transfer_next_queued_frame_for_test().unwrap() {}
            output.invoke_callback_for_test(&mut callback);
            if !output.underrun_active() {
                break;
            }
            assert!(callback.iter().all(|sample| *sample == 0.0));
            assert_eq!(output.snapshot().unwrap().played_timeline_nsecs, played);
        }
        assert!(!output.underrun_active(), "rate {final_rate}");
        assert!(callback.iter().all(|sample| (*sample - 0.25).abs() < 0.001));
        assert_eq!(
            output.audio_epoch(),
            epoch,
            "prefill must preserve tempo history"
        );
        assert_eq!(scheduler.scheduled_video_queue.range_nsecs(), video_range);
        assert!(output.snapshot().unwrap().played_timeline_nsecs > played);
    }
}

#[test]
fn tempo_underrun_recovers_from_one_frame_smaller_than_pcm_deficit() {
    const FRAME_NSECS: u64 = 32_000_000;
    const PREFILL_NSECS: u64 = 250_000_000;

    let session_id = PlaybackSessionId(7);
    let control = Arc::new(FfmpegControl::new(session_id));
    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
    let output = AudioOutput::stopped_for_test(Arc::clone(&control), 88_200, 44_100, 2);
    output.reset_clock(1_000_000_000);
    output.activate_current_audio_output(&control);
    let enqueue = |start| {
        assert!(matches!(
            output
                .try_push_timed(vec![0.25; 2822], start, start + FRAME_NSECS, &control)
                .unwrap(),
            crate::backend::ffmpeg::AudioOutputPushResult::Queued
        ));
        while output.transfer_next_queued_frame_for_test().unwrap() {}
    };
    enqueue(1_000_000_000);
    output.invoke_callback_for_test(&mut [0.0; 2822]);
    let played = output.snapshot().unwrap().played_timeline_nsecs;

    control.set_playback_rate(3.0);
    for index in 0..12 {
        enqueue(played + 193_000_000 + index * FRAME_NSECS);
    }
    output.mark_underrun_for_test(played);
    let initial = output.snapshot().unwrap();
    // atempo retains about 192 ms and has produced about 192 ms of PCM.
    // The next 32 ms frame releases a burst large enough to finish prefill.
    assert!(initial.tempo_pending_nsecs > 0);
    assert!(initial.shared_payload_nsecs + FRAME_NSECS < PREFILL_NSECS);
    assert_eq!(initial.buffered_until_timeline_nsecs, played + 577_000_000);
    let epoch = output.audio_epoch();

    let mut scheduler = PlaybackOutputScheduler::new();
    scheduler.set_state(PlaybackOutputState::Playing);
    scheduler.mark_first_frame_presented();
    for index in 0..15 {
        let mut frame = test_queued_video_frame(played + index * 40_000_000);
        frame.duration_nsecs = 40_000_000;
        frame.source_duration_nsecs = 40_000_000;
        scheduler.push_decoded_video_for_test(frame);
    }
    let video_range = scheduler.scheduled_video_queue.range_nsecs();
    scheduler.push_pending_start_audio_for_test(
        DecodedAudio {
            samples: vec![0.25; 2822],
            duration_nsecs: FRAME_NSECS,
        },
        initial.buffered_until_timeline_nsecs,
        initial.buffered_until_timeline_nsecs + FRAME_NSECS,
    );
    assert!(scheduler.pending_start_audio_can_recover_output(Some(initial)));

    let (event_tx, _event_rx) = mpsc::channel();
    let vo_queue = VideoOutputQueue::default();
    vo_queue.begin_session(session_id);
    let frame_presented = AtomicBool::new(true);
    let mut position = PositionReporter::default();
    let mut subtitles = SubtitlePipeline::empty_for_test();
    let mut buffered = BufferedReporter::new_with_events(true, false);
    let mut callback = [0.0; 1024];
    output.invoke_callback_for_test(&mut callback);
    assert!(output.underrun_active());
    assert!(callback.iter().all(|sample| *sample == 0.0));
    for _ in 0..100 {
        scheduler
            .flush_pending_start_audio_if_ready(
                &output,
                &control,
                session_id,
                &vo_queue,
                &frame_presented,
                &mut position,
                &event_tx,
                &mut subtitles,
                &mut buffered,
            )
            .unwrap();
        while output.transfer_next_queued_frame_for_test().unwrap() {}
        let ready_payload_nsecs = output.snapshot().unwrap().shared_payload_nsecs;
        output.invoke_callback_for_test(&mut callback);
        if !output.underrun_active() {
            assert!(ready_payload_nsecs >= PREFILL_NSECS);
            break;
        }
        assert!(callback.iter().all(|sample| *sample == 0.0));
        assert_eq!(output.snapshot().unwrap().played_timeline_nsecs, played);
    }
    assert!(!output.underrun_active());
    assert!(scheduler.pending_start_audio.is_empty());
    assert!(callback.iter().all(|sample| (*sample - 0.25).abs() < 0.001));
    assert_eq!(output.audio_epoch(), epoch);
    assert_eq!(scheduler.scheduled_video_queue.range_nsecs(), video_range);
    assert!(output.snapshot().unwrap().played_timeline_nsecs > played);
}
