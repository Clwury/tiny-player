use super::decode::DecodeInputRetryStatus;
use super::demux_packet_pump::{
    DemuxPacketPump, DemuxPacketPumpAdmissionContext, DemuxPacketPumpResult,
};
use super::playback_pipeline_state::PlaybackPipelineState;
use super::video_decode_pipeline::VideoPacketAdmissionPressure;
use std::time::{Duration, Instant};

use crate::player::render_host::PlaybackSessionId;

use super::{
    DemuxPacketCache, FfmpegControl, PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER,
    PlaybackBlockReason,
};

#[derive(Debug, PartialEq, Eq)]
enum DecoderInputServiceStatus {
    Progress,
    Backpressured,
    OutputLeadThrottled,
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecoderInputServiceOutcome {
    Ready,
    Backpressured,
    OutputLeadThrottled,
    WouldBlock,
    Continue,
    Eof,
    Stopped,
}

#[derive(Default)]
pub(super) struct DecoderInputService {
    demux_packet_pump: DemuxPacketPump,
}

impl DecoderInputService {
    pub(super) fn service_or_wait(
        &mut self,
        mut context: DecoderInputServiceContext<'_>,
    ) -> std::result::Result<DecoderInputServiceOutcome, String> {
        match service_decoder_input_once(self, &mut context, false) {
            DecoderInputServiceStatus::Progress => Ok(DecoderInputServiceOutcome::Ready),
            DecoderInputServiceStatus::Backpressured => {
                Ok(DecoderInputServiceOutcome::Backpressured)
            }
            DecoderInputServiceStatus::OutputLeadThrottled => {
                Ok(DecoderInputServiceOutcome::OutputLeadThrottled)
            }
            DecoderInputServiceStatus::Eof => Ok(DecoderInputServiceOutcome::Eof),
            DecoderInputServiceStatus::WouldBlock => Ok(DecoderInputServiceOutcome::WouldBlock),
            DecoderInputServiceStatus::Interrupted if context.control.should_stop() => {
                Ok(DecoderInputServiceOutcome::Stopped)
            }
            DecoderInputServiceStatus::Interrupted => Ok(DecoderInputServiceOutcome::Continue),
            DecoderInputServiceStatus::Error(error) => {
                if context.control.has_pending_seek() {
                    Ok(DecoderInputServiceOutcome::Continue)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn service_cached_input(
        &mut self,
        mut context: DecoderInputServiceContext<'_>,
    ) -> std::result::Result<DecoderInputServiceOutcome, String> {
        context.should_wait_for_demux = false;
        context.video_output_waiting_for_demux = false;
        match service_decoder_input_once(self, &mut context, true) {
            DecoderInputServiceStatus::Progress => Ok(DecoderInputServiceOutcome::Ready),
            DecoderInputServiceStatus::Backpressured => {
                Ok(DecoderInputServiceOutcome::Backpressured)
            }
            DecoderInputServiceStatus::OutputLeadThrottled => {
                Ok(DecoderInputServiceOutcome::OutputLeadThrottled)
            }
            DecoderInputServiceStatus::Eof => Ok(DecoderInputServiceOutcome::Eof),
            DecoderInputServiceStatus::WouldBlock => Ok(DecoderInputServiceOutcome::WouldBlock),
            DecoderInputServiceStatus::Interrupted if context.control.should_stop() => {
                Ok(DecoderInputServiceOutcome::Stopped)
            }
            DecoderInputServiceStatus::Interrupted => Ok(DecoderInputServiceOutcome::Continue),
            DecoderInputServiceStatus::Error(error) => {
                if context.control.has_pending_seek() {
                    Ok(DecoderInputServiceOutcome::Continue)
                } else {
                    Err(error)
                }
            }
        }
    }
}

pub(super) struct DecoderInputServiceContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) video_admission_pressure: VideoPacketAdmissionPressure,
    pub(super) control: &'a FfmpegControl,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
}

fn service_decoder_input_once(
    service: &mut DecoderInputService,
    context: &mut DecoderInputServiceContext<'_>,
    cached_only: bool,
) -> DecoderInputServiceStatus {
    let service_started_at = Instant::now();
    let retry_started_at = Instant::now();
    let retry_status = match context
        .pipeline
        .retry_pending_decoder_inputs(context.session_id)
    {
        Ok(status) => status,
        Err(error) => return DecoderInputServiceStatus::Error(error),
    };
    let retry_elapsed = retry_started_at.elapsed();
    if !decoder_input_should_pump_after_retry(retry_status) {
        let status = DecoderInputServiceStatus::Progress;
        log_decoder_input_timing(
            context.session_id,
            service_started_at.elapsed(),
            retry_elapsed,
            Duration::ZERO,
            retry_status,
            "skipped_after_retry_progress",
            &status,
            cached_only,
        );
        return status;
    }

    let pump_started_at = Instant::now();
    let result = service
        .demux_packet_pump
        .poll_and_admit_packet(DemuxPacketPumpAdmissionContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            pipeline: context.pipeline,
            video_admission_pressure: context.video_admission_pressure,
            should_wait_for_demux: context.should_wait_for_demux,
            video_output_waiting_for_demux: context.video_output_waiting_for_demux,
            cached_only,
        });
    let pump_elapsed = pump_started_at.elapsed();
    let pump_result = demux_packet_pump_result_name(&result);
    let status = decoder_input_status_after_retry(retry_status, result);
    log_decoder_input_timing(
        context.session_id,
        service_started_at.elapsed(),
        retry_elapsed,
        pump_elapsed,
        retry_status,
        pump_result,
        &status,
        cached_only,
    );
    log_decoder_input_empty_output_diagnostic(
        context,
        service_started_at.elapsed(),
        retry_elapsed,
        pump_elapsed,
        retry_status,
        pump_result,
        &status,
        cached_only,
    );
    status
}

fn decoder_input_should_pump_after_retry(retry_status: DecodeInputRetryStatus) -> bool {
    !retry_status.made_progress()
}

fn decoder_input_status_after_retry(
    retry_status: DecodeInputRetryStatus,
    result: DemuxPacketPumpResult,
) -> DecoderInputServiceStatus {
    let status = decoder_input_status_from_pump(result);
    if retry_status.made_progress()
        && matches!(
            status,
            DecoderInputServiceStatus::WouldBlock
                | DecoderInputServiceStatus::OutputLeadThrottled
                | DecoderInputServiceStatus::Eof
        )
    {
        DecoderInputServiceStatus::Progress
    } else if retry_status.backpressured()
        && matches!(
            status,
            DecoderInputServiceStatus::WouldBlock
                | DecoderInputServiceStatus::OutputLeadThrottled
                | DecoderInputServiceStatus::Eof
        )
    {
        DecoderInputServiceStatus::Backpressured
    } else {
        status
    }
}

fn decoder_input_status_from_pump(result: DemuxPacketPumpResult) -> DecoderInputServiceStatus {
    match result {
        DemuxPacketPumpResult::Progress => DecoderInputServiceStatus::Progress,
        DemuxPacketPumpResult::Backpressured => DecoderInputServiceStatus::Backpressured,
        DemuxPacketPumpResult::OutputLeadThrottled => {
            DecoderInputServiceStatus::OutputLeadThrottled
        }
        DemuxPacketPumpResult::Eof => DecoderInputServiceStatus::Eof,
        DemuxPacketPumpResult::WouldBlock => DecoderInputServiceStatus::WouldBlock,
        DemuxPacketPumpResult::Interrupted => DecoderInputServiceStatus::Interrupted,
        DemuxPacketPumpResult::Error(error) => DecoderInputServiceStatus::Error(error),
    }
}

fn demux_packet_pump_result_name(result: &DemuxPacketPumpResult) -> &'static str {
    match result {
        DemuxPacketPumpResult::Progress => "progress",
        DemuxPacketPumpResult::Backpressured => "backpressured",
        DemuxPacketPumpResult::OutputLeadThrottled => "output_lead_throttled",
        DemuxPacketPumpResult::Eof => "eof",
        DemuxPacketPumpResult::WouldBlock => "would_block",
        DemuxPacketPumpResult::Interrupted => "interrupted",
        DemuxPacketPumpResult::Error(_) => "error",
    }
}

#[allow(clippy::too_many_arguments)]
fn log_decoder_input_timing(
    session_id: PlaybackSessionId,
    total: Duration,
    retry_elapsed: Duration,
    pump_elapsed: Duration,
    retry_status: DecodeInputRetryStatus,
    pump_result: &'static str,
    status: &DecoderInputServiceStatus,
    cached_only: bool,
) {
    tracing::trace!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        retry_pending_input_ms = retry_elapsed.as_secs_f64() * 1000.0,
        demux_packet_pump_ms = pump_elapsed.as_secs_f64() * 1000.0,
        retry_status = ?retry_status,
        pump_result,
        status = ?status,
        cached_only,
        "FFmpeg decoder input service timing"
    );
    if total < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && retry_elapsed < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
        && pump_elapsed < PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        retry_pending_input_ms = retry_elapsed.as_secs_f64() * 1000.0,
        demux_packet_pump_ms = pump_elapsed.as_secs_f64() * 1000.0,
        retry_status = ?retry_status,
        pump_result,
        status = ?status,
        cached_only,
        "FFmpeg decoder input service completed slowly"
    );
}

