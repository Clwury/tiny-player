use super::*;

impl VideoDecodePipeline {
    pub(in super::super) fn request_hevc_same_hardware_recovery(
        &mut self,
        fallback: HevcDecodeChainFallback,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        if !self.info().hardware_accelerated {
            return HevcDecodeRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();

        let snapshot = self.snapshot();
        if self.hevc_same_hardware_recovery.is_none() {
            let mut transaction = HevcSameHardwareRecoveryTransaction::new(
                fallback,
                snapshot.result_produced_sequence,
                self.last_hevc_decode_error.clone(),
                now,
            );
            transaction.set_root_evidence(
                self.hevc_decode_chain_watchdog.recent_zero_output_packets,
                self.hevc_decode_chain_watchdog
                    .recent_input_packet_high_water_nsecs,
                self.hevc_decode_chain_watchdog
                    .recent_output_high_water_nsecs,
            );
            tracing::warn!(
                target_nsecs = transaction.target_nsecs,
                reason = transaction.reason.as_str(),
                root_zero_output_packets = transaction.root_zero_output_packets,
                root_input_high_water_nsecs = ?transaction.root_input_high_water_nsecs,
                root_output_high_water_nsecs = ?transaction.root_output_high_water_nsecs,
                decoder_epoch = self.decoder_epoch,
                same_hw_recovery_phase = transaction.phase.as_str(),
                submitted_sequence = snapshot.submitted_sequence,
                result_produced_sequence = snapshot.result_produced_sequence,
                result_consumed_sequence = snapshot.result_consumed_sequence,
                oldest_submitted_packet_nsecs = ?snapshot.oldest_submitted_packet_nsecs,
                "started bounded HEVC same-Vulkan recovery transaction"
            );
            self.hevc_same_hardware_recovery = Some(transaction);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }

        if self
            .hevc_same_hardware_recovery
            .as_ref()
            .is_some_and(|transaction| {
                !hevc_fallback_targets_match(transaction.target_nsecs, fallback.target_nsecs)
            })
        {
            let transaction = self
                .hevc_same_hardware_recovery
                .as_ref()
                .expect("same-hardware recovery transaction exists");
            tracing::warn!(
                root_target_nsecs = transaction.target_nsecs,
                observed_target_nsecs = fallback.target_nsecs,
                root_reason = transaction.reason.as_str(),
                observed_reason = fallback.reason.as_str(),
                same_hw_recovery_phase = transaction.phase.as_str(),
                "kept bounded same-Vulkan transaction across fallback target drift"
            );
        }

        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware recovery transaction exists");
        transaction.observed_target_nsecs = fallback.target_nsecs;
        if transaction.expired(now) {
            transaction.fail("same-Vulkan recovery wall-time limit exceeded");
            return transaction.terminal_action(self.requested_hardware_mode);
        }
        if transaction.failed_attempt_needs_decoder_drain(snapshot, now) {
            transaction.observe_result_progress(snapshot.result_produced_sequence, now);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }
        transaction.advance_after_repeated_failure_if_idle(
            snapshot.result_produced_sequence,
            now,
            self.requested_hardware_mode,
        )
    }

    pub(in super::super) fn request_hevc_resource_pressure_recovery(
        &mut self,
        target_nsecs: u64,
        cutoff_nsecs: Option<u64>,
        error: &str,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        if !self.info().hardware_accelerated {
            return HevcDecodeRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();

        let snapshot = self.snapshot();
        if self.hevc_same_hardware_recovery.is_none() {
            let fallback = HevcDecodeChainFallback {
                target_nsecs,
                reason: HevcDecodeChainFallbackReason::ResourcePressure,
            };
            let mut transaction = HevcSameHardwareRecoveryTransaction::new(
                fallback,
                snapshot.result_produced_sequence,
                Some(error.to_string()),
                now,
            );
            transaction.set_root_evidence(0, cutoff_nsecs, Some(target_nsecs));
            transaction.record_resource_pressure_error(error, cutoff_nsecs, now);
            tracing::warn!(
                target_nsecs = transaction.target_nsecs,
                frozen_cutoff_nsecs = ?transaction.replay_required_high_water_nsecs,
                decoder_epoch = self.decoder_epoch,
                same_hw_recovery_phase = transaction.phase.as_str(),
                submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
                "started release-first HEVC Vulkan resource-pressure recovery"
            );
            self.hevc_same_hardware_recovery = Some(transaction);
            return HevcDecodeRecoveryAction::FlushSameHardware;
        }

        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware recovery transaction exists");
        transaction.promote_to_resource_pressure(target_nsecs, cutoff_nsecs, error, now);
        if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
            transaction.finish_active_attempt("resource_pressure_escalated_to_reopen", now);
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            HevcDecodeRecoveryAction::ReopenSameHardware
        } else {
            transaction.phase = HevcSameHardwareRecoveryPhase::Flushing;
            HevcDecodeRecoveryAction::FlushSameHardware
        }
    }

    pub(in super::super) fn hevc_same_hardware_recovery_is_resource_pressure(&self) -> bool {
        self.hevc_same_hardware_recovery
            .as_ref()
            .is_some_and(HevcSameHardwareRecoveryTransaction::resource_pressure)
    }

    pub(in super::super) fn hevc_resource_pressure_demux_admission_stopped(&self) -> bool {
        self.hevc_same_hardware_recovery.as_ref().is_some_and(
            HevcSameHardwareRecoveryTransaction::resource_pressure_demux_admission_stopped,
        )
    }

    pub(in super::super) fn hevc_resource_pressure_decoder_input_stopped(&self) -> bool {
        self.hevc_same_hardware_recovery.as_ref().is_some_and(
            HevcSameHardwareRecoveryTransaction::resource_pressure_decoder_input_stopped,
        )
    }

    pub(in super::super) fn pending_hevc_same_hardware_recovery_action(
        &mut self,
        now: Instant,
    ) -> HevcDecodeRecoveryAction {
        let snapshot = self.snapshot();
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return HevcDecodeRecoveryAction::None;
        };
        if transaction.expired(now)
            && !matches!(
                transaction.phase,
                HevcSameHardwareRecoveryPhase::Recovered | HevcSameHardwareRecoveryPhase::Failed
            )
        {
            transaction.fail("same-Vulkan recovery wall-time limit exceeded");
        }
        if transaction.failed_attempt_needs_decoder_drain(snapshot, now) {
            transaction.observe_result_progress(snapshot.result_produced_sequence, now);
            return HevcDecodeRecoveryAction::DrainPendingResults;
        }
        transaction.advance_after_repeated_failure_if_idle(
            snapshot.result_produced_sequence,
            now,
            self.requested_hardware_mode,
        )
    }

