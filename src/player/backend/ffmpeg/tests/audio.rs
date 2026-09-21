use super::*;

#[test]
fn seeking_inside_time_stretched_audio_trims_pcm_in_media_time() {
    for (samples, remaining) in [(1920, 960), (480, 240)] {
        let mut pending = PendingStartAudio::default();
        pending.push(
            DecodedAudio {
                samples: vec![0.25; samples],
                duration_nsecs: 20_000_000,
            },
            1_000_000_000,
            1_020_000_000,
        );
        let mut frame = pending.pop_front_until(1_020_000_000).unwrap();
        assert!(frame.trim_before(1_010_000_000, 48_000, 2));
        assert_eq!(frame.samples.len(), remaining);
        assert_eq!(frame.start_timeline_nsecs, 1_010_000_000);
        assert_eq!(frame.end_timeline_nsecs, 1_020_000_000);
    }
}

#[test]
fn pending_start_audio_discards_frames_before_first_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        900_000_000,
        920_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );

    assert_eq!(pending.discard_before(1_000_000_000), 1);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.queued_samples(), 4);
}

#[test]
fn pending_start_audio_keeps_frame_overlapping_playback_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 32_000_000,
        },
        576_000_000,
        608_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 32_000_000,
        },
        608_000_000,
        640_000_000,
    );

    assert_eq!(pending.discard_before(576_018_171), 0);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.first_start_timeline_nsecs(), Some(576_000_000));
    assert_eq!(pending.forward_duration_from(576_018_171), Some(63_981_829));
}

#[test]
fn pending_start_audio_trims_overlapping_frame_to_playback_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 64],
            duration_nsecs: 32_000_000,
        },
        576_000_000,
        608_000_000,
    );

    let mut frame = pending
        .pop_front_until(608_000_000)
        .expect("covered frame pops");
    assert!(frame.trim_before(592_000_000, 1_000, 2));

    assert_eq!(frame.start_timeline_nsecs, 592_000_000);
    assert_eq!(frame.end_timeline_nsecs, 608_000_000);
    assert_eq!(frame.samples.len(), 32);
}

#[test]
fn pending_start_audio_reports_first_start_timeline() {
    let mut pending = PendingStartAudio::default();
    assert_eq!(pending.first_start_timeline_nsecs(), None);

    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );

    assert_eq!(pending.first_start_timeline_nsecs(), Some(1_000_000_000));
}

#[test]
fn pending_start_audio_reports_contiguous_forward_duration() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_040_000_000,
        1_060_000_000,
    );

    assert_eq!(
        pending.forward_duration_from(1_000_000_000),
        Some(20_000_000)
    );
    assert_eq!(pending.forward_duration_from(1_020_000_000), None);
}

#[test]
fn pending_start_audio_tolerates_small_timestamp_gaps() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_021_000_000,
        1_041_000_000,
    );

    assert_eq!(
        pending.forward_duration_from(1_000_000_000),
        Some(41_000_000)
    );
}

#[test]
fn pending_start_audio_pops_only_frames_covered_by_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_000_000_000,
        1_020_000_000,
    );
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 20_000_000,
        },
        1_020_000_000,
        1_040_000_000,
    );

    assert!(pending.pop_front_until(1_019_999_999).is_none());
    assert_eq!(pending.len(), 2);
    assert!(pending.pop_front_until(1_020_000_000).is_some());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.first_start_timeline_nsecs(), Some(1_020_000_000));
}

#[test]
fn pending_audio_underrun_recovery_waits_for_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(&pending, 1_000_000_000, 0, 1_000_000_000, None, None),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_resets_to_next_audio_with_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            1_000_000_000,
            Some(1_400_000_000),
            Some(1_900_000_000)
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_400_000_000,
            audio_flush_until_timeline_nsecs: 1_900_000_000,
            reset_audio_to_timeline_nsecs: Some(1_400_000_000),
        })
    );
}

