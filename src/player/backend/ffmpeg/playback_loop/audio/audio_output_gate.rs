use std::{
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::pending_audio_queue::{PendingStartAudio, PendingStartAudioFrame};
use super::playback_block::PlaybackBlockReason;
use super::scheduled_video_queue::ScheduledVideoQueue;
use super::video_output_gate::{AudioClockedVideoDrainStatus, admit_decoded_video_frame_to_vo};
use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioClockMode,
    AudioOutput, AudioOutputPushResult, BufferedReporter, DecodedAudio, FfmpegControl,
    PENDING_AUDIO_CONTINUITY_TOLERANCE, PositionReporter, SubtitlePipeline,
    audio_elements_for_frames, audio_frames_for_duration_round, duration_nsecs,
};

const AUDIO_STAGE_SERVICE_BUDGET: Duration = Duration::from_millis(2);
const AUDIO_STAGE_SERVICE_MAX_FRAMES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PendingAudioUnderrunRecoveryPlan {
    pub(in crate::player::backend::ffmpeg) audio_start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) audio_flush_until_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) reset_audio_to_timeline_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DelayedAudioStartSilenceStatus {
    NotNeeded,
    Queued {
        end_timeline_nsecs: u64,
        samples: usize,
    },
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum DelayedAudioStartSilencePolicy {
    Allow,
    Skip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioStageResult {
    pub(in crate::player::backend::ffmpeg) made_progress: bool,
    pub(in crate::player::backend::ffmpeg) staged_frames: usize,
    pub(in crate::player::backend::ffmpeg) staged_samples: usize,
    pub(in crate::player::backend::ffmpeg) staged_range_nsecs: Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg) interrupted: bool,
    pub(in crate::player::backend::ffmpeg) would_block: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioStageCheckpoint {
    PendingPopped(usize),
    FirstEnqueued,
}

impl AudioStageResult {
    fn observe_staged_frame(
        &mut self,
        samples: usize,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
    ) {
        self.made_progress = true;
        self.staged_frames = self.staged_frames.saturating_add(1);
        self.staged_samples = self.staged_samples.saturating_add(samples);
        self.staged_range_nsecs = Some(match self.staged_range_nsecs {
            Some((start, end)) => (start.min(start_timeline_nsecs), end.max(end_timeline_nsecs)),
            None => (start_timeline_nsecs, end_timeline_nsecs),
        });
    }
}

pub(in crate::player::backend::ffmpeg) fn pending_audio_underrun_recovery_plan(
    pending_audio: &PendingStartAudio,
    played_timeline_nsecs: u64,
    output_pending_nsecs: u64,
    video_start_timeline_nsecs: Option<u64>,
    video_buffered_until_nsecs: Option<u64>,
) -> Option<PendingAudioUnderrunRecoveryPlan> {
    if pending_audio.is_empty() {
        return None;
    }

    let video_start_timeline_nsecs = video_start_timeline_nsecs?;
    let video_buffered_until_nsecs = video_buffered_until_nsecs?;
    let recovery_nsecs = duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION);
    if output_pending_nsecs > 0 && video_start_timeline_nsecs > played_timeline_nsecs {
        return None;
    }
    let mut audio_start_timeline_nsecs = played_timeline_nsecs.max(video_start_timeline_nsecs);
    let mut reset_audio_to_timeline_nsecs =
        (audio_start_timeline_nsecs != played_timeline_nsecs).then_some(audio_start_timeline_nsecs);
    let mut pending_buffered_until_nsecs =
        pending_audio.buffered_until_from(audio_start_timeline_nsecs);

    if pending_buffered_until_nsecs.is_none() {
        if output_pending_nsecs > 0 {
            return None;
        }
        let next_audio_start_nsecs =
            pending_audio.first_start_at_or_after(audio_start_timeline_nsecs)?;
        audio_start_timeline_nsecs = next_audio_start_nsecs;
        reset_audio_to_timeline_nsecs = Some(next_audio_start_nsecs);
        pending_buffered_until_nsecs =
            pending_audio.buffered_until_from(audio_start_timeline_nsecs);
    }

    let pending_buffered_until_nsecs = pending_buffered_until_nsecs?;
    if pending_buffered_until_nsecs <= audio_start_timeline_nsecs {
        return None;
    }
    if audio_start_timeline_nsecs < video_start_timeline_nsecs
        || audio_start_timeline_nsecs >= video_buffered_until_nsecs
    {
        return None;
    }

    let minimum_flush_until_nsecs = audio_start_timeline_nsecs.saturating_add(recovery_nsecs);
    if video_buffered_until_nsecs < minimum_flush_until_nsecs {
        return None;
    }
    let video_lead_until_nsecs =
        video_buffered_until_nsecs.saturating_add(duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION));
    if video_lead_until_nsecs < minimum_flush_until_nsecs {
        return None;
    }
    let audio_flush_until_timeline_nsecs = video_lead_until_nsecs.min(pending_buffered_until_nsecs);

    (audio_flush_until_timeline_nsecs >= minimum_flush_until_nsecs).then_some(
        PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            reset_audio_to_timeline_nsecs,
        },
    )
}

