use std::time::{Duration, Instant};

use crate::player::render_host::{PlaybackSessionId, VideoOutputQueue, VideoOutputQueueSnapshot};

use super::audio_decode_worker::AudioDecodeWorkerSnapshot;
use super::demux_cache::DemuxPacketQueueSnapshot;
use super::playback_block::{
    VideoOutputResourcePressure, video_decode_block_reason_with_output_queue,
    video_output_resource_pressure,
};
use super::subtitle_decode_worker::SubtitleDecodeWorkerSnapshot;
use super::video_decode_worker::VideoDecodeWorkerSnapshot;
use super::video_frame_prepare_worker::VideoFramePrepareWorkerSnapshot;
use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AudioDecodePipeline, AudioOutput, AudioOutputSnapshot,
    DemuxPacketCache, DemuxReaderWatermark, PlaybackBlockReason, PlaybackOutputScheduler,
    PlaybackOutputSnapshot, PlaybackOutputState, SubtitlePipeline, VideoDecodePipeline,
    VideoFramePrepareWorker,
};

const PLAYBACK_PIPELINE_SNAPSHOT_LOG_INTERVAL: Duration = Duration::from_millis(250);

pub(super) struct PlaybackPipelineSnapshotContext<'a> {
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) video_decode_pipeline: &'a VideoDecodePipeline,
    pub(super) video_frame_duration_nsecs: u64,
    pub(super) video_frame_prepare_worker: Option<&'a VideoFramePrepareWorker>,
    pub(super) audio_decode_pipeline: Option<&'a AudioDecodePipeline>,
    pub(super) subtitle_pipeline: &'a SubtitlePipeline,
    pub(super) output_scheduler: &'a PlaybackOutputScheduler,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) vo_queue: &'a VideoOutputQueue,
}

pub(super) struct PlaybackPipelineSnapshot {
    blocked_on: PlaybackBlockReason,
    blocked_reasons: Vec<PlaybackBlockReason>,
    output_state: PlaybackOutputState,
    demux_packet_snapshot: DemuxPacketQueueSnapshot,
    demux_reader_watermark: DemuxReaderWatermark,
    video_decode_snapshot: VideoDecodeWorkerSnapshot,
    video_decode_blocked_on: Option<PlaybackBlockReason>,
    video_frame_prepare_snapshot: Option<VideoFramePrepareWorkerSnapshot>,
    video_frame_prepare_blocked_on: Option<PlaybackBlockReason>,
    audio_decode_snapshot: Option<AudioDecodeWorkerSnapshot>,
    audio_decode_blocked_on: Option<PlaybackBlockReason>,
    subtitle_decode_snapshot: Option<SubtitleDecodeWorkerSnapshot>,
    subtitle_decode_blocked_on: Option<PlaybackBlockReason>,
    output_snapshot: PlaybackOutputSnapshot,
    vo_snapshot: VideoOutputQueueSnapshot,
    audio_output_snapshot: Option<AudioOutputSnapshot>,
    demux_snapshot_unavailable: bool,
    demux_read_blocked_for: Option<Duration>,
}

