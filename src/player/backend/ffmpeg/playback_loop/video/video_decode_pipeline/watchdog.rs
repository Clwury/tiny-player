use super::*;

impl VideoDecodePipeline {
    pub(in super::super) fn observe_hevc_startup_stall(
        &mut self,
        observation: HevcStartupStallObservation,
    ) -> HevcDecodeChainRecoveryAction {
        if self.hevc_same_hardware_recovery.is_some() {
            self.hevc_decode_chain_watchdog
                .suspend_playback_watchdogs_for_decode_recovery();
            return HevcDecodeChainRecoveryAction::None;
        }
        self.hevc_decode_chain_watchdog
            .observe_startup_stall(observation)
    }

    pub(in super::super) fn hevc_startup_stall_watchdog_deadline(&self) -> Option<Instant> {
        if self.hevc_same_hardware_recovery.is_some() {
            return None;
        }
        self.hevc_decode_chain_watchdog
            .startup_watchdog_deadline(self.info().hardware_accelerated)
    }

    pub(in super::super) fn suspend_hevc_playback_watchdogs_for_decode_recovery(&mut self) {
        self.hevc_decode_chain_watchdog
            .suspend_playback_watchdogs_for_decode_recovery();
    }

    pub(in super::super) fn complete_hevc_startup_watchdog_after_first_frame(&mut self) {
        self.hevc_decode_chain_watchdog
            .complete_startup_watchdog_after_first_frame();
    }

    pub(in super::super) fn defer_hevc_startup_stall_watchdog_after_no_action(
        &mut self,
        now: Instant,
    ) {
        self.hevc_decode_chain_watchdog
            .defer_startup_watchdog_after_no_action(now);
    }

    pub(in super::super) fn suspend_hevc_startup_watchdog_for_input_wait(&mut self) -> bool {
        self.hevc_decode_chain_watchdog
            .suspend_startup_watchdog_for_input_wait()
    }

    pub(in super::super) fn observe_hevc_decode_pipeline_progress(&mut self, now: Instant) {
        self.hevc_decode_chain_watchdog
            .observe_replay_packet_progress(now);
        if let Some(transaction) = self.hevc_same_hardware_recovery.as_mut() {
            transaction.last_progress_at = now;
        }
    }

    pub(in super::super) fn record_hevc_startup_stall_watchdog_rejection(
        &mut self,
        reason: &'static str,
        now: Instant,
    ) -> Option<u64> {
        self.hevc_decode_chain_watchdog
            .record_startup_watchdog_rejection(reason, now)
    }

    pub(in super::super) fn hevc_recent_video_progress_grace_active(&self, now: Instant) -> bool {
        self.hevc_decode_chain_watchdog
            .recovery_progress_grace_active(now, self.info().hardware_accelerated)
    }

    pub(in super::super) fn hevc_decode_chain_stats(&self) -> HevcDecodeChainStats {
        self.hevc_decode_chain_watchdog.stats()
    }

    pub(in super::super) fn hevc_exact_seek_evidence_scope_active(&self) -> bool {
        self.hevc_decode_chain_watchdog
            .exact_seek_evidence_scope_active()
    }

    pub(in super::super) fn hevc_exact_seek_landing_nsecs(&self) -> Option<u64> {
        self.hevc_decode_chain_watchdog
            .completed_exact_seek_landing_nsecs
    }

    pub(in super::super) fn take_hevc_decode_chain_fallback(
        &mut self,
    ) -> Option<HevcDecodeChainFallback> {
        self.hevc_decode_chain_watchdog.take_fallback()
    }

    pub(in super::super) fn hevc_decode_chain_fallback_pending(&self) -> bool {
        self.hevc_decode_chain_watchdog.has_pending_fallback()
    }

    pub(in super::super) fn pending_hevc_decode_chain_fallback(
        &self,
    ) -> Option<HevcDecodeChainFallback> {
        self.hevc_decode_chain_watchdog.pending_fallback()
    }

    pub(in super::super) fn hevc_decode_chain_fallback_loop_action(
        &self,
        fallback: HevcDecodeChainFallback,
    ) -> HevcDecodeChainFallbackLoopAction {
        hevc_decode_chain_fallback_loop_action(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
        )
    }

    pub(in super::super) fn has_prior_matching_hevc_decode_chain_fallback(
        &self,
        fallback: HevcDecodeChainFallback,
    ) -> bool {
        self.last_hevc_decode_chain_fallback.is_some_and(|last| {
            hevc_fallback_targets_match(last.last_target_nsecs, fallback.target_nsecs)
                && last.last_reason == fallback.reason
        })
    }

    pub(in super::super) fn remember_hevc_decode_chain_fallback(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        self.last_hevc_decode_chain_fallback = Some(hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        ));
    }

    pub(in super::super) fn remember_hevc_decode_chain_software_suppression(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        let mut record = hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        );
        if record.low_level_seeks > 0 {
            record.post_low_level_suppressions =
                record.post_low_level_suppressions.saturating_add(1);
        } else {
            record.software_suppressions = record.software_suppressions.saturating_add(1);
        }
        self.last_hevc_decode_chain_fallback = Some(record);
    }

    pub(in super::super) fn remember_hevc_decode_chain_low_level_seek(
        &mut self,
        fallback: HevcDecodeChainFallback,
    ) {
        let mut record = hevc_decode_chain_fallback_record_after(
            self.last_hevc_decode_chain_fallback,
            fallback,
            self.info().hardware_accelerated,
            Instant::now(),
        );
        record.low_level_seeks = record.low_level_seeks.saturating_add(1);
        self.last_hevc_decode_chain_fallback = Some(record);
    }

    pub(in super::super) fn remember_hevc_recovery_low_level_seek_target(
        &mut self,
        target_nsecs: u64,
    ) {
        let Some(mut record) = self.last_hevc_decode_chain_fallback else {
            return;
        };
        record.last_target_nsecs = target_nsecs;
        record.hardware_accelerated = self.info().hardware_accelerated;
        record.recorded_at = Instant::now();
        record.low_level_seeks = record.low_level_seeks.saturating_add(1);
        self.last_hevc_decode_chain_fallback = Some(record);
    }
}
