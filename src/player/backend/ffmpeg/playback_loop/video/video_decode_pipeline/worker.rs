use super::*;

impl VideoDecodePipeline {
    #[allow(clippy::too_many_arguments)]
    pub(in super::super) fn recover_error_if_needed(
        &mut self,
        result: std::result::Result<(), String>,
        playback_generation: &mut PlaybackGeneration,
        codec_id: ffi::AVCodecID,
        packet: &AvPacket,
        recovery: &mut VideoDecodeRecovery,
        realign_after_recovery_point: bool,
        committed_output_high_water_nsecs: Option<u64>,
    ) -> std::result::Result<bool, String> {
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            && let Err(error) = &result
        {
            self.last_hevc_decode_error = Some(error.clone());
        }
        match result {
            Ok(()) => Ok(false),
            Err(error)
                if video_decode_error_requires_hevc_resource_pressure_recovery(
                    &error,
                    codec_id,
                    self.info().hardware_accelerated,
                ) =>
            {
                let packet_nsecs = packet
                    .read_diagnostic()
                    .and_then(|diagnostic| diagnostic.packet_start_nsecs)
                    .or_else(|| {
                        packet.best_timestamp().and_then(|timestamp| {
                            timestamp_to_nsecs(timestamp, self.info().time_base)
                        })
                    });
                let target_nsecs = committed_output_high_water_nsecs
                    .or(self.hevc_decode_chain_watchdog.last_decoded_video_end_nsecs)
                    .or(packet_nsecs)
                    .unwrap_or_default();
                self.request_hevc_resource_pressure_recovery(
                    target_nsecs,
                    packet_nsecs.map(|packet| packet.max(target_nsecs)),
                    &error,
                    Instant::now(),
                );
                let decoder_epoch = self.decoder_epoch;
                let release_external_references = self
                    .hevc_same_hardware_recovery
                    .as_mut()
                    .is_some_and(|transaction| {
                        transaction.claim_resource_pressure_external_release(decoder_epoch)
                    });
                recovery.reset();
                Ok(release_external_references)
            }
            Err(error) if video_decode_error_is_recoverable(&error) => {
                tracing::debug!(
                    %error,
                    codec = ?codec_id,
                    packet_pts = ?packet.best_timestamp(),
                    packet_keyframe = packet.is_key(),
                    packet_bytes = packet.byte_len(),
                    recovery_point = packet_is_video_recovery_point(packet, codec_id),
                    safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                    recovery_waiting_before = recovery.waiting_for_keyframe(),
                    recovery_skipped_packets = recovery.skipped_packets,
                    realign_after_recovery_point,
                    resource_pressure = video_decode_error_is_resource_pressure(&error),
                    "recovering FFmpeg video decoder after recoverable decode error"
                );
                let generation = playback_generation.advance();
                self.flush_buffers(generation)?;
                recovery.begin_with_realign(realign_after_recovery_point);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(in super::super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodedFrame>, String> {
        let result = self.worker.poll_frame(generation);
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
    }

    pub(in super::super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodePacketStatus>, String> {
        let result = self.worker.poll_packet_status(generation);
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
    }

    pub(in super::super) fn flush_buffers(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.worker.flush_buffers(generation)?;
        self.clear_packets();
        Ok(())
    }

    pub(in super::super) fn service_worker(&mut self) -> std::result::Result<(), String> {
        let result = self.worker.service();
        self.observe_hevc_same_hardware_worker_progress(Instant::now());
        result
    }

    pub(in super::super) fn request_drain(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.worker.request_drain(generation)
    }

    pub(in super::super) fn poll_drain_result(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodeDrainResult>, String> {
        self.worker.poll_drain_result(generation)
    }

    pub(in super::super) fn clear_packets(&mut self) {
        self.packets.clear();
        self.hevc_hw_replay.clear();
    }

    pub(in super::super) fn reset_hevc_decode_chain_transient_state(&mut self) {
        self.hevc_decode_chain_watchdog.reset();
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::Transient,
        );
    }

    pub(in super::super) fn reset_hevc_decoder_transient_preserving_gap_evidence(
        &mut self,
        now: Instant,
    ) {
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
    }

    pub(in super::super) fn reset_hevc_decode_chain_recovery_transaction(&mut self) {
        self.reset_hevc_decode_chain_transient_state();
        self.hevc_hw_replay_journal.clear();
        self.hevc_same_hardware_recovery = None;
        self.last_hevc_decode_error = None;
        self.last_hevc_decode_chain_fallback = hevc_decode_chain_recovery_record_after_reset(
            self.last_hevc_decode_chain_fallback,
            HevcDecodeChainResetScope::RecoveryTransaction,
        );
        self.hevc_low_level_seek_observation = None;
        self.last_hevc_cra_low_level_landing = None;
    }
}
