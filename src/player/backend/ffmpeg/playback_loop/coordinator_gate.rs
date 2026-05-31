use super::playback_wait_service::PlaybackPipelineWaitService;
use super::*;

#[derive(Default)]
pub(super) struct PlaybackCoordinatorGateService;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackCoordinatorGateStatus {
    Ready,
    Continue,
}

impl PlaybackCoordinatorGateStatus {
    pub(super) fn should_continue(self) -> bool {
        matches!(self, Self::Continue)
    }
}

pub(super) struct PlaybackCoordinatorGateContext<'a> {
    pub(super) control: &'a FfmpegControl,
    pub(super) output_scheduler: &'a PlaybackOutputScheduler,
    pub(super) scheduler: &'a mut PlaybackScheduler,
    pub(super) playback_wait: &'a PlaybackPipelineWaitService,
}

impl PlaybackCoordinatorGateService {
    pub(super) fn service(
        &mut self,
        context: PlaybackCoordinatorGateContext<'_>,
    ) -> PlaybackCoordinatorGateStatus {
        if coordinator_should_wait_for_pause(context.control, context.output_scheduler) {
            context
                .playback_wait
                .wait_poll_interval_and_delay_scheduler(context.scheduler);
            return PlaybackCoordinatorGateStatus::Continue;
        }

        if context.control.has_pending_seek() {
            context.playback_wait.yield_once();
            return PlaybackCoordinatorGateStatus::Continue;
        }

        PlaybackCoordinatorGateStatus::Ready
    }
}

fn coordinator_should_wait_for_pause(
    control: &FfmpegControl,
    output_scheduler: &PlaybackOutputScheduler,
) -> bool {
    control.is_user_paused() || (control.is_cache_paused() && !output_scheduler.rebuffering())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_gate_status_reports_continue() {
        assert!(PlaybackCoordinatorGateStatus::Continue.should_continue());
        assert!(!PlaybackCoordinatorGateStatus::Ready.should_continue());
    }
}
