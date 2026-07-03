use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use crate::player::render_host::PlaybackSessionId;

use super::decode::DecodePacketAdmissionStatus;
use super::demux_cache::{DemuxPacketCacheReadTiming, DemuxPacketQueueSnapshot};
use super::playback_pipeline_state::{DecoderInputSnapshot, PlaybackPipelineState};
use super::video_decode_pipeline::VideoPacketAdmissionPressure;
use super::video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState};
use super::{
    AUDIO_VIDEO_QUEUE_LIMIT_DURATION, AvPacket, DEMUX_CACHE_LOCK_TIMING_LOG_AFTER,
    DEMUX_PACKET_CACHE_LOCK_WAIT, DEMUX_PUMP_TIMING_LOG_INTERVAL, DEMUX_READ_WAIT_LOG_AFTER,
    DemuxPacketCache, DemuxReadResult, DemuxReaderWatermark, PlaybackBlockReason,
    PlaybackOutputSnapshot, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_PREBUFFER_DURATION, duration_nsecs,
};

const DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK: usize = 16;
const DEMUX_PACKET_PUMP_MAX_SYNC_DURATION_PER_TICK: Duration = Duration::from_millis(4);
const DEMUX_PACKET_PUMP_HARD_DEADLINE: Duration = Duration::from_millis(3);
const DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT: Duration = Duration::from_millis(20);
const DEMUX_PACKET_CACHE_READY_LOCK_WAIT: Duration = Duration::from_millis(1);
const DEMUX_PACKET_CACHE_READY_FORWARD_NSECS: u64 = 2_000_000_000;
const DEMUX_PACKET_FORCE_CONSUMER_DRAIN_RETRY_WAIT: Duration = Duration::from_millis(2);

#[derive(Default)]
pub(super) struct DemuxPacketPump {
    stream_cursor: usize,
    rebuffer_audio_priority_without_audio_progress: usize,
    last_timing_log_at: Option<Instant>,
}

struct DemuxPacketPumpContext<'a> {
    session_id: PlaybackSessionId,
    demux_cache: &'a DemuxPacketCache,
    decoder_input: &'a DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    cached_reader_watermark: DemuxReaderWatermark,
    hard_deadline: Option<Instant>,
}

pub(super) struct DemuxPacketPumpAdmissionContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) video_admission_pressure: VideoPacketAdmissionPressure,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemuxPacketRoute {
    Video,
    Audio,
    Subtitle,
    Other,
}

pub(super) enum DemuxPacketPumpResult {
    Progress,
    Backpressured,
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

impl DemuxPacketPump {
    fn poll_packet(&mut self, context: DemuxPacketPumpContext<'_>) -> DemuxReadResult {
        let (demux_streams, demux_stream_rotation) = self.ordered_demux_streams_for_context(
            context.decoder_input,
            context.video_admission_pressure.output_snapshot,
        );

        let (mut demux_packet_snapshot, _, _) = context.demux_cache.monitor_snapshot();
        let reader_head_lost_streams =
            demux_packet_snapshot.reader_head_lost_streams(&demux_streams);
        if !reader_head_lost_streams.is_empty() {
            tracing::warn!(
                session_id = ?context.session_id,
                streams = ?demux_streams,
                reader_head_lost_streams = ?reader_head_lost_streams,
                demux_packet_queued = demux_packet_snapshot.total_packets,
                demux_packet_streams = ?demux_packet_snapshot.streams,
                "demux_reader_head_lost"
            );
            if context
                .demux_cache
                .repair_reader_heads_for_read_index("demux_reader_head_lost")
            {
                demux_packet_snapshot = context.demux_cache.monitor_snapshot().0;
            }
        }
        let decoder_input_waiting = decoder_input_waiting_for_packets(context.decoder_input);
        let consumer_drainable_selected =
            demux_packet_snapshot.consumer_drainable_for_streams(&demux_streams);
        let force_consumer_drain = decoder_input_waiting && consumer_drainable_selected;

        let demux_read_started_at = Instant::now();
        let read_path;
        let mut requested_lock_wait = None;
        let mut applied_lock_wait = None;
        let mut force_consumer_drain_retry_count = 0;
        let (demux_read_result, demux_consumed_stream_offset, demux_cache_timing) =
            if force_consumer_drain {
                read_path = "force_consumer_drain";
                tracing::trace!(
                    session_id = ?context.session_id,
                    streams = ?demux_streams,
                    demux_packet_streams = ?demux_packet_snapshot.streams,
                    "draining FFmpeg demux packets from consumer-readable cache"
                );
                let (mut result, mut stream_offset, mut timing) = context
                    .demux_cache
                    .poll_packet_round_robin_with_timing(&demux_streams);
                if matches!(result, DemuxReadResult::WouldBlock)
                    && let Some(lock_wait) = force_consumer_drain_retry_wait(&context)
                {
                    force_consumer_drain_retry_count = 1;
                    requested_lock_wait = Some(lock_wait);
                    applied_lock_wait = Some(lock_wait);
                    tracing::trace!(
                        session_id = ?context.session_id,
                        streams = ?demux_streams,
                        retry_lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
                        "retrying FFmpeg demux force-consumer-drain after would-block"
                    );
                    let (retry_result, retry_stream_offset, retry_timing) = context
                        .demux_cache
                        .read_available_packet_round_robin_with_cache_pause_signal_and_timing(
                            &demux_streams,
                            lock_wait,
                            demux_cache_pause_signal(&context),
                        );
                    result = retry_result;
                    stream_offset = retry_stream_offset;
                    timing = combine_demux_read_timing(timing, retry_timing);
                }
                (result, stream_offset, timing)
            } else if let Some(lock_wait) = demux_pump_cache_lock_wait(
                context.should_wait_for_demux,
                context.video_output_waiting_for_demux,
                context.decoder_input,
                context.video_admission_pressure,
                context.cached_reader_watermark,
            ) {
                read_path = "bounded_wait";
                requested_lock_wait = Some(lock_wait);
                let lock_wait = context
                    .hard_deadline
                    .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                    .map(|remaining| remaining.min(lock_wait))
                    .unwrap_or(Duration::ZERO);
                applied_lock_wait = Some(lock_wait);
                context
                    .demux_cache
                    .read_available_packet_round_robin_with_cache_pause_signal_and_timing(
                        &demux_streams,
                        lock_wait,
                        demux_cache_pause_signal(&context),
                    )
            } else {
                read_path = "poll_nowait";
                context
                    .demux_cache
                    .poll_packet_round_robin_with_timing(&demux_streams)
            };
        let demux_read_elapsed = demux_read_started_at.elapsed();
        self.log_read_path_diagnostic(
            &context,
            &demux_streams,
            demux_stream_rotation,
            read_path,
            decoder_input_waiting,
            consumer_drainable_selected,
            force_consumer_drain,
            requested_lock_wait,
            applied_lock_wait,
            force_consumer_drain_retry_count,
            demux_read_elapsed,
            demux_cache_timing,
            &demux_read_result,
            &demux_packet_snapshot,
        );
        self.log_rebuffer_audio_reader_head_diagnostic(
            &context,
            &demux_streams,
            read_path,
            &demux_read_result,
        );
        self.trace_timing(
            &context,
            &demux_streams,
            demux_read_elapsed,
            demux_cache_timing,
            &demux_read_result,
        );
        if demux_read_elapsed >= DEMUX_READ_WAIT_LOG_AFTER {
            self.log_wait(
                &context,
                &demux_streams,
                demux_read_elapsed,
                demux_cache_timing,
                &demux_read_result,
            );
        } else if self.should_log_timing(demux_cache_timing) {
            self.log_timing(
                &context,
                &demux_streams,
                demux_read_elapsed,
                demux_cache_timing,
                &demux_read_result,
            );
        }

        if matches!(demux_read_result, DemuxReadResult::Packet(_))
            && !demux_streams.is_empty()
            && let Some(consumed_offset) = demux_consumed_stream_offset
        {
            self.stream_cursor =
                (demux_stream_rotation + consumed_offset + 1) % demux_streams.len();
        }

        demux_read_result
    }

