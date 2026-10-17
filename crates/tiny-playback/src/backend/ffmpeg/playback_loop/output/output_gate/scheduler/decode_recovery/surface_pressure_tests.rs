use super::*;
use crate::{
    backend::ffmpeg::VideoFrameConvertContext,
    render_host::{DecodedFrame, FramePixels, FramePts, RenderSize},
};
use std::sync::atomic::AtomicUsize;

#[cfg(target_os = "linux")]
#[path = "surface_pressure_gpu_tests.rs"]
mod gpu;

fn frame(timeline_nsecs: u64, duration_nsecs: u64) -> QueuedVideoFrame {
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
        duration_nsecs,
        source_duration_nsecs: duration_nsecs,
    }
}

#[test]
fn hardware_surface_pressure_commits_short_continuous_recovery_without_waiting_for_more_frames() {
    let session_id = PlaybackSessionId(1);
    let control = FfmpegControl::new(session_id);
    // The logged 25 fps stream deadlocked at 8 frames / 320 ms. Higher frame
    // rates also hit the shared surface limit before the 500 ms waterline.
    for frame_duration_nsecs in [40_000_000, 20_000_000, 16_666_667] {
        for source in [
            DecodeRecoverySource::FlushReplay,
            DecodeRecoverySource::HardwareReopenReplay,
            DecodeRecoverySource::CachedSafeIdrRebuild,
        ] {
            let target_nsecs = 566_680_000_000;
            let mut output = PlaybackOutputScheduler::new();
            output.begin_decode_recovery(3, target_nsecs, source, &control, session_id);
            output.mark_decode_recovery_replaying(3);
            assert!(output.maybe_enter_decode_recovery_barrier(
                Instant::now(),
                target_nsecs,
                &control,
                session_id,
            ));
            let counter = Arc::new(AtomicUsize::new(0));
            let mut convert_context = VideoFrameConvertContext::new_for_test(RenderSize {
                width: 1,
                height: 1,
            });
            convert_context.set_mapped_surface_counter_for_test(Arc::clone(&counter));
            for index in 0..8 {
                assert!(output.stage_decode_recovery_frame(
                    frame(
                        target_nsecs + index * frame_duration_nsecs,
                        frame_duration_nsecs
                    ),
                    3,
                    session_id,
                ));
                counter.store(index as usize + 1, std::sync::atomic::Ordering::Release);
                if index < 7 {
                    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(
                        convert_context.hardware_surface_pressure(),
                        session_id,
                    ));
                }
            }
            assert!(convert_context.hardware_surface_pressure());
            assert_eq!(
                output.decode_recovery_phase(),
                Some(DecodeRecoveryPhase::Replaying)
            );
            assert!(output.scheduled_video_queue.is_empty());
            assert!(output.maybe_buffer_decode_recovery_for_surface_pressure(
                convert_context.hardware_surface_pressure(),
                session_id,
            ));
            assert!(output.decode_recovery_video_admission_blocked());
            assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));

            let mut scheduler = PlaybackScheduler::new(target_nsecs);
            let mut current_start_position_nsecs = target_nsecs;
            assert!(!output.commit_decode_recovery_if_buffered(
                &control,
                &mut scheduler,
                None,
                &mut current_start_position_nsecs,
                false,
                session_id,
            ));
            assert!(control.is_output_rebuffer_paused());
            assert!(output.commit_decode_recovery_if_buffered(
                &control,
                &mut scheduler,
                None,
                &mut current_start_position_nsecs,
                true,
                session_id,
            ));
            assert!(!control.is_output_rebuffer_paused());
            assert!(!output.decode_recovery_video_admission_blocked());
            assert_eq!(
                output.decode_recovery_phase(),
                Some(DecodeRecoveryPhase::Committed)
            );
            assert_eq!(output.recovery_staging_frames(), 0);
            assert_eq!(output.scheduled_video_queue.len(), 8);
            assert_eq!(
                output.scheduled_video_queue.range_nsecs(),
                Some((target_nsecs, target_nsecs + 8 * frame_duration_nsecs)),
            );
            assert_eq!(current_start_position_nsecs, target_nsecs);
            output
                .check_decode_recovery_deadline(
                    Instant::now() + DECODE_RECOVERY_MAX_WALL_TIME,
                    &control,
                    session_id,
                )
                .unwrap();
        }
    }
}

