use super::*;

impl VideoDecodePipeline {
    pub(in super::super) fn begin_hevc_low_level_seek_observation(
        &mut self,
        transaction_id: u64,
        target_nsecs: u64,
        seek_position_nsecs: u64,
        reason: &'static str,
    ) -> bool {
        if self.hevc_low_level_seek_would_repeat_cra(target_nsecs, seek_position_nsecs) {
            return false;
        }
        self.hevc_low_level_seek_observation = Some(HevcLowLevelSeekObservation {
            transaction_id,
            target_nsecs,
            seek_position_nsecs,
            reason,
            landing: None,
        });
        true
    }

    pub(in super::super) fn hevc_low_level_seek_would_repeat_cra(
        &self,
        target_nsecs: u64,
        seek_position_nsecs: u64,
    ) -> bool {
        hevc_low_level_seek_would_repeat_cra(
            self.last_hevc_cra_low_level_landing,
            target_nsecs,
            seek_position_nsecs,
        )
    }

    pub(in super::super) fn finish_hevc_low_level_exact_recovery(
        &mut self,
        transaction_id: u64,
    ) -> Option<HevcLowLevelSeekLanding> {
        let observation = self.hevc_low_level_seek_observation?;
        if observation.transaction_id != transaction_id {
            return None;
        }
        self.hevc_low_level_seek_observation = None;
        observation.landing
    }

    pub(in super::super) fn clear_hevc_low_level_seek_recovery(
        &mut self,
    ) -> Option<HevcLowLevelSeekLanding> {
        self.hevc_low_level_seek_observation
            .take()
            .and_then(|observation| observation.landing)
    }

