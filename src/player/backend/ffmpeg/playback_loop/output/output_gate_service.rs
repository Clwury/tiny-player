use super::output_gate::{OutputGateResumeStatus, service_output_gate_resume_if_ready};
use super::output_rebuffer::demux_reader_ready_for_output;
use super::playback_snapshot::PlaybackPipelineTelemetry;
use super::playback_wait_service::{PlaybackPipelineWaitContext, PlaybackPipelineWaitService};
use super::video_decode_pipeline::HevcPostFallbackRebufferObservation;
use std::{
    os::raw::c_int,
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, ByteCacheState},
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AudioOutput, AudioOutputSnapshot, DemuxPacketCache,
    FfmpegControl, HttpRingCache, OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    PlaybackOutputSnapshot, PlaybackOutputState, PlaybackPipelineState,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    duration_nsecs,
};

const HEVC_POST_FALLBACK_AUDIO_READY_NSECS: u64 = 250_000_000;

#[derive(Default)]
pub(super) struct OutputGateService;

impl OutputGateService {
    pub(super) fn service_or_wait(
        &mut self,
        context: OutputGateServiceContext<'_>,
    ) -> std::result::Result<OutputGateServiceStatus, String> {
        service_output_gate_or_wait(context)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputGateServiceOutcome {
    Ready,
    Continue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputGateServiceStatus {
    pub(super) outcome: OutputGateServiceOutcome,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
    pub(super) played_until_nsecs: Option<u64>,
    pub(super) has_audio_output: bool,
    pub(super) output_resource_pressure: bool,
}

impl OutputGateServiceStatus {
    pub(super) fn should_continue(self) -> bool {
        matches!(self.outcome, OutputGateServiceOutcome::Continue)
    }
}

pub(super) struct OutputGateServiceContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) http_cache: Option<&'a HttpRingCache>,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) control: &'a FfmpegControl,
    pub(super) event_tx: &'a Sender<BackendEvent>,
    pub(super) vo_queue: &'a VideoOutputQueue,
    pub(super) frame_presented: &'a AtomicBool,
    pub(super) playback_wait: &'a PlaybackPipelineWaitService,
    pub(super) playback_telemetry: &'a mut PlaybackPipelineTelemetry,
}

fn service_output_gate_or_wait(
    mut context: OutputGateServiceContext<'_>,
) -> std::result::Result<OutputGateServiceStatus, String> {
    let started_at = Instant::now();
    let mut timing = OutputGateServiceTiming::default();
    let stage_started_at = Instant::now();
    let status = output_gate_service_status(&mut context)?;
    timing.status = stage_started_at.elapsed();
    let current_start_position_nsecs = context.pipeline.current_start_position_nsecs;
    let stage_started_at = Instant::now();
    let output_resource_pressure = status.output_resource_pressure;
    timing.resource_pressure = stage_started_at.elapsed();
    let stage_started_at = Instant::now();
    let audio_decode_snapshot = context
        .pipeline
        .audio_decode_pipeline
        .as_ref()
        .map(|pipeline| pipeline.snapshot());
    let video_decode_snapshot = context.pipeline.video_decode_pipeline.snapshot();
    let (demux_packet_snapshot, _, _) = context.demux_cache.monitor_snapshot();
    let demux_audio_cached_packets = demux_audio_cached_packets_for_stream(
        &demux_packet_snapshot,
        context.pipeline.audio_stream.map(|stream| stream.index),
    );
    let resume_status = service_output_gate_resume_if_ready(
        &mut context.pipeline.output_scheduler,
        context.pipeline.audio_output.as_ref(),
        Some(context.demux_cache),
        context.control,
        context.session_id,
        context.vo_queue,
        context.frame_presented,
        &mut context.pipeline.position_reporter,
        context.event_tx,
        &mut context.pipeline.subtitle_pipeline,
        &mut context.pipeline.buffered_reporter,
        current_start_position_nsecs,
        &mut context.pipeline.current_start_position_nsecs,
        &mut context.pipeline.scheduler,
        output_resource_pressure,
        audio_decode_snapshot
            .map(|snapshot| snapshot.queued_duration_nsecs)
            .unwrap_or_default(),
        audio_decode_snapshot
            .map(|snapshot| snapshot.in_flight_packets)
            .unwrap_or_default(),
        demux_audio_cached_packets,
        Some(demux_packet_snapshot.read_index),
        video_decode_snapshot.pending_input_packets,
        video_decode_snapshot,
        context.pipeline.video_stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC,
        context
            .pipeline
            .video_decode_pipeline
            .hevc_decode_chain_stats(),
        || context.demux_cache.cached_reader_watermark(),
    )?;
    timing.resume = stage_started_at.elapsed();
    let service_status = match resume_status {
        OutputGateResumeStatus::Resumed => OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Continue,
            ..status
        },
        OutputGateResumeStatus::WaitingForDemux => {
            let stage_started_at = Instant::now();
            observe_output_gate_stall(&mut context, "output_gate_demux_wait");
            timing.wait = stage_started_at.elapsed();
            output_gate_service_status_after_resume(status, resume_status)
        }
        OutputGateResumeStatus::Idle | OutputGateResumeStatus::Waiting => {
            output_gate_service_status_after_resume(status, resume_status)
        }
    };
    log_output_gate_service_timing(
        context.session_id,
        started_at.elapsed(),
        timing,
        resume_status,
        service_status,
    );
    Ok(service_status)
}

