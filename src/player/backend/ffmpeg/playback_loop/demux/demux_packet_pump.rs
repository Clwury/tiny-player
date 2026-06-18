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
    DemuxPacketCache, DemuxReadResult, PlaybackBlockReason, PlaybackOutputSnapshot, duration_nsecs,
};

const DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK: usize = 16;
const DEMUX_PACKET_PUMP_MAX_SYNC_DURATION_PER_TICK: Duration = Duration::from_millis(4);

#[derive(Default)]
pub(super) struct DemuxPacketPump {
    stream_cursor: usize,
    last_timing_log_at: Option<Instant>,
}

struct DemuxPacketPumpContext<'a> {
    session_id: PlaybackSessionId,
    demux_cache: &'a DemuxPacketCache,
    decoder_input: &'a DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
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
        let mut demux_streams = context.decoder_input.demux_streams.clone();
        let demux_stream_rotation = if demux_streams.is_empty() {
            0
        } else {
            self.stream_cursor % demux_streams.len()
        };
        demux_streams.rotate_left(demux_stream_rotation);

        let demux_read_started_at = Instant::now();
        let (demux_read_result, demux_consumed_stream_offset, demux_cache_timing) =
            if let Some(lock_wait) = demux_pump_cache_lock_wait(
                context.should_wait_for_demux,
                context.video_output_waiting_for_demux,
                context.decoder_input,
                context.video_admission_pressure,
            ) {
                context
                    .demux_cache
                    .read_available_packet_round_robin_with_cache_pause_signal_and_timing(
                        &demux_streams,
                        lock_wait,
                        demux_cache_pause_signal(&context),
                    )
            } else {
                context
                    .demux_cache
                    .poll_packet_round_robin_with_timing(&demux_streams)
            };
        let demux_read_elapsed = demux_read_started_at.elapsed();
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

    pub(super) fn poll_and_admit_packet(
        &mut self,
        mut context: DemuxPacketPumpAdmissionContext<'_>,
    ) -> DemuxPacketPumpResult {
        let started_at = Instant::now();
        let mut made_progress = false;
        for _ in 0..DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK {
            match self.poll_and_admit_one(&mut context) {
                DemuxPacketPumpResult::Progress => {
                    made_progress = true;
                    if started_at.elapsed() >= DEMUX_PACKET_PUMP_MAX_SYNC_DURATION_PER_TICK {
                        return DemuxPacketPumpResult::Progress;
                    }
                }
                DemuxPacketPumpResult::Backpressured => {
                    return DemuxPacketPumpResult::Backpressured;
                }
                DemuxPacketPumpResult::Eof => {
                    return if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::Eof
                    };
                }
                DemuxPacketPumpResult::WouldBlock => {
                    return if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::WouldBlock
                    };
                }
                DemuxPacketPumpResult::Interrupted => return DemuxPacketPumpResult::Interrupted,
                DemuxPacketPumpResult::Error(error) => return DemuxPacketPumpResult::Error(error),
            }
        }
        DemuxPacketPumpResult::Progress
    }

    fn poll_and_admit_one(
        &mut self,
        context: &mut DemuxPacketPumpAdmissionContext<'_>,
    ) -> DemuxPacketPumpResult {
        let decoder_input = context
            .pipeline
            .decoder_input_snapshot(context.video_admission_pressure.output_resource_pressure);
        let demux_read_result = self.poll_packet(DemuxPacketPumpContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            decoder_input: &decoder_input,
            video_admission_pressure: context.video_admission_pressure,
            should_wait_for_demux: context.should_wait_for_demux,
            video_output_waiting_for_demux: context.video_output_waiting_for_demux,
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
            DemuxReadResult::WouldBlock => return DemuxPacketPumpResult::WouldBlock,
            DemuxReadResult::Interrupted => return DemuxPacketPumpResult::Interrupted,
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

        match process_result {
            Ok(status) if status.backpressured() => DemuxPacketPumpResult::Backpressured,
            Ok(_) => DemuxPacketPumpResult::Progress,
            Err(error) => DemuxPacketPumpResult::Error(error),
        }
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
        let demux_packet_queue_full = demux_packet_snapshot
            .streams
            .iter()
            .any(|stream| stream.packet_queue_full);
        let video_decode_snapshot = context.decoder_input.video_decode_snapshot;
        let video_decode_blocked_on = context.decoder_input.video_decode_blocked_on;
        let blocked_on = if matches!(
            video_decode_blocked_on,
            Some(
                PlaybackBlockReason::PacketQueueFull
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
    )
    .is_some()
}

fn demux_pump_cache_lock_wait(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
) -> Option<Duration> {
    if decoder_input.demux_streams.is_empty() || !decoder_input_accepts_video_packet(decoder_input)
    {
        return demux_pump_audio_cache_lock_wait(
            should_wait_for_demux,
            video_output_waiting_for_demux,
            decoder_input,
            video_admission_pressure,
        );
    }
    if should_wait_for_demux
        || video_output_waiting_for_demux
        || output_queue_needs_decoder_input(video_admission_pressure.output_snapshot)
    {
        return Some(DEMUX_PACKET_CACHE_LOCK_WAIT);
    }
    if video_decoder_has_pending_work(decoder_input.video_decode_snapshot) {
        return None;
    }
    None
}

fn demux_cache_pause_signal(context: &DemuxPacketPumpContext<'_>) -> bool {
    context.video_output_waiting_for_demux
        || context.video_admission_pressure.output_snapshot.rebuffering
}

fn demux_pump_audio_cache_lock_wait(
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    decoder_input: &DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
) -> Option<Duration> {
    if !decoder_input_accepts_audio_packet(decoder_input) {
        return None;
    }
    if should_wait_for_demux
        || video_output_waiting_for_demux
        || output_queue_needs_decoder_input(video_admission_pressure.output_snapshot)
    {
        return Some(DEMUX_PACKET_CACHE_LOCK_WAIT);
    }
    None
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

#[cfg(test)]
mod tests {
    use crate::player::backend::{
        StreamCacheKind,
        ffmpeg::playback_loop::{
            demux_cache::{DemuxPacketQueueSnapshot, DemuxStreamPacketQueueSnapshot},
            video_decode_worker::{VideoDecodeWorkerSnapshot, VideoDecodeWorkerState},
        },
    };

    use super::super::VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION;
    use super::super::output_rebuffer::PlaybackOutputState;
    use super::super::playback_pipeline_state::DecoderInputSnapshot;
    use super::{
        AUDIO_VIDEO_QUEUE_LIMIT_DURATION, AvPacket, DemuxPacketPump, DemuxPacketRoute,
        PlaybackBlockReason, PlaybackOutputSnapshot, VideoPacketAdmissionPressure,
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
            video_output_low_water: queued_video_forward_nsecs
                <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
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
                    queued_bytes: queued_packets,
                    forward_nsecs: None,
                },
            )
            .collect::<Vec<_>>();
        DemuxPacketQueueSnapshot {
            total_packets: streams.iter().map(|stream| stream.queued_packets).sum(),
            total_bytes: streams.iter().map(|stream| stream.queued_bytes).sum(),
            memory_limit_bytes: 1024 * 1024,
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
