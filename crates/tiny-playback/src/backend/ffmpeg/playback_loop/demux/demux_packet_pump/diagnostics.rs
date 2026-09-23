use super::*;

impl DemuxPacketPump {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn log_read_path_diagnostic(
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
            cached_only = context.cached_only,
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
            video_decode_submitted_not_consumed_packets = video_decode_snapshot.submitted_not_consumed_packets,
            "FFmpeg demux packet pump read path returned would-block"
        );
    }

    pub(super) fn log_rebuffer_audio_reader_head_diagnostic(
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
            .try_stream_reader_head_timeline(audio_stream_index);
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

    pub(super) fn log_wait(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_read_elapsed: Duration,
        demux_cache_timing: DemuxPacketCacheReadTiming,
        demux_read_result: &DemuxReadResult,
    ) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let result = match demux_read_result {
            DemuxReadResult::Packet(_) => "packet",
            DemuxReadResult::Eof => "eof",
            DemuxReadResult::WouldBlock => "would_block",
            DemuxReadResult::Interrupted => "interrupted",
            DemuxReadResult::Error(_) => "error",
        };
        let (demux_packet_snapshot, _, demux_snapshot_unavailable) =
            context.demux_cache.monitor_snapshot();
        let demux_packet_queue_full = demux_packet_snapshot.prefetch_queue_full()
            && !demux_packet_snapshot.consumer_drainable();
        let video_decode_snapshot = context.decoder_input.video_decode_snapshot;
        let video_decode_blocked_on = context.decoder_input.video_decode_blocked_on;
        let blocked_on = if matches!(
            video_decode_blocked_on,
            Some(
                PlaybackBlockReason::PacketQueueFull
                    | PlaybackBlockReason::DecoderRecovery
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
            cache_trim_suppressed_for_recovery =
                demux_cache_timing.trim_suppressed_for_recovery,
            cache_trim_performed = demux_cache_timing.trim_outcome.performed,
            cache_trim_steps = demux_cache_timing.trim_outcome.steps,
            cache_trim_removed_packets = demux_cache_timing.trim_outcome.removed_packets,
            cache_trim_removed_bytes = demux_cache_timing.trim_outcome.removed_bytes,
            cache_trim_compacted_global_entries =
                demux_cache_timing.trim_outcome.compacted_global_entries,
            cache_trim_budget_exhausted = demux_cache_timing.trim_outcome.budget_exhausted,
            cache_trim_remaining_overrun_bytes =
                demux_cache_timing.trim_outcome.remaining_overrun_bytes,
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
            demux_snapshot_unavailable,
            demux_packet_bytes = demux_packet_snapshot.total_bytes,
            demux_packet_queue_full,
            demux_packet_streams = ?demux_packet_snapshot.streams,
            video_decode_state = ?video_decode_snapshot.state,
            video_decode_queued_frames = video_decode_snapshot.queued_frames,
            video_decode_queue_capacity = video_decode_snapshot.queue_capacity,
            video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
            video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
            video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
            video_decode_submitted_not_consumed_packets = video_decode_snapshot.submitted_not_consumed_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            "FFmpeg demux packet read wait completed"
        );
    }

    pub(super) fn trace_timing(
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
            cache_trim_suppressed_for_recovery =
                demux_cache_timing.trim_suppressed_for_recovery,
            cache_trim_performed = demux_cache_timing.trim_outcome.performed,
            cache_trim_steps = demux_cache_timing.trim_outcome.steps,
            cache_trim_removed_packets = demux_cache_timing.trim_outcome.removed_packets,
            cache_trim_removed_bytes = demux_cache_timing.trim_outcome.removed_bytes,
            cache_trim_compacted_global_entries =
                demux_cache_timing.trim_outcome.compacted_global_entries,
            cache_trim_budget_exhausted = demux_cache_timing.trim_outcome.budget_exhausted,
            cache_trim_remaining_overrun_bytes =
                demux_cache_timing.trim_outcome.remaining_overrun_bytes,
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

    pub(super) fn should_log_timing(&mut self, timing: DemuxPacketCacheReadTiming) -> bool {
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

    pub(super) fn log_timing(
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

    pub(super) fn log_pump_exit_diagnostic(
        &self,
        context: &DemuxPacketPumpAdmissionContext<'_>,
        started_at: Instant,
        iterations: usize,
        terminal_result: &'static str,
        made_progress: bool,
        returned_result: &DemuxPacketPumpResult,
    ) {
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        let empty_startup_or_rebuffer = output_snapshot.queued_video_frames == 0
            && (output_snapshot.first_video_frame_pending || output_snapshot.rebuffering);
        let terminal_would_block = terminal_result == "would_block"
            || matches!(
                returned_result,
                DemuxPacketPumpResult::WouldBlock | DemuxPacketPumpResult::OutputLeadThrottled
            );
        if !empty_startup_or_rebuffer || !terminal_would_block {
            return;
        }

        let elapsed_before_diagnostic = started_at.elapsed();
        let diagnostic_snapshot_started_at = Instant::now();
        // Diagnostics must not turn a bounded read timeout into an unbounded
        // wait for the same cache lock. Reuse the published snapshot if busy.
        let (demux_packet_snapshot, demux_watermark, demux_snapshot_unavailable) =
            context.demux_cache.monitor_snapshot();
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
            demux_snapshot_unavailable,
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
            video_decode_submitted_not_consumed_packets = video_decode_snapshot.submitted_not_consumed_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            "FFmpeg demux packet pump returned after empty-output would-block"
        );
    }
}