impl PlaybackPipelineSnapshot {
    pub(super) fn capture(context: PlaybackPipelineSnapshotContext<'_>) -> Self {
        let (demux_packet_snapshot, demux_reader_watermark, demux_snapshot_unavailable) =
            context.demux_cache.monitor_snapshot();
        let demux_read_blocked_for = context.demux_cache.demux_read_blocked_for();
        let video_decode_snapshot = context.video_decode_pipeline.snapshot();
        let vo_snapshot = context.vo_queue.snapshot();
        let audio_output_snapshot = context
            .audio_output
            .and_then(|output| output.snapshot().ok());
        let output_snapshot = context.output_scheduler.snapshot_for_played_until(
            audio_output_snapshot.map(|snapshot| snapshot.played_timeline_nsecs),
        );
        let scheduled_video_queue_limit_reached = context
            .output_scheduler
            .scheduled_video_queue_limit_reached(context.subtitle_pipeline.needs_prefetch());
        let video_decode_blocked_on = video_decode_block_reason_with_output_queue(
            VideoDecodePipeline::block_reason_for(
                video_decode_snapshot,
                context.video_decode_pipeline.info(),
            ),
            video_output_resource_pressure(VideoOutputResourcePressure {
                scheduled_video_frames: context.output_scheduler.scheduled_video_queue_len(),
                decoded_video_frames: video_decode_snapshot.queued_frames,
                in_flight_video_packets: video_decode_snapshot.in_flight_packets,
                hardware_accelerated: context.video_decode_pipeline.info().hardware_accelerated,
                scheduled_video_queue_limit_reached,
                fill_phase_for_output_start: context.output_scheduler.output_fill_phase(),
                video_frame_duration_nsecs: context.video_frame_duration_nsecs,
                vo_queue_capacity: vo_snapshot.queue_capacity,
                vo_queued_frames: vo_snapshot.queued_frames,
                queued_video_forward_nsecs: output_snapshot.queued_video_forward_nsecs,
                audio_output_pending_nsecs: audio_output_snapshot
                    .map(|snapshot| snapshot.total_pending_nsecs),
                render_backlogged: vo_snapshot.render_backlogged(),
            }),
        );
        let video_frame_prepare_snapshot = context
            .video_frame_prepare_worker
            .map(VideoFramePrepareWorker::snapshot);
        let video_frame_prepare_blocked_on =
            video_frame_prepare_snapshot.and_then(VideoFramePrepareWorkerSnapshot::block_reason);
        let audio_decode_snapshot = context
            .audio_decode_pipeline
            .map(AudioDecodePipeline::snapshot);
        let audio_decode_blocked_on =
            audio_decode_snapshot.and_then(AudioDecodePipeline::block_reason_for);
        let subtitle_decode_snapshot = context.subtitle_pipeline.snapshot();
        let subtitle_decode_blocked_on =
            subtitle_decode_snapshot.and_then(SubtitlePipeline::block_reason_for);
        let blocked_reasons = Self::resolve_block_reasons(
            vo_snapshot,
            video_decode_blocked_on,
            video_frame_prepare_blocked_on,
            audio_decode_blocked_on,
            subtitle_decode_blocked_on,
            demux_packet_snapshot.prefetch_queue_full()
                && !demux_packet_snapshot.consumer_drainable(),
            demux_reader_watermark,
            audio_output_snapshot,
            output_snapshot.state,
        );
        let blocked_on = blocked_reasons
            .first()
            .copied()
            .unwrap_or(PlaybackBlockReason::OutputGate);

        Self {
            blocked_on,
            blocked_reasons,
            output_state: output_snapshot.state,
            demux_packet_snapshot,
            demux_reader_watermark,
            video_decode_snapshot,
            video_decode_blocked_on,
            video_frame_prepare_snapshot,
            video_frame_prepare_blocked_on,
            audio_decode_snapshot,
            audio_decode_blocked_on,
            subtitle_decode_snapshot,
            subtitle_decode_blocked_on,
            output_snapshot,
            vo_snapshot,
            audio_output_snapshot,
            demux_snapshot_unavailable,
            demux_read_blocked_for,
        }
    }

    pub(super) fn blocked_on(&self) -> PlaybackBlockReason {
        self.blocked_on
    }

