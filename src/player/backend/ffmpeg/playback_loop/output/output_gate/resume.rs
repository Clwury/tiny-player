use super::{
    AtomicBool, AudioClockMode, AudioClockResumeDecision, AudioOutput, BackendEvent,
    BufferedReporter, DelayedAudioStartSilencePolicy, DemuxPacketCache, DemuxReaderWatermark,
    Duration, FfmpegControl, InitialOutputSyncDecision, Instant,
    OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER, PlaybackOutputScheduler, PlaybackOutputState,
    PlaybackResumeWaterline, PlaybackScheduler, PlaybackSessionId, PositionReporter, Sender,
    SubtitlePipeline, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, VideoOutputQueue,
    audio_output_buffered_until_for_resume, audio_output_contiguous_start_timeline_nsecs,
    discard_decoded_video_before_output_gate_resume_if_ready, flush_pending_start_audio,
    initial_delayed_audio_start_timeline_nsecs,
    initial_playback_resume_waterline_after_stale_audio_preroll_wait, nsecs_to_seconds,
    present_first_queued_video_frame, rebuffer_playback_resume_waterline_after_cache_pause,
    rebuffer_playback_resume_waterline_after_prolonged_wait,
    service_initial_video_clock_until_audio_start, timed_output_gate_demux_watermark,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum OutputGateResumeStatus {
    Idle,
    Waiting,
    WaitingForDemux,
    Resumed,
}

#[derive(Clone, Copy, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct OutputGateResumeTiming {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_snapshot: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) resume_decision: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) demux_watermark: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) waterline: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fallback: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) resume_action: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) wait_log: Duration,
}

#[derive(Clone, Copy)]
struct OutputGateResumeLogContext {
    session_id: PlaybackSessionId,
    started_at: Instant,
    timing: OutputGateResumeTiming,
    status: OutputGateResumeStatus,
    output_state: PlaybackOutputState,
    queued_video_frames: usize,
    pending_audio_frames: usize,
    waterline: Option<PlaybackResumeWaterline>,
}

fn output_gate_resume_log_context(
    output_scheduler: &PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    started_at: Instant,
    timing: OutputGateResumeTiming,
    status: OutputGateResumeStatus,
    waterline: Option<PlaybackResumeWaterline>,
) -> OutputGateResumeLogContext {
    OutputGateResumeLogContext {
        session_id,
        started_at,
        timing,
        status,
        output_state: output_scheduler.playback_output_state,
        queued_video_frames: output_scheduler.scheduled_video_queue.len(),
        pending_audio_frames: output_scheduler.pending_start_audio.len(),
        waterline,
    }
}