    #[allow(clippy::too_many_arguments)]
    fn log_read_path_diagnostic(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_stream_rotation: usize,
        read_path: &'static str,
        decoder_input_waiting: bool,
        consumer_drainable_selected: bool,
        force_consumer_drain: bool,
        requested_lock_wait: Option<Duration>,
        applied_lock_wait: Option<Duration>,
        force_consumer_drain_retry_count: u64,
        demux_read_elapsed: Duration,
        demux_cache_timing: DemuxPacketCacheReadTiming,
        demux_read_result: &DemuxReadResult,
        demux_packet_snapshot: &DemuxPacketQueueSnapshot,
    ) {
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        let should_log = matches!(demux_read_result, DemuxReadResult::WouldBlock)
            && (force_consumer_drain
                || output_snapshot.rebuffering
                || output_snapshot.first_video_frame_pending
                || context.video_output_waiting_for_demux);
        if !should_log {
            return;
        }

        let video_decode_snapshot = context.decoder_input.video_decode_snapshot;
        tracing::debug!(
            session_id = ?context.session_id,
            read_path,
            result = demux_read_result_name(demux_read_result),
            total_ms = demux_read_elapsed.as_secs_f64() * 1000.0,
            requested_lock_wait_ms =
                ?requested_lock_wait.map(|duration| duration.as_secs_f64() * 1000.0),
            applied_lock_wait_ms =
                ?applied_lock_wait.map(|duration| duration.as_secs_f64() * 1000.0),
            cache_lock_wait_ms = demux_cache_timing.lock_wait.as_secs_f64() * 1000.0,
            cache_try_lock_failures = demux_cache_timing.try_lock_failures,
            cache_lock_timed_out = demux_cache_timing.lock_timed_out,
            cache_data_wait_ms = demux_cache_timing.data_wait.as_secs_f64() * 1000.0,
            cache_data_waits = demux_cache_timing.data_waits,
            demux_streams = ?demux_streams,
            demux_stream_rotation,
            decoder_input_waiting,
            consumer_drainable_selected,
            force_consumer_drain,
            force_consumer_drain_retry_count,
            should_wait_for_demux = context.should_wait_for_demux,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
            output_state = ?output_snapshot.state,
            output_rebuffering = output_snapshot.rebuffering,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            queued_video_frames = output_snapshot.queued_video_frames,
            queued_video_forward_ms = ?output_snapshot
                .queued_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_packet_queued = demux_packet_snapshot.total_packets,
            demux_packet_bytes = demux_packet_snapshot.total_bytes,
            demux_packet_streams = ?demux_packet_snapshot.streams,
            video_decode_blocked_on = ?context
                .decoder_input
                .video_decode_blocked_on
                .map(PlaybackBlockReason::as_str),
            video_decode_state = ?video_decode_snapshot.state,
            video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
            video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
            video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
            video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
            "FFmpeg demux packet pump read path returned would-block"
        );
    }

    fn log_rebuffer_audio_reader_head_diagnostic(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        read_path: &'static str,
        demux_read_result: &DemuxReadResult,
    ) {
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        if !rebuffer_audio_resume_low_water_priority_active(output_snapshot, context.decoder_input)
        {
            return;
        }
        let Some(audio_stream_index) = context.decoder_input.audio_stream_index else {
            return;
        };
        let Some(audio_waterline) = context.decoder_input.audio_resume_waterline else {
            return;
        };
        let reader_head = context
            .demux_cache
            .stream_reader_head_timeline(audio_stream_index);
        let reader_head_far_ahead = reader_head
            .and_then(|(_, start_nsecs, _)| start_nsecs)
            .is_some_and(|start_nsecs| {
                start_nsecs
                    > audio_waterline
                        .resume_timeline_nsecs
                        .saturating_add(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION))
            });
        if !matches!(demux_read_result, DemuxReadResult::WouldBlock) && !reader_head_far_ahead {
            return;
        }
        tracing::debug!(
            session_id = ?context.session_id,
            reason = "rebuffer_audio_resume_low_water",
            read_path,
            result = demux_read_result_name(demux_read_result),
            demux_streams = ?demux_streams,
            audio_stream_index,
            audio_resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
            audio_resume_target_ms = audio_waterline.target_nsecs as f64 / 1_000_000.0,
            audio_reader_head_packet_id = ?reader_head.map(|(packet_id, _, _)| packet_id),
            audio_reader_head_start_nsecs = ?reader_head.and_then(|(_, start, _)| start),
            audio_reader_head_end_nsecs = ?reader_head.and_then(|(_, _, end)| end),
            reader_head_far_ahead,
            pending_audio_forward_ms = ?audio_waterline
                .pending_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            audio_output_pending_ms = ?audio_waterline
                .audio_output_pending_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?audio_waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "FFmpeg demux audio reader head while rebuffer waits for audio"
        );
    }

    fn ordered_demux_streams_for_context(
        &mut self,
        decoder_input: &DecoderInputSnapshot,
        output_snapshot: PlaybackOutputSnapshot,
    ) -> (Vec<c_int>, usize) {
        let audio_low_water = audio_low_water_priority_active(decoder_input);
        let startup_video_low_water = startup_video_low_water_needs_decoder_input(output_snapshot);
        let rebuffer_video_low_water =
            rebuffer_video_low_water_needs_decoder_input(output_snapshot);
        let rebuffer_audio_low_water =
            rebuffer_audio_resume_low_water_priority_active(output_snapshot, decoder_input);
        if startup_video_low_water && decoder_input_accepts_video_packet(decoder_input) {
            let streams = video_priority_demux_streams(
                &decoder_input.demux_streams,
                decoder_input.video_stream_index,
                decoder_input.audio_stream_index,
                decoder_input.subtitle_stream_index,
            );
            log_ordered_demux_streams_for_context(
                "startup_first_video",
                &streams,
                0,
                output_snapshot,
                audio_low_water,
            );
            self.rebuffer_audio_priority_without_audio_progress = 0;
            return (streams, 0);
        }
        if rebuffer_audio_low_water {
            let prefer_video_this_turn = rebuffer_video_low_water
                && decoder_input_accepts_video_packet(decoder_input)
                && self.rebuffer_audio_priority_without_audio_progress >= 2;
            let (reason, streams) = if prefer_video_this_turn {
                (
                    "rebuffer_audio_video_weighted",
                    video_priority_demux_streams(
                        &decoder_input.demux_streams,
                        decoder_input.video_stream_index,
                        decoder_input.audio_stream_index,
                        decoder_input.subtitle_stream_index,
                    ),
                )
            } else {
                (
                    "rebuffer_audio_resume_low_water",
                    audio_priority_demux_streams(
                        &decoder_input.demux_streams,
                        decoder_input.audio_stream_index,
                        decoder_input.video_stream_index,
                        decoder_input.subtitle_stream_index,
                    ),
                )
            };
            log_ordered_demux_streams_for_context(
                reason,
                &streams,
                0,
                output_snapshot,
                audio_low_water,
            );
            return (streams, 0);
        }
        self.rebuffer_audio_priority_without_audio_progress = 0;
        if rebuffer_video_low_water && decoder_input_accepts_video_packet(decoder_input) {
            let streams = video_priority_demux_streams(
                &decoder_input.demux_streams,
                decoder_input.video_stream_index,
                decoder_input.audio_stream_index,
                decoder_input.subtitle_stream_index,
            );
            log_ordered_demux_streams_for_context(
                "rebuffer_video_low_water",
                &streams,
                0,
                output_snapshot,
                audio_low_water,
            );
            return (streams, 0);
        }
        if audio_low_water {
            let streams = audio_priority_demux_streams(
                &decoder_input.demux_streams,
                decoder_input.audio_stream_index,
                decoder_input.video_stream_index,
                decoder_input.subtitle_stream_index,
            );
            log_ordered_demux_streams_for_context(
                "audio_low_water",
                &streams,
                0,
                output_snapshot,
                audio_low_water,
            );
            return (streams, 0);
        }

        let mut demux_streams = decoder_input.demux_streams.clone();
        let demux_stream_rotation = if demux_streams.is_empty() {
            0
        } else {
            self.stream_cursor % demux_streams.len()
        };
        demux_streams.rotate_left(demux_stream_rotation);
        log_ordered_demux_streams_for_context(
            "rotation",
            &demux_streams,
            demux_stream_rotation,
            output_snapshot,
            audio_low_water,
        );
        (demux_streams, demux_stream_rotation)
    }

