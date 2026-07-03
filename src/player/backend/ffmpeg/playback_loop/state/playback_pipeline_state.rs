use std::{
    os::raw::c_int,
    sync::{atomic::AtomicBool, mpsc::Sender},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue, VideoOutputQueueSnapshot},
};

use super::audio_decode_worker::{AudioDecodePacketResult, AudioDecodeWorkerSnapshot};
use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoded_audio_frame::process_audio_decode_drain_result;
use super::drain_phase::{PlaybackDrainPhase, PlaybackDrainResults};
use super::playback_block::{
    VideoOutputResourcePressure, video_decode_block_reason_with_output_queue,
    video_output_resource_pressure,
};
use super::playback_wait_service::PlaybackLoopDeadline;
use super::video_decode_drain_frame_processor::{
    VideoDecodeDrainFrameProcessor, VideoDecodeDrainProcessStatus,
};
use super::video_decode_pipeline::{VideoPacketAdmissionContext, VideoPacketAdmissionPressure};
use super::video_decode_worker::{VideoDecodeDrainResult, VideoDecodeWorkerSnapshot};
use super::{
    AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioDecodePipeline, AudioOutput, AudioResumeWaterline,
    AvPacket, BufferedReporter, DoviPipeline, FfmpegControl, PlaybackBlockReason,
    PlaybackGeneration, PlaybackOutputScheduler, PlaybackOutputSnapshot, PlaybackScheduler,
    PositionReporter, StreamInfo, SubtitleDecodeContext, SubtitlePipeline, TimestampMapper,
    VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VideoDecodePipeline, VideoDecodeRecovery, VideoFramePrepareWorker, duration_nsecs,
};

const CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT: Duration = Duration::from_millis(2_500);
const CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS: u64 = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CachedSeekRecoveryFallbackReason {
    FirstVideoFrameTimeout,
    VideoPacketLimit,
}

impl CachedSeekRecoveryFallbackReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::FirstVideoFrameTimeout => "first_video_frame_timeout",
            Self::VideoPacketLimit => "video_packet_limit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CachedSeekRecoveryFallbackAction {
    SoftRecover,
    ReopenSoftware,
    LowLevelSeek,
}

