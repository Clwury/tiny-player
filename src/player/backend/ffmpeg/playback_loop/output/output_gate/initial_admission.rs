use super::{
    Duration, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VIDEO_OUTPUT_START_FAST_READY_DURATION,
    VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS,
    duration_nsecs,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialAvPair {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) video_anchor_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) audio_start_target_nsecs:
        u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialStartAdmissionMode {
    FastLookahead,
    CachedExactConfirmed,
    PrimeExactPair,
}

impl InitialStartAdmissionMode {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn as_str(
        self,
    ) -> &'static str {
        match self {
            Self::FastLookahead => "fast_lookahead",
            Self::CachedExactConfirmed => "cached_exact_confirmed",
            Self::PrimeExactPair => "prime_exact_pair",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialStartBlockReason {
    NoVideoFrame,
    NoAudioFrame,
    NoAudioCoverage,
    AudioVideoOffset,
    ActiveRecovery,
    InsufficientLookahead,
    SingleFrameNotVulkan,
    TargetFrameNotConfirmed,
    FrameNotConfirmedClean,
}

impl InitialStartBlockReason {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn as_str(
        self,
    ) -> &'static str {
        match self {
            Self::NoVideoFrame => "no_video_frame",
            Self::NoAudioFrame => "no_audio_frame",
            Self::NoAudioCoverage => "no_audio_coverage",
            Self::AudioVideoOffset => "audio_video_offset",
            Self::ActiveRecovery => "active_recovery",
            Self::InsufficientLookahead => "insufficient_lookahead",
            Self::SingleFrameNotVulkan => "single_frame_not_vulkan",
            Self::TargetFrameNotConfirmed => "target_frame_not_confirmed",
            Self::FrameNotConfirmedClean => "frame_not_confirmed_clean",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) enum InitialStartAdmission {
    Waiting(InitialStartBlockReason),
    Prime {
        pair: InitialAvPair,
        mode: InitialStartAdmissionMode,
    },
}

impl InitialStartAdmission {
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn blocked_on(
        self,
    ) -> Option<InitialStartBlockReason> {
        match self {
            Self::Waiting(reason) => Some(reason),
            Self::Prime { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialStartAdmissionInput
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) expected_target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_video_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_audio_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_video_forward_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) strict_video_forward_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) decoded_audio_forward_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) contiguous_video_frames:
        usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_video_duration_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_following_video_gap_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_frame_is_vulkan: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) first_frame_confirmed_clean:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) active_recovery: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) require_strict_fast_lookahead:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) cached_exact_landing_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) startup_sync_elapsed:
        Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) struct InitialStartAdmissionEvaluation
{
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) pair: Option<InitialAvPair>,
    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) admission:
        InitialStartAdmission,
}

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn initial_start_admission(
    input: InitialStartAdmissionInput,
) -> InitialStartAdmissionEvaluation {
    let Some(video_anchor_nsecs) = input.first_video_nsecs else {
        return waiting(InitialStartBlockReason::NoVideoFrame, None);
    };
    let Some(observed_audio_start_nsecs) = input.first_audio_nsecs else {
        return waiting(InitialStartBlockReason::NoAudioFrame, None);
    };
    if input
        .decoded_audio_forward_nsecs
        .is_none_or(|forward_nsecs| forward_nsecs == 0)
    {
        return waiting(InitialStartBlockReason::NoAudioCoverage, None);
    }
    if video_anchor_nsecs.abs_diff(observed_audio_start_nsecs)
        > duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE)
    {
        return waiting(InitialStartBlockReason::AudioVideoOffset, None);
    }
    // Audio that begins slightly before the first video frame is already covered
    // by the decoded queue, but the output transaction must not wait for a media
    // timestamp that precedes its video anchor. Start both clocks at the video
    // anchor in that case, matching the prior direct-start behavior.
    let pair = InitialAvPair {
        video_anchor_nsecs,
        audio_start_target_nsecs: observed_audio_start_nsecs.max(video_anchor_nsecs),
    };
    if input.active_recovery {
        return waiting(InitialStartBlockReason::ActiveRecovery, Some(pair));
    }

    // Cached exact HEVC seeks retain the strict 80 ms rule until the exact
    // target has produced a confirmed-clean frame. Once that evidence scope is
    // closed, two admitted (therefore clean) frames with at most one frame
    // period of finite gap are sufficient. This is deliberately a dedicated
    // path; the global 800 ms single-frame fallback remains unchanged.
    let cached_exact_landing_confirmed = input
        .cached_exact_landing_nsecs
        .zip(input.first_video_duration_nsecs)
        .is_some_and(|(landing_nsecs, frame_duration_nsecs)| {
            landing_nsecs == video_anchor_nsecs
                && landing_nsecs.abs_diff(input.expected_target_nsecs)
                    <= frame_duration_nsecs.saturating_add(VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS)
        });
    let cached_exact_fast_path_ready = input.require_strict_fast_lookahead
        && cached_exact_landing_confirmed
        && input.first_frame_confirmed_clean
        && input.contiguous_video_frames >= 2
        && input
            .first_video_duration_nsecs
            .zip(input.first_following_video_gap_nsecs)
            .is_some_and(|(frame_duration_nsecs, gap_nsecs)| {
                gap_nsecs
                    <= frame_duration_nsecs.saturating_add(VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS)
            });
    let strict_fast_lookahead_ready =
        input
            .strict_video_forward_nsecs
            .is_some_and(|forward_nsecs| {
                forward_nsecs >= duration_nsecs(VIDEO_OUTPUT_START_FAST_READY_DURATION)
            });
    let fast_lookahead_mode = if cached_exact_fast_path_ready {
        Some(InitialStartAdmissionMode::CachedExactConfirmed)
    } else if input.require_strict_fast_lookahead {
        strict_fast_lookahead_ready.then_some(InitialStartAdmissionMode::FastLookahead)
    } else {
        (input.contiguous_video_frames >= 2
            || input
                .decoded_video_forward_nsecs
                .is_some_and(|forward_nsecs| {
                    forward_nsecs >= duration_nsecs(VIDEO_OUTPUT_START_FAST_READY_DURATION)
                }))
        .then_some(InitialStartAdmissionMode::FastLookahead)
    };
    if let Some(mode) = fast_lookahead_mode {
        return InitialStartAdmissionEvaluation {
            pair: Some(pair),
            admission: InitialStartAdmission::Prime { pair, mode },
        };
    }

    let bounded_single_frame_ready = input
        .startup_sync_elapsed
        .is_some_and(|elapsed| elapsed >= VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER);
    if !bounded_single_frame_ready {
        return waiting(InitialStartBlockReason::InsufficientLookahead, Some(pair));
    }
    if !input.first_frame_is_vulkan {
        return waiting(InitialStartBlockReason::SingleFrameNotVulkan, Some(pair));
    }
    let target_frame_confirmed = if input.require_strict_fast_lookahead {
        cached_exact_landing_confirmed
    } else {
        video_anchor_nsecs == input.expected_target_nsecs
    };
    if !target_frame_confirmed {
        return waiting(InitialStartBlockReason::TargetFrameNotConfirmed, Some(pair));
    }
    if !input.first_frame_confirmed_clean {
        return waiting(InitialStartBlockReason::FrameNotConfirmedClean, Some(pair));
    }

    InitialStartAdmissionEvaluation {
        pair: Some(pair),
        admission: InitialStartAdmission::Prime {
            pair,
            mode: InitialStartAdmissionMode::PrimeExactPair,
        },
    }
}