    pub(super) fn poll_and_admit_packet(
        &mut self,
        mut context: DemuxPacketPumpAdmissionContext<'_>,
    ) -> DemuxPacketPumpResult {
        let started_at = Instant::now();
        let hard_deadline = started_at.checked_add(DEMUX_PACKET_PUMP_HARD_DEADLINE);
        let mut made_progress = false;
        for iteration in 0..DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK {
            if hard_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                let result = if made_progress {
                    DemuxPacketPumpResult::Progress
                } else {
                    DemuxPacketPumpResult::WouldBlock
                };
                self.log_pump_exit_diagnostic(
                    &context,
                    started_at,
                    iteration,
                    "hard_deadline",
                    made_progress,
                    &result,
                );
                return result;
            }
            match self.poll_and_admit_one(&mut context, hard_deadline) {
                DemuxPacketPumpResult::Progress => {
                    made_progress = true;
                    if hard_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        let result = DemuxPacketPumpResult::Progress;
                        self.log_pump_exit_diagnostic(
                            &context,
                            started_at,
                            iteration + 1,
                            "post_progress_hard_deadline",
                            made_progress,
                            &result,
                        );
                        return result;
                    }
                    if started_at.elapsed() >= DEMUX_PACKET_PUMP_MAX_SYNC_DURATION_PER_TICK {
                        let result = DemuxPacketPumpResult::Progress;
                        self.log_pump_exit_diagnostic(
                            &context,
                            started_at,
                            iteration + 1,
                            "sync_budget",
                            made_progress,
                            &result,
                        );
                        return DemuxPacketPumpResult::Progress;
                    }
                }
                DemuxPacketPumpResult::Backpressured => {
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "backpressured",
                        made_progress,
                        &DemuxPacketPumpResult::Backpressured,
                    );
                    return DemuxPacketPumpResult::Backpressured;
                }
                DemuxPacketPumpResult::Eof => {
                    let result = if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::Eof
                    };
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "eof",
                        made_progress,
                        &result,
                    );
                    return result;
                }
                DemuxPacketPumpResult::WouldBlock => {
                    let result = if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::WouldBlock
                    };
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "would_block",
                        made_progress,
                        &result,
                    );
                    return result;
                }
                DemuxPacketPumpResult::Interrupted => {
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "interrupted",
                        made_progress,
                        &DemuxPacketPumpResult::Interrupted,
                    );
                    return DemuxPacketPumpResult::Interrupted;
                }
                DemuxPacketPumpResult::Error(error) => {
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "error",
                        made_progress,
                        &DemuxPacketPumpResult::Error(String::new()),
                    );
                    return DemuxPacketPumpResult::Error(error);
                }
            }
        }
        let result = DemuxPacketPumpResult::Progress;
        self.log_pump_exit_diagnostic(
            &context,
            started_at,
            DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK,
            "packet_limit",
            made_progress,
            &result,
        );
        result
    }

    fn poll_and_admit_one(
        &mut self,
        context: &mut DemuxPacketPumpAdmissionContext<'_>,
        hard_deadline: Option<Instant>,
    ) -> DemuxPacketPumpResult {
        let decoder_input = context
            .pipeline
            .decoder_input_snapshot(context.video_admission_pressure.output_resource_pressure);
        self.request_rebuffer_audio_reader_head_realign_if_needed(context, &decoder_input);
        let rebuffer_audio_priority_active = rebuffer_audio_resume_low_water_priority_active(
            context.video_admission_pressure.output_snapshot,
            &decoder_input,
        );
        let demux_read_result = self.poll_packet(DemuxPacketPumpContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            decoder_input: &decoder_input,
            video_admission_pressure: context.video_admission_pressure,
            should_wait_for_demux: context.should_wait_for_demux,
            video_output_waiting_for_demux: context.video_output_waiting_for_demux,
            cached_reader_watermark: context.demux_cache.cached_reader_watermark(),
            hard_deadline,
        });
        let mut packet = match demux_read_result {
            DemuxReadResult::Packet(packet) => packet,
            DemuxReadResult::Eof => {
                let demux_packet_snapshot = context.demux_cache.packet_queue_snapshot();
                let blocked_cached_streams =
                    eof_cached_backpressured_streams(&decoder_input, &demux_packet_snapshot);
                if !blocked_cached_streams.is_empty() {
                    tracing::debug!(
                        session_id = ?context.session_id,
                        blocked_cached_streams = ?blocked_cached_streams,
                        demux_streams = ?decoder_input.demux_streams,
                        demux_packet_queued = demux_packet_snapshot.total_packets,
                        demux_packet_bytes = demux_packet_snapshot.total_bytes,
                        demux_packet_streams = ?demux_packet_snapshot.streams,
                        video_decode_blocked_on = ?decoder_input
                            .video_decode_blocked_on
                            .map(PlaybackBlockReason::as_str),
                        "deferring FFmpeg demux EOF while selected streams remain backpressured"
                    );
                    return DemuxPacketPumpResult::Backpressured;
                }
                return DemuxPacketPumpResult::Eof;
            }
            DemuxReadResult::WouldBlock => {
                self.record_rebuffer_audio_priority_result(
                    rebuffer_audio_priority_active,
                    false,
                    false,
                );
                return DemuxPacketPumpResult::WouldBlock;
            }
            DemuxReadResult::Interrupted => {
                self.record_rebuffer_audio_priority_result(false, false, false);
                return DemuxPacketPumpResult::Interrupted;
            }
            DemuxReadResult::Error(error) => return DemuxPacketPumpResult::Error(error),
        };

        let route = self.route_packet(
            &packet,
            decoder_input.video_stream_index,
            decoder_input.audio_stream_index,
            decoder_input.subtitle_stream_index,
        );
        let process_result: std::result::Result<DecodePacketAdmissionStatus, String> = match route {
            DemuxPacketRoute::Video => context.pipeline.admit_video_demux_packet(
                &packet,
                context.session_id,
                context.video_admission_pressure,
            ),
            DemuxPacketRoute::Audio => context
                .pipeline
                .admit_audio_demux_packet(&packet, context.session_id),
            DemuxPacketRoute::Subtitle => context
                .pipeline
                .admit_subtitle_demux_packet(&packet, context.session_id),
            DemuxPacketRoute::Other => Ok(DecodePacketAdmissionStatus::Dropped),
        };
        packet.unref();

        let audio_admitted = route == DemuxPacketRoute::Audio
            && matches!(&process_result, Ok(DecodePacketAdmissionStatus::Queued));
        let weighted_video_admitted = route == DemuxPacketRoute::Video
            && matches!(&process_result, Ok(status) if !(*status).backpressured());
        self.record_rebuffer_audio_priority_result(
            rebuffer_audio_priority_active,
            audio_admitted,
            weighted_video_admitted,
        );
        match process_result {
            Ok(status) if status.backpressured() => DemuxPacketPumpResult::Backpressured,
            Ok(_) => DemuxPacketPumpResult::Progress,
            Err(error) => DemuxPacketPumpResult::Error(error),
        }
    }

    fn request_rebuffer_audio_reader_head_realign_if_needed(
        &self,
        context: &mut DemuxPacketPumpAdmissionContext<'_>,
        decoder_input: &DecoderInputSnapshot,
    ) {
        let Some(audio_stream_index) = decoder_input.audio_stream_index else {
            return;
        };
        let Some(audio_waterline) = decoder_input.audio_resume_waterline else {
            return;
        };
        let Some((_, Some(reader_head_start_nsecs), _)) = context
            .demux_cache
            .stream_reader_head_timeline(audio_stream_index)
        else {
            return;
        };

        let current_start_position_nsecs = context.pipeline.current_start_position_nsecs;
        context
            .pipeline
            .output_scheduler
            .request_rebuffer_audio_reader_head_realign_if_needed(
                reader_head_start_nsecs,
                audio_waterline,
                current_start_position_nsecs,
                context.session_id,
            );
    }

    fn record_rebuffer_audio_priority_result(
        &mut self,
        active: bool,
        audio_admitted: bool,
        weighted_video_admitted: bool,
    ) {
        if !active || audio_admitted || weighted_video_admitted {
            self.rebuffer_audio_priority_without_audio_progress = 0;
            return;
        }
        self.rebuffer_audio_priority_without_audio_progress = self
            .rebuffer_audio_priority_without_audio_progress
            .saturating_add(1);
    }

    fn route_packet(
        &self,
        packet: &AvPacket,
        video_stream_index: c_int,
        audio_stream_index: Option<c_int>,
        subtitle_stream_index: Option<c_int>,
    ) -> DemuxPacketRoute {
        let stream_index = packet.stream_index();
        if stream_index == video_stream_index {
            DemuxPacketRoute::Video
        } else if audio_stream_index == Some(stream_index) {
            DemuxPacketRoute::Audio
        } else if subtitle_stream_index == Some(stream_index) {
            DemuxPacketRoute::Subtitle
        } else {
            DemuxPacketRoute::Other
        }
    }

    fn log_wait(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_read_elapsed: Duration,
        demux_cache_timing: DemuxPacketCacheReadTiming,
        demux_read_result: &DemuxReadResult,
    ) {
        let result = match demux_read_result {
            DemuxReadResult::Packet(_) => "packet",
            DemuxReadResult::Eof => "eof",
            DemuxReadResult::WouldBlock => "would_block",
            DemuxReadResult::Interrupted => "interrupted",
            DemuxReadResult::Error(_) => "error",
        };
        let demux_packet_snapshot = context.demux_cache.packet_queue_snapshot();
        let demux_packet_queue_full = demux_packet_snapshot.prefetch_queue_full()
            && !demux_packet_snapshot.consumer_drainable();
        let video_decode_snapshot = context.decoder_input.video_decode_snapshot;
        let video_decode_blocked_on = context.decoder_input.video_decode_blocked_on;
        let blocked_on = if matches!(
            video_decode_blocked_on,
            Some(
                PlaybackBlockReason::PacketQueueFull
                    | PlaybackBlockReason::DecoderInFlight
                    | PlaybackBlockReason::DecoderOutputPending
                    | PlaybackBlockReason::DecodedVideoQueue
                    | PlaybackBlockReason::DecodedQueueFull
                    | PlaybackBlockReason::HwSurfacePool
            )
        ) {
            video_decode_blocked_on.expect("video decode block reason checked above")
        } else if demux_packet_queue_full {
            PlaybackBlockReason::PacketQueueFull
        } else if context.should_wait_for_demux
            || matches!(demux_read_result, DemuxReadResult::WouldBlock)
        {
            video_decode_blocked_on.unwrap_or(PlaybackBlockReason::DemuxCache)
        } else {
            PlaybackBlockReason::OutputGate
        };
        tracing::debug!(
            session_id = ?context.session_id,
            blocked_on = blocked_on.as_str(),
            waited_ms = demux_read_elapsed.as_secs_f64() * 1000.0,
            cache_lock_wait_ms = demux_cache_timing.lock_wait.as_secs_f64() * 1000.0,
            cache_try_lock_failures = demux_cache_timing.try_lock_failures,
            cache_lock_timed_out = demux_cache_timing.lock_timed_out,
            cache_data_wait_ms = demux_cache_timing.data_wait.as_secs_f64() * 1000.0,
            cache_data_waits = demux_cache_timing.data_waits,
            cache_take_packet_ms = demux_cache_timing.take_packet.as_secs_f64() * 1000.0,
            cache_advance_reader_head_ms =
                demux_cache_timing.advance_reader_head.as_secs_f64() * 1000.0,
            cache_refresh_reader_tracking_ms =
                demux_cache_timing.refresh_reader_tracking.as_secs_f64() * 1000.0,
            cache_trim_ms = demux_cache_timing.trim.as_secs_f64() * 1000.0,
            cache_forward_bytes_ms = demux_cache_timing.forward_bytes.as_secs_f64() * 1000.0,
            cache_forward_window_ms = demux_cache_timing.forward_window.as_secs_f64() * 1000.0,
            packet_ref_ms = demux_cache_timing.packet_ref.as_secs_f64() * 1000.0,
            disk_read_ms = demux_cache_timing.disk_read.as_secs_f64() * 1000.0,
            disk_reads = demux_cache_timing.disk_reads,
            result,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
            should_wait_for_demux = context.should_wait_for_demux,
            demux_streams = ?demux_streams,
            demux_packet_queued = demux_packet_snapshot.total_packets,
            demux_packet_bytes = demux_packet_snapshot.total_bytes,
            demux_packet_queue_full,
            demux_packet_streams = ?demux_packet_snapshot.streams,
            video_decode_state = ?video_decode_snapshot.state,
            video_decode_queued_frames = video_decode_snapshot.queued_frames,
            video_decode_queue_capacity = video_decode_snapshot.queue_capacity,
            video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
            video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
            video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
            video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            "FFmpeg demux packet read wait completed"
        );
    }

    fn trace_timing(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_read_elapsed: Duration,
        demux_cache_timing: DemuxPacketCacheReadTiming,
        demux_read_result: &DemuxReadResult,
    ) {
        tracing::trace!(
            session_id = ?context.session_id,
            result = demux_read_result_name(demux_read_result),
            total_ms = demux_read_elapsed.as_secs_f64() * 1000.0,
            cache_lock_wait_ms = demux_cache_timing.lock_wait.as_secs_f64() * 1000.0,
            cache_try_lock_failures = demux_cache_timing.try_lock_failures,
            cache_lock_timed_out = demux_cache_timing.lock_timed_out,
            cache_data_wait_ms = demux_cache_timing.data_wait.as_secs_f64() * 1000.0,
            cache_data_waits = demux_cache_timing.data_waits,
            cache_take_packet_ms = demux_cache_timing.take_packet.as_secs_f64() * 1000.0,
            cache_advance_reader_head_ms =
                demux_cache_timing.advance_reader_head.as_secs_f64() * 1000.0,
            cache_refresh_reader_tracking_ms =
                demux_cache_timing.refresh_reader_tracking.as_secs_f64() * 1000.0,
            cache_trim_ms = demux_cache_timing.trim.as_secs_f64() * 1000.0,
            cache_forward_bytes_ms = demux_cache_timing.forward_bytes.as_secs_f64() * 1000.0,
            cache_forward_window_ms = demux_cache_timing.forward_window.as_secs_f64() * 1000.0,
            packet_ref_ms = demux_cache_timing.packet_ref.as_secs_f64() * 1000.0,
            disk_read_ms = demux_cache_timing.disk_read.as_secs_f64() * 1000.0,
            disk_reads = demux_cache_timing.disk_reads,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
            should_wait_for_demux = context.should_wait_for_demux,
            demux_streams = ?demux_streams,
            "FFmpeg demux packet pump timing"
        );
    }

    fn should_log_timing(&mut self, timing: DemuxPacketCacheReadTiming) -> bool {
        if !timing.lock_timed_out
            && timing.try_lock_failures == 0
            && timing.lock_wait < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.data_wait < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.take_packet < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.advance_reader_head < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.refresh_reader_tracking < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.trim < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.forward_bytes < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.forward_window < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.packet_ref < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
            && timing.disk_read < DEMUX_CACHE_LOCK_TIMING_LOG_AFTER
        {
            return false;
        }
        let now = Instant::now();
        if self.last_timing_log_at.is_some_and(|last| {
            now.saturating_duration_since(last) < DEMUX_PUMP_TIMING_LOG_INTERVAL
        }) {
            return false;
        }
        self.last_timing_log_at = Some(now);
        true
    }

    fn log_timing(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_read_elapsed: Duration,
        demux_cache_timing: DemuxPacketCacheReadTiming,
        demux_read_result: &DemuxReadResult,
    ) {
        tracing::debug!(
            session_id = ?context.session_id,
            result = demux_read_result_name(demux_read_result),
            total_ms = demux_read_elapsed.as_secs_f64() * 1000.0,
            cache_lock_wait_ms = demux_cache_timing.lock_wait.as_secs_f64() * 1000.0,
            cache_try_lock_failures = demux_cache_timing.try_lock_failures,
            cache_lock_timed_out = demux_cache_timing.lock_timed_out,
            cache_data_wait_ms = demux_cache_timing.data_wait.as_secs_f64() * 1000.0,
            cache_data_waits = demux_cache_timing.data_waits,
            cache_take_packet_ms = demux_cache_timing.take_packet.as_secs_f64() * 1000.0,
            cache_advance_reader_head_ms =
                demux_cache_timing.advance_reader_head.as_secs_f64() * 1000.0,
            cache_refresh_reader_tracking_ms =
                demux_cache_timing.refresh_reader_tracking.as_secs_f64() * 1000.0,
            cache_trim_ms = demux_cache_timing.trim.as_secs_f64() * 1000.0,
            cache_forward_bytes_ms = demux_cache_timing.forward_bytes.as_secs_f64() * 1000.0,
            cache_forward_window_ms = demux_cache_timing.forward_window.as_secs_f64() * 1000.0,
            packet_ref_ms = demux_cache_timing.packet_ref.as_secs_f64() * 1000.0,
            disk_read_ms = demux_cache_timing.disk_read.as_secs_f64() * 1000.0,
            disk_reads = demux_cache_timing.disk_reads,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
            should_wait_for_demux = context.should_wait_for_demux,
            demux_streams = ?demux_streams,
            "FFmpeg demux packet pump waited for cache lock/data"
        );
    }

    fn log_pump_exit_diagnostic(
        &self,
        context: &DemuxPacketPumpAdmissionContext<'_>,
        started_at: Instant,
        iterations: usize,
        terminal_result: &'static str,
        made_progress: bool,
        returned_result: &DemuxPacketPumpResult,
    ) {
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        let empty_startup_or_rebuffer = output_snapshot.queued_video_frames == 0
            && (output_snapshot.first_video_frame_pending || output_snapshot.rebuffering);
        let terminal_would_block = terminal_result == "would_block"
            || matches!(returned_result, DemuxPacketPumpResult::WouldBlock);
        if !empty_startup_or_rebuffer || !terminal_would_block {
            return;
        }

        let elapsed_before_diagnostic = started_at.elapsed();
        let diagnostic_snapshot_started_at = Instant::now();
        let demux_watermark = context.demux_cache.cached_reader_watermark();
        let demux_packet_snapshot = context.demux_cache.packet_queue_snapshot();
        let diagnostic_snapshot_wait = diagnostic_snapshot_started_at.elapsed();
        let decoder_input = context
            .pipeline
            .decoder_input_snapshot(context.video_admission_pressure.output_resource_pressure);
        let video_decode_snapshot = decoder_input.video_decode_snapshot;
        tracing::debug!(
            session_id = ?context.session_id,
            terminal_result,
            returned_result = demux_packet_pump_result_name(returned_result),
            made_progress,
            iterations,
            elapsed_ms = started_at.elapsed().as_secs_f64() * 1000.0,
            elapsed_before_diagnostic_ms =
                elapsed_before_diagnostic.as_secs_f64() * 1000.0,
            diagnostic_snapshot_wait_ms = diagnostic_snapshot_wait.as_secs_f64() * 1000.0,
            should_wait_for_demux = context.should_wait_for_demux,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
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
            video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            "FFmpeg demux packet pump returned after empty-output would-block"
        );
    }
}

