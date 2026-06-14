use super::audio_decode_pipeline::AudioDecodePipeline;
use super::audio_decode_worker::{AudioDecodePacketResult, AudioDecodedFrame};
use super::*;

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
    let timestamp = audio_clock.map(raw_timestamp, audio_time_base);
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

    let audio = decoded_frame.audio;
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
    let mut made_progress = false;
    loop {
        let Some(worker) = audio_decode_pipeline.as_deref_mut() else {
            break;
        };
        let Some(front_generation) = worker.front_generation() else {
            break;
        };
        if output_scheduler.pending_start_audio_backpressured() {
            let Some(output) = audio_output else {
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
                break;
            }
        }
        let audio_time_base = worker.info().time_base;

        if let Some(decoded_frame) = worker.poll_frame(front_generation)? {
            made_progress = true;
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
            if control.has_pending_seek() {
                break;
            }
            continue;
        }

        let Some(status) = worker.poll_packet_status(front_generation)? else {
            break;
        };
        let pending_packet = worker
            .pop_completed_packet()
            .expect("front audio decode packet exists for status");
        made_progress = true;
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
        status.result?;
    }
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