fn demux_audio_cached_packets_for_stream(
    demux_packet_snapshot: &super::demux_cache::DemuxPacketQueueSnapshot,
    audio_stream_index: Option<c_int>,
) -> Option<usize> {
    let audio_stream_index = audio_stream_index?;
    demux_packet_snapshot
        .streams
        .iter()
        .find(|stream| stream.stream_index == audio_stream_index)
        .map(|stream| stream.queued_packets)
}

fn output_gate_service_status_after_resume(
    status: OutputGateServiceStatus,
    resume_status: OutputGateResumeStatus,
) -> OutputGateServiceStatus {
    match resume_status {
        OutputGateResumeStatus::Resumed => OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Continue,
            ..status
        },
        OutputGateResumeStatus::Idle
        | OutputGateResumeStatus::Waiting
        | OutputGateResumeStatus::WaitingForDemux => status,
    }
}

fn output_gate_service_status(
    context: &mut OutputGateServiceContext<'_>,
) -> std::result::Result<OutputGateServiceStatus, String> {
    let vo_snapshot = context.vo_queue.snapshot();
    let render_backlogged = vo_snapshot.render_backlogged();
    let has_audio_output = context.pipeline.audio_output.is_some();
    let audio_output_snapshot = context
        .pipeline
        .audio_output
        .as_ref()
        .map(AudioOutput::snapshot)
        .transpose()?;
    let output_underrun = context
        .pipeline
        .audio_output
        .as_ref()
        .is_some_and(AudioOutput::underrun_active);
    let played_until_nsecs = audio_output_snapshot.map(|snapshot| snapshot.played_timeline_nsecs);
    let output_snapshot = context
        .pipeline
        .output_scheduler
        .snapshot_for_played_until(played_until_nsecs);
    context
        .pipeline
        .output_scheduler
        .clear_audio_gap_recovery_if_audio_ready(
            audio_output_snapshot,
            played_until_nsecs,
            context.session_id,
            "output_gate_status",
        );
    let output_underflowing = output_snapshot.underflowing()
        || audio_output_starving(output_snapshot, audio_output_snapshot);
    let pending_audio_recoverable = context
        .pipeline
        .output_scheduler
        .pending_start_audio_can_recover_output(audio_output_snapshot)
        || audio_output_underrun_can_recover(
            output_underrun,
            output_snapshot,
            audio_output_snapshot,
        );
    let demux_watermark = context.demux_cache.cached_reader_watermark();
    let demux_cache_insufficient =
        !demux_reader_ready_for_output(demux_watermark, has_audio_output);
    let byte_cache_low_water = context
        .http_cache
        .and_then(HttpRingCache::try_playback_byte_cache_status)
        .is_some_and(byte_cache_active_forward_low_water);
    let (forward_cache_insufficient, forward_cache_min_nsecs) = output_forward_cache_gate(
        demux_cache_insufficient,
        demux_watermark.selected_min_forward_nsecs,
        byte_cache_low_water,
    );
    let entered_rebuffer = context
        .pipeline
        .output_scheduler
        .maybe_enter_video_output_rebuffer(
            Instant::now(),
            output_underflowing,
            output_snapshot.queued_video_forward_nsecs,
            output_underrun,
            forward_cache_insufficient,
            forward_cache_min_nsecs,
            render_backlogged,
            vo_snapshot.queued_frames,
            has_audio_output,
            pending_audio_recoverable,
            context.control,
            context.pipeline.audio_output.as_ref(),
            audio_output_snapshot.map(|snapshot| snapshot.total_pending_nsecs),
            context.session_id,
            output_snapshot.queued_video_forward_nsecs,
        );
    if entered_rebuffer {
        if audio_output_snapshot.is_some_and(|snapshot| snapshot.total_pending_nsecs == 0) {
            context
                .pipeline
                .output_scheduler
                .observe_audio_output_underrun_for_rebuffer(Instant::now(), context.session_id);
        }
        context.pipeline.restore_video_decode_skip_nonref_default(
            Some(context.session_id),
            "video_rebuffer_entry",
        )?;
    }
    let post_rebuffer_snapshot = context
        .pipeline
        .output_scheduler
        .snapshot_for_played_until(played_until_nsecs);
    let decoded_audio_ready_nsecs = audio_output_snapshot
        .map(|snapshot| snapshot.total_pending_nsecs)
        .unwrap_or_default()
        .max(post_rebuffer_snapshot.pending_start_audio_nsecs);
    let post_fallback_target_nsecs = post_rebuffer_snapshot
        .video_output_rebuffer_anchor
        .map(|anchor| anchor.timeline_nsecs)
        .or(played_until_nsecs)
        .unwrap_or(context.pipeline.current_start_position_nsecs);
    context
        .pipeline
        .video_decode_pipeline
        .observe_hevc_post_fallback_rebuffer_underfill(HevcPostFallbackRebufferObservation {
            session_id: context.session_id,
            codec_id: context.pipeline.video_stream.codec_id,
            now: Instant::now(),
            output_snapshot: post_rebuffer_snapshot,
            demux_watermark,
            audio_ready: decoded_audio_ready_nsecs >= HEVC_POST_FALLBACK_AUDIO_READY_NSECS,
            fallback_target_nsecs: post_fallback_target_nsecs,
        });
    let should_wait_for_demux = context
        .pipeline
        .output_scheduler
        .snapshot()
        .should_wait_for_demux();
    let output_resource_pressure = context.pipeline.video_output_resource_pressure_for(
        output_snapshot,
        vo_snapshot,
        audio_output_snapshot.map(|snapshot| snapshot.total_pending_nsecs),
    );

    Ok(OutputGateServiceStatus {
        outcome: OutputGateServiceOutcome::Ready,
        should_wait_for_demux,
        video_output_waiting_for_demux: output_snapshot.waiting_for_demux(),
        played_until_nsecs,
        has_audio_output,
        output_resource_pressure,
    })
}