#[test]
fn hardware_surface_pressure_does_not_commit_empty_discontinuous_or_failed_recovery() {
    let session_id = PlaybackSessionId(2);
    let control = FfmpegControl::new(session_id);
    let target_nsecs = 1_000_000_000;
    let mut output = PlaybackOutputScheduler::new();
    output.begin_decode_recovery(
        2,
        target_nsecs,
        DecodeRecoverySource::CachedSafeIdrRebuild,
        &control,
        session_id,
    );
    output.mark_decode_recovery_replaying(2);
    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));
    assert!(output.stage_decode_recovery_frame(frame(target_nsecs, 0), 3, session_id));
    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));

    // A frame still undergoing preparation must not make a disconnected
    // staging tail appear ready merely because the prefix has coverage.
    let transaction = output.decode_recovery_transaction.as_mut().unwrap();
    transaction
        .staging_queue
        .push_queued(frame(target_nsecs, 40_000_000));
    transaction
        .staging_queue
        .push_queued(frame(target_nsecs + 80_000_000, 40_000_000));
    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));
    output
        .decode_recovery_transaction
        .as_mut()
        .unwrap()
        .staging_queue
        .clear();
    assert!(output.stage_decode_recovery_frame(frame(target_nsecs, 40_000_000), 3, session_id));
    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(false, session_id));
    assert!(
        output
            .check_decode_recovery_deadline(
                Instant::now() + DECODE_RECOVERY_MAX_WALL_TIME,
                &control,
                session_id,
            )
            .is_err()
    );
    assert!(!output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));
    assert_eq!(
        output.decode_recovery_phase(),
        Some(DecodeRecoveryPhase::Failed)
    );
}

#[test]
fn hardware_surface_pressure_preserves_decoder_error_reanchor_boundary() {
    let session_id = PlaybackSessionId(3);
    let control = FfmpegControl::new(session_id);
    let target_nsecs = 2_000_000_000;
    let resume_nsecs = target_nsecs + 1_000_000_000;
    let mut output = PlaybackOutputScheduler::new();
    output
        .scheduled_video_queue
        .push_queued(frame(target_nsecs - 40_000_000, 40_000_000));
    output.begin_decode_recovery(
        3,
        target_nsecs,
        DecodeRecoverySource::DecoderError,
        &control,
        session_id,
    );
    output.mark_decode_recovery_replaying(3);
    for index in 0..7 {
        assert!(output.stage_decode_recovery_frame(
            frame(resume_nsecs + index * 40_000_000, 40_000_000),
            3,
            session_id
        ));
    }
    assert!(output.maybe_buffer_decode_recovery_for_surface_pressure(true, session_id));
    let mut scheduler = PlaybackScheduler::new(target_nsecs - 40_000_000);
    let mut current_start_position_nsecs = target_nsecs - 40_000_000;
    assert!(!output.commit_decode_recovery_if_buffered(
        &control,
        &mut scheduler,
        None,
        &mut current_start_position_nsecs,
        true,
        session_id,
    ));
    assert_eq!(output.scheduled_video_queue.len(), 1);
    assert_eq!(current_start_position_nsecs, target_nsecs - 40_000_000);
    assert!(output.maybe_enter_decode_recovery_barrier(
        Instant::now(),
        target_nsecs,
        &control,
        session_id
    ));
    assert!(output.commit_decode_recovery_if_buffered(
        &control,
        &mut scheduler,
        None,
        &mut current_start_position_nsecs,
        true,
        session_id,
    ));
    assert_eq!(
        output.decode_recovery_phase(),
        Some(DecodeRecoveryPhase::Reanchored)
    );
    assert_eq!(current_start_position_nsecs, resume_nsecs);
}
