use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, PendingStartAudio,
    duration_nsecs,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::backend::ffmpeg) struct PendingAudioUnderrunRecoveryPlan {
    pub(in crate::backend::ffmpeg) audio_start_timeline_nsecs: u64,
    pub(in crate::backend::ffmpeg) audio_flush_until_timeline_nsecs: u64,
    pub(in crate::backend::ffmpeg) reset_audio_to_timeline_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UnderrunRecoveryBlocked {
    PendingAudioEmpty,
    MissingVideoWindow,
    PendingAudioGap,
    NoNextAudio,
    NoStageableCoverage,
    AudioBeforeVideoWindow,
    AudioPastVideoWindow,
    VideoWindowBelowPrefill,
    PendingPayloadBelowPrefill,
    OutputStagingNoProgress,
}

impl UnderrunRecoveryBlocked {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::PendingAudioEmpty => "pending_audio_empty",
            Self::MissingVideoWindow => "missing_video_window",
            Self::PendingAudioGap => "pending_audio_gap",
            Self::NoNextAudio => "no_next_audio",
            Self::NoStageableCoverage => "no_stageable_coverage",
            Self::AudioBeforeVideoWindow => "audio_before_video_window",
            Self::AudioPastVideoWindow => "audio_past_video_window",
            Self::VideoWindowBelowPrefill => "video_window_below_prefill",
            Self::PendingPayloadBelowPrefill => "pending_payload_below_prefill",
            Self::OutputStagingNoProgress => "output_staging_no_progress",
        }
    }
}

pub(super) fn minimum_recovery_input_nsecs(
    output_payload_nsecs: u64,
    tempo_pending_nsecs: u64,
) -> u64 {
    if output_payload_nsecs > 0 && tempo_pending_nsecs > 0 {
        0
    } else {
        duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION).saturating_sub(output_payload_nsecs)
    }
}

pub(in crate::backend::ffmpeg) fn pending_audio_underrun_recovery_plan(
    pending_audio: &PendingStartAudio,
    played_timeline_nsecs: u64,
    output_payload_nsecs: u64,
    output_buffered_until_nsecs: u64,
    output_tempo_pending_nsecs: u64,
    video_start_timeline_nsecs: Option<u64>,
    video_buffered_until_nsecs: Option<u64>,
) -> Option<PendingAudioUnderrunRecoveryPlan> {
    pending_audio_underrun_recovery_decision(
        pending_audio,
        played_timeline_nsecs,
        output_payload_nsecs,
        output_buffered_until_nsecs,
        output_tempo_pending_nsecs,
        video_start_timeline_nsecs,
        video_buffered_until_nsecs,
    )
    .ok()
}