fn demux_read_result_name(result: &DemuxReadResult) -> &'static str {
    match result {
        DemuxReadResult::Packet(_) => "packet",
        DemuxReadResult::Eof => "eof",
        DemuxReadResult::WouldBlock => "would_block",
        DemuxReadResult::Interrupted => "interrupted",
        DemuxReadResult::Error(_) => "error",
    }
}

fn demux_packet_pump_result_name(result: &DemuxPacketPumpResult) -> &'static str {
    match result {
        DemuxPacketPumpResult::Progress => "progress",
        DemuxPacketPumpResult::Backpressured => "backpressured",
        DemuxPacketPumpResult::Eof => "eof",
        DemuxPacketPumpResult::WouldBlock => "would_block",
        DemuxPacketPumpResult::Interrupted => "interrupted",
        DemuxPacketPumpResult::Error(_) => "error",
    }
}

fn force_consumer_drain_retry_wait(context: &DemuxPacketPumpContext<'_>) -> Option<Duration> {
    let wait = if let Some(deadline) = context.hard_deadline {
        deadline
            .checked_duration_since(Instant::now())
            .map(|remaining| remaining.min(DEMUX_PACKET_FORCE_CONSUMER_DRAIN_RETRY_WAIT))?
    } else {
        DEMUX_PACKET_FORCE_CONSUMER_DRAIN_RETRY_WAIT
    };
    (!wait.is_zero()).then_some(wait)
}