    pub(in super::super) fn record_hevc_same_hardware_drain_pass(
        &mut self,
        made_progress: bool,
        decoder_work_pending: bool,
        now: Instant,
    ) -> bool {
        self.observe_hevc_same_hardware_worker_progress(now);
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        transaction.record_decoder_drain_pass(made_progress, decoder_work_pending, now)
    }

    pub(in super::super) fn begin_hevc_same_hardware_flush(
        &mut self,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("HEVC same-Vulkan flush requested without a transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::Flushing {
            return Err(format!(
                "HEVC same-Vulkan flush requested in phase {}",
                transaction.phase.as_str()
            ));
        }
        if transaction.flush_attempts >= HEVC_SAME_HARDWARE_MAX_FLUSH_ATTEMPTS {
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            transaction.last_error = Some("same-decoder flush attempt limit reached".to_string());
            return Err("HEVC same-decoder flush attempt limit reached".to_string());
        }
        transaction.flush_attempts = transaction.flush_attempts.saturating_add(1);
        transaction.last_progress_at = now;
        if let Err(error) = self.flush_buffers(generation) {
            let transaction = self
                .hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives flush failure");
            transaction.last_error = Some(format!("same-decoder flush failed: {error}"));
            transaction.phase = HevcSameHardwareRecoveryPhase::Reopening;
            return Err(error);
        }
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware transaction survives flush");
        transaction.begin_attempt(
            self.decoder_epoch,
            HevcSameHardwareRecoveryAttemptKind::FlushReplay,
            generation,
            now,
        );
        transaction.phase = HevcSameHardwareRecoveryPhase::ReplayingAfterFlush;
        Ok(())
    }

