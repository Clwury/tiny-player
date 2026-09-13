use super::*;

impl PlaybackPipelineState {
    #[allow(clippy::too_many_arguments)]
    pub(in super::super::super) fn service_audio_output_state_machine(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
        now: Instant,
    ) -> std::result::Result<(), String> {
        let control_before = control.audio_output_control_snapshot();
        let desired_lifecycle = match self.output_scheduler.playback_output_state {
            PlaybackOutputState::Syncing => AudioOutputLifecycle::Syncing,
            PlaybackOutputState::Primed | PlaybackOutputState::Rebuffering => {
                AudioOutputLifecycle::Ready
            }
            PlaybackOutputState::Playing
                if control_before.paused_by_seek_transition()
                    && control_before.lifecycle() == AudioOutputLifecycle::Syncing =>
            {
                // The scheduler can still describe the old Playing
                // generation while a cache-only seek is selecting its final
                // fallback. Do not infer readiness until the new generation
                // has reached Primed/Ready at least once.
                AudioOutputLifecycle::Syncing
            }
            PlaybackOutputState::Playing => AudioOutputLifecycle::Playing,
        };
        let previous_lifecycle = control.audio_output_lifecycle();
        let seek_transition_released =
            desired_lifecycle == AudioOutputLifecycle::Playing && control.finish_seek_audio_pause();
        let lifecycle_changed = control.set_audio_output_lifecycle(desired_lifecycle);
        if lifecycle_changed || seek_transition_released {
            tracing::debug!(
                session_id = ?session_id,
                previous_audio_output_lifecycle = previous_lifecycle.as_str(),
                audio_output_lifecycle = desired_lifecycle.as_str(),
                seek_transition_released,
                output_state = ?self.output_scheduler.playback_output_state,
                "coordinator reevaluated native audio output lifecycle"
            );
        }

        let Some(output) = self.audio_output.as_ref() else {
            self.output_scheduler.reset_audio_output_activity_watchdog();
            return Ok(());
        };
        let activity = output.activity_snapshot()?;
        let output_state = control.audio_output_control_snapshot();
        let supervision_enabled = self.output_scheduler.playback_output_state
            == PlaybackOutputState::Playing
            && !self.output_scheduler.restart_pending()
            && !output_state.externally_paused()
            && !control.has_pending_seek();
        let underrun_active = output.underrun_active();
        if supervision_enabled
            && underrun_active
            && self
                .output_scheduler
                .audio_output_clock_stall_fallback_active()
        {
            // A bounded re-anchor can briefly expose an empty AO. Keep the
            // video-clock escape hatch armed until the normal underrun refill
            // path reports real callback progress.
            return Ok(());
        }
        let eligible = supervision_enabled && !underrun_active;
        let Some(event) = self.output_scheduler.observe_audio_output_activity(
            now,
            activity,
            eligible,
            output_state.paused_by_seek_transition(),
        ) else {
            return Ok(());
        };

        match event.action {
            AudioOutputActivityWatchdogAction::ReleaseSeekTransition => {
                let released = control.finish_seek_audio_pause();
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    released_seek_transition = released,
                    played_timeline_nsecs = activity.played_timeline_nsecs,
                    shared_buffer_pending_ms =
                        activity.shared_buffer_pending_nsecs as f64 / 1_000_000.0,
                    queue_pending_ms = activity.queue_pending_nsecs as f64 / 1_000_000.0,
                    pending_audio_frames = self.output_scheduler.pending_start_audio.len(),
                    audio_stream_active = output.stream_active(),
                    callback_count = activity.callback_count,
                    consumed_callback_count = activity.consumed_callback_count,
                    silenced_callback_count = activity.silenced_callback_count,
                    underrun_count = activity.underrun_count,
                    audio_output_lifecycle = control.audio_output_lifecycle().as_str(),
                    "native audio output should be consuming but seek transition remained silent"
                );
            }
            AudioOutputActivityWatchdogAction::WarnFrozenClock => {
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    played_timeline_nsecs = activity.played_timeline_nsecs,
                    shared_buffer_pending_ms =
                        activity.shared_buffer_pending_nsecs as f64 / 1_000_000.0,
                    queue_pending_ms = activity.queue_pending_nsecs as f64 / 1_000_000.0,
                    pending_audio_frames = self.output_scheduler.pending_start_audio.len(),
                    audio_stream_active = output.stream_active(),
                    callback_count = activity.callback_count,
                    consumed_callback_count = activity.consumed_callback_count,
                    silenced_callback_count = activity.silenced_callback_count,
                    underrun_count = activity.underrun_count,
                    audio_output_lifecycle = control.audio_output_lifecycle().as_str(),
                    video_clock_fallback = true,
                    "native audio output clock stopped advancing while playback is active"
                );
            }
            AudioOutputActivityWatchdogAction::RecoverAndReanchor => {
                let reanchor_timeline_nsecs = self
                    .scheduler
                    .current_timeline_nsecs()
                    .max(activity.played_timeline_nsecs);
                control.set_audio_output_lifecycle(AudioOutputLifecycle::Ready);
                output.reset_clock(reanchor_timeline_nsecs);
                let recovered = recover_pending_start_audio_after_underrun(
                    &mut self.output_scheduler.pending_start_audio,
                    output,
                    control,
                    &mut self.output_scheduler.scheduled_video_queue,
                    session_id,
                    vo_queue,
                    frame_presented,
                    &mut self.position_reporter,
                    event_tx,
                    &mut self.subtitle_pipeline,
                    &mut self.buffered_reporter,
                )?;
                if recovered {
                    control.set_audio_output_lifecycle(AudioOutputLifecycle::Playing);
                }
                tracing::warn!(
                    session_id = ?session_id,
                    stalled_ms = event.stalled_for.as_secs_f64() * 1000.0,
                    previous_played_timeline_nsecs = activity.played_timeline_nsecs,
                    reanchor_timeline_nsecs,
                    recovered_pending_audio = recovered,
                    pending_audio_frames = self.output_scheduler.pending_start_audio.len(),
                    pending_audio_ms = self
                        .output_scheduler
                        .pending_start_audio
                        .buffered_duration()
                        .as_secs_f64()
                        * 1000.0,
                    video_clock_fallback = true,
                    "bounded native audio output stall recovery reset and re-anchored the clock"
                );
            }
        }
        Ok(())
    }

    pub(in super::super::super) fn cache_pause_work_snapshot(&self) -> CachePauseWorkSnapshot {
        let output = self.output_scheduler.snapshot();
        let decode = self.video_decode_pipeline.snapshot();
        let prepare = self.video_frame_prepare_worker.snapshot();
        let decoder_input = self.decoder_input_snapshot(false);
        let video_packet_input_available = decoder_input
            .demux_streams
            .contains(&decoder_input.video_stream_index);
        let mut selected_streams = vec![decoder_input.video_stream_index];
        selected_streams.extend(decoder_input.audio_stream_index);
        selected_streams.extend(decoder_input.subtitle_stream_index);
        selected_streams.sort_unstable();
        selected_streams.dedup();
        CachePauseWorkSnapshot {
            actual_decode_work: cache_pause_actual_decode_work_for(decode, prepare),
            first_frame_input_demand: cache_pause_first_frame_input_demand_for(
                output,
                video_packet_input_available,
            ),
            selected_streams,
            requested_streams: decoder_input.demux_streams,
            video_stream_index: decoder_input.video_stream_index,
        }
    }

    pub(in super::super::super) fn exact_seek_actual_anchor_nsecs(&self) -> Option<u64> {
        match self.video_decode_recovery.recovery_scope() {
            VideoDecodeRecoveryScope::ExactLowLevelSeek {
                actual_anchor_nsecs,
                ..
            } => Some(actual_anchor_nsecs),
            VideoDecodeRecoveryScope::ExactCachedSeek { .. } => self
                .cached_seek_recovery_watchdog
                .and_then(|watchdog| watchdog.cached_seek)
                .map(|cached_seek| cached_seek.anchor_nsecs),
            VideoDecodeRecoveryScope::SafeBoundary => None,
        }
    }

    pub(in super::super::super) fn begin_recovery_transaction(&mut self) -> u64 {
        let transaction_id = self.next_recovery_transaction_id.max(1);
        self.next_recovery_transaction_id = transaction_id.saturating_add(1).max(1);
        self.active_recovery_transaction_id = transaction_id;
        transaction_id
    }

    pub(in super::super::super) fn active_recovery_transaction_id(&self) -> u64 {
        self.active_recovery_transaction_id
    }

    pub(in super::super::super) fn continue_recovery_transaction(&mut self, transaction_id: u64) {
        let transaction_id = transaction_id.max(1);
        self.active_recovery_transaction_id = transaction_id;
        self.next_recovery_transaction_id = self
            .next_recovery_transaction_id
            .max(transaction_id.saturating_add(1).max(1));
    }

    pub(in super::super::super) fn advance_playback_generation(&mut self) -> u64 {
        self.output_scheduler.advance_discontinuity_epoch();
        self.playback_generation.advance()
    }

    pub(in super::super::super) fn flush_playback_generation(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.restore_video_decode_skip_nonref_default(None, "playback_generation_flush")?;
        self.video_decode_pipeline
            .reset_hevc_decode_chain_recovery_transaction();
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(in super::super::super) fn flush_playback_generation_preserving_hevc_same_hardware_recovery(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        self.restore_video_decode_skip_nonref_default(None, "same_vulkan_cached_safe_idr_rebuild")?;
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(in super::super::super) fn observe_rebuffer_audio_realign_request(
        &mut self,
        request: RebufferAudioRealignRequest,
    ) -> AudioRealignRequestAction {
        self.refresh_audio_realign_progress();
        observe_audio_realign_request(&mut self.audio_realign_transaction, request)
    }

    pub(in super::super::super) fn begin_audio_realign_transaction(
        &mut self,
        transaction_id: u64,
        request: RebufferAudioRealignRequest,
        generation: u64,
        started_at: Instant,
    ) {
        self.continue_recovery_transaction(transaction_id);
        self.audio_realign_transaction = Some(AudioRealignTransaction {
            transaction_id,
            target_timeline_nsecs: request.target_timeline_nsecs,
            generation,
            started_at,
            attempts: 1,
            request,
            phase: AudioRealignPhase::Flushing,
            coverage_nsecs: 0,
            coverage_target_nsecs: duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)),
            observations: 0,
            first_observed_pts_nsecs: None,
            last_observed_pts_nsecs: None,
            last_progress_at: started_at,
            warning_emitted: false,
            fallback_exhausted_logged: false,
        });
    }

    pub(in super::super::super) fn retain_audio_for_realign(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        if retain_pending_audio_for_realign_once(
            &mut self.audio_realign_retained_pending,
            &mut self.output_scheduler.pending_start_audio,
        ) {
            let retained = self
                .audio_realign_retained_pending
                .as_ref()
                .expect("retained pending audio was just installed");
            tracing::debug!(
                session_id = ?session_id,
                reason,
                retained_pending_audio_frames = retained.len(),
                retained_pending_audio_ms = retained.buffered_duration().as_secs_f64() * 1000.0,
                retained_pending_audio_range_nsecs = ?retained.range_nsecs(),
                retained_pending_audio_contiguous_range_nsecs = ?retained.contiguous_range_nsecs(),
                retained_pending_audio_first_gap_nsecs = ?retained.first_gap_nsecs(),
                "retained pending FFmpeg audio until realign reaches a terminal result"
            );
        }
        if let Some(audio_decode_pipeline) = self.audio_decode_pipeline.as_mut() {
            let retained_frames = audio_decode_pipeline.take_output_frames_for_realign();
            if !retained_frames.is_empty() {
                tracing::debug!(
                    session_id = ?session_id,
                    reason,
                    retained_decoded_frames = retained_frames.len(),
                    first_retained_generation = ?retained_frames
                        .first()
                        .map(|(generation, _)| *generation),
                    last_retained_generation = ?retained_frames
                        .last()
                        .map(|(generation, _)| *generation),
                    first_retained_raw_timestamp = ?retained_frames
                        .first()
                        .map(|(_, frame)| frame.raw_timestamp),
                    last_retained_raw_timestamp = ?retained_frames
                        .last()
                        .map(|(_, frame)| frame.raw_timestamp),
                    retained_decoded_ms = retained_frames
                        .iter()
                        .map(|(_, frame)| frame.audio.duration_nsecs)
                        .fold(0_u64, u64::saturating_add) as f64
                        / 1_000_000.0,
                    "retained decoded FFmpeg audio frames across realign"
                );
            }
            self.audio_realign_retained_decoded_frames
                .extend(retained_frames);
        }
    }

    fn discard_audio_retained_for_completed_realign(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let retained_pending = self.audio_realign_retained_pending.take();
        let retained_decoded = std::mem::take(&mut self.audio_realign_retained_decoded_frames);
        if retained_pending.is_none() && retained_decoded.is_empty() {
            return;
        }
        tracing::debug!(
            session_id = ?session_id,
            reason,
            discarded_retained_pending_audio_frames = retained_pending
                .as_ref()
                .map(PendingStartAudio::len)
                .unwrap_or_default(),
            discarded_retained_pending_audio_ms = retained_pending
                .as_ref()
                .map(PendingStartAudio::buffered_duration)
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0,
            discarded_retained_pending_audio_range_nsecs = ?retained_pending
                .as_ref()
                .and_then(PendingStartAudio::range_nsecs),
            discarded_retained_decoded_frames = retained_decoded.len(),
            discarded_retained_decoded_first_raw_timestamp = ?retained_decoded
                .first()
                .map(|(_, frame)| frame.raw_timestamp),
            discarded_retained_decoded_last_raw_timestamp = ?retained_decoded
                .last()
                .map(|(_, frame)| frame.raw_timestamp),
            "released pre-realign FFmpeg audio after realign reached a terminal result"
        );
    }

    pub(in super::super::super) fn finish_audio_realign_as_confirmed_media_gap(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> Option<(AudioRealignTransaction, u64)> {
        let mut transaction = self.audio_realign_transaction.take()?;
        transaction.phase = AudioRealignPhase::MediaGap;
        let resume_timeline_nsecs = self
            .output_scheduler
            .resume_after_confirmed_audio_media_gap(
                transaction.target_timeline_nsecs,
                transaction.request.far_ahead_audio_timeline_nsecs,
                control,
                session_id,
            );
        self.scheduler.reset(resume_timeline_nsecs);
        if let Some(audio_output) = self.audio_output.as_ref() {
            audio_output.reset_clock(resume_timeline_nsecs);
        }
        self.current_start_position_nsecs =
            self.current_start_position_nsecs.max(resume_timeline_nsecs);
        self.discard_audio_retained_for_completed_realign(
            session_id,
            "audio_realign_confirmed_media_gap",
        );
        Some((transaction, resume_timeline_nsecs))
    }

    pub(in super::super::super) fn update_audio_realign_recovery_generation(
        &mut self,
        generation: u64,
    ) {
        if let Some(transaction) = self.audio_realign_transaction.as_mut() {
            transaction.generation = generation;
            transaction.phase = AudioRealignPhase::FallbackUsed;
            transaction.coverage_nsecs = 0;
            transaction.last_progress_at = Instant::now();
        }
    }

    pub(in super::super::super) fn poll_audio_recovery_watchdog(
        &mut self,
    ) -> Option<AudioRecoveryWatchdogAction> {
        self.refresh_audio_realign_progress();
        let worker = self.audio_decode_pipeline.as_ref()?.snapshot();
        let transaction = self.audio_realign_transaction.as_mut()?;
        poll_audio_recovery_watchdog(transaction, worker, Instant::now())
    }

    fn refresh_audio_realign_progress(&mut self) {
        let Some(transaction) = self.audio_realign_transaction else {
            return;
        };
        let Some(worker) = self
            .audio_decode_pipeline
            .as_ref()
            .map(AudioDecodePipeline::snapshot)
        else {
            return;
        };
        let coverage = self.output_scheduler.audio_realign_coverage(
            transaction.target_timeline_nsecs,
            duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
        );
        if let Some(transaction) = self.audio_realign_transaction.as_mut() {
            let previous_phase = transaction.phase;
            let previous_coverage_nsecs = transaction.coverage_nsecs;
            update_audio_realign_progress(transaction, worker, coverage, Instant::now());
            if transaction.phase != previous_phase
                || transaction.coverage_nsecs != previous_coverage_nsecs
            {
                tracing::debug!(
                    transaction_id = transaction.transaction_id,
                    recovery_scope = "audio_realign",
                    target_timeline_nsecs = transaction.target_timeline_nsecs,
                    transaction_generation = transaction.generation,
                    previous_phase = previous_phase.as_str(),
                    phase = transaction.phase.as_str(),
                    audio_accepted_start = ?coverage.audio_accepted_start_timeline_nsecs,
                    start_gap_ms = ?coverage
                        .start_gap_nsecs
                        .map(|gap| gap as f64 / 1_000_000.0),
                    contiguous_coverage_ms = ?coverage
                        .contiguous_coverage_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    coverage_target_ms = coverage.protected_target_nsecs as f64 / 1_000_000.0,
                    recovery_satisfied = transaction.phase == AudioRealignPhase::Covered,
                    fallback_eligible = false,
                    "updated FFmpeg audio realign transaction coverage"
                );
            }
        }
    }

    pub(in super::super::super) fn clear_audio_realign_transaction(&mut self) {
        self.audio_realign_transaction = None;
        self.audio_realign_retained_pending = None;
        self.audio_realign_retained_decoded_frames.clear();
    }

    pub(in super::super::super) fn clear_audio_realign_transaction_after_resume(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<AudioRealignTransaction> {
        self.refresh_audio_realign_progress();
        let output_resumed = self.output_scheduler.snapshot().state == PlaybackOutputState::Playing;
        let recovery_complete = self
            .audio_decode_pipeline
            .as_ref()
            .is_none_or(|pipeline| pipeline.snapshot().state != AudioDecodeWorkerState::Recovering);
        if !audio_realign_completion_ready(
            self.audio_realign_transaction,
            output_resumed,
            recovery_complete,
        ) {
            return None;
        }
        let transaction = self.audio_realign_transaction.take()?;
        self.discard_audio_retained_for_completed_realign(
            session_id,
            "audio_realign_coverage_confirmed",
        );
        Some(transaction)
    }

    pub(in super::super::super) fn restore_video_decode_skip_nonref_default(
        &mut self,
        session_id: Option<PlaybackSessionId>,
        reason: &'static str,
    ) -> std::result::Result<(), String> {
        self.video_decode_pipeline.set_skip_nonref_frames(false)?;
        let was_active =
            mark_video_decode_skip_nonref_inactive(&mut self.video_decode_skip_nonref_active);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            was_active,
            "resetting FFmpeg video decode nonref skip to default"
        );
        Ok(())
    }
}