fn combine_demux_read_timing(
    first: DemuxPacketCacheReadTiming,
    second: DemuxPacketCacheReadTiming,
) -> DemuxPacketCacheReadTiming {
    DemuxPacketCacheReadTiming {
        lock_wait: first.lock_wait + second.lock_wait,
        try_lock_failures: first
            .try_lock_failures
            .saturating_add(second.try_lock_failures),
        lock_timed_out: first.lock_timed_out || second.lock_timed_out,
        data_wait: first.data_wait + second.data_wait,
        data_waits: first.data_waits.saturating_add(second.data_waits),
        take_packet: first.take_packet + second.take_packet,
        advance_reader_head: first.advance_reader_head + second.advance_reader_head,
        refresh_reader_tracking: first.refresh_reader_tracking + second.refresh_reader_tracking,
        trim: first.trim + second.trim,
        forward_bytes: first.forward_bytes + second.forward_bytes,
        forward_window: first.forward_window + second.forward_window,
        packet_ref: first.packet_ref + second.packet_ref,
        disk_read: first.disk_read + second.disk_read,
        disk_reads: first.disk_reads.saturating_add(second.disk_reads),
    }
}

fn eof_cached_backpressured_streams(
    decoder_input: &DecoderInputSnapshot,
    demux_packet_snapshot: &DemuxPacketQueueSnapshot,
) -> Vec<c_int> {
    selected_decoder_streams(decoder_input)
        .into_iter()
        .flatten()
        .filter(|stream_index| !decoder_input.demux_streams.contains(stream_index))
        .filter(|stream_index| {
            demux_stream_has_cached_packets(demux_packet_snapshot, *stream_index)
        })
        .collect()
}

fn selected_decoder_streams(decoder_input: &DecoderInputSnapshot) -> [Option<c_int>; 3] {
    [
        Some(decoder_input.video_stream_index),
        decoder_input.audio_stream_index,
        decoder_input.subtitle_stream_index,
    ]
}

fn demux_stream_has_cached_packets(
    demux_packet_snapshot: &DemuxPacketQueueSnapshot,
    stream_index: c_int,
) -> bool {
    demux_packet_snapshot
        .streams
        .iter()
        .any(|stream| stream.stream_index == stream_index && stream.queued_packets > 0)
}

#[cfg(test)]
fn demux_pump_should_wait_for_cache_lock(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
) -> bool {
    demux_pump_cache_lock_wait(
        should_wait_for_demux,
        video_output_waiting_for_demux,
        decoder_input,
        video_admission_pressure,
        DemuxReaderWatermark::default(),
    )
    .is_some()
}

#[cfg(test)]
fn demux_pump_cache_lock_wait_for_test(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
) -> Option<Duration> {
    demux_pump_cache_lock_wait(
        should_wait_for_demux,
        video_output_waiting_for_demux,
        decoder_input,
        video_admission_pressure,
        DemuxReaderWatermark::default(),
    )
}

#[cfg(test)]
fn demux_pump_cache_lock_wait_with_watermark_for_test(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    cached_reader_watermark: DemuxReaderWatermark,
) -> Option<Duration> {
    demux_pump_cache_lock_wait(
        should_wait_for_demux,
        video_output_waiting_for_demux,
        decoder_input,
        video_admission_pressure,
        cached_reader_watermark,
    )
}

fn demux_pump_cache_lock_wait(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    cached_reader_watermark: DemuxReaderWatermark,
) -> Option<Duration> {
    if decoder_input.demux_streams.is_empty() || !decoder_input_accepts_video_packet(decoder_input)
    {
        return demux_pump_audio_cache_lock_wait(
            should_wait_for_demux,
            video_output_waiting_for_demux,
            decoder_input,
            video_admission_pressure,
            cached_reader_watermark,
        );
    }
    if should_wait_for_demux
        || video_output_waiting_for_demux
        || output_queue_low_water_needs_decoder_input(video_admission_pressure.output_snapshot)
    {
        return Some(demux_cache_lock_wait_for_watermark(
            DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT,
            cached_reader_watermark,
        ));
    }
    if output_queue_needs_decoder_input(video_admission_pressure.output_snapshot) {
        return Some(demux_cache_lock_wait_for_watermark(
            DEMUX_PACKET_CACHE_LOCK_WAIT,
            cached_reader_watermark,
        ));
    }
    if video_decoder_has_pending_work(decoder_input.video_decode_snapshot) {
        return None;
    }
    None
}

