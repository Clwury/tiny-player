use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use crate::player::{
    backend::{BackendEvent, BackendEventKind},
    render_host::{DecodedFrame, PlaybackSessionId, VideoOutputQueue, VideoOutputQueueAdmission},
};

use super::audio_output_gate::{
    DelayedAudioStartSilencePolicy, drain_audio_clocked_decoded_video_step,
    flush_pending_start_audio, service_audio_clocked_video_queue,
};
use super::output_gate::{
    OutputGateResumeStatus, PlaybackOutputScheduler, service_output_gate_resume_if_ready,
};
use super::output_rebuffer::PlaybackOutputState;
use super::playback_block::PlaybackBlockReason;
use super::video_decode_pipeline::HevcDecodeChainStats;
use super::video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState};
use super::{
    AudioClockMode, AudioOutput, AudioOutputSnapshot, BufferedReporter, DemuxReaderWatermark,
    FFMPEG_FRAME_COUNT, FfmpegControl, OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    PlaybackScheduler, PositionReporter, QueuedVideoFrame, SubtitlePipeline, nsecs_to_seconds,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioClockedVideoDrainStatus {
    Drained,
    WaitingAudio { made_progress: bool },
    Interrupted,
}

fn report_first_video_frame_presented(
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) {
    let _ = event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::Buffering(false),
    ));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioClockAvailability {
    pub(in crate::player::backend::ffmpeg) has_audio_output: bool,
    pub(in crate::player::backend::ffmpeg) available: bool,
    pub(in crate::player::backend::ffmpeg) pending_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) buffered_until_timeline_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) underrun_active: bool,
}