    pub(super) fn block_reasons(&self) -> &[PlaybackBlockReason] {
        &self.blocked_reasons
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn resolve_blocked_on(
        vo_snapshot: VideoOutputQueueSnapshot,
        video_decode_blocked_on: Option<PlaybackBlockReason>,
        video_frame_prepare_blocked_on: Option<PlaybackBlockReason>,
        audio_decode_blocked_on: Option<PlaybackBlockReason>,
        subtitle_decode_blocked_on: Option<PlaybackBlockReason>,
        demux_packet_queue_full: bool,
        demux_reader_watermark: DemuxReaderWatermark,
        audio_output_snapshot: Option<AudioOutputSnapshot>,
        output_state: PlaybackOutputState,
    ) -> PlaybackBlockReason {
        Self::resolve_block_reasons(
            vo_snapshot,
            video_decode_blocked_on,
            video_frame_prepare_blocked_on,
            audio_decode_blocked_on,
            subtitle_decode_blocked_on,
            demux_packet_queue_full,
            demux_reader_watermark,
            audio_output_snapshot,
            output_state,
        )
        .first()
        .copied()
        .unwrap_or(PlaybackBlockReason::OutputGate)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_block_reasons(
        vo_snapshot: VideoOutputQueueSnapshot,
        video_decode_blocked_on: Option<PlaybackBlockReason>,
        video_frame_prepare_blocked_on: Option<PlaybackBlockReason>,
        audio_decode_blocked_on: Option<PlaybackBlockReason>,
        subtitle_decode_blocked_on: Option<PlaybackBlockReason>,
        demux_packet_queue_full: bool,
        demux_reader_watermark: DemuxReaderWatermark,
        audio_output_snapshot: Option<AudioOutputSnapshot>,
        output_state: PlaybackOutputState,
    ) -> Vec<PlaybackBlockReason> {
        let mut reasons = Vec::new();

        if let Some(blocked_on) = vo_block_reason(vo_snapshot) {
            push_unique_block_reason(&mut reasons, blocked_on);
        }
        for blocked_on in decoder_backpressure_reasons([
            video_decode_blocked_on,
            video_frame_prepare_blocked_on,
            audio_decode_blocked_on,
            subtitle_decode_blocked_on,
        ]) {
            push_unique_block_reason(&mut reasons, blocked_on);
        }
        if demux_packet_queue_full {
            push_unique_block_reason(&mut reasons, PlaybackBlockReason::PacketQueueFull);
        }
        if audio_output_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs >= duration_to_nsecs(AUDIO_OUTPUT_DELAY_LIMIT)
        }) {
            push_unique_block_reason(&mut reasons, PlaybackBlockReason::AudioOutput);
        }
        for blocked_on in decoder_empty_reasons([
            video_decode_blocked_on,
            audio_decode_blocked_on,
            subtitle_decode_blocked_on,
        ]) {
            push_unique_block_reason(&mut reasons, blocked_on);
        }
        if demux_reader_watermark.underrun
            || demux_reader_watermark.video_underrun
            || demux_reader_watermark.audio_underrun
        {
            push_unique_block_reason(&mut reasons, PlaybackBlockReason::DemuxCache);
        }
        if output_state.first_video_frame_pending() || output_state.rebuffering() {
            push_unique_block_reason(&mut reasons, PlaybackBlockReason::OutputGate);
        }
        if reasons.is_empty() {
            push_unique_block_reason(&mut reasons, PlaybackBlockReason::OutputGate);
        }
        reasons
    }

