#[cfg(test)]
use super::AudioOutputUnstableSnapshot;
use super::{AudioOutputSnapshot, AudioOutputStableSnapshot};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) enum InitialAudioPreparePhase {
    #[default]
    Collecting,
    Preparing,
    Prepared,
    Committed,
    Aborted,
}

impl InitialAudioPreparePhase {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn as_str(self) -> &'static str {
        match self {
            Self::Collecting => "collecting",
            Self::Preparing => "preparing",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct InitialAudioPrepareToken {
    pub(in crate::player::backend::ffmpeg::playback_loop) transaction_id: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) discontinuity_epoch: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) seek_generation: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) audio_epoch: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) staged_range_nsecs: (u64, u64),
    pub(in crate::player::backend::ffmpeg::playback_loop) staged_frames: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) staged_samples: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) staged_until_nsecs: u64,
}

impl InitialAudioPrepareToken {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn covers_target(self) -> bool {
        self.staged_range_nsecs.0 <= self.target_nsecs
            && self.staged_until_nsecs > self.target_nsecs
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct PrestartAudioOwnershipInput {
    pub(in crate::player::backend::ffmpeg::playback_loop) phase: InitialAudioPreparePhase,
    pub(in crate::player::backend::ffmpeg::playback_loop) token: Option<InitialAudioPrepareToken>,
    pub(in crate::player::backend::ffmpeg::playback_loop) current_audio_epoch: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) current_seek_generation: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) snapshot: AudioOutputStableSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) enum PrestartAudioOwnership {
    CollectingEmpty,
    PreparedCurrentEpoch,
    StaleEpoch,
    UnexpectedCurrentEpoch,
    SnapshotUnstable,
}

impl PrestartAudioOwnership {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn as_str(self) -> &'static str {
        match self {
            Self::CollectingEmpty => "collecting_empty",
            Self::PreparedCurrentEpoch => "prepared_current_epoch",
            Self::StaleEpoch => "stale_epoch",
            Self::UnexpectedCurrentEpoch => "unexpected_current_epoch",
            Self::SnapshotUnstable => "snapshot_unstable",
        }
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop) fn classify_prestart_audio_ownership(
    input: PrestartAudioOwnershipInput,
) -> PrestartAudioOwnership {
    let AudioOutputStableSnapshot::Stable(snapshot) = input.snapshot else {
        return PrestartAudioOwnership::SnapshotUnstable;
    };
    if !snapshot_has_software_payload(snapshot) {
        if input.phase == InitialAudioPreparePhase::Collecting && input.token.is_none() {
            return PrestartAudioOwnership::CollectingEmpty;
        }
        if input.token.is_some_and(|token| {
            token.audio_epoch != input.current_audio_epoch
                || token.seek_generation != input.current_seek_generation
        }) {
            return PrestartAudioOwnership::StaleEpoch;
        }
        return PrestartAudioOwnership::UnexpectedCurrentEpoch;
    }
    if snapshot.audio_epoch != input.current_audio_epoch
        || snapshot.queue_generation != input.current_audio_epoch
    {
        return PrestartAudioOwnership::StaleEpoch;
    }
    let Some(token) = input.token else {
        return PrestartAudioOwnership::UnexpectedCurrentEpoch;
    };
    if token.audio_epoch != input.current_audio_epoch
        || token.seek_generation != input.current_seek_generation
    {
        return PrestartAudioOwnership::StaleEpoch;
    }
    if token.target_nsecs != input.target_nsecs {
        return PrestartAudioOwnership::UnexpectedCurrentEpoch;
    }
    let snapshot_covers_target = snapshot
        .payload_range_nsecs
        .is_some_and(|(start, end)| start <= input.target_nsecs && end > input.target_nsecs);
    if input.phase == InitialAudioPreparePhase::Prepared
        && token.covers_target()
        && snapshot_covers_target
        && !snapshot.queue_active
    {
        PrestartAudioOwnership::PreparedCurrentEpoch
    } else {
        PrestartAudioOwnership::UnexpectedCurrentEpoch
    }
}

fn snapshot_has_software_payload(snapshot: AudioOutputSnapshot) -> bool {
    snapshot.shared_payload_nsecs > 0
        || snapshot.queue_pending_nsecs > 0
        || snapshot.worker_in_flight_nsecs > 0
        || snapshot.queue_frames > 0
        || snapshot.worker_in_flight_frames > 0
}

#[cfg(test)]
mod tests {
    use std::thread;

    use crate::player::backend::ffmpeg::{AudioOutputLifecycle, FfmpegControl};
    use crate::player::render_host::PlaybackSessionId;

    use super::*;

    fn token() -> InitialAudioPrepareToken {
        InitialAudioPrepareToken {
            transaction_id: 17,
            discontinuity_epoch: 41,
            seek_generation: 9,
            audio_epoch: 12,
            target_nsecs: 1_050_500_000_000,
            staged_range_nsecs: (1_050_500_000_000, 1_050_900_000_000),
            staged_frames: 19,
            staged_samples: 38_912,
            staged_until_nsecs: 1_050_900_000_000,
        }
    }

