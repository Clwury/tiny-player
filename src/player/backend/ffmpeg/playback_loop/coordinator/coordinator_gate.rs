use super::playback_wait_service::{PlaybackLoopDeadline, PlaybackPipelineWaitService};
use super::{FfmpegControl, OutputServiceDemand, PlaybackOutputScheduler, PlaybackScheduler};
use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use super::demux_cache::DemuxStreamPacketQueueSnapshot;

#[derive(Default)]
pub(super) struct PlaybackCoordinatorGateService {
    last_status: Option<(PlaybackCoordinatorGateStatus, OutputServiceDemand)>,
    output_probe_preemption_started_at: Option<Instant>,
    output_probe_preemption_count: u64,
    last_output_probe_preemption_log_at: Option<Instant>,
}

const OUTPUT_PROBE_INPUT_PREEMPTION_WARN_AFTER: Duration = Duration::from_millis(100);
const OUTPUT_PROBE_INPUT_PREEMPTION_LOG_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackCoordinatorGateStatus {
    Ready,
    ServiceOutput,
    DrainDecodeOnly,
    DrainCachedInput,
    WaitForStateChange,
    WaitForCache,
    Wait,
}

impl PlaybackCoordinatorGateStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::ServiceOutput => "service_output",
            Self::DrainDecodeOnly => "drain_decode_only",
            Self::DrainCachedInput => "drain_cached_input",
            Self::WaitForStateChange => "wait_for_state_change",
            Self::WaitForCache => "wait_for_cache",
            Self::Wait => "wait",
        }
    }

    fn reason(self, output_service_demand: OutputServiceDemand) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::ServiceOutput => output_service_demand.as_str(),
            Self::DrainDecodeOnly => "actual_decode_work",
            Self::DrainCachedInput => "cached_input_admissible",
            Self::WaitForStateChange => "cached_input_not_admissible",
            Self::WaitForCache => "waiting_cached_input",
            Self::Wait => "paused_or_seek_pending",
        }
    }
}

pub(super) struct PlaybackCoordinatorGateContext<'a> {
    pub(super) control: &'a FfmpegControl,
    pub(super) output_scheduler: &'a PlaybackOutputScheduler,
    pub(super) scheduler: &'a mut PlaybackScheduler,
    pub(super) playback_wait: &'a PlaybackPipelineWaitService,
    pub(super) playback_loop_deadline: PlaybackLoopDeadline,
    pub(super) actual_decode_work: bool,
    pub(super) output_service_demand: OutputServiceDemand,
    pub(super) first_frame_input_demand: bool,
    pub(super) cached_input_drainable: bool,
    pub(super) cached_input_admissible: bool,
    pub(super) output_lead_throttled: bool,
    pub(super) output_transaction_blocked: bool,
    pub(super) cache_generation: u64,
    pub(super) selected_streams: &'a [c_int],
    pub(super) requested_streams: &'a [c_int],
    pub(super) cached_streams: &'a [DemuxStreamPacketQueueSnapshot],
    pub(super) exact_seek_target_nsecs: u64,
    pub(super) actual_anchor_nsecs: Option<u64>,
    pub(super) preroll_debt_nsecs: Option<u64>,
    pub(super) cached_video_end_nsecs: Option<u64>,
    pub(super) cached_video_drainable_packets: usize,
}