    fn log(
        self,
        session_id: PlaybackSessionId,
        stall_reason: &'static str,
        planned_wait: Option<Duration>,
    ) {
        let blocked_on_all = self
            .blocked_reasons
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>();
        let audio_decode_state = self.audio_decode_snapshot.map(|snapshot| snapshot.state);
        let video_frame_prepare_state = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.state);
        let video_frame_prepare_pending_input_frames = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.pending_input_frames);
        let video_frame_prepare_pending_input_capacity = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.pending_input_capacity);
        let video_frame_prepare_pending_input_full = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.pending_input_full());
        let video_frame_prepare_in_flight_frames = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.in_flight_frames);
        let video_frame_prepare_completed_frames = self
            .video_frame_prepare_snapshot
            .map(|snapshot| snapshot.completed_frames);
        let audio_decode_queued_ms = self
            .audio_decode_snapshot
            .map(|snapshot| snapshot.queued_duration_nsecs as f64 / 1_000_000.0);
        let audio_decode_in_flight_packets = self
            .audio_decode_snapshot
            .map(|snapshot| snapshot.in_flight_packets);
        let audio_decode_pending_input_packets = self
            .audio_decode_snapshot
            .map(|snapshot| snapshot.pending_input_packets);
        let audio_decode_pending_input_capacity = self
            .audio_decode_snapshot
            .map(|snapshot| snapshot.pending_input_capacity);
        let audio_decode_pending_input_full = self
            .audio_decode_snapshot
            .map(|snapshot| snapshot.pending_input_full());
        let subtitle_decode_state = self.subtitle_decode_snapshot.map(|snapshot| snapshot.state);
        let subtitle_decode_in_flight_packets = self
            .subtitle_decode_snapshot
            .map(|snapshot| snapshot.in_flight_packets);
        let subtitle_decode_pending_input_packets = self
            .subtitle_decode_snapshot
            .map(|snapshot| snapshot.pending_input_packets);
        let subtitle_decode_pending_input_capacity = self
            .subtitle_decode_snapshot
            .map(|snapshot| snapshot.pending_input_capacity);
        let subtitle_decode_pending_input_full = self
            .subtitle_decode_snapshot
            .map(|snapshot| snapshot.pending_input_full());
        let audio_output_pending_ms = self
            .audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0);
        let audio_output_shared_ms = self
            .audio_output_snapshot
            .map(|snapshot| snapshot.shared_pending_nsecs as f64 / 1_000_000.0);
        let audio_output_queue_ms = self
            .audio_output_snapshot
            .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0);
        let audio_output_queue_frames = self
            .audio_output_snapshot
            .map(|snapshot| snapshot.queue_frames);
        let audio_output_queue_generation = self
            .audio_output_snapshot
            .map(|snapshot| snapshot.queue_generation);

        tracing::debug!(
            session_id = ?session_id,
            stall_reason,
            planned_wait_ms = ?planned_wait.map(|duration| duration.as_secs_f64() * 1000.0),
            blocked_on = self.blocked_on.as_str(),
            blocked_on_all = ?blocked_on_all,
            output_state = ?self.output_state,
            demux_packet_queued = self.demux_packet_snapshot.total_packets,
            demux_packet_bytes = self.demux_packet_snapshot.total_bytes,
            demux_packet_limit_bytes = self.demux_packet_snapshot.memory_limit_bytes,
            demux_snapshot_unavailable = self.demux_snapshot_unavailable,
            demux_read_blocked_ms = ?self
                .demux_read_blocked_for
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            demux_packet_streams = ?self.demux_packet_snapshot.streams,
            demux_video_forward_ms = ?self
                .demux_reader_watermark
                .video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?self
                .demux_reader_watermark
                .audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_min_forward_ms = ?self
                .demux_reader_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_underrun = self.demux_reader_watermark.underrun,
            demux_idle = self.demux_reader_watermark.idle,
            video_decode_state = ?self.video_decode_snapshot.state,
            video_decode_blocked_on = ?self.video_decode_blocked_on.map(PlaybackBlockReason::as_str),
            video_decode_queued_frames = self.video_decode_snapshot.queued_frames,
            video_decode_queue_capacity = self.video_decode_snapshot.queue_capacity,
            video_decode_pending_input_packets = self.video_decode_snapshot.pending_input_packets,
            video_decode_pending_input_capacity = self.video_decode_snapshot.pending_input_capacity,
            video_decode_pending_input_full = self.video_decode_snapshot.pending_input_full(),
            video_decode_in_flight_packets = self.video_decode_snapshot.in_flight_packets,
            video_frame_prepare_state = ?video_frame_prepare_state,
            video_frame_prepare_blocked_on =
                ?self.video_frame_prepare_blocked_on.map(PlaybackBlockReason::as_str),
            video_frame_prepare_pending_input_frames =
                ?video_frame_prepare_pending_input_frames,
            video_frame_prepare_pending_input_capacity =
                ?video_frame_prepare_pending_input_capacity,
            video_frame_prepare_pending_input_full = ?video_frame_prepare_pending_input_full,
            video_frame_prepare_in_flight_frames = ?video_frame_prepare_in_flight_frames,
            video_frame_prepare_completed_frames = ?video_frame_prepare_completed_frames,
            audio_decode_state = ?audio_decode_state,
            audio_decode_blocked_on = ?self.audio_decode_blocked_on.map(PlaybackBlockReason::as_str),
            audio_decode_queued_ms = ?audio_decode_queued_ms,
            audio_decode_pending_input_packets = ?audio_decode_pending_input_packets,
            audio_decode_pending_input_capacity = ?audio_decode_pending_input_capacity,
            audio_decode_pending_input_full = ?audio_decode_pending_input_full,
            audio_decode_in_flight_packets = ?audio_decode_in_flight_packets,
            audio_decode_recovery_generation = ?self
                .audio_decode_snapshot
                .and_then(|snapshot| snapshot.recovery_generation),
            audio_decode_recovery_elapsed_ms = ?self
                .audio_decode_snapshot
                .and_then(|snapshot| snapshot.recovery_elapsed)
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            audio_decode_flush_command_sent = ?self
                .audio_decode_snapshot
                .map(|snapshot| snapshot.flush_command_sent),
            audio_decode_stale_results_discarded = ?self
                .audio_decode_snapshot
                .map(|snapshot| snapshot.stale_results_discarded),
            audio_decode_last_result_progress_ms = ?self
                .audio_decode_snapshot
                .and_then(|snapshot| snapshot.last_result_progress_elapsed)
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            subtitle_decode_state = ?subtitle_decode_state,
            subtitle_decode_blocked_on =
                ?self.subtitle_decode_blocked_on.map(PlaybackBlockReason::as_str),
            subtitle_decode_pending_input_packets = ?subtitle_decode_pending_input_packets,
            subtitle_decode_pending_input_capacity = ?subtitle_decode_pending_input_capacity,
            subtitle_decode_pending_input_full = ?subtitle_decode_pending_input_full,
            subtitle_decode_in_flight_packets = ?subtitle_decode_in_flight_packets,
            queued_video_frames = self.output_snapshot.queued_video_frames,
            queued_video_ms = self.output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
            queued_video_range = ?self.output_snapshot.queued_video_range_nsecs,
            queued_video_forward_ms = ?self
                .output_snapshot
                .queued_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            queued_video_contiguous_forward_ms = ?self
                .output_snapshot
                .queued_video_contiguous_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            queued_video_largest_gap_ms = ?self
                .output_snapshot
                .queued_video_largest_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            scheduler_dropped_video_frames =
                self.output_snapshot.scheduler_dropped_video_frames,
            recent_coordinator_stall_ms = ?self
                .output_snapshot
                .recent_coordinator_stall_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            recent_coordinator_stall_age_ms = ?self
                .output_snapshot
                .recent_coordinator_stall_age_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            output_rebuffering = self.output_snapshot.rebuffering,
            output_video_low_water = self.output_snapshot.video_output_low_water,
            output_rebuffer_anchor = ?self.output_snapshot.video_output_rebuffer_anchor,
            pending_start_audio_frames = self.output_snapshot.pending_start_audio_frames,
            pending_start_audio_ms = self.output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
            audio_output_pending_ms = ?audio_output_pending_ms,
            audio_output_shared_ms = ?audio_output_shared_ms,
            audio_output_queue_ms = ?audio_output_queue_ms,
            audio_output_queue_frames = ?audio_output_queue_frames,
            audio_output_queue_generation = ?audio_output_queue_generation,
            vo_queued_frames = self.vo_snapshot.queued_frames,
            vo_queue_capacity = self.vo_snapshot.queue_capacity,
            vo_dropped_frames = self.vo_snapshot.dropped_frames,
            render_backlogged = self.vo_snapshot.render_backlogged(),
            pending_render_requests = self.vo_snapshot.render_backpressure.pending_requests,
            render_last_ms =
                self.vo_snapshot.render_backpressure.last_render_nsecs as f64 / 1_000_000.0,
            render_avg_ms =
                self.vo_snapshot.render_backpressure.average_render_nsecs as f64 / 1_000_000.0,
            "FFmpeg playback pipeline snapshot at coordinator stall"
        );
    }
}

