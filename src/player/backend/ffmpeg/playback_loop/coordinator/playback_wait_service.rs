use super::playback_snapshot::{
    PlaybackPipelineSnapshot, PlaybackPipelineSnapshotContext, PlaybackPipelineTelemetry,
};
use std::{
    thread,
    time::{Duration, Instant},
};

use crate::player::render_host::{PlaybackSessionId, VideoOutputQueue};

use super::{
    AudioDecodePipeline, AudioOutput, DemuxPacketCache, PlaybackOutputScheduler, PlaybackScheduler,
    SCHEDULER_POLL_INTERVAL, SubtitlePipeline, VideoDecodePipeline, VideoFramePrepareWorker,
};

#[derive(Default)]
pub(super) struct PlaybackPipelineWaitService;

pub(super) struct PlaybackPipelineWaitContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) video_decode_pipeline: &'a VideoDecodePipeline,
    pub(super) video_frame_duration_nsecs: u64,
    pub(super) video_frame_prepare_worker: Option<&'a VideoFramePrepareWorker>,
    pub(super) audio_decode_pipeline: Option<&'a AudioDecodePipeline>,
    pub(super) subtitle_pipeline: &'a SubtitlePipeline,
    pub(super) output_scheduler: &'a PlaybackOutputScheduler,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) playback_telemetry: &'a mut PlaybackPipelineTelemetry,
    pub(super) playback_loop_deadline: PlaybackLoopDeadline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PlaybackLoopDeadline {
    cached_seek_recovery_watchdog_deadline: Option<Instant>,
    hevc_startup_stall_watchdog_deadline: Option<Instant>,
    rebuffer_empty_audio_output_watchdog_deadline: Option<Instant>,
}

impl PlaybackLoopDeadline {
    pub(super) fn from_cached_seek_recovery_watchdog(deadline: Option<Instant>) -> Self {
        Self {
            cached_seek_recovery_watchdog_deadline: deadline,
            hevc_startup_stall_watchdog_deadline: None,
            rebuffer_empty_audio_output_watchdog_deadline: None,
        }
    }

    pub(super) fn with_hevc_startup_stall_watchdog_deadline(
        mut self,
        deadline: Option<Instant>,
    ) -> Self {
        self.hevc_startup_stall_watchdog_deadline = deadline;
        self
    }

    pub(super) fn with_rebuffer_empty_audio_output_watchdog_delay(
        mut self,
        delay: Option<Duration>,
    ) -> Self {
        self.rebuffer_empty_audio_output_watchdog_deadline =
            delay.map(|delay| Instant::now() + delay);
        self
    }

