use super::*;

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn observe_rebuffer_far_ahead_audio_frame(
        &mut self,
        far_ahead_audio_timeline_nsecs: u64,
        current_start_position_nsecs: u64,
        audio_output_pending_nsecs: Option<u64>,
        force_immediate_realign: bool,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.playback_output_state.rebuffering() && !self.restart_pending() {
            self.rebuffer_far_ahead_audio_observation_count = 0;
            return None;
        }
        self.rebuffer_far_ahead_audio_observation_count = self
            .rebuffer_far_ahead_audio_observation_count
            .saturating_add(1);

        let (target_timeline_nsecs, anchor_timeline_nsecs, first_video_timeline_nsecs) =
            self.rebuffer_audio_realign_target(current_start_position_nsecs)?;
        if self.audio_realign_exhausted_for(target_timeline_nsecs, far_ahead_audio_timeline_nsecs) {
            return None;
        }
        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let queued_video_covers_target = self
            .scheduled_video_queue
            .buffered_until_from_nsecs(target_timeline_nsecs)
            .is_some();
        let first_video_after_anchor_gap_ms =
            (i128::from(first_video_timeline_nsecs) - i128::from(anchor_timeline_nsecs)) as f64
                / 1_000_000.0;
        let far_ahead_audio_delta_ms = (i128::from(far_ahead_audio_timeline_nsecs)
            - i128::from(target_timeline_nsecs)) as f64
            / 1_000_000.0;
        let pending_audio_covers_target = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .is_some();
        let pending_audio_near_resume_target = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .map(|buffered_until_nsecs| {
                let pending_forward_nsecs =
                    buffered_until_nsecs.saturating_sub(target_timeline_nsecs);
                let protected_target_nsecs = duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
                    .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
                pending_forward_nsecs >= protected_target_nsecs
            })
            .unwrap_or(false);
        let audio_output_empty = audio_output_pending_nsecs == Some(0);
        let audio_output_continuous = audio_output_pending_nsecs.is_some_and(|pending| pending > 0);
        let pending_audio_continuous =
            pending_audio_covers_target || pending_audio_near_resume_target;
        let recent_coordinator_stall = self
            .scheduled_video_queue
            .recent_coordinator_stall(Instant::now());
        if !force_immediate_realign
            && recent_coordinator_stall.is_some()
            && (audio_output_continuous || pending_audio_continuous)
        {
            self.rebuffer_far_ahead_audio_observation_count = 0;
            tracing::debug!(
                session_id = ?session_id,
                reason,
                far_ahead_audio_timeline_nsecs,
                target_timeline_nsecs,
                audio_output_continuous,
                pending_audio_continuous,
                recent_coordinator_stall_ms = ?recent_coordinator_stall
                    .map(|stall| stall.elapsed.as_secs_f64() * 1000.0),
                recent_coordinator_stall_age_ms = ?recent_coordinator_stall
                    .map(|stall| stall.age.as_secs_f64() * 1000.0),
                "suppressed FFmpeg audio realign after coordinator stall with continuous audio"
            );
            return None;
        }
        let progress_nsecs = self
            .pending_start_audio
            .buffered_until_from(target_timeline_nsecs)
            .map(|until| until.saturating_sub(target_timeline_nsecs))
            .unwrap_or_default()
            .max(audio_output_pending_nsecs.unwrap_or_default());
        let gap_watchdog_decision = observe_audio_reader_gap_watchdog(
            &mut self.audio_reader_gap_watchdog,
            AudioReaderGapWatchdogObservation {
                target_timeline_nsecs,
                progress_nsecs,
                has_resume_coverage: pending_audio_continuous || audio_output_continuous,
                input_can_fill_gap: false,
                force_immediate_realign,
                now: Instant::now(),
            },
        );
        if gap_watchdog_decision != AudioReaderGapWatchdogDecision::Request {
            tracing::trace!(
                session_id = ?session_id,
                target_timeline_nsecs,
                far_ahead_audio_timeline_nsecs,
                gap_watchdog_decision = ?gap_watchdog_decision,
                pending_audio_continuous,
                audio_output_continuous,
                progress_ms = progress_nsecs as f64 / 1_000_000.0,
                "deferred FFmpeg decoded-audio realign until continuity gap watchdog expires"
            );
            return None;
        }
        let realign_needed = !pending_audio_near_resume_target
            && (force_immediate_realign || audio_output_empty || !pending_audio_covers_target);
        let bypass_observation_threshold = force_immediate_realign
            || (self.rebuffer_empty_audio_output_blocked
                && self.playback_output_state.rebuffering());
        if (!bypass_observation_threshold
            && self.rebuffer_far_ahead_audio_observation_count
                < REBUFFER_AUDIO_REALIGN_AFTER_FAR_AHEAD_OBSERVATIONS)
            || !realign_needed
        {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                far_ahead_audio_timeline_nsecs,
                target_timeline_nsecs,
                anchor_timeline_nsecs,
                first_video_timeline_nsecs,
                queued_video_frames = self.scheduled_video_queue.len(),
                queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                queued_video_range_nsecs = ?queued_video_range_nsecs,
                queued_video_covers_target,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = self.rebuffer_far_ahead_audio_observation_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                pending_audio_near_resume_target,
                realign_needed,
                bypass_observation_threshold,
                force_immediate_realign,
                "observed FFmpeg rebuffer audio far ahead of video target"
            );
            return None;
        }

        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs,
            far_ahead_observation_count: self.rebuffer_far_ahead_audio_observation_count,
            reason,
        };
        if self.rebuffer_audio_realign_request.is_none() {
            self.rebuffer_audio_realign_request = Some(request);
            tracing::debug!(
                session_id = ?session_id,
                reason,
                target_timeline_nsecs,
                anchor_timeline_nsecs,
                first_video_timeline_nsecs,
                far_ahead_audio_timeline_nsecs,
                queued_video_frames = self.scheduled_video_queue.len(),
                queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                queued_video_range_nsecs = ?queued_video_range_nsecs,
                queued_video_covers_target,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = request.far_ahead_observation_count,
                audio_output_pending_ms =
                    ?audio_output_pending_nsecs.map(|duration| duration as f64 / 1_000_000.0),
                audio_output_empty,
                pending_audio_covers_target,
                pending_audio_near_resume_target,
                bypass_observation_threshold,
                force_immediate_realign,
                "requested FFmpeg rebuffer audio realign to video target"
            );
        }
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn request_output_wait_audio_reader_head_realign_if_needed(
        &mut self,
        reader_head_start_nsecs: u64,
        audio_waterline: AudioResumeWaterline,
        current_start_position_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<RebufferAudioRealignRequest> {
        if !self.waiting_for_output_resume() || self.rebuffer_audio_realign_request.is_some() {
            return None;
        }
        let (target_timeline_nsecs, anchor_timeline_nsecs, first_video_timeline_nsecs) =
            self.rebuffer_audio_realign_target(current_start_position_nsecs)?;
        let pending_audio_buffered_until_nsecs = self
            .pending_start_audio
            .buffered_until_from(audio_waterline.resume_timeline_nsecs);
        let pending_audio_covers_resume = pending_audio_buffered_until_nsecs.is_some();
        let protected_audio_target_nsecs = audio_waterline
            .target_nsecs
            .saturating_sub(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        let accepted_start_within_tolerance = audio_waterline
            .audio_accepted_start_timeline_nsecs
            .is_some()
            && audio_waterline
                .audio_accepted_start_gap_nsecs
                .is_some_and(|gap| gap <= duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE));
        let pending_audio_near_target = accepted_start_within_tolerance
            && audio_waterline
                .accepted_contiguous_coverage_nsecs
                .is_some_and(|coverage| coverage >= protected_audio_target_nsecs);
        if pending_audio_near_target {
            self.audio_reader_gap_watchdog = None;
            return None;
        }
        let audio_output_empty = audio_waterline.audio_output_pending_nsecs == Some(0);
        let audio_output_continuous = audio_waterline
            .audio_output_pending_nsecs
            .is_some_and(|pending| pending > 0);
        let pending_audio_continuous = pending_audio_covers_resume || pending_audio_near_target;
        let recent_coordinator_stall = self
            .scheduled_video_queue
            .recent_coordinator_stall(Instant::now());
        if recent_coordinator_stall.is_some()
            && (audio_output_continuous || pending_audio_continuous)
        {
            tracing::debug!(
                session_id = ?session_id,
                reader_head_start_nsecs,
                resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
                audio_output_continuous,
                pending_audio_continuous,
                recent_coordinator_stall_ms = ?recent_coordinator_stall
                    .map(|stall| stall.elapsed.as_secs_f64() * 1000.0),
                recent_coordinator_stall_age_ms = ?recent_coordinator_stall
                    .map(|stall| stall.age.as_secs_f64() * 1000.0),
                "suppressed FFmpeg audio reader realign after coordinator stall with continuous audio"
            );
            return None;
        }
        let pending_resume_coverage = pending_audio_covers_resume
            || (accepted_start_within_tolerance
                && audio_waterline
                    .accepted_contiguous_coverage_nsecs
                    .is_some_and(|coverage| coverage > 0));
        let decoded_resume_coverage = audio_waterline
            .decoded_audio_forward_nsecs
            .is_some_and(|coverage| coverage > 0);
        let audio_output_resume_coverage = audio_waterline
            .audio_output_buffered_until_nsecs
            .is_some_and(|until| until > audio_waterline.resume_timeline_nsecs);
        let has_resume_coverage =
            pending_resume_coverage || decoded_resume_coverage || audio_output_resume_coverage;
        let input_can_fill_gap = audio_waterline.audio_decode_in_flight_packets > 0;
        let progress_nsecs = audio_waterline
            .accepted_contiguous_coverage_nsecs
            .unwrap_or_default()
            .max(
                audio_waterline
                    .decoded_audio_forward_nsecs
                    .unwrap_or_default(),
            )
            .max(
                audio_waterline
                    .audio_output_buffered_until_nsecs
                    .map(|until| until.saturating_sub(audio_waterline.resume_timeline_nsecs))
                    .unwrap_or_default(),
            );
        let gap_watchdog_decision = observe_audio_reader_gap_watchdog(
            &mut self.audio_reader_gap_watchdog,
            AudioReaderGapWatchdogObservation {
                target_timeline_nsecs,
                progress_nsecs,
                has_resume_coverage,
                input_can_fill_gap,
                force_immediate_realign: false,
                now: Instant::now(),
            },
        );
        if gap_watchdog_decision != AudioReaderGapWatchdogDecision::Request {
            tracing::trace!(
                session_id = ?session_id,
                target_timeline_nsecs,
                reader_head_start_nsecs,
                gap_watchdog_decision = ?gap_watchdog_decision,
                has_resume_coverage,
                pending_resume_coverage,
                decoded_resume_coverage,
                audio_output_resume_coverage,
                input_can_fill_gap,
                progress_ms = progress_nsecs as f64 / 1_000_000.0,
                "deferred FFmpeg audio reader realign until a real continuity gap stalls"
            );
            return None;
        }
        let blocked_rebuffer_recovery = self.rebuffer_empty_audio_output_blocked
            && self.playback_output_state.rebuffering()
            && audio_waterline.below_target()
            && !pending_audio_near_target
            && (audio_output_empty || !pending_audio_covers_resume);
        let pending_contiguous_until_nsecs = self
            .pending_start_audio
            .contiguous_range_nsecs()
            .filter(|(start_nsecs, _)| {
                *start_nsecs
                    <= audio_waterline
                        .resume_timeline_nsecs
                        .saturating_add(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN))
            })
            .map(|(_, end_nsecs)| end_nsecs);
        let decoded_contiguous_until_nsecs = audio_waterline
            .decoded_audio_forward_nsecs
            .map(|forward_nsecs| {
                audio_waterline
                    .resume_timeline_nsecs
                    .saturating_add(forward_nsecs)
            })
            .or(pending_contiguous_until_nsecs)
            .or(audio_waterline.audio_output_buffered_until_nsecs);
        let in_flight_allowance_nsecs = duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN)
            .saturating_mul(
                u64::try_from(audio_waterline.audio_decode_in_flight_packets).unwrap_or(u64::MAX),
            );
        let proactive_reader_limit_nsecs = decoded_contiguous_until_nsecs.map(|until_nsecs| {
            until_nsecs
                .saturating_add(audio_waterline.audio_decode_queued_nsecs)
                .saturating_add(in_flight_allowance_nsecs)
                .saturating_add(duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE))
        });
        let blocked_rebuffer_reader_limit_nsecs = audio_waterline
            .resume_timeline_nsecs
            .saturating_add(
                audio_waterline
                    .target_nsecs
                    .max(duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)),
            )
            .saturating_add(duration_nsecs(AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN));
        let (reason, reader_limit_nsecs) = if blocked_rebuffer_recovery {
            (
                "rebuffer_audio_reader_far_ahead",
                blocked_rebuffer_reader_limit_nsecs,
            )
        } else {
            (
                "output_wait_audio_reader_continuity_gap",
                proactive_reader_limit_nsecs?,
            )
        };
        if reader_head_start_nsecs <= reader_limit_nsecs {
            return None;
        }

        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let queued_video_covers_target = self
            .scheduled_video_queue
            .buffered_until_from_nsecs(target_timeline_nsecs)
            .is_some();
        let request = RebufferAudioRealignRequest {
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            far_ahead_audio_timeline_nsecs: reader_head_start_nsecs,
            far_ahead_observation_count: 0,
            reason,
        };
        self.rebuffer_audio_realign_request = Some(request);
        tracing::debug!(
            session_id = ?session_id,
            reason = request.reason,
            reader_head_start_nsecs,
            resume_timeline_nsecs = audio_waterline.resume_timeline_nsecs,
            reader_limit_nsecs,
            proactive_reader_limit_nsecs,
            blocked_rebuffer_reader_limit_nsecs,
            blocked_rebuffer_recovery,
            current_start_position_nsecs,
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
            queued_video_frames = self.scheduled_video_queue.len(),
            queued_video_ms = self.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
            queued_video_range_nsecs = ?queued_video_range_nsecs,
            queued_video_covers_target,
            pending_audio_start_nsecs = ?audio_waterline.pending_audio_start_nsecs,
            audio_accepted_start = ?audio_waterline.audio_accepted_start_timeline_nsecs,
            start_gap_ms = ?audio_waterline
                .audio_accepted_start_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            contiguous_coverage_ms = ?audio_waterline
                .accepted_contiguous_coverage_nsecs
                .map(|coverage| coverage as f64 / 1_000_000.0),
            pending_audio_covers_resume,
            accepted_start_within_tolerance,
            pending_audio_near_target,
            protected_audio_target_ms = protected_audio_target_nsecs as f64 / 1_000_000.0,
            pending_contiguous_until_nsecs,
            decoded_contiguous_until_nsecs,
            pending_audio_forward_ms = ?audio_waterline
                .pending_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            audio_decode_queued_ms = audio_waterline.audio_decode_queued_nsecs as f64
                / 1_000_000.0,
            audio_decode_in_flight_packets = audio_waterline.audio_decode_in_flight_packets,
            in_flight_allowance_ms = in_flight_allowance_nsecs as f64 / 1_000_000.0,
            audio_output_pending_ms = ?audio_waterline
                .audio_output_pending_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            demux_audio_forward_ms = ?audio_waterline
                .demux_audio_forward_nsecs
                .map(|duration| duration as f64 / 1_000_000.0),
            "requested FFmpeg output-wait audio reader realign before resume"
        );
        Some(request)
    }

    pub(in crate::player::backend::ffmpeg) fn record_audio_continuity_rejection(
        &mut self,
        rejected_pts_nsecs: u64,
        reference_nsecs: u64,
        request: Option<RebufferAudioRealignRequest>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let now = Instant::now();
        let gap_nsecs = rejected_pts_nsecs.saturating_sub(reference_nsecs);
        if self
            .audio_continuity_rejection_summary
            .as_ref()
            .is_some_and(|summary| summary.last_rejected_pts_nsecs == rejected_pts_nsecs)
        {
            return;
        }
        let summary = self.audio_continuity_rejection_summary.get_or_insert(
            AudioContinuityRejectionSummary {
                first_rejected_pts_nsecs: rejected_pts_nsecs,
                last_rejected_pts_nsecs: rejected_pts_nsecs,
                rejected_count: 0,
                largest_gap_nsecs: 0,
                last_log_at: now
                    .checked_sub(AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL)
                    .unwrap_or(now),
            },
        );
        summary.last_rejected_pts_nsecs = rejected_pts_nsecs;
        summary.rejected_count = summary.rejected_count.saturating_add(1);
        summary.largest_gap_nsecs = summary.largest_gap_nsecs.max(gap_nsecs);
        if now.saturating_duration_since(summary.last_log_at)
            < AUDIO_CONTINUITY_REJECTION_LOG_INTERVAL
        {
            return;
        }
        summary.last_log_at = now;
        tracing::warn!(
            session_id = ?session_id,
            reason,
            first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
            last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
            rejected_count = summary.rejected_count,
            largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
            rebuffer_audio_realign_target_nsecs =
                ?request.map(|request| request.target_timeline_nsecs),
            "rate-limited FFmpeg audio continuity rejection summary"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer_far_ahead_audio_observation(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.rebuffer_far_ahead_audio_observation_count = 0;
        if let Some(summary) = self.audio_continuity_rejection_summary.take() {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
                last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
                rejected_count = summary.rejected_count,
                largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
                "completed FFmpeg audio continuity rejection series"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffer_audio_realign_request_pending(
        &self,
    ) -> bool {
        self.rebuffer_audio_realign_request.is_some()
    }

    pub(in crate::player::backend::ffmpeg) fn take_rebuffer_audio_realign_request(
        &mut self,
    ) -> Option<RebufferAudioRealignRequest> {
        let request = self.rebuffer_audio_realign_request.take();
        if request.is_some() {
            self.rebuffer_far_ahead_audio_observation_count = 0;
        }
        request
    }

    pub(in crate::player::backend::ffmpeg) fn defer_audio_reader_gap_watchdog_after_input_pending(
        &mut self,
        target_timeline_nsecs: u64,
    ) {
        if let Some(watchdog) = self.audio_reader_gap_watchdog.as_mut()
            && watchdog.target_timeline_nsecs == target_timeline_nsecs
        {
            watchdog.last_progress_at = Instant::now();
            watchdog.request_issued = false;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn prepare_audio_after_rebuffer_realign(
        &mut self,
        target_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        debug_assert!(
            self.pending_start_audio.is_empty(),
            "pending audio must be retained transactionally before realign"
        );
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_audio_realign_request = None;
        self.audio_reader_gap_watchdog = None;
        self.rebuffer_far_ahead_audio_observation_count = 0;
        if let Some(summary) = self.audio_continuity_rejection_summary.take() {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                first_rejected_pts_nsecs = summary.first_rejected_pts_nsecs,
                last_rejected_pts_nsecs = summary.last_rejected_pts_nsecs,
                rejected_count = summary.rejected_count,
                largest_gap_ms = summary.largest_gap_nsecs as f64 / 1_000_000.0,
                "completed FFmpeg audio continuity rejection series after realign"
            );
        }
        self.set_audio_sync_drop_before_timeline_nsecs(target_timeline_nsecs, session_id, reason);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            "prepared FFmpeg audio output scheduler for transactional rebuffer realign"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_far_ahead_reference_timeline_nsecs(
        &self,
        current_start_position_nsecs: u64,
        audio_snapshot: Option<AudioOutputSnapshot>,
    ) -> u64 {
        let actual_audio_timeline_nsecs = audio_snapshot
            .map(|snapshot| {
                if snapshot.total_pending_nsecs > 0 {
                    snapshot
                        .played_timeline_nsecs
                        .max(snapshot.buffered_until_timeline_nsecs)
                } else {
                    snapshot.played_timeline_nsecs
                }
            })
            .unwrap_or(current_start_position_nsecs);
        let resume_reference_nsecs = if self.playback_output_state.rebuffering() {
            self.rebuffer_audio_realign_target(current_start_position_nsecs)
                .map(|(target_timeline_nsecs, _, _)| target_timeline_nsecs)
                .or_else(|| {
                    self.video_output_rebuffer_anchor
                        .map(|anchor| anchor.timeline_nsecs)
                })
        } else if self.restart_pending() {
            self.scheduled_video_queue
                .range_nsecs()
                .map(|(first_video_timeline_nsecs, _)| first_video_timeline_nsecs)
        } else {
            None
        };
        resume_reference_nsecs
            .unwrap_or(current_start_position_nsecs)
            .max(current_start_position_nsecs)
            .max(actual_audio_timeline_nsecs)
    }

    pub(super) fn rebuffer_audio_realign_target(
        &self,
        current_start_position_nsecs: u64,
    ) -> Option<(u64, u64, u64)> {
        let (first_video_timeline_nsecs, _) = self.scheduled_video_queue.range_nsecs()?;
        let anchor_timeline_nsecs = self
            .video_output_rebuffer_anchor
            .map(|anchor| anchor.timeline_nsecs)
            .unwrap_or(current_start_position_nsecs);
        let target_timeline_nsecs = if first_video_timeline_nsecs <= anchor_timeline_nsecs
            && self
                .scheduled_video_queue
                .buffered_until_from_nsecs(anchor_timeline_nsecs)
                .is_some()
        {
            anchor_timeline_nsecs
        } else {
            first_video_timeline_nsecs
        };
        Some((
            target_timeline_nsecs,
            anchor_timeline_nsecs,
            first_video_timeline_nsecs,
        ))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn begin_audio_gap_recovery(
        &mut self,
        target_timeline_nsecs: u64,
        now: Instant,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        self.audio_gap_recovery_until = now.checked_add(AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR);
        self.audio_gap_recovery_target_nsecs = Some(target_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            suppress_rebuffer_ms =
                AUDIO_GAP_RECOVERY_SUPPRESS_REBUFFER_FOR.as_secs_f64() * 1000.0,
            "entered FFmpeg audio gap recovery after video-clock resume"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_realign_exhausted_for(
        &self,
        target_timeline_nsecs: u64,
        frame_timeline_nsecs: u64,
    ) -> bool {
        self.audio_realign_exhausted_range_nsecs
            .is_some_and(|(start, end)| {
                let tolerance = duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE);
                target_timeline_nsecs.saturating_add(tolerance) >= start
                    && target_timeline_nsecs <= end
                    && frame_timeline_nsecs <= end.saturating_add(tolerance)
            })
    }

    pub(in crate::player::backend::ffmpeg) fn resume_after_exhausted_audio_realign(
        &mut self,
        target_timeline_nsecs: u64,
        far_ahead_audio_timeline_nsecs: u64,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> u64 {
        // A bounded recovery failure is not proof of missing media. Remember
        // the attempted interval until the next timeline reset, and admit its
        // deferred audio instead of starting the same destructive seek again.
        self.audio_realign_exhausted_range_nsecs = Some((
            target_timeline_nsecs,
            far_ahead_audio_timeline_nsecs.max(target_timeline_nsecs),
        ));
        let first_video_timeline_nsecs = self
            .scheduled_video_queue
            .range_nsecs()
            .map(|(start_nsecs, _)| start_nsecs)
            .unwrap_or(target_timeline_nsecs);
        let resume_timeline_nsecs = first_video_timeline_nsecs.max(target_timeline_nsecs);
        control.set_output_rebuffer_paused(false);
        self.video_output_underrun_started_at = None;
        self.rebuffer_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        self.rebuffer_empty_audio_output_blocked = false;
        self.rebuffer_audio_realign_request = None;
        self.audio_reader_gap_watchdog = None;
        self.set_state(PlaybackOutputState::Playing);
        self.mark_video_clock_anchor_valid();
        self.begin_audio_gap_recovery(
            resume_timeline_nsecs,
            Instant::now(),
            session_id,
            "bounded_audio_realign_exhausted",
        );
        self.clear_rebuffer_far_ahead_audio_observation(
            session_id,
            "bounded_audio_realign_exhausted",
        );
        tracing::warn!(
            session_id = ?session_id,
            target_timeline_nsecs,
            first_video_timeline_nsecs,
            resume_timeline_nsecs,
            far_ahead_audio_timeline_nsecs,
            far_ahead_audio_delta_ms = far_ahead_audio_timeline_nsecs
                .saturating_sub(target_timeline_nsecs) as f64
                / 1_000_000.0,
            video_clock_anchor_valid = self.video_clock_anchor_valid(),
            "resumed FFmpeg video clock after bounded audio realign was exhausted"
        );
        resume_timeline_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn audio_gap_recovery_active(&self) -> bool {
        self.audio_gap_recovery_until.is_some()
    }

    pub(in crate::player::backend::ffmpeg) fn clear_audio_gap_recovery_if_audio_ready(
        &mut self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        played_until_nsecs: Option<u64>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> bool {
        let Some(target_timeline_nsecs) = self.audio_gap_recovery_target_nsecs else {
            return false;
        };
        let audio_attach_timeline_nsecs = played_until_nsecs
            .unwrap_or(target_timeline_nsecs)
            .max(target_timeline_nsecs);
        let audio_output_covers = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)
                && snapshot.buffered_until_timeline_nsecs > audio_attach_timeline_nsecs
        });
        if !audio_output_covers {
            return false;
        }
        self.audio_gap_recovery_until = None;
        self.audio_gap_recovery_target_nsecs = None;
        tracing::debug!(
            session_id = ?session_id,
            reason,
            target_timeline_nsecs,
            audio_attach_timeline_nsecs,
            audio_output_covers,
            "cleared FFmpeg audio gap recovery after audio reattached"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn set_rebuffer_empty_audio_output_blocked(
        &mut self,
        blocked: bool,
    ) {
        self.rebuffer_empty_audio_output_blocked =
            blocked && self.playback_output_state.rebuffering();
    }

    pub(in crate::player::backend::ffmpeg) fn set_audio_sync_drop_before_timeline_nsecs(
        &mut self,
        drop_before_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        if self
            .audio_sync_drop_before_timeline_nsecs
            .is_some_and(|current| current >= drop_before_timeline_nsecs)
        {
            return;
        }
        self.finish_audio_sync_drop_log_summary(session_id, "drop_before_advanced");
        self.audio_sync_drop_before_timeline_nsecs = Some(drop_before_timeline_nsecs);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs,
            "set FFmpeg audio sync drop-before timeline"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn audio_sync_drop_before_timeline_nsecs(
        &self,
    ) -> Option<u64> {
        self.audio_sync_drop_before_timeline_nsecs
    }

    pub(in crate::player::backend::ffmpeg) fn record_audio_sync_drop_before_frame(
        &mut self,
        raw_timestamp: i64,
        timeline_nsecs: u64,
        buffered_until_nsecs: u64,
        output_snapshot: PlaybackOutputSnapshot,
        session_id: PlaybackSessionId,
    ) {
        let Some(drop_before_timeline_nsecs) = self.audio_sync_drop_before_timeline_nsecs else {
            return;
        };
        if self
            .audio_sync_drop_log_summary
            .is_some_and(|summary| summary.drop_before_timeline_nsecs != drop_before_timeline_nsecs)
        {
            self.finish_audio_sync_drop_log_summary(session_id, "drop_before_changed");
        }

        let now = Instant::now();
        let summary = self
            .audio_sync_drop_log_summary
            .get_or_insert(AudioSyncDropLogSummary {
                drop_before_timeline_nsecs,
                started_at: now,
                last_log_at: now,
                total_dropped_frames: 0,
                suppressed_since_last_log: 0,
                first_raw_timestamp: raw_timestamp,
                last_raw_timestamp: raw_timestamp,
                first_timeline_nsecs: timeline_nsecs,
                last_timeline_nsecs: timeline_nsecs,
                last_buffered_until_nsecs: buffered_until_nsecs,
            });
        summary.total_dropped_frames = summary.total_dropped_frames.saturating_add(1);
        summary.last_raw_timestamp = raw_timestamp;
        summary.last_timeline_nsecs = timeline_nsecs;
        summary.last_buffered_until_nsecs = buffered_until_nsecs;
        let first = summary.total_dropped_frames == 1;
        let periodic_summary = !first
            && now.saturating_duration_since(summary.last_log_at)
                >= AUDIO_SYNC_DROP_LOG_SUMMARY_INTERVAL;
        if !first && !periodic_summary {
            summary.suppressed_since_last_log = summary.suppressed_since_last_log.saturating_add(1);
            return;
        }
        let suppressed_dropped_frames = std::mem::take(&mut summary.suppressed_since_last_log);
        summary.last_log_at = now;
        let summary = *summary;
        tracing::debug!(
            session_id = ?session_id,
            raw_timestamp,
            timeline_nsecs,
            buffered_until_nsecs,
            drop_before_timeline_nsecs,
            total_dropped_frames = summary.total_dropped_frames,
            suppressed_dropped_frames,
            log_kind = if first { "first" } else { "periodic_summary" },
            elapsed_ms = now
                .saturating_duration_since(summary.started_at)
                .as_secs_f64()
                * 1_000.0,
            output_state = ?output_snapshot.state,
            first_video_frame_pending = output_snapshot.first_video_frame_pending,
            rebuffering = output_snapshot.rebuffering,
            "dropping FFmpeg audio frames before rebuffer audio sync drop-before"
        );
    }

    pub(super) fn finish_audio_sync_drop_log_summary(
        &mut self,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) {
        let Some(summary) = self.audio_sync_drop_log_summary.take() else {
            return;
        };
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs = summary.drop_before_timeline_nsecs,
            total_dropped_frames = summary.total_dropped_frames,
            suppressed_dropped_frames = summary.suppressed_since_last_log,
            first_raw_timestamp = summary.first_raw_timestamp,
            last_raw_timestamp = summary.last_raw_timestamp,
            first_timeline_nsecs = summary.first_timeline_nsecs,
            last_timeline_nsecs = summary.last_timeline_nsecs,
            last_buffered_until_nsecs = summary.last_buffered_until_nsecs,
            elapsed_ms = summary.started_at.elapsed().as_secs_f64() * 1_000.0,
            "finished aggregated FFmpeg audio sync drop-before sequence"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn clear_audio_sync_drop_before_if_covered(
        &mut self,
        audio_snapshot: Option<AudioOutputSnapshot>,
        session_id: PlaybackSessionId,
        reason: &'static str,
    ) -> bool {
        let Some(drop_before_timeline_nsecs) = self.audio_sync_drop_before_timeline_nsecs else {
            return false;
        };
        let audio_output_covers_drop_before = audio_snapshot.is_some_and(|snapshot| {
            snapshot.total_pending_nsecs > 0
                && snapshot.buffered_until_timeline_nsecs > drop_before_timeline_nsecs
        });
        if !audio_output_covers_drop_before {
            return false;
        }
        self.audio_sync_drop_before_timeline_nsecs = None;
        self.finish_audio_sync_drop_log_summary(session_id, reason);
        tracing::debug!(
            session_id = ?session_id,
            reason,
            drop_before_timeline_nsecs,
            audio_output_covers_drop_before,
            "cleared FFmpeg audio sync drop-before timeline after coverage"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn observe_audio_output_underrun_for_rebuffer(
        &mut self,
        now: Instant,
        session_id: PlaybackSessionId,
    ) {
        let window_active = self
            .recent_audio_output_underrun_window_started_at
            .is_some_and(|started_at| {
                now.saturating_duration_since(started_at) <= AUDIO_REBUFFER_LOOP_DETECTION_WINDOW
            });
        if !window_active {
            self.recent_audio_output_underrun_window_started_at = Some(now);
            self.recent_audio_output_underruns = 1;
            return;
        }

        self.recent_audio_output_underruns = self.recent_audio_output_underruns.saturating_add(1);
        if self.audio_rebuffer_loop_active() {
            tracing::debug!(
                session_id = ?session_id,
                recent_audio_output_underruns = self.recent_audio_output_underruns,
                loop_window_ms = AUDIO_REBUFFER_LOOP_DETECTION_WINDOW.as_secs_f64() * 1000.0,
                "detected repeated FFmpeg audio output underruns; using loop recovery waterline"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_rebuffer_loop_active(
        &self,
    ) -> bool {
        self.recent_audio_output_underruns >= 2
            && self
                .recent_audio_output_underrun_window_started_at
                .is_some_and(|started_at| {
                    started_at.elapsed() <= AUDIO_REBUFFER_LOOP_DETECTION_WINDOW
                })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn audio_rebuffer_prefill_target_nsecs(
        &self,
        queued_video_contiguous_forward_nsecs: Option<u64>,
    ) -> u64 {
        let base_target = if self.audio_rebuffer_loop_active() {
            AUDIO_REBUFFER_PREFILL_LOOP_TARGET
        } else {
            AUDIO_REBUFFER_PREFILL_TARGET
        };
        let mut target_nsecs = duration_nsecs(base_target.min(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION));
        if let Some(video_forward_nsecs) = queued_video_contiguous_forward_nsecs {
            target_nsecs = target_nsecs.min(video_forward_nsecs);
        }
        target_nsecs
    }
}