fn byte_cache_active_forward_low_water(status: ByteCacheState) -> bool {
    if status.idle || status.content_length.is_none() {
        return false;
    }
    if status.active_forward_bytes == 0 {
        return true;
    }
    status
        .active_forward_est_seconds
        .is_some_and(|seconds| seconds <= VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION.as_secs_f64())
}

fn output_forward_cache_gate(
    demux_cache_insufficient: bool,
    demux_min_forward_nsecs: Option<u64>,
    byte_cache_low_water: bool,
) -> (bool, Option<u64>) {
    let byte_cache_low_water = demux_cache_insufficient && byte_cache_low_water;
    (
        demux_cache_insufficient,
        if byte_cache_low_water {
            Some(0)
        } else {
            demux_min_forward_nsecs
        },
    )
}

fn audio_output_underrun_can_recover(
    output_underrun: bool,
    output_snapshot: PlaybackOutputSnapshot,
    audio_output_snapshot: Option<AudioOutputSnapshot>,
) -> bool {
    output_underrun
        && matches!(output_snapshot.state, PlaybackOutputState::Playing)
        && output_snapshot
            .queued_video_forward_nsecs
            .is_some_and(|duration| {
                duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
            })
        && audio_output_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)
        })
}

fn audio_output_starving(
    output_snapshot: PlaybackOutputSnapshot,
    audio_output_snapshot: Option<AudioOutputSnapshot>,
) -> bool {
    matches!(output_snapshot.state, PlaybackOutputState::Playing)
        && output_snapshot.queued_video_frames > 0
        && audio_output_snapshot.is_some_and(|snapshot| snapshot.total_pending_nsecs == 0)
}

fn observe_output_gate_stall(
    context: &mut OutputGateServiceContext<'_>,
    stall_reason: &'static str,
) {
    context.playback_wait.observe_stall(
        &mut PlaybackPipelineWaitContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            video_decode_pipeline: &context.pipeline.video_decode_pipeline,
            video_frame_duration_nsecs: context.pipeline.video_frame_duration_nsecs,
            video_frame_prepare_worker: Some(&context.pipeline.video_frame_prepare_worker),
            audio_decode_pipeline: context.pipeline.audio_decode_pipeline.as_ref(),
            subtitle_pipeline: &context.pipeline.subtitle_pipeline,
            output_scheduler: &context.pipeline.output_scheduler,
            audio_output: context.pipeline.audio_output.as_ref(),
            vo_queue: context.vo_queue,
            playback_telemetry: &mut *context.playback_telemetry,
            playback_loop_deadline: context.pipeline.playback_loop_deadline(),
        },
        stall_reason,
    );
}