pub(super) fn pending_audio_underrun_recovery_decision(
    pending_audio: &PendingStartAudio,
    played_timeline_nsecs: u64,
    output_payload_nsecs: u64,
    output_buffered_until_nsecs: u64,
    output_tempo_pending_nsecs: u64,
    video_start_timeline_nsecs: Option<u64>,
    video_buffered_until_nsecs: Option<u64>,
) -> Result<PendingAudioUnderrunRecoveryPlan, UnderrunRecoveryBlocked> {
    if pending_audio.is_empty() {
        return Err(UnderrunRecoveryBlocked::PendingAudioEmpty);
    }

    let video_start_timeline_nsecs =
        video_start_timeline_nsecs.ok_or(UnderrunRecoveryBlocked::MissingVideoWindow)?;
    let video_buffered_until_nsecs =
        video_buffered_until_nsecs.ok_or(UnderrunRecoveryBlocked::MissingVideoWindow)?;
    // During underrun the callback retains its short tail instead of consuming
    // it. Append from that tail; waiting for it to drain would deadlock prefill.
    let mut audio_start_timeline_nsecs = if output_payload_nsecs > 0 {
        output_buffered_until_nsecs.max(played_timeline_nsecs)
    } else {
        played_timeline_nsecs.max(video_start_timeline_nsecs)
    };
    let mut reset_audio_to_timeline_nsecs = (output_payload_nsecs == 0
        && audio_start_timeline_nsecs != played_timeline_nsecs)
        .then_some(audio_start_timeline_nsecs);
    let mut coverage =
        pending_audio.stageable_coverage_between(audio_start_timeline_nsecs, u64::MAX);

    if coverage.is_none() {
        if output_payload_nsecs > 0 {
            return Err(UnderrunRecoveryBlocked::PendingAudioGap);
        }
        let next_audio_start_nsecs = pending_audio
            .first_start_at_or_after(audio_start_timeline_nsecs)
            .ok_or(UnderrunRecoveryBlocked::NoNextAudio)?;
        audio_start_timeline_nsecs = next_audio_start_nsecs;
        reset_audio_to_timeline_nsecs = Some(next_audio_start_nsecs);
        coverage = pending_audio.stageable_coverage_between(audio_start_timeline_nsecs, u64::MAX);
    }

    let pending_buffered_until_nsecs = coverage
        .ok_or(UnderrunRecoveryBlocked::NoStageableCoverage)?
        .until_nsecs;
    if pending_buffered_until_nsecs <= audio_start_timeline_nsecs {
        return Err(UnderrunRecoveryBlocked::NoStageableCoverage);
    }
    let video_lead_until_nsecs =
        video_buffered_until_nsecs.saturating_add(duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION));
    // A retained tail may already lead the last scheduled video frame. Audio
    // has to finish prefilling before that frozen video clock can advance.
    // Keep the usual bounded lead for appends, while a new audio anchor still
    // requires actual video coverage before resetting the clock.
    let recovery_covered_until_nsecs = if output_payload_nsecs > 0 {
        video_lead_until_nsecs
    } else {
        video_buffered_until_nsecs
    };
    if audio_start_timeline_nsecs < video_start_timeline_nsecs {
        return Err(UnderrunRecoveryBlocked::AudioBeforeVideoWindow);
    }
    if audio_start_timeline_nsecs >= recovery_covered_until_nsecs {
        return Err(UnderrunRecoveryBlocked::AudioPastVideoWindow);
    }

    // A live tempo graph can release retained media in bursts. Permit bounded
    // feeding even when the new input alone cannot complete prefill; only the
    // callback can release the underrun clock after enough real PCM arrives.
    let minimum_input_nsecs =
        minimum_recovery_input_nsecs(output_payload_nsecs, output_tempo_pending_nsecs);
    let minimum_flush_until_nsecs = audio_start_timeline_nsecs.saturating_add(minimum_input_nsecs);
    if recovery_covered_until_nsecs < minimum_flush_until_nsecs {
        return Err(UnderrunRecoveryBlocked::VideoWindowBelowPrefill);
    }
    let audio_flush_until_timeline_nsecs = video_lead_until_nsecs.min(pending_buffered_until_nsecs);

    let staged_payload_nsecs = pending_audio
        .stageable_coverage_between(audio_start_timeline_nsecs, audio_flush_until_timeline_nsecs)
        .ok_or(UnderrunRecoveryBlocked::NoStageableCoverage)?
        .payload_nsecs;
    if staged_payload_nsecs < minimum_input_nsecs {
        return Err(UnderrunRecoveryBlocked::PendingPayloadBelowPrefill);
    }
    Ok(PendingAudioUnderrunRecoveryPlan {
        audio_start_timeline_nsecs,
        audio_flush_until_timeline_nsecs,
        reset_audio_to_timeline_nsecs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ffmpeg::DecodedAudio;

    fn pending(start: u64, end: u64) -> PendingStartAudio {
        let mut pending = PendingStartAudio::default();
        pending.push(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: end - start,
            },
            start,
            end,
        );
        pending
    }

    #[test]
    fn recovery_diagnostics_distinguish_missing_video_and_discontinuous_audio() {
        assert_eq!(
            pending_audio_underrun_recovery_decision(
                &pending(1_192_000_000, 2_000_000_000),
                1_000_000_000,
                305_000_000,
                1_192_000_000,
                113_000_000,
                None,
                None,
            ),
            Err(UnderrunRecoveryBlocked::MissingVideoWindow)
        );
        assert_eq!(
            pending_audio_underrun_recovery_decision(
                &pending(1_300_000_000, 2_000_000_000),
                1_000_000_000,
                305_000_000,
                1_192_000_000,
                113_000_000,
                Some(1_000_000_000),
                Some(2_000_000_000),
            ),
            Err(UnderrunRecoveryBlocked::PendingAudioGap)
        );
    }

    #[test]
    fn recovery_diagnostics_distinguish_video_lead_limit_from_prefill_deficit() {
        assert_eq!(
            pending_audio_underrun_recovery_decision(
                &pending(1_600_000_000, 2_100_000_000),
                1_000_000_000,
                192_000_000,
                1_600_000_000,
                0,
                Some(1_000_000_000),
                Some(1_100_000_000),
            ),
            Err(UnderrunRecoveryBlocked::AudioPastVideoWindow)
        );
        assert_eq!(
            pending_audio_underrun_recovery_decision(
                &pending(1_000_000_000, 1_500_000_000),
                1_000_000_000,
                0,
                1_000_000_000,
                0,
                Some(1_000_000_000),
                Some(1_200_000_000),
            ),
            Err(UnderrunRecoveryBlocked::VideoWindowBelowPrefill)
        );
        assert_eq!(
            pending_audio_underrun_recovery_decision(
                &pending(1_000_000_000, 1_200_000_000),
                1_000_000_000,
                0,
                1_000_000_000,
                0,
                Some(1_000_000_000),
                Some(2_000_000_000),
            ),
            Err(UnderrunRecoveryBlocked::PendingPayloadBelowPrefill)
        );
    }
}