fn finish_output_gate_resume_timing(context: OutputGateResumeLogContext) -> OutputGateResumeStatus {
    let total = context.started_at.elapsed();
    tracing::trace!(
        session_id = ?context.session_id,
        status = ?context.status,
        output_state = ?context.output_state,
        total_ms = total.as_secs_f64() * 1000.0,
        audio_snapshot_ms = context.timing.audio_snapshot.as_secs_f64() * 1000.0,
        resume_decision_ms = context.timing.resume_decision.as_secs_f64() * 1000.0,
        demux_watermark_ms = context.timing.demux_watermark.as_secs_f64() * 1000.0,
        waterline_ms = context.timing.waterline.as_secs_f64() * 1000.0,
        fallback_ms = context.timing.fallback.as_secs_f64() * 1000.0,
        resume_action_ms = context.timing.resume_action.as_secs_f64() * 1000.0,
        wait_log_ms = context.timing.wait_log.as_secs_f64() * 1000.0,
        queued_video_frames = context.queued_video_frames,
        pending_audio_frames = context.pending_audio_frames,
        waterline_ready = ?context.waterline.map(PlaybackResumeWaterline::ready),
        decoded_ready = ?context.waterline.map(PlaybackResumeWaterline::decoded_ready),
        target_ms = ?context.waterline.map(|waterline| waterline.target_nsecs as f64 / 1_000_000.0),
        decoded_video_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.demux_min_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        "FFmpeg output gate resume timing"
    );
    if total < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.audio_snapshot < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.resume_decision < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.demux_watermark < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.waterline < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.fallback < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.resume_action < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && context.timing.wait_log < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return context.status;
    }
    tracing::debug!(
        session_id = ?context.session_id,
        status = ?context.status,
        output_state = ?context.output_state,
        total_ms = total.as_secs_f64() * 1000.0,
        audio_snapshot_ms = context.timing.audio_snapshot.as_secs_f64() * 1000.0,
        resume_decision_ms = context.timing.resume_decision.as_secs_f64() * 1000.0,
        demux_watermark_ms = context.timing.demux_watermark.as_secs_f64() * 1000.0,
        waterline_ms = context.timing.waterline.as_secs_f64() * 1000.0,
        fallback_ms = context.timing.fallback.as_secs_f64() * 1000.0,
        resume_action_ms = context.timing.resume_action.as_secs_f64() * 1000.0,
        wait_log_ms = context.timing.wait_log.as_secs_f64() * 1000.0,
        queued_video_frames = context.queued_video_frames,
        pending_audio_frames = context.pending_audio_frames,
        waterline_ready = ?context.waterline.map(PlaybackResumeWaterline::ready),
        decoded_ready = ?context.waterline.map(PlaybackResumeWaterline::decoded_ready),
        target_ms = ?context.waterline.map(|waterline| waterline.target_nsecs as f64 / 1_000_000.0),
        decoded_video_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.demux_min_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        "FFmpeg output gate resume completed slowly"
    );
    context.status
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_output_gate_resume_if_ready<F>(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: Option<&AudioOutput>,
    demux_cache: Option<&DemuxPacketCache>,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    fallback_timeline_nsecs: u64,
    current_start_position_nsecs: &mut u64,
    scheduler: &mut PlaybackScheduler,
    output_resource_pressure: bool,
    mut demux_watermark: F,
) -> std::result::Result<OutputGateResumeStatus, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let mut timing = OutputGateResumeTiming::default();
    let Some(output) = output else {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    };
    if !output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        && !output_scheduler.playback_output_state.rebuffering()
    {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    }
    if output_scheduler.scheduled_video_queue.is_empty() {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Idle,
                None,
            ),
        ));
    }

    let needs_prefetch = subtitle_pipeline.needs_prefetch();
    let stage_started_at = Instant::now();
    let audio_snapshot = output.snapshot()?;
    timing.audio_snapshot = stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    let previous_audio_played_until = audio_snapshot.played_timeline_nsecs;
    let rebuffer_anchor = output_scheduler
        .playback_output_state
        .rebuffering()
        .then_some(output_scheduler.video_output_rebuffer_anchor)
        .flatten();
    let resume_audio_played_until = rebuffer_anchor
        .map(|anchor| anchor.timeline_nsecs)
        .unwrap_or(previous_audio_played_until);
    let audio_output_buffered_until_nsecs = if output_scheduler.playback_output_state.rebuffering()
    {
        Some(audio_snapshot.buffered_until_timeline_nsecs)
    } else {
        None
    };
    let first_video_frame_pending = output_scheduler
        .playback_output_state
        .first_video_frame_pending();
    let mut initial_sync_decision = None;
    let resume_decision = if first_video_frame_pending {
        let sync_decision = output_scheduler
            .scheduled_video_queue
            .initial_output_sync_decision(
                &output_scheduler.pending_start_audio,
                previous_audio_played_until,
            )
            .unwrap_or(InitialOutputSyncDecision {
                video_resume_timeline_nsecs: fallback_timeline_nsecs,
                audio_start_timeline_nsecs: None,
                delayed_audio_start_timeline_nsecs: None,
                drop_audio_before_timeline_nsecs: None,
                stale_audio_preroll_until_nsecs: None,
                stale_audio_preroll_gap_nsecs: None,
                allow_initial_audio_gap_at_video_start: false,
                reset_audio_to_video: false,
            });
        initial_sync_decision = Some(sync_decision);
        sync_decision.audio_clock_resume_decision()
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_audio_clock_resume_decision(
                &output_scheduler.pending_start_audio,
                resume_audio_played_until,
                audio_output_buffered_until_nsecs,
                rebuffer_anchor
                    .is_some_and(|anchor| anchor.reset_to_video_when_decoded_queue_misses_anchor),
            )
            .unwrap_or(AudioClockResumeDecision {
                timeline_nsecs: fallback_timeline_nsecs,
                reset_audio_to_video: false,
            })
    };
    let resume_audio_output_buffered_until_nsecs =
        audio_output_buffered_until_for_resume(resume_decision, audio_output_buffered_until_nsecs);
    if let Some(sync_decision) = initial_sync_decision {
        let first_video_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .range_nsecs()
            .map(|(start, _)| start);
        let pending_audio_buffered_until_nsecs =
            sync_decision
                .audio_start_timeline_nsecs
                .and_then(|audio_start| {
                    output_scheduler
                        .pending_start_audio
                        .buffered_until_from(audio_start)
                });
        let audio_minus_video_ms = first_video_timeline_nsecs
            .zip(sync_decision.audio_start_timeline_nsecs)
            .map(|(video_start, audio_start)| {
                (audio_start as i128 - video_start as i128) as f64 / 1_000_000.0
            });
        let sync_reason = if sync_decision.delayed_audio_start_timeline_nsecs.is_some() {
            "initial_video_clock_until_delayed_audio"
        } else if sync_decision.allow_initial_audio_gap_at_video_start {
            "initial_skip_audio_preroll_before_video"
        } else if sync_decision.stale_audio_preroll_gap_nsecs.is_some() {
            "initial_wait_after_audio_preroll_gap"
        } else {
            "initial_audio_video_start"
        };
        tracing::debug!(
            session_id = ?session_id,
            first_video_timeline_nsecs = ?first_video_timeline_nsecs,
            first_audio_timeline_nsecs = ?sync_decision.audio_start_timeline_nsecs,
            pending_audio_buffered_until_nsecs = ?pending_audio_buffered_until_nsecs,
            video_resume_timeline_nsecs = sync_decision.video_resume_timeline_nsecs,
            delayed_audio_start_timeline_nsecs =
                ?sync_decision.delayed_audio_start_timeline_nsecs,
            audio_minus_video_ms = ?audio_minus_video_ms,
            stale_audio_preroll_until_nsecs = ?sync_decision.stale_audio_preroll_until_nsecs,
            stale_audio_preroll_gap_ms = ?sync_decision
                .stale_audio_preroll_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            drop_audio_before_timeline_nsecs =
                ?sync_decision.drop_audio_before_timeline_nsecs,
            allow_initial_audio_gap_at_video_start =
                sync_decision.allow_initial_audio_gap_at_video_start,
            waterline_timeline_nsecs = sync_decision.video_resume_timeline_nsecs,
            sync_reason,
            "FFmpeg output gate initial sync decision"
        );
        if sync_decision.allow_initial_audio_gap_at_video_start {
            output_scheduler.initial_audio_gap_at_video_start_timeline_nsecs =
                Some(sync_decision.video_resume_timeline_nsecs);
        }
        if let Some(drop_audio_before_timeline_nsecs) =
            sync_decision.drop_audio_before_timeline_nsecs
        {
            let dropped_audio_frames = output_scheduler
                .pending_start_audio
                .discard_before(drop_audio_before_timeline_nsecs);
            if dropped_audio_frames > 0 {
                tracing::debug!(
                    session_id = ?session_id,
                    dropped_audio_frames,
                    drop_audio_before_timeline_nsecs,
                    stale_audio_preroll_until_nsecs =
                        ?sync_decision.stale_audio_preroll_until_nsecs,
                    stale_audio_preroll_gap_ms = ?sync_decision
                        .stale_audio_preroll_gap_nsecs
                        .map(|gap| gap as f64 / 1_000_000.0),
                    "discarded stale initial FFmpeg audio preroll before video start"
                );
            }
        }
    }
    timing.resume_decision = stage_started_at.elapsed();

    let waterline_demux_watermark =
        timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
    let stage_started_at = Instant::now();
    let mut waterline = if first_video_frame_pending {
        let sync_decision = initial_sync_decision.expect("initial sync decision exists");
        output_scheduler
            .scheduled_video_queue
            .initial_playback_resume_waterline(
                &output_scheduler.pending_start_audio,
                sync_decision.video_resume_timeline_nsecs,
                sync_decision.delayed_audio_start_timeline_nsecs,
                sync_decision.allow_initial_audio_gap_at_video_start
                    || output_scheduler.initial_audio_gap_at_video_start_timeline_nsecs
                        == Some(sync_decision.video_resume_timeline_nsecs),
                waterline_demux_watermark,
                needs_prefetch,
                true,
            )
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_playback_resume_waterline_with_resource_pressure(
                &output_scheduler.pending_start_audio,
                resume_decision.timeline_nsecs,
                waterline_demux_watermark,
                resume_audio_output_buffered_until_nsecs,
                needs_prefetch,
                true,
                output_resource_pressure,
            )
    };
    timing.waterline = stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if first_video_frame_pending && !waterline.ready() {
        let stale_preroll_waterline =
            initial_playback_resume_waterline_after_stale_audio_preroll_wait(
                waterline,
                initial_sync_decision,
                output_scheduler.startup_sync_elapsed(),
            );
        if !waterline.decoded_audio_ready && stale_preroll_waterline.decoded_audio_ready {
            tracing::debug!(
                session_id = ?session_id,
                startup_wait_ms = output_scheduler
                    .startup_sync_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?waterline
                    .decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?waterline
                    .decoded_audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_ready = waterline.demux_ready,
                stale_audio_preroll_gap_ms = ?initial_sync_decision
                    .and_then(|decision| decision.stale_audio_preroll_gap_nsecs)
                    .map(|gap| gap as f64 / 1_000_000.0),
                "startup output gate stale audio preroll timed out; allowing video-clocked start"
            );
        }
        waterline = stale_preroll_waterline;
    }
    timing.fallback += stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        && !waterline.ready()
        && waterline.decoded_ready()
        && output_scheduler
            .startup_sync_elapsed()
            .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER)
    {
        tracing::debug!(
            session_id = ?session_id,
            startup_wait_ms = output_scheduler
                .startup_sync_elapsed()
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
            decoded_video_ms = ?waterline
                .decoded_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            decoded_audio_ms = ?waterline
                .decoded_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_min_ms = ?waterline
                .demux_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_video_ms = ?waterline
                .demux_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_ms = ?waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "startup output gate demux waterline timed out; allowing decoded queues to start"
        );
        waterline.demux_ready = true;
    }
    timing.fallback += stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler.playback_output_state.rebuffering() && !waterline.ready() {
        let cache_pause_waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            output_scheduler.rebuffer_wait_elapsed(),
            demux_cache.is_some() && control.is_cache_paused(),
        );
        if cache_pause_waterline.ready() {
            if let Some(demux_cache) = demux_cache {
                demux_cache.clear_cache_pause_for_decoded_resume();
            }
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                target_ms = cache_pause_waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?cache_pause_waterline
                    .decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?cache_pause_waterline
                    .decoded_audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?cache_pause_waterline
                    .demux_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "rebuffer cache pause stalled with decoded queues ready; resuming from decoded waterline"
            );
            waterline = cache_pause_waterline;
        }
    }
    timing.fallback += stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    if output_scheduler.playback_output_state.rebuffering() && !waterline.ready() {
        let stalled_waterline = rebuffer_playback_resume_waterline_after_prolonged_wait(
            waterline,
            output_scheduler.rebuffer_wait_elapsed(),
        );
        if stalled_waterline.ready() {
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                original_target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                target_ms = stalled_waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?stalled_waterline
                    .decoded_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?stalled_waterline
                    .decoded_audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?stalled_waterline
                    .demux_min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "rebuffer output gate waited for stable decoded video target; resuming with available queues"
            );
        }
        waterline = stalled_waterline;
    }
    timing.fallback += stage_started_at.elapsed();
    if waterline.ready()
        && first_video_frame_pending
        && let Some(sync_decision) = initial_sync_decision
        && let Some(delayed_audio_start_timeline_nsecs) =
            initial_delayed_audio_start_timeline_nsecs(output_scheduler, sync_decision)
    {
        let stage_started_at = Instant::now();
        let status = service_initial_video_clock_until_audio_start(
            output_scheduler,
            output,
            delayed_audio_start_timeline_nsecs,
            control,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
            current_start_position_nsecs,
            scheduler,
        )?;
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                status,
                Some(waterline),
            ),
        ));
    }
    if waterline.ready() && first_video_frame_pending {
        let stage_started_at = Instant::now();
        output_scheduler.set_state(PlaybackOutputState::Ready);
        let Some(first_video) = present_first_queued_video_frame(
            output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            |first_video, output_scheduler| {
                if first_video.timeline_nsecs > *current_start_position_nsecs {
                    *current_start_position_nsecs = first_video.timeline_nsecs;
                    scheduler.reset(first_video.timeline_nsecs);
                    subtitle_pipeline.reset_cues_for_position(first_video.timeline_nsecs);
                    buffered_reporter.reset_to(
                        nsecs_to_seconds(first_video.timeline_nsecs),
                        session_id,
                        event_tx,
                    );
                }
                output.reset_clock(first_video.timeline_nsecs);
                tracing::debug!(
                    session_id = ?session_id,
                    pts = first_video.timeline_nsecs,
                    output_scheduler.scheduled_video_queue =
                        output_scheduler.scheduled_video_queue.len(),
                    decoded_video_range =
                        ?output_scheduler.scheduled_video_queue.range_nsecs(),
                    queued_video_ms =
                        output_scheduler.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                    target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                    demux_min_ms = ?waterline
                        .demux_min_forward_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    reset_audio_to_video = resume_decision.reset_audio_to_video,
                    "presenting first FFmpeg video frame from output gate"
                );
            },
        ) else {
            timing.resume_action += stage_started_at.elapsed();
            return Ok(finish_output_gate_resume_timing(
                output_gate_resume_log_context(
                    output_scheduler,
                    session_id,
                    started_at,
                    timing,
                    OutputGateResumeStatus::Waiting,
                    Some(waterline),
                ),
            ));
        };
        let audio_start_timeline_nsecs = first_video.timeline_nsecs;
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or_else(|| {
                first_video
                    .timeline_nsecs
                    .saturating_add(first_video.duration_nsecs)
            });
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
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
        output_scheduler.defer_next_pending_start_audio_flush();
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Resumed,
                Some(waterline),
            ),
        ));
    }
    if waterline.ready()
        && output_scheduler.playback_output_state.rebuffering()
        && output_scheduler.finish_rebuffer_if_ready(waterline, session_id)
    {
        let stage_started_at = Instant::now();
        // Keep pre-resume video while the restart waterline is still filling;
        // dropping it early can prevent the decoded window from ever catching up.
        discard_decoded_video_before_output_gate_resume_if_ready(
            output_scheduler,
            waterline,
            resume_decision,
            session_id,
            previous_audio_played_until,
            rebuffer_anchor,
        );
        let audio_start_timeline_nsecs = if resume_decision.reset_audio_to_video {
            output.reset_clock(resume_decision.timeline_nsecs);
            resume_decision.timeline_nsecs
        } else {
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot)
                .max(resume_decision.timeline_nsecs)
        };
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or(audio_start_timeline_nsecs);
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::UnderrunRecovery,
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
        output_scheduler.defer_next_pending_start_audio_flush();
        control.set_output_rebuffer_paused(false);
        output_scheduler.set_state(PlaybackOutputState::Playing);
        timing.resume_action += stage_started_at.elapsed();
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::Resumed,
                Some(waterline),
            ),
        ));
    }
    if !waterline.ready() {
        let demux_watermark = timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
        let stage_started_at = Instant::now();
        output_scheduler
            .scheduled_video_queue
            .log_resume_waterline_wait(
                session_id,
                "output_gate",
                output_scheduler.playback_output_state,
                resume_decision.timeline_nsecs,
                &output_scheduler.pending_start_audio,
                waterline,
                demux_watermark,
            );
        timing.wait_log += stage_started_at.elapsed();
    }
    if waterline.decoded_ready() && !waterline.demux_ready {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                OutputGateResumeStatus::WaitingForDemux,
                Some(waterline),
            ),
        ));
    }
    Ok(finish_output_gate_resume_timing(
        output_gate_resume_log_context(
            output_scheduler,
            session_id,
            started_at,
            timing,
            OutputGateResumeStatus::Waiting,
            Some(waterline),
        ),
    ))
}
