use super::OutputGateResumeStatus;
use super::{
    AtomicBool, AudioClockMode, AudioOutput, BackendEvent, BufferedReporter,
    DelayedAudioStartSilencePolicy, FfmpegControl, InitialOutputSyncDecision,
    PlaybackOutputScheduler, PlaybackOutputState, PlaybackScheduler, PlaybackSessionId,
    PositionReporter, Sender, SubtitlePipeline, VideoOutputQueue, flush_pending_start_audio,
    nsecs_to_seconds, present_video_frame_to_vo,
};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_delayed_audio_start_timeline_nsecs(
    output_scheduler: &PlaybackOutputScheduler,
    sync_decision: InitialOutputSyncDecision,
) -> Option<u64> {
    if let Some(audio_start_timeline_nsecs) =
        output_scheduler.initial_delayed_audio_start_timeline_nsecs
    {
        return Some(audio_start_timeline_nsecs);
    }
    if !output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        return None;
    }
    sync_decision.delayed_audio_start_timeline_nsecs
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn service_initial_video_clock_until_audio_start(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: &AudioOutput,
    delayed_audio_start_timeline_nsecs: u64,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    current_start_position_nsecs: &mut u64,
    scheduler: &mut PlaybackScheduler,
) -> std::result::Result<OutputGateResumeStatus, String> {
    let Some((first_video_timeline_nsecs, _)) =
        output_scheduler.scheduled_video_queue.range_nsecs()
    else {
        return Ok(OutputGateResumeStatus::Waiting);
    };

    if output_scheduler
        .initial_delayed_audio_start_timeline_nsecs
        .is_none()
    {
        output_scheduler.initial_delayed_audio_start_timeline_nsecs =
            Some(delayed_audio_start_timeline_nsecs);
        output_scheduler.set_state(PlaybackOutputState::Ready);
        if first_video_timeline_nsecs > *current_start_position_nsecs {
            *current_start_position_nsecs = first_video_timeline_nsecs;
            subtitle_pipeline.reset_cues_for_position(first_video_timeline_nsecs);
            buffered_reporter.reset_to(
                nsecs_to_seconds(first_video_timeline_nsecs),
                session_id,
                event_tx,
            );
        }
        scheduler.reset(first_video_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            first_video_timeline_nsecs,
            delayed_audio_start_timeline_nsecs,
            delayed_audio_start_gap_ms = delayed_audio_start_timeline_nsecs
                .saturating_sub(first_video_timeline_nsecs) as f64
                / 1_000_000.0,
            silence_fill_reason = "not_filled",
            clock_mode = AudioClockMode::SyncingVideo.as_str(),
            "starting video-clocked initial playback until first FFmpeg audio frame"
        );
    }

    let mut presented_video_frames = 0usize;
    while let Some((front_timeline_nsecs, _)) = output_scheduler.scheduled_video_queue.range_nsecs()
    {
        if front_timeline_nsecs >= delayed_audio_start_timeline_nsecs
            || !scheduler.ready_for(front_timeline_nsecs)
            || control.should_interrupt()
        {
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
        presented_video_frames = presented_video_frames.saturating_add(1);
    }

    if !scheduler.ready_for(delayed_audio_start_timeline_nsecs) || control.should_interrupt() {
        if presented_video_frames > 0 {
            tracing::trace!(
                session_id = ?session_id,
                presented_video_frames,
                delayed_audio_start_timeline_nsecs,
                clock_mode = AudioClockMode::SyncingVideo.as_str(),
                "presented initial FFmpeg video frames while waiting for first audio PTS"
            );
        }
        return Ok(OutputGateResumeStatus::Waiting);
    }

    output.reset_clock(delayed_audio_start_timeline_nsecs);
    let audio_flush_until_timeline_nsecs = output_scheduler
        .scheduled_video_queue
        .buffered_until_from_nsecs(delayed_audio_start_timeline_nsecs)
        .or_else(|| {
            output_scheduler
                .pending_start_audio
                .buffered_until_from(delayed_audio_start_timeline_nsecs)
        })
        .unwrap_or(delayed_audio_start_timeline_nsecs);
    flush_pending_start_audio(
        &mut output_scheduler.pending_start_audio,
        output,
        delayed_audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        AudioClockMode::AudioStarted,
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
    output_scheduler.initial_delayed_audio_start_timeline_nsecs = None;
    output_scheduler.set_state(PlaybackOutputState::Playing);
    tracing::debug!(
        session_id = ?session_id,
        presented_video_frames,
        delayed_audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        pending_audio_frames = output_scheduler.pending_start_audio.len(),
        pending_audio_ms = output_scheduler.pending_start_audio.buffered_duration().as_secs_f64()
            * 1000.0,
        clock_mode = AudioClockMode::AudioStarted.as_str(),
        "started native audio output after video-clocked initial gap"
    );
    Ok(OutputGateResumeStatus::Resumed)
}