fn demux_cache_pause_signal(context: &DemuxPacketPumpContext<'_>) -> bool {
    context.video_output_waiting_for_demux
        || context
            .video_admission_pressure
            .output_snapshot
            .video_output_low_water
        || startup_video_low_water_needs_decoder_input(
            context.video_admission_pressure.output_snapshot,
        )
        || rebuffer_video_low_water_needs_decoder_input(
            context.video_admission_pressure.output_snapshot,
        )
        || context.video_admission_pressure.output_snapshot.rebuffering
}

fn demux_pump_audio_cache_lock_wait(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    cached_reader_watermark: DemuxReaderWatermark,
) -> Option<Duration> {
    if !decoder_input_accepts_audio_packet(decoder_input) {
        return None;
    }
    if should_wait_for_demux
        || video_output_waiting_for_demux
        || output_queue_low_water_needs_decoder_input(video_admission_pressure.output_snapshot)
    {
        return Some(demux_cache_lock_wait_for_watermark(
            DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT,
            cached_reader_watermark,
        ));
    }
    if output_queue_needs_decoder_input(video_admission_pressure.output_snapshot) {
        return Some(demux_cache_lock_wait_for_watermark(
            DEMUX_PACKET_CACHE_LOCK_WAIT,
            cached_reader_watermark,
        ));
    }
    None
}

fn demux_cache_lock_wait_for_watermark(
    requested_wait: Duration,
    cached_reader_watermark: DemuxReaderWatermark,
) -> Duration {
    if cached_reader_watermark.underrun || cached_reader_watermark.idle {
        return requested_wait;
    }
    if cached_reader_watermark
        .selected_min_forward_nsecs
        .is_some_and(|forward| forward > DEMUX_PACKET_CACHE_READY_FORWARD_NSECS)
    {
        return requested_wait.min(DEMUX_PACKET_CACHE_READY_LOCK_WAIT);
    }
    requested_wait
}

fn output_queue_low_water_needs_decoder_input(output_snapshot: PlaybackOutputSnapshot) -> bool {
    if output_snapshot.first_video_frame_pending {
        return startup_video_low_water_needs_decoder_input(output_snapshot);
    }
    if rebuffer_video_low_water_needs_decoder_input(output_snapshot) {
        return true;
    }
    output_snapshot.video_output_low_water || output_snapshot.queued_video_frames == 0
}

fn output_queue_needs_decoder_input(output_snapshot: PlaybackOutputSnapshot) -> bool {
    if output_snapshot.first_video_frame_pending || output_snapshot.rebuffering {
        return false;
    }
    if output_snapshot.queued_video_frames == 0 {
        return true;
    }
    let queued_forward_nsecs = output_snapshot
        .queued_video_forward_nsecs
        .unwrap_or(output_snapshot.queued_video_duration_nsecs);
    queued_forward_nsecs <= duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION)
}

fn decoder_input_accepts_video_packet(decoder_input: &DecoderInputSnapshot) -> bool {
    decoder_input
        .demux_streams
        .contains(&decoder_input.video_stream_index)
}

fn decoder_input_accepts_audio_packet(decoder_input: &DecoderInputSnapshot) -> bool {
    decoder_input
        .audio_stream_index
        .is_some_and(|stream_index| decoder_input.demux_streams.contains(&stream_index))
}

fn decoder_input_waiting_for_packets(decoder_input: &DecoderInputSnapshot) -> bool {
    matches!(
        decoder_input.video_decode_blocked_on,
        Some(PlaybackBlockReason::DecoderInputEmpty)
    ) || audio_low_water_priority_active(decoder_input)
}

fn audio_low_water_priority_active(decoder_input: &DecoderInputSnapshot) -> bool {
    decoder_input
        .audio_resume_waterline
        .is_some_and(|waterline| waterline.below_target())
        && decoder_input_accepts_audio_packet(decoder_input)
}

fn rebuffer_audio_resume_low_water_priority_active(
    output_snapshot: PlaybackOutputSnapshot,
    decoder_input: &DecoderInputSnapshot,
) -> bool {
    if !output_snapshot.rebuffer_empty_audio_output_blocked
        || !decoder_input_accepts_audio_packet(decoder_input)
    {
        return false;
    }
    let Some(waterline) = decoder_input.audio_resume_waterline else {
        return false;
    };
    waterline.below_target() && rebuffer_output_has_recoverable_video(output_snapshot)
}

fn rebuffer_output_has_recoverable_video(output_snapshot: PlaybackOutputSnapshot) -> bool {
    let contiguous_forward_nsecs = output_snapshot
        .queued_video_contiguous_forward_nsecs
        .or(output_snapshot.queued_video_forward_nsecs)
        .unwrap_or(output_snapshot.queued_video_duration_nsecs);
    output_snapshot.queued_video_range_nsecs.is_some()
        || output_snapshot.queued_video_frames >= 2
        || contiguous_forward_nsecs >= 80_000_000
}

fn audio_priority_demux_streams(
    demux_streams: &[c_int],
    audio_stream_index: Option<c_int>,
    video_stream_index: c_int,
    subtitle_stream_index: Option<c_int>,
) -> Vec<c_int> {
    let mut ordered = Vec::with_capacity(demux_streams.len());
    for stream_index in [
        audio_stream_index,
        Some(video_stream_index),
        subtitle_stream_index,
    ]
    .into_iter()
    .flatten()
    {
        if demux_streams.contains(&stream_index) && !ordered.contains(&stream_index) {
            ordered.push(stream_index);
        }
    }
    for stream_index in demux_streams {
        if !ordered.contains(stream_index) {
            ordered.push(*stream_index);
        }
    }
    ordered
}

fn video_priority_demux_streams(
    demux_streams: &[c_int],
    video_stream_index: c_int,
    audio_stream_index: Option<c_int>,
    subtitle_stream_index: Option<c_int>,
) -> Vec<c_int> {
    let mut ordered = Vec::with_capacity(demux_streams.len());
    for stream_index in [
        Some(video_stream_index),
        audio_stream_index,
        subtitle_stream_index,
    ]
    .into_iter()
    .flatten()
    {
        if demux_streams.contains(&stream_index) && !ordered.contains(&stream_index) {
            ordered.push(stream_index);
        }
    }
    for stream_index in demux_streams {
        if !ordered.contains(stream_index) {
            ordered.push(*stream_index);
        }
    }
    ordered
}

fn startup_video_low_water_needs_decoder_input(output_snapshot: PlaybackOutputSnapshot) -> bool {
    output_snapshot.first_video_frame_pending
        && output_snapshot.queued_video_duration_nsecs
            < duration_nsecs(VIDEO_OUTPUT_START_PREBUFFER_DURATION)
}

fn rebuffer_video_low_water_needs_decoder_input(output_snapshot: PlaybackOutputSnapshot) -> bool {
    output_snapshot.rebuffering
        && output_snapshot
            .queued_video_contiguous_forward_nsecs
            .unwrap_or_default()
            < duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
}

fn log_ordered_demux_streams_for_context(
    reason: &'static str,
    streams: &[c_int],
    rotation: usize,
    output_snapshot: PlaybackOutputSnapshot,
    audio_low_water: bool,
) {
    tracing::trace!(
        reason,
        streams = ?streams,
        rotation,
        first_video_frame_pending = output_snapshot.first_video_frame_pending,
        rebuffering = output_snapshot.rebuffering,
        rebuffer_empty_audio_output_blocked =
            output_snapshot.rebuffer_empty_audio_output_blocked,
        video_bootstrap_after_seek = output_snapshot.video_bootstrap_after_seek,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_bootstrap_forward_ms =
            output_snapshot.queued_video_bootstrap_forward_nsecs() as f64 / 1_000_000.0,
        audio_low_water,
        "ordered FFmpeg demux streams for decoder input"
    );
}