    fn prepared_snapshot() -> AudioOutputSnapshot {
        AudioOutputSnapshot {
            audio_epoch: 12,
            stable_version: Some(44),
            queue_pending_nsecs: 400_000_000,
            total_pending_nsecs: 400_000_000,
            queue_frames: 19,
            queue_generation: 12,
            payload_range_nsecs: Some((1_050_500_000_000, 1_050_900_000_000)),
            queue_active: false,
            ..AudioOutputSnapshot::default()
        }
    }

    fn input(
        phase: InitialAudioPreparePhase,
        token: Option<InitialAudioPrepareToken>,
        snapshot: AudioOutputStableSnapshot,
    ) -> PrestartAudioOwnershipInput {
        PrestartAudioOwnershipInput {
            phase,
            token,
            current_audio_epoch: 12,
            current_seek_generation: 9,
            target_nsecs: 1_050_500_000_000,
            snapshot,
        }
    }

    #[test]
    fn primed_stage_only_payload_is_prepared_without_becoming_active() {
        let control = FfmpegControl::new(PlaybackSessionId(17));
        let seek_generation = control.request_seek();
        control.finish_seek(seek_generation);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
        let snapshot = prepared_snapshot();
        assert!(!snapshot.queue_active);
        assert_eq!(
            control.audio_output_lifecycle(),
            AudioOutputLifecycle::Ready
        );
        assert!(
            control
                .audio_output_control_snapshot()
                .paused_by_seek_transition()
        );
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Prepared,
                Some(token()),
                AudioOutputStableSnapshot::Stable(snapshot),
            )),
            PrestartAudioOwnership::PreparedCurrentEpoch
        );
    }

    #[test]
    fn ownership_classifier_distinguishes_all_recovery_classes() {
        let empty_with_driver_delay = AudioOutputSnapshot {
            audio_epoch: 12,
            stable_version: Some(2),
            driver_delay_nsecs: 40_000_000,
            shared_pending_nsecs: 40_000_000,
            total_pending_nsecs: 40_000_000,
            queue_generation: 12,
            ..AudioOutputSnapshot::default()
        };
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Collecting,
                None,
                AudioOutputStableSnapshot::Stable(empty_with_driver_delay),
            )),
            PrestartAudioOwnership::CollectingEmpty
        );
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Prepared,
                Some(token()),
                AudioOutputStableSnapshot::Stable(empty_with_driver_delay),
            )),
            PrestartAudioOwnership::UnexpectedCurrentEpoch
        );

        let prepared = prepared_snapshot();
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Collecting,
                None,
                AudioOutputStableSnapshot::Stable(prepared),
            )),
            PrestartAudioOwnership::UnexpectedCurrentEpoch
        );

        let active_prepared = AudioOutputSnapshot {
            queue_active: true,
            ..prepared
        };
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Prepared,
                Some(token()),
                AudioOutputStableSnapshot::Stable(active_prepared),
            )),
            PrestartAudioOwnership::UnexpectedCurrentEpoch
        );

        let stale = AudioOutputSnapshot {
            audio_epoch: 11,
            queue_generation: 11,
            ..prepared
        };
        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Prepared,
                Some(token()),
                AudioOutputStableSnapshot::Stable(stale),
            )),
            PrestartAudioOwnership::StaleEpoch
        );

        assert_eq!(
            classify_prestart_audio_ownership(input(
                InitialAudioPreparePhase::Prepared,
                Some(token()),
                AudioOutputStableSnapshot::SnapshotUnstable(AudioOutputUnstableSnapshot {
                    audio_epoch: 12,
                    observed_version: 45,
                    attempts: 8,
                }),
            )),
            PrestartAudioOwnership::SnapshotUnstable
        );
    }

    #[test]
    fn debug_race_classification_repeats_ten_thousand_times_without_unwind() {
        let join = thread::Builder::new()
            .name("initial-audio-race-regression".to_string())
            .spawn(|| {
                for iteration in 0..10_000 {
                    let mut snapshot = prepared_snapshot();
                    if iteration % 3 == 0 {
                        snapshot.audio_epoch = 11;
                        snapshot.queue_generation = 11;
                    } else if iteration % 3 == 1 {
                        snapshot.queue_pending_nsecs = 0;
                        snapshot.queue_frames = 0;
                        snapshot.total_pending_nsecs = snapshot.driver_delay_nsecs;
                        snapshot.payload_range_nsecs = None;
                    }
                    let _ = classify_prestart_audio_ownership(input(
                        InitialAudioPreparePhase::Prepared,
                        Some(token()),
                        AudioOutputStableSnapshot::Stable(snapshot),
                    ));
                }
            })
            .expect("spawn classifier regression thread");

        assert!(join.join().is_ok());
    }
}
