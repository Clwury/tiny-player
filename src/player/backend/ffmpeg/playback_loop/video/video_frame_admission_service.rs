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
        frame,
        timeline_nsecs,
        duration_nsecs,
        ..
    } = context.prepared_frame;
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
    pub(super) demux_reader_watermark: F,
}

pub(super) fn admit_drained_prepared_video_frame(
    context: DrainedPreparedVideoFrameAdmissionContext<'_>,
) -> std::result::Result<(), String> {
    let timeline_nsecs = context.prepared_frame.timeline_nsecs;
    let duration_nsecs = context.prepared_frame.duration_nsecs;
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
