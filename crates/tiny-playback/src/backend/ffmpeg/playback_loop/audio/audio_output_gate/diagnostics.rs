use super::recovery::minimum_recovery_input_nsecs;
use super::{
    AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AudioOutput, FfmpegControl, Instant,
    PendingAudioUnderrunRecoveryPlan, PendingStartAudio, PlaybackSessionId,
    UnderrunRecoveryBlocked, duration_nsecs,
};
use crate::backend::ffmpeg::AudioOutputSnapshot;

#[allow(clippy::too_many_arguments)]
pub(super) fn report_underrun_recovery_blocked(
    pending_audio: &mut PendingStartAudio,
    output: &AudioOutput,
    snapshot: AudioOutputSnapshot,
    reason: UnderrunRecoveryBlocked,
    control: &FfmpegControl,
    video_range_nsecs: Option<(u64, u64)>,
    session_id: PlaybackSessionId,
    plan: Option<PendingAudioUnderrunRecoveryPlan>,
) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let rate = control.playback_rate();
    let Some(emission) = pending_audio.observe_recovery_blocked(
        reason.as_str(),
        snapshot.audio_epoch,
        rate,
        Instant::now(),
    ) else {
        return;
    };
    let output_payload_nsecs = snapshot
        .total_pending_nsecs
        .saturating_sub(snapshot.driver_delay_nsecs);
    let minimum_input_nsecs =
        minimum_recovery_input_nsecs(output_payload_nsecs, snapshot.tempo_pending_nsecs);
    tracing::debug!(
        session_id = ?session_id,
        blocked_on = reason.as_str(),
        log_kind = emission.log_kind,
        suppressed_repeats = emission.suppressed_repeats,
        blocked_ms = emission.blocked_for.as_secs_f64() * 1000.0,
        playback_rate = rate,
        audio_epoch = snapshot.audio_epoch,
        underrun_active = output.underrun_active(),
        underrun_resume_ms = output.underrun_resume_nsecs() as f64 / 1_000_000.0,
        callback_prefill_deficit_ms = output
            .underrun_resume_nsecs()
            .saturating_sub(snapshot.shared_payload_nsecs) as f64 / 1_000_000.0,
        played_timeline_nsecs = snapshot.played_timeline_nsecs,
        audio_buffered_until_timeline_nsecs = snapshot.buffered_until_timeline_nsecs,
        audio_output_pending_ms = snapshot.total_pending_nsecs as f64 / 1_000_000.0,
        audio_output_callback_pending_ms = snapshot.shared_payload_nsecs as f64 / 1_000_000.0,
        audio_output_tempo_pending_ms = snapshot.tempo_pending_nsecs as f64 / 1_000_000.0,
        audio_output_queue_ms = snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        audio_output_worker_in_flight_ms = snapshot.worker_in_flight_nsecs as f64 / 1_000_000.0,
        audio_output_driver_delay_ms = snapshot.driver_delay_nsecs as f64 / 1_000_000.0,
        audio_output_payload_range_nsecs = ?snapshot.payload_range_nsecs,
        video_range_nsecs = ?video_range_nsecs,
        video_lead_limit_nsecs = ?video_range_nsecs
            .map(|(_, end)| end.saturating_add(duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION))),
        recovery_minimum_input_ms = minimum_input_nsecs as f64 / 1_000_000.0,
        pending_audio_frames = pending_audio.len(),
        pending_audio_ms = pending_audio.buffered_duration().as_secs_f64() * 1000.0,
        pending_audio_range_nsecs = ?pending_audio.range_nsecs(),
        pending_audio_contiguous_range_nsecs = ?pending_audio.contiguous_range_nsecs(),
        pending_audio_first_gap_nsecs = ?pending_audio.first_gap_nsecs(),
        audio_start_timeline_nsecs = ?plan.map(|plan| plan.audio_start_timeline_nsecs),
        audio_flush_until_timeline_nsecs = ?plan.map(|plan| plan.audio_flush_until_timeline_nsecs),
        "pending FFmpeg audio underrun recovery blocked"
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicBool, mpsc};

    use super::*;
    use crate::backend::ffmpeg::{
        AudioOutputLifecycle, BufferedReporter, DecodedAudio, PositionReporter,
        playback_loop::{
            SubtitlePipeline, audio_output_gate::recover_pending_start_audio_after_underrun,
            scheduled_video_queue::ScheduledVideoQueue,
        },
    };
    use crate::{render_host::VideoOutputQueue, test_logging::capture_events};

    #[test]
    fn blocked_recovery_logs_reason_and_callback_deficit_without_changing_audio() {
        let session_id = PlaybackSessionId(71);
        let control = Arc::new(FfmpegControl::new(session_id));
        control.set_playback_rate(1.4641);
        control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
        let output = AudioOutput::stopped_for_test(Arc::clone(&control), 88_200, 44_100, 2);
        output.reset_clock(1_000_000_000);
        output.mark_underrun_for_test(1_000_000_000);
        let original_epoch = output.audio_epoch();
        let mut pending = PendingStartAudio::default();
        pending.push(
            DecodedAudio {
                samples: vec![0.25; 2822],
                duration_nsecs: 32_000_000,
            },
            1_000_000_000,
            1_032_000_000,
        );
        let (event_tx, _event_rx) = mpsc::channel();
        let mut video = ScheduledVideoQueue::default();
        let vo_queue = VideoOutputQueue::default();
        let frame_presented = AtomicBool::new(true);
        let mut position = PositionReporter::default();
        let mut subtitles = SubtitlePipeline::empty_for_test();
        let mut buffered = BufferedReporter::new_with_events(true, false);
        let events = capture_events(|| {
            for _ in 0..2 {
                assert!(
                    !recover_pending_start_audio_after_underrun(
                        &mut pending,
                        &output,
                        &control,
                        &mut video,
                        session_id,
                        &vo_queue,
                        &frame_presented,
                        &mut position,
                        &event_tx,
                        &mut subtitles,
                        &mut buffered,
                    )
                    .unwrap()
                );
            }
        });
        assert_eq!(events.len(), 1, "repeated blocked ticks must be suppressed");
        let event = &events[0];
        assert_eq!(event.level, tracing::Level::DEBUG);
        assert!(event.target.starts_with("tiny_playback::backend::ffmpeg"));
        for (key, value) in [
            ("message", "pending FFmpeg audio underrun recovery blocked"),
            ("blocked_on", "missing_video_window"),
            ("playback_rate", "1.4641"),
            ("underrun_active", "true"),
            ("callback_prefill_deficit_ms", "250.0"),
            ("audio_output_callback_pending_ms", "0.0"),
            ("audio_output_tempo_pending_ms", "0.0"),
            ("pending_audio_ms", "32.0"),
            ("video_range_nsecs", "None"),
        ] {
            assert_eq!(event.fields[key], value, "{key}");
        }
        assert_eq!(pending.len(), 1);
        assert_eq!(output.audio_epoch(), original_epoch);
        assert_eq!(
            output.snapshot().unwrap().played_timeline_nsecs,
            1_000_000_000
        );
        assert!(output.underrun_active());
    }
}
