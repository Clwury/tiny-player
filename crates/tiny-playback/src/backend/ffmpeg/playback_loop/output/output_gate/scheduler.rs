#[cfg(test)]
use super::DecodedAudio;
use super::RebufferResumeAnchor;
use super::{
    AUDIO_OUTPUT_ACTIVITY_RECOVERY_AFTER, AUDIO_OUTPUT_ACTIVITY_STALL_AFTER,
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_REBUFFER_LOOP_DETECTION_WINDOW, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, Arc, AtomicBool,
    AudioClockHandle, AudioContinuityRejectionSummary, AudioOutput, AudioOutputActivitySnapshot,
    AudioOutputActivityWatchdog, AudioOutputActivityWatchdogAction,
    AudioOutputActivityWatchdogEvent, AudioOutputLifecycle, AudioOutputSnapshot,
    AudioReaderGapWatchdog, AudioRealignCoverage, AudioResumeWaterline, AudioSyncDropLogSummary,
    BackendEvent, DECODE_RECOVERY_AUDIO_READY_HYSTERESIS_NSECS,
    DECODE_RECOVERY_DECODER_IN_FLIGHT_ALLOWANCE, DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
    DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS, DECODE_RECOVERY_MAX_WALL_TIME,
    DECODE_RECOVERY_STAGING_NSECS, DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS,
    DecodeRecoveryDisposition, DecodeRecoveryDropForFallback, DecodeRecoveryGapProvenance,
    DecodeRecoveryPhase, DecodeRecoverySource, DecodeRecoveryTransaction, Duration, FfmpegControl,
    INITIAL_AUDIO_DEFER_LOG_SUMMARY_INTERVAL, INITIAL_AUDIO_START_RETRY_INTERVAL,
    INITIAL_AV_START_HARD_TIMEOUT, INITIAL_SYNC_LOG_SUMMARY_INTERVAL, InitialAudioDeferLogState,
    InitialAudioDeferObservation, InitialAudioPreparePhase, InitialAudioPrepareToken,
    InitialAudioTransientRetry, InitialAvStartTransaction, InitialSyncLogDecision,
    InitialSyncLogObservation, InitialSyncLogState, Instant, OUTPUT_GATE_PERIODIC_PROBE_INTERVAL,
    OutputGateBlockLogEmission, OutputGateBlockLogState, OutputServiceDemand,
    PENDING_AUDIO_CONTINUITY_TOLERANCE, PendingAudioRetentionAnchorSource,
    PendingAudioRetentionPlan, PendingStartAudio, PendingStartAudioPressureLevel,
    PlaybackBlockReason, PlaybackOutputScheduler, PlaybackOutputSnapshot, PlaybackOutputState,
    PlaybackResumeWaterline, PlaybackScheduler, PlaybackSessionId, PrestartAudioOwnership,
    PrestartAudioOwnershipLogState, QueuedVideoFrame, RebufferAudioRealignRequest,
    ScheduledVideoQueue, Sender, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER,
    VideoDeadlineService, VideoOutputQueue, VideoOutputUnderflowClassification,
    clear_video_output_rebuffer, decode_recovery_gap_within_limit, duration_nsecs,
    enter_video_output_rebuffer, finish_video_output_rebuffer_if_ready,
    video_output_rebuffer_should_enter, video_output_underflow_classification,
};
use ffmpeg_sys_next as ffi;

const REBUFFER_EMPTY_AUDIO_OUTPUT_WAKE_INTERVAL: Duration = Duration::from_millis(100);
const REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_OBSERVATIONS: u8 = 3;
const AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR: Duration = Duration::from_secs(2);
const DECODE_RECOVERY_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
const AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME: Duration = Duration::from_secs(2);
const AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(1);
const AUDIO_SYNC_DROP_LOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

