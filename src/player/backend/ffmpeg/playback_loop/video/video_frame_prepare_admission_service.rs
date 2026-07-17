use std::sync::mpsc::Sender;

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::BackendEvent,
    render_host::{FramePts, PlaybackSessionId},
};

use super::decoded_video_frame::{DecodedVideoFrameStartAction, decoded_video_frame_start_action};
use super::video_decode_pipeline::SeekPrerollFrameProgress;
use super::video_decode_worker::VideoDecodedFrame;
use super::video_frame_prepare_worker::{
    DecodedVideoFrameDiagnostic, VideoFramePrepareDiagnosticContext,
    VideoFramePrepareEnqueueResult, VideoFramePrepareInput, VideoFramePrepareWorker,
};
use super::{
    AudioOutput, BufferedReporter, DoviPipeline, FfmpegControl, PlaybackOutputScheduler,
    PlaybackScheduler, SubtitlePipeline, TimestampMapper, VideoDecodeRecovery,
    VideoFrameConvertContext, frame_best_effort_timestamp, frame_decode_error_flags,
    frame_is_corrupt, nsecs_to_seconds,
};

pub(super) struct DecodedVideoFrameStartFrame {
    pub(super) timeline_nsecs: u64,
    pub(super) frame_pts: FramePts,
}