    pub(super) fn cached_seek_recovery_watchdog_remaining(self) -> Option<Duration> {
        self.cached_seek_recovery_watchdog_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) fn rebuffer_empty_audio_output_watchdog_remaining(self) -> Option<Duration> {
        self.rebuffer_empty_audio_output_watchdog_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub(super) fn hevc_startup_stall_watchdog_remaining(self) -> Option<Duration> {
        self.hevc_startup_stall_watchdog_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn cap_wait_duration(self, duration: Duration) -> Duration {
        let duration = self
            .cached_seek_recovery_watchdog_remaining()
            .map(|remaining| duration.min(remaining))
            .unwrap_or(duration);
        let duration = self
            .hevc_startup_stall_watchdog_remaining()
            .map(|remaining| duration.min(remaining))
            .unwrap_or(duration);
        self.rebuffer_empty_audio_output_watchdog_remaining()
            .map(|remaining| duration.min(remaining))
            .unwrap_or(duration)
    }
}

impl PlaybackPipelineWaitService {
    pub(super) fn wait_poll_interval_and_delay_scheduler_until(
        &self,
        scheduler: &mut PlaybackScheduler,
        playback_loop_deadline: PlaybackLoopDeadline,
    ) {
        let waited_at = Instant::now();
        wait_for_stall_duration(playback_loop_deadline.cap_wait_duration(SCHEDULER_POLL_INTERVAL));
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
            video_frame_duration_nsecs: context.video_frame_duration_nsecs,
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
        let wait_duration = self.wait_duration_after_stall(&context);
        self.observe_stall_with_wait(&mut context, stall_reason, wait_duration);
        wait_for_stall_duration(wait_duration);
    }

    fn observe_stall_with_wait(
        &self,
        context: &mut PlaybackPipelineWaitContext<'_>,
        stall_reason: &'static str,
        planned_wait: Duration,
    ) {
        let snapshot = PlaybackPipelineSnapshot::capture(PlaybackPipelineSnapshotContext {
            demux_cache: context.demux_cache,
            video_decode_pipeline: context.video_decode_pipeline,
            video_frame_duration_nsecs: context.video_frame_duration_nsecs,
            video_frame_prepare_worker: context.video_frame_prepare_worker,
            audio_decode_pipeline: context.audio_decode_pipeline,
            subtitle_pipeline: context.subtitle_pipeline,
            output_scheduler: context.output_scheduler,
            audio_output: context.audio_output,
            vo_queue: context.vo_queue,
        });
        context.playback_telemetry.observe_stall_with_wait(
            context.session_id,
            stall_reason,
            snapshot,
            Some(planned_wait),
        );
    }

    fn wait_duration_after_stall(&self, context: &PlaybackPipelineWaitContext<'_>) -> Duration {
        let wait_duration = if let Some(audio_output) = context.audio_output {
            if let Ok(audio_snapshot) = audio_output.snapshot() {
                if audio_snapshot.total_pending_nsecs == 0 {
                    SCHEDULER_POLL_INTERVAL
                } else {
                    context
                        .output_scheduler
                        .audio_clocked_video_wait_duration(audio_snapshot.played_timeline_nsecs)
                        .map(|duration| duration.min(SCHEDULER_POLL_INTERVAL))
                        .unwrap_or(SCHEDULER_POLL_INTERVAL)
                }
            } else {
                SCHEDULER_POLL_INTERVAL
            }
        } else {
            SCHEDULER_POLL_INTERVAL
        };
        context
            .playback_loop_deadline
            .with_rebuffer_empty_audio_output_watchdog_delay(
                context
                    .output_scheduler
                    .rebuffer_empty_audio_output_watchdog_delay(),
            )
            .cap_wait_duration(wait_duration)
    }
}

fn wait_for_stall_duration(duration: Duration) {
    if duration.is_zero() {
        thread::yield_now();
    } else {
        thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::PlaybackLoopDeadline;
    use std::time::{Duration, Instant};

    #[test]
    fn cached_seek_watchdog_caps_wait_duration() {
        let remaining = Duration::from_millis(50);
        let deadline = PlaybackLoopDeadline::from_cached_seek_recovery_watchdog(Some(
            Instant::now() + remaining,
        ));
        let capped = deadline.cap_wait_duration(Duration::from_millis(5_000));

        assert!(capped <= remaining);
        assert!(capped > Duration::ZERO);
    }

    #[test]
    fn expired_cached_seek_watchdog_yields_next_loop() {
        let deadline = PlaybackLoopDeadline::from_cached_seek_recovery_watchdog(Some(
            Instant::now() - Duration::from_millis(1),
        ));
        assert_eq!(
            deadline.cap_wait_duration(Duration::from_millis(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn rebuffer_empty_audio_watchdog_caps_wait_duration() {
        let deadline = PlaybackLoopDeadline::default()
            .with_rebuffer_empty_audio_output_watchdog_delay(Some(Duration::from_millis(100)));

        let capped = deadline.cap_wait_duration(Duration::from_secs(5));

        assert!(capped <= Duration::from_millis(100));
        assert!(capped > Duration::ZERO);
    }

    #[test]
    fn hevc_startup_stall_watchdog_caps_wait_duration() {
        let deadline = PlaybackLoopDeadline::default().with_hevc_startup_stall_watchdog_deadline(
            Some(Instant::now() + Duration::from_millis(75)),
        );

        let capped = deadline.cap_wait_duration(Duration::from_secs(5));

        assert!(capped <= Duration::from_millis(75));
        assert!(capped > Duration::ZERO);
    }
}
