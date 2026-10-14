use super::super::{
    playback_wait_service::PlaybackLoopDeadline,
    video_frame_admission_service::{
        PreparedVideoFrameAdmissionContext, admit_paused_final_video_frame,
    },
};
use super::PlaybackEofDrainContext;

pub(super) fn publish_paused_seek_final_frame(
    context: &mut PlaybackEofDrainContext<'_>,
) -> std::result::Result<(), String> {
    let pipeline = &mut *context.pipeline;
    if !context.control.is_user_paused()
        || context.control.should_interrupt()
        || !pipeline.output_scheduler.restart_pending()
        || !pipeline.output_scheduler.scheduled_video_queue.is_empty()
    {
        return Ok(());
    }
    let Some(input) = pipeline.video_frame_prepare_worker.take_seek_preroll() else {
        return Ok(());
    };
    // Packet generations advance throughout a seek. The prepare worker's
    // generation floor and flush fence the seek; use this frame's packet id.
    let generation = input.generation;
    // All decoder outputs have drained. A target beyond the last video PTS
    // cannot acquire another frame; convert only the retained final source frame.
    pipeline.video_frame_prepare_worker.try_enqueue(input)?;
    loop {
        if context.control.should_interrupt() {
            return Ok(());
        }
        pipeline.video_frame_prepare_worker.retry_pending_input()?;
        if let Some(result) = pipeline
            .video_frame_prepare_worker
            .poll_result(generation)?
        {
            let prepared = result.result?;
            let requested_target_nsecs = pipeline.current_start_position_nsecs;
            let final_video_timeline_nsecs = prepared.timeline_nsecs;
            if !admit_paused_final_video_frame(PreparedVideoFrameAdmissionContext {
                prepared_frame: prepared,
                decoded_video_frame_count: pipeline.decoded_video_frame_count,
                scheduler: &mut pipeline.scheduler,
                audio_output: pipeline.audio_output.as_ref(),
                output_scheduler: &mut pipeline.output_scheduler,
                buffered_reporter: &mut pipeline.buffered_reporter,
                control: context.control,
                session_id: context.session_id,
                event_tx: context.event_tx,
                vo_queue: context.vo_queue,
                frame_presented: context.frame_presented,
                position_reporter: &mut pipeline.position_reporter,
                subtitle_pipeline: &mut pipeline.subtitle_pipeline,
                current_start_position_nsecs: &mut pipeline.current_start_position_nsecs,
            }) {
                return Ok(());
            }
            context.demux_cache.clear_cache_pause_for_decoded_resume();
            tracing::debug!(
                session_id = ?context.session_id,
                generation,
                requested_target_nsecs,
                final_video_timeline_nsecs,
                "published final video frame for paused seek beyond video EOF"
            );
            return Ok(());
        }
        context
            .services
            .wait
            .wait_poll_interval_and_delay_scheduler_until(
                &mut pipeline.scheduler,
                PlaybackLoopDeadline::default(),
            );
    }
}