impl PlaybackCoordinatorGateService {
    pub(super) fn service(
        &mut self,
        context: PlaybackCoordinatorGateContext<'_>,
    ) -> PlaybackCoordinatorGateStatus {
        let status = coordinator_gate_status(
            context.control,
            context.output_service_demand,
            context.actual_decode_work,
            context.cached_input_drainable,
            context.cached_input_admissible,
        );
        let status_key = (status, context.output_service_demand);
        if self.last_status != Some(status_key) {
            let output_snapshot = context.output_scheduler.snapshot();
            let mut per_stream = context
                .cached_streams
                .iter()
                .map(|stream| {
                    (
                        stream.stream_index,
                        context.selected_streams.contains(&stream.stream_index),
                        context.requested_streams.contains(&stream.stream_index),
                        stream.consumer_drainable,
                        stream.cached_end_nsecs,
                        stream.target_coverage_nsecs,
                    )
                })
                .collect::<Vec<_>>();
            for stream_index in context.selected_streams {
                if !per_stream.iter().any(|stream| stream.0 == *stream_index) {
                    per_stream.push((
                        *stream_index,
                        true,
                        context.requested_streams.contains(stream_index),
                        false,
                        None,
                        None,
                    ));
                }
            }
            tracing::debug!(
                cache_pause_gate_mode = status.as_str(),
                gate_reason = status.reason(context.output_service_demand),
                user_paused = context.control.is_user_paused(),
                cache_paused = context.control.is_cache_paused(),
                output_rebuffering = context.output_scheduler.rebuffering(),
                initial_start_phase = context.output_scheduler.initial_start_phase(),
                first_frame_presented = output_snapshot.first_frame_presented,
                audio_start_target_nsecs = ?output_snapshot.audio_start_target_nsecs,
                output_transition_deadline_ms = ?output_snapshot.output_transition_deadline_ms,
                actual_decode_work = context.actual_decode_work,
                output_service_due = context.output_service_demand.is_due(),
                output_service_due_kind = context.output_service_demand.as_str(),
                first_frame_input_demand = context.first_frame_input_demand,
                cached_input_drainable = context.cached_input_drainable,
                consumer_drainable = context.cached_input_drainable,
                input_admissible = context.cached_input_admissible,
                lead_throttled = context.output_lead_throttled,
                output_transaction_blocked = context.output_transaction_blocked,
                cache_generation = context.cache_generation,
                selected_streams = ?context.selected_streams,
                requested_streams = ?context.requested_streams,
                per_stream_fields = "(stream_index, selected, requested, consumer_drainable, cached_end_nsecs, target_coverage_nsecs)",
                per_stream = ?per_stream,
                exact_seek_target_nsecs = context.exact_seek_target_nsecs,
                actual_anchor_nsecs = ?context.actual_anchor_nsecs,
                preroll_debt_nsecs = ?context.preroll_debt_nsecs,
                cached_video_end_nsecs = ?context.cached_video_end_nsecs,
                cached_video_drainable_packets = context.cached_video_drainable_packets,
                "updated FFmpeg coordinator pause gate mode"
            );
            self.last_status = Some(status_key);
        }

        self.observe_output_probe_input_preemption(
            status,
            context.output_service_demand,
            context.cached_input_admissible,
            context.cache_generation,
        );

        if context.control.has_pending_seek() {
            context.playback_wait.yield_once();
            return PlaybackCoordinatorGateStatus::Wait;
        }

        if status == PlaybackCoordinatorGateStatus::Wait {
            let watchdog_remaining = if context.control.is_user_paused() {
                PlaybackLoopDeadline::default()
            } else {
                context.playback_loop_deadline
            };
            let watchdog_remaining = watchdog_remaining
                .with_rebuffer_empty_audio_output_watchdog_delay(
                    context
                        .output_scheduler
                        .rebuffer_empty_audio_output_watchdog_delay(),
                );
            context
                .playback_wait
                .wait_poll_interval_and_delay_scheduler_until(
                    context.scheduler,
                    watchdog_remaining,
                );
            return status;
        }

        status
    }

    fn observe_output_probe_input_preemption(
        &mut self,
        status: PlaybackCoordinatorGateStatus,
        demand: OutputServiceDemand,
        cached_input_admissible: bool,
        cache_generation: u64,
    ) {
        let probe_preempted_input = status == PlaybackCoordinatorGateStatus::ServiceOutput
            && !demand.hard_deadline_due()
            && cached_input_admissible;
        if !probe_preempted_input {
            self.output_probe_preemption_started_at = None;
            self.output_probe_preemption_count = 0;
            return;
        }

        let now = Instant::now();
        let started_at = *self.output_probe_preemption_started_at.get_or_insert(now);
        self.output_probe_preemption_count = self.output_probe_preemption_count.saturating_add(1);
        let elapsed = now.saturating_duration_since(started_at);
        let log_due = elapsed >= OUTPUT_PROBE_INPUT_PREEMPTION_WARN_AFTER
            && self
                .last_output_probe_preemption_log_at
                .is_none_or(|last_log_at| {
                    now.saturating_duration_since(last_log_at)
                        >= OUTPUT_PROBE_INPUT_PREEMPTION_LOG_INTERVAL
                });
        if log_due {
            tracing::warn!(
                output_service_due_kind = demand.as_str(),
                cache_generation,
                cached_input_admissible,
                preemption_ms = elapsed.as_secs_f64() * 1000.0,
                preemption_count = self.output_probe_preemption_count,
                "FFmpeg output probe continuously preempted drainable decoder input"
            );
            self.last_output_probe_preemption_log_at = Some(now);
        }
    }
}

