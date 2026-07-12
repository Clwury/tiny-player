use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use ffmpeg_sys_next as ffi;

use super::audio_decode_pipeline::AudioDecodePipeline;
use super::scheduled_video_queue::queued_video_continuity_gap_threshold_nsecs;
use super::video_decode_pipeline::{
    HevcAdmittedVideoProgressObservation, HevcDecodeChainRecoveryAction,
    HevcDecodePacketObservation, HevcDecodedFrameGapAction, HevcDecodedFrameGapObservation,
    HevcSeekPrerollProgressObservation, VideoDecodePipeline,
};
use super::video_decode_recovery_service::{
    VideoDecodeRecoveryServiceContext, service_video_decode_recovery_result,
};
use super::video_decode_worker::VideoDecodedFrame;
use super::video_frame_admission_service::{
    PreparedVideoFrameAdmissionContext, admit_prepared_video_frame,
};
use super::video_frame_prepare_admission_service::{
    DecodedVideoFramePrepareStatus, DecodedVideoFrameStartStatus,
    enqueue_decoded_video_frame_prepare, service_decoded_video_frame_start,
};
use super::video_frame_prepare_worker::{
    VideoFramePrepareDiagnosticContext, VideoFramePrepareEnqueueResult, VideoFramePrepareResult,
    VideoFramePrepareWorker,
};
use super::{
    AudioOutput, AvPacket, BufferedReporter, CORRUPT_VIDEO_FRAME_RECOVERY_ERROR,
    DECODE_PACKET_SLOW_LOG_AFTER, DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    DemuxReaderWatermark, DoviPipeline, FfmpegControl, PlaybackGeneration, PlaybackOutputScheduler,
    PlaybackScheduler, PositionReporter, StreamInfo, SubtitlePipeline, TimestampMapper,
    VideoDecodeRecovery,
};

