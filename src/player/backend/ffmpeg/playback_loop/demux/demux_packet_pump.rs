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
const DEMUX_PACKET_READER_OUTPUT_LEAD_LIMIT: Duration = Duration::from_millis(2500);
const DEMUX_OUTPUT_LEAD_THROTTLE_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct DemuxPacketPump {
    stream_cursor: usize,
    rebuffer_audio_priority_without_audio_progress: usize,
    last_timing_log_at: Option<Instant>,
    last_poll_output_lead_throttled: bool,
    output_lead_throttle_started_at: Option<Instant>,
    output_lead_throttle_last_summary_at: Option<Instant>,
    output_lead_throttle_suppressed_count: u64,
}

struct DemuxPacketPumpContext<'a> {
    session_id: PlaybackSessionId,
    demux_cache: &'a DemuxPacketCache,
    decoder_input: &'a DecoderInputSnapshot,
    video_admission_pressure: VideoPacketAdmissionPressure,
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
    cached_reader_watermark: DemuxReaderWatermark,
    current_start_position_nsecs: u64,
    hard_deadline: Option<Instant>,
    cached_only: bool,
}

pub(super) struct DemuxPacketPumpAdmissionContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) video_admission_pressure: VideoPacketAdmissionPressure,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
    pub(super) cached_only: bool,
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
    OutputLeadThrottled,
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

#[path = "demux_packet_pump/admission.rs"]
mod admission;
#[path = "demux_packet_pump/diagnostics.rs"]
mod diagnostics;
#[path = "demux_packet_pump/poll.rs"]
mod poll;

fn demux_playback_recovery_critical(
    output_snapshot: PlaybackOutputSnapshot,
    decoder_input: &DecoderInputSnapshot,
) -> bool {
    output_snapshot.initial_av_start_pending
        || output_snapshot.rebuffering
        || output_snapshot.video_output_low_water
        || decoder_input.audio_output_low_water
        || (!output_snapshot.initial_av_start_pending && output_snapshot.queued_video_frames == 0)
}

