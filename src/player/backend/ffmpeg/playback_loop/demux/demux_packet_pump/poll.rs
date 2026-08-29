use super::*;

impl DemuxPacketPump {
    pub(super) fn poll_packet(&mut self, context: DemuxPacketPumpContext<'_>) -> DemuxReadResult {
        self.last_poll_output_lead_throttled = false;
        let (mut demux_streams, demux_stream_rotation) = self.ordered_demux_streams_for_context(
            context.decoder_input,
            context.video_admission_pressure.output_snapshot,
        );
        let all_timed_streams_throttled =
            self.filter_demux_streams_by_output_lead(&context, &mut demux_streams);
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        context.demux_cache.set_playback_recovery_demand(
            demux_playback_recovery_critical(output_snapshot, context.decoder_input),
            demux_video_recovery_required(output_snapshot, context.decoder_input, &demux_streams),
            context
                .decoder_input
                .audio_stream_index
                .is_some_and(|stream_index| demux_streams.contains(&stream_index)),
        );
        if all_timed_streams_throttled {
            self.last_poll_output_lead_throttled = true;
            return DemuxReadResult::WouldBlock;
        }
        if demux_streams.is_empty() {
            return DemuxReadResult::WouldBlock;
        }

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
        let mut force_consumer_drain_retry_count = 0_u64;
        let (demux_read_result, demux_consumed_stream_offset, demux_cache_timing) =
            if context.cached_only {
                read_path = "cached_only_poll";
                context
                    .demux_cache
                    .poll_packet_round_robin_with_timing(&demux_streams)
            } else if force_consumer_drain {
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
                if matches!(result, DemuxReadResult::WouldBlock) && timing.lock_timed_out {
                    // The decoder is starving with readable packets available;
                    // bounded try-lock spins lose to long append/trim holds, so
                    // park on the mutex and drain as soon as it is released.
                    force_consumer_drain_retry_count =
                        force_consumer_drain_retry_count.saturating_add(1);
                    let (drain_result, drain_stream_offset, drain_timing) = context
                        .demux_cache
                        .drain_available_packet_round_robin_with_unbounded_lock_and_timing(
                            &demux_streams,
                            demux_cache_pause_signal(&context),
                        );
                    result = drain_result;
                    stream_offset = drain_stream_offset;
                    timing = combine_demux_read_timing(timing, drain_timing);
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
            && let Some(consumed_stream_index) = demux_streams.get(consumed_offset)
        {
            self.stream_cursor = context
                .decoder_input
                .demux_streams
                .iter()
                .position(|stream_index| stream_index == consumed_stream_index)
                .map(|position| (position + 1) % context.decoder_input.demux_streams.len().max(1))
                .unwrap_or_else(|| {
                    (demux_stream_rotation + consumed_offset + 1) % demux_streams.len()
                });
        }

        demux_read_result
    }

    pub(in super::super::super) fn poll_and_admit_packet(
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
                DemuxPacketPumpResult::OutputLeadThrottled => {
                    let result = if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::OutputLeadThrottled
                    };
                    self.log_pump_exit_diagnostic(
                        &context,
                        started_at,
                        iteration + 1,
                        "output_lead_throttled",
                        made_progress,
                        &result,
                    );
                    return result;
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

    pub(super) fn poll_and_admit_one(
        &mut self,
        context: &mut DemuxPacketPumpAdmissionContext<'_>,
        hard_deadline: Option<Instant>,
    ) -> DemuxPacketPumpResult {
        let decoder_input = context
            .pipeline
            .decoder_input_snapshot(context.video_admission_pressure.output_resource_pressure);
        self.request_output_wait_audio_reader_head_realign_if_needed(context, &decoder_input);
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
            current_start_position_nsecs: context.pipeline.current_start_position_nsecs,
            hard_deadline,
            cached_only: context.cached_only,
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
                return if self.last_poll_output_lead_throttled {
                    DemuxPacketPumpResult::OutputLeadThrottled
                } else {
                    DemuxPacketPumpResult::WouldBlock
                };
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
                context.demux_cache.cached_reader_watermark(),
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

    pub(super) fn route_packet(
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
}
