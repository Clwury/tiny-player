use super::{
    DemuxReaderWatermark, Instant, OutputGateResumeTiming, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE,
    duration_nsecs,
};

pub(in crate::backend::ffmpeg::playback_loop::output_gate) fn timed_output_gate_demux_watermark<F>(
    demux_watermark: &mut F,
    timing: &mut OutputGateResumeTiming,
) -> DemuxReaderWatermark
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let watermark = demux_watermark();
    timing.demux_watermark += started_at.elapsed();
    watermark
}

pub(in crate::backend::ffmpeg::playback_loop) fn demux_watermark_with_initial_combined_coverage(
    mut demux_watermark: DemuxReaderWatermark,
    exact_target_nsecs: u64,
    decoded_video_until_nsecs: Option<u64>,
    video_reader_nsecs: Option<u64>,
    pending_audio_range_nsecs: Option<(u64, u64)>,
    audio_reader_nsecs: Option<u64>,
    has_audio_output: bool,
) -> DemuxReaderWatermark {
    let continuity_tolerance_nsecs = duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE);
    let combined_video_forward_nsecs = decoded_video_until_nsecs.and_then(|until_nsecs| {
        initial_stream_forward_nsecs(
            exact_target_nsecs,
            (exact_target_nsecs, until_nsecs),
            video_reader_nsecs,
            demux_watermark.video_forward_nsecs,
            demux_watermark.video_underrun,
            continuity_tolerance_nsecs,
        )
    });
    if let Some(combined_video_forward_nsecs) = combined_video_forward_nsecs {
        demux_watermark.video_forward_nsecs = Some(combined_video_forward_nsecs);
        demux_watermark.video_underrun = false;
    }
    let combined_audio_forward_nsecs = has_audio_output
        .then(|| {
            pending_audio_range_nsecs.and_then(|range| {
                initial_stream_forward_nsecs(
                    exact_target_nsecs,
                    range,
                    audio_reader_nsecs,
                    demux_watermark.audio_forward_nsecs,
                    demux_watermark.audio_underrun,
                    continuity_tolerance_nsecs,
                )
            })
        })
        .flatten();
    if let Some(combined_audio_forward_nsecs) = combined_audio_forward_nsecs {
        demux_watermark.audio_forward_nsecs = Some(combined_audio_forward_nsecs);
        demux_watermark.audio_underrun = false;
    }
    demux_watermark.underrun =
        demux_watermark.video_underrun || (has_audio_output && demux_watermark.audio_underrun);
    demux_watermark.selected_min_forward_nsecs = if has_audio_output {
        demux_watermark
            .video_forward_nsecs
            .zip(demux_watermark.audio_forward_nsecs)
            .map(|(video, audio)| video.min(audio))
            .or(demux_watermark.video_forward_nsecs)
            .or(demux_watermark.audio_forward_nsecs)
    } else {
        demux_watermark.video_forward_nsecs
    };
    demux_watermark
}

