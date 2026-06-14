use super::playback_snapshot::{
    PlaybackPipelineSnapshot, PlaybackPipelineSnapshotContext, PlaybackPipelineTelemetry,
};
use super::*;

#[derive(Default)]
pub(super) struct PlaybackPipelineWaitService;

pub(super) struct PlaybackPipelineWaitContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) video_decode_pipeline: &'a VideoDecodePipeline,
    pub(super) video_frame_prepare_worker: Option<&'a VideoFramePrepareWorker>,
    pub(super) audio_decode_pipeline: Option<&'a AudioDecodePipeline>,
    pub(super) subtitle_pipeline: &'a SubtitlePipeline,
    pub(super) output_scheduler: &'a PlaybackOutputScheduler,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) playback_telemetry: &'a mut PlaybackPipelineTelemetry,
}

impl PlaybackPipelineWaitService {
    pub(super) fn wait_poll_interval_and_delay_scheduler(&self, scheduler: &mut PlaybackScheduler) {
        let waited_at = Instant::now();
        thread::sleep(SCHEDULER_POLL_INTERVAL);
        scheduler.delay_by(waited_at.elapsed());
    }

    pub(super) fn yield_once(&self) {
        thread::yield_now();
    }

    pub(super) fn observe_stall(
        &self,
        context: &mut PlaybackPipelineWaitContext<'_>,
        stall_reason: &'static str,
    ) {
        let snapshot = PlaybackPipelineSnapshot::capture(PlaybackPipelineSnapshotContext {
            demux_cache: context.demux_cache,
            video_decode_pipeline: context.video_decode_pipeline,
            video_frame_prepare_worker: context.video_frame_prepare_worker,
            audio_decode_pipeline: context.audio_decode_pipeline,
            subtitle_pipeline: context.subtitle_pipeline,
            output_scheduler: context.output_scheduler,
            audio_output: context.audio_output,
            vo_queue: context.vo_queue,
        });
        context
            .playback_telemetry
            .observe_stall(context.session_id, stall_reason, snapshot);
    }

    pub(super) fn wait_after_stall(
        &self,
        mut context: PlaybackPipelineWaitContext<'_>,
        stall_reason: &'static str,
    ) {
        self.observe_stall(&mut context, stall_reason);
        let wait_duration = self.wait_duration_after_stall(&context);
        wait_for_stall_duration(wait_duration);
    }

    fn wait_duration_after_stall(&self, context: &PlaybackPipelineWaitContext<'_>) -> Duration {
        let Some(audio_output) = context.audio_output else {
            return SCHEDULER_POLL_INTERVAL;
        };
        let Ok(audio_snapshot) = audio_output.snapshot() else {
            return SCHEDULER_POLL_INTERVAL;
        };
        if audio_snapshot.total_pending_nsecs == 0 {
            return SCHEDULER_POLL_INTERVAL;
        }
        context
            .output_scheduler
            .audio_clocked_video_wait_duration(audio_snapshot.played_timeline_nsecs)
            .map(|duration| duration.min(SCHEDULER_POLL_INTERVAL))
            .unwrap_or(SCHEDULER_POLL_INTERVAL)
    }
}

fn wait_for_stall_duration(duration: Duration) {
    if duration.is_zero() {
        thread::yield_now();
    } else {
        thread::sleep(duration);
    }
}
