use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::player::render_host::VideoPresentation;

use super::{LATE_VIDEO_DROP_TOLERANCE, duration_nsecs};

const MAX_DECODER_DROP_BUDGET: u64 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum VideoDecodeDropPolicy {
    #[default]
    None,
    Pressure {
        epoch: u64,
    },
    SeekPreroll,
}

impl VideoDecodeDropPolicy {
    pub(super) fn skip_nonref(self) -> bool {
        self != Self::None
    }

    pub(super) fn effective(self, current_epoch: u64) -> Self {
        match self {
            Self::Pressure { epoch } if epoch != current_epoch || epoch & 1 == 0 => Self::None,
            _ => self,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct VideoDecodeDropStats {
    pub(super) pressure_attempts: u64,
    pub(super) seek_preroll_attempts: u64,
    pub(super) estimated_pressure_drops: u64,
    pub(super) estimated_seek_preroll_drops: u64,
}

#[derive(Default)]
struct DropEstimator {
    have_output: bool,
    pending_pressure: u64,
    pending_seek: u64,
}

impl DropEstimator {
    fn observe(
        &mut self,
        stats: &mut VideoDecodeDropStats,
        policy: VideoDecodeDropPolicy,
        decoded_frames: u64,
        decode_ok: bool,
    ) {
        match policy {
            VideoDecodeDropPolicy::Pressure { .. } => stats.pressure_attempts += 1,
            VideoDecodeDropPolicy::SeekPreroll => stats.seek_preroll_attempts += 1,
            VideoDecodeDropPolicy::None => {}
        }
        if !decode_ok {
            *self = Self::default();
        } else if decoded_frames > 0 {
            // Like mpv's packets-without-output estimate, only confirm a run
            // when output returns. Exclude initial decoder/reorder warm-up and
            // never count ordinary zero-output packets as intentional drops.
            stats.estimated_pressure_drops += self.pending_pressure;
            stats.estimated_seek_preroll_drops += self.pending_seek;
            self.pending_pressure = 0;
            self.pending_seek = 0;
            self.have_output = true;
        } else if self.have_output {
            match policy {
                VideoDecodeDropPolicy::Pressure { .. } => self.pending_pressure += 1,
                VideoDecodeDropPolicy::SeekPreroll => self.pending_seek += 1,
                VideoDecodeDropPolicy::None => {}
            }
        }
    }
}

#[derive(Default)]
pub(super) struct VideoDecodeFrameDrop {
    // Even epochs disable pressure dropping. Changing the epoch cancels old
    // policies even when their Decode commands are already queued in the worker.
    epoch: Arc<AtomicU64>,
    last_presentation_sequence: Option<u64>,
    remaining: u64,
    outstanding: u64,
    stats: VideoDecodeDropStats,
    estimator: DropEstimator,
}

impl VideoDecodeFrameDrop {
    pub(super) fn epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.epoch)
    }

    pub(super) fn enabled(&self) -> bool {
        self.epoch.load(Ordering::Acquire) & 1 != 0
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled() != enabled {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.reset_budget();
        }
    }

    pub(super) fn reset(&mut self) {
        self.epoch.fetch_add(2, Ordering::AcqRel);
        self.reset_budget();
        self.estimator = DropEstimator::default();
    }

    fn reset_budget(&mut self) {
        self.remaining = 0;
        self.outstanding = 0;
        self.last_presentation_sequence = None;
    }

    pub(super) fn observe_output(
        &mut self,
        allowed: bool,
        presentation: Option<VideoPresentation>,
        played_until_nsecs: Option<u64>,
        frame_duration_nsecs: Option<u64>,
    ) {
        let feedback = presentation
            .zip(played_until_nsecs)
            .zip(frame_duration_nsecs.filter(|duration| *duration > 0));
        let Some(((presentation, audio), duration)) =
            feedback.filter(|_| allowed && self.enabled())
        else {
            self.remaining = 0;
            return;
        };
        let late_by = audio
            .saturating_sub(presentation.timeline_nsecs)
            .saturating_sub(duration)
            .saturating_sub(duration_nsecs(LATE_VIDEO_DROP_TOLERANCE));
        let target = (late_by / duration).min(MAX_DECODER_DROP_BUDGET);
        let available = target.saturating_sub(self.outstanding);
        if self.last_presentation_sequence != Some(presentation.sequence) {
            self.last_presentation_sequence = Some(presentation.sequence);
            self.remaining = available;
        } else {
            // The coordinator can admit many packets before the next visible
            // frame. The same output observation must never refill its budget.
            self.remaining = self.remaining.min(available);
        }
    }

    pub(super) fn pressure_policy(&self, packet_is_late: bool) -> VideoDecodeDropPolicy {
        if packet_is_late && self.enabled() && self.remaining > 0 {
            VideoDecodeDropPolicy::Pressure {
                epoch: self.epoch.load(Ordering::Acquire),
            }
        } else {
            VideoDecodeDropPolicy::None
        }
    }

    pub(super) fn submission_policy(&self, policy: VideoDecodeDropPolicy) -> VideoDecodeDropPolicy {
        let policy = policy.effective(self.epoch.load(Ordering::Acquire));
        if matches!(policy, VideoDecodeDropPolicy::Pressure { .. }) && self.remaining == 0 {
            VideoDecodeDropPolicy::None
        } else {
            policy
        }
    }

    pub(super) fn submitted(&mut self, policy: VideoDecodeDropPolicy) {
        if matches!(policy, VideoDecodeDropPolicy::Pressure { .. }) {
            self.remaining = self.remaining.saturating_sub(1);
            self.outstanding += 1;
        }
    }

    pub(super) fn completed(
        &mut self,
        policy: VideoDecodeDropPolicy,
        frames: u64,
        decode_ok: bool,
    ) {
        if matches!(
            policy.effective(self.epoch.load(Ordering::Acquire)),
            VideoDecodeDropPolicy::Pressure { .. }
        ) {
            self.outstanding = self.outstanding.saturating_sub(1);
        }
        self.estimator
            .observe(&mut self.stats, policy, frames, decode_ok);
    }

    pub(super) fn remaining_budget(&self) -> u64 {
        self.remaining
    }
    pub(super) fn stats(&self) -> VideoDecodeDropStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(sequence: u64, video: u64) -> Option<VideoPresentation> {
        Some(VideoPresentation {
            sequence,
            timeline_nsecs: video,
        })
    }

    fn arm(drop: &mut VideoDecodeFrameDrop, sequence: u64) {
        drop.observe_output(
            true,
            feedback(sequence, 1_000_000_000),
            Some(1_515_000_000),
            Some(40_000_000),
        );
    }

    #[test]
    fn ordinary_decoder_dropping_is_disabled_until_explicitly_enabled() {
        let mut drop = VideoDecodeFrameDrop::default();
        arm(&mut drop, 1);
        assert_eq!(drop.pressure_policy(true), VideoDecodeDropPolicy::None);
        assert_eq!(drop.remaining_budget(), 0);
        drop.set_enabled(true);
        arm(&mut drop, 1);
        assert!(drop.pressure_policy(true).skip_nonref());
        assert_eq!(drop.pressure_policy(false), VideoDecodeDropPolicy::None);
    }

    #[test]
    fn one_presentation_never_replenishes_consumed_budget() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        arm(&mut drop, 1);
        assert_eq!(drop.remaining_budget(), 10);
        for _ in 0..10 {
            let policy = drop.pressure_policy(true);
            assert!(policy.skip_nonref());
            drop.submitted(policy);
            drop.completed(policy, 0, true);
            arm(&mut drop, 1);
        }
        assert_eq!(drop.pressure_policy(true), VideoDecodeDropPolicy::None);
        arm(&mut drop, 2);
        assert_eq!(drop.remaining_budget(), 10);
    }

