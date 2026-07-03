use std::sync::{atomic::AtomicBool, mpsc::Sender};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::video_frame_prepare_worker::PreparedVideoFrame;
use super::video_output_gate::{
    DecodedVideoAdmissionStatus, service_audio_clocked_decoded_video_frame,
    service_audio_clocked_drain_decoded_video_frame, service_video_clocked_decoded_video_frame,
};
use super::{
    AudioOutput, BufferedReporter, DemuxReaderWatermark, FfmpegControl, PlaybackOutputScheduler,
    PlaybackScheduler, PositionReporter, SubtitlePipeline,
};

pub(super) fn admit_prepared_video_frame<F>(
    context: PreparedVideoFrameAdmissionContext<'_, F>,
) -> std::result::Result<(), String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let PreparedVideoFrame {
        generation,
        frame,
        timeline_nsecs,
        duration_nsecs,
        ..
    } = context.prepared_frame;
    log_prepared_video_frame_admission_entry(
        context.session_id,
        generation,
        context.decoded_video_frame_count,
        timeline_nsecs,
        duration_nsecs,
        *context.current_start_position_nsecs,
        context.audio_output.is_some(),
        context.output_scheduler,
    );
    if let Some(output) = context.audio_output {
        if service_audio_clocked_decoded_video_frame(
            context.output_scheduler,
            output,
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            context.position_reporter,
            context.event_tx,
            context.subtitle_pipeline,
            context.buffered_reporter,
            context.scheduler,
            frame,
            timeline_nsecs,
            duration_nsecs,
            context.current_start_position_nsecs,
            context.video_is_hevc,
            context.demux_reader_watermark,
        )? == DecodedVideoAdmissionStatus::Stop
        {
            return Ok(());
        }
    } else if service_video_clocked_decoded_video_frame(
        context.scheduler,
        context.control,
        context.output_scheduler,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        context.position_reporter,
        context.event_tx,
        context.subtitle_pipeline,
        context.buffered_reporter,
        frame,
        timeline_nsecs,
        duration_nsecs,
        context.current_start_position_nsecs,
        context.decoded_video_frame_count,
    ) == DecodedVideoAdmissionStatus::Stop
    {
        return Ok(());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn log_prepared_video_frame_admission_entry(
    session_id: PlaybackSessionId,
    generation: u64,
    decoded_video_frame_count: u64,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    current_start_position_nsecs: u64,
    has_audio_output: bool,
    output_scheduler: &PlaybackOutputScheduler,
) {
    let output_snapshot = output_scheduler.snapshot();
    if decoded_video_frame_count != 1
        && !output_snapshot.first_video_frame_pending
        && !output_snapshot.rebuffering
        && output_snapshot.queued_video_frames > 0
    {
        return;
    }

    tracing::debug!(
        session_id = ?session_id,
        generation,
        decoded_video_frame_count,
        timeline_nsecs,
        duration_nsecs,
        duration_ms = duration_nsecs as f64 / 1_000_000.0,
        current_start_position_nsecs,
        has_audio_output,
        output_state = ?output_snapshot.state,
        output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_largest_gap_ms = ?output_snapshot
            .queued_video_largest_gap_nsecs
            .map(|gap| gap as f64 / 1_000_000.0),
        pending_start_audio_frames = output_snapshot.pending_start_audio_frames,
        pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
        output_rebuffer_anchor = ?output_snapshot.video_output_rebuffer_anchor,
        video_bootstrap_after_seek = output_snapshot.video_bootstrap_after_seek,
        "admitting prepared FFmpeg video frame into output gate"
    );
}

pub(super) struct PreparedVideoFrameAdmissionContext<'a, F>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    pub(super) prepared_frame: PreparedVideoFrame,
    pub(super) decoded_video_frame_count: u64,
    pub(super) scheduler: &'a mut PlaybackScheduler,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) output_scheduler: &'a mut PlaybackOutputScheduler,
    pub(super) buffered_reporter: &'a mut BufferedReporter,
    pub(super) control: &'a FfmpegControl,
    pub(super) session_id: PlaybackSessionId,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) position_reporter: &'a mut PositionReporter,
    pub(super) subtitle_pipeline: &'a mut SubtitlePipeline,
    pub(super) current_start_position_nsecs: &'a mut u64,
    pub(super) video_is_hevc: bool,
    pub(super) demux_reader_watermark: F,
}

pub(super) fn admit_drained_prepared_video_frame(
    context: DrainedPreparedVideoFrameAdmissionContext<'_>,
) -> std::result::Result<(), String> {
    let generation = context.prepared_frame.generation;
    let timeline_nsecs = context.prepared_frame.timeline_nsecs;
    let duration_nsecs = context.prepared_frame.duration_nsecs;
    log_prepared_video_frame_admission_entry(
        context.session_id,
        generation,
        context.decoded_video_frame_count,
        timeline_nsecs,
        duration_nsecs,
        *context.current_start_position_nsecs,
        context.audio_output.is_some(),
        context.output_scheduler,
    );
    if let Some(output) = context.audio_output {
        let _ = service_audio_clocked_drain_decoded_video_frame(
            context.output_scheduler,
            output,
            context.control,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            context.position_reporter,
            context.event_tx,
            context.subtitle_pipeline,
            context.buffered_reporter,
            context.prepared_frame.frame,
            timeline_nsecs,
            duration_nsecs,
        )?;
    } else {
        let _ = service_video_clocked_decoded_video_frame(
            context.scheduler,
            context.control,
            context.output_scheduler,
            context.session_id,
            context.vo_queue,
            context.frame_presented,
            context.position_reporter,
            context.event_tx,
            context.subtitle_pipeline,
            context.buffered_reporter,
            context.prepared_frame.frame,
            timeline_nsecs,
            duration_nsecs,
            context.current_start_position_nsecs,
            context.decoded_video_frame_count,
        );
    }
    Ok(())
}

pub(super) struct DrainedPreparedVideoFrameAdmissionContext<'a> {
    pub(super) prepared_frame: PreparedVideoFrame,
    pub(super) decoded_video_frame_count: u64,
    pub(super) scheduler: &'a mut PlaybackScheduler,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) output_scheduler: &'a mut PlaybackOutputScheduler,
    pub(super) buffered_reporter: &'a mut BufferedReporter,
    pub(super) control: &'a FfmpegControl,
    pub(super) session_id: PlaybackSessionId,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) position_reporter: &'a mut PositionReporter,
    pub(super) subtitle_pipeline: &'a mut SubtitlePipeline,
    pub(super) current_start_position_nsecs: &'a mut u64,
}
