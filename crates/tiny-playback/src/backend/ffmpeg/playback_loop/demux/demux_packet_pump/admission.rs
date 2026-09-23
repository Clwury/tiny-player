use super::*;

impl DemuxPacketPump {
    pub(super) fn filter_demux_streams_by_output_lead(
        &mut self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &mut Vec<c_int>,
    ) -> bool {
        let throttle_audio_video = demux_reader_output_lead_throttle_enabled(
            context.video_admission_pressure.output_snapshot,
            context.should_wait_for_demux,
            context.video_output_waiting_for_demux,
        );
        let output_reference_nsecs = context
            .video_admission_pressure
            .played_until_nsecs
            .unwrap_or(context.current_start_position_nsecs);
        let mut throttled_streams = Vec::new();
        demux_streams.retain(|stream_index| {
            if !decoder_input_output_lead_throttled_stream(
                context.decoder_input,
                *stream_index,
                throttle_audio_video,
            ) {
                return true;
            }
            let Some((_, Some(reader_head_start_nsecs), _)) = context
                .demux_cache
                .try_stream_reader_head_timeline(*stream_index)
            else {
                return true;
            };
            if demux_reader_head_exceeds_output_lead(
                reader_head_start_nsecs,
                output_reference_nsecs,
            ) {
                throttled_streams.push((*stream_index, reader_head_start_nsecs));
                false
            } else {
                true
            }
        });
        if throttled_streams.is_empty() {
            self.observe_output_lead_throttle(context, &[], demux_streams, false);
            return false;
        }
        let has_unthrottled_timed_stream = demux_streams
            .iter()
            .any(|stream_index| decoder_input_timed_stream(context.decoder_input, *stream_index));
        let all_timed_streams_throttled = !has_unthrottled_timed_stream;
        self.observe_output_lead_throttle(
            context,
            &throttled_streams,
            demux_streams,
            all_timed_streams_throttled,
        );
        all_timed_streams_throttled
    }

    pub(super) fn observe_output_lead_throttle(
        &mut self,
        context: &DemuxPacketPumpContext<'_>,
        throttled_streams: &[(c_int, u64)],
        remaining_streams: &[c_int],
        active: bool,
    ) {
        let now = Instant::now();
        if !active {
            if let Some(started_at) = self.output_lead_throttle_started_at.take() {
                tracing::debug!(
                    session_id = ?context.session_id,
                    throttle_elapsed_ms = now
                        .saturating_duration_since(started_at)
                        .as_secs_f64()
                        * 1000.0,
                    suppressed_count = self.output_lead_throttle_suppressed_count,
                    "left FFmpeg demux output-lead throttle"
                );
            }
            self.output_lead_throttle_last_summary_at = None;
            self.output_lead_throttle_suppressed_count = 0;
            return;
        }
        let output_snapshot = context.video_admission_pressure.output_snapshot;
        if self.output_lead_throttle_started_at.is_none() {
            self.output_lead_throttle_started_at = Some(now);
            self.output_lead_throttle_last_summary_at = Some(now);
            tracing::debug!(
                session_id = ?context.session_id,
                throttled_streams = ?throttled_streams,
                remaining_streams = ?remaining_streams,
                output_reference_nsecs = context
                    .video_admission_pressure
                    .played_until_nsecs
                    .unwrap_or(context.current_start_position_nsecs),
                max_reader_output_lead_ms =
                    demux_reader_output_lead_limit_nsecs() as f64 / 1_000_000.0,
                output_state = ?output_snapshot.state,
                initial_start_phase = ?output_snapshot.state,
                first_frame_presented = output_snapshot.first_frame_presented,
                queued_video_frames = output_snapshot.queued_video_frames,
                queued_video_forward_ms = ?output_snapshot
                    .queued_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "entered FFmpeg demux output-lead throttle"
            );
            return;
        }
        self.output_lead_throttle_suppressed_count =
            self.output_lead_throttle_suppressed_count.saturating_add(1);
        if self
            .output_lead_throttle_last_summary_at
            .is_some_and(|last| {
                now.saturating_duration_since(last) < DEMUX_OUTPUT_LEAD_THROTTLE_SUMMARY_INTERVAL
            })
        {
            return;
        }
        let suppressed_count = std::mem::take(&mut self.output_lead_throttle_suppressed_count);
        self.output_lead_throttle_last_summary_at = Some(now);
        tracing::debug!(
            session_id = ?context.session_id,
            suppressed_count,
            throttle_elapsed_ms = self
                .output_lead_throttle_started_at
                .map(|started_at| now.saturating_duration_since(started_at).as_secs_f64() * 1000.0),
            throttled_streams = ?throttled_streams,
            queued_video_frames = output_snapshot.queued_video_frames,
            queued_video_forward_ms = ?output_snapshot
                .queued_video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "FFmpeg demux output-lead throttle summary"
        );
    }

    pub(super) fn ordered_demux_streams_for_context(
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

    pub(super) fn request_output_wait_audio_reader_head_realign_if_needed(
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
            .try_stream_reader_head_timeline(audio_stream_index)
        else {
            return;
        };

        let current_start_position_nsecs = context.pipeline.current_start_position_nsecs;
        context
            .pipeline
            .output_scheduler
            .request_output_wait_audio_reader_head_realign_if_needed(
                reader_head_start_nsecs,
                audio_waterline,
                current_start_position_nsecs,
                context.session_id,
            );
    }

    pub(super) fn record_rebuffer_audio_priority_result(
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
}
