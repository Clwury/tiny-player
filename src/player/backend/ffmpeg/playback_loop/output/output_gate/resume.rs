use super::{
    AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION, AtomicBool, AudioClockMode,
    AudioClockResumeDecision, AudioOutput, AudioOutputSnapshot, BackendEvent, BufferedReporter,
    DelayedAudioStartSilencePolicy, DemuxPacketCache, DemuxReaderWatermark, Duration,
    FfmpegControl, HevcDecodeChainStats, InitialOutputSyncDecision, InitialStartAdmission,
    InitialStartAdmissionInput, InitialStartBlockReason, InitialSyncLogDecision,
    InitialSyncLogObservation, Instant, OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    OutputServiceDemand, PENDING_AUDIO_CONTINUITY_TOLERANCE,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PendingAudioPressureContext,
    PlaybackOutputScheduler, PlaybackOutputState, PlaybackResumeWaterline, PlaybackScheduler,
    PlaybackSessionId, PositionReporter, Sender, SubtitlePipeline,
    VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VIDEO_OUTPUT_START_FIRST_FRAME_STALL_LOG_AFTER,
    VideoDecodeWorkerSnapshot, VideoDecodeWorkerState, VideoOutputQueue,
    audio_output_buffered_until_for_resume, audio_output_contiguous_start_timeline_nsecs,
    discard_decoded_video_before_output_gate_resume_if_ready, duration_nsecs,
    flush_pending_start_audio, initial_start_admission, playback_resume_waterline_blocked_on,
    rebuffer_playback_resume_waterline_after_cache_pause,
    rebuffer_playback_resume_waterline_after_prolonged_wait,
    service_initial_video_clock_until_audio_start, timed_output_gate_demux_watermark,
};
use crate::player::render_host::FramePixels;

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) const
    MAX_REBUFFER_AUDIO_LEAD_NSECS: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum StaleRebufferPendingAudio {
    Ahead {
        pending_start_nsecs: u64,
    },
    Behind {
        pending_start_nsecs: u64,
        pending_until_nsecs: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum OutputGateResumeStatus {
    Idle,
    Waiting,
    Rebuffering,
    WaitingForDecodedVideo,
    WaitingForDecodedAudio,
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

#[derive(Clone, Copy, Debug)]
struct RebufferAudioPrefillStatus {
    audio_start_timeline_nsecs: u64,
    target_nsecs: u64,
    pending_audio_forward_nsecs: u64,
    loop_recovery: bool,
    delayed_audio_start: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RebufferSchedulerResumePlan {
    DelayBy(Duration),
    ResetTo(u64),
}

fn rebuffer_scheduler_resume_plan(
    reset_video_clock_anchor: bool,
    first_video_timeline_nsecs: u64,
    resume_timeline_nsecs: u64,
    rebuffer_elapsed: Duration,
) -> RebufferSchedulerResumePlan {
    if reset_video_clock_anchor {
        RebufferSchedulerResumePlan::ResetTo(first_video_timeline_nsecs.max(resume_timeline_nsecs))
    } else {
        RebufferSchedulerResumePlan::DelayBy(rebuffer_elapsed)
    }
}

fn apply_rebuffer_scheduler_resume_plan(
    scheduler: &mut PlaybackScheduler,
    plan: RebufferSchedulerResumePlan,
) {
    match plan {
        RebufferSchedulerResumePlan::DelayBy(elapsed) => scheduler.delay_by(elapsed),
        RebufferSchedulerResumePlan::ResetTo(timeline_nsecs) => scheduler.reset(timeline_nsecs),
    }
}

impl RebufferAudioPrefillStatus {
    fn ready(self) -> bool {
        self.pending_audio_forward_nsecs >= self.target_nsecs
    }
}

fn initial_start_wait_status(admission: InitialStartAdmission) -> OutputGateResumeStatus {
    let InitialStartAdmission::Waiting(reason) = admission else {
        return OutputGateResumeStatus::Waiting;
    };
    match reason {
        super::InitialStartBlockReason::NoAudioFrame
        | super::InitialStartBlockReason::NoAudioCoverage
        | super::InitialStartBlockReason::AudioVideoOffset => {
            OutputGateResumeStatus::WaitingForDecodedAudio
        }
        super::InitialStartBlockReason::NoVideoFrame
        | super::InitialStartBlockReason::ActiveRecovery
        | super::InitialStartBlockReason::InsufficientLookahead
        | super::InitialStartBlockReason::SingleFrameNotVulkan
        | super::InitialStartBlockReason::TargetFrameNotConfirmed
        | super::InitialStartBlockReason::FrameNotConfirmedClean => {
            OutputGateResumeStatus::WaitingForDecodedVideo
        }
    }
}

fn log_initial_sync_observation(
    output_scheduler: &mut PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    observation: InitialSyncLogObservation,
    now: Instant,
) {
    let decision = output_scheduler.observe_initial_sync_log(observation, now);
    let (log_kind, repeated_observations) = match decision {
        InitialSyncLogDecision::Changed { suppressed_repeats } => ("changed", suppressed_repeats),
        InitialSyncLogDecision::Summary {
            repeated_observations,
        } => ("summary", repeated_observations),
        InitialSyncLogDecision::Suppressed => return,
    };
    tracing::debug!(
        session_id = ?session_id,
        target = observation.target_nsecs,
        first_video = ?observation.first_video_nsecs,
        first_audio = ?observation.first_audio_nsecs,
        decoded_video = ?observation.decoded_video_nsecs,
        decoded_audio = ?observation.decoded_audio_nsecs,
        demux_min = ?observation.demux_min_nsecs,
        blocked_on = observation.blocked_on,
        due_kind = observation.due_kind.as_str(),
        output_generation = output_scheduler.output_housekeeping_generation,
        log_kind,
        repeated_observations,
        "FFmpeg output gate initial-sync decision"
    );
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
            .and_then(|waterline| waterline.decoded_output.video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_output.audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        resume_anchor_source = ?context.waterline
            .map(|waterline| waterline.resume_anchor_source.as_str()),
        audio_start_gap_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_output.delayed_audio_start_gap_nsecs)
            .map(|gap| gap as f64 / 1_000_000.0),
        allow_audio_gap_at_video_resume = ?context.waterline
            .map(|waterline| waterline.allow_audio_gap_at_video_resume),
        audio_output_pending_ms = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .and_then(|audio| audio.audio_output_pending_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        audio_decode_queued_ms = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .map(|audio| audio.audio_decode_queued_nsecs as f64 / 1_000_000.0),
        audio_decode_in_flight_packets = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .map(|audio| audio.audio_decode_in_flight_packets),
        demux_audio_cached_packets = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .and_then(|audio| audio.demux_audio_cached_packets),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.prefetch.min_forward_nsecs)
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
            .and_then(|waterline| waterline.decoded_output.video_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        decoded_audio_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_output.audio_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        resume_anchor_source = ?context.waterline
            .map(|waterline| waterline.resume_anchor_source.as_str()),
        audio_start_gap_ms = ?context.waterline
            .and_then(|waterline| waterline.decoded_output.delayed_audio_start_gap_nsecs)
            .map(|gap| gap as f64 / 1_000_000.0),
        allow_audio_gap_at_video_resume = ?context.waterline
            .map(|waterline| waterline.allow_audio_gap_at_video_resume),
        audio_output_pending_ms = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .and_then(|audio| audio.audio_output_pending_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        audio_decode_queued_ms = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .map(|audio| audio.audio_decode_queued_nsecs as f64 / 1_000_000.0),
        audio_decode_in_flight_packets = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .map(|audio| audio.audio_decode_in_flight_packets),
        demux_audio_cached_packets = ?context.waterline
            .and_then(|waterline| waterline.audio_resume_waterline)
            .and_then(|audio| audio.demux_audio_cached_packets),
        demux_min_ms = ?context.waterline
            .and_then(|waterline| waterline.prefetch.min_forward_nsecs)
            .map(|duration| duration as f64 / 1_000_000.0),
        "FFmpeg output gate resume completed slowly"
    );
    context.status
}

fn rebuffer_audio_prefill_status(
    output_scheduler: &PlaybackOutputScheduler,
    audio_snapshot: AudioOutputSnapshot,
    resume_decision: AudioClockResumeDecision,
) -> Option<RebufferAudioPrefillStatus> {
    if !output_scheduler.playback_output_state.rebuffering() {
        return None;
    }
    let delayed_audio_start_timeline_nsecs = resume_decision
        .allow_audio_gap_at_video_resume
        .then_some(resume_decision.delayed_audio_start_timeline_nsecs)
        .flatten();
    let loop_recovery = output_scheduler.audio_rebuffer_loop_active();
    if delayed_audio_start_timeline_nsecs.is_none() && !loop_recovery {
        return None;
    }

    let audio_start_timeline_nsecs =
        if let Some(delayed_audio_start_timeline_nsecs) = delayed_audio_start_timeline_nsecs {
            delayed_audio_start_timeline_nsecs
        } else if resume_decision.reset_audio_to_video {
            resume_decision.timeline_nsecs
        } else {
            audio_output_contiguous_start_timeline_nsecs(audio_snapshot)
                .max(resume_decision.timeline_nsecs)
        };
    let queued_video_contiguous_forward_nsecs = output_scheduler
        .scheduled_video_queue
        .forward_nsecs_from(audio_start_timeline_nsecs);
    let mut target_nsecs =
        output_scheduler.audio_rebuffer_prefill_target_nsecs(queued_video_contiguous_forward_nsecs);
    if delayed_audio_start_timeline_nsecs.is_some_and(|delayed_start_nsecs| {
        delayed_start_nsecs.saturating_sub(resume_decision.timeline_nsecs)
            <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
    }) {
        target_nsecs =
            target_nsecs.min(duration_nsecs(AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION));
    }
    let pending_audio_forward_nsecs = output_scheduler
        .pending_start_audio
        .buffered_until_from(audio_start_timeline_nsecs)
        .map(|buffered_until| buffered_until.saturating_sub(audio_start_timeline_nsecs))
        .unwrap_or_default();

    Some(RebufferAudioPrefillStatus {
        audio_start_timeline_nsecs,
        target_nsecs,
        pending_audio_forward_nsecs,
        loop_recovery,
        delayed_audio_start: delayed_audio_start_timeline_nsecs.is_some(),
    })
}

fn rebuffer_audio_flush_start(
    resume_decision: AudioClockResumeDecision,
    audio_snapshot: AudioOutputSnapshot,
) -> (u64, DelayedAudioStartSilencePolicy) {
    let delayed_audio_start_timeline_nsecs = resume_decision
        .allow_audio_gap_at_video_resume
        .then_some(resume_decision.delayed_audio_start_timeline_nsecs)
        .flatten();
    if let Some(delayed_start_nsecs) = delayed_audio_start_timeline_nsecs {
        let gap_nsecs = delayed_start_nsecs.saturating_sub(resume_decision.timeline_nsecs);
        if gap_nsecs <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE) {
            return (
                resume_decision.timeline_nsecs,
                DelayedAudioStartSilencePolicy::Allow,
            );
        }
        return (delayed_start_nsecs, DelayedAudioStartSilencePolicy::Skip);
    }
    if resume_decision.reset_audio_to_video {
        return (
            resume_decision.timeline_nsecs,
            DelayedAudioStartSilencePolicy::Skip,
        );
    }
    (
        audio_output_contiguous_start_timeline_nsecs(audio_snapshot)
            .max(resume_decision.timeline_nsecs),
        DelayedAudioStartSilencePolicy::Skip,
    )
}

fn enforce_rebuffer_audio_prefill_waterline(
    mut waterline: PlaybackResumeWaterline,
    prefill: Option<RebufferAudioPrefillStatus>,
    session_id: PlaybackSessionId,
) -> PlaybackResumeWaterline {
    let Some(prefill) = prefill else {
        return waterline;
    };
    if prefill.ready() {
        return waterline;
    }

    waterline.decoded_output.audio_ready = false;
    if let Some(mut audio_waterline) = waterline.audio_resume_waterline {
        audio_waterline.target_nsecs = prefill.target_nsecs;
        audio_waterline.pending_audio_forward_nsecs = Some(prefill.pending_audio_forward_nsecs);
        audio_waterline.decoded_audio_forward_nsecs = Some(prefill.pending_audio_forward_nsecs);
        audio_waterline.ready = false;
        waterline.audio_resume_waterline = Some(audio_waterline);
    }
    tracing::debug!(
        session_id = ?session_id,
        audio_start_timeline_nsecs = prefill.audio_start_timeline_nsecs,
        prefill_target_ms = prefill.target_nsecs as f64 / 1_000_000.0,
        pending_audio_forward_ms = prefill.pending_audio_forward_nsecs as f64 / 1_000_000.0,
        loop_recovery = prefill.loop_recovery,
        delayed_audio_start = prefill.delayed_audio_start,
        "holding FFmpeg rebuffer resume until audio output prefill target is available"
    );
    waterline
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn stale_rebuffer_pending_audio_ahead(
    output_scheduler: &PlaybackOutputScheduler,
    audio_snapshot: AudioOutputSnapshot,
    resume_timeline_nsecs: u64,
) -> Option<u64> {
    match stale_rebuffer_pending_audio(output_scheduler, audio_snapshot, resume_timeline_nsecs) {
        Some(StaleRebufferPendingAudio::Ahead {
            pending_start_nsecs,
        }) => Some(pending_start_nsecs),
        _ => None,
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn stale_rebuffer_pending_audio(
    output_scheduler: &PlaybackOutputScheduler,
    audio_snapshot: AudioOutputSnapshot,
    resume_timeline_nsecs: u64,
) -> Option<StaleRebufferPendingAudio> {
    if !output_scheduler.playback_output_state.rebuffering()
        || audio_snapshot.total_pending_nsecs != 0
    {
        return None;
    }
    let pending_audio_start_nsecs = output_scheduler
        .pending_start_audio
        .first_start_timeline_nsecs()?;
    if pending_audio_start_nsecs
        > resume_timeline_nsecs.saturating_add(MAX_REBUFFER_AUDIO_LEAD_NSECS)
    {
        return Some(StaleRebufferPendingAudio::Ahead {
            pending_start_nsecs: pending_audio_start_nsecs,
        });
    }
    if output_scheduler
        .pending_start_audio
        .forward_duration_from(resume_timeline_nsecs)
        .is_some()
    {
        return None;
    }

    let anchor = output_scheduler.video_output_rebuffer_anchor?;
    if !anchor.reset_to_video_when_decoded_queue_misses_anchor {
        return None;
    }
    let first_video_timeline_nsecs = output_scheduler
        .scheduled_video_queue
        .range_nsecs()
        .map(|(start, _)| start)?;
    let gap_tolerance_nsecs = duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE);
    if first_video_timeline_nsecs <= anchor.timeline_nsecs.saturating_add(gap_tolerance_nsecs)
        || resume_timeline_nsecs < first_video_timeline_nsecs
    {
        return None;
    }

    let pending_until_nsecs = output_scheduler
        .pending_start_audio
        .buffered_until_from(pending_audio_start_nsecs);
    let pending_audio_clearly_behind_by_start = resume_timeline_nsecs
        > pending_audio_start_nsecs.saturating_add(MAX_REBUFFER_AUDIO_LEAD_NSECS);
    let pending_audio_clearly_behind_by_until = pending_until_nsecs.is_none_or(|pending_until| {
        resume_timeline_nsecs > pending_until.saturating_add(gap_tolerance_nsecs)
    });
    (pending_audio_clearly_behind_by_start || pending_audio_clearly_behind_by_until).then_some(
        StaleRebufferPendingAudio::Behind {
            pending_start_nsecs: pending_audio_start_nsecs,
            pending_until_nsecs,
        },
    )
}

fn clear_stale_rebuffer_pending_audio_if_needed(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    audio_snapshot: AudioOutputSnapshot,
    resume_timeline_nsecs: u64,
    session_id: PlaybackSessionId,
) -> Option<StaleRebufferPendingAudio> {
    let stale_audio =
        stale_rebuffer_pending_audio(output_scheduler, audio_snapshot, resume_timeline_nsecs)?;
    match stale_audio {
        StaleRebufferPendingAudio::Ahead {
            pending_start_nsecs,
        } => {
            let cleared_pending_audio_frames = output_scheduler.pending_start_audio.len();
            let cleared_pending_audio_ms = output_scheduler
                .pending_start_audio
                .buffered_duration()
                .as_secs_f64()
                * 1000.0;
            output_scheduler.pending_start_audio.clear();
            output_scheduler.set_rebuffer_empty_audio_output_blocked(false);
            output.reset_clock(resume_timeline_nsecs);
            tracing::warn!(
                session_id = ?session_id,
                resume_timeline_nsecs,
                pending_audio_start_nsecs = pending_start_nsecs,
                pending_audio_lead_ms =
                    pending_start_nsecs.saturating_sub(resume_timeline_nsecs) as f64
                    / 1_000_000.0,
                cleared_pending_audio_frames,
                cleared_pending_audio_ms,
                skipped_content_ms = pending_start_nsecs
                    .saturating_sub(resume_timeline_nsecs) as f64
                    / 1_000_000.0,
                "discarded stale rebuffer pending audio ahead of video resume"
            );
        }
        StaleRebufferPendingAudio::Behind {
            pending_start_nsecs,
            pending_until_nsecs,
        } => {
            let pending_audio_frames_before = output_scheduler.pending_start_audio.len();
            let pending_audio_ms_before = output_scheduler
                .pending_start_audio
                .buffered_duration()
                .as_secs_f64()
                * 1000.0;
            let discarded_before_resume_frames = output_scheduler
                .pending_start_audio
                .discard_before(resume_timeline_nsecs);
            let pending_audio_covers_resume = output_scheduler
                .pending_start_audio
                .buffered_until_from(resume_timeline_nsecs)
                .is_some();
            let cleared_discontinuous_pending_audio_frames = if pending_audio_covers_resume {
                0
            } else {
                let remaining_frames = output_scheduler.pending_start_audio.len();
                output_scheduler.pending_start_audio.clear();
                remaining_frames
            };
            let pending_audio_ms_after = output_scheduler
                .pending_start_audio
                .buffered_duration()
                .as_secs_f64()
                * 1000.0;
            let dropped_audio_frames = discarded_before_resume_frames
                .saturating_add(cleared_discontinuous_pending_audio_frames);
            let dropped_audio_ms = (pending_audio_ms_before - pending_audio_ms_after).max(0.0);
            output_scheduler.set_audio_sync_drop_before_timeline_nsecs(
                resume_timeline_nsecs,
                session_id,
                "stale_rebuffer_pending_audio_behind",
            );
            output_scheduler.clear_audio_sync_drop_before_if_covered(
                None,
                session_id,
                "stale_rebuffer_pending_audio_behind_pending_covers_resume",
            );
            output_scheduler.set_rebuffer_empty_audio_output_blocked(false);
            output.reset_clock(resume_timeline_nsecs);
            tracing::warn!(
                session_id = ?session_id,
                resume_timeline_nsecs,
                pending_audio_start_nsecs = pending_start_nsecs,
                pending_until_nsecs = ?pending_until_nsecs,
                pending_audio_lag_ms =
                    resume_timeline_nsecs.saturating_sub(pending_start_nsecs) as f64
                    / 1_000_000.0,
                pending_audio_frames_before,
                pending_audio_ms_before,
                dropped_audio_frames,
                dropped_audio_ms,
                skipped_content_ms = resume_timeline_nsecs
                    .saturating_sub(pending_start_nsecs) as f64
                    / 1_000_000.0,
                discarded_before_resume_frames,
                cleared_discontinuous_pending_audio_frames,
                pending_audio_frames_after = output_scheduler.pending_start_audio.len(),
                pending_audio_covers_resume,
                audio_sync_drop_before_timeline_nsecs =
                    ?output_scheduler.audio_sync_drop_before_timeline_nsecs(),
                "discarded stale rebuffer pending audio behind video resume"
            );
        }
    }
    Some(stale_audio)
}

fn apply_stale_rebuffer_pending_audio_resume_policy(
    resume_decision: &mut AudioClockResumeDecision,
    stale_audio: StaleRebufferPendingAudio,
) {
    resume_decision.reset_audio_to_video = true;
    resume_decision.allow_audio_gap_at_video_resume = true;
    resume_decision.delayed_audio_start_timeline_nsecs = match stale_audio {
        StaleRebufferPendingAudio::Ahead { .. } => Some(
            resume_decision
                .timeline_nsecs
                .saturating_add(MAX_REBUFFER_AUDIO_LEAD_NSECS),
        ),
        StaleRebufferPendingAudio::Behind { .. } => None,
    };
}

fn rebuffer_audio_prefill_status_after_stale_pending_audio(
    output_scheduler: &PlaybackOutputScheduler,
    audio_snapshot: AudioOutputSnapshot,
    resume_decision: AudioClockResumeDecision,
    stale_rebuffer_pending_audio_cleared: bool,
) -> Option<RebufferAudioPrefillStatus> {
    if stale_rebuffer_pending_audio_cleared {
        None
    } else {
        rebuffer_audio_prefill_status(output_scheduler, audio_snapshot, resume_decision)
    }
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
    audio_decode_queued_nsecs_snapshot: Option<u64>,
    audio_decode_in_flight_packets: usize,
    demux_audio_cached_packets: Option<usize>,
    demux_read_index: Option<usize>,
    video_decoder_pending_packets: usize,
    video_decode_snapshot: VideoDecodeWorkerSnapshot,
    video_is_hevc: bool,
    hevc_cached_exact_seek: bool,
    hevc_cached_exact_landing_nsecs: Option<u64>,
    hevc_decode_chain_stats: HevcDecodeChainStats,
    output_service_demand: OutputServiceDemand,
    mut demux_watermark: F,
) -> std::result::Result<OutputGateResumeStatus, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let audio_decode_queued_nsecs = audio_decode_queued_nsecs_snapshot.unwrap_or_default();
    let mut timing = OutputGateResumeTiming::default();
    output_scheduler.mark_output_housekeeping_serviced();
    output_scheduler.set_rebuffer_empty_audio_output_blocked(false);
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
    if let Some(transaction) = output_scheduler.initial_av_start_transaction() {
        if !output_service_demand.audio_start_due() && !output_service_demand.hard_deadline_due() {
            return Ok(finish_output_gate_resume_timing(
                output_gate_resume_log_context(
                    output_scheduler,
                    session_id,
                    started_at,
                    timing,
                    OutputGateResumeStatus::Waiting,
                    None,
                ),
            ));
        }
        let stage_started_at = Instant::now();
        let status = service_initial_video_clock_until_audio_start(
            output_scheduler,
            output,
            demux_cache,
            transaction.audio_start_target_nsecs,
            audio_decode_queued_nsecs_snapshot,
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
                None,
            ),
        ));
    }
    if !output_scheduler.restart_pending() && !output_scheduler.playback_output_state.rebuffering()
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
        if output_scheduler.playback_output_state.restart_pending() {
            let stage_started_at = Instant::now();
            let audio_snapshot = output.snapshot()?;
            timing.audio_snapshot = stage_started_at.elapsed();
            let waterline_demux_watermark =
                timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
            let stage_started_at = Instant::now();
            let audio_waterline = output_scheduler.audio_resume_waterline_for_output_wait(
                Some(audio_snapshot),
                audio_decode_queued_nsecs,
                audio_decode_in_flight_packets,
                *current_start_position_nsecs,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                waterline_demux_watermark.audio_forward_nsecs,
                demux_audio_cached_packets,
            );
            timing.waterline = stage_started_at.elapsed();
            let observation = InitialSyncLogObservation {
                target_nsecs: fallback_timeline_nsecs,
                first_video_nsecs: None,
                first_audio_nsecs: output_scheduler
                    .pending_start_audio
                    .first_start_timeline_nsecs(),
                decoded_video_nsecs: None,
                decoded_audio_nsecs: audio_waterline
                    .and_then(|waterline| waterline.decoded_audio_forward_nsecs),
                demux_min_nsecs: waterline_demux_watermark.selected_min_forward_nsecs,
                blocked_on: "no_video_frame",
                due_kind: output_service_demand,
            };
            log_initial_sync_observation(output_scheduler, session_id, observation, Instant::now());
        }
        let empty_queue_status = if output_scheduler.playback_output_state.restart_pending() {
            OutputGateResumeStatus::WaitingForDecodedVideo
        } else {
            OutputGateResumeStatus::Idle
        };
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                empty_queue_status,
                None,
            ),
        ));
    }

    let needs_prefetch = subtitle_pipeline.needs_prefetch();
    let stage_started_at = Instant::now();
    let mut audio_snapshot = output.snapshot()?;
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
        && audio_snapshot.total_pending_nsecs > 0
    {
        Some(audio_snapshot.buffered_until_timeline_nsecs)
    } else {
        None
    };
    let first_video_frame_pending = output_scheduler.restart_pending();
    let mut initial_sync_decision = None;
    let mut resume_decision = if first_video_frame_pending {
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
                Some(audio_snapshot.total_pending_nsecs),
                rebuffer_anchor
                    .is_some_and(|anchor| anchor.reset_to_video_when_decoded_queue_misses_anchor),
            )
            .unwrap_or(AudioClockResumeDecision {
                timeline_nsecs: fallback_timeline_nsecs,
                reset_audio_to_video: false,
                delayed_audio_start_timeline_nsecs: None,
                allow_audio_gap_at_video_resume: false,
                resume_anchor_source: Default::default(),
            })
    };
    let stale_rebuffer_pending_audio_cleared = if let Some(stale_audio) =
        clear_stale_rebuffer_pending_audio_if_needed(
            output_scheduler,
            output,
            audio_snapshot,
            resume_decision.timeline_nsecs,
            session_id,
        ) {
        apply_stale_rebuffer_pending_audio_resume_policy(&mut resume_decision, stale_audio);
        audio_snapshot = output.snapshot()?;
        true
    } else {
        false
    };
    let resume_audio_output_buffered_until_nsecs =
        audio_output_buffered_until_for_resume(resume_decision, audio_output_buffered_until_nsecs);
    if let Some(sync_decision) = initial_sync_decision {
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
            .rebuffer_playback_resume_waterline_for_decision(
                &output_scheduler.pending_start_audio,
                resume_decision,
                waterline_demux_watermark,
                resume_audio_output_buffered_until_nsecs,
                needs_prefetch,
                true,
                output_resource_pressure,
            )
    };
    if let Some(audio_resume_waterline) = output_scheduler.audio_resume_waterline_for_output_wait(
        Some(audio_snapshot),
        audio_decode_queued_nsecs,
        audio_decode_in_flight_packets,
        *current_start_position_nsecs,
        waterline.target_nsecs,
        waterline_demux_watermark.audio_forward_nsecs,
        demux_audio_cached_packets,
    ) {
        waterline.audio_resume_waterline = Some(audio_resume_waterline);
    }
    timing.waterline = stage_started_at.elapsed();
    let mut initial_admission = None;
    if first_video_frame_pending {
        let sync_decision = initial_sync_decision.expect("initial sync decision exists");
        let (
            first_video_nsecs,
            first_video_duration_nsecs,
            first_following_video_gap_nsecs,
            first_frame_is_vulkan,
            first_frame_confirmed_clean,
        ) = output_scheduler
            .scheduled_video_queue
            .with_frames(|frames| {
                let first = frames.front();
                (
                    first.map(|frame| frame.timeline_nsecs),
                    first.map(|frame| frame.duration_nsecs),
                    first.and_then(|first| {
                        frames.get(1).map(|following| {
                            following.timeline_nsecs.saturating_sub(
                                first.timeline_nsecs.saturating_add(first.duration_nsecs),
                            )
                        })
                    }),
                    first.is_some_and(|frame| {
                        matches!(&frame.frame.pixels, FramePixels::VulkanVideo(_))
                    }),
                    first.is_some(),
                )
            });
        // Corrupt/decode-error frames are rejected before they can enter the
        // scheduled output queue, so reaching this queue confirms a clean frame.
        let contiguous_video_frames = first_video_nsecs
            .map(|timeline_nsecs| {
                output_scheduler
                    .scheduled_video_queue
                    .contiguous_frame_count_from(timeline_nsecs)
            })
            .unwrap_or_default();
        let strict_video_forward_nsecs = first_video_nsecs.and_then(|timeline_nsecs| {
            output_scheduler
                .scheduled_video_queue
                .strict_forward_nsecs_from(timeline_nsecs)
        });
        let active_recovery = output_scheduler.decode_recovery_active()
            || video_decode_snapshot.state == VideoDecodeWorkerState::Recovering
            || (video_is_hevc && hevc_decode_chain_stats.pending_fallback_reason.is_some());
        let evaluation = initial_start_admission(InitialStartAdmissionInput {
            expected_target_nsecs: fallback_timeline_nsecs,
            first_video_nsecs,
            first_audio_nsecs: sync_decision.audio_start_timeline_nsecs,
            decoded_video_forward_nsecs: waterline.decoded_output.video_forward_nsecs,
            strict_video_forward_nsecs,
            decoded_audio_forward_nsecs: waterline.decoded_output.audio_forward_nsecs,
            contiguous_video_frames,
            first_video_duration_nsecs,
            first_following_video_gap_nsecs,
            first_frame_is_vulkan,
            first_frame_confirmed_clean,
            active_recovery,
            require_strict_fast_lookahead: video_is_hevc && hevc_cached_exact_seek,
            cached_exact_landing_nsecs: (video_is_hevc && hevc_cached_exact_seek)
                .then_some(hevc_cached_exact_landing_nsecs)
                .flatten(),
            startup_sync_elapsed: output_scheduler.startup_sync_elapsed(),
        });
        let now = Instant::now();
        if evaluation.pair.is_some() {
            output_scheduler.note_initial_av_pair(now);
        }
        log_initial_sync_observation(
            output_scheduler,
            session_id,
            InitialSyncLogObservation {
                target_nsecs: fallback_timeline_nsecs,
                first_video_nsecs,
                first_audio_nsecs: sync_decision.audio_start_timeline_nsecs,
                decoded_video_nsecs: waterline.decoded_output.video_forward_nsecs,
                decoded_audio_nsecs: waterline.decoded_output.audio_forward_nsecs,
                demux_min_nsecs: waterline.prefetch.min_forward_nsecs,
                blocked_on: evaluation
                    .admission
                    .blocked_on()
                    .map(InitialStartBlockReason::as_str)
                    .unwrap_or("none"),
                due_kind: output_service_demand,
            },
            now,
        );
        match evaluation.admission {
            InitialStartAdmission::Prime { pair, mode } => {
                tracing::debug!(
                    session_id = ?session_id,
                    target = fallback_timeline_nsecs,
                    decoded_video = ?waterline.decoded_output.video_forward_nsecs,
                    decoded_audio = ?waterline.decoded_output.audio_forward_nsecs,
                    demux_min = ?waterline.prefetch.min_forward_nsecs,
                    blocked_on = "none",
                    due_kind = output_service_demand.as_str(),
                    admission_mode = mode.as_str(),
                    video_anchor_nsecs = pair.video_anchor_nsecs,
                    audio_start_target_nsecs = pair.audio_start_target_nsecs,
                    "admitted bounded FFmpeg initial A/V output transaction"
                );
                let stage_started_at = Instant::now();
                let status = service_initial_video_clock_until_audio_start(
                    output_scheduler,
                    output,
                    demux_cache,
                    pair.audio_start_target_nsecs,
                    audio_decode_queued_nsecs_snapshot,
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
            InitialStartAdmission::Waiting(reason) => {
                if output_scheduler.initial_av_pair_watchdog_expired(now) {
                    output_scheduler.fail_initial_av_start_transaction(
                        control,
                        session_id,
                        "initial_av_pair_watchdog_expired",
                    );
                }
                initial_admission = Some(InitialStartAdmission::Waiting(reason));
            }
        }
    }
    let stage_started_at = Instant::now();
    if output_scheduler.playback_output_state.rebuffering() && !waterline.ready() {
        let demux_consumer_drainable = !waterline_demux_watermark.underrun
            && !waterline_demux_watermark.video_underrun
            && !waterline_demux_watermark.audio_underrun
            && waterline_demux_watermark.forward_bytes > 0
            && waterline
                .prefetch
                .min_forward_nsecs
                .is_some_and(|duration| duration > 0);
        let cache_pause_waterline = rebuffer_playback_resume_waterline_after_cache_pause(
            waterline,
            output_scheduler.rebuffer_wait_elapsed(),
            demux_cache.is_some() && control.is_cache_paused(),
            demux_consumer_drainable,
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
                    .decoded_output.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?cache_pause_waterline
                    .decoded_output.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?cache_pause_waterline
                    .prefetch.min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_target_shortfall_ms = ?cache_pause_waterline
                    .prefetch.min_forward_nsecs
                    .map(|duration| cache_pause_waterline.target_nsecs.saturating_sub(duration)
                        as f64
                        / 1_000_000.0),
                demux_video_ms = ?cache_pause_waterline
                    .prefetch.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_audio_ms = ?cache_pause_waterline
                    .prefetch.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_forward_bytes = waterline_demux_watermark.forward_bytes,
                demux_consumer_drainable,
                resume_reason = "decoded_waterline_with_drainable_demux",
                "rebuffer cache pause stalled with decoded queues ready and drainable demux packets; resuming from decoded waterline"
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
        if !waterline.decoded_output.audio_ready
            && stalled_waterline.ready()
            && output_scheduler
                .rebuffer_wait_elapsed()
                .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER)
        {
            output_scheduler.begin_audio_gap_recovery(
                resume_decision.timeline_nsecs,
                Instant::now(),
                session_id,
                "forced_video_clock_resume_after_audio_gap",
            );
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                resume_timeline_nsecs = resume_decision.timeline_nsecs,
                delayed_audio_start_timeline_nsecs =
                    ?resume_decision.delayed_audio_start_timeline_nsecs,
                allow_audio_gap_at_video_resume =
                    resume_decision.allow_audio_gap_at_video_resume,
                decoded_video_ms = ?stalled_waterline
                    .decoded_output.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?waterline
                    .decoded_output.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?stalled_waterline
                    .prefetch.min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                resume_reason = "forced_video_clock_resume_after_audio_gap",
                "forced_video_clock_resume_after_audio_gap"
            );
        }
        if stalled_waterline.ready() {
            tracing::debug!(
                session_id = ?session_id,
                rebuffer_wait_ms = ?output_scheduler
                    .rebuffer_wait_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                original_target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                target_ms = stalled_waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?stalled_waterline
                    .decoded_output.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?stalled_waterline
                    .decoded_output.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?stalled_waterline
                    .prefetch.min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "rebuffer output gate waited for stable decoded video target; resuming with available queues"
            );
        }
        waterline = stalled_waterline;
    }
    timing.fallback += stage_started_at.elapsed();
    let rebuffer_audio_prefill = rebuffer_audio_prefill_status_after_stale_pending_audio(
        output_scheduler,
        audio_snapshot,
        resume_decision,
        stale_rebuffer_pending_audio_cleared,
    );
    waterline =
        enforce_rebuffer_audio_prefill_waterline(waterline, rebuffer_audio_prefill, session_id);
    let rebuffer_ready = waterline.ready() && output_scheduler.playback_output_state.rebuffering();
    let rebuffer_elapsed = rebuffer_ready
        .then(|| output_scheduler.rebuffer_pause_elapsed())
        .flatten();
    if rebuffer_ready && output_scheduler.finish_rebuffer_if_ready(waterline, session_id) {
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
        let delayed_audio_start_timeline_nsecs = resume_decision
            .allow_audio_gap_at_video_resume
            .then_some(resume_decision.delayed_audio_start_timeline_nsecs)
            .flatten();
        let (audio_start_timeline_nsecs, delayed_audio_start_silence_policy) =
            rebuffer_audio_flush_start(resume_decision, audio_snapshot);
        let first_video_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .range_nsecs()
            .map(|(start_nsecs, _)| start_nsecs)
            .unwrap_or(resume_decision.timeline_nsecs);
        let reset_video_clock_anchor = audio_snapshot.total_pending_nsecs == 0
            || delayed_audio_start_timeline_nsecs.is_some()
            || resume_decision.reset_audio_to_video
            || output_scheduler.audio_gap_recovery_active();
        let scheduler_resume_plan = rebuffer_scheduler_resume_plan(
            reset_video_clock_anchor,
            first_video_timeline_nsecs,
            resume_decision.timeline_nsecs,
            rebuffer_elapsed.unwrap_or_default(),
        );
        apply_rebuffer_scheduler_resume_plan(scheduler, scheduler_resume_plan);
        output_scheduler.mark_video_clock_anchor_valid();
        let scheduler_timeline_nsecs = scheduler.current_timeline_nsecs();
        let scheduler_lateness_ms = (i128::from(scheduler_timeline_nsecs)
            - i128::from(resume_decision.timeline_nsecs))
            as f64
            / 1_000_000.0;
        tracing::debug!(
            session_id = ?session_id,
            scheduler_timeline_nsecs,
            resume_timeline_nsecs = resume_decision.timeline_nsecs,
            scheduler_lateness_ms,
            rebuffer_elapsed_ms = ?rebuffer_elapsed
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            video_clock_anchor_valid = output_scheduler.video_clock_anchor_valid(),
            first_video_timeline_nsecs,
            reset_video_clock_anchor,
            scheduler_resume_plan = ?scheduler_resume_plan,
            "synchronized FFmpeg video scheduler after rebuffer"
        );
        if delayed_audio_start_timeline_nsecs.is_some() || resume_decision.reset_audio_to_video {
            output.reset_clock(resume_decision.timeline_nsecs);
        }
        let mut audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_from_nsecs(audio_start_timeline_nsecs)
            .unwrap_or(audio_start_timeline_nsecs);
        if delayed_audio_start_timeline_nsecs.is_some() {
            let pending_audio_buffered_until_timeline_nsecs = output_scheduler
                .pending_start_audio
                .buffered_until_from(audio_start_timeline_nsecs)
                .unwrap_or(audio_start_timeline_nsecs);
            let prefill_target_nsecs = rebuffer_audio_prefill
                .map(|prefill| prefill.target_nsecs)
                .unwrap_or_else(|| {
                    let queued_video_contiguous_forward_nsecs = output_scheduler
                        .scheduled_video_queue
                        .forward_nsecs_from(audio_start_timeline_nsecs);
                    output_scheduler
                        .audio_rebuffer_prefill_target_nsecs(queued_video_contiguous_forward_nsecs)
                });
            let minimum_prefill_until_timeline_nsecs =
                audio_start_timeline_nsecs.saturating_add(prefill_target_nsecs);
            audio_flush_until_timeline_nsecs = audio_flush_until_timeline_nsecs
                .max(minimum_prefill_until_timeline_nsecs)
                .max(pending_audio_buffered_until_timeline_nsecs);
            tracing::debug!(
                session_id = ?session_id,
                resume_timeline_nsecs = resume_decision.timeline_nsecs,
                audio_start_timeline_nsecs,
                audio_flush_until_timeline_nsecs,
                pending_audio_buffered_until_timeline_nsecs,
                prefill_ms = prefill_target_nsecs as f64 / 1_000_000.0,
                delayed_audio_start_gap_ms = delayed_audio_start_timeline_nsecs
                    .map(|delayed_start_nsecs| {
                        delayed_start_nsecs.saturating_sub(resume_decision.timeline_nsecs) as f64
                            / 1_000_000.0
                    }),
                fill_short_gap_with_silence = delayed_audio_start_silence_policy
                    == DelayedAudioStartSilencePolicy::Allow,
                "prefilling delayed FFmpeg audio after rebuffer video-clock reset"
            );
        }
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            AudioClockMode::UnderrunRecovery,
            delayed_audio_start_silence_policy,
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
        let audio_output_pending_after_flush_nsecs = output.snapshot()?.total_pending_nsecs;
        let should_defer_next_flush = rebuffer_audio_prefill
            .is_none_or(|prefill| audio_output_pending_after_flush_nsecs >= prefill.target_nsecs);
        if should_defer_next_flush {
            output_scheduler.defer_next_pending_start_audio_flush();
        } else if let Some(prefill) = rebuffer_audio_prefill {
            tracing::debug!(
                session_id = ?session_id,
                audio_output_pending_ms =
                    audio_output_pending_after_flush_nsecs as f64 / 1_000_000.0,
                prefill_target_ms = prefill.target_nsecs as f64 / 1_000_000.0,
                loop_recovery = prefill.loop_recovery,
                delayed_audio_start = prefill.delayed_audio_start,
                "kept FFmpeg pending audio flush armed after rebuffer because AO prefill stayed below target"
            );
        }
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
    if waterline.ready() {
        output_scheduler.clear_output_gate_block_log();
    }
    if !waterline.ready() {
        if first_video_frame_pending
            && !output_scheduler.scheduled_video_queue.is_empty()
            && waterline
                .prefetch
                .min_forward_nsecs
                .is_some_and(|duration| duration >= waterline.target_nsecs)
            && waterline
                .decoded_output
                .video_forward_nsecs
                .is_some_and(|duration| duration < waterline.target_nsecs)
            && output_scheduler
                .startup_sync_elapsed()
                .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_START_FIRST_FRAME_STALL_LOG_AFTER)
            && output_scheduler.mark_startup_first_frame_stall_logged()
        {
            tracing::debug!(
                session_id = ?session_id,
                startup_wait_ms = output_scheduler
                    .startup_sync_elapsed()
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                queued_video_frames = output_scheduler.scheduled_video_queue.len(),
                queued_video_ms =
                    output_scheduler.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                decoded_video_range =
                    ?output_scheduler.scheduled_video_queue.range_nsecs(),
                target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                decoded_video_ms = ?waterline
                    .decoded_output.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?waterline
                    .decoded_output.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_min_ms = ?waterline
                    .prefetch.min_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_video_ms = ?waterline
                    .prefetch.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_audio_ms = ?waterline
                    .prefetch.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                demux_read_index,
                video_decoder_pending_packets,
                audio_decode_queued_ms = audio_decode_queued_nsecs as f64 / 1_000_000.0,
                audio_decode_in_flight_packets,
                demux_audio_cached_packets,
                "startup output gate decoded video queue stalled with cached demux ready; requesting video decoder input pump"
            );
        }
        let demux_watermark = timed_output_gate_demux_watermark(&mut demux_watermark, &mut timing);
        let stage_started_at = Instant::now();
        let pending_audio_pressure_context = output_scheduler.pending_audio_pressure_context();
        let startup_pending_pressure_suppressed_hard_reset = pending_audio_pressure_context
            == PendingAudioPressureContext::StartupSync
            && output_scheduler.pending_start_audio.buffered_duration()
                >= PLAYING_PENDING_AUDIO_HARD_RESET_DURATION;
        let rebuffer_audio_prefill_blocked =
            rebuffer_audio_prefill.is_some_and(|prefill| !prefill.ready());
        let rebuffer_empty_audio_output_blocked =
            output_scheduler.playback_output_state.rebuffering()
                && (rebuffer_audio_prefill_blocked
                    || (audio_snapshot.total_pending_nsecs == 0
                        && !output_scheduler.pending_start_audio.is_empty()))
                && !waterline.decoded_output.audio_ready;
        let blocked_on = playback_resume_waterline_blocked_on(waterline);
        let block_detail = if rebuffer_empty_audio_output_blocked {
            if rebuffer_audio_prefill_blocked {
                "rebuffer_audio_prefill"
            } else {
                "rebuffer_empty_audio_output"
            }
        } else {
            blocked_on.as_str()
        };
        let block_log_emission = output_scheduler.observe_output_gate_block_log(
            blocked_on,
            block_detail,
            Instant::now(),
        );
        output_scheduler
            .set_rebuffer_empty_audio_output_blocked(rebuffer_empty_audio_output_blocked);
        if rebuffer_empty_audio_output_blocked && let Some(emission) = block_log_emission {
            let decoded_video_range = output_scheduler.scheduled_video_queue.range_nsecs();
            tracing::debug!(
                session_id = ?session_id,
                blocked_on = block_detail,
                log_kind = emission.log_kind,
                suppressed_repeats = emission.suppressed_repeats,
                blocked_ms = emission.blocked_for.as_secs_f64() * 1000.0,
                rebuffer_anchor_timeline_nsecs =
                    ?rebuffer_anchor.map(|anchor| anchor.timeline_nsecs),
                first_video_timeline_nsecs = ?decoded_video_range.map(|(start, _)| start),
                pending_audio_start_nsecs =
                    ?output_scheduler.pending_start_audio.first_start_timeline_nsecs(),
                decoded_video_ms = ?waterline
                    .decoded_output.video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ms = ?waterline
                    .decoded_output.audio_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                decoded_audio_ready = waterline.decoded_output.audio_ready,
                allow_audio_gap_at_video_resume =
                    resume_decision.allow_audio_gap_at_video_resume,
                reset_audio_to_video = resume_decision.reset_audio_to_video,
                audio_output_pending_ms =
                    audio_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
                prefill_target_ms = ?rebuffer_audio_prefill
                    .map(|prefill| prefill.target_nsecs as f64 / 1_000_000.0),
                prefill_pending_audio_ms = ?rebuffer_audio_prefill
                    .map(|prefill| prefill.pending_audio_forward_nsecs as f64 / 1_000_000.0),
                prefill_loop_recovery = ?rebuffer_audio_prefill
                    .map(|prefill| prefill.loop_recovery),
                resume_timeline_nsecs = resume_decision.timeline_nsecs,
                pending_audio_ms =
                    output_scheduler.pending_start_audio.buffered_duration().as_secs_f64()
                        * 1000.0,
                "FFmpeg output gate rebuffer waiting for audio output prefill"
            );
        }
        if !rebuffer_empty_audio_output_blocked && let Some(emission) = block_log_emission {
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
                    pending_audio_pressure_context.as_str(),
                    startup_pending_pressure_suppressed_hard_reset,
                    emission.log_kind,
                    emission.suppressed_repeats,
                    emission.blocked_for,
                );
        }
        timing.wait_log += stage_started_at.elapsed();
    }
    if first_video_frame_pending && let Some(admission) = initial_admission {
        return Ok(finish_output_gate_resume_timing(
            output_gate_resume_log_context(
                output_scheduler,
                session_id,
                started_at,
                timing,
                initial_start_wait_status(admission),
                Some(waterline),
            ),
        ));
    }
    if waterline.decoded_ready() && !waterline.prefetch.ready {
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

#[cfg(test)]
mod tests {
    use crate::player::render_host::{DecodedFrame, FramePixels, FramePts, RenderSize};

    use super::super::{
        DecodedAudio, DecodedOutputReadiness, PrefetchReadiness, QueuedVideoFrame,
        RebufferResumeAnchor,
    };
    use super::*;

    fn test_audio_snapshot(played_timeline_nsecs: u64) -> AudioOutputSnapshot {
        AudioOutputSnapshot {
            played_timeline_nsecs,
            buffered_until_timeline_nsecs: played_timeline_nsecs,
            shared_pending_nsecs: 0,
            queue_pending_nsecs: 0,
            total_pending_nsecs: 0,
            queue_frames: 0,
            queue_generation: 0,
            ..AudioOutputSnapshot::default()
        }
    }

    fn test_video_frame(timeline_nsecs: u64, duration_nsecs: u64) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            duration_nsecs,
        }
    }

    fn ready_rebuffer_waterline() -> PlaybackResumeWaterline {
        PlaybackResumeWaterline {
            target_nsecs: 1_000_000_000,
            audio_resume_waterline: None,
            decoded_output: DecodedOutputReadiness {
                video_forward_nsecs: Some(1_000_000_000),
                audio_forward_nsecs: Some(1_000_000_000),
                delayed_audio_start_gap_nsecs: None,
                video_ready: true,
                audio_ready: true,
            },
            prefetch: PrefetchReadiness {
                video_forward_nsecs: Some(1_000_000_000),
                audio_forward_nsecs: Some(1_000_000_000),
                min_forward_nsecs: Some(1_000_000_000),
                ready: true,
            },
            allow_audio_gap_at_video_resume: true,
            resume_anchor_source: Default::default(),
        }
    }

    #[test]
    fn ordinary_rebuffer_resume_applies_pause_delay_to_scheduler() {
        let mut scheduler = PlaybackScheduler::new(5_000_000_000);
        scheduler.set_elapsed_for_test(Duration::from_millis(1_200));
        let before_resume = scheduler.current_timeline_nsecs();
        let plan = rebuffer_scheduler_resume_plan(
            false,
            5_000_000_000,
            5_000_000_000,
            Duration::from_secs(1),
        );

        apply_rebuffer_scheduler_resume_plan(&mut scheduler, plan);

        assert_eq!(
            plan,
            RebufferSchedulerResumePlan::DelayBy(Duration::from_secs(1))
        );
        assert!(before_resume >= 6_190_000_000);
        assert!(scheduler.current_timeline_nsecs() < 5_300_000_000);
    }

    #[test]
    fn audio_gap_resume_reanchors_before_draining_one_point_six_seconds() {
        let mut scheduler = PlaybackScheduler::new(0);
        scheduler.set_elapsed_for_test(Duration::from_secs(4));
        let plan = rebuffer_scheduler_resume_plan(
            true,
            10_000_000_000,
            10_100_000_000,
            Duration::from_secs(1),
        );

        apply_rebuffer_scheduler_resume_plan(&mut scheduler, plan);

        assert_eq!(plan, RebufferSchedulerResumePlan::ResetTo(10_100_000_000));
        assert!(!scheduler.ready_for(11_700_000_000));
        std::thread::sleep(Duration::from_millis(10));
        assert!(!scheduler.ready_for(11_700_000_000));
    }

    #[test]
    fn short_rebuffer_audio_gap_uses_silence_from_video_resume_timeline() {
        let resume_timeline_nsecs = 35_040_000_000;
        let audio_snapshot = test_audio_snapshot(resume_timeline_nsecs);
        let short_gap_decision = AudioClockResumeDecision {
            timeline_nsecs: resume_timeline_nsecs,
            reset_audio_to_video: true,
            delayed_audio_start_timeline_nsecs: Some(resume_timeline_nsecs + 42_000_000),
            allow_audio_gap_at_video_resume: true,
            resume_anchor_source: Default::default(),
        };
        assert_eq!(
            rebuffer_audio_flush_start(short_gap_decision, audio_snapshot),
            (resume_timeline_nsecs, DelayedAudioStartSilencePolicy::Allow,)
        );
        let mut scheduler = PlaybackOutputScheduler::new();
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        for index in 0..10_u64 {
            scheduler.push_decoded_video_for_test(test_video_frame(
                resume_timeline_nsecs + index * 40_000_000,
                40_000_000,
            ));
        }
        let audio_start_nsecs = short_gap_decision
            .delayed_audio_start_timeline_nsecs
            .expect("short delayed start");
        let short_prefill_nsecs = duration_nsecs(AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION);
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: short_prefill_nsecs,
            },
            audio_start_nsecs,
            audio_start_nsecs + short_prefill_nsecs,
        );
        let prefill = rebuffer_audio_prefill_status(&scheduler, audio_snapshot, short_gap_decision)
            .expect("short delayed start requests bounded prefill");
        assert_eq!(prefill.target_nsecs, short_prefill_nsecs);
        assert!(prefill.ready());

        let long_gap_decision = AudioClockResumeDecision {
            delayed_audio_start_timeline_nsecs: Some(
                resume_timeline_nsecs + duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE) + 1,
            ),
            ..short_gap_decision
        };
        assert_eq!(
            rebuffer_audio_flush_start(long_gap_decision, audio_snapshot),
            (
                long_gap_decision
                    .delayed_audio_start_timeline_nsecs
                    .expect("long delayed start"),
                DelayedAudioStartSilencePolicy::Skip,
            )
        );
    }

    #[test]
    fn stale_rebuffer_pending_audio_policy_skips_prefill_and_keeps_video_resume_ready() {
        let mut scheduler = PlaybackOutputScheduler::new();
        let resume_nsecs = 35_394_566_033;
        let stale_audio_start_nsecs = 237_802_666_667;
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        scheduler.push_decoded_video_for_test(test_video_frame(resume_nsecs, 1_000_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 500_000_000,
            },
            stale_audio_start_nsecs,
            stale_audio_start_nsecs + 500_000_000,
        );
        let audio_snapshot = test_audio_snapshot(resume_nsecs);

        assert_eq!(
            stale_rebuffer_pending_audio_ahead(&scheduler, audio_snapshot, resume_nsecs),
            Some(stale_audio_start_nsecs)
        );

        let mut resume_decision = AudioClockResumeDecision {
            timeline_nsecs: resume_nsecs,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: None,
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: Default::default(),
        };
        apply_stale_rebuffer_pending_audio_resume_policy(
            &mut resume_decision,
            StaleRebufferPendingAudio::Ahead {
                pending_start_nsecs: stale_audio_start_nsecs,
            },
        );

        assert!(resume_decision.reset_audio_to_video);
        assert!(resume_decision.allow_audio_gap_at_video_resume);
        assert_eq!(
            resume_decision.delayed_audio_start_timeline_nsecs,
            Some(resume_nsecs + MAX_REBUFFER_AUDIO_LEAD_NSECS)
        );

        let prefill_without_stale_clear =
            rebuffer_audio_prefill_status(&scheduler, audio_snapshot, resume_decision)
                .expect("delayed audio resume would normally request prefill");
        assert!(!prefill_without_stale_clear.ready());
        assert_eq!(
            prefill_without_stale_clear.audio_start_timeline_nsecs,
            resume_nsecs + MAX_REBUFFER_AUDIO_LEAD_NSECS
        );

        let blocked_waterline = enforce_rebuffer_audio_prefill_waterline(
            ready_rebuffer_waterline(),
            Some(prefill_without_stale_clear),
            PlaybackSessionId(1),
        );
        assert!(!blocked_waterline.ready());

        let prefill_after_stale_clear = rebuffer_audio_prefill_status_after_stale_pending_audio(
            &scheduler,
            audio_snapshot,
            resume_decision,
            true,
        );
        assert!(prefill_after_stale_clear.is_none());
        assert!(
            enforce_rebuffer_audio_prefill_waterline(
                ready_rebuffer_waterline(),
                prefill_after_stale_clear,
                PlaybackSessionId(1),
            )
            .ready()
        );
    }

    #[test]
    fn stale_rebuffer_pending_audio_behind_skips_prefill_and_allows_video_resume() {
        let mut scheduler = PlaybackOutputScheduler::new();
        let resume_nsecs = 24_000_000_000;
        let pending_audio_start_nsecs = 639_999_984;
        let pending_audio_until_nsecs = 1_639_999_984;
        scheduler.set_state(PlaybackOutputState::Rebuffering);
        scheduler.set_video_output_rebuffer_anchor_for_test(RebufferResumeAnchor {
            timeline_nsecs: 605_805_324,
            reset_to_video_when_decoded_queue_misses_anchor: true,
        });
        scheduler.push_decoded_video_for_test(test_video_frame(resume_nsecs, 1_000_000_000));
        scheduler.push_pending_start_audio_for_test(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: pending_audio_until_nsecs - pending_audio_start_nsecs,
            },
            pending_audio_start_nsecs,
            pending_audio_until_nsecs,
        );
        let audio_snapshot = test_audio_snapshot(605_805_324);

        let stale_audio = stale_rebuffer_pending_audio(&scheduler, audio_snapshot, resume_nsecs)
            .expect("stale behind pending audio");
        assert_eq!(
            stale_audio,
            StaleRebufferPendingAudio::Behind {
                pending_start_nsecs: pending_audio_start_nsecs,
                pending_until_nsecs: Some(pending_audio_until_nsecs),
            }
        );

        let mut resume_decision = AudioClockResumeDecision {
            timeline_nsecs: resume_nsecs,
            reset_audio_to_video: false,
            delayed_audio_start_timeline_nsecs: Some(resume_nsecs + 5_000_000_000),
            allow_audio_gap_at_video_resume: false,
            resume_anchor_source: Default::default(),
        };
        apply_stale_rebuffer_pending_audio_resume_policy(&mut resume_decision, stale_audio);

        assert!(resume_decision.reset_audio_to_video);
        assert!(resume_decision.allow_audio_gap_at_video_resume);
        assert_eq!(resume_decision.delayed_audio_start_timeline_nsecs, None);
        assert!(
            rebuffer_audio_prefill_status_after_stale_pending_audio(
                &scheduler,
                audio_snapshot,
                resume_decision,
                true,
            )
            .is_none()
        );
        assert!(
            enforce_rebuffer_audio_prefill_waterline(
                ready_rebuffer_waterline(),
                None,
                PlaybackSessionId(1),
            )
            .ready()
        );
    }
}
