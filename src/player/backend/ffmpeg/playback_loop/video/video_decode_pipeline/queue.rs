use super::*;

impl VideoDecodePipeline {
    pub(in super::super) fn has_pending_or_in_flight(&self) -> bool {
        self.packets.has_pending_or_in_flight() || !self.hevc_hw_replay.is_empty()
    }

    pub(in super::super) fn take_pending_input(&mut self) -> Option<PendingVideoDecodePacket> {
        take_next_video_decode_input(&mut self.packets, &mut self.hevc_hw_replay)
    }

    pub(in super::super) fn push_in_flight(
        &mut self,
        packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) {
        let arm_hevc_startup_in_flight = packet.hevc_startup_in_flight_watchdog;
        let from_hevc_hw_replay = packet.from_hevc_hw_replay;
        self.packets.push_in_flight(packet);
        let now = Instant::now();
        self.hevc_decode_chain_watchdog
            .resume_startup_watchdog_after_packet_submission(now);
        if from_hevc_hw_replay {
            self.hevc_decode_chain_watchdog
                .observe_replay_packet_progress(now);
            if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
                transaction.last_progress_at = now;
            }
        }
        if arm_hevc_startup_in_flight {
            self.hevc_decode_chain_watchdog
                .arm_startup_in_flight_stall(session_id, now);
        }
    }

    pub(in super::super) fn front_generation(&self) -> Option<u64> {
        self.packets.front_generation()
    }

    pub(in super::super) fn front_realign_after_decode_recovery(&self, fallback: bool) -> bool {
        self.packets.front_realign_after_decode_recovery(fallback)
    }

    pub(in super::super) fn front_packet(&self) -> Option<&AvPacket> {
        self.packets.front_packet()
    }

    pub(in super::super) fn pop_completed_packet(&mut self) -> Option<PendingVideoDecodePacket> {
        self.packets.pop_completed_packet()
    }

    pub(in super::super) fn reopen_software_decoder(
        &mut self,
        stream: StreamInfo,
    ) -> std::result::Result<bool, String> {
        if !self.info().hardware_accelerated {
            return Ok(false);
        }
        if !runtime_hevc_software_fallback_allowed(self.requested_hardware_mode) {
            return Err(format!(
                "software decoder reopen blocked by hardware-mode invariant: requested_hw_mode={:?}",
                self.requested_hardware_mode
            ));
        }
        // Match mpv's force_fallback(): completely retire the failed hardware
        // AVCodecContext before selecting and opening the final software path.
        // At this point bounded Vulkan recovery is exhausted, so preserving an
        // already-failed worker for an atomic swap provides no useful rollback.
        self.worker
            .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
            .map_err(|error| format!("FFmpeg 硬解 worker 退役失败：{error}"))?;
        let decoder = Decoder::open_video(stream, HardwareDecodeMode::Off)
            .map_err(|error| format!("FFmpeg 重新打开软件视频解码器失败：{error}"))?;
        let worker = VideoDecodeWorker::spawn(decoder, self.frame_drop.epoch_handle())?;
        self.worker = worker;
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.clear_packets();
        self.hevc_decode_chain_watchdog.reset();
        Ok(true)
    }

    pub(super) fn remember_hevc_hw_replay_packet(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
        session_id: PlaybackSessionId,
    ) {
        if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC || !self.info().hardware_accelerated {
            return;
        }
        let time_base = self.info().time_base;
        let safe_anchor = hevc_packet_is_safe_replay_anchor(packet, codec_id);
        let packet_nsecs = hevc_replay_packet_start_nsecs(packet, time_base);
        let decoded_output_end_nsecs = max_optional_u64(
            self.hevc_decode_chain_watchdog.last_decoded_video_end_nsecs,
            self.hevc_decode_chain_watchdog
                .recent_output_high_water_nsecs,
        );
        let recovery_cutoff_locked = self.hevc_same_hardware_recovery.is_some()
            || self.hevc_decode_chain_watchdog.has_pending_fallback();
        let recent_evidence_would_preserve =
            self.hevc_decode_chain_watchdog.has_recent_gap_evidence()
                || self.hevc_decode_chain_watchdog.health_state == HevcDecodeHealthState::Suspected;
        let roll_safe_anchor = recent_evidence_would_preserve
            && hevc_safe_anchor_can_roll_past_preserved_evidence(
                safe_anchor,
                packet_nsecs,
                decoded_output_end_nsecs,
                recovery_cutoff_locked,
            );
        let preserve_safe_anchor =
            !roll_safe_anchor && (recovery_cutoff_locked || recent_evidence_would_preserve);
        let coverage_was_exhausted = self.hevc_hw_replay_journal.coverage_exhausted;
        let previous_anchor_nsecs = self.hevc_hw_replay_journal.anchor_nsecs;
        let previous_high_water_nsecs = self.hevc_hw_replay_journal.high_water_nsecs;
        let remember_result = if preserve_safe_anchor {
            self.hevc_hw_replay_journal
                .remember_preserving_safe_anchor(packet, codec_id, time_base)
        } else {
            self.hevc_hw_replay_journal
                .remember(packet, codec_id, time_base)
        };
        match remember_result {
            Ok(true) => {
                if roll_safe_anchor {
                    tracing::debug!(
                        session_id = ?session_id,
                        packet_nsecs,
                        decoded_output_end_nsecs,
                        previous_anchor_nsecs,
                        previous_high_water_nsecs,
                        previous_coverage_exhausted = coverage_was_exhausted,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_anchor_kind = ?self.hevc_hw_replay_journal.anchor_kind
                            .map(|kind| kind.as_str()),
                        "rolled HEVC recovery journal to safe anchor already covered by output"
                    );
                } else {
                    tracing::trace!(
                        session_id = ?session_id,
                        packet_pts = ?packet.best_timestamp(),
                        hevc_hw_replay_packets = self.hevc_hw_replay_journal.len(),
                        hevc_hw_replay_bytes = self.hevc_hw_replay_journal.total_bytes,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_anchor_kind = ?self.hevc_hw_replay_journal.anchor_kind
                            .map(|kind| kind.as_str()),
                        preserve_safe_anchor,
                        "remembered HEVC packet in safe hardware replay journal"
                    );
                }
            }
            Ok(false) => {
                if preserve_safe_anchor
                    && !coverage_was_exhausted
                    && self.hevc_hw_replay_journal.coverage_exhausted
                {
                    tracing::warn!(
                        session_id = ?session_id,
                        hevc_hw_replay_packets = self.hevc_hw_replay_journal.len(),
                        hevc_hw_replay_bytes = self.hevc_hw_replay_journal.total_bytes,
                        rejected_packet_bytes = packet.byte_len(),
                        hevc_hw_replay_packet_limit = HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
                        hevc_hw_replay_byte_limit = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
                        hevc_hw_replay_duration_limit_ms =
                            HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS as f64 / 1_000_000.0,
                        hevc_hw_replay_anchor_nsecs = ?self.hevc_hw_replay_journal.anchor_nsecs,
                        hevc_hw_replay_high_water_nsecs = ?self.hevc_hw_replay_journal.high_water_nsecs,
                        "HEVC recovery journal exhausted its bounded packet, byte, or duration coverage"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    session_id = ?session_id,
                    %error,
                    "failed to remember HEVC hardware replay packet"
                );
            }
        }
    }
}