#[derive(Clone, Copy, Default)]
struct OutputGateServiceTiming {
    status: Duration,
    resource_pressure: Duration,
    resume: Duration,
    wait: Duration,
}

fn log_output_gate_service_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    timing: OutputGateServiceTiming,
    resume_status: OutputGateResumeStatus,
    status: OutputGateServiceStatus,
) {
    tracing::trace!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        status_ms = timing.status.as_secs_f64() * 1000.0,
        resource_pressure_ms = timing.resource_pressure.as_secs_f64() * 1000.0,
        resume_ms = timing.resume.as_secs_f64() * 1000.0,
        wait_ms = timing.wait.as_secs_f64() * 1000.0,
        resume_status = ?resume_status,
        outcome = ?status.outcome,
        should_wait_for_demux = status.should_wait_for_demux,
        video_output_waiting_for_demux = status.video_output_waiting_for_demux,
        played_until_nsecs = ?status.played_until_nsecs,
        "FFmpeg output gate service timing"
    );
    if total < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.status < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.resource_pressure < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.resume < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
        && timing.wait < OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        status_ms = timing.status.as_secs_f64() * 1000.0,
        resource_pressure_ms = timing.resource_pressure.as_secs_f64() * 1000.0,
        resume_ms = timing.resume.as_secs_f64() * 1000.0,
        wait_ms = timing.wait.as_secs_f64() * 1000.0,
        resume_status = ?resume_status,
        outcome = ?status.outcome,
        should_wait_for_demux = status.should_wait_for_demux,
        video_output_waiting_for_demux = status.video_output_waiting_for_demux,
        played_until_nsecs = ?status.played_until_nsecs,
        "FFmpeg output gate service completed slowly"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AudioOutputSnapshot, OutputGateResumeStatus,
        OutputGateServiceOutcome, OutputGateServiceStatus, PlaybackOutputSnapshot,
        PlaybackOutputState, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
        VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, audio_output_starving,
        audio_output_underrun_can_recover, byte_cache_active_forward_low_water, duration_nsecs,
        output_forward_cache_gate, output_gate_service_status_after_resume,
    };
    use crate::player::backend::ByteCacheState;

    #[test]
    fn output_gate_service_status_reports_continue() {
        let status = OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Continue,
            should_wait_for_demux: false,
            video_output_waiting_for_demux: false,
            played_until_nsecs: None,
            has_audio_output: true,
            output_resource_pressure: false,
        };

        assert!(status.should_continue());
        assert!(
            !OutputGateServiceStatus {
                outcome: OutputGateServiceOutcome::Ready,
                ..status
            }
            .should_continue()
        );
    }

    #[test]
    fn output_gate_demux_wait_allows_decoder_input_pump() {
        let status = OutputGateServiceStatus {
            outcome: OutputGateServiceOutcome::Ready,
            should_wait_for_demux: true,
            video_output_waiting_for_demux: false,
            played_until_nsecs: Some(1_000_000_000),
            has_audio_output: true,
            output_resource_pressure: false,
        };

        let status = output_gate_service_status_after_resume(
            status,
            OutputGateResumeStatus::WaitingForDemux,
        );

        assert_eq!(status.outcome, OutputGateServiceOutcome::Ready);
        assert!(status.should_wait_for_demux);
        assert!(!status.should_continue());
    }

    #[test]
    fn byte_cache_low_water_tracks_active_forward_window() {
        assert!(byte_cache_active_forward_low_water(ByteCacheState {
            content_length: Some(1_000),
            active_forward_bytes: 0,
            ..ByteCacheState::default()
        }));
        assert!(byte_cache_active_forward_low_water(ByteCacheState {
            content_length: Some(1_000),
            active_forward_bytes: 1,
            active_forward_est_seconds: Some(
                VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION.as_secs_f64()
            ),
            ..ByteCacheState::default()
        }));
        assert!(!byte_cache_active_forward_low_water(ByteCacheState {
            content_length: Some(1_000),
            active_forward_bytes: 1,
            active_forward_est_seconds: Some(
                VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION.as_secs_f64() + 0.001
            ),
            ..ByteCacheState::default()
        }));
        assert!(!byte_cache_active_forward_low_water(ByteCacheState {
            idle: true,
            content_length: Some(1_000),
            active_forward_bytes: 0,
            ..ByteCacheState::default()
        }));
        assert!(!byte_cache_active_forward_low_water(ByteCacheState {
            content_length: None,
            active_forward_bytes: 0,
            ..ByteCacheState::default()
        }));
    }

    #[test]
    fn byte_cache_low_water_is_gated_by_demux_packet_cache() {
        let demux_min_forward_nsecs = Some(1_000_000_000);

        assert_eq!(
            output_forward_cache_gate(false, demux_min_forward_nsecs, true),
            (false, demux_min_forward_nsecs)
        );
        assert_eq!(
            output_forward_cache_gate(true, demux_min_forward_nsecs, false),
            (true, demux_min_forward_nsecs)
        );
        assert_eq!(
            output_forward_cache_gate(true, demux_min_forward_nsecs, true),
            (true, Some(0))
        );
    }

    #[test]
    fn audio_output_starving_tracks_audio_clock_output_underrun() {
        let playing_with_video = PlaybackOutputSnapshot {
            state: PlaybackOutputState::Playing,
            first_video_frame_pending: false,
            rebuffering: false,
            queued_video_frames: 20,
            queued_video_duration_nsecs: 800_000_000,
            queued_video_range_nsecs: Some((1_000_000_000, 1_800_000_000)),
            queued_video_forward_nsecs: Some(800_000_000),
            queued_video_contiguous_forward_nsecs: Some(800_000_000),
            queued_video_largest_gap_nsecs: None,
            video_output_low_water: false,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
        };
        let underrun_audio = AudioOutputSnapshot {
            played_timeline_nsecs: 1_000_000_000,
            buffered_until_timeline_nsecs: 1_000_000_000,
            shared_pending_nsecs: 0,
            queue_pending_nsecs: 0,
            total_pending_nsecs: 0,
            queue_frames: 0,
            queue_generation: 7,
        };
        let buffered_audio = AudioOutputSnapshot {
            total_pending_nsecs: 100_000_000,
            buffered_until_timeline_nsecs: 1_100_000_000,
            ..underrun_audio
        };

        assert!(audio_output_starving(
            playing_with_video,
            Some(underrun_audio)
        ));
        assert!(!audio_output_starving(
            playing_with_video,
            Some(buffered_audio)
        ));
        assert!(!audio_output_starving(
            PlaybackOutputSnapshot {
                queued_video_frames: 0,
                ..playing_with_video
            },
            Some(underrun_audio)
        ));
        assert!(!audio_output_starving(
            PlaybackOutputSnapshot {
                state: PlaybackOutputState::Rebuffering,
                rebuffering: true,
                ..playing_with_video
            },
            Some(underrun_audio)
        ));
    }

    #[test]
    fn audio_output_underrun_recovery_requires_decoded_video_and_audio_pending() {
        let playing_with_video = PlaybackOutputSnapshot {
            state: PlaybackOutputState::Playing,
            first_video_frame_pending: false,
            rebuffering: false,
            queued_video_frames: 50,
            queued_video_duration_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            queued_video_range_nsecs: Some((
                0,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            )),
            queued_video_forward_nsecs: Some(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            queued_video_contiguous_forward_nsecs: Some(duration_nsecs(
                VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
            )),
            queued_video_largest_gap_nsecs: None,
            video_output_low_water: false,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
        };
        let recovered_audio = AudioOutputSnapshot {
            played_timeline_nsecs: 0,
            buffered_until_timeline_nsecs: duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION),
            shared_pending_nsecs: duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION),
            queue_pending_nsecs: 0,
            total_pending_nsecs: duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION),
            queue_frames: 0,
            queue_generation: 1,
        };

        assert!(audio_output_underrun_can_recover(
            true,
            playing_with_video,
            Some(recovered_audio)
        ));
        assert!(!audio_output_underrun_can_recover(
            false,
            playing_with_video,
            Some(recovered_audio)
        ));
        assert!(!audio_output_underrun_can_recover(
            true,
            PlaybackOutputSnapshot {
                queued_video_forward_nsecs: Some(
                    duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1
                ),
                ..playing_with_video
            },
            Some(recovered_audio)
        ));
        assert!(!audio_output_underrun_can_recover(
            true,
            playing_with_video,
            Some(AudioOutputSnapshot {
                total_pending_nsecs: duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION) - 1,
                ..recovered_audio
            })
        ));
        assert!(!audio_output_underrun_can_recover(
            true,
            PlaybackOutputSnapshot {
                state: PlaybackOutputState::Rebuffering,
                rebuffering: true,
                ..playing_with_video
            },
            Some(recovered_audio)
        ));
    }
}
