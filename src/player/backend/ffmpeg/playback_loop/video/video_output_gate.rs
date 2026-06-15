use super::audio_output_gate::{
    DelayedAudioStartSilencePolicy, drain_audio_clocked_decoded_video_step,
    flush_pending_start_audio, service_audio_clocked_video_queue,
};
use super::output_gate::{
    OutputGateResumeStatus, PlaybackOutputScheduler, service_output_gate_resume_if_ready,
};
use super::output_rebuffer::PlaybackOutputState;
use super::playback_block::PlaybackBlockReason;
use super::*;
use crate::player::render_host::VideoOutputQueueAdmission;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioClockedVideoDrainStatus {
    Drained,
    WaitingAudio { made_progress: bool },
    Interrupted,
}

impl AudioClockedVideoDrainStatus {
    pub(in crate::player::backend::ffmpeg) fn made_progress(self) -> bool {
        matches!(
            self,
            Self::WaitingAudio {
                made_progress: true
            }
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn admit_decoded_video_frame_to_vo(
    frame: DecodedFrame,
    session_id: PlaybackSessionId,
    timeline_nsecs: u64,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) {
    let started_at = Instant::now();
    let admission = vo_queue.admit_presented_frame(session_id, frame);
    let admission_elapsed = started_at.elapsed();
    log_vo_admission_timing(session_id, timeline_nsecs, admission_elapsed, admission);
    let backpressure = admission.before.render_backpressure;
    if admission.dropped_backlogged() {
        tracing::debug!(
            blocked_on = PlaybackBlockReason::RenderWorker.as_str(),
            pts = timeline_nsecs,
            vo_queued_frames = admission.after.queued_frames,
            vo_queue_capacity = admission.after.queue_capacity,
            vo_dropped_frames = admission.after.dropped_frames,
            pending_render_requests = backpressure.pending_requests,
            render_avg_ms = backpressure.average_render_nsecs as f64 / 1_000_000.0,
            render_last_ms = backpressure.last_render_nsecs as f64 / 1_000_000.0,
            "VO dropped non-key video frame because rendering is backlogged"
        );
        return;
    }
    if !admission.accepted() {
        tracing::debug!(
            session_id = ?session_id,
            pts = timeline_nsecs,
            "dropped FFmpeg video frame for inactive playback session"
        );
        return;
    }
    let count = FFMPEG_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(60) {
        tracing::debug!(
            frame_count = count,
            pts = timeline_nsecs,
            vo_replaced_pending = admission.replaced_pending_frame,
            vo_push_result = ?admission.result,
            vo_queued_frames = admission.after.queued_frames,
            vo_queue_capacity = admission.after.queue_capacity,
            vo_dropped_frames = admission.after.dropped_frames,
            vo_dropped_on_push = admission.result.dropped_oldest(),
            render_backlogged = admission.before.render_backlogged(),
            render_rendering = backpressure.rendering,
            pending_render_requests = backpressure.pending_requests,
            render_last_ms = backpressure.last_render_nsecs as f64 / 1_000_000.0,
            render_avg_ms = backpressure.average_render_nsecs as f64 / 1_000_000.0,
            "presented FFmpeg video frame"
        );
    }
    frame_presented.store(true, Ordering::Relaxed);
    position_reporter.report(timeline_nsecs, session_id, event_tx);
}

fn log_vo_admission_timing(
    session_id: PlaybackSessionId,
    timeline_nsecs: u64,
    elapsed: Duration,
    admission: VideoOutputQueueAdmission,
) {
    tracing::trace!(
        session_id = ?session_id,
        pts = timeline_nsecs,
        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
        result = ?admission.result,
        before_queued_frames = admission.before.queued_frames,
        after_queued_frames = admission.after.queued_frames,
        vo_queue_capacity = admission.after.queue_capacity,
        vo_dropped_frames = admission.after.dropped_frames,
        replaced_pending_frame = admission.replaced_pending_frame,
        render_backlogged = admission.before.render_backlogged(),
        pending_render_requests = admission.before.render_backpressure.pending_requests,
        render_last_ms = admission.before.render_backpressure.last_render_nsecs as f64 / 1_000_000.0,
        render_avg_ms = admission.before.render_backpressure.average_render_nsecs as f64 / 1_000_000.0,
        "FFmpeg VO admission timing"
    );
    if elapsed < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        pts = timeline_nsecs,
        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
        result = ?admission.result,
        before_queued_frames = admission.before.queued_frames,
        after_queued_frames = admission.after.queued_frames,
        vo_queue_capacity = admission.after.queue_capacity,
        vo_dropped_frames = admission.after.dropped_frames,
        replaced_pending_frame = admission.replaced_pending_frame,
        render_backlogged = admission.before.render_backlogged(),
        pending_render_requests = admission.before.render_backpressure.pending_requests,
        render_last_ms = admission.before.render_backpressure.last_render_nsecs as f64 / 1_000_000.0,
        render_avg_ms = admission.before.render_backpressure.average_render_nsecs as f64 / 1_000_000.0,
        "FFmpeg VO admission completed slowly"
    );
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_audio_clocked_video_queue_if_playing(
    output: Option<&AudioOutput>,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    let Some(output) = output else {
        return Ok(false);
    };
    if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        || output_scheduler.playback_output_state.rebuffering()
    {
        return Ok(false);
    }
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
    service_audio_clocked_video_queue(
        output,
        control,
        &mut output_scheduler.scheduled_video_queue,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_decode_backpressure_step(
    scheduler: &PlaybackScheduler,
    output: Option<&AudioOutput>,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    let audio_progressed = service_audio_clocked_video_queue_if_playing(
        output,
        control,
        output_scheduler,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;
    let video_progressed = service_video_clocked_video_queue_if_no_audio(
        scheduler,
        control,
        output_scheduler,
        output.is_some(),
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    );
    Ok(audio_progressed || video_progressed)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_audio_clocked_video_drain_step(
    output: &AudioOutput,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
) -> std::result::Result<AudioClockedVideoDrainStatus, String> {
    drain_audio_clocked_decoded_video_step(
        &mut output_scheduler.scheduled_video_queue,
        output,
        control,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        |played_until| {
            subtitle_pipeline.update_overlay(played_until, session_id, event_tx);
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_video_clocked_video_queue(
    scheduler: &PlaybackScheduler,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> bool {
    let mut presented_video_frame = false;
    while output_scheduler
        .scheduled_video_queue
        .front_ready_for(scheduler)
    {
        if control.should_interrupt() {
            break;
        }
        let Some(frame) = output_scheduler.scheduled_video_queue.pop_front() else {
            break;
        };
        let timeline_nsecs = frame.timeline_nsecs;
        let duration_nsecs = frame.duration_nsecs;
        subtitle_pipeline.update_overlay(timeline_nsecs, session_id, event_tx);
        present_video_frame_to_vo(
            frame.frame,
            timeline_nsecs,
            Some(timeline_nsecs.saturating_add(duration_nsecs)),
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            buffered_reporter,
        );
        presented_video_frame = true;
    }
    presented_video_frame
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_video_clocked_video_queue_if_no_audio(
    scheduler: &PlaybackScheduler,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    has_audio_output: bool,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> bool {
    if has_audio_output
        || output_scheduler
            .playback_output_state
            .first_video_frame_pending()
    {
        return false;
    }
    service_video_clocked_video_queue(
        scheduler,
        control,
        output_scheduler,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn present_video_frame_to_vo(
    frame: DecodedFrame,
    timeline_nsecs: u64,
    buffered_until_nsecs: Option<u64>,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    buffered_reporter: &mut BufferedReporter,
) {
    admit_decoded_video_frame_to_vo(
        frame,
        session_id,
        timeline_nsecs,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
    );
    if let Some(buffered_until_nsecs) = buffered_until_nsecs {
        buffered_reporter.report_video_timeline_nsecs(buffered_until_nsecs, session_id, event_tx);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PresentedQueuedVideoFrame {
    pub(in crate::player::backend::ffmpeg) timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) duration_nsecs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DecodedVideoAdmissionStatus {
    Continue,
    Stop,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn present_first_queued_video_frame<F>(
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    before_present: F,
) -> Option<PresentedQueuedVideoFrame>
where
    F: FnOnce(PresentedQueuedVideoFrame, &PlaybackOutputScheduler),
{
    let frame = output_scheduler.scheduled_video_queue.pop_front()?;
    let timing = PresentedQueuedVideoFrame {
        timeline_nsecs: frame.timeline_nsecs,
        duration_nsecs: frame.duration_nsecs,
    };
    before_present(timing, output_scheduler);
    admit_decoded_video_frame_to_vo(
        frame.frame,
        session_id,
        timing.timeline_nsecs,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
    );
    output_scheduler.set_state(PlaybackOutputState::Playing);
    Some(timing)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn queue_decoded_video_frame(
    output_scheduler: &mut PlaybackOutputScheduler,
    frame: DecodedFrame,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    report_buffered_on_queue: bool,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
    buffered_reporter: &mut BufferedReporter,
) {
    output_scheduler
        .scheduled_video_queue
        .push_queued(QueuedVideoFrame {
            frame,
            timeline_nsecs,
            duration_nsecs,
        });
    if report_buffered_on_queue {
        buffered_reporter.report_video_timeline_nsecs(
            timeline_nsecs.saturating_add(duration_nsecs),
            session_id,
            event_tx,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_audio_clocked_decoded_video_frame<F>(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    scheduler: &mut PlaybackScheduler,
    frame: DecodedFrame,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    current_start_position_nsecs: &mut u64,
    demux_watermark: F,
) -> std::result::Result<DecodedVideoAdmissionStatus, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    subtitle_pipeline.update_overlay_from_audio_clock(output, session_id, event_tx)?;

    let first_video_frame_pending_before_queue = output_scheduler.first_video_frame_pending;
    if first_video_frame_pending_before_queue
        && output_scheduler.scheduled_video_queue.is_empty()
        && timeline_nsecs > *current_start_position_nsecs
    {
        tracing::debug!(
            previous_start_position_nsecs = *current_start_position_nsecs,
            first_video_frame_nsecs = timeline_nsecs,
            "realigning FFmpeg playback start to first decoded video frame"
        );
        *current_start_position_nsecs = timeline_nsecs;
        scheduler.reset(timeline_nsecs);
        output.reset_clock(timeline_nsecs);
        subtitle_pipeline.reset_cues_for_position(timeline_nsecs);
        buffered_reporter.reset_to(nsecs_to_seconds(timeline_nsecs), session_id, event_tx);
    }
    queue_decoded_video_frame(
        output_scheduler,
        frame,
        timeline_nsecs,
        duration_nsecs,
        true,
        session_id,
        event_tx,
        buffered_reporter,
    );
    match service_output_gate_resume_if_ready(
        output_scheduler,
        Some(output),
        None,
        control,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
        timeline_nsecs,
        current_start_position_nsecs,
        scheduler,
        false,
        demux_watermark,
    )? {
        OutputGateResumeStatus::Resumed => return Ok(DecodedVideoAdmissionStatus::Stop),
        OutputGateResumeStatus::Waiting | OutputGateResumeStatus::WaitingForDemux => {
            return Ok(DecodedVideoAdmissionStatus::Stop);
        }
        OutputGateResumeStatus::Idle => {}
    }
    if !output_scheduler.pending_start_audio.is_empty() {
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
    }
    service_audio_clocked_video_queue(
        output,
        control,
        &mut output_scheduler.scheduled_video_queue,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
    )?;
    Ok(DecodedVideoAdmissionStatus::Continue)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_audio_clocked_drain_decoded_video_frame(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    frame: DecodedFrame,
    timeline_nsecs: u64,
    duration_nsecs: u64,
) -> std::result::Result<DecodedVideoAdmissionStatus, String> {
    subtitle_pipeline.update_overlay_from_audio_clock(output, session_id, event_tx)?;

    if output_scheduler.first_video_frame_pending {
        present_video_frame_to_vo(
            frame,
            timeline_nsecs,
            Some(timeline_nsecs.saturating_add(duration_nsecs)),
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            buffered_reporter,
        );
        output_scheduler.set_state(PlaybackOutputState::Playing);
        let audio_flush_until_timeline_nsecs = timeline_nsecs.saturating_add(duration_nsecs);
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            DelayedAudioStartSilencePolicy::Allow,
            control,
            &mut output_scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        return Ok(DecodedVideoAdmissionStatus::Stop);
    }

    queue_decoded_video_frame(
        output_scheduler,
        frame,
        timeline_nsecs,
        duration_nsecs,
        true,
        session_id,
        event_tx,
        buffered_reporter,
    );
    if !output_scheduler.pending_start_audio.is_empty() {
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
    }
    service_audio_clocked_video_queue(
        output,
        control,
        &mut output_scheduler.scheduled_video_queue,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
    )?;
    Ok(DecodedVideoAdmissionStatus::Continue)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_video_clocked_decoded_video_frame(
    scheduler: &mut PlaybackScheduler,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    frame: DecodedFrame,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    current_start_position_nsecs: &mut u64,
    decoded_video_frame_count: u64,
) -> DecodedVideoAdmissionStatus {
    subtitle_pipeline.update_overlay(timeline_nsecs, session_id, event_tx);

    if output_scheduler.first_video_frame_pending {
        if timeline_nsecs > *current_start_position_nsecs {
            tracing::debug!(
                previous_start_position_nsecs = *current_start_position_nsecs,
                first_video_frame_nsecs = timeline_nsecs,
                "realigning FFmpeg playback start to first decoded video frame"
            );
            *current_start_position_nsecs = timeline_nsecs;
            scheduler.reset(timeline_nsecs);
            subtitle_pipeline.reset_cues_for_position(timeline_nsecs);
            buffered_reporter.reset_to(nsecs_to_seconds(timeline_nsecs), session_id, event_tx);
        }
        tracing::debug!(
            frame_count = decoded_video_frame_count,
            pts = timeline_nsecs,
            current_start_position_nsecs = *current_start_position_nsecs,
            "presenting first FFmpeg video frame after start gate"
        );
        present_video_frame_to_vo(
            frame,
            timeline_nsecs,
            Some(timeline_nsecs.saturating_add(duration_nsecs)),
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            buffered_reporter,
        );
        output_scheduler.set_state(PlaybackOutputState::Playing);
        return DecodedVideoAdmissionStatus::Stop;
    }
    output_scheduler.set_state(PlaybackOutputState::Playing);
    queue_decoded_video_frame(
        output_scheduler,
        frame,
        timeline_nsecs,
        duration_nsecs,
        false,
        session_id,
        event_tx,
        buffered_reporter,
    );
    let _ = service_video_clocked_video_queue(
        scheduler,
        control,
        output_scheduler,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    );
    DecodedVideoAdmissionStatus::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_clocked_video_drain_status_tracks_progress() {
        assert!(
            AudioClockedVideoDrainStatus::WaitingAudio {
                made_progress: true
            }
            .made_progress()
        );
        assert!(
            !AudioClockedVideoDrainStatus::WaitingAudio {
                made_progress: false
            }
            .made_progress()
        );
        assert!(!AudioClockedVideoDrainStatus::Drained.made_progress());
        assert!(!AudioClockedVideoDrainStatus::Interrupted.made_progress());
    }
}