fn hevc_decoded_frame_gap_allows_scheduled_queue_admission(
    action: HevcDecodedFrameGapAction,
) -> bool {
    action == HevcDecodedFrameGapAction::Admit
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodedVideoFrameStartAction {
    DropBeforeStart,
    Use { realign: bool },
}

const DECODED_VIDEO_FRAME_START_TOLERANCE_NSECS: u64 = 5_000_000;

pub(in crate::player::backend::ffmpeg) fn decoded_video_frame_start_action(
    frame_timeline_nsecs: u64,
    current_start_position_nsecs: u64,
    recovery_realign: bool,
) -> DecodedVideoFrameStartAction {
    if recovery_realign {
        return DecodedVideoFrameStartAction::Use { realign: true };
    }
    let earliest_accepted_start_nsecs =
        current_start_position_nsecs.saturating_sub(DECODED_VIDEO_FRAME_START_TOLERANCE_NSECS);
    if frame_timeline_nsecs < earliest_accepted_start_nsecs {
        return DecodedVideoFrameStartAction::DropBeforeStart;
    }
    DecodedVideoFrameStartAction::Use { realign: false }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_decoded_video_frame<F>(
    decoded_frame: VideoDecodedFrame,
    generation: u64,
    video_decode_pipeline: &mut VideoDecodePipeline,
    video_stream: StreamInfo,
    video_decode_recovery: &mut VideoDecodeRecovery,
    decoded_video_frame_count: &mut u64,
    dropped_video_frames_before_start_count: &mut u64,
    video_frame_duration_nsecs: u64,
    video_clock: &mut TimestampMapper,
    playback_timeline_origin_nsecs: &mut Option<u64>,
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
    _vo_queue: &VideoOutputQueue,
    _frame_presented: &AtomicBool,
    subtitle_pipeline: &mut SubtitlePipeline,
    video_frame_prepare_worker: &mut VideoFramePrepareWorker,
    current_start_position_nsecs: &mut u64,
    _demux_reader_watermark: F,
) -> std::result::Result<bool, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let frame = decoded_frame.as_mut_ptr();
    if control.has_pending_seek() {
        return Ok(false);
    }
    *decoded_video_frame_count = (*decoded_video_frame_count).saturating_add(1);
    let start_frame = match service_decoded_video_frame_start(
        frame,
        *decoded_video_frame_count,
        video_decode_pipeline.info().time_base,
        video_clock,
        playback_timeline_origin_nsecs,
        subtitle_pipeline,
        video_decode_recovery,
        dropped_video_frames_before_start_count,
        current_start_position_nsecs,
        audio_stream_start_nsecs,
        audio_clock,
        scheduler,
        audio_output,
        output_scheduler,
        dovi_pipeline,
        buffered_reporter,
        control,
        session_id,
        event_tx,
    ) {
        DecodedVideoFrameStartStatus::Ready(frame) => frame,
        DecodedVideoFrameStartStatus::DroppedBeforeStart => {
            let output_snapshot = output_scheduler.snapshot();
            if output_snapshot.rebuffering
                || output_snapshot.first_video_frame_pending
                || output_snapshot.queued_video_frames == 0
            {
                let video_decode_snapshot = video_decode_pipeline.snapshot();
                let prepare_snapshot = video_frame_prepare_worker.snapshot();
                tracing::debug!(
                    session_id = ?session_id,
                    generation,
                    decoded_video_frame_count = *decoded_video_frame_count,
                    current_start_position_nsecs = *current_start_position_nsecs,
                    video_decode_state = ?video_decode_snapshot.state,
                    video_decode_queued_frames = video_decode_snapshot.queued_frames,
                    video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
                    video_decode_completed_packets = video_decode_snapshot.completed_packets,
                    video_prepare_state = ?prepare_snapshot.state,
                    video_prepare_pending_input_frames = prepare_snapshot.pending_input_frames,
                    video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
                    video_prepare_completed_frames = prepare_snapshot.completed_frames,
                    output_state = ?output_snapshot.state,
                    output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    output_rebuffering = output_snapshot.rebuffering,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    queued_video_range = ?output_snapshot.queued_video_range_nsecs,
                    "decoded FFmpeg video frame dropped before prepare admission"
                );
            }
            return Ok(true);
        }
        DecodedVideoFrameStartStatus::SeekPrerollBeforeStart(progress) => {
            video_decode_pipeline.observe_hevc_seek_preroll_progress(
                HevcSeekPrerollProgressObservation {
                    session_id,
                    codec_id: video_stream.codec_id,
                    frame_timeline_nsecs: progress.timeline_nsecs,
                    target_nsecs: progress.target_nsecs,
                    preroll_frames: progress.preroll_frames,
                },
            );
            let output_snapshot = output_scheduler.snapshot();
            if output_snapshot.rebuffering
                || output_snapshot.first_video_frame_pending
                || output_snapshot.queued_video_frames == 0
            {
                let video_decode_snapshot = video_decode_pipeline.snapshot();
                let prepare_snapshot = video_frame_prepare_worker.snapshot();
                tracing::debug!(
                    session_id = ?session_id,
                    generation,
                    decoded_video_frame_count = *decoded_video_frame_count,
                    timeline_nsecs = progress.timeline_nsecs,
                    target_nsecs = progress.target_nsecs,
                    preroll_frames = progress.preroll_frames,
                    current_start_position_nsecs = *current_start_position_nsecs,
                    video_decode_state = ?video_decode_snapshot.state,
                    video_decode_queued_frames = video_decode_snapshot.queued_frames,
                    video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
                    video_decode_completed_packets = video_decode_snapshot.completed_packets,
                    video_prepare_state = ?prepare_snapshot.state,
                    video_prepare_pending_input_frames = prepare_snapshot.pending_input_frames,
                    video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
                    video_prepare_completed_frames = prepare_snapshot.completed_frames,
                    output_state = ?output_snapshot.state,
                    output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    output_rebuffering = output_snapshot.rebuffering,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    queued_video_range = ?output_snapshot.queued_video_range_nsecs,
                    "decoded FFmpeg video frame consumed as seek preroll before prepare admission"
                );
            }
            return Ok(true);
        }
        DecodedVideoFrameStartStatus::DroppedCorrupt => {
            return Err(CORRUPT_VIDEO_FRAME_RECOVERY_ERROR.to_string());
        }
    };
    let frame_pts = start_frame.frame_pts;
    let timeline_nsecs = start_frame.timeline_nsecs;
    let output_snapshot = output_scheduler.snapshot();
    let force_completion_reason = if *decoded_video_frame_count == 1 {
        "first_decoded_video_frame"
    } else if output_snapshot.rebuffering {
        "output_rebuffering"
    } else if output_snapshot.first_video_frame_pending {
        "first_video_frame_pending"
    } else if output_snapshot.queued_video_frames == 0 {
        "empty_video_output"
    } else {
        "normal"
    };
    let force_completion_log = force_completion_reason != "normal";
    let diagnostic = VideoFramePrepareDiagnosticContext::from_output_snapshot(
        session_id,
        *decoded_video_frame_count,
        force_completion_log,
        force_completion_reason,
        output_snapshot,
    );

    let prepare_status = enqueue_decoded_video_frame_prepare(
        decoded_frame,
        generation,
        diagnostic,
        frame_pts,
        timeline_nsecs,
        video_frame_duration_nsecs,
        dovi_pipeline,
        video_frame_prepare_worker,
        &video_decode_pipeline.info().convert_context,
    )?;
    if force_completion_log || prepare_status != DecodedVideoFramePrepareStatus::Queued {
        let prepare_snapshot = video_frame_prepare_worker.snapshot();
        let video_decode_snapshot = video_decode_pipeline.snapshot();
        let audio_output_snapshot = audio_output.and_then(|output| output.snapshot().ok());
        tracing::debug!(
            session_id = ?session_id,
            generation,
            decoded_video_frame_count = *decoded_video_frame_count,
            prepare_status = ?prepare_status,
            frame_pts_nsecs = frame_pts.nsecs,
            timeline_nsecs,
            duration_nsecs = video_frame_duration_nsecs,
            current_start_position_nsecs = *current_start_position_nsecs,
            force_completion_log,
            force_completion_reason,
            video_decode_state = ?video_decode_snapshot.state,
            video_decode_queued_frames = video_decode_snapshot.queued_frames,
            video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            video_prepare_state = ?prepare_snapshot.state,
            video_prepare_pending_input_frames = prepare_snapshot.pending_input_frames,
            video_prepare_pending_input_capacity = prepare_snapshot.pending_input_capacity,
            video_prepare_pending_input_full = prepare_snapshot.pending_input_full(),
            video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
            video_prepare_completed_frames = prepare_snapshot.completed_frames,
            video_prepare_command_queue_capacity = prepare_snapshot.command_queue_capacity,
            output_state = ?output_snapshot.state,
            output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
            output_rebuffering = output_snapshot.rebuffering,
            queued_video_frames = output_snapshot.queued_video_frames,
            queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
            queued_video_range = ?output_snapshot.queued_video_range_nsecs,
            pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
            audio_output_pending_ms = ?audio_output_snapshot
                .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
            audio_output_queue_ms = ?audio_output_snapshot
                .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
            "FFmpeg decoded video frame prepare enqueue decision"
        );
    }
    match prepare_status {
        DecodedVideoFramePrepareStatus::Queued => Ok(true),
        DecodedVideoFramePrepareStatus::Backpressured => Ok(false),
        DecodedVideoFramePrepareStatus::DroppedCorrupt => {
            Err(CORRUPT_VIDEO_FRAME_RECOVERY_ERROR.to_string())
        }
    }
}

pub(super) fn log_prepared_video_frame_if_slow(
    result: &VideoFramePrepareResult,
    session_id: PlaybackSessionId,
    output_scheduler: &PlaybackOutputScheduler,
    audio_output: Option<&AudioOutput>,
) {
    if result.elapsed < DECODE_PACKET_SLOW_LOG_AFTER {
        return;
    }
    let audio_output_snapshot = audio_output.and_then(|output| output.snapshot().ok());
    let output_snapshot = output_scheduler.snapshot();
    tracing::debug!(
        session_id = ?session_id,
        generation = result.generation,
        elapsed_ms = result.elapsed.as_secs_f64() * 1000.0,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        decoded_video_range = ?output_snapshot.queued_video_range_nsecs,
        pending_audio_ms = audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        audio_output_queue_ms = audio_output_snapshot
            .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
        "FFmpeg video frame prepare completed slowly"
    );
}

#[derive(Clone, Copy, Default)]
struct ReadyVideoDecodeOutputTiming {
    service_worker: Duration,
    retry_prepare_input: Duration,
    poll_prepare_result: Duration,
    admit_prepared_frame: Duration,
    poll_decoded_frame: Duration,
    enqueue_decoded_frame_prepare: Duration,
    has_pending_prepare: Duration,
    poll_packet_status: Duration,
    decode_recovery: Duration,
    iterations: u64,
    prepared_results: u64,
    decoded_frames: u64,
    completed_packets: u64,
}

fn log_ready_video_decode_output_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    timing: ReadyVideoDecodeOutputTiming,
    made_progress: bool,
    video_decode_pipeline: &VideoDecodePipeline,
    video_frame_prepare_worker: &VideoFramePrepareWorker,
    output_scheduler: &PlaybackOutputScheduler,
) {
    let video_decode_snapshot = video_decode_pipeline.snapshot();
    let prepare_snapshot = video_frame_prepare_worker.snapshot();
    let output_snapshot = output_scheduler.snapshot();
    tracing::trace!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        service_worker_ms = timing.service_worker.as_secs_f64() * 1000.0,
        retry_prepare_input_ms = timing.retry_prepare_input.as_secs_f64() * 1000.0,
        poll_prepare_result_ms = timing.poll_prepare_result.as_secs_f64() * 1000.0,
        admit_prepared_frame_ms = timing.admit_prepared_frame.as_secs_f64() * 1000.0,
        poll_decoded_frame_ms = timing.poll_decoded_frame.as_secs_f64() * 1000.0,
        enqueue_decoded_frame_prepare_ms =
            timing.enqueue_decoded_frame_prepare.as_secs_f64() * 1000.0,
        has_pending_prepare_ms = timing.has_pending_prepare.as_secs_f64() * 1000.0,
        poll_packet_status_ms = timing.poll_packet_status.as_secs_f64() * 1000.0,
        decode_recovery_ms = timing.decode_recovery.as_secs_f64() * 1000.0,
        iterations = timing.iterations,
        prepared_results = timing.prepared_results,
        decoded_frames = timing.decoded_frames,
        completed_packets = timing.completed_packets,
        made_progress,
        video_decode_state = ?video_decode_snapshot.state,
        video_decode_queued_frames = video_decode_snapshot.queued_frames,
        video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
        video_prepare_state = ?prepare_snapshot.state,
        video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
        video_prepare_completed_frames = prepare_snapshot.completed_frames,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        output_state = ?output_snapshot.state,
        "FFmpeg ready video decode output drain timing"
    );
    if total < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.service_worker < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.retry_prepare_input < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.poll_prepare_result < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.admit_prepared_frame < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.poll_decoded_frame < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.enqueue_decoded_frame_prepare < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.has_pending_prepare < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.poll_packet_status < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.decode_recovery < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        service_worker_ms = timing.service_worker.as_secs_f64() * 1000.0,
        retry_prepare_input_ms = timing.retry_prepare_input.as_secs_f64() * 1000.0,
        poll_prepare_result_ms = timing.poll_prepare_result.as_secs_f64() * 1000.0,
        admit_prepared_frame_ms = timing.admit_prepared_frame.as_secs_f64() * 1000.0,
        poll_decoded_frame_ms = timing.poll_decoded_frame.as_secs_f64() * 1000.0,
        enqueue_decoded_frame_prepare_ms =
            timing.enqueue_decoded_frame_prepare.as_secs_f64() * 1000.0,
        has_pending_prepare_ms = timing.has_pending_prepare.as_secs_f64() * 1000.0,
        poll_packet_status_ms = timing.poll_packet_status.as_secs_f64() * 1000.0,
        decode_recovery_ms = timing.decode_recovery.as_secs_f64() * 1000.0,
        iterations = timing.iterations,
        prepared_results = timing.prepared_results,
        decoded_frames = timing.decoded_frames,
        completed_packets = timing.completed_packets,
        made_progress,
        video_decode_state = ?video_decode_snapshot.state,
        video_decode_queued_frames = video_decode_snapshot.queued_frames,
        video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
        video_prepare_state = ?prepare_snapshot.state,
        video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
        video_prepare_completed_frames = prepare_snapshot.completed_frames,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        output_state = ?output_snapshot.state,
        "FFmpeg ready video decode output drain completed slowly"
    );
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::while_let_loop)]
pub(super) fn drain_ready_video_decode_output<F>(
    video_decode_pipeline: &mut VideoDecodePipeline,
    mut audio_decode_pipeline: Option<&mut AudioDecodePipeline>,
    video_stream: StreamInfo,
    video_decode_recovery: &mut VideoDecodeRecovery,
    playback_generation: &mut PlaybackGeneration,
    decoded_video_frame_count: &mut u64,
    dropped_video_frames_before_start_count: &mut u64,
    video_frame_duration_nsecs: u64,
    video_clock: &mut TimestampMapper,
    playback_timeline_origin_nsecs: &mut Option<u64>,
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
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    subtitle_pipeline: &mut SubtitlePipeline,
    video_frame_prepare_worker: &mut VideoFramePrepareWorker,
    video_decode_skip_nonref_active: &mut bool,
    current_start_position_nsecs: &mut u64,
    mut demux_reader_watermark: F,
) -> std::result::Result<bool, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let mut timing = ReadyVideoDecodeOutputTiming::default();
    let mut made_progress = false;
    let stage_started_at = Instant::now();
    video_decode_pipeline.service_worker()?;
    timing.service_worker = stage_started_at.elapsed();
    loop {
        timing.iterations = timing.iterations.saturating_add(1);
        let Some(front_generation) = video_decode_pipeline.front_generation() else {
            break;
        };

        let stage_started_at = Instant::now();
        let retry_prepare_input = video_frame_prepare_worker.retry_pending_input()?;
        timing.retry_prepare_input += stage_started_at.elapsed();
        if retry_prepare_input == VideoFramePrepareEnqueueResult::InputFull {
            break;
        }

        let stage_started_at = Instant::now();
        let prepare_result = video_frame_prepare_worker.poll_result(front_generation)?;
        timing.poll_prepare_result += stage_started_at.elapsed();
        if let Some(prepare_result) = prepare_result {
            timing.prepared_results = timing.prepared_results.saturating_add(1);
            log_prepared_video_frame_if_slow(
                &prepare_result,
                session_id,
                output_scheduler,
                audio_output,
            );
            made_progress = true;
            match prepare_result.result {
                Ok(prepared_frame) => {
                    let admitted_frame_timeline_nsecs = prepared_frame.timeline_nsecs;
                    let before_queue_end_nsecs = output_scheduler
                        .scheduled_video_queue
                        .range_nsecs()
                        .map(|(_, end)| end);
                    let previous_frame_timing =
                        output_scheduler.scheduled_video_queue.back_timing_nsecs();
                    let previous_expected_next_nsecs =
                        previous_frame_timing.map(|(pts, duration)| pts.saturating_add(duration));
                    let previous_gap_nsecs = previous_expected_next_nsecs.map(|expected| {
                        i128::from(prepared_frame.timeline_nsecs)
                            .saturating_sub(i128::from(expected))
                    });
                    let max_gap_nsecs = previous_frame_timing
                        .map(|(_, duration)| queued_video_continuity_gap_threshold_nsecs(duration))
                        .unwrap_or_else(|| {
                            queued_video_continuity_gap_threshold_nsecs(
                                prepared_frame.duration_nsecs,
                            )
                        });
                    let audio_snapshot = audio_output.and_then(|output| output.snapshot().ok());
                    let audio_played_timeline_nsecs =
                        audio_snapshot.map(|snapshot| snapshot.played_timeline_nsecs);
                    let output_snapshot =
                        output_scheduler.snapshot_for_played_until(audio_played_timeline_nsecs);
                    let fallback_target_nsecs = previous_expected_next_nsecs
                        .or(audio_played_timeline_nsecs)
                        .unwrap_or(*current_start_position_nsecs);
                    let gap_action = video_decode_pipeline.observe_hevc_decoded_frame_gap(
                        HevcDecodedFrameGapObservation {
                            session_id,
                            codec_id: video_stream.codec_id,
                            timeline_nsecs: prepared_frame.timeline_nsecs,
                            duration_nsecs: prepared_frame.duration_nsecs,
                            previous_expected_next_nsecs,
                            previous_gap_nsecs,
                            max_gap_nsecs,
                            fallback_target_nsecs,
                            audio_played_timeline_nsecs,
                            recovery_waiting: video_decode_recovery.waiting_for_keyframe(),
                            output_snapshot,
                        },
                    );
                    if !hevc_decoded_frame_gap_allows_scheduled_queue_admission(gap_action) {
                        tracing::debug!(
                            session_id = ?session_id,
                            timeline_nsecs = prepared_frame.timeline_nsecs,
                            duration_nsecs = prepared_frame.duration_nsecs,
                            previous_expected_next_nsecs,
                            previous_gap_nsecs,
                            fallback_reason = ?video_decode_pipeline
                                .hevc_decode_chain_stats()
                                .pending_fallback_reason
                                .map(|reason| reason.as_str()),
                            "dropped prepared HEVC video frame after decode-chain gap fallback"
                        );
                        break;
                    }
                    let stage_started_at = Instant::now();
                    admit_prepared_video_frame(PreparedVideoFrameAdmissionContext {
                        prepared_frame,
                        decoded_video_frame_count: *decoded_video_frame_count,
                        scheduler,
                        audio_output,
                        output_scheduler,
                        buffered_reporter,
                        control,
                        session_id,
                        event_tx,
                        vo_queue,
                        frame_presented,
                        position_reporter,
                        subtitle_pipeline,
                        current_start_position_nsecs,
                        video_is_hevc: video_stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC,
                        demux_reader_watermark: &mut demux_reader_watermark,
                    })?;
                    let after_queue_end_nsecs = output_scheduler
                        .scheduled_video_queue
                        .range_nsecs()
                        .map(|(_, end)| end);
                    video_decode_pipeline.observe_hevc_admitted_video_progress(
                        HevcAdmittedVideoProgressObservation {
                            session_id,
                            codec_id: video_stream.codec_id,
                            frame_timeline_nsecs: admitted_frame_timeline_nsecs,
                            current_start_position_nsecs: *current_start_position_nsecs,
                            before_queue_end_nsecs,
                            after_queue_end_nsecs,
                        },
                    );
                    timing.admit_prepared_frame += stage_started_at.elapsed();
                }
                Err(error) => {
                    let realign_after_decode_recovery = video_decode_pipeline
                        .front_realign_after_decode_recovery(
                            output_scheduler.snapshot().first_video_frame_pending,
                        );
                    let packet = AvPacket::ref_from(
                        video_decode_pipeline
                            .front_packet()
                            .expect("front video decode packet exists"),
                    )?;
                    service_video_decode_recovery_result(VideoDecodeRecoveryServiceContext {
                        result: Err(error),
                        packet: &packet,
                        realign_after_decode_recovery,
                        video_stream,
                        playback_generation,
                        video_decode_pipeline,
                        video_decode_skip_nonref_active,
                        audio_decode_pipeline: audio_decode_pipeline.as_deref_mut(),
                        subtitle_pipeline,
                        video_decode_recovery,
                        output_scheduler,
                        dovi_pipeline,
                        control,
                    })?;
                    break;
                }
            }
            if control.has_pending_seek() {
                break;
            }
            continue;
        }
        log_video_frame_prepare_result_pending(
            session_id,
            front_generation,
            timing.iterations,
            video_decode_pipeline,
            video_frame_prepare_worker,
            output_scheduler,
            audio_output,
        );

        let stage_started_at = Instant::now();
        let decoded_frame = video_decode_pipeline.poll_frame(front_generation)?;
        timing.poll_decoded_frame += stage_started_at.elapsed();
        if let Some(decoded_frame) = decoded_frame {
            timing.decoded_frames = timing.decoded_frames.saturating_add(1);
            made_progress = true;
            let stage_started_at = Instant::now();
            let frame_result = service_decoded_video_frame(
                decoded_frame,
                front_generation,
                video_decode_pipeline,
                video_stream,
                video_decode_recovery,
                decoded_video_frame_count,
                dropped_video_frames_before_start_count,
                video_frame_duration_nsecs,
                video_clock,
                playback_timeline_origin_nsecs,
                audio_stream_start_nsecs,
                audio_clock,
                scheduler,
                audio_output,
                output_scheduler,
                dovi_pipeline,
                buffered_reporter,
                control,
                session_id,
                event_tx,
                vo_queue,
                frame_presented,
                subtitle_pipeline,
                video_frame_prepare_worker,
                current_start_position_nsecs,
                &mut demux_reader_watermark,
            );
            timing.enqueue_decoded_frame_prepare += stage_started_at.elapsed();
            let queued_for_prepare = match frame_result {
                Ok(queued_for_prepare) => queued_for_prepare,
                Err(error) => {
                    let realign_after_decode_recovery = video_decode_pipeline
                        .front_realign_after_decode_recovery(
                            output_scheduler.snapshot().first_video_frame_pending,
                        );
                    let packet = AvPacket::ref_from(
                        video_decode_pipeline
                            .front_packet()
                            .expect("front video decode packet exists"),
                    )?;
                    service_video_decode_recovery_result(VideoDecodeRecoveryServiceContext {
                        result: Err(error),
                        packet: &packet,
                        realign_after_decode_recovery,
                        video_stream,
                        playback_generation,
                        video_decode_pipeline,
                        video_decode_skip_nonref_active,
                        audio_decode_pipeline: audio_decode_pipeline.as_deref_mut(),
                        subtitle_pipeline,
                        video_decode_recovery,
                        output_scheduler,
                        dovi_pipeline,
                        control,
                    })?;
                    break;
                }
            };
            if control.has_pending_seek() {
                break;
            }
            if !queued_for_prepare {
                break;
            }
            continue;
        }

        let stage_started_at = Instant::now();
        let has_pending_prepare =
            video_frame_prepare_worker.has_pending_for_generation(front_generation)?;
        timing.has_pending_prepare += stage_started_at.elapsed();
        if has_pending_prepare {
            break;
        }

        let stage_started_at = Instant::now();
        let packet_status = video_decode_pipeline.poll_packet_status(front_generation)?;
        timing.poll_packet_status += stage_started_at.elapsed();
        let Some(status) = packet_status else {
            break;
        };
        let pending_packet = video_decode_pipeline
            .pop_completed_packet()
            .expect("front video decode packet exists for status");
        made_progress = true;
        timing.completed_packets = timing.completed_packets.saturating_add(1);
        if !status.drained && status.elapsed >= DECODE_PACKET_SLOW_LOG_AFTER {
            let video_decode_snapshot = video_decode_pipeline.snapshot();
            let audio_output_snapshot = audio_output.and_then(|output| output.snapshot().ok());
            let output_snapshot = output_scheduler.snapshot();
            tracing::debug!(
                session_id = ?session_id,
                packet_pts = ?pending_packet.packet.best_timestamp(),
                packet_bytes = pending_packet.packet.byte_len(),
                decoded_frames = status.decoded_frames,
                elapsed_ms = status.elapsed.as_secs_f64() * 1000.0,
                queued_video_frames = output_snapshot.queued_video_frames,
                queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
                decoded_video_range = ?output_snapshot.queued_video_range_nsecs,
                pending_audio_ms = audio_output_snapshot
                    .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
                audio_output_queue_ms = audio_output_snapshot
                    .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
                video_decode_state = ?video_decode_snapshot.state,
                video_decode_queued_frames = video_decode_snapshot.queued_frames,
                video_decode_queue_capacity = video_decode_snapshot.queue_capacity,
                video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
                video_decode_completed_packets = video_decode_snapshot.completed_packets,
                "FFmpeg video decode packet completed slowly"
            );
        }
        let output_snapshot = output_scheduler.snapshot();
        let demux_watermark = demux_reader_watermark();
        let fallback_target_nsecs = audio_output
            .and_then(|output| output.snapshot().ok())
            .map(|snapshot| snapshot.played_timeline_nsecs)
            .or_else(|| {
                output_snapshot
                    .queued_video_range_nsecs
                    .map(|(start, _)| start)
            })
            .unwrap_or(*current_start_position_nsecs);
        let hevc_recovery_action =
            video_decode_pipeline.observe_hevc_decode_packet_status(HevcDecodePacketObservation {
                status: &status,
                packet: &pending_packet.packet,
                video_stream,
                output_snapshot,
                demux_watermark,
                has_audio_output: audio_output.is_some(),
                fallback_target_nsecs,
                session_id,
            });
        if hevc_recovery_action == HevcDecodeChainRecoveryAction::SoftRecovery {
            if *video_decode_skip_nonref_active {
                video_decode_pipeline.set_skip_nonref_frames(false)?;
                *video_decode_skip_nonref_active = false;
            }
            let generation = playback_generation.advance();
            video_decode_pipeline.flush_buffers(generation)?;
            video_decode_recovery.begin_with_realign(true);
            video_decode_pipeline.clear_packets();
            dovi_pipeline.reset();
            break;
        }
        let stage_started_at = Instant::now();
        service_video_decode_recovery_result(VideoDecodeRecoveryServiceContext {
            result: status.result,
            packet: &pending_packet.packet,
            realign_after_decode_recovery: pending_packet.realign_after_decode_recovery,
            video_stream,
            playback_generation,
            video_decode_pipeline,
            video_decode_skip_nonref_active,
            audio_decode_pipeline: audio_decode_pipeline.as_deref_mut(),
            subtitle_pipeline,
            video_decode_recovery,
            output_scheduler,
            dovi_pipeline,
            control,
        })?;
        timing.decode_recovery += stage_started_at.elapsed();
    }
    log_ready_video_decode_output_timing(
        session_id,
        started_at.elapsed(),
        timing,
        made_progress,
        video_decode_pipeline,
        video_frame_prepare_worker,
        output_scheduler,
    );
    Ok(made_progress)
}