fn demux_video_recovery_required(
    output_snapshot: PlaybackOutputSnapshot,
    decoder_input: &DecoderInputSnapshot,
    requested_streams: &[c_int],
) -> bool {
    output_snapshot.first_frame_needed
        || requested_streams.contains(&decoder_input.video_stream_index)
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
        DemuxPacketPumpResult::OutputLeadThrottled => "output_lead_throttled",
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
        trim_outcome: first.trim_outcome.merged(second.trim_outcome),
        trim_suppressed_for_recovery: first.trim_suppressed_for_recovery
            || second.trim_suppressed_for_recovery,
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

fn demux_reader_output_lead_throttle_enabled(
    output_snapshot: PlaybackOutputSnapshot,
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
) -> bool {
    !should_wait_for_demux
        && !video_output_waiting_for_demux
        && !output_queue_low_water_needs_decoder_input(output_snapshot)
        && !output_queue_needs_decoder_input(output_snapshot)
}

fn demux_reader_head_exceeds_output_lead(
    reader_head_start_nsecs: u64,
    output_reference_nsecs: u64,
) -> bool {
    reader_head_start_nsecs
        > output_reference_nsecs.saturating_add(demux_reader_output_lead_limit_nsecs())
}

fn demux_reader_output_lead_limit_nsecs() -> u64 {
    duration_nsecs(DEMUX_PACKET_READER_OUTPUT_LEAD_LIMIT)
}

pub(super) fn cached_input_output_lead_throttled(
    demux_packet_snapshot: &DemuxPacketQueueSnapshot,
    requested_streams: &[c_int],
    output_snapshot: PlaybackOutputSnapshot,
    output_reference_nsecs: u64,
) -> bool {
    if !demux_reader_output_lead_throttle_enabled(output_snapshot, false, false) {
        return false;
    }
    let mut drainable_requested = demux_packet_snapshot.streams.iter().filter(|stream| {
        requested_streams.contains(&stream.stream_index) && stream.consumer_drainable
    });
    let Some(first) = drainable_requested.next() else {
        return false;
    };
    first.reader_nsecs.is_some_and(|reader_nsecs| {
        demux_reader_head_exceeds_output_lead(reader_nsecs, output_reference_nsecs)
    }) && drainable_requested.all(|stream| {
        stream.reader_nsecs.is_some_and(|reader_nsecs| {
            demux_reader_head_exceeds_output_lead(reader_nsecs, output_reference_nsecs)
        })
    })
}

fn decoder_input_timed_stream(decoder_input: &DecoderInputSnapshot, stream_index: c_int) -> bool {
    stream_index == decoder_input.video_stream_index
        || decoder_input.audio_stream_index == Some(stream_index)
        || decoder_input.subtitle_stream_index == Some(stream_index)
}

fn decoder_input_output_lead_throttled_stream(
    decoder_input: &DecoderInputSnapshot,
    stream_index: c_int,
    throttle_audio_video: bool,
) -> bool {
    decoder_input.subtitle_stream_index == Some(stream_index)
        || (throttle_audio_video
            && (stream_index == decoder_input.video_stream_index
                || decoder_input.audio_stream_index == Some(stream_index)))
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
    if output_snapshot.first_frame_needed {
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
    ) || (decoder_input_accepts_video_packet(decoder_input)
        && matches!(
            decoder_input.video_decode_snapshot.state,
            VideoDecodeWorkerState::NeedPacket
        )
        && decoder_input.video_decode_snapshot.queued_frames == 0
        && decoder_input.video_decode_snapshot.pending_input_packets == 0
        && decoder_input
            .video_decode_snapshot
            .submitted_not_consumed_packets
            == 0
        && decoder_input.video_decode_snapshot.completed_packets == 0)
        || audio_low_water_priority_active(decoder_input)
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
    output_snapshot.first_frame_needed
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
        || snapshot.submitted_not_consumed_packets > 0
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
        cached_input_output_lead_throttled, decoder_input_output_lead_throttled_stream,
        decoder_input_waiting_for_packets, demux_playback_recovery_critical,
        demux_pump_cache_lock_wait_for_test, demux_pump_cache_lock_wait_with_watermark_for_test,
        demux_pump_should_wait_for_cache_lock, demux_reader_head_exceeds_output_lead,
        demux_reader_output_lead_limit_nsecs, demux_reader_output_lead_throttle_enabled,
        demux_video_recovery_required, duration_nsecs, eof_cached_backpressured_streams,
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
    fn demux_packet_pump_treats_idle_need_packet_decoder_as_waiting() {
        let mut decoder_input = decoder_input_snapshot(vec![0], 0, None, None);
        decoder_input.video_decode_blocked_on = Some(PlaybackBlockReason::DecodedVideoQueue);

        assert!(decoder_input_waiting_for_packets(&decoder_input));

        decoder_input.video_decode_snapshot.state = VideoDecodeWorkerState::Decoding;
        decoder_input
            .video_decode_snapshot
            .submitted_not_consumed_packets = 1;
        assert!(!decoder_input_waiting_for_packets(&decoder_input));
    }

    #[test]
    fn first_frame_keeps_video_recovery_demand_when_current_input_is_audio_only() {
        let decoder_input = decoder_input_snapshot(vec![1], 0, Some(1), None);
        let output = startup_output_snapshot_for_test(0);

        assert!(demux_playback_recovery_critical(output, &decoder_input));
        assert!(demux_video_recovery_required(output, &decoder_input, &[1],));
    }

    #[test]
    fn queued_startup_frame_closes_forced_video_input_demand() {
        let decoder_input = decoder_input_snapshot(vec![1], 0, Some(1), None);
        let output = startup_output_snapshot_for_test(40_000_000);

        assert!(demux_playback_recovery_critical(output, &decoder_input));
        assert!(!demux_video_recovery_required(output, &decoder_input, &[1]));
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
            presentation: None,
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
            presentation: None,
            output_snapshot: playback_output_snapshot_for_test(
                10,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };
        let draining_output = VideoPacketAdmissionPressure {
            presentation: None,
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
            presentation: None,
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
            presentation: None,
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
            presentation: None,
            output_snapshot: playback_output_snapshot_for_test(
                12,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            skip_nonref_for_pressure: false,
            played_until_nsecs: Some(1_000_000_000),
            output_resource_pressure: false,
        };
        let draining_output = VideoPacketAdmissionPressure {
            presentation: None,
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
            presentation: None,
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
    fn demux_packet_pump_marks_playing_audio_low_water_recovery_critical() {
        let mut decoder_input = decoder_input_snapshot(vec![0, 1], 0, Some(1), None);
        let output_snapshot =
            playback_output_snapshot_for_test(12, duration_nsecs(Duration::from_millis(1_200)));

        assert!(!demux_playback_recovery_critical(
            output_snapshot,
            &decoder_input
        ));
        decoder_input.audio_output_low_water = true;
        assert!(demux_playback_recovery_critical(
            output_snapshot,
            &decoder_input
        ));
    }

    #[test]
    fn demux_packet_pump_stops_forcing_video_after_first_startup_frame() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 0, 2], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(false));

        let (streams, rotation) = pump.ordered_demux_streams_for_context(
            &decoder_input,
            startup_output_snapshot_for_test(40_000_000),
        );

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![1, 0, 2]);
    }

    #[test]
    fn demux_packet_pump_uses_normal_rotation_after_first_startup_frame() {
        let mut pump = DemuxPacketPump::default();
        let mut decoder_input = decoder_input_snapshot(vec![1, 2, 0], 0, Some(1), Some(2));
        decoder_input.audio_resume_waterline = Some(audio_resume_waterline(true));

        let (streams, rotation) = pump.ordered_demux_streams_for_context(
            &decoder_input,
            startup_output_snapshot_for_test(40_000_000),
        );

        assert_eq!(rotation, 0);
        assert_eq!(streams, vec![1, 2, 0]);
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
    fn demux_packet_pump_does_not_force_cache_wait_after_first_startup_frame() {
        let decoder_input = decoder_input_snapshot(vec![0, 1], 0, Some(1), None);
        let pressure = VideoPacketAdmissionPressure {
            presentation: None,
            output_snapshot: startup_output_snapshot_for_test(40_000_000),
            skip_nonref_for_pressure: false,
            played_until_nsecs: None,
            output_resource_pressure: false,
        };

        assert_eq!(
            demux_pump_cache_lock_wait_for_test(false, false, &decoder_input, pressure),
            None
        );
    }

    #[test]
    fn demux_packet_pump_uses_short_lock_wait_when_cached_reader_is_ready() {
        let decoder_input = decoder_input_snapshot(vec![0, 1], 0, Some(1), None);
        let pressure = VideoPacketAdmissionPressure {
            presentation: None,
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
    fn demux_packet_pump_throttles_reader_when_output_has_headroom() {
        let output_reference_nsecs = 10_000_000_000_u64;
        let allowed_reader_head =
            output_reference_nsecs.saturating_add(demux_reader_output_lead_limit_nsecs());
        let output_snapshot = playback_output_snapshot_for_test(
            10,
            duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
        );

        assert!(demux_reader_output_lead_throttle_enabled(
            output_snapshot,
            false,
            false
        ));
        assert!(!demux_reader_head_exceeds_output_lead(
            allowed_reader_head,
            output_reference_nsecs
        ));
        assert!(demux_reader_head_exceeds_output_lead(
            allowed_reader_head + 1,
            output_reference_nsecs
        ));
    }

    #[test]
    fn drainable_output_lead_throttled_cache_is_not_input_admissible() {
        let output_reference_nsecs = 184_700_000_000_u64;
        let mut packet_snapshot = demux_packet_snapshot(vec![
            (0, StreamCacheKind::Video, 85),
            (1, StreamCacheKind::Audio, 85),
        ]);
        for stream in &mut packet_snapshot.streams {
            stream.reader_nsecs = Some(
                output_reference_nsecs
                    .saturating_add(demux_reader_output_lead_limit_nsecs())
                    .saturating_add(1),
            );
        }
        let output = playback_output_snapshot_for_test(
            38,
            duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
        );

        assert!(packet_snapshot.consumer_drainable_for_streams(&[0, 1]));
        assert!(cached_input_output_lead_throttled(
            &packet_snapshot,
            &[0, 1],
            output,
            output_reference_nsecs,
        ));
    }

    #[test]
    fn demux_packet_pump_throttles_subtitle_reader_even_when_output_needs_packets() {
        let decoder_input = decoder_input_snapshot(vec![0, 1, 3], 0, Some(1), Some(3));

        assert!(decoder_input_output_lead_throttled_stream(
            &decoder_input,
            3,
            false
        ));
        assert!(!decoder_input_output_lead_throttled_stream(
            &decoder_input,
            0,
            false
        ));
        assert!(decoder_input_output_lead_throttled_stream(
            &decoder_input,
            0,
            true
        ));
    }

    #[test]
    fn demux_packet_pump_does_not_throttle_reader_when_output_needs_packets() {
        let low_water_output = playback_output_snapshot_for_test(
            1,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION),
        );
        let draining_output =
            playback_output_snapshot_for_test(9, duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION));

        assert!(!demux_reader_output_lead_throttle_enabled(
            low_water_output,
            false,
            false
        ));
        assert!(!demux_reader_output_lead_throttle_enabled(
            draining_output,
            false,
            false
        ));
        assert!(!demux_reader_output_lead_throttle_enabled(
            playback_output_snapshot_for_test(
                10,
                duration_nsecs(AUDIO_VIDEO_QUEUE_LIMIT_DURATION) + 1,
            ),
            true,
            false
        ));
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
            audio_output_low_water: false,
            video_decode_snapshot: VideoDecodeWorkerSnapshot {
                state: VideoDecodeWorkerState::NeedPacket,
                queued_frames: 0,
                queue_capacity: 48,
                pending_input_packets: 0,
                pending_input_capacity: 8,
                submitted_not_consumed_packets: 0,
                command_queue_capacity: 8,
                completed_packets: 0,
                ..VideoDecodeWorkerSnapshot::default()
            },
            video_decode_blocked_on: Some(PlaybackBlockReason::DecodedVideoQueue),
        }
    }

    fn audio_resume_waterline(ready: bool) -> AudioResumeWaterline {
        let forward_nsecs = if ready { 1_000_000_000 } else { 500_000_000 };
        AudioResumeWaterline {
            resume_timeline_nsecs: 1_000_000_000,
            target_nsecs: 1_000_000_000,
            audio_accepted_start_timeline_nsecs: Some(1_000_000_000),
            audio_accepted_start_gap_nsecs: Some(0),
            accepted_contiguous_coverage_nsecs: Some(forward_nsecs),
            audio_output_buffered_until_nsecs: None,
            audio_output_pending_nsecs: None,
            pending_audio_start_nsecs: Some(1_000_000_000),
            pending_audio_forward_nsecs: Some(forward_nsecs),
            decoded_audio_forward_nsecs: Some(forward_nsecs),
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
            first_frame_needed: false,
            first_frame_presented: true,
            initial_av_start_pending: false,
            output_clock_running: true,
            audio_start_target_nsecs: None,
            output_transition_deadline_ms: None,
            rebuffering: false,
            queued_video_frames,
            recovery_staging_frames: 0,
            recovery_staging_frame_budget: None,
            committed_output_high_water_nsecs: None,
            recovery_staged_high_water_nsecs: None,
            decode_recovery_audio_ready_latched: false,
            queued_video_coverage_nsecs: queued_video_forward_nsecs,
            queued_video_duration_nsecs: queued_video_forward_nsecs,
            queued_video_range_span_nsecs: queued_video_forward_nsecs,
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
            scheduler_dropped_video_frames: 0,
            recent_coordinator_stall_nsecs: None,
            recent_coordinator_stall_age_nsecs: None,
        }
    }

    fn startup_output_snapshot_for_test(
        queued_video_duration_nsecs: u64,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state: PlaybackOutputState::Syncing,
            first_video_frame_pending: true,
            first_frame_needed: queued_video_duration_nsecs == 0,
            first_frame_presented: false,
            initial_av_start_pending: true,
            output_clock_running: false,
            audio_start_target_nsecs: None,
            output_transition_deadline_ms: None,
            rebuffering: false,
            queued_video_frames: (queued_video_duration_nsecs > 0) as usize,
            recovery_staging_frames: 0,
            recovery_staging_frame_budget: None,
            committed_output_high_water_nsecs: None,
            recovery_staged_high_water_nsecs: None,
            decode_recovery_audio_ready_latched: false,
            queued_video_coverage_nsecs: queued_video_duration_nsecs,
            queued_video_duration_nsecs,
            queued_video_range_span_nsecs: queued_video_duration_nsecs,
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
            scheduler_dropped_video_frames: 0,
            recent_coordinator_stall_nsecs: None,
            recent_coordinator_stall_age_nsecs: None,
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
                    reader_nsecs: None,
                    cached_end_nsecs: None,
                    target_coverage_nsecs: None,
                    forward_nsecs: None,
                },
            )
            .collect::<Vec<_>>();
        DemuxPacketQueueSnapshot {
            cache_generation: 0,
            total_packets: streams.iter().map(|stream| stream.queued_packets).sum(),
            total_bytes: streams.iter().map(|stream| stream.queued_bytes).sum(),
            memory_limit_bytes: 1024 * 1024,
            prefetch_limit_bytes: 1024 * 1024,
            read_index: 0,
            exact_seek_target_nsecs: 0,
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