fn coordinator_gate_status(
    control: &FfmpegControl,
    output_service_demand: OutputServiceDemand,
    actual_decode_work: bool,
    cached_input_drainable: bool,
    cached_input_admissible: bool,
) -> PlaybackCoordinatorGateStatus {
    if control.is_user_paused() || control.has_pending_seek() {
        return PlaybackCoordinatorGateStatus::Wait;
    }
    if control.is_cache_paused() {
        return if output_service_demand.hard_deadline_due() {
            PlaybackCoordinatorGateStatus::ServiceOutput
        } else if actual_decode_work {
            PlaybackCoordinatorGateStatus::DrainDecodeOnly
        } else if cached_input_admissible {
            PlaybackCoordinatorGateStatus::DrainCachedInput
        } else if output_service_demand.is_due() {
            PlaybackCoordinatorGateStatus::ServiceOutput
        } else if cached_input_drainable {
            PlaybackCoordinatorGateStatus::WaitForStateChange
        } else {
            PlaybackCoordinatorGateStatus::WaitForCache
        };
    }
    PlaybackCoordinatorGateStatus::Ready
}

#[cfg(test)]
mod tests {
    use super::super::{FfmpegControl, OutputServiceDemand};
    use super::{PlaybackCoordinatorGateStatus, coordinator_gate_status};
    use crate::player::render_host::PlaybackSessionId;

    #[test]
    fn coordinator_gate_status_names_all_modes() {
        assert_eq!(PlaybackCoordinatorGateStatus::Ready.as_str(), "ready");
        assert_eq!(
            PlaybackCoordinatorGateStatus::ServiceOutput.as_str(),
            "service_output"
        );
        assert_eq!(
            PlaybackCoordinatorGateStatus::DrainDecodeOnly.as_str(),
            "drain_decode_only"
        );
        assert_eq!(
            PlaybackCoordinatorGateStatus::DrainCachedInput.as_str(),
            "drain_cached_input"
        );
        assert_eq!(
            PlaybackCoordinatorGateStatus::WaitForCache.as_str(),
            "wait_for_cache"
        );
        assert_eq!(
            PlaybackCoordinatorGateStatus::WaitForStateChange.as_str(),
            "wait_for_state_change"
        );
        assert_eq!(PlaybackCoordinatorGateStatus::Wait.as_str(), "wait");
    }

    #[test]
    fn cache_pause_with_first_frame_work_drains_decode_instead_of_waiting() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);
        assert_eq!(
            coordinator_gate_status(&control, OutputServiceDemand::None, true, false, false),
            PlaybackCoordinatorGateStatus::DrainDecodeOnly
        );
        assert_eq!(
            coordinator_gate_status(&control, OutputServiceDemand::None, false, true, true),
            PlaybackCoordinatorGateStatus::DrainCachedInput
        );
        assert_eq!(
            coordinator_gate_status(&control, OutputServiceDemand::None, false, false, false),
            PlaybackCoordinatorGateStatus::WaitForCache
        );
    }

    #[test]
    fn cache_pause_with_85_cached_video_packets_drains_cached_input() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);

        assert_eq!(
            coordinator_gate_status(&control, OutputServiceDemand::None, false, 85 > 0, true,),
            PlaybackCoordinatorGateStatus::DrainCachedInput
        );
    }

    #[test]
    fn periodic_probe_due_does_not_preempt_cached_input() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);
        assert_eq!(
            coordinator_gate_status(
                &control,
                OutputServiceDemand::PeriodicProbe,
                false,
                true,
                true,
            ),
            PlaybackCoordinatorGateStatus::DrainCachedInput
        );
    }

    #[test]
    fn hard_deadline_due_preempts_cached_input() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);
        assert_eq!(
            coordinator_gate_status(
                &control,
                OutputServiceDemand::HardDeadline,
                true,
                true,
                true,
            ),
            PlaybackCoordinatorGateStatus::ServiceOutput
        );
    }

    #[test]
    fn audio_start_due_does_not_preempt_cached_input() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);
        assert_eq!(
            coordinator_gate_status(
                &control,
                OutputServiceDemand::AudioStartDue,
                false,
                true,
                true,
            ),
            PlaybackCoordinatorGateStatus::DrainCachedInput
        );
    }

    #[test]
    fn drainable_but_throttled_input_waits_for_state_change() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        control.set_cache_paused(true);
        assert_eq!(
            coordinator_gate_status(&control, OutputServiceDemand::None, false, true, false),
            PlaybackCoordinatorGateStatus::WaitForStateChange
        );
    }
}