    pub(super) fn observe_hevc_low_level_recovery_packet(
        &mut self,
        packet: &AvPacket,
        packet_nsecs: Option<u64>,
        codec_id: ffi::AVCodecID,
    ) -> Option<HevcLowLevelRecoveryObservationAction> {
        let observation = self.hevc_low_level_seek_observation?;
        if observation.landing.is_some() || codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return None;
        }
        let cache_read = packet.read_diagnostic();
        let recovery_kind = cache_read
            .map(|diagnostic| diagnostic.recovery_kind)
            .unwrap_or_else(|| packet_video_recovery_point_kind(packet, codec_id));
        if !recovery_kind.is_recovery_point() {
            return None;
        }
        let anchor_nsecs = cache_read
            .and_then(|diagnostic| diagnostic.packet_start_nsecs)
            .or(packet_nsecs)?;
        let landing = HevcLowLevelSeekLanding {
            transaction_id: observation.transaction_id,
            target_nsecs: observation.target_nsecs,
            seek_position_nsecs: observation.seek_position_nsecs,
            anchor_nsecs,
            anchor_kind: recovery_kind,
            range_id: cache_read.map(|diagnostic| diagnostic.read_range_id),
            anchor_packet_id: cache_read.map(|diagnostic| diagnostic.packet_id),
        };
        if recovery_kind == VideoRecoveryPointKind::Cra {
            let repeated = self
                .last_hevc_cra_low_level_landing
                .is_some_and(|previous| hevc_cra_low_level_landing_repeats(previous, landing));
            self.last_hevc_cra_low_level_landing = Some(landing);
            if let Some(current) = self.hevc_low_level_seek_observation.as_mut() {
                current.landing = Some(landing);
            }
            return Some(HevcLowLevelRecoveryObservationAction::CraLanding {
                landing,
                repeated,
                reason: observation.reason,
            });
        }
        if let Some(current) = self.hevc_low_level_seek_observation.as_mut() {
            current.landing = Some(landing);
        }
        Some(HevcLowLevelRecoveryObservationAction::SafeLanding {
            landing,
            reason: observation.reason,
        })
    }

    pub(in super::super) fn observe_hevc_decode_packet_status(
        &mut self,
        observation: HevcDecodePacketObservation<'_>,
    ) -> HevcDecodeChainRecoveryAction {
        if observation.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.hevc_decode_chain_watchdog.reset();
            self.hevc_decode_packet_diagnostics.clear();
            self.hevc_hw_replay_journal.clear();
            return HevcDecodeChainRecoveryAction::None;
        }
        let packet_nsecs = observation
            .packet
            .read_diagnostic()
            .and_then(|diagnostic| diagnostic.packet_start_nsecs)
            .or_else(|| {
                observation.packet.best_timestamp().and_then(|timestamp| {
                    timestamp_to_nsecs(timestamp, observation.video_stream.time_base)
                })
            });
        let hardware_accelerated = self.info().hardware_accelerated;
        let now = Instant::now();
        let exact_seek_scoped = self
            .hevc_decode_chain_watchdog
            .observe_exact_seek_decoder_result(
                observation.recovery_scope,
                packet_nsecs,
                observation.status.decoded_frames,
                observation.status.result.is_ok(),
                now,
            );
        let evidence_scope = hevc_decode_packet_evidence_scope(
            exact_seek_scoped,
            observation.decode_recovery_active,
            self.hevc_same_hardware_recovery.is_some(),
            observation.packet_decode_recovery_scoped,
        );
        let action = match evidence_scope {
            HevcDecodePacketEvidenceScope::ExactSeek => HevcDecodeChainRecoveryAction::None,
            HevcDecodePacketEvidenceScope::DecodeRecovery => {
                self.hevc_decode_chain_watchdog
                    .observe_packet_during_decode_recovery(
                        observation.status.result.is_ok(),
                        observation.status.decoded_frames,
                        now,
                    );
                HevcDecodeChainRecoveryAction::None
            }
            HevcDecodePacketEvidenceScope::Playback => self
                .hevc_decode_chain_watchdog
                .observe_packet(HevcDecodeChainWatchdogInput {
                    session_id: observation.session_id,
                    packet_nsecs,
                    safe_seek_point: hevc_packet_is_safe_replay_anchor(
                        observation.packet,
                        observation.video_stream.codec_id,
                    ),
                    decoded_frames: observation.status.decoded_frames,
                    decode_ok: observation.status.result.is_ok(),
                    hardware_accelerated,
                    output_snapshot: observation.output_snapshot,
                    demux_watermark: observation.demux_watermark,
                    has_audio_output: observation.has_audio_output,
                    synchronized_audio_timeline_gap_checked: observation
                        .synchronized_audio_timeline_gap_checked,
                    synchronized_audio_timeline_gap: observation.synchronized_audio_timeline_gap,
                    cache_sequence_contiguous: observation
                        .packet
                        .read_diagnostic()
                        .and_then(|diagnostic| diagnostic.sequence_contiguous)
                        .unwrap_or(true),
                    fallback_target_nsecs: observation.fallback_target_nsecs,
                    now,
                }),
        };
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.observe_packet(
                observation.generation,
                packet_nsecs,
                observation.status.decoded_frames,
            );
        }
        let zero_output_run_packets = match evidence_scope {
            HevcDecodePacketEvidenceScope::ExactSeek => {
                self.hevc_decode_chain_watchdog
                    .exact_seek_zero_output_packets
            }
            HevcDecodePacketEvidenceScope::DecodeRecovery => 0,
            HevcDecodePacketEvidenceScope::Playback if observation.status.decoded_frames == 0 => {
                self.hevc_decode_chain_watchdog.zero_output_packets
            }
            HevcDecodePacketEvidenceScope::Playback => 0,
        };
        self.hevc_decode_packet_diagnostics.record(
            observation.status,
            observation.packet,
            observation.video_stream,
            zero_output_run_packets,
            hardware_accelerated,
        );
        action
    }

    pub(in super::super) fn observe_hevc_decoded_frame_gap(
        &mut self,
        mut observation: HevcDecodedFrameGapObservation,
    ) -> HevcDecodedFrameGapAction {
        observation.recent_cache_read_anomaly =
            self.hevc_decode_packet_diagnostics.has_cache_read_anomaly();
        observation.decode_recovery_active |= self.hevc_same_hardware_recovery.is_some();
        let action = self
            .hevc_decode_chain_watchdog
            .observe_decoded_frame_gap(observation);
        if observation.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && observation
                .previous_gap_nsecs
                .and_then(|gap| u64::try_from(gap).ok())
                .is_some_and(|gap| {
                    !video_timestamp_gap_within_threshold(gap, observation.max_gap_nsecs)
                })
        {
            self.log_hevc_decoded_frame_gap_diagnostics(observation, action);
        }
        action
    }

    pub(super) fn log_hevc_decoded_frame_gap_diagnostics(
        &self,
        observation: HevcDecodedFrameGapObservation,
        action: HevcDecodedFrameGapAction,
    ) {
        let frame = observation.source_frame_diagnostic;
        let front_generation = self.front_generation();
        let front_packet = self.front_packet().map(|packet| {
            HevcPacketDiagnosticFields::from_packet(
                packet,
                observation.codec_id,
                self.info().time_base,
            )
        });
        let non_contiguous_cache_reads = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet
                    .packet
                    .cache_read
                    .is_some_and(|cache| cache.sequence_contiguous == Some(false))
            })
            .count();
        let packets_without_cache_diagnostic = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| packet.packet.cache_read.is_none())
            .count();
        let non_monotonic_dts_contiguous_cache_packets = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet.dts_delta_nsecs.is_some_and(|delta| delta < 0)
                    && packet
                        .packet
                        .cache_read
                        .is_some_and(|cache| cache.sequence_contiguous == Some(true))
            })
            .count();
        let repeated_cache_packet_reads = self
            .hevc_decode_packet_diagnostics
            .packets
            .iter()
            .filter(|packet| {
                packet.packet.cache_read.is_some_and(|cache| {
                    cache.previous_read_generation == Some(cache.cache_generation)
                        && cache.previous_read_packet_id == Some(cache.packet_id)
                })
            })
            .count();
        tracing::debug!(
            session_id = ?observation.session_id,
            action = ?action,
            decoder_name = %self.info().decoder_name,
            hardware_accelerated = self.info().hardware_accelerated,
            video_time_base_num = self.info().time_base.num,
            video_time_base_den = self.info().time_base.den,
            frame_timeline_nsecs = observation.timeline_nsecs,
            frame_duration_nsecs = observation.duration_nsecs,
            previous_expected_next_nsecs = ?observation.previous_expected_next_nsecs,
            previous_gap_ms = ?observation
                .previous_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            max_gap_ms = observation.max_gap_nsecs as f64 / 1_000_000.0,
            frame_best_effort_timestamp = frame.best_effort_timestamp,
            frame_pts = frame.pts,
            frame_packet_dts = frame.packet_dts,
            frame_raw_duration = frame.duration,
            frame_flags = frame.flags,
            frame_key = frame.key_frame,
            frame_corrupt = frame.corrupt,
            frame_picture_type = frame.picture_type,
            frame_decode_error_flags = frame.decode_error_flags,
            frame_width = frame.width,
            frame_height = frame.height,
            frame_pixel_format = frame.pixel_format,
            recovery_waiting = observation.recovery_waiting,
            demux_underrun = observation.demux_watermark.underrun,
            demux_video_underrun = observation.demux_watermark.video_underrun,
            demux_audio_underrun = observation.demux_watermark.audio_underrun,
            demux_video_forward_ms = ?observation
                .demux_watermark
                .video_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?observation
                .demux_watermark
                .audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_selected_min_forward_ms = ?observation
                .demux_watermark
                .selected_min_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            recent_completed_packet_diagnostics =
                self.hevc_decode_packet_diagnostics.packets.len(),
            non_contiguous_cache_reads,
            repeated_cache_packet_reads,
            packets_without_cache_diagnostic,
            non_monotonic_dts_contiguous_cache_packets,
            current_front_generation = ?front_generation,
            current_front_stream_index = ?front_packet.map(|packet| packet.stream_index),
            current_front_pts = ?front_packet.and_then(|packet| packet.pts),
            current_front_dts = ?front_packet.and_then(|packet| packet.dts),
            current_front_pts_nsecs = ?front_packet.and_then(|packet| packet.pts_nsecs),
            current_front_dts_nsecs = ?front_packet.and_then(|packet| packet.dts_nsecs),
            current_front_duration = ?front_packet.and_then(|packet| packet.duration),
            current_front_flags = ?front_packet.map(|packet| packet.flags),
            current_front_key = ?front_packet.map(|packet| packet.key_frame),
            current_front_recovery_point = ?front_packet.map(|packet| packet.recovery_point),
            current_front_safe_seek_point = ?front_packet.map(|packet| packet.safe_seek_point),
            current_front_packet_bytes = ?front_packet.map(|packet| packet.byte_len),
            current_front_cache_read = ?front_packet.and_then(|packet| packet.cache_read),
            "HEVC decoded frame PTS gap diagnostic snapshot"
        );

        for packet in &self.hevc_decode_packet_diagnostics.packets {
            let cache = packet.packet.cache_read;
            tracing::debug!(
                session_id = ?observation.session_id,
                diagnostic_ordinal = packet.ordinal,
                generation = packet.generation,
                hardware_accelerated = packet.hardware_accelerated,
                packet_stream_index = packet.packet.stream_index,
                packet_pts = ?packet.packet.pts,
                packet_dts = ?packet.packet.dts,
                packet_pts_nsecs = ?packet.packet.pts_nsecs,
                packet_dts_nsecs = ?packet.packet.dts_nsecs,
                packet_duration = ?packet.packet.duration,
                packet_duration_nsecs = ?packet.packet.duration_nsecs,
                packet_pts_delta_ms = ?packet
                    .pts_delta_nsecs
                    .map(|delta| delta as f64 / 1_000_000.0),
                packet_dts_delta_ms = ?packet
                    .dts_delta_nsecs
                    .map(|delta| delta as f64 / 1_000_000.0),
                packet_flags = packet.packet.flags,
                packet_key = packet.packet.key_frame,
                packet_recovery_point = packet.packet.recovery_point,
                packet_safe_seek_point = packet.packet.safe_seek_point,
                packet_bytes = packet.packet.byte_len,
                decoded_frames = packet.decoded_frames,
                zero_output_run_packets = packet.zero_output_run_packets,
                decode_ok = packet.decode_ok,
                decode_error = ?packet.decode_error,
                decode_elapsed_ms = packet.decode_elapsed_micros as f64 / 1_000.0,
                drained = packet.drained,
                cache_read_sequence = ?cache.map(|cache| cache.read_sequence),
                cache_generation = ?cache.map(|cache| cache.cache_generation),
                cache_read_range_id = ?cache.map(|cache| cache.read_range_id),
                cache_packet_id = ?cache.map(|cache| cache.packet_id),
                cache_stream_offset = ?cache.map(|cache| cache.stream_offset),
                cache_storage = ?cache.map(|cache| cache.storage),
                cache_read_index_before = ?cache.map(|cache| cache.read_index_before),
                cache_read_index_after = ?cache.map(|cache| cache.read_index_after),
                cache_reader_head_before = ?cache.and_then(|cache| cache.reader_head_before),
                cache_reader_head_after = ?cache.and_then(|cache| cache.reader_head_after),
                cache_previous_read_packet_id =
                    ?cache.and_then(|cache| cache.previous_read_packet_id),
                cache_previous_read_generation =
                    ?cache.and_then(|cache| cache.previous_read_generation),
                cache_previous_expected_next_packet_id =
                    ?cache.and_then(|cache| cache.previous_expected_next_packet_id),
                cache_sequence_contiguous = ?cache.and_then(|cache| cache.sequence_contiguous),
                cache_packet_start_nsecs = ?cache.and_then(|cache| cache.packet_start_nsecs),
                cache_packet_end_nsecs = ?cache.and_then(|cache| cache.packet_end_nsecs),
                cache_timeline_anchor = ?cache.map(|cache| cache.timeline_anchor),
                cache_recovery_point = ?cache.map(|cache| cache.recovery_point),
                cache_safe_seek_point = ?cache.map(|cache| cache.safe_seek_point),
                "HEVC decoded frame PTS gap recent decode packet diagnostic"
            );
        }
    }

    pub(in super::super) fn observe_hevc_seek_preroll_progress(
        &mut self,
        observation: HevcSeekPrerollProgressObservation,
    ) {
        self.hevc_decode_chain_watchdog
            .observe_seek_preroll_progress(observation);
    }

    pub(in super::super) fn complete_hevc_exact_seek_evidence(
        &mut self,
        completion: ExactSeekCompletion,
        decode_recovery_active: bool,
    ) {
        let preserve_playback_evidence =
            decode_recovery_active || self.hevc_same_hardware_recovery.is_some();
        let promote_failed_seek_evidence = self.info().hardware_accelerated;
        self.hevc_decode_chain_watchdog
            .complete_exact_seek_evidence_scope(
                completion.transaction_id,
                completion.first_eligible_frame_nsecs,
                preserve_playback_evidence,
                promote_failed_seek_evidence,
                Instant::now(),
            );
    }
}
