use super::*;

impl PlaybackPipelineState {
    pub(in super::super::super) fn soft_recover_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<(), String> {
        let transaction_id = self
            .video_decode_recovery
            .recovery_scope()
            .transaction_id()
            .unwrap_or_else(|| self.begin_recovery_transaction());
        self.active_recovery_transaction_id = transaction_id;
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.advance_playback_generation();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.video_decode_pipeline.clear_packets();
        self.dovi_pipeline.reset();
        tracing::debug!(
            session_id = ?session_id,
            transaction_id,
            recovery_scope = self.video_decode_recovery.recovery_scope().as_str(),
            generation,
            "soft recovered HEVC decode chain while waiting for first decoded video frame"
        );
        Ok(())
    }

    pub(in super::super::super) fn soft_recover_cached_seek_hevc_decode_chain(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        let transaction_id = self
            .video_decode_recovery
            .recovery_scope()
            .transaction_id()
            .unwrap_or_else(|| self.begin_recovery_transaction());
        self.active_recovery_transaction_id = transaction_id;
        if self.video_decode_skip_nonref_active {
            self.video_decode_pipeline.set_skip_nonref_frames(false)?;
            self.video_decode_skip_nonref_active = false;
        }
        let generation = self.advance_playback_generation();
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.video_decode_recovery.begin_with_realign(true);
        self.dovi_pipeline.reset();
        // Keep the safe hardware packet journal intact: it belongs to the subsequent
        // software fallback transaction, not to this lightweight hardware flush.
        let requeued_probe_packets = 0;
        tracing::debug!(
            session_id = ?session_id,
            transaction_id,
            recovery_scope = self.video_decode_recovery.recovery_scope().as_str(),
            generation,
            requeued_probe_packets,
            "soft recovered HEVC cached seek decode chain without low-level seek"
        );
        Ok(requeued_probe_packets)
    }

    pub(in super::super::super) fn begin_cached_seek_recovery_watchdog(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(target_nsecs, None, session_id);
    }

    pub(in super::super::super) fn begin_cached_seek_recovery_watchdog_for_hit(
        &mut self,
        cached_seek: DemuxCachedSeekInfo,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(
            cached_seek.target_nsecs,
            Some(cached_seek),
            session_id,
        );
    }

    pub(in super::super::super) fn rearm_cached_seek_recovery_watchdog(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
        session_id: PlaybackSessionId,
    ) {
        self.begin_cached_seek_recovery_watchdog_with_context(
            target_nsecs,
            cached_seek,
            session_id,
        );
    }