impl CachedSeekRecoveryFallbackAction {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::SoftRecover => "soft_recover",
            Self::ReopenSoftware => "reopen_software",
            Self::LowLevelSeek => "low_level_seek",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryFallback {
    pub(super) target_nsecs: u64,
    pub(super) reason: CachedSeekRecoveryFallbackReason,
    pub(super) action: CachedSeekRecoveryFallbackAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachedSeekRecoveryWatchdogDecision {
    Wait,
    Clear,
    Fallback(CachedSeekRecoveryFallbackReason),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CachedSeekRecoveryWatchdog {
    target_nsecs: u64,
    started_at: Instant,
    start_video_packet_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryWatchdogSnapshot {
    pub(super) target_nsecs: u64,
    pub(super) elapsed: Duration,
    pub(super) remaining: Duration,
    pub(super) video_packets_since_seek: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CachedSeekRecoveryAttempt {
    target_nsecs: u64,
    soft_recoveries: u8,
    software_reopens: u8,
    low_level_seeks: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CachedSeekRecoveryProgress {
    video_packets_since_seek: u64,
    video_decode_pending_input_packets: usize,
    video_decode_in_flight_packets: usize,
    video_decode_completed_packets: usize,
    video_decode_queued_frames: usize,
    seek_preroll_frames: u64,
}

impl CachedSeekRecoveryProgress {
    fn from_decode_snapshot(
        video_packets_since_seek: u64,
        snapshot: VideoDecodeWorkerSnapshot,
        seek_preroll_frames: u64,
    ) -> Self {
        Self {
            video_packets_since_seek,
            video_decode_pending_input_packets: snapshot.pending_input_packets,
            video_decode_in_flight_packets: snapshot.in_flight_packets,
            video_decode_completed_packets: snapshot.completed_packets,
            video_decode_queued_frames: snapshot.queued_frames,
            seek_preroll_frames,
        }
    }

    fn decoder_work_pending(self) -> bool {
        self.video_decode_pending_input_packets > 0
            || self.video_decode_in_flight_packets > 0
            || self.video_decode_completed_packets > 0
            || self.video_decode_queued_frames > 0
    }

    fn has_actual_progress(self) -> bool {
        self.seek_preroll_frames > 0
            || (self.video_packets_since_seek > 0 && self.decoder_work_pending())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputStreamState {
    pub(super) stream_index: c_int,
    pub(super) packet_input_blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputSnapshot {
    pub(super) demux_streams: Vec<c_int>,
    pub(super) video_stream_index: c_int,
    pub(super) audio_stream_index: Option<c_int>,
    pub(super) subtitle_stream_index: Option<c_int>,
    pub(super) audio_resume_waterline: Option<AudioResumeWaterline>,
    pub(super) video_decode_snapshot: VideoDecodeWorkerSnapshot,
    pub(super) video_decode_blocked_on: Option<PlaybackBlockReason>,
}

pub(super) struct PlaybackPipelineState {
    pub(super) video_stream: StreamInfo,
    pub(super) video_frame_duration_nsecs: u64,
    pub(super) video_decode_pipeline: VideoDecodePipeline,
    pub(super) audio_decode_pipeline: Option<AudioDecodePipeline>,
    pub(super) subtitle_pipeline: SubtitlePipeline,
    pub(super) video_decode_recovery: VideoDecodeRecovery,
    pub(super) playback_generation: PlaybackGeneration,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) decoded_video_frame_count: u64,
    pub(super) dropped_video_frames_before_start_count: u64,
    pub(super) dropped_audio_frames_before_start_count: u64,
    pub(super) video_clock: TimestampMapper,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
    pub(super) audio_clock: TimestampMapper,
    pub(super) audio_output: Option<AudioOutput>,
    pub(super) scheduler: PlaybackScheduler,
    pub(super) output_scheduler: PlaybackOutputScheduler,
    pub(super) dovi_pipeline: DoviPipeline,
    pub(super) buffered_reporter: BufferedReporter,
    pub(super) position_reporter: PositionReporter,
    pub(super) video_frame_prepare_worker: VideoFramePrepareWorker,
    pub(super) current_start_position_nsecs: u64,
    pub(super) video_packet_count: u64,
    pub(super) video_decode_skip_nonref_active: bool,
    pub(super) cached_seek_recovery_watchdog: Option<CachedSeekRecoveryWatchdog>,
    pub(super) cached_seek_recovery_attempt: Option<CachedSeekRecoveryAttempt>,
    pub(super) rebuffer_audio_realign_attempt: Option<RebufferAudioRealignAttempt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RebufferAudioRealignAttempt {
    pub(super) target_timeline_nsecs: u64,
    pub(super) attempts: u8,
}

fn mark_video_decode_skip_nonref_inactive(skip_nonref_active: &mut bool) -> bool {
    let was_active = *skip_nonref_active;
    *skip_nonref_active = false;
    was_active
}

impl PlaybackPipelineState {
    pub(super) fn advance_playback_generation(&mut self) -> u64 {
        self.playback_generation.advance()
    }

    pub(super) fn flush_playback_generation(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.restore_video_decode_skip_nonref_default(None, "playback_generation_flush")?;
        self.video_decode_pipeline
            .reset_hevc_decode_chain_watchdog();
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(super) fn observe_rebuffer_audio_realign_attempt(
        &mut self,
        target_timeline_nsecs: u64,
    ) -> u8 {
        let attempts = match self.rebuffer_audio_realign_attempt {
            Some(previous)
                if previous
                    .target_timeline_nsecs
                    .abs_diff(target_timeline_nsecs)
                    <= 500_000_000 =>
            {
                previous.attempts.saturating_add(1)
            }
            _ => 1,
        };
        self.rebuffer_audio_realign_attempt = Some(RebufferAudioRealignAttempt {
            target_timeline_nsecs,
            attempts,
        });
        attempts
    }

    pub(super) fn clear_rebuffer_audio_realign_attempt(&mut self) {
        self.rebuffer_audio_realign_attempt = None;
    }

    pub(super) fn restore_video_decode_skip_nonref_default(
        &mut self,
        session_id: Option<PlaybackSessionId>,
        reason: &'static str,
    ) -> std::result::Result<(), String> {
        self.video_decode_pipeline.set_skip_nonref_frames(false)?;
        let was_active =
            mark_video_decode_skip_nonref_inactive(&mut self.video_decode_skip_nonref_active);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            was_active,
            "resetting FFmpeg video decode nonref skip to default"
        );
        Ok(())
    }

    pub(super) fn soft_recover_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<(), String> {
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.playback_generation.advance();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.video_decode_pipeline.clear_packets();
        self.dovi_pipeline.reset();
        tracing::debug!(
            session_id = ?session_id,
            generation,
            "soft recovered HEVC decode chain while waiting for first decoded video frame"
        );
        Ok(())
    }

    pub(super) fn soft_recover_cached_seek_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.playback_generation.advance();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.dovi_pipeline.reset();
        let requeued_probe_packets = self
            .video_decode_pipeline
            .requeue_hevc_startup_probe_packets(&mut self.playback_generation, session_id)?;
        self.video_decode_pipeline
            .reset_hevc_decode_chain_watchdog();
        tracing::debug!(
            session_id = ?session_id,
            generation,
            requeued_probe_packets,
            "soft recovered HEVC cached seek decode chain without low-level seek"
        );
        Ok(requeued_probe_packets)
    }

    pub(super) fn begin_cached_seek_recovery_watchdog(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) {
        if self.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.cached_seek_recovery_watchdog = None;
            return;
        }
        self.cached_seek_recovery_watchdog = Some(CachedSeekRecoveryWatchdog {
            target_nsecs,
            started_at: Instant::now(),
            start_video_packet_count: self.video_packet_count,
        });
        tracing::debug!(
            ?session_id,
            target_nsecs,
            video_packet_count = self.video_packet_count,
            "started HEVC cached seek recovery watchdog"
        );
    }

    pub(super) fn clear_cached_seek_recovery_watchdog(&mut self) {
        self.cached_seek_recovery_watchdog = None;
        self.cached_seek_recovery_attempt = None;
    }

    pub(super) fn cached_seek_recovery_watchdog_deadline(&self) -> Option<Instant> {
        self.cached_seek_recovery_watchdog
            .map(|watchdog| watchdog.started_at + CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT)
    }

    pub(super) fn playback_loop_deadline(&self) -> PlaybackLoopDeadline {
        PlaybackLoopDeadline::from_cached_seek_recovery_watchdog(
            self.cached_seek_recovery_watchdog_deadline(),
        )
        .with_hevc_startup_stall_watchdog_deadline(
            self.video_decode_pipeline
                .hevc_startup_stall_watchdog_deadline(),
        )
    }

    pub(super) fn cached_seek_recovery_watchdog_expired(&self) -> bool {
        self.cached_seek_recovery_watchdog_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(super) fn cached_seek_recovery_watchdog_snapshot(
        &self,
    ) -> Option<CachedSeekRecoveryWatchdogSnapshot> {
        let watchdog = self.cached_seek_recovery_watchdog?;
        let elapsed = watchdog.started_at.elapsed();
        Some(CachedSeekRecoveryWatchdogSnapshot {
            target_nsecs: watchdog.target_nsecs,
            elapsed,
            remaining: CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT.saturating_sub(elapsed),
            video_packets_since_seek: self
                .video_packet_count
                .saturating_sub(watchdog.start_video_packet_count),
        })
    }

    pub(super) fn take_cached_seek_recovery_fallback(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<CachedSeekRecoveryFallback> {
        let watchdog = self.cached_seek_recovery_watchdog?;
        let output_snapshot = self.output_scheduler.snapshot();
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let elapsed = watchdog.started_at.elapsed();
        let video_packets_since_seek = self
            .video_packet_count
            .saturating_sub(watchdog.start_video_packet_count);
        let progress = CachedSeekRecoveryProgress::from_decode_snapshot(
            video_packets_since_seek,
            video_decode_snapshot,
            self.video_decode_recovery.seek_bootstrap_preroll_frames(),
        );
        tracing::trace!(
            ?session_id,
            target_nsecs = watchdog.target_nsecs,
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            remaining_ms = CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT
                .saturating_sub(elapsed)
                .as_secs_f64()
                * 1000.0,
            video_packets_since_seek,
            video_decode_pending_input_packets = progress.video_decode_pending_input_packets,
            video_decode_in_flight_packets = progress.video_decode_in_flight_packets,
            video_decode_completed_packets = progress.video_decode_completed_packets,
            video_decode_queued_frames = progress.video_decode_queued_frames,
            seek_preroll_frames = progress.seek_preroll_frames,
            decoder_work_pending = progress.decoder_work_pending(),
            queued_video_frames = output_snapshot.queued_video_frames,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            "checked HEVC cached seek recovery watchdog"
        );
        match cached_seek_recovery_watchdog_decision(output_snapshot, elapsed, progress) {
            CachedSeekRecoveryWatchdogDecision::Wait => None,
            CachedSeekRecoveryWatchdogDecision::Clear => {
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    "cleared HEVC cached seek recovery watchdog after first video frame"
                );
                self.cached_seek_recovery_watchdog = None;
                self.cached_seek_recovery_attempt = None;
                None
            }
            CachedSeekRecoveryWatchdogDecision::Fallback(reason) => {
                let action = self.cached_seek_recovery_next_action(watchdog.target_nsecs);
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    reason = reason.as_str(),
                    action = action.as_str(),
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    timeout_ms = CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    video_decode_pending_input_packets =
                        progress.video_decode_pending_input_packets,
                    video_decode_in_flight_packets = progress.video_decode_in_flight_packets,
                    video_decode_completed_packets = progress.video_decode_completed_packets,
                    video_decode_queued_frames = progress.video_decode_queued_frames,
                    seek_preroll_frames = progress.seek_preroll_frames,
                    decoder_work_pending = progress.decoder_work_pending(),
                    max_video_packets = CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    recovery_waiting = self.video_decode_recovery.waiting_for_keyframe(),
                    recovery_skipped_packets = self.video_decode_recovery.skipped_packets(),
                    "HEVC cached seek recovery watchdog requesting fallback"
                );
                self.cached_seek_recovery_watchdog = None;
                Some(CachedSeekRecoveryFallback {
                    target_nsecs: watchdog.target_nsecs,
                    reason,
                    action,
                })
            }
        }
    }

    fn cached_seek_recovery_next_action(
        &mut self,
        target_nsecs: u64,
    ) -> CachedSeekRecoveryFallbackAction {
        cached_seek_recovery_next_action_for_attempt(
            &mut self.cached_seek_recovery_attempt,
            target_nsecs,
            self.video_decode_pipeline.info().hardware_accelerated,
        )
    }

    pub(super) fn decoder_outputs_pending_or_in_flight(&self) -> bool {
        self.video_decode_pipeline.has_pending_or_in_flight()
            || self
                .audio_decode_pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.has_pending_or_in_flight())
            || self.subtitle_pipeline.has_pending_or_in_flight()
    }

    pub(super) fn start_decoder_drain_phase(
        &mut self,
    ) -> std::result::Result<PlaybackDrainPhase, String> {
        PlaybackDrainPhase::start(
            &mut self.playback_generation,
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn poll_decoder_drain_phase(
        &mut self,
        drain_phase: &mut PlaybackDrainPhase,
    ) -> std::result::Result<Option<PlaybackDrainResults>, String> {
        drain_phase.poll(
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn video_drain_frame_processor(
        &mut self,
        video_drain_result: VideoDecodeDrainResult,
    ) -> VideoDecodeDrainFrameProcessor {
        let video_prepare_generation = self.playback_generation.advance();
        VideoDecodeDrainFrameProcessor::new(
            video_drain_result,
            video_prepare_generation,
            self.decoded_video_frame_count,
        )
    }

    pub(super) fn poll_video_drain_processor(
        &mut self,
        processor: &mut VideoDecodeDrainFrameProcessor,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<VideoDecodeDrainProcessStatus, String> {
        processor.poll(
            &self.video_decode_pipeline,
            self.video_frame_duration_nsecs,
            &mut self.video_clock,
            &mut self.playback_timeline_origin_nsecs,
            &mut self.subtitle_pipeline,
            &mut self.current_start_position_nsecs,
            &mut self.dovi_pipeline,
            self.audio_output.as_ref(),
            &mut self.output_scheduler,
            vo_queue,
            &mut self.video_frame_prepare_worker,
            control,
            session_id,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.buffered_reporter,
            &mut self.scheduler,
        )
    }

    pub(super) fn process_audio_drain_result(
        &mut self,
        audio_drain_result: AudioDecodePacketResult,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<(), String> {
        let audio_time_base = self
            .audio_decode_pipeline
            .as_ref()
            .map(|worker| worker.info().time_base);
        process_audio_decode_drain_result(
            audio_drain_result,
            audio_time_base,
            control,
            self.audio_output.as_ref(),
            &mut self.audio_clock,
            self.current_start_position_nsecs,
            &mut self.dropped_audio_frames_before_start_count,
            &mut self.output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.subtitle_pipeline,
            &mut self.buffered_reporter,
        )
    }

    pub(super) fn retry_pending_decoder_inputs(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let video_retry_status = self.video_decode_pipeline.retry_pending_input(session_id)?;

        let audio_retry_status = if self.audio_input_suppressed_until_output_resume() {
            None
        } else {
            self.audio_decode_pipeline
                .as_mut()
                .map(|worker| worker.retry_pending_input(session_id))
                .transpose()?
        };

        let subtitle_retry_status = self.subtitle_pipeline.retry_pending_input(
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )?;

        Ok(decoder_input_retry_status_from_streams([
            Some(video_retry_status),
            audio_retry_status,
            Some(subtitle_retry_status),
        ]))
    }

    pub(super) fn video_decode_stream_index(&self) -> c_int {
        self.video_decode_pipeline.info().stream_index
    }

    pub(super) fn decoder_input_snapshot(
        &self,
        output_resource_pressure: bool,
    ) -> DecoderInputSnapshot {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let video_decode_blocked_on = video_decode_block_reason_with_output_queue(
            VideoDecodePipeline::block_reason_for(
                video_decode_snapshot,
                self.video_decode_pipeline.info(),
            ),
            output_resource_pressure,
        );
        let video_stream_index = self.video_decode_stream_index();
        let audio_decode_snapshot = self
            .audio_decode_pipeline
            .as_ref()
            .map(|pipeline| pipeline.snapshot());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_decode_snapshot);
        let audio_input_suppressed = audio_input_suppressed_until_output_resume_state(
            self.audio_decode_pipeline.is_some(),
            self.output_scheduler.waiting_for_output_resume(),
            audio_resume_waterline,
        );
        let audio_stream = self.audio_decode_pipeline.as_ref().map(|pipeline| {
            let audio_decode_snapshot =
                audio_decode_snapshot.expect("audio snapshot exists when pipeline exists");
            DecoderInputStreamState {
                stream_index: pipeline.info().stream_index,
                packet_input_blocked: audio_input_suppressed
                    || decoder_block_reason_blocks_packet_input(
                        AudioDecodePipeline::block_reason_for(audio_decode_snapshot),
                    ),
            }
        });
        let subtitle_stream = self.subtitle_pipeline.stream_index().map(|stream_index| {
            let subtitle_decode_blocked_on = self
                .subtitle_pipeline
                .snapshot()
                .and_then(SubtitlePipeline::block_reason_for);
            DecoderInputStreamState {
                stream_index,
                packet_input_blocked: decoder_block_reason_blocks_packet_input(
                    subtitle_decode_blocked_on,
                ),
            }
        });

        DecoderInputSnapshot {
            demux_streams: decoder_input_streams_for_state(
                DecoderInputStreamState {
                    stream_index: video_stream_index,
                    packet_input_blocked: decoder_block_reason_blocks_packet_input(
                        video_decode_blocked_on,
                    ),
                },
                audio_stream,
                subtitle_stream,
            ),
            video_stream_index,
            audio_stream_index: audio_stream.map(|stream| stream.stream_index),
            subtitle_stream_index: subtitle_stream.map(|stream| stream.stream_index),
            audio_resume_waterline,
            video_decode_snapshot,
            video_decode_blocked_on,
        }
    }

    fn audio_input_suppressed_until_output_resume(&self) -> bool {
        let audio_decode_snapshot = self
            .audio_decode_pipeline
            .as_ref()
            .map(|pipeline| pipeline.snapshot());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_decode_snapshot);
        audio_input_suppressed_until_output_resume_state(
            self.audio_decode_pipeline.is_some(),
            self.output_scheduler.waiting_for_output_resume(),
            audio_resume_waterline,
        )
    }

    fn audio_resume_waterline_for_output_wait(
        &self,
        audio_decode_snapshot: Option<AudioDecodeWorkerSnapshot>,
    ) -> Option<AudioResumeWaterline> {
        let audio_snapshot = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok());
        self.output_scheduler
            .audio_resume_waterline_for_output_wait(
                audio_snapshot,
                audio_decode_snapshot
                    .map(|snapshot| snapshot.queued_duration_nsecs)
                    .unwrap_or_default(),
                audio_decode_snapshot
                    .map(|snapshot| snapshot.in_flight_packets)
                    .unwrap_or_default(),
                self.current_start_position_nsecs,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
                None,
                None,
            )
    }

    pub(super) fn video_packet_admission_pressure(
        &self,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        vo_snapshot: VideoOutputQueueSnapshot,
    ) -> VideoPacketAdmissionPressure {
        let output_snapshot = self
            .output_scheduler
            .snapshot_for_played_until(played_until_nsecs);
        let audio_output_pending_nsecs = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok())
            .map(|snapshot| snapshot.total_pending_nsecs);
        let output_resource_pressure = self.video_output_resource_pressure_for(
            output_snapshot,
            vo_snapshot,
            audio_output_pending_nsecs,
        );
        VideoPacketAdmissionPressure {
            output_snapshot,
            skip_nonref_for_pressure: self.output_scheduler.video_decode_skip_nonref_for_pressure(
                self.video_stream.codec_id,
                played_until_nsecs,
                has_audio_output,
                audio_output_pending_nsecs,
                self.video_decode_skip_nonref_active,
            ),
            played_until_nsecs,
            output_resource_pressure,
        }
    }

    pub(super) fn video_output_resource_pressure_for(
        &self,
        output_snapshot: PlaybackOutputSnapshot,
        vo_snapshot: VideoOutputQueueSnapshot,
        audio_output_pending_nsecs: Option<u64>,
    ) -> bool {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let scheduled_video_queue_limit_reached = self
            .output_scheduler
            .scheduled_video_queue_limit_reached(self.subtitle_pipeline.needs_prefetch());
        video_output_resource_pressure(VideoOutputResourcePressure {
            scheduled_video_frames: self.output_scheduler.scheduled_video_queue_len(),
            decoded_video_frames: video_decode_snapshot.queued_frames,
            in_flight_video_packets: video_decode_snapshot.in_flight_packets,
            hardware_accelerated: self.video_decode_pipeline.info().hardware_accelerated,
            scheduled_video_queue_limit_reached,
            fill_phase_for_output_start: self.output_scheduler.output_fill_phase(),
            video_frame_duration_nsecs: self.video_frame_duration_nsecs,
            vo_queue_capacity: vo_snapshot.queue_capacity,
            vo_queued_frames: vo_snapshot.queued_frames,
            queued_video_forward_nsecs: output_snapshot.queued_video_forward_nsecs,
            audio_output_pending_nsecs,
            render_backlogged: vo_snapshot.render_backlogged(),
        })
    }

    pub(super) fn admit_video_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
        pressure: VideoPacketAdmissionPressure,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.video_decode_pipeline.admit_demux_packet(
            packet,
            &mut self.video_packet_count,
            &mut self.playback_generation,
            &mut self.video_decode_recovery,
            &mut self.dovi_pipeline,
            &mut self.video_decode_skip_nonref_active,
            VideoPacketAdmissionContext {
                session_id,
                video_stream: self.video_stream,
                output_snapshot: pressure.output_snapshot,
                skip_nonref_for_pressure: pressure.skip_nonref_for_pressure,
                played_until_nsecs: pressure.played_until_nsecs,
            },
        )
    }

    pub(super) fn admit_audio_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if let Some(pipeline) = self.audio_decode_pipeline.as_mut() {
            pipeline.admit_demux_packet(packet, &mut self.playback_generation, session_id)
        } else {
            Ok(DecodePacketAdmissionStatus::Dropped)
        }
    }

    pub(super) fn admit_subtitle_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.subtitle_pipeline.admit_demux_packet(
            packet,
            &mut self.playback_generation,
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )
    }
}

fn cached_seek_recovery_fallback_reason(
    elapsed: Duration,
    progress: CachedSeekRecoveryProgress,
) -> Option<CachedSeekRecoveryFallbackReason> {
    if elapsed >= CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT && !progress.has_actual_progress() {
        return Some(CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout);
    }
    if progress.video_packets_since_seek >= CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS {
        return Some(CachedSeekRecoveryFallbackReason::VideoPacketLimit);
    }
    None
}

fn cached_seek_recovery_next_action_for_attempt(
    attempt: &mut Option<CachedSeekRecoveryAttempt>,
    target_nsecs: u64,
    hardware_accelerated: bool,
) -> CachedSeekRecoveryFallbackAction {
    let reset_attempt = attempt.is_none_or(|attempt| attempt.target_nsecs != target_nsecs);
    if reset_attempt {
        *attempt = Some(CachedSeekRecoveryAttempt {
            target_nsecs,
            ..Default::default()
        });
    }
    let attempt = attempt
        .as_mut()
        .expect("cached seek recovery attempt exists");
    if attempt.soft_recoveries == 0 {
        attempt.soft_recoveries = attempt.soft_recoveries.saturating_add(1);
        return CachedSeekRecoveryFallbackAction::SoftRecover;
    }
    if hardware_accelerated && attempt.software_reopens == 0 {
        attempt.software_reopens = attempt.software_reopens.saturating_add(1);
        return CachedSeekRecoveryFallbackAction::ReopenSoftware;
    }
    attempt.low_level_seeks = attempt.low_level_seeks.saturating_add(1);
    CachedSeekRecoveryFallbackAction::LowLevelSeek
}

fn cached_seek_recovery_watchdog_decision(
    output_snapshot: PlaybackOutputSnapshot,
    elapsed: Duration,
    progress: CachedSeekRecoveryProgress,
) -> CachedSeekRecoveryWatchdogDecision {
    if !output_snapshot.first_video_frame_pending || output_snapshot.queued_video_frames > 0 {
        return CachedSeekRecoveryWatchdogDecision::Clear;
    }
    cached_seek_recovery_fallback_reason(elapsed, progress)
        .map(CachedSeekRecoveryWatchdogDecision::Fallback)
        .unwrap_or(CachedSeekRecoveryWatchdogDecision::Wait)
}

fn decoder_input_retry_status_from_streams(
    statuses: impl IntoIterator<Item = Option<DecodeInputRetryStatus>>,
) -> DecodeInputRetryStatus {
    let mut made_progress = false;
    let mut backpressured = false;
    for status in statuses.into_iter().flatten() {
        made_progress |= status.made_progress();
        backpressured |= status.backpressured();
    }
    if backpressured {
        DecodeInputRetryStatus::Backpressured
    } else if made_progress {
        DecodeInputRetryStatus::Queued
    } else {
        DecodeInputRetryStatus::Idle
    }
}

fn decoder_block_reason_blocks_packet_input(blocked_on: Option<PlaybackBlockReason>) -> bool {
    matches!(
        blocked_on,
        Some(
            PlaybackBlockReason::PacketQueueFull
                | PlaybackBlockReason::DecoderInFlight
                | PlaybackBlockReason::DecoderOutputPending
                | PlaybackBlockReason::DecodedVideoQueue
                | PlaybackBlockReason::DecodedQueueFull
                | PlaybackBlockReason::HwSurfacePool
        )
    )
}

fn audio_input_suppressed_until_output_resume_state(
    has_audio_decode_pipeline: bool,
    output_waiting_for_resume: bool,
    audio_resume_waterline: Option<AudioResumeWaterline>,
) -> bool {
    // Total pending-start audio duration is not the same as continuous coverage
    // from the eventual resume timeline. Keep feeding audio until the same
    // resume waterline used by the output gate has headroom beyond the resume
    // target; stale preroll or disconnected pending audio must not close the
    // audio demux/decode path.
    has_audio_decode_pipeline
        && output_waiting_for_resume
        && audio_resume_waterline.is_some_and(|waterline| {
            waterline.reaches_target_with_margin(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
        })
}

fn decoder_input_streams_for_state(
    video: DecoderInputStreamState,
    audio: Option<DecoderInputStreamState>,
    subtitle: Option<DecoderInputStreamState>,
) -> Vec<c_int> {
    let mut streams = Vec::with_capacity(3);
    push_decoder_input_stream_if_open(&mut streams, video);
    if let Some(audio) = audio {
        push_decoder_input_stream_if_open(&mut streams, audio);
    }
    if let Some(subtitle) = subtitle {
        push_decoder_input_stream_if_open(&mut streams, subtitle);
    }
    streams
}

fn push_decoder_input_stream_if_open(streams: &mut Vec<c_int>, stream: DecoderInputStreamState) {
    if !stream.packet_input_blocked && !streams.contains(&stream.stream_index) {
        streams.push(stream.stream_index);
    }
}

#[cfg(test)]
mod tests {
    use std::{os::raw::c_int, time::Duration};

    use super::super::decode::DecodeInputRetryStatus;
    use super::super::{
        AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN, AudioResumeWaterline, PlaybackBlockReason,
        PlaybackOutputSnapshot, PlaybackOutputState, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
        duration_nsecs,
    };
    use super::{
        CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT, CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS,
        CachedSeekRecoveryAttempt, CachedSeekRecoveryFallbackAction,
        CachedSeekRecoveryFallbackReason, CachedSeekRecoveryProgress,
        CachedSeekRecoveryWatchdogDecision, DecoderInputStreamState,
        audio_input_suppressed_until_output_resume_state, cached_seek_recovery_fallback_reason,
        cached_seek_recovery_next_action_for_attempt, cached_seek_recovery_watchdog_decision,
        decoder_block_reason_blocks_packet_input, decoder_input_retry_status_from_streams,
        decoder_input_streams_for_state, mark_video_decode_skip_nonref_inactive,
    };

    fn stream(stream_index: c_int, packet_input_blocked: bool) -> DecoderInputStreamState {
        DecoderInputStreamState {
            stream_index,
            packet_input_blocked,
        }
    }

    fn audio_waterline(decoded_audio_forward_nsecs: Option<u64>) -> AudioResumeWaterline {
        AudioResumeWaterline {
            resume_timeline_nsecs: 1_000_000_000,
            target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
            audio_output_buffered_until_nsecs: None,
            audio_output_pending_nsecs: None,
            pending_audio_start_nsecs: Some(1_000_000_000),
            pending_audio_forward_nsecs: decoded_audio_forward_nsecs,
            decoded_audio_forward_nsecs,
            audio_decode_queued_nsecs: 0,
            audio_decode_in_flight_packets: 0,
            demux_audio_forward_nsecs: None,
            demux_audio_cached_packets: None,
            ready: decoded_audio_forward_nsecs.is_some_and(|duration| {
                duration >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
            }),
        }
    }

    fn output_snapshot(
        first_video_frame_pending: bool,
        queued_video_frames: usize,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state: PlaybackOutputState::Syncing,
            first_video_frame_pending,
            rebuffering: false,
            queued_video_frames,
            queued_video_duration_nsecs: 0,
            queued_video_range_nsecs: None,
            queued_video_forward_nsecs: None,
            queued_video_contiguous_forward_nsecs: None,
            queued_video_largest_gap_nsecs: None,
            video_output_low_water: false,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
        }
    }

    fn cached_seek_progress(video_packets_since_seek: u64) -> CachedSeekRecoveryProgress {
        CachedSeekRecoveryProgress {
            video_packets_since_seek,
            ..Default::default()
        }
    }

    fn cached_seek_progress_with_decoder_work(
        video_packets_since_seek: u64,
    ) -> CachedSeekRecoveryProgress {
        CachedSeekRecoveryProgress {
            video_packets_since_seek,
            video_decode_in_flight_packets: 1,
            ..Default::default()
        }
    }

    #[test]
    fn video_decode_skip_nonref_reset_marks_active_state_inactive() {
        let mut active = true;

        assert!(mark_video_decode_skip_nonref_inactive(&mut active));
        assert!(!active);
        assert!(!mark_video_decode_skip_nonref_inactive(&mut active));
    }

    #[test]
    fn cached_seek_recovery_fallback_waits_before_limits() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS - 1),
            ),
            None
        );
    }

    #[test]
    fn cached_seek_recovery_fallback_triggers_on_first_frame_timeout() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
            ),
            Some(CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout)
        );
    }

    #[test]
    fn cached_seek_recovery_fallback_waits_after_deadline_with_decoder_progress() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress_with_decoder_work(1),
            ),
            None
        );
    }

    #[test]
    fn cached_seek_recovery_fallback_triggers_on_video_packet_limit() {
        assert_eq!(
            cached_seek_recovery_fallback_reason(
                Duration::from_millis(1),
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS),
            ),
            Some(CachedSeekRecoveryFallbackReason::VideoPacketLimit)
        );
    }

    #[test]
    fn cached_seek_recovery_actions_escalate_for_same_hardware_target() {
        let mut attempt = None::<CachedSeekRecoveryAttempt>;

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true),
            CachedSeekRecoveryFallbackAction::ReopenSoftware
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, true),
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
    }

    #[test]
    fn cached_seek_recovery_actions_reset_for_new_target() {
        let mut attempt = Some(CachedSeekRecoveryAttempt {
            target_nsecs: 35_000_000_000,
            soft_recoveries: 1,
            software_reopens: 1,
            low_level_seeks: 1,
        });

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 83_000_000_000, true),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
    }

    #[test]
    fn cached_seek_recovery_actions_skip_software_reopen_when_decoder_is_software() {
        let mut attempt = None::<CachedSeekRecoveryAttempt>;

        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, false),
            CachedSeekRecoveryFallbackAction::SoftRecover
        );
        assert_eq!(
            cached_seek_recovery_next_action_for_attempt(&mut attempt, 35_000_000_000, false),
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_waits_before_deadline_without_first_video() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                output_snapshot(true, 0),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT - Duration::from_millis(1),
                cached_seek_progress(CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS - 1),
            ),
            CachedSeekRecoveryWatchdogDecision::Wait
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_falls_back_after_deadline_without_first_video() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                output_snapshot(true, 0),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
            ),
            CachedSeekRecoveryWatchdogDecision::Fallback(
                CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout
            )
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_waits_after_deadline_with_seek_preroll_progress() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                output_snapshot(true, 0),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                CachedSeekRecoveryProgress {
                    seek_preroll_frames: 1,
                    ..Default::default()
                },
            ),
            CachedSeekRecoveryWatchdogDecision::Wait
        );
    }

    #[test]
    fn cached_seek_recovery_watchdog_clears_after_first_video() {
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                output_snapshot(false, 0),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
            ),
            CachedSeekRecoveryWatchdogDecision::Clear
        );
        assert_eq!(
            cached_seek_recovery_watchdog_decision(
                output_snapshot(true, 1),
                CACHED_SEEK_FIRST_VIDEO_FRAME_TIMEOUT,
                cached_seek_progress(0),
            ),
            CachedSeekRecoveryWatchdogDecision::Clear
        );
    }

    #[test]
    fn decoder_input_streams_skip_only_backpressured_streams() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, true),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![11, 12]
        );
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, true)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_deduplicate_shared_stream_indices() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(10, false)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_allow_all_streams_until_their_decoder_queue_is_full() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn audio_input_suppression_waits_until_resume_waterline_has_margin() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(duration_nsecs(
                VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
            ))))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
                    - 1
            )))
        ));
        assert!(audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(None))
        ));
    }

    #[test]
    fn decoder_input_keeps_audio_stream_open_when_resume_audio_is_below_target() {
        let audio_input_suppressed = audio_input_suppressed_until_output_resume_state(
            true,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
            ))),
        );

        assert!(!audio_input_suppressed);
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, audio_input_suppressed)),
                Some(stream(12, false))
            ),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn audio_input_suppression_only_applies_while_output_waits_for_video() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            false,
            true,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            false,
            Some(audio_waterline(Some(
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    + duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            )))
        ));
    }

    #[test]
    fn decoder_block_reason_blocks_only_packet_input_pressure() {
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::PacketQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderInFlight
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderOutputPending
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedVideoQueue
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::HwSurfacePool
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderInputEmpty
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::RenderWorker
        )));
        assert!(!decoder_block_reason_blocks_packet_input(None));
    }

    #[test]
    fn decoder_input_retry_status_keeps_backpressure_after_other_stream_progress() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Backpressured),
                Some(DecodeInputRetryStatus::Queued),
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_retry_status_reports_progress_without_backpressure() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                Some(DecodeInputRetryStatus::Queued),
                None,
            ]),
            DecodeInputRetryStatus::Queued
        );
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                None,
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Idle
        );
    }
}
