use super::audio_decode_pipeline::AudioDecodePipeline;
use super::video_decode_pipeline::VideoDecodePipeline;
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
    VideoFramePrepareEnqueueResult, VideoFramePrepareResult, VideoFramePrepareWorker,
};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodedVideoFrameStartAction {
    DropBeforeStart,
    Use { realign: bool },
}

pub(in crate::player::backend::ffmpeg) fn decoded_video_frame_start_action(
    frame_timeline_nsecs: u64,
    current_start_position_nsecs: u64,
    recovery_realign: bool,
) -> DecodedVideoFrameStartAction {
    if recovery_realign {
        return DecodedVideoFrameStartAction::Use { realign: true };
    }
    if frame_timeline_nsecs < current_start_position_nsecs {
        return DecodedVideoFrameStartAction::DropBeforeStart;
    }
    DecodedVideoFrameStartAction::Use { realign: false }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service_decoded_video_frame<F>(
    decoded_frame: VideoDecodedFrame,
    generation: u64,
    video_decode_pipeline: &VideoDecodePipeline,
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
        DecodedVideoFrameStartStatus::DroppedBeforeStart => return Ok(true),
        DecodedVideoFrameStartStatus::DroppedCorrupt => {
            return Err(CORRUPT_VIDEO_FRAME_RECOVERY_ERROR.to_string());
        }
    };
    let frame_pts = start_frame.frame_pts;
    let timeline_nsecs = start_frame.timeline_nsecs;

    match enqueue_decoded_video_frame_prepare(
        decoded_frame,
        generation,
        frame_pts,
        timeline_nsecs,
        video_frame_duration_nsecs,
        dovi_pipeline,
        video_frame_prepare_worker,
        &video_decode_pipeline.info().convert_context,
    )? {
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
    current_start_position_nsecs: &mut u64,
    mut demux_reader_watermark: F,
) -> std::result::Result<bool, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let mut made_progress = false;
    loop {
        let Some(front_generation) = video_decode_pipeline.front_generation() else {
            break;
        };

        if video_frame_prepare_worker.retry_pending_input()?
            == VideoFramePrepareEnqueueResult::InputFull
        {
            break;
        }

        if let Some(prepare_result) = video_frame_prepare_worker.poll_result(front_generation)? {
            log_prepared_video_frame_if_slow(
                &prepare_result,
                session_id,
                output_scheduler,
                audio_output,
            );
            made_progress = true;
            match prepare_result.result {
                Ok(prepared_frame) => {
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
                        demux_reader_watermark: &mut demux_reader_watermark,
                    })?
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

        if let Some(decoded_frame) = video_decode_pipeline.poll_frame(front_generation)? {
            made_progress = true;
            let frame_result = service_decoded_video_frame(
                decoded_frame,
                front_generation,
                video_decode_pipeline,
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

        if video_frame_prepare_worker.has_pending_for_generation(front_generation)? {
            break;
        }

        let Some(status) = video_decode_pipeline.poll_packet_status(front_generation)? else {
            break;
        };
        let pending_packet = video_decode_pipeline
            .pop_completed_packet()
            .expect("front video decode packet exists for status");
        made_progress = true;
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
        service_video_decode_recovery_result(VideoDecodeRecoveryServiceContext {
            result: status.result,
            packet: &pending_packet.packet,
            realign_after_decode_recovery: pending_packet.realign_after_decode_recovery,
            video_stream,
            playback_generation,
            video_decode_pipeline,
            audio_decode_pipeline: audio_decode_pipeline.as_deref_mut(),
            subtitle_pipeline,
            video_decode_recovery,
            output_scheduler,
            dovi_pipeline,
            control,
        })?;
    }
    Ok(made_progress)
}
