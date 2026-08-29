use super::*;

impl PlaybackPipelineState {
    pub(in super::super::super) fn decoder_input_snapshot(
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
        let audio_snapshot = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_snapshot, audio_decode_snapshot);
        let audio_output_low_water = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs < duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)
        });
        let audio_input_suppressed = !self.output_scheduler.decode_recovery_active()
            && (self
                .output_scheduler
                .output_wait_audio_input_backpressured()
                || audio_input_suppressed_until_output_resume_state(
                    self.audio_decode_pipeline.is_some(),
                    self.output_scheduler.waiting_for_output_resume(),
                    audio_resume_waterline,
                ));
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
            audio_output_low_water,
            video_decode_snapshot,
            video_decode_blocked_on,
        }
    }

    pub(super) fn audio_input_suppressed_until_output_resume(&self) -> bool {
        if self.output_scheduler.decode_recovery_active() {
            return false;
        }
        let audio_decode_snapshot = self
            .audio_decode_pipeline
            .as_ref()
            .map(|pipeline| pipeline.snapshot());
        let audio_snapshot = self
            .audio_output
            .as_ref()
            .and_then(|output| output.snapshot().ok());
        let audio_resume_waterline =
            self.audio_resume_waterline_for_output_wait(audio_snapshot, audio_decode_snapshot);
        audio_input_suppressed_until_output_resume_state(
            self.audio_decode_pipeline.is_some(),
            self.output_scheduler.waiting_for_output_resume(),
            audio_resume_waterline,
        )
    }

    fn audio_resume_waterline_for_output_wait(
        &self,
        audio_snapshot: Option<super::super::super::AudioOutputSnapshot>,
        audio_decode_snapshot: Option<AudioDecodeWorkerSnapshot>,
    ) -> Option<AudioResumeWaterline> {
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

    pub(in super::super::super) fn video_packet_admission_pressure(
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

    pub(in super::super::super) fn video_output_resource_pressure_for(
        &self,
        output_snapshot: PlaybackOutputSnapshot,
        vo_snapshot: VideoOutputQueueSnapshot,
        audio_output_pending_nsecs: Option<u64>,
    ) -> bool {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let prepare_snapshot = self.video_frame_prepare_worker.snapshot();
        let prepare_worker_frames = prepare_snapshot
            .pending_input_frames
            .saturating_add(prepare_snapshot.in_flight_frames)
            .saturating_add(prepare_snapshot.completed_frames);
        let scheduled_video_queue_limit_reached = self
            .output_scheduler
            .scheduled_video_queue_limit_reached(self.subtitle_pipeline.needs_prefetch());
        video_output_resource_pressure(VideoOutputResourcePressure {
            scheduled_video_frames: self.output_scheduler.scheduled_video_queue_len(),
            decoded_video_frames: video_decode_snapshot.queued_frames,
            submitted_not_consumed_video_packets: video_decode_snapshot
                .submitted_not_consumed_packets,
            prepare_worker_frames,
            recovery_staging_frames: self.output_scheduler.recovery_staging_frames(),
            hardware_accelerated: self.video_decode_pipeline.info().hardware_accelerated,
            scheduled_video_queue_limit_reached,
            decode_recovery_video_admission_blocked: self
                .output_scheduler
                .decode_recovery_video_admission_blocked()
                || self
                    .video_decode_pipeline
                    .hevc_resource_pressure_demux_admission_stopped(),
            fill_phase_for_output_start: self.output_scheduler.output_fill_phase(),
            video_frame_duration_nsecs: self.video_frame_duration_nsecs,
            vo_queue_capacity: vo_snapshot.queue_capacity,
            vo_queued_frames: vo_snapshot.queued_frames,
            queued_video_forward_nsecs: output_snapshot.queued_video_forward_nsecs,
            audio_output_pending_nsecs,
            render_backlogged: vo_snapshot.render_backlogged(),
        })
    }

    pub(in super::super::super) fn admit_video_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
        pressure: VideoPacketAdmissionPressure,
        demux_watermark: DemuxReaderWatermark,
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
                demux_watermark,
                has_audio_output: self.audio_output.is_some(),
                skip_nonref_for_pressure: pressure.skip_nonref_for_pressure,
                played_until_nsecs: pressure.played_until_nsecs,
            },
        )
    }

    pub(in super::super::super) fn admit_audio_demux_packet(
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

    pub(in super::super::super) fn admit_subtitle_demux_packet(
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