    #[test]
    fn new_output_feedback_subtracts_attempts_still_in_flight() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        arm(&mut drop, 1);
        for _ in 0..4 {
            drop.submitted(drop.pressure_policy(true));
        }
        arm(&mut drop, 2);
        assert_eq!(drop.remaining_budget(), 6);
        drop.completed(drop.pressure_policy(true), 1, true);
        arm(&mut drop, 3);
        assert_eq!(drop.remaining_budget(), 7);
    }

    #[test]
    fn output_catchup_budget_is_bounded_and_clears_when_video_catches_up() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        drop.observe_output(true, feedback(1, 0), Some(u64::MAX), Some(40_000_000));
        assert_eq!(drop.remaining_budget(), 100);
        drop.observe_output(
            true,
            feedback(2, 9_900_000_000),
            Some(10_000_000_000),
            Some(40_000_000),
        );
        assert_eq!(drop.remaining_budget(), 0);
    }

    #[test]
    fn decoder_catchup_requires_output_feedback_pressure_and_known_duration() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        for (allowed, output, audio, duration) in [
            (false, feedback(1, 0), Some(1_000_000_000), Some(40_000_000)),
            (true, None, Some(1_000_000_000), Some(40_000_000)),
            (true, feedback(1, 0), None, Some(40_000_000)),
            (true, feedback(1, 0), Some(1_000_000_000), None),
            (true, feedback(1, 0), Some(1_000_000_000), Some(0)),
        ] {
            drop.observe_output(allowed, output, audio, duration);
            assert_eq!(drop.pressure_policy(true), VideoDecodeDropPolicy::None);
        }
    }

    #[test]
    fn disabling_cancels_queued_pressure_policies_even_after_reenabling() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        arm(&mut drop, 1);
        let queued = drop.pressure_policy(true);
        let epoch = drop.epoch_handle();
        assert!(
            queued
                .effective(epoch.load(Ordering::Acquire))
                .skip_nonref()
        );
        drop.set_enabled(false);
        assert_eq!(
            queued.effective(epoch.load(Ordering::Acquire)),
            VideoDecodeDropPolicy::None
        );
        drop.set_enabled(true);
        arm(&mut drop, 2);
        assert_eq!(drop.submission_policy(queued), VideoDecodeDropPolicy::None);
        assert!(drop.pressure_policy(true).skip_nonref());
    }

    #[test]
    fn seek_reset_cancels_old_pressure_without_changing_opt_in_or_seek_policy() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        arm(&mut drop, 1);
        let queued = drop.pressure_policy(true);
        drop.reset();
        assert!(drop.enabled());
        assert_eq!(drop.submission_policy(queued), VideoDecodeDropPolicy::None);
        assert_eq!(drop.remaining_budget(), 0);
        drop.set_enabled(false);
        assert_eq!(
            drop.submission_policy(VideoDecodeDropPolicy::SeekPreroll),
            VideoDecodeDropPolicy::SeekPreroll
        );
    }

    #[test]
    fn backpressure_does_not_spend_budget_before_command_is_queued() {
        let mut drop = VideoDecodeFrameDrop::default();
        drop.set_enabled(true);
        arm(&mut drop, 1);
        let pending = drop.pressure_policy(true);
        for _ in 0..20 {
            assert_eq!(drop.submission_policy(pending), pending);
        }
        assert_eq!(drop.remaining_budget(), 10);
        drop.submitted(pending);
        assert_eq!(drop.remaining_budget(), 9);
    }

    #[test]
    fn drop_statistics_separate_attempts_preroll_and_estimates_after_output_resumes() {
        let mut drop = VideoDecodeFrameDrop::default();
        let pressure = VideoDecodeDropPolicy::Pressure { epoch: 1 };
        let seek = VideoDecodeDropPolicy::SeekPreroll;
        drop.completed(seek, 0, true); // Decoder warm-up is not a dropped frame.
        drop.completed(VideoDecodeDropPolicy::None, 1, true);
        drop.completed(pressure, 0, true);
        drop.completed(pressure, 0, true);
        drop.completed(seek, 0, true);
        drop.completed(VideoDecodeDropPolicy::None, 0, true);
        assert_eq!(drop.stats().estimated_pressure_drops, 0);
        drop.completed(VideoDecodeDropPolicy::None, 1, true);
        assert_eq!(
            drop.stats(),
            VideoDecodeDropStats {
                pressure_attempts: 2,
                seek_preroll_attempts: 2,
                estimated_pressure_drops: 2,
                estimated_seek_preroll_drops: 1,
            }
        );
    }

    #[test]
    fn errors_and_flushes_discard_unconfirmed_drop_estimates() {
        let mut drop = VideoDecodeFrameDrop::default();
        let pressure = VideoDecodeDropPolicy::Pressure { epoch: 1 };
        drop.completed(VideoDecodeDropPolicy::None, 1, true);
        drop.completed(pressure, 0, true);
        drop.completed(VideoDecodeDropPolicy::None, 0, false);
        drop.completed(VideoDecodeDropPolicy::None, 1, true);
        assert_eq!(drop.stats().estimated_pressure_drops, 0);
        drop.completed(pressure, 0, true);
        drop.reset();
        drop.completed(VideoDecodeDropPolicy::None, 1, true);
        assert_eq!(drop.stats().estimated_pressure_drops, 0);
        assert_eq!(drop.stats().pressure_attempts, 2);
    }
}