pub(super) enum DecodedVideoFrameStartStatus {
    Ready(DecodedVideoFrameStartFrame),
    DroppedBeforeStart,
    SeekPrerollBeforeStart(SeekPrerollFrameProgress),
    DroppedCorrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecodedVideoFramePrepareStatus {
    Queued,
    Backpressured,
    DroppedCorrupt,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_decoded_video_frame_start(
    frame: *mut ffi::AVFrame,
    frame_count: u64,
    video_time_base: ffi::AVRational,
    video_clock: &mut TimestampMapper,
    playback_timeline_origin_nsecs: &mut Option<u64>,
    subtitle_pipeline: &mut SubtitlePipeline,
    video_decode_recovery: &mut VideoDecodeRecovery,
    dropped_video_frames_before_start_count: &mut u64,
    current_start_position_nsecs: &mut u64,
    audio_stream_start_nsecs: Option<u64>,
    audio_clock: &mut TimestampMapper,
    scheduler: &mut PlaybackScheduler,
    audio_output: Option<&AudioOutput>,
    output_scheduler: &mut PlaybackOutputScheduler,
    dovi_pipeline: &mut DoviPipeline,
    buffered_reporter: &mut BufferedReporter,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) -> DecodedVideoFrameStartStatus {
    let raw_timestamp = frame_best_effort_timestamp(frame);
    let timestamp = video_clock.map(raw_timestamp, video_time_base);
    subtitle_pipeline.refresh_timeline_origin(playback_timeline_origin_nsecs, video_clock);
    let frame_pts = FramePts {
        nsecs: timestamp.timeline_nsecs,
    };
    if frame_count == 1
        || frame_count.is_multiple_of(60)
        || video_decode_recovery.waiting_for_keyframe()
    {
        let output_snapshot = output_scheduler.snapshot();
        tracing::debug!(
            frame_count,
            raw_timestamp,
            timeline_nsecs = timestamp.timeline_nsecs,
            current_start_position_nsecs = *current_start_position_nsecs,
            output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
            recovery_waiting = video_decode_recovery.waiting_for_keyframe(),
            decode_error_flags = frame_decode_error_flags(frame),
            corrupt = frame_is_corrupt(frame),
            "decoded FFmpeg video frame"
        );
    }
    if drop_corrupt_video_frame_if_needed(frame, frame_pts, dovi_pipeline) {
        return DecodedVideoFrameStartStatus::DroppedCorrupt;
    }

    let realign_on_next_frame = video_decode_recovery.take_realign_on_next_frame();
    let start_action = decoded_video_frame_start_action(
        timestamp.timeline_nsecs,
        *current_start_position_nsecs,
        realign_on_next_frame,
    );
    match start_action {
        DecodedVideoFrameStartAction::DropBeforeStart => {
            if let Some(progress) =
                video_decode_recovery.observe_seek_preroll_frame(timestamp.timeline_nsecs)
            {
                if progress.preroll_frames == 1 || progress.preroll_frames.is_multiple_of(60) {
                    let output_snapshot = output_scheduler.snapshot();
                    tracing::debug!(
                        frame_count,
                        preroll_frames = progress.preroll_frames,
                        raw_timestamp,
                        timeline_nsecs = timestamp.timeline_nsecs,
                        target_nsecs = progress.target_nsecs,
                        first_preroll_frame_nsecs = ?progress.first_preroll_frame_nsecs,
                        last_preroll_frame_nsecs = ?progress.last_preroll_frame_nsecs,
                        current_start_position_nsecs = *current_start_position_nsecs,
                        output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                        recovery_realign_on_next_frame = realign_on_next_frame,
                        "dropping decoded FFmpeg video frame as seek preroll before playback target"
                    );
                }
                dovi_pipeline.discard_frame(frame_pts);
                return DecodedVideoFrameStartStatus::SeekPrerollBeforeStart(progress);
            }
            *dropped_video_frames_before_start_count =
                dropped_video_frames_before_start_count.saturating_add(1);
            if *dropped_video_frames_before_start_count == 1 {
                let output_snapshot = output_scheduler.snapshot();
                tracing::trace!(
                    frame_count,
                    dropped_frames_before_start = *dropped_video_frames_before_start_count,
                    raw_timestamp,
                    timeline_nsecs = timestamp.timeline_nsecs,
                    current_start_position_nsecs = *current_start_position_nsecs,
                    output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    recovery_realign_on_next_frame = realign_on_next_frame,
                    "dropping decoded FFmpeg video frame before playback start"
                );
            } else if dropped_video_frames_before_start_count.is_multiple_of(60) {
                let output_snapshot = output_scheduler.snapshot();
                tracing::debug!(
                    frame_count,
                    dropped_frames_before_start = *dropped_video_frames_before_start_count,
                    raw_timestamp,
                    timeline_nsecs = timestamp.timeline_nsecs,
                    current_start_position_nsecs = *current_start_position_nsecs,
                    output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    recovery_realign_on_next_frame = realign_on_next_frame,
                    "dropping decoded FFmpeg video frame before playback start"
                );
            }
            dovi_pipeline.discard_frame(frame_pts);
            DecodedVideoFrameStartStatus::DroppedBeforeStart
        }
        DecodedVideoFrameStartAction::Use { realign } => {
            if realign {
                tracing::debug!(
                    previous_start_position_nsecs = *current_start_position_nsecs,
                    pts = frame_pts.nsecs,
                    "realigning FFmpeg playback clock to recovered video keyframe"
                );
                *current_start_position_nsecs = frame_pts.nsecs;
                scheduler.reset(frame_pts.nsecs);
                if let Some(output) = audio_output {
                    output.reset_clock(frame_pts.nsecs);
                }
                *audio_clock =
                    TimestampMapper::new(audio_stream_start_nsecs, frame_pts.nsecs, None);
                output_scheduler.reset(control);
                subtitle_pipeline.realign_cues_for_position(frame_pts.nsecs);
                buffered_reporter.reset_to(nsecs_to_seconds(frame_pts.nsecs), session_id, event_tx);
            }
            if let Some(progress) =
                video_decode_recovery.finish_seek_bootstrap_after_target_frame(frame_pts.nsecs)
            {
                tracing::debug!(
                    frame_count,
                    pts = frame_pts.nsecs,
                    target_nsecs = progress.target_nsecs,
                    preroll_frames = progress.preroll_frames,
                    first_preroll_frame_nsecs = ?progress.first_preroll_frame_nsecs,
                    last_preroll_frame_nsecs = ?progress.last_preroll_frame_nsecs,
                    "completed FFmpeg seek preroll bootstrap at first admitted target video frame"
                );
            }
            DecodedVideoFrameStartStatus::Ready(DecodedVideoFrameStartFrame {
                timeline_nsecs: timestamp.timeline_nsecs,
                frame_pts,
            })
        }
    }
}

pub(super) fn service_drained_video_frame_start(
    frame: *mut ffi::AVFrame,
    video_time_base: ffi::AVRational,
    video_clock: &mut TimestampMapper,
    playback_timeline_origin_nsecs: &mut Option<u64>,
    subtitle_pipeline: &mut SubtitlePipeline,
    current_start_position_nsecs: u64,
    dovi_pipeline: &mut DoviPipeline,
) -> DecodedVideoFrameStartStatus {
    let timestamp = video_clock.map(frame_best_effort_timestamp(frame), video_time_base);
    subtitle_pipeline.refresh_timeline_origin(playback_timeline_origin_nsecs, video_clock);
    if timestamp.timeline_nsecs < current_start_position_nsecs {
        return DecodedVideoFrameStartStatus::DroppedBeforeStart;
    }
    let frame_pts = FramePts {
        nsecs: timestamp.timeline_nsecs,
    };
    if drop_corrupt_video_frame_if_needed(frame, frame_pts, dovi_pipeline) {
        return DecodedVideoFrameStartStatus::DroppedCorrupt;
    }
    DecodedVideoFrameStartStatus::Ready(DecodedVideoFrameStartFrame {
        timeline_nsecs: timestamp.timeline_nsecs,
        frame_pts,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn enqueue_decoded_video_frame_prepare(
    decoded_frame: VideoDecodedFrame,
    generation: u64,
    diagnostic: VideoFramePrepareDiagnosticContext,
    frame_pts: FramePts,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    dovi_pipeline: &mut DoviPipeline,
    video_frame_prepare_worker: &mut VideoFramePrepareWorker,
    convert_context: &VideoFrameConvertContext,
) -> std::result::Result<DecodedVideoFramePrepareStatus, String> {
    let frame = decoded_frame.as_mut_ptr();
    let frame_diagnostic = DecodedVideoFrameDiagnostic::from_frame(frame);
    if drop_corrupt_video_frame_if_needed(frame, frame_pts, dovi_pipeline) {
        return Ok(DecodedVideoFramePrepareStatus::DroppedCorrupt);
    }

    let dovi_metadata = dovi_pipeline.metadata_for_decoded_frame(frame, frame_pts);
    match video_frame_prepare_worker.try_enqueue(VideoFramePrepareInput {
        generation,
        diagnostic,
        frame_diagnostic,
        frame: decoded_frame,
        frame_pts,
        timeline_nsecs,
        duration_nsecs,
        convert_context: convert_context.clone(),
        dovi_metadata,
    })? {
        VideoFramePrepareEnqueueResult::Queued => Ok(DecodedVideoFramePrepareStatus::Queued),
        VideoFramePrepareEnqueueResult::InputFull => {
            Ok(DecodedVideoFramePrepareStatus::Backpressured)
        }
    }
}

pub(super) fn drop_corrupt_video_frame_if_needed(
    frame: *mut ffi::AVFrame,
    frame_pts: FramePts,
    dovi_pipeline: &mut DoviPipeline,
) -> bool {
    if !frame_is_corrupt(frame) {
        return false;
    }

    tracing::debug!(
        pts = frame_pts.nsecs,
        decode_error_flags = frame_decode_error_flags(frame),
        "dropping corrupt FFmpeg video frame"
    );
    dovi_pipeline.discard_frame(frame_pts);
    true
}