    pub(in super::super) fn begin_hevc_same_hardware_reopen(
        &mut self,
        stream: StreamInfo,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<Arc<VulkanDecodeDevice>, String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("HEVC same-Vulkan reopen requested without a transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::Reopening {
            return Err(format!(
                "HEVC same-Vulkan reopen requested in phase {}",
                transaction.phase.as_str()
            ));
        }
        if transaction.reopen_attempts >= HEVC_SAME_HARDWARE_MAX_REOPEN_ATTEMPTS {
            transaction.fail("same-Vulkan reopen attempt limit reached");
            return Err("HEVC same-Vulkan reopen attempt limit reached".to_string());
        }
        transaction.reopen_attempts = transaction.reopen_attempts.saturating_add(1);
        let release_first = transaction.resource_pressure();

        // mpv's force_fallback() tears down the failed AVCodecContext before
        // opening its replacement. Preserve the atomic open-first swap for
        // ordinary corruption recovery, but never keep two Vulkan pools alive
        // while recovering from device-memory pressure.
        if release_first
            && let Err(error) = self
                .worker
                .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
        {
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives worker retirement failure")
                .fail(format!(
                    "same-Vulkan old worker release-first retirement failed: {error}"
                ));
            return Err(error);
        }

        // Force the candidate open to remain hardware-only even when the original
        // policy was Auto. Software fallback is a separate terminal action.
        let decoder = match Decoder::open_video(stream, hevc_same_hardware_reopen_mode()) {
            Ok(decoder) => decoder,
            Err(error) => {
                let transaction = self
                    .hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives reopen failure");
                transaction.fail(format!("same-Vulkan decoder open failed: {error}"));
                return Err(error);
            }
        };
        if !decoder.is_hardware_accelerated() {
            let error = "same-Vulkan candidate unexpectedly opened without hardware acceleration"
                .to_string();
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives invalid candidate")
                .fail(error.clone());
            return Err(error);
        }
        let Some(device) = decoder.vulkan_device() else {
            let error = "same-Vulkan candidate did not expose a Vulkan decode device".to_string();
            self.hevc_same_hardware_recovery
                .as_mut()
                .expect("same-hardware transaction survives missing device")
                .fail(error.clone());
            return Err(error);
        };
        let worker = match VideoDecodeWorker::spawn(decoder) {
            Ok(worker) => worker,
            Err(error) => {
                self.hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives worker spawn failure")
                    .fail(format!("same-Vulkan worker spawn failed: {error}"));
                return Err(error);
            }
        };

        if !release_first {
            // Ordinary recovery retains the open-first atomic swap.
            if let Err(error) = self
                .worker
                .shutdown_and_join(HEVC_SAME_HARDWARE_WORKER_RETIRE_TIMEOUT)
            {
                self.hevc_same_hardware_recovery
                    .as_mut()
                    .expect("same-hardware transaction survives worker retirement failure")
                    .fail(format!("same-Vulkan old worker retirement failed: {error}"));
                return Err(error);
            }
        }
        self.worker = worker;
        self.clear_packets();
        self.decoder_epoch = self.decoder_epoch.saturating_add(1).max(1);
        self.hevc_decode_chain_watchdog
            .reset_transient_after_progress(None, None, now);
        self.last_hevc_decode_error = None;
        let transaction = self
            .hevc_same_hardware_recovery
            .as_mut()
            .expect("same-hardware transaction survives atomic worker swap");
        transaction.begin_attempt(
            self.decoder_epoch,
            HevcSameHardwareRecoveryAttemptKind::VulkanReopenReplay,
            generation,
            now,
        );
        transaction.phase = HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen;
        transaction.last_progress_at = now;
        transaction.last_result_produced_sequence = self.worker.snapshot().result_produced_sequence;
        Ok(device)
    }