#[test]
fn pending_audio_underrun_recovery_waits_for_existing_audio_before_clock_reset() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_400_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            50_000_000,
            1_050_000_000,
            Some(1_400_000_000),
            Some(1_900_000_000)
        ),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_uses_video_lead_when_available() {
    let mut pending = PendingStartAudio::default();
    for index in 0..9 {
        let start = 1_000_000_000 + index * 100_000_000;
        pending.push(
            DecodedAudio {
                samples: vec![0.0; 4],
                duration_nsecs: 100_000_000,
            },
            start,
            start + 100_000_000,
        );
    }

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            1_000_000_000,
            Some(1_000_000_000),
            Some(1_300_000_000),
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_000_000_000,
            audio_flush_until_timeline_nsecs: 1_800_000_000,
            reset_audio_to_timeline_nsecs: None,
        })
    );
}

#[test]
fn pending_audio_underrun_prefill_counts_payload_instead_of_pts_span() {
    const PLAYED: u64 = 1_301_797_375_331;
    const PREFIX_NSECS: u64 = 149_319_724;
    const AUDIO_END: u64 = PLAYED + PREFIX_NSECS;
    const NEXT_AUDIO: u64 = 1_301_997_062_500;
    for (tail_nsecs, ready) in [(100_000_000, false), (120_000_000, true)] {
        let mut pending = PendingStartAudio::default();
        pending.push(
            DecodedAudio {
                samples: vec![0.25; 4],
                duration_nsecs: tail_nsecs,
            },
            NEXT_AUDIO,
            NEXT_AUDIO + tail_nsecs,
        );
        let plan = pending_audio_underrun_recovery_plan(
            &pending,
            PLAYED,
            PREFIX_NSECS,
            AUDIO_END,
            Some(PLAYED + 40_000_000),
            Some(PLAYED + 1_000_000_000),
        );
        assert_eq!(plan.is_some(), ready);
        if let Some(plan) = plan {
            assert_eq!(plan.audio_start_timeline_nsecs, AUDIO_END);
            assert_eq!(plan.reset_audio_to_timeline_nsecs, None);
        }
    }
}

#[test]
fn pending_audio_underrun_prefill_waits_for_a_frame_to_fit_the_video_limit() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.25; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );
    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            1_000_000_000,
            Some(1_000_000_000),
            Some(1_300_000_000),
        ),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_resets_to_video_start_when_audio_leads_video() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            1_000_000_000,
            Some(1_400_000_000),
            Some(1_900_000_000),
        ),
        Some(PendingAudioUnderrunRecoveryPlan {
            audio_start_timeline_nsecs: 1_400_000_000,
            audio_flush_until_timeline_nsecs: 1_900_000_000,
            reset_audio_to_timeline_nsecs: Some(1_400_000_000),
        })
    );
}

#[test]
fn pending_audio_underrun_recovery_waits_for_actual_video_window() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 900_000_000,
        },
        1_000_000_000,
        1_900_000_000,
    );

    assert_eq!(
        pending_audio_underrun_recovery_plan(
            &pending,
            1_000_000_000,
            0,
            1_000_000_000,
            Some(1_000_000_000),
            Some(1_040_000_000),
        ),
        None
    );
}

#[test]
fn pending_audio_underrun_recovery_discards_stale_pending_audio_before_video_start() {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs: 500_000_000,
        },
        1_000_000_000,
        1_500_000_000,
    );

    assert_eq!(
        discard_stale_pending_audio_before_recovery_start(
            &mut pending,
            1_800_000_000,
            0,
            Some(2_000_000_000)
        ),
        1
    );
    assert!(pending.is_empty());
}

#[test]
fn audio_sample_len_rejects_invalid_sizes() {
    assert!(audio_sample_len(-1, FALLBACK_AUDIO_OUTPUT_CHANNELS).is_err());
    assert!(audio_sample_len(1024, 0).is_err());
    assert_eq!(
        audio_sample_len(1024, FALLBACK_AUDIO_OUTPUT_CHANNELS).unwrap(),
        1024 * FALLBACK_AUDIO_OUTPUT_CHANNELS as usize
    );
}

#[test]
fn dovi_packet_timeline_uses_stream_start_when_available() {
    let time_base = ffi::AVRational { num: 1, den: 1_000 };
    let mut first_packet_nsecs = None;

    assert_eq!(
        dovi_packet_timeline_nsecs(
            &mut first_packet_nsecs,
            Some(1_000_000_000),
            1_250,
            time_base,
        ),
        Some(250_000_000)
    );
    assert_eq!(first_packet_nsecs, None);
}