fn initial_stream_forward_nsecs(
    exact_target_nsecs: u64,
    downstream_range_nsecs: (u64, u64),
    demux_reader_nsecs: Option<u64>,
    demux_forward_nsecs: Option<u64>,
    demux_underrun: bool,
    continuity_tolerance_nsecs: u64,
) -> Option<u64> {
    let (downstream_start_nsecs, downstream_end_nsecs) = downstream_range_nsecs;
    if downstream_end_nsecs <= exact_target_nsecs
        || downstream_start_nsecs > exact_target_nsecs.saturating_add(continuity_tolerance_nsecs)
    {
        return None;
    }
    let downstream_forward_nsecs = downstream_end_nsecs.saturating_sub(exact_target_nsecs);
    let combined_end_nsecs = demux_reader_nsecs
        .zip(demux_forward_nsecs)
        .filter(|(reader_nsecs, demux_forward_nsecs)| {
            *reader_nsecs <= downstream_end_nsecs.saturating_add(continuity_tolerance_nsecs)
                && reader_nsecs
                    .saturating_add(*demux_forward_nsecs)
                    .saturating_add(continuity_tolerance_nsecs)
                    >= downstream_end_nsecs
        })
        .map(|(reader_nsecs, demux_forward_nsecs)| reader_nsecs.saturating_add(demux_forward_nsecs))
        .unwrap_or(downstream_end_nsecs);
    let contiguous_forward_nsecs = combined_end_nsecs
        .saturating_sub(exact_target_nsecs)
        .max(downstream_forward_nsecs);
    // Like mpv's cache pause, prefetch measures readable demux packets
    // independently of output readiness. Decoder delay/in-flight frames can
    // put the reader ahead of the decoded prefix; that must not shorten a
    // healthy packet cache and make startup wait for the cache size limit.
    // Only contiguous ranges are joined above. An underrun reader provides no
    // independent lower bound: it still needs coverage from decoded output.
    let readable_packet_forward_nsecs = if demux_underrun {
        0
    } else {
        demux_forward_nsecs.unwrap_or_default()
    };
    Some(contiguous_forward_nsecs.max(readable_packet_forward_nsecs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_packet_waterline_survives_decoder_readers_ahead_of_output() {
        // 34:45.730 seek: both readers have advanced beyond the decoded
        // contiguous prefix, while their packet queues keep filling.
        let target_nsecs = 2_085_730_000_000;
        for (video_forward_nsecs, audio_forward_nsecs) in [
            (20_728_000_000, 20_341_000_000),
            (29_612_000_000, 29_034_000_000),
            (64_647_000_000, 64_223_000_000),
        ] {
            let raw = DemuxReaderWatermark {
                video_forward_nsecs: Some(video_forward_nsecs),
                audio_forward_nsecs: Some(audio_forward_nsecs),
                selected_min_forward_nsecs: Some(audio_forward_nsecs),
                ..DemuxReaderWatermark::default()
            };
            let combined = demux_watermark_with_initial_combined_coverage(
                raw,
                target_nsecs,
                Some(2_087_084_708_333),
                Some(2_087_544_249_998),
                Some((2_085_750_000_000, 2_085_919_303_816)),
                Some(2_087_798_000_000),
                true,
            );

            assert_eq!(combined.video_forward_nsecs, raw.video_forward_nsecs);
            assert_eq!(combined.audio_forward_nsecs, raw.audio_forward_nsecs);
            assert_eq!(
                combined.selected_min_forward_nsecs,
                Some(audio_forward_nsecs)
            );
            assert!(!combined.underrun);
            assert!(
                !combined.idle,
                "startup must not depend on filling the cache limit"
            );
        }
    }

    #[test]
    fn startup_packet_waterline_survives_video_only_decoder_delay() {
        let target_nsecs = 10_000_000_000;
        let combined = demux_watermark_with_initial_combined_coverage(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(20_000_000_000),
                selected_min_forward_nsecs: Some(20_000_000_000),
                ..DemuxReaderWatermark::default()
            },
            target_nsecs,
            Some(target_nsecs + 500_000_000),
            Some(target_nsecs + 2_000_000_000),
            None,
            None,
            false,
        );

        assert_eq!(combined.selected_min_forward_nsecs, Some(20_000_000_000));
        assert!(!combined.underrun);
    }

    #[test]
    fn startup_packet_waterline_does_not_bridge_decoder_gaps_below_prefetch_target() {
        let target_nsecs = 10_000_000_000;
        let combined = demux_watermark_with_initial_combined_coverage(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(800_000_000),
                audio_forward_nsecs: Some(700_000_000),
                selected_min_forward_nsecs: Some(700_000_000),
                ..DemuxReaderWatermark::default()
            },
            target_nsecs,
            Some(target_nsecs + 500_000_000),
            Some(target_nsecs + 2_000_000_000),
            Some((target_nsecs, target_nsecs + 169_000_000)),
            Some(target_nsecs + 2_000_000_000),
            true,
        );

        assert_eq!(combined.selected_min_forward_nsecs, Some(700_000_000));
    }

    #[test]
    fn startup_waterline_joins_exact_decoded_and_demux_coverage() {
        let target_nsecs = 184_700_000_000;
        let decoded_until_nsecs = 186_300_000_000;
        let raw = DemuxReaderWatermark {
            video_forward_nsecs: Some(900_000_000),
            audio_forward_nsecs: Some(2_500_000_000),
            selected_min_forward_nsecs: Some(900_000_000),
            video_underrun: true,
            underrun: true,
            ..DemuxReaderWatermark::default()
        };

        let combined = demux_watermark_with_initial_combined_coverage(
            raw,
            target_nsecs,
            Some(decoded_until_nsecs),
            Some(decoded_until_nsecs),
            None,
            None,
            true,
        );

        assert_eq!(combined.video_forward_nsecs, Some(2_500_000_000));
        assert_eq!(combined.selected_min_forward_nsecs, Some(2_500_000_000));
        assert!(!combined.video_underrun);
        assert!(!combined.underrun);
    }

    #[test]
    fn startup_waterline_does_not_replace_missing_exact_target_coverage() {
        let raw = DemuxReaderWatermark {
            video_forward_nsecs: Some(900_000_000),
            video_underrun: true,
            underrun: true,
            ..DemuxReaderWatermark::default()
        };

        let combined = demux_watermark_with_initial_combined_coverage(
            raw,
            184_700_000_000,
            None,
            Some(186_300_000_000),
            None,
            None,
            false,
        );

        assert_eq!(combined.video_forward_nsecs, Some(900_000_000));
        assert!(combined.video_underrun);
        assert!(combined.underrun);
    }

    #[test]
    fn startup_waterline_joins_delayed_pending_audio_with_adjacent_demux() {
        let target_nsecs = 184_700_000_000;
        let pending_audio_end_nsecs = 185_573_739_000;
        let raw = DemuxReaderWatermark {
            video_forward_nsecs: Some(2_500_000_000),
            audio_forward_nsecs: Some(1_700_000_000),
            selected_min_forward_nsecs: Some(1_700_000_000),
            audio_underrun: true,
            underrun: true,
            ..DemuxReaderWatermark::default()
        };

        let combined = demux_watermark_with_initial_combined_coverage(
            raw,
            target_nsecs,
            Some(187_200_000_000),
            Some(187_200_000_000),
            Some((184_714_739_000, pending_audio_end_nsecs)),
            Some(pending_audio_end_nsecs),
            true,
        );

        assert_eq!(combined.audio_forward_nsecs, Some(2_573_739_000));
        assert_eq!(combined.selected_min_forward_nsecs, Some(2_573_739_000));
        assert!(!combined.audio_underrun);
        assert!(!combined.underrun);
    }

    #[test]
    fn startup_waterline_does_not_sum_disconnected_audio_ranges() {
        let target_nsecs = 184_700_000_000;
        let pending_audio_end_nsecs = 185_573_739_000;
        let raw = DemuxReaderWatermark {
            video_forward_nsecs: Some(2_500_000_000),
            audio_forward_nsecs: Some(2_500_000_000),
            audio_underrun: true,
            underrun: true,
            ..DemuxReaderWatermark::default()
        };

        let combined = demux_watermark_with_initial_combined_coverage(
            raw,
            target_nsecs,
            Some(187_200_000_000),
            Some(187_200_000_000),
            Some((184_714_739_000, pending_audio_end_nsecs)),
            Some(190_000_000_000),
            true,
        );

        assert_eq!(combined.audio_forward_nsecs, Some(873_739_000));
        assert_eq!(combined.selected_min_forward_nsecs, Some(873_739_000));
        assert!(!combined.audio_underrun);
        assert!(!combined.underrun);
    }
}
