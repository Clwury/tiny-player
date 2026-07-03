use std::{os::raw::c_int, time::Duration};

use crate::player::render_host::PlaybackSessionId;

use super::types::{DemuxPacketAppendOutcome, DemuxPacketAppendTiming};
use super::{DEMUX_PACKET_APPEND_TIMING_LOG_AFTER, DemuxPacketCacheState, nsecs_to_seconds};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_cache_blocked_on(
    state: &DemuxPacketCacheState,
    cache_paused: bool,
) -> &'static str {
    if cache_paused && !state.cache_pause_recovered() {
        "demux_cache_pause"
    } else if state.seeking {
        "demux_seek"
    } else if state.has_demux_underrun() {
        "demux_cache_underrun"
    } else if state.should_pause_demux() {
        "demux_packet_cache_full"
    } else {
        "demux_cache"
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn log_demux_packet_append_timing(
    session_id: PlaybackSessionId,
    packet_stream_index: c_int,
    packet_bytes: usize,
    outcome: DemuxPacketAppendOutcome,
    cache_pause_changed: bool,
) {
    let timing = outcome.timing;
    let append_other = timing
        .lock_hold
        .saturating_sub(timing.disk_write)
        .saturating_sub(timing.trim)
        .saturating_sub(timing.emit_state);
    let append_other_measured = [
        timing.record_input_bytes,
        timing.skip_resume_overlap,
        timing.packet_index,
        timing.queue_insert,
        timing.maybe_start_reader_head,
        timing.refresh_readahead_hysteresis,
        timing.should_pause_demux,
        timing.refresh_cache_pause,
        timing.notify,
    ]
    .into_iter()
    .fold(Duration::default(), |total, elapsed| {
        total.saturating_add(elapsed)
    });
    let append_other_untracked = append_other.saturating_sub(append_other_measured);
    tracing::trace!(
        session_id = ?session_id,
        stream_index = packet_stream_index,
        packet_bytes,
        appended = outcome.appended,
        force_cache_state_report = outcome.force_cache_state_report,
        cache_state_emit_deferred_for_consumer = outcome.cache_state_emit_deferred_for_consumer,
        cache_pause_changed,
        append_lock_wait_ms = timing.lock_wait.as_secs_f64() * 1000.0,
        append_lock_hold_ms = timing.lock_hold.as_secs_f64() * 1000.0,
        append_disk_write_ms = timing.disk_write.as_secs_f64() * 1000.0,
        append_trim_ms = timing.trim.as_secs_f64() * 1000.0,
        append_emit_state_ms = timing.emit_state.as_secs_f64() * 1000.0,
        append_emit_state_lock_wait_ms = timing.emit_state_lock_wait.as_secs_f64() * 1000.0,
        append_emit_state_prepare_ms = timing.emit_state_prepare.as_secs_f64() * 1000.0,
        append_emit_state_send_ms = timing.emit_state_send.as_secs_f64() * 1000.0,
        append_other_ms = append_other.as_secs_f64() * 1000.0,
        append_record_input_bytes_ms = timing.record_input_bytes.as_secs_f64() * 1000.0,
        append_skip_resume_overlap_ms = timing.skip_resume_overlap.as_secs_f64() * 1000.0,
        append_packet_index_ms = timing.packet_index.as_secs_f64() * 1000.0,
        append_queue_insert_ms = timing.queue_insert.as_secs_f64() * 1000.0,
        append_maybe_start_reader_head_ms =
            timing.maybe_start_reader_head.as_secs_f64() * 1000.0,
        append_refresh_readahead_hysteresis_ms =
            timing.refresh_readahead_hysteresis.as_secs_f64() * 1000.0,
        append_should_pause_demux_ms = timing.should_pause_demux.as_secs_f64() * 1000.0,
        append_refresh_cache_pause_ms = timing.refresh_cache_pause.as_secs_f64() * 1000.0,
        append_notify_ms = timing.notify.as_secs_f64() * 1000.0,
        append_other_untracked_ms = append_other_untracked.as_secs_f64() * 1000.0,
        "FFmpeg demux packet append timing"
    );
    if !demux_packet_append_timing_should_log(timing) {
        return;
    }
    tracing::debug!(
        session_id = ?session_id,
        stream_index = packet_stream_index,
        packet_bytes,
        appended = outcome.appended,
        force_cache_state_report = outcome.force_cache_state_report,
        cache_state_emit_deferred_for_consumer = outcome.cache_state_emit_deferred_for_consumer,
        cache_pause_changed,
        append_lock_wait_ms = timing.lock_wait.as_secs_f64() * 1000.0,
        append_lock_hold_ms = timing.lock_hold.as_secs_f64() * 1000.0,
        append_disk_write_ms = timing.disk_write.as_secs_f64() * 1000.0,
        append_trim_ms = timing.trim.as_secs_f64() * 1000.0,
        append_emit_state_ms = timing.emit_state.as_secs_f64() * 1000.0,
        append_emit_state_lock_wait_ms = timing.emit_state_lock_wait.as_secs_f64() * 1000.0,
        append_emit_state_prepare_ms = timing.emit_state_prepare.as_secs_f64() * 1000.0,
        append_emit_state_send_ms = timing.emit_state_send.as_secs_f64() * 1000.0,
        append_other_ms = append_other.as_secs_f64() * 1000.0,
        append_record_input_bytes_ms = timing.record_input_bytes.as_secs_f64() * 1000.0,
        append_skip_resume_overlap_ms = timing.skip_resume_overlap.as_secs_f64() * 1000.0,
        append_packet_index_ms = timing.packet_index.as_secs_f64() * 1000.0,
        append_queue_insert_ms = timing.queue_insert.as_secs_f64() * 1000.0,
        append_maybe_start_reader_head_ms =
            timing.maybe_start_reader_head.as_secs_f64() * 1000.0,
        append_refresh_readahead_hysteresis_ms =
            timing.refresh_readahead_hysteresis.as_secs_f64() * 1000.0,
        append_should_pause_demux_ms = timing.should_pause_demux.as_secs_f64() * 1000.0,
        append_refresh_cache_pause_ms = timing.refresh_cache_pause.as_secs_f64() * 1000.0,
        append_notify_ms = timing.notify.as_secs_f64() * 1000.0,
        append_other_untracked_ms = append_other_untracked.as_secs_f64() * 1000.0,
        "FFmpeg demux packet append completed slowly"
    );
}

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn log_would_block_diagnostic(
        &self,
        stream_indices: &[c_int],
    ) {
        let per_stream: Vec<String> = stream_indices
            .iter()
            .copied()
            .map(|stream_index| {
                let reader_head = self.reader_heads.get(&stream_index).copied();
                let head_in_active_range = reader_head.is_some_and(|head| {
                    self.read_range()
                        .stream_queues
                        .get(&stream_index)
                        .is_some_and(|queue| queue.iter().any(|candidate| *candidate == head))
                });
                let active_queue_len = self
                    .read_range()
                    .stream_queues
                    .get(&stream_index)
                    .map(|queue| queue.len())
                    .unwrap_or(0);
                let detached_queue_len = self
                    .detached_append_range()
                    .and_then(|range| range.stream_queues.get(&stream_index))
                    .map(|queue| queue.len())
                    .unwrap_or(0);
                format!(
                    "stream={stream_index} head={reader_head:?} head_in_active={head_in_active_range} active_q={active_queue_len} detached_q={detached_queue_len}"
                )
            })
            .collect();
        tracing::debug!(
            session_id = ?self.session_id,
            read_range_id = self.read_range_id,
            append_range_id = self.append_range_id,
            append_range_detached = self.append_range_id != self.read_range_id,
            detached_append_range_id = ?self.detached_append_range_id(),
            demux_position_detached = self.demux_position_detached,
            forward_duration_ms = self.forward_duration_nsecs() as f64 / 1_000_000.0,
            reader_pts_seconds = nsecs_to_seconds(self.reader_nsecs),
            per_stream = ?per_stream,
            "FFmpeg demux pump WouldBlock with buffered packets: reader_head/range state"
        );
    }
}

fn demux_packet_append_timing_should_log(timing: DemuxPacketAppendTiming) -> bool {
    [
        timing.lock_wait,
        timing.lock_hold,
        timing.disk_write,
        timing.trim,
        timing.emit_state,
        timing.record_input_bytes,
        timing.skip_resume_overlap,
        timing.packet_index,
        timing.queue_insert,
        timing.maybe_start_reader_head,
        timing.refresh_readahead_hysteresis,
        timing.should_pause_demux,
        timing.refresh_cache_pause,
        timing.emit_state_lock_wait,
        timing.emit_state_prepare,
        timing.emit_state_send,
        timing.notify,
    ]
    .into_iter()
    .any(|elapsed| elapsed >= DEMUX_PACKET_APPEND_TIMING_LOG_AFTER)
}