pub(in crate::player::backend::ffmpeg) fn audio_clock_availability(
    output: Option<&AudioOutput>,
) -> std::result::Result<AudioClockAvailability, String> {
    let Some(output) = output else {
        return Ok(AudioClockAvailability {
            has_audio_output: false,
            available: false,
            pending_nsecs: None,
            buffered_until_timeline_nsecs: None,
            underrun_active: false,
        });
    };
    let snapshot = output.snapshot()?;
    let underrun_active = output.underrun_active();
    let pending_nsecs = snapshot.total_pending_nsecs;
    Ok(AudioClockAvailability {
        has_audio_output: true,
        available: pending_nsecs > 0 && !underrun_active,
        pending_nsecs: Some(pending_nsecs),
        buffered_until_timeline_nsecs: Some(snapshot.buffered_until_timeline_nsecs),
        underrun_active,
    })
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
) -> bool {
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
        return false;
    }
    if !admission.accepted() {
        tracing::debug!(
            session_id = ?session_id,
            pts = timeline_nsecs,
            "dropped FFmpeg video frame for inactive playback session"
        );
        return false;
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
    true
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
    audio_clock: AudioClockAvailability,
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
    let video_progressed = service_video_clocked_video_queue_if_audio_clock_unavailable(
        scheduler,
        control,
        output_scheduler,
        audio_clock,
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
pub(in crate::player::backend::ffmpeg) fn service_video_clocked_video_queue_if_audio_clock_unavailable(
    scheduler: &PlaybackScheduler,
    control: &FfmpegControl,
    output_scheduler: &mut PlaybackOutputScheduler,
    audio_clock: AudioClockAvailability,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> bool {
    if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        return false;
    }
    // Draining on the video clock while rebuffering advances the resume point
    // past the anchor, so refilling audio can never catch up and rejoin.
    if output_scheduler.playback_output_state.rebuffering() {
        return false;
    }
    if audio_clock.available {
        return false;
    }
    if !video_clock_drain_allowed_while_audio_unavailable(audio_clock) {
        tracing::debug!(
            session_id = ?session_id,
            audio_output_pending_ms =
                ?audio_clock.pending_nsecs.map(|pending| pending as f64 / 1_000_000.0),
            audio_output_buffered_until_timeline_nsecs =
                ?audio_clock.buffered_until_timeline_nsecs,
            queued_video_frames = output_scheduler.scheduled_video_queue.len(),
            queued_video_ms =
                output_scheduler.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
            "holding FFmpeg video clock while audio output is underrun"
        );
        return false;
    }
    if audio_clock.has_audio_output {
        tracing::debug!(
            session_id = ?session_id,
            audio_gap_recovery_active = output_scheduler.audio_gap_recovery_active(),
            audio_output_pending_ms =
                ?audio_clock.pending_nsecs.map(|pending| pending as f64 / 1_000_000.0),
            audio_output_buffered_until_timeline_nsecs =
                ?audio_clock.buffered_until_timeline_nsecs,
            audio_output_underrun = audio_clock.underrun_active,
            queued_video_frames = output_scheduler.scheduled_video_queue.len(),
            queued_video_ms =
                output_scheduler.scheduled_video_queue.duration_nsecs() as f64 / 1_000_000.0,
            queued_video_range = ?output_scheduler.scheduled_video_queue.range_nsecs(),
            "draining FFmpeg video queue with video clock while audio clock unavailable"
        );
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

fn video_clock_drain_allowed_while_audio_unavailable(audio_clock: AudioClockAvailability) -> bool {
    !audio_clock.has_audio_output || !audio_clock.underrun_active
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
    let admitted = admit_decoded_video_frame_to_vo(
        frame.frame,
        session_id,
        timing.timeline_nsecs,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
    );
    if admitted {
        report_first_video_frame_presented(session_id, event_tx);
    }
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
    audio_snapshot: Option<AudioOutputSnapshot>,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
    buffered_reporter: &mut BufferedReporter,
) {
    let before_queue_len = output_scheduler.scheduled_video_queue.len();
    let before_queue_duration_nsecs = output_scheduler.scheduled_video_queue.duration_nsecs();
    let before_queue_range = output_scheduler.scheduled_video_queue.range_nsecs();
    let before_queue_largest_gap_nsecs = output_scheduler.scheduled_video_queue.largest_gap_nsecs();
    let previous_frame_timing = output_scheduler.scheduled_video_queue.back_timing_nsecs();
    let previous_expected_next_nsecs =
        previous_frame_timing.map(|(pts, duration)| pts.saturating_add(duration));
    let previous_gap_nsecs = previous_expected_next_nsecs
        .map(|expected| i128::from(timeline_nsecs).saturating_sub(i128::from(expected)));
    let inserted_after_tail = previous_frame_timing.is_none_or(|(pts, _)| pts <= timeline_nsecs);
    let output_state = output_scheduler.playback_output_state;
    let first_video_frame_pending = output_scheduler.first_video_frame_pending;

    output_scheduler
        .scheduled_video_queue
        .push_queued(QueuedVideoFrame {
            frame,
            timeline_nsecs,
            duration_nsecs,
        });
    log_decoded_video_frame_queue_admission(
        session_id,
        timeline_nsecs,
        duration_nsecs,
        report_buffered_on_queue,
        output_state,
        first_video_frame_pending,
        before_queue_len,
        before_queue_duration_nsecs,
        before_queue_range,
        before_queue_largest_gap_nsecs,
        previous_frame_timing,
        previous_expected_next_nsecs,
        previous_gap_nsecs,
        inserted_after_tail,
        output_scheduler.scheduled_video_queue.len(),
        output_scheduler.scheduled_video_queue.duration_nsecs(),
        output_scheduler.scheduled_video_queue.range_nsecs(),
        output_scheduler.scheduled_video_queue.largest_gap_nsecs(),
        audio_snapshot.and_then(|snapshot| {
            output_scheduler
                .scheduled_video_queue
                .continuity_from_nsecs(snapshot.played_timeline_nsecs)
                .forward_nsecs
        }),
        audio_snapshot,
    );
    if report_buffered_on_queue {
        buffered_reporter.report_video_timeline_nsecs(
            timeline_nsecs.saturating_add(duration_nsecs),
            session_id,
            event_tx,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn log_decoded_video_frame_queue_admission(
    session_id: PlaybackSessionId,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    report_buffered_on_queue: bool,
    output_state: PlaybackOutputState,
    first_video_frame_pending: bool,
    before_queue_len: usize,
    before_queue_duration_nsecs: u64,
    before_queue_range: Option<(u64, u64)>,
    before_queue_largest_gap_nsecs: Option<u64>,
    previous_frame_timing: Option<(u64, u64)>,
    previous_expected_next_nsecs: Option<u64>,
    previous_gap_nsecs: Option<i128>,
    inserted_after_tail: bool,
    after_queue_len: usize,
    after_queue_duration_nsecs: u64,
    after_queue_range: Option<(u64, u64)>,
    after_queue_largest_gap_nsecs: Option<u64>,
    queued_video_contiguous_forward_nsecs: Option<u64>,
    audio_snapshot: Option<AudioOutputSnapshot>,
) {
    let previous_frame_pts_nsecs = previous_frame_timing.map(|(pts, _)| pts);
    let previous_frame_duration_nsecs = previous_frame_timing.map(|(_, duration)| duration);
    tracing::debug!(
        session_id = ?session_id,
        pts = timeline_nsecs,
        duration_nsecs,
        duration_ms = duration_nsecs as f64 / 1_000_000.0,
        report_buffered_on_queue,
        output_state = ?output_state,
        first_video_frame_pending,
        inserted_after_tail,
        previous_frame_pts_nsecs,
        previous_frame_duration_nsecs,
        previous_expected_next_nsecs,
        previous_gap_nsecs,
        previous_gap_ms = previous_gap_nsecs.map(|gap| gap as f64 / 1_000_000.0),
        before_queue_frames = before_queue_len,
        before_queue_ms = before_queue_duration_nsecs as f64 / 1_000_000.0,
        before_queue_range = ?before_queue_range,
        before_queue_largest_gap_ms =
            ?before_queue_largest_gap_nsecs.map(|gap| gap as f64 / 1_000_000.0),
        admitted_video_frame_count = after_queue_len.saturating_sub(before_queue_len),
        after_queue_frames = after_queue_len,
        after_queue_ms = after_queue_duration_nsecs as f64 / 1_000_000.0,
        after_queue_range = ?after_queue_range,
        after_queue_largest_gap_ms =
            ?after_queue_largest_gap_nsecs.map(|gap| gap as f64 / 1_000_000.0),
        queued_video_contiguous_forward_ms =
            ?queued_video_contiguous_forward_nsecs.map(|duration| duration as f64 / 1_000_000.0),
        audio_played_timeline_nsecs = audio_snapshot.map(|snapshot| snapshot.played_timeline_nsecs),
        audio_buffered_until_timeline_nsecs =
            audio_snapshot.map(|snapshot| snapshot.buffered_until_timeline_nsecs),
        audio_pending_ms =
            audio_snapshot.map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        audio_queue_ms =
            audio_snapshot.map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
        video_minus_audio_ms = audio_snapshot.map(|snapshot| {
            (i128::from(timeline_nsecs)
                .saturating_sub(i128::from(snapshot.played_timeline_nsecs)))
                as f64
                / 1_000_000.0
        }),
        "admitted decoded FFmpeg video frame into scheduled queue"
    );
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
    video_is_hevc: bool,
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
            "deferring FFmpeg playback start realign until first decoded video frame is presented"
        );
    }
    queue_decoded_video_frame(
        output_scheduler,
        frame,
        timeline_nsecs,
        duration_nsecs,
        true,
        output.snapshot().ok(),
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
        0,
        0,
        None,
        None,
        0,
        VideoDecodeWorkerSnapshot {
            state: VideoDecodeWorkerState::NeedPacket,
            queued_frames: 0,
            queue_capacity: 0,
            pending_input_packets: 0,
            pending_input_capacity: 0,
            in_flight_packets: 0,
            command_queue_capacity: 0,
            completed_packets: 0,
        },
        video_is_hevc,
        HevcDecodeChainStats::default(),
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
            AudioClockMode::SyncingVideo,
            DelayedAudioStartSilencePolicy::Skip,
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
        output.snapshot().ok(),
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
            subtitle_pipeline.realign_cues_for_position(timeline_nsecs);
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
        None,
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
    use std::sync::mpsc;

    use crate::player::{backend::BackendEventKind, render_host::PlaybackSessionId};

    use super::{
        AudioClockAvailability, AudioClockedVideoDrainStatus, report_first_video_frame_presented,
        video_clock_drain_allowed_while_audio_unavailable,
    };

    #[test]
    fn first_presented_video_frame_finishes_visible_buffering() {
        let (event_tx, event_rx) = mpsc::channel();
        let session_id = PlaybackSessionId(7);

        report_first_video_frame_presented(session_id, &event_tx);

        let event = event_rx.try_recv().expect("buffering completion event");
        assert_eq!(event.session_id, session_id);
        assert!(matches!(event.kind, BackendEventKind::Buffering(false)));
    }

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

    #[test]
    fn audio_underrun_holds_video_clocked_queue() {
        assert!(!video_clock_drain_allowed_while_audio_unavailable(
            AudioClockAvailability {
                has_audio_output: true,
                available: false,
                pending_nsecs: Some(0),
                buffered_until_timeline_nsecs: Some(24_716_997_309),
                underrun_active: true,
            }
        ));
        assert!(video_clock_drain_allowed_while_audio_unavailable(
            AudioClockAvailability {
                has_audio_output: false,
                available: false,
                pending_nsecs: None,
                buffered_until_timeline_nsecs: None,
                underrun_active: false,
            }
        ));
    }
}