fn waiting(
    reason: InitialStartBlockReason,
    pair: Option<InitialAvPair>,
) -> InitialStartAdmissionEvaluation {
    InitialStartAdmissionEvaluation {
        pair,
        admission: InitialStartAdmission::Waiting(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> InitialStartAdmissionInput {
        InitialStartAdmissionInput {
            expected_target_nsecs: 184_700_000_000,
            first_video_nsecs: Some(184_700_000_000),
            first_audio_nsecs: Some(184_714_739_000),
            decoded_video_forward_nsecs: Some(33_333_333),
            strict_video_forward_nsecs: Some(33_333_333),
            decoded_audio_forward_nsecs: Some(641_677_848),
            contiguous_video_frames: 1,
            first_video_duration_nsecs: Some(33_333_333),
            first_following_video_gap_nsecs: None,
            first_frame_is_vulkan: true,
            first_frame_confirmed_clean: true,
            active_recovery: false,
            require_strict_fast_lookahead: false,
            cached_exact_landing_nsecs: None,
            startup_sync_elapsed: Some(Duration::from_millis(800)),
        }
    }

    #[test]
    fn exact_vulkan_pair_uses_bounded_single_frame_admission() {
        let evaluation = initial_start_admission(input());
        assert_eq!(
            evaluation.admission,
            InitialStartAdmission::Prime {
                pair: InitialAvPair {
                    video_anchor_nsecs: 184_700_000_000,
                    audio_start_target_nsecs: 184_714_739_000,
                },
                mode: InitialStartAdmissionMode::PrimeExactPair,
            }
        );
    }

    #[test]
    fn single_vulkan_frame_waits_until_bounded_fallback_deadline() {
        let mut input = input();
        input.startup_sync_elapsed = Some(Duration::from_millis(799));
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::InsufficientLookahead)
        );
    }

    #[test]
    fn fast_lookahead_does_not_depend_on_demux_or_vulkan() {
        let mut input = input();
        input.contiguous_video_frames = 2;
        input.decoded_video_forward_nsecs = Some(66_666_666);
        input.first_frame_is_vulkan = false;
        input.startup_sync_elapsed = Some(Duration::from_millis(10));
        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                mode: InitialStartAdmissionMode::FastLookahead,
                ..
            }
        ));
    }

    #[test]
    fn cached_hevc_fast_lookahead_requires_strict_eighty_ms_coverage() {
        let mut input = input();
        input.contiguous_video_frames = 2;
        input.decoded_video_forward_nsecs = Some(100_000_000);
        input.strict_video_forward_nsecs = Some(66_666_666);
        input.require_strict_fast_lookahead = true;
        input.startup_sync_elapsed = Some(Duration::from_millis(10));

        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::InsufficientLookahead)
        );

        input.strict_video_forward_nsecs =
            Some(duration_nsecs(VIDEO_OUTPUT_START_FAST_READY_DURATION));
        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                mode: InitialStartAdmissionMode::FastLookahead,
                ..
            }
        ));
    }

    #[test]
    fn initial_transaction_requires_real_audio_ownership_even_for_gap_recovery() {
        let mut input = input();
        input.first_audio_nsecs = None;
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::NoAudioFrame)
        );

        input.first_audio_nsecs = input.first_video_nsecs;
        input.decoded_audio_forward_nsecs = None;
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::NoAudioCoverage)
        );
    }

    #[test]
    fn initial_transaction_rejects_audio_delay_beyond_eighty_ms() {
        let mut input = input();
        input.first_audio_nsecs = input
            .first_video_nsecs
            .map(|video| video + duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE) + 1);

        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::AudioVideoOffset)
        );
    }

    #[test]
    fn confirmed_cached_exact_hevc_allows_two_clean_frames_with_one_period_gap() {
        let mut input = input();
        input.expected_target_nsecs = 165_266_666_667;
        input.first_video_nsecs = Some(165_266_666_667);
        input.first_audio_nsecs = Some(165_279_637_171);
        input.contiguous_video_frames = 2;
        input.first_video_duration_nsecs = Some(33_333_333);
        input.first_following_video_gap_nsecs = Some(33_333_333);
        input.decoded_video_forward_nsecs = Some(100_000_000);
        input.strict_video_forward_nsecs = Some(33_333_333);
        input.require_strict_fast_lookahead = true;
        input.cached_exact_landing_nsecs = input.first_video_nsecs;
        input.startup_sync_elapsed = Some(Duration::from_millis(10));

        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                mode: InitialStartAdmissionMode::CachedExactConfirmed,
                ..
            }
        ));

        input.first_following_video_gap_nsecs = Some(33_334_334);
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::InsufficientLookahead)
        );
    }

    #[test]
    fn confirmed_cached_exact_hevc_accepts_first_eligible_frame_after_target() {
        let mut input = input();
        input.expected_target_nsecs = 486_433_333_333;
        input.first_video_nsecs = Some(486_466_666_667);
        input.first_audio_nsecs = input.first_video_nsecs;
        input.contiguous_video_frames = 2;
        input.first_video_duration_nsecs = Some(33_333_333);
        input.first_following_video_gap_nsecs = Some(33_333_333);
        input.decoded_video_forward_nsecs = Some(100_000_000);
        input.strict_video_forward_nsecs = Some(33_333_333);
        input.require_strict_fast_lookahead = true;
        input.cached_exact_landing_nsecs = input.first_video_nsecs;
        input.startup_sync_elapsed = Some(Duration::from_millis(10));

        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                mode: InitialStartAdmissionMode::CachedExactConfirmed,
                ..
            }
        ));

        input.first_video_nsecs = Some(486_500_000_000);
        input.first_audio_nsecs = input.first_video_nsecs;
        input.cached_exact_landing_nsecs = input.first_video_nsecs;
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::InsufficientLookahead)
        );
    }

    #[test]
    fn audio_leading_video_within_tolerance_clamps_transaction_to_video_anchor() {
        let mut input = input();
        input.first_audio_nsecs = Some(184_684_000_000);
        input.contiguous_video_frames = 2;
        input.decoded_video_forward_nsecs = Some(66_666_666);
        input.startup_sync_elapsed = Some(Duration::from_millis(10));
        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                pair: InitialAvPair {
                    video_anchor_nsecs: 184_700_000_000,
                    audio_start_target_nsecs: 184_700_000_000,
                },
                mode: InitialStartAdmissionMode::FastLookahead,
            }
        ));
    }

    #[test]
    fn active_recovery_blocks_even_after_single_frame_deadline() {
        let mut input = input();
        input.active_recovery = true;
        assert_eq!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Waiting(InitialStartBlockReason::ActiveRecovery)
        );
    }

    #[test]
    fn historical_zero_output_is_not_an_input_to_clean_frame_admission() {
        let input = input();
        assert!(matches!(
            initial_start_admission(input).admission,
            InitialStartAdmission::Prime {
                mode: InitialStartAdmissionMode::PrimeExactPair,
                ..
            }
        ));
    }
}