    fn begin_cached_seek_recovery_watchdog_with_context(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
        session_id: PlaybackSessionId,
    ) {
        if self.video_stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            self.cached_seek_recovery_watchdog = None;
            return;
        }
        let previous_target_nsecs = self
            .cached_seek_recovery_watchdog
            .map(|watchdog| watchdog.target_nsecs);
        let decoder_epoch = self.video_decode_pipeline.decoder_epoch();
        let recovery_transaction_id = self.active_recovery_transaction_id();
        let admitted_video_sequence = self.video_decode_pipeline.admitted_video_sequence();
        let (watchdog, started) = cached_seek_recovery_watchdog_after_begin(
            self.cached_seek_recovery_watchdog,
            target_nsecs,
            cached_seek,
            decoder_epoch,
            recovery_transaction_id,
            admitted_video_sequence,
            Instant::now(),
            self.video_packet_count,
        );
        self.cached_seek_recovery_watchdog = Some(watchdog);
        if started {
            tracing::debug!(
                ?session_id,
                target_nsecs,
                decoder_epoch,
                recovery_transaction_id,
                start_admitted_video_sequence = admitted_video_sequence,
                range_id = ?cached_seek.map(|info| info.range_id),
                anchor_packet_id = ?cached_seek.map(|info| info.anchor_packet_id),
                anchor_kind = ?cached_seek.map(|info| info.anchor_kind.as_str()),
                anchor_nsecs = ?cached_seek.map(|info| info.anchor_nsecs),
                preroll_nsecs = ?cached_seek.map(|info| info.preroll_nsecs),
                video_packet_count = self.video_packet_count,
                "started HEVC cached seek recovery watchdog"
            );
        } else {
            tracing::debug!(
                ?session_id,
                previous_target_nsecs,
                target_nsecs,
                decoder_epoch = watchdog.decoder_epoch,
                recovery_transaction_id = watchdog.recovery_transaction_id,
                start_admitted_video_sequence = watchdog.start_admitted_video_sequence,
                range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                total_elapsed_ms = watchdog.started_at.elapsed().as_secs_f64() * 1000.0,
                video_packets_since_start = self
                    .video_packet_count
                    .saturating_sub(watchdog.start_video_packet_count),
                "rearmed HEVC cached seek recovery watchdog after internal recovery"
            );
        }
    }

    pub(in super::super::super) fn clear_cached_seek_recovery_watchdog(&mut self) {
        self.cached_seek_recovery_watchdog = None;
        self.cached_seek_recovery_attempt = None;
    }

    pub(in super::super::super) fn active_cra_cached_seek(&self) -> Option<DemuxCachedSeekInfo> {
        self.cached_seek_recovery_watchdog
            .and_then(|watchdog| watchdog.cached_seek)
            .filter(|info| info.uses_cra_anchor())
    }

    pub(in super::super::super) fn cached_seek_recovery_watchdog_deadline(
        &self,
    ) -> Option<Instant> {
        self.cached_seek_recovery_watchdog.map(|watchdog| {
            watchdog.last_progress_at + self.cached_seek_recovery_timeout(watchdog.target_nsecs)
        })
    }

    fn cached_seek_recovery_timeout(&self, target_nsecs: u64) -> Duration {
        let info = self.video_decode_pipeline.info();
        let stats = self.video_decode_pipeline.hevc_decode_chain_stats();
        cached_seek_recovery_timeout(
            info.hardware_accelerated,
            info.size,
            target_nsecs,
            stats.first_zero_output_packet_nsecs,
        )
    }

    pub(in super::super::super) fn playback_loop_deadline(&self) -> PlaybackLoopDeadline {
        PlaybackLoopDeadline::from_cached_seek_recovery_watchdog(
            self.cached_seek_recovery_watchdog_deadline(),
        )
        .with_hevc_startup_stall_watchdog_deadline(
            self.video_decode_pipeline
                .hevc_startup_stall_watchdog_deadline(),
        )
        .with_audio_decode_recovery_watchdog_deadline(
            self.audio_decode_recovery_watchdog_deadline(),
        )
        .with_output_housekeeping_deadline(self.output_scheduler.output_housekeeping_deadline())
    }

    fn audio_decode_recovery_watchdog_deadline(&self) -> Option<Instant> {
        let transaction = self.audio_realign_transaction?;
        if matches!(
            transaction.phase,
            AudioRealignPhase::Covered | AudioRealignPhase::MediaGap
        ) || transaction.fallback_exhausted_logged
        {
            return None;
        }
        let threshold = if transaction.warning_emitted
            || transaction.phase == AudioRealignPhase::FallbackUsed
        {
            AUDIO_DECODE_RECOVERY_STALL_FALLBACK_AFTER
        } else {
            AUDIO_DECODE_RECOVERY_STALL_WARN_AFTER
        };
        Some(
            (transaction.last_progress_at + threshold)
                .min(transaction.started_at + AUDIO_REALIGN_MAX_WALL_TIME),
        )
    }

    pub(in super::super::super) fn cached_seek_recovery_watchdog_expired(&self) -> bool {
        self.cached_seek_recovery_watchdog_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub(in super::super::super) fn cached_seek_recovery_watchdog_snapshot(
        &self,
    ) -> Option<CachedSeekRecoveryWatchdogSnapshot> {
        let watchdog = self.cached_seek_recovery_watchdog?;
        let timeout = self.cached_seek_recovery_timeout(watchdog.target_nsecs);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(watchdog.started_at);
        let stalled = now.saturating_duration_since(watchdog.last_progress_at);
        Some(CachedSeekRecoveryWatchdogSnapshot {
            target_nsecs: watchdog.target_nsecs,
            elapsed,
            remaining: timeout.saturating_sub(stalled),
            video_packets_since_seek: self
                .video_packet_count
                .saturating_sub(watchdog.start_video_packet_count),
        })
    }

    pub(in super::super::super) fn take_cached_seek_recovery_fallback(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<CachedSeekRecoveryFallback> {
        let mut watchdog = self.cached_seek_recovery_watchdog?;
        let now = Instant::now();
        let output_snapshot = self.output_scheduler.snapshot();
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let elapsed = now.saturating_duration_since(watchdog.started_at);
        let video_packets_since_seek = self
            .video_packet_count
            .saturating_sub(watchdog.start_video_packet_count);
        let progress = CachedSeekRecoveryProgress::from_decode_snapshot(
            video_packets_since_seek,
            video_decode_snapshot,
            self.video_decode_recovery.seek_bootstrap_preroll_frames(),
        );
        let admitted_video_sequence = self.video_decode_pipeline.admitted_video_sequence();
        let matching_admitted_progress = self.video_decode_pipeline.decoder_epoch()
            == watchdog.decoder_epoch
            && self.video_decode_pipeline.last_admitted_decoder_epoch()
                == Some(watchdog.decoder_epoch)
            && admitted_video_sequence > watchdog.start_admitted_video_sequence
            && self.active_recovery_transaction_id() == watchdog.recovery_transaction_id;
        if progress.seek_preroll_frames > watchdog.last_seek_preroll_frames {
            watchdog.last_seek_preroll_frames = progress.seek_preroll_frames;
            watchdog.last_progress_at = now;
        }
        self.cached_seek_recovery_watchdog = Some(watchdog);
        let timeout = self.cached_seek_recovery_timeout(watchdog.target_nsecs);
        let stalled = now.saturating_duration_since(watchdog.last_progress_at);
        tracing::trace!(
            ?session_id,
            target_nsecs = watchdog.target_nsecs,
            range_id = ?watchdog.cached_seek.map(|info| info.range_id),
            anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
            anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
            anchor_nsecs = ?watchdog.cached_seek.map(|info| info.anchor_nsecs),
            preroll_nsecs = ?watchdog.cached_seek.map(|info| info.preroll_nsecs),
            elapsed_ms = elapsed.as_secs_f64() * 1000.0,
            stalled_ms = stalled.as_secs_f64() * 1000.0,
            remaining_ms = timeout.saturating_sub(stalled).as_secs_f64() * 1000.0,
            timeout_ms = timeout.as_secs_f64() * 1000.0,
            video_packets_since_seek,
            video_decode_pending_input_packets = progress.video_decode_pending_input_packets,
            video_decode_submitted_not_consumed_packets = progress.video_decode_submitted_not_consumed_packets,
            video_decode_completed_packets = progress.video_decode_completed_packets,
            video_decode_queued_frames = progress.video_decode_queued_frames,
            seek_preroll_frames = progress.seek_preroll_frames,
            decoder_work_pending = progress.decoder_work_pending(),
            queued_video_frames = output_snapshot.queued_video_frames,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            decoder_epoch = watchdog.decoder_epoch,
            recovery_transaction_id = watchdog.recovery_transaction_id,
            start_admitted_video_sequence = watchdog.start_admitted_video_sequence,
            admitted_video_sequence,
            matching_admitted_progress,
            "checked HEVC cached seek recovery watchdog"
        );
        match cached_seek_recovery_watchdog_decision(
            stalled,
            timeout,
            progress,
            matching_admitted_progress,
        ) {
            CachedSeekRecoveryWatchdogDecision::Wait => None,
            CachedSeekRecoveryWatchdogDecision::WaitingCachedInput => {
                watchdog.last_progress_at = now;
                self.cached_seek_recovery_watchdog = Some(watchdog);
                tracing::trace!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    video_packets_since_seek,
                    video_decode_state = ?progress.video_decode_state,
                    gate_reason = "waiting_cached_input",
                    "paused HEVC cached seek recovery watchdog while decoder waits for input"
                );
                None
            }
            CachedSeekRecoveryWatchdogDecision::Clear => {
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                    anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                    anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    preroll_decode_ms = elapsed.as_secs_f64() * 1000.0,
                    stalled_ms = stalled.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    decoder_epoch = watchdog.decoder_epoch,
                    recovery_transaction_id = watchdog.recovery_transaction_id,
                    admitted_video_sequence,
                    cached_seek_succeeded = true,
                    low_level_fallback = false,
                    "completed HEVC cached seek recovery at first target video frame"
                );
                self.cached_seek_recovery_watchdog = None;
                self.cached_seek_recovery_attempt = None;
                None
            }
            CachedSeekRecoveryWatchdogDecision::Fallback(reason) => {
                let action = self
                    .cached_seek_recovery_next_action(watchdog.target_nsecs, watchdog.cached_seek);
                tracing::debug!(
                    ?session_id,
                    target_nsecs = watchdog.target_nsecs,
                    range_id = ?watchdog.cached_seek.map(|info| info.range_id),
                    anchor_packet_id = ?watchdog.cached_seek.map(|info| info.anchor_packet_id),
                    anchor_kind = ?watchdog.cached_seek.map(|info| info.anchor_kind.as_str()),
                    reason = reason.as_str(),
                    action = action.as_str(),
                    elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                    stalled_ms = stalled.as_secs_f64() * 1000.0,
                    timeout_ms = timeout.as_secs_f64() * 1000.0,
                    video_packets_since_seek,
                    video_decode_pending_input_packets =
                        progress.video_decode_pending_input_packets,
                    video_decode_submitted_not_consumed_packets = progress.video_decode_submitted_not_consumed_packets,
                    video_decode_completed_packets = progress.video_decode_completed_packets,
                    video_decode_queued_frames = progress.video_decode_queued_frames,
                    seek_preroll_frames = progress.seek_preroll_frames,
                    decoder_work_pending = progress.decoder_work_pending(),
                    max_video_packets = CACHED_SEEK_STARTUP_MAX_VIDEO_PACKETS,
                    queued_video_frames = output_snapshot.queued_video_frames,
                    first_video_frame_pending = output_snapshot.first_video_frame_pending,
                    recovery_waiting = self.video_decode_recovery.waiting_for_keyframe(),
                    recovery_skipped_packets = self.video_decode_recovery.skipped_packets(),
                    "HEVC cached seek recovery watchdog requesting fallback"
                );
                self.cached_seek_recovery_watchdog = None;
                Some(CachedSeekRecoveryFallback {
                    target_nsecs: watchdog.target_nsecs,
                    cached_seek: watchdog.cached_seek,
                    reason,
                    action,
                })
            }
        }
    }
}
