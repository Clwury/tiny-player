use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::audio_decode_pipeline::AudioDecodePipeline;
use super::audio_decode_worker::{AudioDecodePacketResult, AudioDecodedFrame};
use super::{
    AudioOutput, BufferedReporter, DECODE_PACKET_SLOW_LOG_AFTER,
    DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER, FfmpegControl,
    PENDING_AUDIO_CONTINUITY_TOLERANCE, PlaybackOutputScheduler, PositionReporter,
    SubtitlePipeline, TimestampMapper, duration_nsecs,
};

const MAX_SEEK_AUDIO_LEAD_NSECS: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodedAudioFrameServiceStatus {
    ContinueDrain,
    StopDrain,
}

fn far_ahead_audio_frame_is_contiguous(
    frame_start_nsecs: u64,
    far_ahead_reference_nsecs: u64,
    pending_audio_range_nsecs: Option<(u64, u64)>,
    audio_output_buffered_until_nsecs: u64,
) -> bool {
    let pending_audio_until_nsecs = pending_audio_range_nsecs
        .filter(|(start_nsecs, _)| {
            *start_nsecs <= far_ahead_reference_nsecs.saturating_add(MAX_SEEK_AUDIO_LEAD_NSECS)
        })
        .map(|(_, end_nsecs)| end_nsecs);
    let contiguous_until_nsecs = pending_audio_until_nsecs
        .unwrap_or(audio_output_buffered_until_nsecs.max(far_ahead_reference_nsecs));
    frame_start_nsecs
        <= contiguous_until_nsecs.saturating_add(duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
}

fn keep_filling_audio_resume_waterline(
    waiting_for_output_resume: bool,
    contiguous_audio_backpressured: bool,
    audio_resume_waterline_below_input_suppression: bool,
) -> bool {
    waiting_for_output_resume
        && !contiguous_audio_backpressured
        && audio_resume_waterline_below_input_suppression
}

fn decoded_audio_frame_drops_before_rebuffer_audio_sync_drop_before(
    buffered_until_nsecs: u64,
    drop_before_timeline_nsecs: u64,
) -> bool {
    buffered_until_nsecs <= drop_before_timeline_nsecs
}

#[allow(clippy::too_many_arguments)]
fn service_decoded_audio_frame(
    decoded_frame: AudioDecodedFrame,
    audio_time_base: ffi::AVRational,
    control: &FfmpegControl,
    audio_output: Option<&AudioOutput>,
    audio_clock: &mut TimestampMapper,
    current_start_position_nsecs: u64,
    dropped_audio_frames_before_start_count: &mut u64,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<DecodedAudioFrameServiceStatus, String> {
    if control.has_pending_seek() {
        return Ok(DecodedAudioFrameServiceStatus::StopDrain);
    }
    let Some(output) = audio_output else {
        return Ok(DecodedAudioFrameServiceStatus::ContinueDrain);
    };

    let raw_timestamp = decoded_frame.raw_timestamp;
    let audio = decoded_frame.audio;
    let timestamp = audio_clock.map_contiguous(
        raw_timestamp,
        audio_time_base,
        audio.duration_nsecs,
        PENDING_AUDIO_CONTINUITY_TOLERANCE,
    );
    let buffered_until_nsecs = timestamp
        .timeline_nsecs
        .saturating_add(audio.duration_nsecs);
    let output_snapshot = output_scheduler.snapshot();
    if let Some(drop_before_timeline_nsecs) =
        output_scheduler.audio_sync_drop_before_timeline_nsecs()
        && decoded_audio_frame_drops_before_rebuffer_audio_sync_drop_before(
            buffered_until_nsecs,
            drop_before_timeline_nsecs,
        )
    {
        tracing::debug!(
            session_id = ?session_id,
            raw_timestamp,
            timeline_nsecs = timestamp.timeline_nsecs,
            buffered_until_nsecs,
            drop_before_timeline_nsecs,
            output_state = ?output_snapshot.state,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            rebuffering = output_snapshot.rebuffering,
            "dropping FFmpeg audio frame before rebuffer audio sync drop-before"
        );
        return Ok(DecodedAudioFrameServiceStatus::ContinueDrain);
    }
    let far_ahead_reference_nsecs =
        output_scheduler.audio_far_ahead_reference_timeline_nsecs(current_start_position_nsecs);
    if (output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
        && timestamp.timeline_nsecs
            > far_ahead_reference_nsecs.saturating_add(MAX_SEEK_AUDIO_LEAD_NSECS)
    {
        let audio_snapshot = output.snapshot()?;
        let frame_is_contiguous = far_ahead_audio_frame_is_contiguous(
            timestamp.timeline_nsecs,
            far_ahead_reference_nsecs,
            output_scheduler.pending_audio_contiguous_range_nsecs(),
            audio_snapshot.buffered_until_timeline_nsecs,
        );
        if frame_is_contiguous {
            tracing::debug!(
                session_id = ?session_id,
                raw_timestamp,
                timeline_nsecs = timestamp.timeline_nsecs,
                buffered_until_nsecs,
                current_start_position_nsecs,
                far_ahead_reference_nsecs,
                audio_lead_ms = timestamp
                    .timeline_nsecs
                    .saturating_sub(far_ahead_reference_nsecs) as f64
                    / 1_000_000.0,
                output_state = ?output_snapshot.state,
                first_video_frame_pending = output_snapshot.first_video_frame_pending,
                rebuffering = output_snapshot.rebuffering,
                queued_video_frames = output_snapshot.queued_video_frames,
                audio_output_pending_ms = audio_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
                "buffering contiguous FFmpeg audio frame at seek/rebuffer lead limit and stopping drain"
            );
        } else {
            let rebuffer_audio_realign_request = output_scheduler
                .observe_rebuffer_far_ahead_audio_frame(
                    timestamp.timeline_nsecs,
                    current_start_position_nsecs,
                    Some(audio_snapshot.total_pending_nsecs),
                    true,
                    session_id,
                    "decoded_audio_continuity_gap",
                );
            tracing::debug!(
                session_id = ?session_id,
                raw_timestamp,
                timeline_nsecs = timestamp.timeline_nsecs,
                current_start_position_nsecs,
                far_ahead_reference_nsecs,
                audio_lead_ms = timestamp
                    .timeline_nsecs
                    .saturating_sub(far_ahead_reference_nsecs) as f64
                    / 1_000_000.0,
                output_state = ?output_snapshot.state,
                first_video_frame_pending = output_snapshot.first_video_frame_pending,
                rebuffering = output_snapshot.rebuffering,
                queued_video_frames = output_snapshot.queued_video_frames,
                audio_output_pending_ms = audio_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
                rebuffer_audio_realign_target_nsecs =
                    ?rebuffer_audio_realign_request.map(|request| request.target_timeline_nsecs),
                rebuffer_audio_realign_drop_count =
                    ?rebuffer_audio_realign_request.map(|request| request.far_ahead_drop_count),
                "dropping discontinuous FFmpeg audio frame and stopping drain for reader realign"
            );
            return Ok(DecodedAudioFrameServiceStatus::StopDrain);
        }
    }
    output_scheduler.clear_rebuffer_far_ahead_audio_observation();
    if timestamp.timeline_nsecs < current_start_position_nsecs {
        *dropped_audio_frames_before_start_count =
            (*dropped_audio_frames_before_start_count).saturating_add(1);
        if *dropped_audio_frames_before_start_count == 1 {
            tracing::trace!(
                dropped_audio_frames_before_start = *dropped_audio_frames_before_start_count,
                raw_timestamp,
                timeline_nsecs = timestamp.timeline_nsecs,
                current_start_position_nsecs,
                output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                "dropping FFmpeg audio frame before playback start"
            );
        } else if (*dropped_audio_frames_before_start_count).is_multiple_of(60) {
            tracing::debug!(
                dropped_audio_frames_before_start = *dropped_audio_frames_before_start_count,
                raw_timestamp,
                timeline_nsecs = timestamp.timeline_nsecs,
                current_start_position_nsecs,
                output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
                "dropping FFmpeg audio frame before playback start"
            );
        }
        return Ok(DecodedAudioFrameServiceStatus::ContinueDrain);
    }

    output_scheduler.push_decoded_audio_or_buffer(
        output,
        control,
        audio,
        timestamp.timeline_nsecs,
        buffered_until_nsecs,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;
    output_scheduler.flush_pending_start_audio_if_ready(
        output,
        control,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;
    let audio_snapshot = output.snapshot()?;
    output_scheduler.clear_audio_sync_drop_before_if_covered(
        Some(audio_snapshot),
        session_id,
        "decoded_audio_frame",
    );
    if (output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
        && timestamp.timeline_nsecs
            > far_ahead_reference_nsecs.saturating_add(MAX_SEEK_AUDIO_LEAD_NSECS)
    {
        Ok(DecodedAudioFrameServiceStatus::StopDrain)
    } else {
        Ok(DecodedAudioFrameServiceStatus::ContinueDrain)
    }
}

#[derive(Clone, Copy, Default)]
struct ReadyAudioDecodeOutputTiming {
    pending_audio_backpressure: Duration,
    poll_frame: Duration,
    service_frame: Duration,
    poll_packet_status: Duration,
    packet_result: Duration,
    iterations: u64,
    decoded_frames: u64,
    completed_packets: u64,
}

fn log_ready_audio_decode_output_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    timing: ReadyAudioDecodeOutputTiming,
    made_progress: bool,
) {
    tracing::trace!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        pending_audio_backpressure_ms =
            timing.pending_audio_backpressure.as_secs_f64() * 1000.0,
        poll_frame_ms = timing.poll_frame.as_secs_f64() * 1000.0,
        service_frame_ms = timing.service_frame.as_secs_f64() * 1000.0,
        poll_packet_status_ms = timing.poll_packet_status.as_secs_f64() * 1000.0,
        packet_result_ms = timing.packet_result.as_secs_f64() * 1000.0,
        iterations = timing.iterations,
        decoded_frames = timing.decoded_frames,
        completed_packets = timing.completed_packets,
        made_progress,
        "FFmpeg ready audio decode output drain timing"
    );
    if total < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.pending_audio_backpressure < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.poll_frame < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.service_frame < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.poll_packet_status < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.packet_result < DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        pending_audio_backpressure_ms =
            timing.pending_audio_backpressure.as_secs_f64() * 1000.0,
        poll_frame_ms = timing.poll_frame.as_secs_f64() * 1000.0,
        service_frame_ms = timing.service_frame.as_secs_f64() * 1000.0,
        poll_packet_status_ms = timing.poll_packet_status.as_secs_f64() * 1000.0,
        packet_result_ms = timing.packet_result.as_secs_f64() * 1000.0,
        iterations = timing.iterations,
        decoded_frames = timing.decoded_frames,
        completed_packets = timing.completed_packets,
        made_progress,
        "FFmpeg ready audio decode output drain completed slowly"
    );
}

#[allow(clippy::too_many_arguments, clippy::while_let_loop)]
pub(super) fn drain_ready_audio_decode_output(
    mut audio_decode_pipeline: Option<&mut AudioDecodePipeline>,
    audio_output: Option<&AudioOutput>,
    audio_clock: &mut TimestampMapper,
    current_start_position_nsecs: u64,
    dropped_audio_frames_before_start_count: &mut u64,
    output_scheduler: &mut PlaybackOutputScheduler,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    let started_at = Instant::now();
    let mut timing = ReadyAudioDecodeOutputTiming::default();
    let mut made_progress = false;
    loop {
        timing.iterations = timing.iterations.saturating_add(1);
        let Some(worker) = audio_decode_pipeline.as_deref_mut() else {
            break;
        };
        let Some(front_generation) = worker.front_generation() else {
            break;
        };
        let stage_started_at = Instant::now();
        if output_scheduler.pending_start_audio_backpressured() {
            let Some(output) = audio_output else {
                timing.pending_audio_backpressure += stage_started_at.elapsed();
                break;
            };
            output_scheduler.flush_pending_start_audio_if_ready(
                output,
                control,
                session_id,
                vo_queue,
                frame_presented,
                position_reporter,
                event_tx,
                subtitle_pipeline,
                buffered_reporter,
            )?;
            if output_scheduler.pending_start_audio_backpressured() {
                let worker_snapshot = worker.snapshot();
                let audio_snapshot = audio_output.and_then(|output| output.snapshot().ok());
                if output_scheduler.waiting_for_output_resume() {
                    output_scheduler.discard_stale_pending_audio_before_output_resume(
                        audio_snapshot,
                        worker_snapshot.queued_duration_nsecs,
                        worker_snapshot.in_flight_packets,
                        current_start_position_nsecs,
                        session_id,
                    );
                }
                let keep_filling_resume_waterline = keep_filling_audio_resume_waterline(
                    output_scheduler.waiting_for_output_resume(),
                    output_scheduler.output_wait_audio_input_backpressured(),
                    output_scheduler.audio_resume_waterline_below_input_suppression(
                        audio_snapshot,
                        worker_snapshot.queued_duration_nsecs,
                        worker_snapshot.in_flight_packets,
                        current_start_position_nsecs,
                    ),
                );
                if !keep_filling_resume_waterline {
                    timing.pending_audio_backpressure += stage_started_at.elapsed();
                    break;
                }
                // Filling the waterline requires draining a decoded frame below;
                // looping back here without consuming one cannot make progress.
            }
        }
        timing.pending_audio_backpressure += stage_started_at.elapsed();
        let audio_time_base = worker.info().time_base;

        let stage_started_at = Instant::now();
        let decoded_frame = worker.poll_frame(front_generation)?;
        timing.poll_frame += stage_started_at.elapsed();
        if let Some(decoded_frame) = decoded_frame {
            made_progress = true;
            timing.decoded_frames = timing.decoded_frames.saturating_add(1);
            let stage_started_at = Instant::now();
            let service_status = service_decoded_audio_frame(
                decoded_frame,
                audio_time_base,
                control,
                audio_output,
                audio_clock,
                current_start_position_nsecs,
                dropped_audio_frames_before_start_count,
                output_scheduler,
                session_id,
                vo_queue,
                frame_presented,
                position_reporter,
                event_tx,
                subtitle_pipeline,
                buffered_reporter,
            )?;
            timing.service_frame += stage_started_at.elapsed();
            if control.has_pending_seek()
                || service_status == DecodedAudioFrameServiceStatus::StopDrain
            {
                break;
            }
            continue;
        }

        let stage_started_at = Instant::now();
        let packet_status = worker.poll_packet_status(front_generation)?;
        timing.poll_packet_status += stage_started_at.elapsed();
        let Some(status) = packet_status else {
            break;
        };
        let pending_packet = worker
            .pop_completed_packet()
            .expect("front audio decode packet exists for status");
        made_progress = true;
        timing.completed_packets = timing.completed_packets.saturating_add(1);
        if !status.drained && status.elapsed >= DECODE_PACKET_SLOW_LOG_AFTER {
            let audio_decode_snapshot = worker.snapshot();
            let audio_output_snapshot = audio_output.and_then(|output| output.snapshot().ok());
            let output_snapshot = output_scheduler.snapshot();
            tracing::debug!(
                session_id = ?session_id,
                packet_pts = ?pending_packet.packet.best_timestamp(),
                packet_bytes = pending_packet.packet.byte_len(),
                decoded_audio_frames = status.decoded_frames,
                elapsed_ms = status.elapsed.as_secs_f64() * 1000.0,
                pending_audio_ms = audio_output_snapshot
                    .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
                audio_output_shared_ms = audio_output_snapshot
                    .map(|snapshot| snapshot.shared_pending_nsecs as f64 / 1_000_000.0),
                audio_output_queue_ms = audio_output_snapshot
                    .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
                queued_video_frames = output_snapshot.queued_video_frames,
                queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
                decoded_video_range = ?output_snapshot.queued_video_range_nsecs,
                audio_decode_state = ?audio_decode_snapshot.state,
                audio_decode_queued_frames = audio_decode_snapshot.queued_frames,
                audio_decode_queued_ms =
                    audio_decode_snapshot.queued_duration_nsecs as f64 / 1_000_000.0,
                audio_decode_in_flight_packets = audio_decode_snapshot.in_flight_packets,
                audio_decode_completed_packets = audio_decode_snapshot.completed_packets,
                "FFmpeg audio decode packet completed slowly"
            );
        }
        let stage_started_at = Instant::now();
        status.result?;
        timing.packet_result += stage_started_at.elapsed();
    }
    log_ready_audio_decode_output_timing(session_id, started_at.elapsed(), timing, made_progress);
    Ok(made_progress)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process_audio_decode_drain_result(
    audio_drain_result: AudioDecodePacketResult,
    audio_time_base: Option<ffi::AVRational>,
    control: &FfmpegControl,
    audio_output: Option<&AudioOutput>,
    audio_clock: &mut TimestampMapper,
    current_start_position_nsecs: u64,
    dropped_audio_frames_before_start_count: &mut u64,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<(), String> {
    let AudioDecodePacketResult {
        frames,
        result,
        decoded_frames,
        elapsed,
    } = audio_drain_result;
    let mut process_result = Ok(());
    if let Some(audio_time_base) = audio_time_base {
        for decoded_frame in frames {
            if let Err(error) = service_decoded_audio_frame(
                decoded_frame,
                audio_time_base,
                control,
                audio_output,
                audio_clock,
                current_start_position_nsecs,
                dropped_audio_frames_before_start_count,
                output_scheduler,
                session_id,
                vo_queue,
                frame_presented,
                position_reporter,
                event_tx,
                subtitle_pipeline,
                buffered_reporter,
            ) {
                process_result = Err(error);
                break;
            }
        }
    }
    if elapsed >= DECODE_PACKET_SLOW_LOG_AFTER {
        tracing::debug!(
            session_id = ?session_id,
            decoded_audio_frames = decoded_frames,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            "FFmpeg audio decoder drain completed slowly"
        );
    }
    process_result.and(result)
}

#[cfg(test)]
mod tests {
    use super::{
        decoded_audio_frame_drops_before_rebuffer_audio_sync_drop_before as drops_before_sync_watermark,
        far_ahead_audio_frame_is_contiguous, keep_filling_audio_resume_waterline,
    };

    #[test]
    fn decoded_audio_frame_drops_before_rebuffer_audio_sync_drop_before() {
        let drop_before_timeline_nsecs = 24_000_000_000;

        assert!(drops_before_sync_watermark(
            640_000_000,
            drop_before_timeline_nsecs
        ));
        assert!(drops_before_sync_watermark(
            2_000_000_000,
            drop_before_timeline_nsecs
        ));
        assert!(drops_before_sync_watermark(
            10_000_000_000,
            drop_before_timeline_nsecs
        ));
        assert!(!drops_before_sync_watermark(
            24_000_000_001,
            drop_before_timeline_nsecs
        ));
    }

    #[test]
    fn far_ahead_boundary_frame_remains_contiguous_with_pending_audio_tail() {
        assert!(far_ahead_audio_frame_is_contiguous(
            204_567_800_334,
            202_549_751_669,
            Some((202_570_884_290, 204_567_800_334)),
            202_549_751_669,
        ));
    }

    #[test]
    fn far_ahead_frame_after_reader_gap_requires_realign() {
        assert!(!far_ahead_audio_frame_is_contiguous(
            210_535_328_512,
            204_583_333_333,
            None,
            204_567_800_334,
        ));
    }

    #[test]
    fn contiguous_startup_audio_backpressure_stops_waterline_drain() {
        assert!(!keep_filling_audio_resume_waterline(true, true, true));
        assert!(keep_filling_audio_resume_waterline(true, false, true));
    }
}