    pub(in super::super) fn record_hevc_same_hardware_replay(
        &mut self,
        replay_packets: usize,
        after_reopen: bool,
        now: Instant,
    ) {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return;
        };
        transaction.record_replay(replay_packets, after_reopen, now);
    }

    pub(in super::super) fn begin_hevc_same_hardware_cached_rebuild(
        &mut self,
        generation: u64,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("cached safe-IDR rebuild requested without a transaction".to_string());
        };
        transaction.begin_cached_rebuild(self.decoder_epoch, generation, now)
    }

    pub(in super::super) fn fail_hevc_same_hardware_cached_rebuild(
        &mut self,
        error: impl Into<String>,
    ) {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return;
        };
        if transaction.phase == HevcSameHardwareRecoveryPhase::RebuildingFromCache {
            transaction.cached_rebuild_attempts =
                transaction.cached_rebuild_attempts.saturating_add(1);
        }
        transaction.fail(error);
    }

    pub(in super::super) fn mark_hevc_same_hardware_prewarm_ready(&mut self, now: Instant) -> bool {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen {
            return false;
        }
        transaction.prewarm_ticket = None;
        transaction.last_progress_at = now;
        true
    }

    pub(in super::super) fn record_hevc_same_hardware_prewarm_request(
        &mut self,
        ticket: VulkanPrewarmTicket,
    ) -> std::result::Result<(), String> {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return Err("Vulkan prewarm requested without a same-hardware transaction".to_string());
        };
        if transaction.phase != HevcSameHardwareRecoveryPhase::PrewarmingAfterReopen {
            return Err(format!(
                "Vulkan prewarm requested in same-hardware phase {}",
                transaction.phase.as_str()
            ));
        }
        transaction.prewarm_ticket = Some(ticket);
        Ok(())
    }

    pub(in super::super) fn hevc_same_hardware_prewarm_ticket(
        &self,
    ) -> Option<VulkanPrewarmTicket> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.prewarm_ticket)
    }

    pub(in super::super) fn fail_hevc_same_hardware_recovery(&mut self, error: impl Into<String>) {
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.fail(error);
        }
    }

    pub(in super::super) fn hevc_same_hardware_recovery_target(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .map(|transaction| transaction.target_nsecs)
    }

    pub(in super::super) fn hevc_same_hardware_recovery_attempt_id(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(HevcSameHardwareRecoveryTransaction::active_attempt_id)
    }

    pub(in super::super) fn hevc_same_hardware_recovery_decoder_epoch(&self) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .and_then(HevcSameHardwareRecoveryTransaction::active_decoder_epoch)
    }

    pub(in super::super) fn mark_hevc_same_hardware_unbridged_continuous_gap(&mut self) {
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.mark_unbridged_continuous_gap();
        }
    }

    pub(in super::super) fn hevc_same_hardware_action_log_summary(
        &mut self,
        action: HevcDecodeRecoveryAction,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_mut()
            .and_then(|transaction| transaction.should_log_action(action, now))
    }

    pub(in super::super) fn hevc_same_hardware_drain_log_summary(
        &mut self,
        advanced: bool,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_same_hardware_recovery
            .as_mut()
            .and_then(|transaction| transaction.should_log_drain(advanced, now))
    }

    pub(in super::super) fn hevc_same_hardware_recovery_terminal_error(
        &self,
        now: Instant,
    ) -> Option<String> {
        self.hevc_same_hardware_recovery
            .as_ref()
            .filter(|transaction| transaction.phase == HevcSameHardwareRecoveryPhase::Failed)
            .map(|transaction| transaction.terminal_error(now, self.requested_hardware_mode))
    }

    pub(in super::super) fn finish_hevc_same_hardware_recovery_terminal(&mut self) {
        self.hevc_same_hardware_recovery = None;
    }

    pub(in super::super) fn requested_hardware_mode(&self) -> HardwareDecodeMode {
        self.requested_hardware_mode
    }

    pub(super) fn observe_hevc_same_hardware_worker_progress(&mut self, now: Instant) -> bool {
        let result_produced_sequence = self.worker.snapshot().result_produced_sequence;
        self.hevc_same_hardware_recovery
            .as_mut()
            .is_some_and(|transaction| {
                transaction.observe_result_progress(result_produced_sequence, now)
            })
    }

    pub(in super::super) fn observe_hevc_same_hardware_staged_output_progress(
        &mut self,
        session_id: PlaybackSessionId,
        generation: u64,
        staged_end_nsecs: u64,
    ) -> bool {
        let now = Instant::now();
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        let Some(attempt) = transaction.active_attempt.as_mut() else {
            return false;
        };
        if !attempt.observe_staged_video_progress(generation, staged_end_nsecs, now) {
            return false;
        }
        transaction.last_progress_at = now;
        tracing::debug!(
            session_id = ?session_id,
            target_nsecs = transaction.target_nsecs,
            attempt_id = attempt.attempt_id,
            decoder_epoch = attempt.decoder_epoch,
            attempt_kind = attempt.kind.as_str(),
            staged_end_nsecs,
            "recorded accepted staged output progress for bounded HEVC recovery"
        );
        true
    }

    pub(in super::super) fn mark_hevc_same_hardware_output_committed(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> bool {
        let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() else {
            return false;
        };
        if !matches!(
            transaction.phase,
            HevcSameHardwareRecoveryPhase::ReplayingAfterFlush
                | HevcSameHardwareRecoveryPhase::ReplayingAfterReopen
        ) {
            return false;
        }
        let Some(attempt) = transaction.active_attempt.as_mut() else {
            return false;
        };
        if attempt.output_commit_observed {
            return false;
        }
        attempt.output_commit_observed = true;
        tracing::debug!(
            session_id = ?session_id,
            target_nsecs = transaction.target_nsecs,
            attempt_id = attempt.attempt_id,
            decoder_epoch = attempt.decoder_epoch,
            attempt_kind = attempt.kind.as_str(),
            admitted_span_after_catch_up_ms =
                attempt.admitted_span_after_catch_up_nsecs as f64 / 1_000_000.0,
            "recorded atomic output commit for bounded HEVC recovery"
        );
        true
    }

    pub(in super::super) fn observe_hevc_admitted_video_progress(
        &mut self,
        observation: HevcAdmittedVideoProgressObservation,
    ) {
        let now = Instant::now();
        let root_progress = if self.hevc_same_hardware_recovery.is_none() {
            self.hevc_decode_chain_watchdog
                .observe_admitted_video_progress(observation)
        } else {
            HevcAdmittedVideoProgress::None
        };
        let admitted_decoder_epoch = self
            .hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.active_attempt.as_ref())
            .filter(|attempt| attempt.observes_generation(observation.generation))
            .map(|attempt| attempt.decoder_epoch)
            .or_else(|| {
                self.hevc_same_hardware_recovery
                    .as_ref()
                    .is_none_or(|transaction| {
                        transaction.phase == HevcSameHardwareRecoveryPhase::DrainingResults
                    })
                    .then_some(self.decoder_epoch)
            });
        let transaction_progress = self
            .hevc_same_hardware_recovery
            .as_mut()
            .map(|transaction| transaction.observe_admitted_video_progress(observation, now));
        let admitted_progress = transaction_progress.unwrap_or(root_progress);
        if matches!(
            admitted_progress,
            HevcAdmittedVideoProgress::Partial | HevcAdmittedVideoProgress::Stable
        ) {
            self.admitted_video_sequence = self.admitted_video_sequence.saturating_add(1);
            self.last_admitted_decoder_epoch = admitted_decoder_epoch;
            self.last_hevc_decode_error = None;
        }
        if admitted_progress != HevcAdmittedVideoProgress::Stable {
            return;
        }

        self.hevc_decode_chain_watchdog.clear_recent_gap_evidence();
        self.hevc_decode_chain_watchdog.pending_fallback = None;
        self.last_hevc_decode_chain_fallback = None;
        if let Some(mut transaction) = self.hevc_same_hardware_recovery.take() {
            transaction.finish_active_attempt("recovered", now);
            transaction.phase = HevcSameHardwareRecoveryPhase::Recovered;
            transaction.last_progress_at = now;
            tracing::info!(
                session_id = ?observation.session_id,
                target_nsecs = transaction.target_nsecs,
                reason = transaction.reason.as_str(),
                same_hw_recovery_phase = transaction.phase.as_str(),
                decoder_epoch = self.decoder_epoch,
                admitted_video_sequence = self.admitted_video_sequence,
                flush_attempts = transaction.flush_attempts,
                reopen_attempts = transaction.reopen_attempts,
                replay_packets = transaction.replay_packets,
                attempt_ledger = ?transaction.attempt_ledger,
                elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                "completed bounded HEVC same-Vulkan recovery transaction"
            );
        }
    }

    pub(in super::super) fn observe_hevc_post_fallback_rebuffer_underfill(
        &mut self,
        observation: HevcPostFallbackRebufferObservation,
    ) {
        if observation.decode_recovery_active || self.hevc_same_hardware_recovery.is_some() {
            self.hevc_decode_chain_watchdog
                .suspend_playback_watchdogs_for_decode_recovery();
            return;
        }
        let hardware_accelerated = self.info().hardware_accelerated;
        if self
            .hevc_decode_chain_watchdog
            .recovery_progress_grace_active(observation.now, hardware_accelerated)
        {
            self.hevc_decode_chain_watchdog
                .post_fallback_rebuffer_underfill_started_at = None;
            return;
        }
        self.hevc_decode_chain_watchdog
            .observe_post_fallback_rebuffer_underfill(observation);
    }
}