#[derive(Default)]
pub(super) struct PlaybackPipelineTelemetry {
    last_logged_at: Option<Instant>,
    last_blocked_on: Option<PlaybackBlockReason>,
    last_block_reasons: Vec<PlaybackBlockReason>,
    last_stall_reason: Option<&'static str>,
}

impl PlaybackPipelineTelemetry {
    pub(super) fn observe_stall(
        &mut self,
        session_id: PlaybackSessionId,
        stall_reason: &'static str,
        snapshot: PlaybackPipelineSnapshot,
    ) {
        self.observe_stall_with_wait(session_id, stall_reason, snapshot, None);
    }

    pub(super) fn observe_stall_with_wait(
        &mut self,
        session_id: PlaybackSessionId,
        stall_reason: &'static str,
        snapshot: PlaybackPipelineSnapshot,
        planned_wait: Option<Duration>,
    ) {
        let now = Instant::now();
        let blocked_on = snapshot.blocked_on();
        let blocked_reasons_changed =
            self.last_block_reasons.as_slice() != snapshot.block_reasons();
        let should_log = self.last_blocked_on != Some(blocked_on)
            || blocked_reasons_changed
            || self.last_stall_reason != Some(stall_reason)
            || self.last_logged_at.is_none_or(|last_logged_at| {
                now.saturating_duration_since(last_logged_at)
                    >= PLAYBACK_PIPELINE_SNAPSHOT_LOG_INTERVAL
            });
        if !should_log {
            return;
        }

        self.last_logged_at = Some(now);
        self.last_blocked_on = Some(blocked_on);
        self.last_block_reasons.clear();
        self.last_block_reasons
            .extend_from_slice(snapshot.block_reasons());
        self.last_stall_reason = Some(stall_reason);
        snapshot.log(session_id, stall_reason, planned_wait);
    }
}