fn video_decoder_has_pending_work(snapshot: VideoDecodeWorkerSnapshot) -> bool {
    snapshot.queued_frames > 0
        || snapshot.pending_input_packets > 0
        || snapshot.in_flight_packets > 0
        || snapshot.completed_packets > 0
        || matches!(
            snapshot.state,
            VideoDecodeWorkerState::Decoding
                | VideoDecodeWorkerState::HaveFrame
                | VideoDecodeWorkerState::OutputFull
                | VideoDecodeWorkerState::Draining
                | VideoDecodeWorkerState::Recovering
        )
}

#[cfg(test)]
mod tests {
    use crate::player::backend::{
        StreamCacheKind,
        ffmpeg::playback_loop::{
            demux_cache::{DemuxPacketQueueSnapshot, DemuxStreamPacketQueueSnapshot},
            video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState},
        },
    };

    use super::super::output_rebuffer::PlaybackOutputState;
    use super::super::playback_pipeline_state::DecoderInputSnapshot;
    use super::super::{
        AudioResumeWaterline, DemuxReaderWatermark, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
        VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    };
    use super::{
        AUDIO_VIDEO_QUEUE_LIMIT_DURATION, AvPacket, DEMUX_PACKET_CACHE_LOCK_WAIT,
        DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT, DEMUX_PACKET_CACHE_READY_LOCK_WAIT,
        DemuxPacketPump, DemuxPacketRoute, PlaybackBlockReason, PlaybackOutputSnapshot,
        VideoPacketAdmissionPressure, audio_priority_demux_streams,
        demux_pump_cache_lock_wait_for_test, demux_pump_cache_lock_wait_with_watermark_for_test,
        demux_pump_should_wait_for_cache_lock, duration_nsecs, eof_cached_backpressured_streams,
    };
    use std::os::raw::c_int;
    use std::time::Duration;

    fn packet_for_stream(stream_index: c_int) -> AvPacket {
        let mut packet = AvPacket::new().expect("packet allocates");
        unsafe {
            (*packet.as_mut_ptr()).stream_index = stream_index;
        }
        packet
    }

    #[test]
    fn demux_packet_pump_routes_packets_by_selected_streams() {
        let pump = DemuxPacketPump::default();

        assert_eq!(
            pump.route_packet(&packet_for_stream(10), 10, Some(11), Some(12)),
            DemuxPacketRoute::Video
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(11), 10, Some(11), Some(12)),
            DemuxPacketRoute::Audio
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(12), 10, Some(11), Some(12)),
            DemuxPacketRoute::Subtitle
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(13), 10, Some(11), Some(12)),
            DemuxPacketRoute::Other
        );
    }

    #[test]
    fn demux_packet_pump_treats_unselected_audio_subtitle_as_other() {
        let pump = DemuxPacketPump::default();

        assert_eq!(
            pump.route_packet(&packet_for_stream(11), 10, None, Some(12)),
            DemuxPacketRoute::Other
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(12), 10, Some(11), None),
            DemuxPacketRoute::Other
        );
    }

    #[test]
    fn demux_packet_pump_only_waits_for_cache_lock_when_output_waits_for_demux() {
        let decoder_input = decoder_input_snapshot(vec![0], 0, None, None);
        let pressure = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                10,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert!(!demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            pressure
        ));
        assert!(demux_pump_should_wait_for_cache_lock(
            true,
            false,
            &decoder_input,
            pressure
        ));
        assert!(demux_pump_should_wait_for_cache_lock(
            false,
            true,
            &decoder_input,
            pressure
        ));
    }

    #[test]
    fn demux_packet_pump_waits_for_cache_lock_when_output_window_needs_packets() {
        let decoder_input = decoder_input_snapshot(vec![0], 0, None, None);
        let full_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                10,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };
        let draining_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                9,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION),
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };
        let empty_decoder_input = decoder_input_snapshot(Vec::new(), 0, None, None);

        assert!(!demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            full_output
        ));
        assert!(demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            draining_output
        ));
        assert!(!demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &empty_decoder_input,
            draining_output
        ));
        assert_eq!(
            demux_pump_cache_lock_wait_for_test(false, false, &decoder_input, draining_output),
            Some(DEMUX_PACKET_CACHE_LOCK_WAIT)
        );
    }

    #[test]
    fn demux_packet_pump_waits_for_cache_lock_for_audio_only_input_when_output_needs_packets() {
        let decoder_input = decoder_input_snapshot(vec![2], 0, Some(2), None);
        let draining_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                3,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert!(demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            draining_output
        ));
        assert_eq!(
            demux_pump_cache_lock_wait_for_test(false, false, &decoder_input, draining_output),
            Some(DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT)
        );
    }

    #[test]
    fn demux_packet_pump_does_not_wait_for_audio_only_input_when_output_has_headroom() {
        let decoder_input = decoder_input_snapshot(vec![2], 0, Some(2), None);
        let full_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                10,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert!(!demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            full_output
        ));
    }

    #[test]
    fn demux_packet_pump_waits_for_playback_need_when_video_decoder_has_pending_work() {
        let mut decoder_input = decoder_input_snapshot(vec![0], 0, None, None);
        decoder_input.video_decode_snapshot.state = VideoDecodeWorkerState::HaveFrame;
        decoder_input.video_decode_snapshot.queued_frames = 7;
        let full_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                12,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };
        let draining_output = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                3,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert!(!demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            full_output
        ));
        assert!(demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            draining_output
        ));
        assert!(demux_pump_should_wait_for_cache_lock(
            true,
            false,
            &decoder_input,
            draining_output
        ));
    }

    #[test]
    fn demux_packet_pump_waits_for_cache_lock_before_video_headroom_is_low() {
        let decoder_input = decoder_input_snapshot(vec![0], 0, None, None);
        let one_second_headroom = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                12,
                duration_nsecs(Duration::from_millis(1_200)),
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert!(demux_pump_should_wait_for_cache_lock(
            false,
            false,
            &decoder_input,
            one_second_headroom
        ));
    }

    #[test]
    fn demux_packet_pump_prioritizes_audio_when_resume_audio_is_low_water() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![0, 1, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let (streams, rotation) = pump.ordered_demux_streams_for_context(
            &decoder_input,
            playback_output_snapshot_for_test(1, 40_000_000),
        );

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![1, 0, 2]);
    }

    #[test]
    fn demux_packet_pump_prioritizes_startup_video_before_audio_low_water() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let (streams, rotation) = pump.ordered_demux_streams_for_context(
            &decoder_input,
            startup_output_snapshot_for_test(40_000_000),
        );

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![0, 1, 2]);
    }

    #[test]
    fn demux_packet_pump_prioritizes_startup_video_after_audio_waterline_is_ready() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 2, 0], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(true));

        let (streams, rotation) = pump.ordered_demux_streams_for_context(
            &decoder_input,
            startup_output_snapshot_for_test(40_000_000),
        );

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![0, 1, 2]);
    }

    #[test]
    fn demux_packet_pump_prioritizes_rebuffer_video_low_water_before_audio_low_water() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let mut output_snapshot = playback_output_snapshot_for_test(
            1,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
        );
        output_snapshot.state = PlaybackOutputState::Rebuffering;
        output_snapshot.rebuffering = true;

        let (streams, rotation) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![0, 1, 2]);
    }

    #[test]
    fn demux_packet_pump_prioritizes_rebuffer_audio_when_gate_waits_audio() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let mut output_snapshot = playback_output_snapshot_for_test(
            1,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
        );
        output_snapshot.state = PlaybackOutputState::Rebuffering;
        output_snapshot.rebuffering = true;
        output_snapshot.pending_start_audio_frames = 4;
        output_snapshot.pending_start_audio_nsecs = 200_000_000;
        output_snapshot.rebuffer_empty_audio_output_blocked = true;

        let (streams, rotation) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![1, 0, 2]);
    }

    #[test]
    fn demux_packet_pump_prioritizes_rebuffer_audio_prefill_without_pending_audio() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let mut output_snapshot = playback_output_snapshot_for_test(
            1,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
        );
        output_snapshot.state = PlaybackOutputState::Rebuffering;
        output_snapshot.rebuffering = true;
        output_snapshot.rebuffer_empty_audio_output_blocked = true;

        let (streams, rotation) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![1, 0, 2]);
    }

    #[test]
    fn demux_packet_pump_rotates_video_after_rebuffer_audio_makes_no_progress() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let mut output_snapshot = playback_output_snapshot_for_test(
            1,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION) - 1,
        );
        output_snapshot.state = PlaybackOutputState::Rebuffering;
        output_snapshot.rebuffering = true;
        output_snapshot.pending_start_audio_frames = 4;
        output_snapshot.pending_start_audio_nsecs = 200_000_000;
        output_snapshot.rebuffer_empty_audio_output_blocked = true;

        let (first_streams, _) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);
        pump.record_rebuffer_audio_priority_result(true, false, false);
        pump.record_rebuffer_audio_priority_result(true, false, false);
        let (second_streams, _) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);
        pump.record_rebuffer_audio_priority_result(true, false, true);
        let (third_streams, _) =
            pump.ordered_demux_streams_for_context(&decoder_input, output_snapshot);

        assert_eq!(first_streams, vec![1, 0, 2]);
        assert_eq!(second_streams, vec![0, 1, 2]);
        assert_eq!(third_streams, vec![1, 0, 2]);
    }

    #[test]
    fn demux_packet_pump_waits_for_cache_lock_during_startup_video_low_water() {
        let decoder_input = decoder_input_snapshot(vec![0, 1], 0, Some(1), None);
        let pressure = VideoPacketAdmissionPressure {
            output_snapshot: startup_output_snapshot_for_test(40_000_000),
            skip_nonref_for_pressure: false,
            played_until_nsecs: None,
            output_resource_pressure: false,
        };

        assert_eq!(
            demux_pump_cache_lock_wait_for_test(false, false, &decoder_input, pressure),
            Some(DEMUX_PACKET_CACHE_LOW_WATER_LOCK_WAIT)
        );
    }

    #[test]
    fn demux_packet_pump_uses_short_lock_wait_when_cached_reader_is_ready() {
        let decoder_input = decoder_input_snapshot(vec![0, 1], 0, Some(1), None);
        let pressure = VideoPacketAdmissionPressure {
            output_snapshot: playback_output_snapshot_for_test(
                3,
                duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };

        assert_eq!(
            demux_pump_cache_lock_wait_with_watermark_for_test(
                false,
                false,
                &decoder_input,
                pressure,
                DemuxReaderWatermark {
                    selected_min_forward_nsecs: Some(2_500_000_000),
                    ..DemuxReaderWatermark::default()
                },
            ),
            Some(DEMUX_PACKET_CACHE_READY_LOCK_WAIT)
        );
    }

    #[test]
    fn demux_packet_pump_keeps_audio_open_when_video_stream_is_backpressured() {
        let decoder_input = decoder_input_snapshot(vec![1, 2], 0, Some(1), Some(2));

        assert_eq!(
            audio_priority_demux_streams(
                &decoder_input.demux_streams,
                decoder_input.audio_stream_index,
                decoder_input.video_stream_index,
                decoder_input.subtitle_stream_index
            ),
            vec![1, 2]
        );
    }

    fn decoder_input_snapshot(
        demux_streams: Vec<c_int>,
        video_stream_index: c_int,
        audio_stream_index: Option<c_int>,
        subtitle_stream_index: Option<c_int>,
    ) -> DecoderInputSnapshot {
        DecoderInputSnapshot {
            demux_streams,
            video_stream_index,
            audio_stream_index,
            subtitle_stream_index,
            audio_resume_waterline: None,
            video_decode_snapshot: VideoDecodeWorkerSnapshot {
                state: VideoDecodeWorkerState::NeedPacket,
                queued_frames: 0,
                queue_capacity: 48,
                pending_input_packets: 0,
                pending_input_capacity: 8,
                in_flight_packets: 0,
                command_queue_capacity: 8,
                completed_packets: 0,
            },
            video_decode_blocked_on: Some(PlaybackBlockReason::DecodedVideoQueue),
        }
    }

    fn audio_resume_waterline(ready: bool) -> AudioResumeWaterline {
        AudioResumeWaterline {
            resume_timeline_nsecs: 1_000_000_000,
            target_nsecs: 1_000_000_000,
            audio_output_buffered_until_nsecs: None,
            audio_output_pending_nsecs: None,
            pending_audio_start_nsecs: Some(1_000_000_000),
            pending_audio_forward_nsecs: Some(if ready { 1_000_000_000 } else { 500_000_000 }),
            decoded_audio_forward_nsecs: Some(if ready { 1_000_000_000 } else { 500_000_000 }),
            audio_decode_queued_nsecs: 0,
            audio_decode_in_flight_packets: 0,
            demux_audio_forward_nsecs: None,
            demux_audio_cached_packets: None,
            ready,
        }
    }

    fn playback_output_snapshot_for_test(
        queued_video_frames: usize,
        queued_video_forward_nsecs: u64,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state: PlaybackOutputState::Playing,
            first_video_frame_pending: false,
            rebuffering: false,
            queued_video_frames,
            queued_video_duration_nsecs: queued_video_forward_nsecs,
            queued_video_range_nsecs: Some((
                1_000_000_000,
                1_000_000_000 + queued_video_forward_nsecs,
            )),
            queued_video_forward_nsecs: Some(queued_video_forward_nsecs),
            queued_video_contiguous_forward_nsecs: Some(queued_video_forward_nsecs),
            queued_video_largest_gap_nsecs: None,
            video_output_low_water: queued_video_forward_nsecs
                <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill: false,
            rebuffer_empty_audio_output_blocked: false,
        }
    }

    fn startup_output_snapshot_for_test(
        queued_video_duration_nsecs: u64,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state: PlaybackOutputState::Syncing,
            first_video_frame_pending: true,
            rebuffering: false,
            queued_video_frames: (queued_video_duration_nsecs > 0) as usize,
            queued_video_duration_nsecs,
            queued_video_range_nsecs: (queued_video_duration_nsecs > 0)
                .then_some((1_000_000_000, 1_000_000_000 + queued_video_duration_nsecs)),
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

    fn demux_packet_snapshot(
        streams: Vec<(c_int, StreamCacheKind, usize)>,
    ) -> DemuxPacketQueueSnapshot {
        let streams = streams
            .into_iter()
            .map(
                |(stream_index, kind, queued_packets)| DemuxStreamPacketQueueSnapshot {
                    stream_index,
                    kind,
                    queued_packets,
                    packet_limit: 2048,
                    packet_queue_full: false,
                    prefetch_packet_queue_full: false,
                    readable_packets_for_stream: queued_packets,
                    reader_head_available: queued_packets > 0,
                    consumer_drainable: queued_packets > 0,
                    queued_bytes: queued_packets,
                    forward_nsecs: None,
                },
            )
            .collect::<Vec<_>>();
        DemuxPacketQueueSnapshot {
            total_packets: streams.iter().map(|stream| stream.queued_packets).sum(),
            total_bytes: streams.iter().map(|stream| stream.queued_bytes).sum(),
            memory_limit_bytes: 1024 * 1024,
            read_index: 0,
            streams,
        }
    }

    #[test]
    fn demux_packet_pump_defers_eof_for_cached_backpressured_streams() {
        let decoder_input = decoder_input_snapshot(vec![1], 0, Some(1), None);
        let demux_packet_snapshot = demux_packet_snapshot(vec![
            (0, StreamCacheKind::Video, 104),
            (1, StreamCacheKind::Audio, 0),
        ]);

        assert_eq!(
            eof_cached_backpressured_streams(&decoder_input, &demux_packet_snapshot),
            vec![0]
        );
    }

    #[test]
    fn demux_packet_pump_allows_eof_when_blocked_streams_have_no_cached_packets() {
        let decoder_input = decoder_input_snapshot(vec![1], 0, Some(1), None);
        let demux_packet_snapshot = demux_packet_snapshot(vec![
            (0, StreamCacheKind::Video, 0),
            (1, StreamCacheKind::Audio, 0),
            (7, StreamCacheKind::Unknown, 3),
        ]);

        assert!(
            eof_cached_backpressured_streams(&decoder_input, &demux_packet_snapshot).is_empty()
        );
    }
}