fn log_video_frame_prepare_result_pending(
    session_id: PlaybackSessionId,
    front_generation: u64,
    iterations: u64,
    video_decode_pipeline: &VideoDecodePipeline,
    video_frame_prepare_worker: &VideoFramePrepareWorker,
    output_scheduler: &PlaybackOutputScheduler,
    audio_output: Option<&AudioOutput>,
) {
    let prepare_snapshot = video_frame_prepare_worker.snapshot();
    let output_snapshot = output_scheduler.snapshot();
    if !output_snapshot.rebuffering
        && !output_snapshot.first_video_frame_pending
        && output_snapshot.queued_video_frames > 0
        && prepare_snapshot.completed_frames == 0
    {
        return;
    }

    let video_decode_snapshot = video_decode_pipeline.snapshot();
    let audio_output_snapshot = audio_output.and_then(|output| output.snapshot().ok());
    tracing::debug!(
        session_id = ?session_id,
        front_generation,
        iterations,
        video_prepare_state = ?prepare_snapshot.state,
        video_prepare_pending_input_frames = prepare_snapshot.pending_input_frames,
        video_prepare_pending_input_capacity = prepare_snapshot.pending_input_capacity,
        video_prepare_pending_input_full = prepare_snapshot.pending_input_full(),
        video_prepare_in_flight_frames = prepare_snapshot.in_flight_frames,
        video_prepare_completed_frames = prepare_snapshot.completed_frames,
        video_prepare_command_queue_capacity = prepare_snapshot.command_queue_capacity,
        video_decode_state = ?video_decode_snapshot.state,
        video_decode_queued_frames = video_decode_snapshot.queued_frames,
        video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
        video_decode_completed_packets = video_decode_snapshot.completed_packets,
        output_state = ?output_snapshot.state,
        output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
        audio_output_pending_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        audio_output_queue_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
        "FFmpeg video frame prepare result not ready for front generation"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        HevcDecodedFrameGapAction, hevc_decoded_frame_gap_allows_scheduled_queue_admission,
    };

    #[test]
    fn hevc_pts_gap_fallback_drops_current_frame_before_scheduled_queue_admission() {
        let mut scheduled_queue_admissions = 0;
        if hevc_decoded_frame_gap_allows_scheduled_queue_admission(
            HevcDecodedFrameGapAction::DropForFallback,
        ) {
            scheduled_queue_admissions += 1;
        }

        assert_eq!(scheduled_queue_admissions, 0);
        assert!(hevc_decoded_frame_gap_allows_scheduled_queue_admission(
            HevcDecodedFrameGapAction::Admit
        ));
    }
}