fn vo_block_reason(snapshot: VideoOutputQueueSnapshot) -> Option<PlaybackBlockReason> {
    match snapshot.blocked_on() {
        Some("render_worker") => Some(PlaybackBlockReason::RenderWorker),
        Some("vo_queue") => Some(PlaybackBlockReason::VideoOutputQueue),
        _ => None,
    }
}

fn push_unique_block_reason(reasons: &mut Vec<PlaybackBlockReason>, reason: PlaybackBlockReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn decoder_backpressure_reasons(
    reasons: [Option<PlaybackBlockReason>; 4],
) -> impl Iterator<Item = PlaybackBlockReason> {
    reasons.into_iter().flatten().filter(|reason| {
        matches!(
            reason,
            PlaybackBlockReason::PacketQueueFull
                | PlaybackBlockReason::DecoderRecovery
                | PlaybackBlockReason::DecoderInFlight
                | PlaybackBlockReason::DecoderOutputPending
                | PlaybackBlockReason::DecodedVideoQueue
                | PlaybackBlockReason::DecodedQueueFull
                | PlaybackBlockReason::HwSurfacePool
                | PlaybackBlockReason::FramePrepareWorker
        )
    })
}

fn decoder_empty_reasons(
    reasons: [Option<PlaybackBlockReason>; 3],
) -> impl Iterator<Item = PlaybackBlockReason> {
    reasons
        .into_iter()
        .flatten()
        .filter(|reason| matches!(reason, PlaybackBlockReason::DecoderInputEmpty))
}

fn duration_to_nsecs(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use crate::player::render_host::{PlaybackSessionId, RenderBackpressure};

    use super::{
        AUDIO_OUTPUT_DELAY_LIMIT, AudioOutputSnapshot, DemuxReaderWatermark, PlaybackBlockReason,
        PlaybackOutputState, PlaybackPipelineSnapshot, VideoOutputQueueSnapshot, duration_to_nsecs,
    };

    fn idle_demux_watermark() -> DemuxReaderWatermark {
        DemuxReaderWatermark {
            video_forward_nsecs: Some(1_000_000_000),
            audio_forward_nsecs: Some(1_000_000_000),
            selected_min_forward_nsecs: Some(1_000_000_000),
            video_underrun: false,
            audio_underrun: false,
            video_idle: false,
            audio_idle: false,
            underrun: false,
            idle: false,
            forward_bytes: 1024,
        }
    }

    fn vo_snapshot(
        queued_frames: usize,
        queue_capacity: usize,
        render_backpressure: RenderBackpressure,
    ) -> VideoOutputQueueSnapshot {
        VideoOutputQueueSnapshot {
            active_session_id: PlaybackSessionId(1),
            queued_frames,
            queue_capacity,
            dropped_frames: 0,
            render_backpressure,
        }
    }

    #[test]
    fn playback_snapshot_prefers_render_worker_block_reason() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(
                1,
                3,
                RenderBackpressure {
                    rendering: true,
                    pending_requests: 1,
                    last_render_nsecs: 10_000_000,
                    average_render_nsecs: 10_000_000,
                },
            ),
            Some(PlaybackBlockReason::DecodedQueueFull),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::RenderWorker);
    }

    #[test]
    fn playback_snapshot_reports_all_simultaneous_block_reasons() {
        let reasons = PlaybackPipelineSnapshot::resolve_block_reasons(
            vo_snapshot(
                1,
                3,
                RenderBackpressure {
                    rendering: true,
                    pending_requests: 1,
                    last_render_nsecs: 10_000_000,
                    average_render_nsecs: 10_000_000,
                },
            ),
            Some(PlaybackBlockReason::HwSurfacePool),
            Some(PlaybackBlockReason::FramePrepareWorker),
            Some(PlaybackBlockReason::DecodedQueueFull),
            None,
            true,
            DemuxReaderWatermark {
                underrun: true,
                ..idle_demux_watermark()
            },
            Some(AudioOutputSnapshot {
                played_timeline_nsecs: 0,
                buffered_until_timeline_nsecs: 0,
                shared_pending_nsecs: 0,
                queue_pending_nsecs: 0,
                total_pending_nsecs: duration_to_nsecs(AUDIO_OUTPUT_DELAY_LIMIT),
                queue_frames: 0,
                queue_generation: 0,
            }),
            PlaybackOutputState::Rebuffering,
        );

        assert_eq!(
            reasons,
            vec![
                PlaybackBlockReason::RenderWorker,
                PlaybackBlockReason::HwSurfacePool,
                PlaybackBlockReason::FramePrepareWorker,
                PlaybackBlockReason::DecodedQueueFull,
                PlaybackBlockReason::PacketQueueFull,
                PlaybackBlockReason::AudioOutput,
                PlaybackBlockReason::DemuxCache,
                PlaybackBlockReason::OutputGate,
            ]
        );
    }

    #[test]
    fn playback_snapshot_deduplicates_shared_packet_queue_pressure() {
        let reasons = PlaybackPipelineSnapshot::resolve_block_reasons(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::PacketQueueFull),
            None,
            None,
            None,
            true,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(reasons, vec![PlaybackBlockReason::PacketQueueFull]);
    }

    #[test]
    fn playback_snapshot_reports_decoder_surface_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::HwSurfacePool),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::HwSurfacePool);
    }

    #[test]
    fn playback_snapshot_reports_decoded_queue_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::DecodedQueueFull),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DecodedQueueFull);
    }

    #[test]
    fn playback_snapshot_reports_decoded_video_queue_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::DecodedVideoQueue),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DecodedVideoQueue);
    }

    #[test]
    fn playback_snapshot_reports_decoder_in_flight_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::DecoderInFlight),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DecoderInFlight);
    }

    #[test]
    fn playback_snapshot_reports_decoder_output_pending_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::DecoderOutputPending),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DecoderOutputPending);
    }

    #[test]
    fn playback_snapshot_reports_frame_prepare_pressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            None,
            Some(PlaybackBlockReason::FramePrepareWorker),
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::FramePrepareWorker);
    }

    #[test]
    fn playback_snapshot_reports_demux_packet_queue_full() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            None,
            None,
            None,
            None,
            true,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::PacketQueueFull);
    }

    #[test]
    fn playback_snapshot_reports_audio_output_backpressure() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            None,
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            Some(AudioOutputSnapshot {
                played_timeline_nsecs: 0,
                buffered_until_timeline_nsecs: 0,
                shared_pending_nsecs: 0,
                queue_pending_nsecs: 0,
                total_pending_nsecs: duration_to_nsecs(AUDIO_OUTPUT_DELAY_LIMIT),
                queue_frames: 0,
                queue_generation: 0,
            }),
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::AudioOutput);
    }

    #[test]
    fn playback_snapshot_reports_decoder_input_empty() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            Some(PlaybackBlockReason::DecoderInputEmpty),
            None,
            None,
            None,
            false,
            idle_demux_watermark(),
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DecoderInputEmpty);
    }

    #[test]
    fn playback_snapshot_reports_demux_cache_underrun() {
        let blocked_on = PlaybackPipelineSnapshot::resolve_blocked_on(
            vo_snapshot(0, 3, RenderBackpressure::default()),
            None,
            None,
            None,
            None,
            false,
            DemuxReaderWatermark {
                underrun: true,
                ..idle_demux_watermark()
            },
            None,
            PlaybackOutputState::Playing,
        );

        assert_eq!(blocked_on, PlaybackBlockReason::DemuxCache);
    }
}