#[allow(clippy::too_many_arguments)]
fn log_decoder_input_empty_output_diagnostic(
    context: &DecoderInputServiceContext<'_>,
    total: Duration,
    retry_elapsed: Duration,
    pump_elapsed: Duration,
    retry_status: DecodeInputRetryStatus,
    pump_result: &'static str,
    status: &DecoderInputServiceStatus,
    cached_only: bool,
) {
    let output_snapshot = context.pipeline.output_scheduler.snapshot();
    if output_snapshot.queued_video_frames > 0
        || !(output_snapshot.first_video_frame_pending || output_snapshot.rebuffering)
    {
        return;
    }
    if !matches!(
        status,
        DecoderInputServiceStatus::Progress
            | DecoderInputServiceStatus::WouldBlock
            | DecoderInputServiceStatus::OutputLeadThrottled
            | DecoderInputServiceStatus::Backpressured
    ) {
        return;
    }

    let demux_watermark = context.demux_cache.cached_reader_watermark();
    let demux_packet_snapshot = context.demux_cache.packet_queue_snapshot();
    let decoder_input = context
        .pipeline
        .decoder_input_snapshot(context.video_admission_pressure.output_resource_pressure);
    let video_decode_snapshot = decoder_input.video_decode_snapshot;
    tracing::debug!(
        session_id = ?context.session_id,
        total_ms = total.as_secs_f64() * 1000.0,
        retry_pending_input_ms = retry_elapsed.as_secs_f64() * 1000.0,
        demux_packet_pump_ms = pump_elapsed.as_secs_f64() * 1000.0,
        retry_status = ?retry_status,
        pump_result,
        status = ?status,
        should_wait_for_demux = context.should_wait_for_demux,
        video_output_waiting_for_demux = context.video_output_waiting_for_demux,
        cached_only,
        output_state = ?output_snapshot.state,
        first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_packet_queued = demux_packet_snapshot.total_packets,
        demux_packet_bytes = demux_packet_snapshot.total_bytes,
        demux_packet_streams = ?demux_packet_snapshot.streams,
        demux_min_forward_ms = ?demux_watermark
            .selected_min_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_video_forward_ms = ?demux_watermark
            .video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_audio_forward_ms = ?demux_watermark
            .audio_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        demux_underrun = demux_watermark.underrun,
        demux_video_underrun = demux_watermark.video_underrun,
        demux_audio_underrun = demux_watermark.audio_underrun,
        video_decode_blocked_on = ?decoder_input
            .video_decode_blocked_on
            .map(PlaybackBlockReason::as_str),
        video_decode_state = ?video_decode_snapshot.state,
        video_decode_queued_frames = video_decode_snapshot.queued_frames,
        video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
        video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
        video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
        video_decode_submitted_not_consumed_packets = video_decode_snapshot.submitted_not_consumed_packets,
        video_decode_completed_packets = video_decode_snapshot.completed_packets,
        "FFmpeg decoder input completed while output still has no video frame"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        DecodeInputRetryStatus, DecoderInputServiceOutcome, DecoderInputServiceStatus,
        DemuxPacketPumpResult, decoder_input_should_pump_after_retry,
        decoder_input_status_after_retry, decoder_input_status_from_pump,
    };

    #[test]
    fn decoder_input_service_maps_pump_statuses() {
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Progress),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Backpressured),
            DecoderInputServiceStatus::Backpressured
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::OutputLeadThrottled),
            DecoderInputServiceStatus::OutputLeadThrottled
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::WouldBlock),
            DecoderInputServiceStatus::WouldBlock
        );
        assert_eq!(
            decoder_input_status_from_pump(DemuxPacketPumpResult::Error("decode".to_string())),
            DecoderInputServiceStatus::Error("decode".to_string())
        );
    }

    #[test]
    fn decoder_input_service_yields_after_retry_progress() {
        assert!(!decoder_input_should_pump_after_retry(
            DecodeInputRetryStatus::Queued
        ));
        assert!(decoder_input_should_pump_after_retry(
            DecodeInputRetryStatus::Idle
        ));
        assert!(decoder_input_should_pump_after_retry(
            DecodeInputRetryStatus::Backpressured
        ));
    }

    #[test]
    fn decoder_input_service_preserves_pending_input_progress() {
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::WouldBlock
            ),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::Eof
            ),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Queued,
                DemuxPacketPumpResult::Backpressured
            ),
            DecoderInputServiceStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_service_pumps_other_streams_while_retry_backpressured() {
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Backpressured,
                DemuxPacketPumpResult::Progress
            ),
            DecoderInputServiceStatus::Progress
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Backpressured,
                DemuxPacketPumpResult::WouldBlock
            ),
            DecoderInputServiceStatus::Backpressured
        );
        assert_eq!(
            decoder_input_status_after_retry(
                DecodeInputRetryStatus::Backpressured,
                DemuxPacketPumpResult::Eof
            ),
            DecoderInputServiceStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_service_outcomes_keep_output_layer_separate() {
        assert_ne!(
            DecoderInputServiceOutcome::Ready,
            DecoderInputServiceOutcome::Backpressured
        );
        assert_ne!(
            DecoderInputServiceOutcome::Backpressured,
            DecoderInputServiceOutcome::WouldBlock
        );
        assert_ne!(
            DecoderInputServiceOutcome::OutputLeadThrottled,
            DecoderInputServiceOutcome::WouldBlock
        );
    }
}
