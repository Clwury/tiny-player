use super::*;

impl VideoDecodePipeline {
    #[allow(clippy::too_many_arguments)]
    pub(in super::super) fn admit_demux_packet(
        &mut self,
        packet: &AvPacket,
        video_packet_count: &mut u64,
        playback_generation: &mut PlaybackGeneration,
        recovery: &mut VideoDecodeRecovery,
        dovi_pipeline: &mut DoviPipeline,
        skip_nonref_active: &mut bool,
        context: VideoPacketAdmissionContext,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        *video_packet_count = video_packet_count.saturating_add(1);
        let codec_id = context.video_stream.codec_id;
        let packet_nsecs = packet
            .best_timestamp()
            .and_then(|timestamp| timestamp_to_nsecs(timestamp, context.video_stream.time_base));
        let hardware_accelerated = self.info().hardware_accelerated;
        if let Some(observation) =
            self.observe_hevc_low_level_recovery_packet(packet, packet_nsecs, codec_id)
        {
            match observation {
                HevcLowLevelRecoveryObservationAction::CraLanding {
                    landing,
                    repeated,
                    reason,
                } => {
                    recovery.enable_hevc_low_level_recovery_point(landing);
                    tracing::warn!(
                        session_id = ?context.session_id,
                        transaction_id = landing.transaction_id,
                        recovery_scope = recovery.recovery_scope().as_str(),
                        reason,
                        target_nsecs = landing.target_nsecs,
                        seek_position_nsecs = landing.seek_position_nsecs,
                        actual_anchor_nsecs = landing.anchor_nsecs,
                        preroll_debt_nsecs =
                            landing.target_nsecs.saturating_sub(landing.anchor_nsecs),
                        actual_recovery_kind = landing.anchor_kind.as_str(),
                        range_id = ?landing.range_id,
                        anchor_packet_id = ?landing.anchor_packet_id,
                        repeated_low_level_landing = repeated,
                        repeat_low_level_seek_suppressed = repeated,
                        awaiting_closed_cached_interval = false,
                        arbitration_outcome = "decode_from_actual_landing",
                        "accepted CRA as the exact low-level seek decode anchor"
                    );
                }
                HevcLowLevelRecoveryObservationAction::SafeLanding { landing, reason } => {
                    recovery.enable_hevc_low_level_recovery_point(landing);
                    tracing::debug!(
                        session_id = ?context.session_id,
                        transaction_id = landing.transaction_id,
                        recovery_scope = recovery.recovery_scope().as_str(),
                        reason,
                        target_nsecs = landing.target_nsecs,
                        seek_position_nsecs = landing.seek_position_nsecs,
                        actual_anchor_nsecs = landing.anchor_nsecs,
                        preroll_debt_nsecs =
                            landing.target_nsecs.saturating_sub(landing.anchor_nsecs),
                        actual_recovery_kind = landing.anchor_kind.as_str(),
                        range_id = ?landing.range_id,
                        anchor_packet_id = ?landing.anchor_packet_id,
                        awaiting_closed_cached_interval = false,
                        arbitration_outcome = "decode_from_actual_landing",
                        "observed safe recovery point after low-level seek"
                    );
                }
            }
        }
        if let Some(progress) = recovery.observe_exact_seek_packet_progress(packet_nsecs) {
            self.hevc_decode_chain_watchdog
                .observe_exact_seek_packet_progress(context.session_id, progress);
        }
        let recovery_skipping_packet = recovery.should_skip_packet(packet, codec_id);
        tracing::trace!(
            session_id = ?context.session_id,
            packet_count = *video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            codec = ?codec_id,
            packet_bytes = packet.byte_len(),
            first_video_frame_pending = context.output_snapshot.first_video_frame_pending,
            recovery_waiting = recovery.waiting_for_keyframe(),
            recovery_skipped_packets = recovery.skipped_packets(),
            recovery_skipping_packet,
            "admitting FFmpeg video demux packet to decoder input"
        );
        if recovery_skipping_packet {
            let skipped_packets = recovery.record_skipped_packet(packet_nsecs);
            let skipped_span_nsecs = recovery.skipped_packet_span_nsecs();
            let fallback_target_nsecs = context
                .output_snapshot
                .video_output_rebuffer_anchor
                .map(|anchor| anchor.timeline_nsecs)
                .or(context.played_until_nsecs)
                .or(packet_nsecs)
                .unwrap_or_default();
            if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
                && self.hevc_same_hardware_recovery.is_none()
            {
                self.hevc_decode_chain_watchdog
                    .observe_post_soft_recovery_skipped_packet(
                        HevcPostSoftRecoverySkippedPacketObservation {
                            session_id: context.session_id,
                            packet_nsecs,
                            cache_sequence_contiguous: packet
                                .read_diagnostic()
                                .and_then(|diagnostic| diagnostic.sequence_contiguous)
                                .unwrap_or(true),
                            hardware_accelerated,
                            output_snapshot: context.output_snapshot,
                            demux_watermark: context.demux_watermark,
                            has_audio_output: context.has_audio_output,
                            fallback_target_nsecs,
                        },
                    );
            }
            if skipped_packets == 1 || skipped_packets.is_multiple_of(60) {
                tracing::debug!(
                    pts = ?packet.best_timestamp(),
                    packet_nsecs = ?packet_nsecs,
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    "skipping FFmpeg video packets while waiting for decode recovery point"
                );
            }
            if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
                && context.output_snapshot.rebuffering
                && !context.output_snapshot.video_decode_underfill
                && self.hevc_decode_chain_watchdog.pending_fallback.is_none()
                && !self
                    .hevc_decode_chain_watchdog
                    .recovery_progress_grace_active(Instant::now(), hardware_accelerated)
                && (skipped_span_nsecs
                    .is_some_and(|span| span >= HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS)
                    || skipped_packets > VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS)
            {
                self.hevc_decode_chain_watchdog.pending_fallback = Some(HevcDecodeChainFallback {
                    target_nsecs: fallback_target_nsecs,
                    reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
                });
                tracing::debug!(
                    session_id = ?context.session_id,
                    fallback_target_nsecs,
                    packet_nsecs = ?packet_nsecs,
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    output_state = ?context.output_snapshot.state,
                    "hevc_decode_chain_recovery_wait_hard"
                );
            }
            return Ok(DecodePacketAdmissionStatus::Dropped);
        }

        if recovery.accept_recovery_point(packet, codec_id) {
            tracing::debug!(
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                codec = ?codec_id,
                packet_bytes = packet.byte_len(),
                recovery_point = packet_is_video_recovery_point(packet, codec_id),
                recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
                safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                recovery_scope = recovery.recovery_scope().as_str(),
                exact_seek_output = recovery.requires_exact_seek_output(),
                "resuming FFmpeg video decode at recovery point"
            );
            let generation = playback_generation.advance();
            self.flush_buffers(generation)?;
        } else {
            let skipped_packets = recovery.skipped_packets();
            let skipped_span_nsecs = recovery.skipped_packet_span_nsecs();
            if recovery.accept_hevc_recovery_point_after_wait_limit(packet, codec_id) {
                tracing::warn!(
                    pts = ?packet.best_timestamp(),
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    skipped_packets,
                    skipped_span_ms =
                        ?skipped_span_nsecs.map(|span| span as f64 / 1_000_000.0),
                    hard_skip_ms = HEVC_DECODE_RECOVERY_WAIT_HARD_SKIP_NSECS as f64 / 1_000_000.0,
                    max_skipped_packets = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
                    "resuming FFmpeg HEVC video decode at recovery point after bounded wait"
                );
                let generation = playback_generation.advance();
                self.flush_buffers(generation)?;
            } else if recovery.accept_after_wait_limit(codec_id) {
                tracing::debug!(
                    pts = ?packet.best_timestamp(),
                    keyframe = packet.is_key(),
                    codec = ?codec_id,
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    recovery_kind = packet_video_recovery_point_kind(packet, codec_id).as_str(),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    max_skipped_packets = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
                    "resuming FFmpeg video decode after recovery point wait limit"
                );
                let generation = playback_generation.advance();
                self.flush_buffers(generation)?;
            }
        }

        log_video_decode_packet_if_needed(packet, codec_id, *video_packet_count, recovery);
        let dovi_packet_rewrite = inspect_hevc_dovi_rpu_decode_packet(
            packet,
            codec_id,
            context.video_stream,
            HevcDecodePacketLogContext {
                video_packet_count: *video_packet_count,
                first_video_frame_pending: context.output_snapshot.first_video_frame_pending,
                recovery_waiting: recovery.waiting_for_keyframe(),
            },
        )?;
        if let Some(metadata) = dovi_packet_rewrite.metadata().cloned() {
            tracing::trace!(
                pts = ?packet.best_timestamp(),
                profile = metadata.profile,
                profile5 = metadata.is_profile5(),
                rpu_bytes = metadata.rpu_payload.len(),
                "using Dolby Vision RPU metadata side channel for FFmpeg packet"
            );
            dovi_pipeline.observe_video_packet_metadata(packet, context.video_stream, metadata);
        } else {
            dovi_pipeline.observe_video_packet(packet, context.video_stream);
        }

        let bounded_decode_recovery_active = self.hevc_same_hardware_recovery.is_some();
        let skip_nonref_for_exact_seek = recovery.should_skip_nonref_for_seek_preroll(
            packet_nsecs,
            bounded_decode_recovery_active,
            hardware_accelerated,
        );
        let skip_nonref = context.skip_nonref_for_pressure || skip_nonref_for_exact_seek;
        if skip_nonref != *skip_nonref_active {
            self.set_skip_nonref_frames(skip_nonref)?;
            *skip_nonref_active = skip_nonref;
            tracing::debug!(
                session_id = ?context.session_id,
                transaction_id = ?recovery.recovery_scope().transaction_id(),
                recovery_scope = recovery.recovery_scope().as_str(),
                skip_nonref,
                skip_nonref_for_pressure = context.skip_nonref_for_pressure,
                skip_nonref_for_exact_seek,
                bounded_decode_recovery_active,
                output_state = ?context.output_snapshot.state,
                played_until_nsecs = context.played_until_nsecs,
                queued_video_frames = context.output_snapshot.queued_video_frames,
                queued_video_ms = context.output_snapshot.queued_video_duration_nsecs as f64
                    / 1_000_000.0,
                decoded_video_range = ?context.output_snapshot.queued_video_range_nsecs,
                decoded_video_forward_ms = ?context
                    .output_snapshot
                    .queued_video_forward_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                "updated FFmpeg video decoder non-reference frame skipping"
            );
        }

        let generation = playback_generation.advance();
        let decode_packet = dovi_packet_rewrite.decode_packet(packet);
        let hardware_accelerated = self.info().hardware_accelerated;
        let startup_target_nsecs = context
            .output_snapshot
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .or(context.played_until_nsecs)
            .unwrap_or_default();
        self.remember_hevc_hw_replay_packet(decode_packet, codec_id, context.session_id);
        let pending_packet = PendingVideoDecodePacket {
            generation,
            packet: AvPacket::ref_from(decode_packet)?,
            realign_after_decode_recovery: context.output_snapshot.first_video_frame_pending,
            hevc_startup_in_flight_watchdog: hevc_startup_in_flight_packet_should_arm(
                codec_id,
                hardware_accelerated,
                packet_nsecs,
                startup_target_nsecs,
            ),
            from_hevc_hw_replay: false,
            hevc_decode_recovery_evidence_scoped: bounded_decode_recovery_active,
        };
        let admission_status =
            self.try_enqueue_pending_packet(pending_packet, context.session_id)?;
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && recovery.recovery_scope() == VideoDecodeRecoveryScope::SafeBoundary
            && self.hevc_same_hardware_recovery.is_none()
        {
            self.hevc_decode_chain_watchdog
                .observe_submitted_safe_recovery_point(
                    context.session_id,
                    packet_nsecs,
                    packet_is_video_seek_point(decode_packet, codec_id),
                    hardware_accelerated,
                );
        }
        tracing::trace!(
            session_id = ?context.session_id,
            video_packet_admitted_count = *video_packet_count,
            admission_status = ?admission_status,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            codec = ?codec_id,
            packet_bytes = packet.byte_len(),
            "admitted FFmpeg video demux packet to decoder input"
        );
        Ok(admission_status)
    }
}