pub(in crate::player::backend::ffmpeg) fn discard_stale_pending_audio_before_recovery_start(
    pending_audio: &mut PendingStartAudio,
    played_timeline_nsecs: u64,
    output_pending_nsecs: u64,
    video_start_timeline_nsecs: Option<u64>,
) -> usize {
    if pending_audio.is_empty() || output_pending_nsecs > 0 {
        return 0;
    }

    let Some(video_start_timeline_nsecs) = video_start_timeline_nsecs else {
        return 0;
    };
    let recovery_start_timeline_nsecs = played_timeline_nsecs.max(video_start_timeline_nsecs);
    if pending_audio
        .buffered_until_from(recovery_start_timeline_nsecs)
        .is_some()
    {
        return 0;
    }

    pending_audio.discard_before(recovery_start_timeline_nsecs)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn flush_pending_start_audio(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    audio_start_timeline_nsecs: u64,
    audio_flush_until_timeline_nsecs: u64,
    clock_mode: AudioClockMode,
    delayed_audio_start_silence: DelayedAudioStartSilencePolicy,
    control: &FfmpegControl,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    let expected_audio_epoch = output.audio_epoch();
    let result = stage_pending_audio(
        pending_audio,
        output,
        expected_audio_epoch,
        audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        clock_mode,
        delayed_audio_start_silence,
        control,
        queued_video_frames,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;
    if result.staged_frames > 0 {
        output.activate_audio_output(expected_audio_epoch, control.seek_generation(), control);
    }
    Ok(result.made_progress)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn stage_pending_audio(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    expected_audio_epoch: u64,
    audio_start_timeline_nsecs: u64,
    audio_flush_until_timeline_nsecs: u64,
    clock_mode: AudioClockMode,
    delayed_audio_start_silence: DelayedAudioStartSilencePolicy,
    control: &FfmpegControl,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<AudioStageResult, String> {
    stage_pending_audio_with_checkpoint(
        pending_audio,
        output,
        expected_audio_epoch,
        audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        clock_mode,
        delayed_audio_start_silence,
        control,
        queued_video_frames,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
        |_| {},
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn stage_pending_audio_with_checkpoint(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    expected_audio_epoch: u64,
    audio_start_timeline_nsecs: u64,
    audio_flush_until_timeline_nsecs: u64,
    clock_mode: AudioClockMode,
    delayed_audio_start_silence: DelayedAudioStartSilencePolicy,
    control: &FfmpegControl,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    mut observe_checkpoint: impl FnMut(AudioStageCheckpoint),
) -> std::result::Result<AudioStageResult, String> {
    let stage_started_at = Instant::now();
    let mut result = AudioStageResult::default();
    let dropped_before_start = pending_audio.discard_before(audio_start_timeline_nsecs);
    if dropped_before_start > 0 {
        tracing::trace!(
            dropped_audio_frames = dropped_before_start,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            "discarded buffered FFmpeg audio before video-covered start"
        );
    }

    if audio_flush_until_timeline_nsecs <= audio_start_timeline_nsecs {
        return Ok(result);
    }

    let mut queued_audio_frames = 0usize;
    let mut queued_audio_until_nsecs = audio_start_timeline_nsecs;
    if delayed_audio_start_silence == DelayedAudioStartSilencePolicy::Allow {
        match queue_delayed_audio_start_silence(
            pending_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            clock_mode,
            expected_audio_epoch,
            control,
            session_id,
        )? {
            DelayedAudioStartSilenceStatus::NotNeeded => {}
            DelayedAudioStartSilenceStatus::Queued {
                end_timeline_nsecs,
                samples,
            } => {
                queued_audio_frames = queued_audio_frames.saturating_add(1);
                queued_audio_until_nsecs = end_timeline_nsecs;
                result.observe_staged_frame(
                    samples,
                    audio_start_timeline_nsecs,
                    end_timeline_nsecs,
                );
                observe_checkpoint(AudioStageCheckpoint::FirstEnqueued);
            }
            DelayedAudioStartSilenceStatus::Blocked => {
                result.would_block = true;
                return Ok(result);
            }
        }
    }

    let mut popped_audio_frames = 0usize;
    while let Some(mut frame) = pending_audio.pop_front_until(audio_flush_until_timeline_nsecs) {
        if popped_audio_frames >= AUDIO_STAGE_SERVICE_MAX_FRAMES
            || stage_started_at.elapsed() >= AUDIO_STAGE_SERVICE_BUDGET
        {
            pending_audio.push_front_frame(frame);
            result.would_block = true;
            break;
        }
        observe_checkpoint(AudioStageCheckpoint::PendingPopped(popped_audio_frames));
        popped_audio_frames = popped_audio_frames.saturating_add(1);
        if frame.start_timeline_nsecs
            > queued_audio_until_nsecs
                .saturating_add(duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
        {
            pending_audio.push_front_frame(frame);
            break;
        }
        let trim_before_timeline_nsecs = audio_start_timeline_nsecs.max(queued_audio_until_nsecs);
        if !frame.trim_before(
            trim_before_timeline_nsecs,
            output.sample_rate(),
            output.channels(),
        ) {
            continue;
        }
        merge_small_pending_audio_gap(&mut frame, queued_audio_until_nsecs, clock_mode, session_id);
        let staged_start_timeline_nsecs = frame.start_timeline_nsecs;
        let buffered_until_nsecs = frame.end_timeline_nsecs;
        let staged_samples = frame.samples.len();
        let push_result = match output.try_push_timed_for_epoch(
            frame.samples,
            frame.start_timeline_nsecs,
            frame.end_timeline_nsecs,
            expected_audio_epoch,
            control,
        ) {
            Ok(result) => result,
            Err(error) => {
                frame.samples = error.samples;
                pending_audio.push_front_frame(frame);
                return Err(error.message);
            }
        };
        match push_result {
            AudioOutputPushResult::Queued => {
                queued_audio_frames = queued_audio_frames.saturating_add(1);
                queued_audio_until_nsecs = buffered_until_nsecs;
                result.observe_staged_frame(
                    staged_samples,
                    staged_start_timeline_nsecs,
                    buffered_until_nsecs,
                );
                if queued_audio_frames == 1 {
                    observe_checkpoint(AudioStageCheckpoint::FirstEnqueued);
                }
            }
            AudioOutputPushResult::WouldBlock {
                samples,
                queued_frames,
                queued_duration,
            } => {
                frame.samples = samples;
                pending_audio.push_front_frame(frame);
                if let Some(audio_snapshot) = output.try_snapshot()? {
                    result.made_progress |= present_due_audio_clocked_frames_to_vo(
                        queued_video_frames,
                        audio_snapshot.played_timeline_nsecs,
                        session_id,
                        vo_queue,
                        frame_presented,
                        position_reporter,
                        event_tx,
                    );
                    subtitle_pipeline.update_overlay(
                        audio_snapshot.played_timeline_nsecs,
                        session_id,
                        event_tx,
                    );
                }
                tracing::debug!(
                    session_id = ?session_id,
                    blocked_on = PlaybackBlockReason::AudioOutput.as_str(),
                    queued_audio_frames = queued_frames,
                    queued_audio_ms = queued_duration.as_secs_f64() * 1000.0,
                    pending_audio_frames = pending_audio.len(),
                    pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
                    clock_mode = clock_mode.as_str(),
                    audio_start_timeline_nsecs,
                    audio_flush_until_timeline_nsecs,
                    "audio output queue full while flushing pending FFmpeg audio"
                );
                result.would_block = true;
                return Ok(result);
            }
            AudioOutputPushResult::Interrupted { samples } => {
                frame.samples = samples;
                pending_audio.push_front_frame(frame);
                result.interrupted = true;
                return Ok(result);
            }
        }
        buffered_reporter.report_audio_timeline_nsecs(buffered_until_nsecs, session_id, event_tx);
    }
    if queued_audio_frames > 0 {
        tracing::trace!(
            queued_audio_frames,
            queued_audio_until_nsecs,
            pending_audio_frames = pending_audio.len(),
            pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
            clock_mode = clock_mode.as_str(),
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            "queued buffered FFmpeg audio covered by decoded video"
        );
    }
    Ok(result)
}

fn merge_small_pending_audio_gap(
    frame: &mut PendingStartAudioFrame,
    contiguous_timeline_nsecs: u64,
    clock_mode: AudioClockMode,
    session_id: PlaybackSessionId,
) {
    if frame.start_timeline_nsecs <= contiguous_timeline_nsecs {
        return;
    }
    let gap_nsecs = frame
        .start_timeline_nsecs
        .saturating_sub(contiguous_timeline_nsecs);
    if gap_nsecs > duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE) {
        return;
    }

    let original_start_timeline_nsecs = frame.start_timeline_nsecs;
    frame.start_timeline_nsecs = contiguous_timeline_nsecs;
    tracing::trace!(
        session_id = ?session_id,
        original_start_timeline_nsecs,
        merged_start_timeline_nsecs = contiguous_timeline_nsecs,
        small_audio_gap_nsecs = gap_nsecs,
        clock_mode = clock_mode.as_str(),
        silence_fill_reason = "not_filled_small_gap",
        "merged small pending FFmpeg audio gap without inserting silence"
    );
}

#[allow(clippy::too_many_arguments)]
fn queue_delayed_audio_start_silence(
    pending_audio: &PendingStartAudio,
    output: &AudioOutput,
    audio_start_timeline_nsecs: u64,
    audio_flush_until_timeline_nsecs: u64,
    clock_mode: AudioClockMode,
    expected_audio_epoch: u64,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
) -> std::result::Result<DelayedAudioStartSilenceStatus, String> {
    if pending_audio
        .buffered_until_from(audio_start_timeline_nsecs)
        .is_some_and(|buffered_until| buffered_until > audio_start_timeline_nsecs)
    {
        return Ok(DelayedAudioStartSilenceStatus::NotNeeded);
    }

    let Some(first_audio_start_nsecs) =
        pending_audio.first_start_at_or_after(audio_start_timeline_nsecs)
    else {
        return Ok(DelayedAudioStartSilenceStatus::NotNeeded);
    };
    if first_audio_start_nsecs <= audio_start_timeline_nsecs
        || first_audio_start_nsecs > audio_flush_until_timeline_nsecs
    {
        return Ok(DelayedAudioStartSilenceStatus::NotNeeded);
    }

    let gap_nsecs = first_audio_start_nsecs.saturating_sub(audio_start_timeline_nsecs);
    if gap_nsecs <= duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE)
        || gap_nsecs > duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)
    {
        return Ok(DelayedAudioStartSilenceStatus::NotNeeded);
    }

    let audio_gap_frames = audio_frames_for_duration_round(gap_nsecs, output.sample_rate());
    let silence_samples = usize::try_from(audio_elements_for_frames(
        audio_gap_frames,
        output.channels(),
    ))
    .map_err(|_| "延迟音频启动静音缓冲区过大".to_string())?;
    if silence_samples == 0 {
        return Ok(DelayedAudioStartSilenceStatus::NotNeeded);
    }

    let push_result = output
        .try_push_timed_for_epoch(
            vec![0.0; silence_samples],
            audio_start_timeline_nsecs,
            first_audio_start_nsecs,
            expected_audio_epoch,
            control,
        )
        .map_err(|error| error.message)?;
    match push_result {
        AudioOutputPushResult::Queued => {
            tracing::debug!(
                session_id = ?session_id,
                audio_start_timeline_nsecs,
                first_audio_start_nsecs,
                delayed_audio_start_silence_ms = gap_nsecs as f64 / 1_000_000.0,
                silence_samples,
                audio_gap_frames,
                silence_fill_reason = "delayed_audio_start",
                clock_mode = clock_mode.as_str(),
                misaligned_audio_buffer_count = output.misaligned_audio_buffer_count(),
                "queued silence before delayed FFmpeg audio start"
            );
            Ok(DelayedAudioStartSilenceStatus::Queued {
                end_timeline_nsecs: first_audio_start_nsecs,
                samples: silence_samples,
            })
        }
        AudioOutputPushResult::WouldBlock {
            queued_frames,
            queued_duration,
            ..
        } => {
            tracing::debug!(
                session_id = ?session_id,
                blocked_on = PlaybackBlockReason::AudioOutput.as_str(),
                queued_audio_frames = queued_frames,
                queued_audio_ms = queued_duration.as_secs_f64() * 1000.0,
                audio_start_timeline_nsecs,
                first_audio_start_nsecs,
                delayed_audio_start_silence_ms = gap_nsecs as f64 / 1_000_000.0,
                audio_gap_frames,
                silence_fill_reason = "delayed_audio_start",
                clock_mode = clock_mode.as_str(),
                misaligned_audio_buffer_count = output.misaligned_audio_buffer_count(),
                "audio output queue full while queuing delayed-start silence"
            );
            Ok(DelayedAudioStartSilenceStatus::Blocked)
        }
        AudioOutputPushResult::Interrupted { .. } => Ok(DelayedAudioStartSilenceStatus::Blocked),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn recover_pending_start_audio_after_underrun(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    control: &FfmpegControl,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    if pending_audio.is_empty() {
        return Ok(false);
    }

    let audio_snapshot = output.snapshot()?;
    let output_starved = audio_snapshot.total_pending_nsecs == 0;
    let recovering_underrun = output.underrun_active();
    if !output_starved && !recovering_underrun {
        return Ok(false);
    }

    let queued_video_range_nsecs = queued_video_frames.range_nsecs();
    let Some(plan) = pending_audio_underrun_recovery_plan(
        pending_audio,
        audio_snapshot.played_timeline_nsecs,
        audio_snapshot.total_pending_nsecs,
        queued_video_range_nsecs.map(|(start, _)| start),
        queued_video_range_nsecs.map(|(_, end)| end),
    ) else {
        let dropped_audio_frames = discard_stale_pending_audio_before_recovery_start(
            pending_audio,
            audio_snapshot.played_timeline_nsecs,
            audio_snapshot.total_pending_nsecs,
            queued_video_range_nsecs.map(|(start, _)| start),
        );
        if dropped_audio_frames > 0 {
            tracing::debug!(
                session_id = ?session_id,
                dropped_audio_frames,
                played_timeline_nsecs = audio_snapshot.played_timeline_nsecs,
                output_pending_ms = audio_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
                video_start_timeline_nsecs = ?queued_video_range_nsecs.map(|(start, _)| start),
                pending_audio_frames = pending_audio.len(),
                pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
                "discarded stale pending FFmpeg audio before underrun recovery start"
            );
            return Ok(true);
        }
        return Ok(false);
    };

    let mut dropped_video_frames = 0usize;
    if let Some(reset_timeline_nsecs) = plan.reset_audio_to_timeline_nsecs {
        dropped_video_frames = queued_video_frames.discard_before(reset_timeline_nsecs);
        output.reset_clock(reset_timeline_nsecs);
    }

    let made_progress = flush_pending_start_audio(
        pending_audio,
        output,
        plan.audio_start_timeline_nsecs,
        plan.audio_flush_until_timeline_nsecs,
        AudioClockMode::UnderrunRecovery,
        DelayedAudioStartSilencePolicy::Skip,
        control,
        queued_video_frames,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
        subtitle_pipeline,
        buffered_reporter,
    )?;

    if made_progress {
        let recovered_snapshot = output.snapshot()?;
        tracing::debug!(
            session_id = ?session_id,
            audio_start_timeline_nsecs = plan.audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs = plan.audio_flush_until_timeline_nsecs,
            reset_audio_to_timeline_nsecs = ?plan.reset_audio_to_timeline_nsecs,
            dropped_video_frames,
            pending_audio_frames = pending_audio.len(),
            pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
            audio_output_pending_ms = recovered_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
            "recovered native audio output underrun from pending FFmpeg audio"
        );
    }

    Ok(made_progress)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn push_decoded_audio_to_output(
    output: &AudioOutput,
    control: &FfmpegControl,
    audio: DecodedAudio,
    start_timeline_nsecs: u64,
    end_timeline_nsecs: u64,
    pending_audio: &mut PendingStartAudio,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
) -> std::result::Result<bool, String> {
    let audio_snapshot = output.snapshot()?;
    let played_timeline_nsecs = audio_snapshot.played_timeline_nsecs;
    if start_timeline_nsecs < played_timeline_nsecs || end_timeline_nsecs <= played_timeline_nsecs {
        tracing::trace!(
            start_timeline_nsecs,
            end_timeline_nsecs,
            played_timeline_nsecs,
            "dropping late decoded FFmpeg audio before output queue"
        );
        return Ok(false);
    }
    let buffered_until_nsecs = end_timeline_nsecs;
    let has_consumable_audio =
        !audio.samples.is_empty() && end_timeline_nsecs > start_timeline_nsecs;
    let audio_duration_nsecs = audio.duration_nsecs;
    let push_result = match output.try_push_timed(
        audio.samples,
        start_timeline_nsecs,
        end_timeline_nsecs,
        control,
    ) {
        Ok(result) => result,
        Err(error) => {
            pending_audio.push(
                DecodedAudio {
                    samples: error.samples,
                    duration_nsecs: audio_duration_nsecs,
                },
                start_timeline_nsecs,
                end_timeline_nsecs,
            );
            return Err(error.message);
        }
    };
    match push_result {
        AudioOutputPushResult::Queued => {
            if !has_consumable_audio {
                return Ok(false);
            }
            output.activate_current_audio_output(control);
            buffered_reporter.report_audio_timeline_nsecs(
                buffered_until_nsecs,
                session_id,
                event_tx,
            );
            Ok(true)
        }
        AudioOutputPushResult::WouldBlock {
            samples,
            queued_frames,
            queued_duration,
        } => {
            pending_audio.push(
                DecodedAudio {
                    samples,
                    duration_nsecs: audio_duration_nsecs,
                },
                start_timeline_nsecs,
                end_timeline_nsecs,
            );
            let made_progress = if let Some(audio_snapshot) = output.try_snapshot()? {
                let made_progress = present_due_audio_clocked_frames_to_vo(
                    queued_video_frames,
                    audio_snapshot.played_timeline_nsecs,
                    session_id,
                    vo_queue,
                    frame_presented,
                    position_reporter,
                    event_tx,
                );
                subtitle_pipeline.update_overlay(
                    audio_snapshot.played_timeline_nsecs,
                    session_id,
                    event_tx,
                );
                made_progress
            } else {
                false
            };
            tracing::debug!(
                session_id = ?session_id,
                blocked_on = PlaybackBlockReason::AudioOutput.as_str(),
                queued_audio_frames = queued_frames,
                queued_audio_ms = queued_duration.as_secs_f64() * 1000.0,
                pending_audio_frames = pending_audio.len(),
                pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
                start_timeline_nsecs,
                end_timeline_nsecs,
                "audio output queue full while queuing decoded FFmpeg audio"
            );
            Ok(made_progress)
        }
        AudioOutputPushResult::Interrupted { samples } => {
            pending_audio.push(
                DecodedAudio {
                    samples,
                    duration_nsecs: audio.duration_nsecs,
                },
                start_timeline_nsecs,
                end_timeline_nsecs,
            );
            Ok(false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn present_due_audio_clocked_frames_to_vo(
    queued_video_frames: &mut ScheduledVideoQueue,
    played_until_nsecs: u64,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) -> bool {
    if queued_video_frames.deadline_service_owns_presentation() {
        return false;
    }
    let pop_result = queued_video_frames.pop_audio_clocked_frame(played_until_nsecs);
    let mut made_progress = pop_result.dropped_frames > 0;
    if pop_result.dropped_frames > 0 {
        let recent_coordinator_stall = queued_video_frames.recent_coordinator_stall(Instant::now());
        tracing::debug!(
            dropped_video_frames = pop_result.dropped_frames,
            scheduler_dropped_video_frames =
                queued_video_frames.scheduler_dropped_video_frames(),
            recent_coordinator_stall_ms = ?recent_coordinator_stall
                .map(|stall| stall.elapsed.as_secs_f64() * 1000.0),
            recent_coordinator_stall_age_ms = ?recent_coordinator_stall
                .map(|stall| stall.age.as_secs_f64() * 1000.0),
            played_until_nsecs,
            queued_video_frames = queued_video_frames.len(),
            "VO admission dropped stale audio-clocked video frames"
        );
    }
    if let Some(frame) = pop_result.frame {
        admit_decoded_video_frame_to_vo(
            frame.frame,
            session_id,
            frame.timeline_nsecs,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
        );
        made_progress = true;
    }
    made_progress
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_audio_clocked_video_queue(
    output: &AudioOutput,
    _control: &FfmpegControl,
    queued_video_frames: &mut ScheduledVideoQueue,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
) -> std::result::Result<bool, String> {
    let audio_snapshot = output.snapshot()?;
    let made_progress = present_due_audio_clocked_frames_to_vo(
        queued_video_frames,
        audio_snapshot.played_timeline_nsecs,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
    );
    subtitle_pipeline.update_overlay(audio_snapshot.played_timeline_nsecs, session_id, event_tx);

    let needs_prefetch = subtitle_pipeline.needs_prefetch();
    if queued_video_frames.limit_reached(needs_prefetch) {
        let vo_snapshot = vo_queue.snapshot();
        let backpressure = vo_snapshot.render_backpressure;
        let audio_snapshot = output.snapshot()?;
        let blocked_on = if vo_snapshot.render_backlogged() {
            PlaybackBlockReason::RenderWorker
        } else if vo_snapshot.queued_frames >= vo_snapshot.queue_capacity {
            PlaybackBlockReason::VideoOutputQueue
        } else {
            PlaybackBlockReason::AudioOutput
        };
        if let Some(suppressed_repeats) =
            queued_video_frames.prebuffer_limit_log_summary(Instant::now())
        {
            tracing::debug!(
                session_id = ?session_id,
                blocked_on = blocked_on.as_str(),
                suppressed_repeats,
                queued_frames = queued_video_frames.len(),
                queue_duration_ms = queued_video_frames.duration().as_secs_f64() * 1000.0,
                limit_frames = queued_video_frames.limit_frames(needs_prefetch),
                target_frames = queued_video_frames.target_frames(needs_prefetch),
                limit_duration_ms = queued_video_frames
                    .limit_duration(needs_prefetch)
                    .as_secs_f64()
                    * 1000.0,
                target_duration_ms = queued_video_frames
                    .target_duration(needs_prefetch)
                    .as_secs_f64()
                    * 1000.0,
                vo_queued_frames = vo_snapshot.queued_frames,
                vo_queue_capacity = vo_snapshot.queue_capacity,
                vo_dropped_frames = vo_snapshot.dropped_frames,
                render_backlogged = vo_snapshot.render_backlogged(),
                pending_render_requests = backpressure.pending_requests,
                render_last_ms = backpressure.last_render_nsecs as f64 / 1_000_000.0,
                render_avg_ms = backpressure.average_render_nsecs as f64 / 1_000_000.0,
                pending_audio_ms = audio_snapshot.total_pending_nsecs as f64 / 1_000_000.0,
                audio_output_shared_ms = audio_snapshot.shared_pending_nsecs as f64 / 1_000_000.0,
                audio_output_queue_ms = audio_snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
                audio_output_queue_frames = audio_snapshot.queue_frames,
                "decoded FFmpeg video queue reached prebuffer limit; leaving backpressure to decoder/VO"
            );
        }
    }

    Ok(made_progress)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn drain_audio_clocked_decoded_video_step<F>(
    queued_video_frames: &mut ScheduledVideoQueue,
    audio_output: &AudioOutput,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    mut on_audio_progress: F,
) -> std::result::Result<AudioClockedVideoDrainStatus, String>
where
    F: FnMut(u64),
{
    if control.should_interrupt() {
        return Ok(AudioClockedVideoDrainStatus::Interrupted);
    }
    if queued_video_frames.is_empty() {
        return Ok(AudioClockedVideoDrainStatus::Drained);
    }
    let audio_snapshot = audio_output.snapshot()?;
    let made_progress = present_due_audio_clocked_frames_to_vo(
        queued_video_frames,
        audio_snapshot.played_timeline_nsecs,
        session_id,
        vo_queue,
        frame_presented,
        position_reporter,
        event_tx,
    );
    on_audio_progress(audio_snapshot.played_timeline_nsecs);
    if queued_video_frames.is_empty() || audio_snapshot.total_pending_nsecs == 0 {
        return Ok(AudioClockedVideoDrainStatus::Drained);
    }
    Ok(AudioClockedVideoDrainStatus::WaitingAudio { made_progress })
}

#[cfg(test)]
mod tests {
    use crate::player::render_host::PlaybackSessionId;

    use super::super::AudioClockMode;
    use super::super::pending_audio_queue::PendingStartAudioFrame;
    use super::merge_small_pending_audio_gap;

    fn pending_audio_frame(start_timeline_nsecs: u64) -> PendingStartAudioFrame {
        PendingStartAudioFrame {
            samples: vec![0.0; 4],
            start_timeline_nsecs,
            end_timeline_nsecs: start_timeline_nsecs.saturating_add(20_000_000),
        }
    }

    #[test]
    fn merge_small_pending_audio_gap_moves_start_without_silence() {
        let mut frame = pending_audio_frame(1_004_000_000);

        merge_small_pending_audio_gap(
            &mut frame,
            1_000_000_000,
            AudioClockMode::AudioStarted,
            PlaybackSessionId(1),
        );

        assert_eq!(frame.start_timeline_nsecs, 1_000_000_000);
        assert_eq!(frame.end_timeline_nsecs, 1_024_000_000);
        assert_eq!(frame.samples.len(), 4);
    }

    #[test]
    fn merge_small_pending_audio_gap_leaves_large_gap_for_clock_policy() {
        let mut frame = pending_audio_frame(1_006_000_000);

        merge_small_pending_audio_gap(
            &mut frame,
            1_000_000_000,
            AudioClockMode::AudioStarted,
            PlaybackSessionId(1),
        );

        assert_eq!(frame.start_timeline_nsecs, 1_006_000_000);
        assert_eq!(frame.end_timeline_nsecs, 1_026_000_000);
    }
}