fn decode_recovery_staging_frame_budget(
    frame_duration_nsecs: u64,
    vo_queue_capacity: usize,
) -> usize {
    let target_frames = DECODE_RECOVERY_STAGING_NSECS
        .checked_div(frame_duration_nsecs)
        .map(|whole_frames| {
            whole_frames.saturating_add(u64::from(
                !DECODE_RECOVERY_STAGING_NSECS.is_multiple_of(frame_duration_nsecs),
            ))
        })
        .and_then(|frames| usize::try_from(frames).ok())
        .unwrap_or(1)
        .max(1);
    target_frames
        .saturating_add(DECODE_RECOVERY_DECODER_IN_FLIGHT_ALLOWANCE)
        .saturating_add(vo_queue_capacity)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioReaderGapWatchdogDecision {
    Covered,
    InputPending,
    Waiting,
    Request,
    RequestAlreadyIssued,
}

#[derive(Clone, Copy)]
struct AudioReaderGapWatchdogObservation {
    target_timeline_nsecs: u64,
    progress_nsecs: u64,
    has_resume_coverage: bool,
    input_can_fill_gap: bool,
    force_immediate_realign: bool,
    now: Instant,
}

#[cfg(test)]
mod audio_gap_watchdog_tests {
    use super::{
        AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME, AudioReaderGapWatchdogDecision, Instant,
        observe_audio_reader_gap_watchdog,
    };

    #[test]
    fn forced_audio_realign_bypasses_gap_watchdog_delay() {
        let mut watchdog = None;
        assert_eq!(
            observe_audio_reader_gap_watchdog(
                &mut watchdog,
                super::AudioReaderGapWatchdogObservation {
                    target_timeline_nsecs: 1_000_000_000,
                    progress_nsecs: 0,
                    has_resume_coverage: true,
                    input_can_fill_gap: true,
                    force_immediate_realign: true,
                    now: Instant::now(),
                },
            ),
            AudioReaderGapWatchdogDecision::Request
        );
    }

    #[test]
    fn repeated_reader_polls_do_not_expire_watchdog_or_reset_wall_time_bound() {
        let mut watchdog = None;
        let now = Instant::now();
        for observation in 0..10_000 {
            assert_eq!(
                observe_audio_reader_gap_watchdog(
                    &mut watchdog,
                    super::AudioReaderGapWatchdogObservation {
                        target_timeline_nsecs: 1_000_000_000 + observation,
                        progress_nsecs: 0,
                        has_resume_coverage: false,
                        input_can_fill_gap: true,
                        force_immediate_realign: false,
                        now,
                    },
                ),
                AudioReaderGapWatchdogDecision::InputPending
            );
        }
        assert_eq!(
            observe_audio_reader_gap_watchdog(
                &mut watchdog,
                super::AudioReaderGapWatchdogObservation {
                    target_timeline_nsecs: 2_000_000_000,
                    progress_nsecs: 0,
                    has_resume_coverage: false,
                    input_can_fill_gap: true,
                    force_immediate_realign: false,
                    now: now + AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME,
                },
            ),
            AudioReaderGapWatchdogDecision::Request
        );
    }
}

fn observe_audio_reader_gap_watchdog(
    watchdog: &mut Option<AudioReaderGapWatchdog>,
    observation: AudioReaderGapWatchdogObservation,
) -> AudioReaderGapWatchdogDecision {
    let AudioReaderGapWatchdogObservation {
        target_timeline_nsecs,
        progress_nsecs,
        has_resume_coverage,
        input_can_fill_gap,
        force_immediate_realign,
        now,
    } = observation;
    if has_resume_coverage && !force_immediate_realign {
        *watchdog = None;
        return AudioReaderGapWatchdogDecision::Covered;
    }
    let current = watchdog.get_or_insert(AudioReaderGapWatchdog {
        target_timeline_nsecs,
        started_at: now,
        last_progress_nsecs: progress_nsecs,
        last_progress_at: now,
        request_issued: false,
    });
    if current.target_timeline_nsecs != target_timeline_nsecs {
        // A moving rebuffer target must not restart the absolute watchdog.
        current.target_timeline_nsecs = target_timeline_nsecs;
        current.last_progress_nsecs = progress_nsecs;
    } else if progress_nsecs > current.last_progress_nsecs {
        current.last_progress_nsecs = progress_nsecs;
        current.last_progress_at = now;
        current.request_issued = false;
    }
    if current.request_issued {
        return AudioReaderGapWatchdogDecision::RequestAlreadyIssued;
    }
    let absolute_bound_exhausted = now.saturating_duration_since(current.started_at)
        >= AUDIO_READER_GAP_WATCHDOG_MAX_WALL_TIME;
    if input_can_fill_gap && !force_immediate_realign && !absolute_bound_exhausted {
        return AudioReaderGapWatchdogDecision::InputPending;
    }
    if !force_immediate_realign
        && !absolute_bound_exhausted
        && now.saturating_duration_since(current.last_progress_at)
            < VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER
    {
        return AudioReaderGapWatchdogDecision::Waiting;
    }
    current.request_issued = true;
    AudioReaderGapWatchdogDecision::Request
}

struct AudioGapRecoveryRebufferSuppressionInput {
    now: Instant,
    queued_video_forward_nsecs: Option<u64>,
    audio_output_pending_nsecs: Option<u64>,
    demux_min_forward_nsecs: Option<u64>,
    render_backlogged: bool,
    vo_queued_frames: usize,
    session_id: PlaybackSessionId,
}

#[path = "scheduler/audio_realign.rs"]
mod audio_realign;
#[path = "scheduler/core.rs"]
mod core;
#[path = "scheduler/decode_recovery.rs"]
mod decode_recovery;
#[path = "scheduler/initial_start.rs"]
mod initial_start;
#[path = "scheduler/rebuffer.rs"]
mod rebuffer;
#[path = "scheduler/scheduling.rs"]
mod scheduling;

#[cfg(test)]
mod decode_recovery_tests {
    use crate::render_host::{DecodedFrame, FramePixels, FramePts, PlaybackSessionId, RenderSize};

    use super::{
        AudioOutputSnapshot, DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS, DECODE_RECOVERY_MAX_WALL_TIME,
        DecodeRecoveryDisposition, DecodeRecoveryPhase, DecodeRecoverySource, Duration,
        FfmpegControl, Instant, OUTPUT_GATE_PERIODIC_PROBE_INTERVAL, OutputServiceDemand,
        PlaybackOutputScheduler, PlaybackOutputState, PlaybackScheduler, QueuedVideoFrame,
        decode_recovery_staging_frame_budget,
    };

    fn frame(timeline_nsecs: u64, duration_nsecs: u64, old_hardware: bool) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: old_hardware,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            duration_nsecs,
            source_duration_nsecs: duration_nsecs,
        }
    }

    #[test]
    fn decode_recovery_discards_future_hw_frames_and_atomically_splices_software() {
        let session_id = PlaybackSessionId(1);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        for timeline_nsecs in [920_000_000, 960_000_000, 1_000_000_000, 1_040_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }

        output.begin_decode_recovery(
            7,
            1_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(7);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Replaying)
        );
        assert!(output.scheduled_video_queue.with_frames(|frames| {
            frames
                .iter()
                .all(|queued| queued.timeline_nsecs < 1_000_000_000)
        }));

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(900_000_000);
        let mut current_start_position_nsecs = 900_000_000;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Committed)
        );
        assert!(!output.decode_recovery_owns_video_admission());
        assert!(!output.decode_recovery_video_admission_blocked());
        assert_eq!(output.scheduled_video_queue.len(), 15);
        assert!(output.scheduled_video_queue.with_frames(|frames| {
            frames
                .iter()
                .filter(|queued| queued.timeline_nsecs >= 1_000_000_000)
                .all(|queued| !queued.frame.key_frame)
        }));
        assert_eq!(
            output.scheduled_video_queue.range_nsecs(),
            Some((920_000_000, 1_520_000_000))
        );
    }

    #[test]
    fn decode_recovery_barrier_pauses_at_target_until_staging_is_buffered() {
        let session_id = PlaybackSessionId(2);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            9,
            2_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(9);

        assert!(!output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            1_999_999_999,
            &control,
            session_id,
        ));
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            2_000_000_000,
            &control,
            session_id,
        ));
        assert!(control.is_output_rebuffer_paused());
        assert_eq!(
            output.playback_output_state,
            PlaybackOutputState::Rebuffering
        );

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(2_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        let mut scheduler = PlaybackScheduler::new(2_000_000_000);
        let mut current_start_position_nsecs = 2_000_000_000;
        assert!(!output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert!(control.is_output_rebuffer_paused());
        assert_eq!(
            output.playback_output_state,
            PlaybackOutputState::Rebuffering
        );
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert!(!control.is_output_rebuffer_paused());
        assert_eq!(output.playback_output_state, PlaybackOutputState::Playing);
    }

    #[test]
    fn decode_recovery_staging_does_not_advance_committed_progress_watermark() {
        let session_id = PlaybackSessionId(8);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            15,
            2_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(15);
        assert_eq!(output.admitted_video_queue_end_nsecs(), None);

        assert!(output.stage_decode_recovery_frame(
            frame(2_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));
        assert!(output.scheduled_video_queue.is_empty());
        assert_eq!(output.admitted_video_queue_end_nsecs(), None);
        assert_eq!(
            output.recovery_staged_high_water_nsecs(),
            Some(2_040_000_000)
        );
    }

    #[test]
    fn decode_recovery_buffered_phase_stops_input_and_caps_in_flight_staging() {
        let session_id = PlaybackSessionId(81);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            81,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(81);

        let frame_duration_nsecs = 33_333_333;
        let budget = decode_recovery_staging_frame_budget(frame_duration_nsecs, 3);
        assert_eq!(budget, 23);
        for index in 0..64_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(
                    1_000_000_000 + index * frame_duration_nsecs,
                    frame_duration_nsecs,
                    false,
                ),
                3,
                session_id,
            ));
            if output.decode_recovery_phase() == Some(DecodeRecoveryPhase::Buffered) {
                assert!(output.decode_recovery_video_admission_blocked());
            }
        }
        assert!(output.decode_recovery_video_admission_blocked());
        assert_eq!(output.recovery_staging_frame_budget(), Some(budget));
        assert_eq!(output.recovery_staging_frames(), budget);
        assert!(output.recovery_staging_frames() <= 24);
    }

    #[test]
    fn decode_recovery_audio_ready_latches_across_500_to_452ms_drop() {
        let session_id = PlaybackSessionId(82);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.begin_decode_recovery(
            82,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        let snapshot = |pending_nsecs| AudioOutputSnapshot {
            played_timeline_nsecs: 1_000_000_000,
            buffered_until_timeline_nsecs: 1_000_000_000 + pending_nsecs,
            shared_pending_nsecs: pending_nsecs,
            queue_pending_nsecs: 0,
            total_pending_nsecs: pending_nsecs,
            queue_frames: 0,
            queue_generation: 1,
            ..AudioOutputSnapshot::default()
        };

        assert!(output.update_decode_recovery_audio_ready(true, Some(snapshot(500_000_000)), 0,));
        assert!(output.update_decode_recovery_audio_ready(true, Some(snapshot(452_000_000)), 0,));
        assert!(output.decode_recovery_audio_ready_latched());

        let mut aggregate = PlaybackOutputScheduler::new();
        aggregate.begin_decode_recovery(
            83,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        assert!(aggregate.update_decode_recovery_audio_ready(
            true,
            Some(snapshot(452_000_000)),
            882_000_000,
        ));
    }

    #[test]
    fn coordinator_tick_can_commit_buffered_recovery_without_another_video_frame() {
        let session_id = PlaybackSessionId(84);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            84,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(84);
        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        let staged_frames_before_tick = output.recovery_staging_frames();
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(1_000_000_000);
        let mut current_start_position_nsecs = 1_000_000_000;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(staged_frames_before_tick, 13);
        assert_eq!(output.recovery_staging_frames(), 0);
        assert_eq!(output.scheduled_video_queue.len(), 13);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Committed)
        );
    }

    #[test]
    fn resource_pressure_releases_all_recovery_staging_references() {
        let session_id = PlaybackSessionId(85);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        for timeline_nsecs in [880_000_000, 920_000_000, 960_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }
        output.begin_decode_recovery(
            85,
            1_000_000_000,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(85);
        for index in 0..7_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(1_000_000_000 + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }

        assert_eq!(
            output.release_vulkan_frames_for_resource_pressure(&control, session_id),
            10
        );
        assert!(output.scheduled_video_queue.is_empty());
        assert_eq!(output.recovery_staging_frames(), 0);
        assert_eq!(output.recovery_staged_high_water_nsecs(), None);
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Failed)
        );
        assert!(control.is_output_rebuffer_paused());
    }

    #[test]
    fn recovery_snapshot_reports_committed_and_staged_watermarks_separately() {
        let session_id = PlaybackSessionId(86);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output
            .scheduled_video_queue
            .push_queued(frame(960_000_000, 40_000_000, true));
        output.begin_decode_recovery(
            86,
            1_000_000_000,
            DecodeRecoverySource::FlushReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(86);
        assert!(output.stage_decode_recovery_frame(
            frame(1_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));

        let snapshot = output.snapshot();
        assert_eq!(snapshot.queued_video_frames, 1);
        assert_eq!(snapshot.recovery_staging_frames, 1);
        assert_eq!(snapshot.recovery_staging_frame_budget, Some(20));
        assert_eq!(
            snapshot.committed_output_high_water_nsecs,
            Some(1_000_000_000)
        );
        assert_eq!(
            snapshot.recovery_staged_high_water_nsecs,
            Some(1_040_000_000)
        );
        assert!(!snapshot.decode_recovery_audio_ready_latched);
    }

    #[test]
    fn decode_recovery_commits_log_derived_233ms_and_1_9s_gaps() {
        let session_id = PlaybackSessionId(3);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 1_036_766_666_666;
        let first_frame_nsecs = 1_037_000_000_000;
        let second_frame_nsecs = 1_038_933_333_333;
        let retained_frame_nsecs = target_nsecs - 33_333_333;
        output
            .scheduled_video_queue
            .push_queued(frame(retained_frame_nsecs, 33_333_333, true));
        output.begin_decode_recovery(
            11,
            target_nsecs,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(11);

        assert!(output.stage_decode_recovery_frame(
            frame(first_frame_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Replaying)
        );
        assert!(output.stage_decode_recovery_frame(
            frame(second_frame_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::HoldGap);
        assert_eq!(
            transaction.first_staged_frame_nsecs,
            Some(first_frame_nsecs)
        );
        assert_eq!(transaction.bridged_gap_count, 2);
        assert_eq!(
            transaction
                .staging_queue
                .with_frames(|frames| frames.front().expect("first staged frame").duration_nsecs),
            second_frame_nsecs - first_frame_nsecs
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::CommittedGap)
        );
        assert!(output.decode_recovery_output_committed());
        assert_eq!(current_start_position_nsecs, target_nsecs);
        let retained_frame_timing = output.scheduled_video_queue.with_frames(|frames| {
            let retained = frames.front().expect("retained visible frame");
            (retained.timeline_nsecs, retained.duration_nsecs)
        });
        assert_eq!(retained_frame_timing.0, retained_frame_nsecs);
        assert_eq!(
            retained_frame_timing.1,
            first_frame_nsecs - retained_frame_nsecs
        );
        assert_eq!(
            output
                .scheduled_video_queue
                .with_frames(|frames| frames.get(1).expect("committed first frame").timeline_nsecs),
            first_frame_nsecs
        );
    }

    #[test]
    fn decode_recovery_accepts_log_derived_mux_rounded_five_second_gap() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 367_466_645_832;
        let first_frame_nsecs = 372_466_687_500;
        output.begin_decode_recovery(
            5,
            target_nsecs,
            DecodeRecoverySource::CachedSafeIdrRebuild,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(5);

        assert_eq!(first_frame_nsecs - target_nsecs, 5_000_041_668);
        assert!(output.stage_decode_recovery_frame(
            frame(first_frame_nsecs, 33_333_332, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::HoldGap);
        assert_eq!(
            transaction.first_staged_frame_nsecs,
            Some(first_frame_nsecs)
        );
        assert_eq!(transaction.staging_queue.len(), 1);
        assert!(transaction.drop_for_fallback.is_none());

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::CommittedGap)
        );
        assert_eq!(current_start_position_nsecs, target_nsecs);
    }

    #[test]
    fn decode_recovery_reanchors_large_discontinuity_without_unbounded_replay() {
        let session_id = PlaybackSessionId(4);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 3_000_000_000;
        let resume_nsecs = 9_100_000_000;
        output.begin_decode_recovery(
            12,
            target_nsecs,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.confirm_decode_recovery_synchronized_timeline_gap();
        output.mark_decode_recovery_replaying(12);

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            false,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
        assert!(scheduler.current_timeline_nsecs() >= resume_nsecs);
        assert_eq!(
            output.audio_sync_drop_before_timeline_nsecs(),
            Some(resume_nsecs)
        );
    }

    #[test]
    fn decoder_error_reanchor_waits_for_last_good_frame_boundary() {
        let session_id = PlaybackSessionId(9);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 797_366_645_832;
        let resume_nsecs = 799_500_000_000;
        for timeline_nsecs in [target_nsecs - 80_000_000, target_nsecs - 40_000_000] {
            output
                .scheduled_video_queue
                .push_queued(frame(timeline_nsecs, 40_000_000, true));
        }
        output.begin_decode_recovery(
            10_591,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(10_591);

        for index in 0..13_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 40_000_000, 40_000_000, false),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .disposition,
            DecodeRecoveryDisposition::Reanchor
        );

        let mut scheduler = PlaybackScheduler::new(target_nsecs - 500_000_000);
        let mut current_start_position_nsecs = target_nsecs - 500_000_000;
        assert!(
            !output.commit_decode_recovery_if_buffered(
                &control,
                &mut scheduler,
                None,
                &mut current_start_position_nsecs,
                true,
                session_id,
            ),
            "the buffered IDR must not discard good queued frames early"
        );
        assert!(!output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs - 1,
            &control,
            session_id,
        ));
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs,
            &control,
            session_id,
        ));
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
        assert_eq!(
            output.scheduled_video_queue.range_nsecs(),
            Some((resume_nsecs, resume_nsecs + 13 * 40_000_000))
        );
        assert_eq!(
            output.audio_sync_drop_before_timeline_nsecs(),
            Some(resume_nsecs)
        );
    }

    #[test]
    fn decoder_error_reanchor_commits_when_audio_underruns_one_frame_before_boundary() {
        let session_id = PlaybackSessionId(9);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);

        // Exact values from the 14:41 failure: the retained queue ended at
        // 881.300s, but native audio exhausted its last real samples roughly
        // one frame earlier and could no longer advance the master clock.
        let target_nsecs = 881_300_020_832;
        let underrun_clock_nsecs = 881_266_458_916;
        let resume_nsecs = 884_500_000_000;
        output.scheduled_video_queue.push_queued(frame(
            target_nsecs - 33_333_332,
            33_333_332,
            true,
        ));
        let before_recovery = Instant::now();
        output.mark_output_housekeeping_serviced_at(before_recovery);
        output.begin_decode_recovery(
            6_343,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        assert_eq!(
            output.output_service_demand(before_recovery),
            OutputServiceDemand::OutputStateChanged,
            "starting recovery must wake output independently of decoder input"
        );
        output.mark_output_housekeeping_serviced_at(before_recovery);
        output.mark_decode_recovery_replaying(6_343);
        for index in 0..16_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 33_333_332, 33_333_332, false,),
                3,
                session_id,
            ));
        }
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(
            output.output_service_demand(Instant::now()),
            OutputServiceDemand::OutputStateChanged,
            "buffering the recovery transaction must publish another output wakeup"
        );

        let serviced_at = Instant::now();
        output.mark_output_housekeeping_serviced_at(serviced_at);
        assert_eq!(
            output.output_service_demand(
                serviced_at + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL - Duration::from_nanos(1)
            ),
            OutputServiceDemand::None
        );
        assert_eq!(
            output.output_service_demand(serviced_at + OUTPUT_GATE_PERIODIC_PROBE_INTERVAL),
            OutputServiceDemand::DecodeRecovery,
            "an active recovery must keep waking output even if decoder input is blocked"
        );
        assert!(!output.maybe_enter_decode_recovery_barrier(
            serviced_at,
            underrun_clock_nsecs,
            &control,
            session_id,
        ));

        // Once the retained frame has been handed to VO, waiting for the audio
        // clock to reach its end timestamp is impossible during underrun.
        output.scheduled_video_queue.clear();
        assert_eq!(
            output.output_service_demand(serviced_at),
            OutputServiceDemand::DecodeRecovery
        );
        assert!(
            output
                .output_housekeeping_deadline()
                .is_some_and(|deadline| deadline <= Instant::now()),
            "a drained recovery boundary must make output housekeeping immediately due"
        );
        output
            .decode_recovery_transaction
            .as_mut()
            .expect("transaction")
            .started_at = Instant::now()
            .checked_sub(DECODE_RECOVERY_MAX_WALL_TIME + Duration::from_millis(1))
            .expect("deadline start");
        assert!(
            output
                .check_decode_recovery_deadline(Instant::now(), &control, session_id)
                .is_ok(),
            "a drained commit-ready boundary must win a race with the wall deadline"
        );
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            underrun_clock_nsecs,
            &control,
            session_id,
        ));

        let mut scheduler = PlaybackScheduler::new(underrun_clock_nsecs);
        let mut current_start_position_nsecs = underrun_clock_nsecs;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
    }

    #[test]
    fn decoder_error_reanchor_does_not_count_log_derived_skipped_gap_as_replay() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);

        let target_nsecs = 955_566_645_832;
        let resume_nsecs = 961_300_000_000;
        output.scheduled_video_queue.push_queued(frame(
            target_nsecs - 33_333_332,
            33_333_332,
            true,
        ));
        output.begin_decode_recovery(
            6_082,
            target_nsecs,
            DecodeRecoverySource::DecoderError,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(6_082);

        assert_eq!(resume_nsecs - target_nsecs, 5_733_354_168);
        for index in 0..16_u64 {
            assert!(output.stage_decode_recovery_frame(
                frame(resume_nsecs + index * 33_333_332, 33_333_332, false,),
                3,
                session_id,
            ));
        }
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.phase, DecodeRecoveryPhase::Buffered);
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::Reanchor);
        assert_eq!(transaction.resume_nsecs, resume_nsecs);
        assert!(transaction.drop_for_fallback.is_none());
        assert_eq!(transaction.staging_queue.len(), 16);

        output.scheduled_video_queue.clear();
        assert!(output.maybe_enter_decode_recovery_barrier(
            Instant::now(),
            target_nsecs - 33_333_332,
            &control,
            session_id,
        ));
        let mut scheduler = PlaybackScheduler::new(target_nsecs - 33_333_332);
        let mut current_start_position_nsecs = target_nsecs - 33_333_332;
        assert!(output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Reanchored)
        );
        assert_eq!(current_start_position_nsecs, resume_nsecs);
    }

    #[test]
    fn decode_recovery_withholds_large_continuous_gap_for_bounded_fallback() {
        let session_id = PlaybackSessionId(6);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        let target_nsecs = 681_266_667_000;
        let resume_nsecs = target_nsecs + 9_000_000_000;
        output.begin_decode_recovery(
            14,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_output_housekeeping_serviced_at(Instant::now());
        output.mark_decode_recovery_replaying(14);

        assert!(output.stage_decode_recovery_frame(
            frame(resume_nsecs, 33_333_333, false),
            3,
            session_id,
        ));
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::DroppedForFallback)
        );
        assert_eq!(
            output.output_service_demand(Instant::now()),
            OutputServiceDemand::OutputStateChanged,
            "terminal fallback must wake output while decoder admission is blocked"
        );
        assert!(!output.decode_recovery_output_committed());
        assert!(output.decode_recovery_owns_video_admission());
        assert!(output.decode_recovery_video_admission_blocked());
        assert!(output.scheduled_video_queue.is_empty());
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
        assert_eq!(output.audio_sync_drop_before_timeline_nsecs(), None);

        let drop = output
            .take_decode_recovery_drop_for_fallback()
            .expect("bounded fallback request");
        assert_eq!(drop.transaction_id, 14);
        assert_eq!(drop.target_nsecs, target_nsecs);
        assert_eq!(drop.first_frame_nsecs, resume_nsecs);
        assert_eq!(drop.gap_nsecs, 9_000_000_000);
        assert_eq!(drop.source, DecodeRecoverySource::VulkanReopenReplay);
        assert!(
            !output.stage_decode_recovery_frame(
                frame(resume_nsecs + 33_333_333, 33_333_333, false),
                3,
                session_id,
            ),
            "frames arriving before coordinator fallback must remain withheld"
        );
        assert!(output.scheduled_video_queue.is_empty());

        let mut scheduler = PlaybackScheduler::new(target_nsecs);
        let mut current_start_position_nsecs = target_nsecs;
        assert!(!output.commit_decode_recovery_if_buffered(
            &control,
            &mut scheduler,
            None,
            &mut current_start_position_nsecs,
            true,
            session_id,
        ));
        assert_eq!(current_start_position_nsecs, target_nsecs);
        assert_eq!(output.audio_sync_drop_before_timeline_nsecs(), None);
    }

    #[test]
    fn replay_span_limit_makes_a_terminal_decision_and_releases_staging() {
        let session_id = PlaybackSessionId(61);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            61,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(61);
        assert!(output.stage_decode_recovery_frame(
            frame(target_nsecs, 40_000_000, false),
            3,
            session_id,
        ));
        assert!(output.stage_decode_recovery_frame(
            frame(
                target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS,
                40_000_000,
                false,
            ),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::DroppedForFallback)
        );
        assert_eq!(output.recovery_staging_frames(), 0);
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
        assert!(output.take_decode_recovery_drop_for_fallback().is_some());
    }

    #[test]
    fn first_hold_gap_frame_end_does_not_count_as_unbounded_replay() {
        let session_id = PlaybackSessionId(63);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            63,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(63);
        assert!(output.stage_decode_recovery_frame(
            frame(
                target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS - 20_000_000,
                40_000_000,
                false,
            ),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        assert_eq!(output.recovery_staging_frames(), 1);
        assert!(output.take_decode_recovery_drop_for_fallback().is_none());
        assert_eq!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .disposition,
            DecodeRecoveryDisposition::HoldGap
        );
    }

    #[test]
    fn replay_span_limit_reanchors_only_a_confirmed_synchronized_gap() {
        let session_id = PlaybackSessionId(62);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        let target_nsecs = 10_000_000_000;
        output.begin_decode_recovery(
            62,
            target_nsecs,
            DecodeRecoverySource::VulkanReopenReplay,
            &control,
            session_id,
        );
        output.confirm_decode_recovery_synchronized_timeline_gap();
        output.mark_decode_recovery_replaying(62);
        assert!(output.stage_decode_recovery_frame(
            frame(target_nsecs, 40_000_000, false),
            3,
            session_id,
        ));
        let resume_nsecs = target_nsecs + DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS;
        assert!(output.stage_decode_recovery_frame(
            frame(resume_nsecs, 40_000_000, false),
            3,
            session_id,
        ));

        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Buffered)
        );
        let transaction = output
            .decode_recovery_transaction
            .as_ref()
            .expect("transaction");
        assert_eq!(transaction.disposition, DecodeRecoveryDisposition::Reanchor);
        assert_eq!(transaction.resume_nsecs, resume_nsecs);
        assert_eq!(transaction.staging_queue.len(), 1);
    }

    #[test]
    fn decode_recovery_wall_deadline_enters_explicit_failed_state() {
        let session_id = PlaybackSessionId(5);
        let control = FfmpegControl::new(session_id);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.begin_decode_recovery(
            13,
            4_000_000_000,
            DecodeRecoverySource::SoftwareFallback,
            &control,
            session_id,
        );
        output.mark_decode_recovery_replaying(13);
        assert!(output.stage_decode_recovery_frame(
            frame(4_000_000_000, 40_000_000, false),
            3,
            session_id,
        ));
        output
            .decode_recovery_transaction
            .as_mut()
            .expect("transaction")
            .started_at = Instant::now()
            .checked_sub(DECODE_RECOVERY_MAX_WALL_TIME + Duration::from_millis(1))
            .expect("deadline start");

        assert!(
            output
                .check_decode_recovery_deadline(Instant::now(), &control, session_id)
                .is_err()
        );
        assert_eq!(
            output.decode_recovery_phase(),
            Some(DecodeRecoveryPhase::Failed)
        );
        assert!(!output.decode_recovery_active());
        assert!(
            output
                .decode_recovery_transaction
                .as_ref()
                .expect("transaction")
                .staging_queue
                .is_empty()
        );
    }

    #[test]
    fn audio_sync_drop_before_logs_first_periodic_and_final_summaries() {
        let session_id = PlaybackSessionId(7);
        let mut output = PlaybackOutputScheduler::new();
        output.set_state(PlaybackOutputState::Playing);
        output.set_audio_sync_drop_before_timeline_nsecs(24_000_000_000, session_id, "test");

        for frame_index in 0..209_u64 {
            let snapshot = output.snapshot();
            output.record_audio_sync_drop_before_frame(
                frame_index as i64,
                23_000_000_000 + frame_index * 10_000_000,
                23_010_000_000 + frame_index * 10_000_000,
                snapshot,
                session_id,
            );
        }
        let summary = output
            .audio_sync_drop_log_summary
            .as_ref()
            .expect("active drop summary");
        assert_eq!(summary.total_dropped_frames, 209);
        assert_eq!(summary.suppressed_since_last_log, 208);

        output
            .audio_sync_drop_log_summary
            .as_mut()
            .expect("active drop summary")
            .last_log_at = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("periodic summary timestamp");
        let snapshot = output.snapshot();
        output.record_audio_sync_drop_before_frame(
            209,
            23_999_000_000,
            24_000_000_000,
            snapshot,
            session_id,
        );
        let summary = output
            .audio_sync_drop_log_summary
            .as_ref()
            .expect("active drop summary");
        assert_eq!(summary.total_dropped_frames, 210);
        assert_eq!(summary.suppressed_since_last_log, 0);

        output.finish_audio_sync_drop_log_summary(session_id, "test_complete");
        assert!(output.audio_sync_drop_log_summary.is_none());
    }
}
