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
    SubtitlePipeline, TimestampMapper,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn service_decoded_audio_frame(
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
) -> std::result::Result<(), String> {
    if control.has_pending_seek() {
        return Ok(());
    }
    let Some(output) = audio_output else {
        return Ok(());
    };

    let raw_timestamp = decoded_frame.raw_timestamp;
    let audio = decoded_frame.audio;
    let timestamp = audio_clock.map_contiguous(
        raw_timestamp,
        audio_time_base,
        audio.duration_nsecs,
        PENDING_AUDIO_CONTINUITY_TOLERANCE,
    );
    if timestamp.timeline_nsecs < current_start_position_nsecs {
        *dropped_audio_frames_before_start_count =
            (*dropped_audio_frames_before_start_count).saturating_add(1);
        let output_snapshot = output_scheduler.snapshot();
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
        return Ok(());
    }

    let buffered_until_nsecs = timestamp
        .timeline_nsecs
        .saturating_add(audio.duration_nsecs);
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
    Ok(())
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
                timing.pending_audio_backpressure += stage_started_at.elapsed();
                break;
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
            service_decoded_audio_frame(
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
            if control.has_pending_seek() {
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
