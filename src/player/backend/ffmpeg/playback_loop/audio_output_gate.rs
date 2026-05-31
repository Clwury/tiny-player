use super::pending_audio_queue::PendingStartAudio;
use super::playback_block::PlaybackBlockReason;
use super::scheduled_video_queue::ScheduledVideoQueue;
use super::video_output_gate::{AudioClockedVideoDrainStatus, admit_decoded_video_frame_to_vo};
use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn flush_pending_start_audio(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    audio_start_timeline_nsecs: u64,
    audio_flush_until_timeline_nsecs: u64,
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
        return Ok(false);
    }

    let mut queued_audio_frames = 0usize;
    let mut queued_audio_until_nsecs = audio_start_timeline_nsecs;
    let mut made_progress = false;
    while let Some(mut frame) = pending_audio.pop_front_until(audio_flush_until_timeline_nsecs) {
        let buffered_until_nsecs = frame.end_timeline_nsecs;
        match output.try_push_timed(
            frame.samples,
            frame.start_timeline_nsecs,
            frame.end_timeline_nsecs,
            control,
        )? {
            AudioOutputPushResult::Queued => {
                queued_audio_frames = queued_audio_frames.saturating_add(1);
                queued_audio_until_nsecs = buffered_until_nsecs;
                made_progress = true;
            }
            AudioOutputPushResult::WouldBlock {
                samples,
                queued_frames,
                queued_duration,
            } => {
                frame.samples = samples;
                pending_audio.push_front_frame(frame);
                let audio_snapshot = output.snapshot()?;
                made_progress |= present_due_audio_clocked_frames_to_vo(
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
                tracing::debug!(
                    session_id = ?session_id,
                    blocked_on = PlaybackBlockReason::AudioOutput.as_str(),
                    queued_audio_frames = queued_frames,
                    queued_audio_ms = queued_duration.as_secs_f64() * 1000.0,
                    pending_audio_frames = pending_audio.len(),
                    pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
                    audio_start_timeline_nsecs,
                    audio_flush_until_timeline_nsecs,
                    "audio output queue full while flushing pending FFmpeg audio"
                );
                return Ok(made_progress);
            }
            AudioOutputPushResult::Interrupted { samples } => {
                frame.samples = samples;
                pending_audio.push_front_frame(frame);
                return Ok(made_progress);
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
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            "queued buffered FFmpeg audio covered by decoded video"
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
    match output.try_push_timed(
        audio.samples,
        start_timeline_nsecs,
        end_timeline_nsecs,
        control,
    )? {
        AudioOutputPushResult::Queued => {
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
                    duration_nsecs: audio.duration_nsecs,
                },
                start_timeline_nsecs,
                end_timeline_nsecs,
            );
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
            subtitle_pipeline.update_overlay(
                audio_snapshot.played_timeline_nsecs,
                session_id,
                event_tx,
            );
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
    let pop_result = queued_video_frames.pop_audio_clocked_frame(played_until_nsecs);
    let mut made_progress = pop_result.dropped_frames > 0;
    if pop_result.dropped_frames > 0 {
        tracing::debug!(
            dropped_video_frames = pop_result.dropped_frames,
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
        tracing::debug!(
            session_id = ?session_id,
            blocked_on = blocked_on.as_str(),
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
