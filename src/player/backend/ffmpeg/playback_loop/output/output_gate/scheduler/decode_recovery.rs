use super::*;

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn begin_decode_recovery(
        &mut self,
        transaction_id: u64,
        target_nsecs: u64,
        source: DecodeRecoverySource,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) {
        if self.playback_output_state.rebuffering() || control.is_output_rebuffer_paused() {
            clear_video_output_rebuffer(&mut self.playback_output_state, control);
            self.set_state(PlaybackOutputState::Playing);
        }
        let discarded_future_frames = self.scheduled_video_queue.discard_at_or_after(target_nsecs);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.video_decode_underfill = false;
        let started_at = Instant::now();
        self.decode_recovery_transaction = Some(DecodeRecoveryTransaction {
            transaction_id: transaction_id.max(1),
            target_nsecs,
            resume_nsecs: target_nsecs,
            phase: DecodeRecoveryPhase::SwitchingDecoder,
            disposition: DecodeRecoveryDisposition::Exact,
            source,
            gap_provenance: DecodeRecoveryGapProvenance::ContinuousMedia,
            drop_for_fallback: None,
            staging_queue: ScheduledVideoQueue::default(),
            staging_frame_budget: None,
            staging_budget_reached: false,
            audio_ready_latched: false,
            started_at,
            barrier_started_at: None,
            first_staged_frame_nsecs: None,
            last_staged_frame_nsecs: None,
            largest_confirmed_gap_nsecs: 0,
            bridged_gap_count: 0,
            rejected_frame_count: 0,
            first_rejected_frame_nsecs: None,
            last_rejected_frame_nsecs: None,
            last_rejection_log_at: None,
        });
        // Decoder input can become blocked as soon as recovery owns frame
        // admission. Wake the output side independently so the transaction is
        // not waiting for another demux/decode event to make progress.
        self.note_output_housekeeping_change();
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = transaction_id.max(1),
            target_nsecs,
            recovery_source = source.as_str(),
            discarded_future_frames,
            retained_video_range = ?self.scheduled_video_queue.range_nsecs(),
            "started independent HEVC decode recovery output transaction"
        );
    }

    pub(in crate::player::backend::ffmpeg) fn mark_decode_recovery_replaying(
        &mut self,
        transaction_id: u64,
    ) {
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return;
        };
        if transaction.transaction_id == transaction_id
            && transaction.phase == DecodeRecoveryPhase::SwitchingDecoder
        {
            transaction.phase = DecodeRecoveryPhase::Replaying;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_phase(
        &self,
    ) -> Option<DecodeRecoveryPhase> {
        self.decode_recovery_transaction
            .as_ref()
            .map(|transaction| transaction.phase)
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_active(&self) -> bool {
        self.decode_recovery_phase()
            .is_some_and(|phase| !phase.terminal())
    }

    pub(super) fn decode_recovery_drained_boundary_ready(&self) -> bool {
        self.scheduled_video_queue.is_empty()
            && self
                .decode_recovery_transaction
                .as_ref()
                .is_some_and(|transaction| {
                    transaction.phase == DecodeRecoveryPhase::Buffered
                        && transaction.source == DecodeRecoverySource::DecoderError
                        && transaction.disposition == DecodeRecoveryDisposition::Reanchor
                })
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_owns_video_admission(&self) -> bool {
        self.decode_recovery_phase().is_some_and(|phase| {
            !matches!(
                phase,
                DecodeRecoveryPhase::Committed
                    | DecodeRecoveryPhase::CommittedGap
                    | DecodeRecoveryPhase::Reanchored
            )
        })
    }

    pub(in crate::player::backend::ffmpeg) fn committed_video_queue_end_nsecs(
        &self,
    ) -> Option<u64> {
        self.scheduled_video_queue
            .range_nsecs()
            .map(|(_, end_nsecs)| end_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn admitted_video_queue_end_nsecs(&self) -> Option<u64> {
        self.committed_video_queue_end_nsecs()
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staged_high_water_nsecs(
        &self,
    ) -> Option<u64> {
        self.recovery_staging_range_nsecs()
            .map(|(_, end_nsecs)| end_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_range_nsecs(
        &self,
    ) -> Option<(u64, u64)> {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .and_then(|transaction| transaction.staging_queue.range_nsecs())
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_frames(&self) -> usize {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .map(|transaction| transaction.staging_queue.len())
            .unwrap_or_default()
    }

    pub(in crate::player::backend::ffmpeg) fn recovery_staging_frame_budget(
        &self,
    ) -> Option<usize> {
        self.decode_recovery_transaction
            .as_ref()
            .filter(|transaction| !transaction.phase.terminal())
            .and_then(|transaction| transaction.staging_frame_budget)
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_video_admission_blocked(
        &self,
    ) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .is_some_and(|transaction| match transaction.phase {
                DecodeRecoveryPhase::Committed
                | DecodeRecoveryPhase::CommittedGap
                | DecodeRecoveryPhase::Reanchored => false,
                DecodeRecoveryPhase::Buffered
                | DecodeRecoveryPhase::DroppedForFallback
                | DecodeRecoveryPhase::Failed => true,
                DecodeRecoveryPhase::SwitchingDecoder | DecodeRecoveryPhase::Replaying => {
                    transaction.staging_budget_reached
                }
            })
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_output_committed(&self) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .is_some_and(|transaction| {
                matches!(
                    transaction.phase,
                    DecodeRecoveryPhase::Committed
                        | DecodeRecoveryPhase::CommittedGap
                        | DecodeRecoveryPhase::Reanchored
                )
            })
    }

    pub(in crate::player::backend::ffmpeg) fn confirm_decode_recovery_synchronized_timeline_gap(
        &mut self,
    ) {
        if let Some(transaction) = self.decode_recovery_transaction.as_mut()
            && !transaction.phase.terminal()
        {
            transaction.gap_provenance =
                DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn take_decode_recovery_drop_for_fallback(
        &mut self,
    ) -> Option<DecodeRecoveryDropForFallback> {
        self.decode_recovery_transaction
            .as_mut()
            .and_then(|transaction| transaction.drop_for_fallback.take())
    }

    pub(in crate::player::backend::ffmpeg) fn release_vulkan_frames_for_resource_pressure(
        &mut self,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> usize {
        let released_scheduled_frames = self.scheduled_video_queue.len();
        let released_scheduled_range = self.scheduled_video_queue.range_nsecs();
        self.scheduled_video_queue.clear();
        let mut transaction_id = None;
        let mut released_staging_frames = 0;
        let mut released_staging_range = None;
        if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
            transaction_id = Some(transaction.transaction_id);
            released_staging_frames = transaction.staging_queue.len();
            released_staging_range = transaction.staging_queue.range_nsecs();
            transaction.staging_queue.clear();
            transaction.staging_budget_reached = true;
            transaction.phase = DecodeRecoveryPhase::Failed;
        }
        control.set_output_rebuffer_paused(true);
        let released_frames = released_scheduled_frames.saturating_add(released_staging_frames);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id = ?transaction_id,
            released_scheduled_video_frames = released_scheduled_frames,
            released_scheduled_video_range = ?released_scheduled_range,
            released_recovery_staging_frames = released_staging_frames,
            released_recovery_staging_range = ?released_staging_range,
            released_vulkan_frames = released_frames,
            "released scheduler-owned Vulkan frames after decoder resource pressure"
        );
        released_frames
    }

    pub(in crate::player::backend::ffmpeg) fn update_decode_recovery_audio_ready(
        &mut self,
        has_audio_output: bool,
        audio_snapshot: Option<AudioOutputSnapshot>,
        audio_decode_queued_nsecs: u64,
    ) -> bool {
        if !has_audio_output {
            if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
                transaction.audio_ready_latched = true;
            }
            return true;
        }
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return true;
        };
        if transaction.disposition == DecodeRecoveryDisposition::Reanchor {
            transaction.audio_ready_latched = true;
            return true;
        }
        if transaction.audio_ready_latched {
            return true;
        }

        let device_pending_nsecs = audio_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs)
            .unwrap_or_default();
        let pending_start_forward_nsecs = self
            .pending_start_audio
            .forward_duration_from(transaction.resume_nsecs)
            .unwrap_or_default();
        let available_nsecs = device_pending_nsecs
            .saturating_add(audio_decode_queued_nsecs)
            .saturating_add(pending_start_forward_nsecs);
        let ready_threshold_nsecs = DECODE_RECOVERY_STAGING_NSECS
            .saturating_sub(DECODE_RECOVERY_AUDIO_READY_HYSTERESIS_NSECS);
        if available_nsecs >= ready_threshold_nsecs {
            transaction.audio_ready_latched = true;
            tracing::debug!(
                transaction_id = transaction.transaction_id,
                resume_nsecs = transaction.resume_nsecs,
                available_audio_ms = available_nsecs as f64 / 1_000_000.0,
                device_pending_ms = device_pending_nsecs as f64 / 1_000_000.0,
                decoded_audio_ms = audio_decode_queued_nsecs as f64 / 1_000_000.0,
                pending_start_forward_ms = pending_start_forward_nsecs as f64 / 1_000_000.0,
                ready_threshold_ms = ready_threshold_nsecs as f64 / 1_000_000.0,
                "latched audio readiness for HEVC decode recovery commit"
            );
        }
        transaction.audio_ready_latched
    }

    pub(in crate::player::backend::ffmpeg) fn decode_recovery_audio_ready_latched(&self) -> bool {
        self.decode_recovery_transaction
            .as_ref()
            .map(|transaction| transaction.audio_ready_latched)
            .unwrap_or(true)
    }

    pub(in crate::player::backend::ffmpeg) fn stage_decode_recovery_frame(
        &mut self,
        mut frame: QueuedVideoFrame,
        vo_queue_capacity: usize,
        session_id: PlaybackSessionId,
    ) -> bool {
        let now = Instant::now();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return false;
        };
        if transaction.phase.terminal() {
            return false;
        }

        let staging_frame_budget = *transaction.staging_frame_budget.get_or_insert_with(|| {
            decode_recovery_staging_frame_budget(frame.duration_nsecs, vo_queue_capacity)
        });
        if transaction.staging_queue.len() >= staging_frame_budget {
            if !transaction.staging_budget_reached {
                transaction.staging_budget_reached = true;
                tracing::warn!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    phase = ?transaction.phase,
                    staging_frames = transaction.staging_queue.len(),
                    staging_frame_budget,
                    recovery_staged_high_water_nsecs = ?transaction
                        .staging_queue
                        .range_nsecs()
                        .map(|(_, end)| end),
                    "HEVC decode recovery staging reached its hard Vulkan-frame budget"
                );
            }
            return true;
        }

        let original_frame_nsecs = frame.timeline_nsecs;
        let frame_end_nsecs = frame.timeline_nsecs.saturating_add(frame.duration_nsecs);
        if frame_end_nsecs <= transaction.target_nsecs {
            transaction.rejected_frame_count = transaction.rejected_frame_count.saturating_add(1);
            transaction
                .first_rejected_frame_nsecs
                .get_or_insert(original_frame_nsecs);
            transaction.last_rejected_frame_nsecs = Some(original_frame_nsecs);
            let should_log = transaction.rejected_frame_count == 1
                || transaction.last_rejection_log_at.is_none_or(|last_log_at| {
                    now.saturating_duration_since(last_log_at)
                        >= DECODE_RECOVERY_REJECTION_LOG_INTERVAL
                });
            if should_log {
                transaction.last_rejection_log_at = Some(now);
                tracing::debug!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    target_nsecs = transaction.target_nsecs,
                    rejected_frame_nsecs = original_frame_nsecs,
                    rejected_frame_end_nsecs = frame_end_nsecs,
                    rejected_frame_count = transaction.rejected_frame_count,
                    "rate-limited pre-boundary frame rejection during HEVC decode recovery"
                );
            }
            return true;
        }

        if transaction.staging_queue.is_empty() {
            if frame.timeline_nsecs < transaction.target_nsecs {
                frame.timeline_nsecs = transaction.target_nsecs;
                frame.duration_nsecs = frame_end_nsecs.saturating_sub(transaction.target_nsecs);
            } else {
                let initial_gap_nsecs = frame
                    .timeline_nsecs
                    .saturating_sub(transaction.target_nsecs);
                transaction.largest_confirmed_gap_nsecs = transaction
                    .largest_confirmed_gap_nsecs
                    .max(initial_gap_nsecs);
                if transaction.source == DecodeRecoverySource::DecoderError
                    && initial_gap_nsecs > DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS
                {
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        recovery_source = transaction.source.as_str(),
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        decoder_output_gap_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                        "scheduled HEVC decoder-error gap for boundary reanchor"
                    );
                } else if initial_gap_nsecs > 0
                    && decode_recovery_gap_within_limit(
                        initial_gap_nsecs,
                        DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                    )
                {
                    transaction.disposition = DecodeRecoveryDisposition::HoldGap;
                    transaction.bridged_gap_count = transaction.bridged_gap_count.saturating_add(1);
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        first_software_frame_nsecs = frame.timeline_nsecs,
                        confirmed_gap_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                        hold_gap_limit_ms =
                            DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                        timestamp_tolerance_ms =
                            DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS as f64 / 1_000_000.0,
                        "accepted recovery frame after confirmed media timeline gap"
                    );
                } else if !decode_recovery_gap_within_limit(
                    initial_gap_nsecs,
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                ) {
                    if transaction.gap_provenance
                        == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
                    {
                        transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                        transaction.resume_nsecs = frame.timeline_nsecs;
                        tracing::warn!(
                            session_id = ?session_id,
                            transaction_id = transaction.transaction_id,
                            recovery_source = transaction.source.as_str(),
                            gap_provenance = ?transaction.gap_provenance,
                            target_nsecs = transaction.target_nsecs,
                            resume_nsecs = transaction.resume_nsecs,
                            discontinuity_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                            hold_gap_limit_ms =
                                DECODE_RECOVERY_HOLD_GAP_MAX_NSECS as f64 / 1_000_000.0,
                            "reanchoring HEVC decode recovery at confirmed synchronized media discontinuity"
                        );
                    } else {
                        transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                        transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                        transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                            transaction_id: transaction.transaction_id,
                            target_nsecs: transaction.target_nsecs,
                            first_frame_nsecs: frame.timeline_nsecs,
                            gap_nsecs: initial_gap_nsecs,
                            source: transaction.source,
                        });
                        tracing::warn!(
                            session_id = ?session_id,
                            transaction_id = transaction.transaction_id,
                            recovery_source = transaction.source.as_str(),
                            gap_provenance = ?transaction.gap_provenance,
                            target_nsecs = transaction.target_nsecs,
                            first_frame_nsecs = frame.timeline_nsecs,
                            discontinuity_ms = initial_gap_nsecs as f64 / 1_000_000.0,
                            "withheld recovery frame at unbridged continuous decode gap"
                        );
                        self.note_output_housekeeping_change();
                        return true;
                    }
                }
            }
        } else if let Some((previous_pts_nsecs, previous_duration_nsecs)) =
            transaction.staging_queue.back_timing_nsecs()
        {
            let previous_end_nsecs = previous_pts_nsecs.saturating_add(previous_duration_nsecs);
            let gap_nsecs = frame.timeline_nsecs.saturating_sub(previous_end_nsecs);
            if gap_nsecs > 0 {
                transaction.largest_confirmed_gap_nsecs =
                    transaction.largest_confirmed_gap_nsecs.max(gap_nsecs);
                if transaction.source == DecodeRecoverySource::DecoderError
                    && gap_nsecs > DECODE_RECOVERY_TIMESTAMP_TOLERANCE_NSECS
                {
                    let discarded_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    transaction.first_staged_frame_nsecs = None;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        decoder_output_gap_ms = gap_nsecs as f64 / 1_000_000.0,
                        discarded_staging_frames,
                        "rescheduled HEVC decoder-error recovery at later boundary gap"
                    );
                } else if decode_recovery_gap_within_limit(
                    gap_nsecs,
                    DECODE_RECOVERY_HOLD_GAP_MAX_NSECS,
                ) {
                    if transaction
                        .staging_queue
                        .extend_back_duration_to(frame.timeline_nsecs)
                        .is_some()
                    {
                        transaction.bridged_gap_count =
                            transaction.bridged_gap_count.saturating_add(1);
                        if transaction.disposition == DecodeRecoveryDisposition::Exact {
                            transaction.disposition = DecodeRecoveryDisposition::HoldGap;
                        }
                    }
                } else if transaction.gap_provenance
                    == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
                {
                    let discarded_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                    transaction.resume_nsecs = frame.timeline_nsecs;
                    transaction.first_staged_frame_nsecs = None;
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        target_nsecs = transaction.target_nsecs,
                        resume_nsecs = transaction.resume_nsecs,
                        discontinuity_ms = gap_nsecs as f64 / 1_000_000.0,
                        discarded_staging_frames,
                        "reanchoring HEVC decode recovery at large staging discontinuity"
                    );
                } else {
                    let released_staging_frames = transaction.staging_queue.len();
                    transaction.staging_queue.clear();
                    transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                    transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                    transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                        transaction_id: transaction.transaction_id,
                        target_nsecs: transaction.target_nsecs,
                        first_frame_nsecs: frame.timeline_nsecs,
                        gap_nsecs,
                        source: transaction.source,
                    });
                    tracing::warn!(
                        session_id = ?session_id,
                        transaction_id = transaction.transaction_id,
                        recovery_source = transaction.source.as_str(),
                        gap_provenance = ?transaction.gap_provenance,
                        target_nsecs = transaction.target_nsecs,
                        first_frame_nsecs = frame.timeline_nsecs,
                        discontinuity_ms = gap_nsecs as f64 / 1_000_000.0,
                        released_staging_frames,
                        "withheld staged recovery frame at unbridged continuous decode gap"
                    );
                    self.note_output_housekeeping_change();
                    return true;
                }
            }
        }

        // HoldGap and Reanchor both account for a timeline interval where the
        // decoder returned no usable pictures; that skipped interval is not
        // replay work. Match mpv's discontinuity handling and measure the
        // bounded buffer built after the first usable frame/new anchor.
        let replay_span_origin_nsecs = match transaction.disposition {
            DecodeRecoveryDisposition::HoldGap => transaction
                .first_staged_frame_nsecs
                .unwrap_or(frame.timeline_nsecs),
            DecodeRecoveryDisposition::Reanchor => transaction.resume_nsecs,
            DecodeRecoveryDisposition::Exact | DecodeRecoveryDisposition::DropForFallback => {
                transaction.target_nsecs
            }
        };
        let replay_span_nsecs = frame_end_nsecs.saturating_sub(replay_span_origin_nsecs);
        if replay_span_nsecs >= DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS {
            let discarded_staging_frames = transaction.staging_queue.len();
            transaction.staging_queue.clear();
            transaction.staging_budget_reached = true;
            if transaction.gap_provenance
                == DecodeRecoveryGapProvenance::ConfirmedSynchronizedTimelineGap
            {
                transaction.disposition = DecodeRecoveryDisposition::Reanchor;
                transaction.resume_nsecs = frame.timeline_nsecs;
                transaction.first_staged_frame_nsecs = Some(frame.timeline_nsecs);
                transaction.last_staged_frame_nsecs = Some(frame.timeline_nsecs);
                transaction.staging_queue.push_queued(frame);
                transaction.phase = DecodeRecoveryPhase::Buffered;
                tracing::warn!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    disposition = ?transaction.disposition,
                    replay_span_origin_nsecs,
                    replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                    replay_span_limit_ms =
                        DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS as f64 / 1_000_000.0,
                    discarded_staging_frames,
                    staging_frames = transaction.staging_queue.len(),
                    resume_nsecs = transaction.resume_nsecs,
                    "bounded HEVC recovery reanchored at the replay-span limit"
                );
            } else {
                let gap_nsecs = frame
                    .timeline_nsecs
                    .saturating_sub(transaction.target_nsecs);
                transaction.disposition = DecodeRecoveryDisposition::DropForFallback;
                transaction.phase = DecodeRecoveryPhase::DroppedForFallback;
                transaction.drop_for_fallback = Some(DecodeRecoveryDropForFallback {
                    transaction_id: transaction.transaction_id,
                    target_nsecs: transaction.target_nsecs,
                    first_frame_nsecs: frame.timeline_nsecs,
                    gap_nsecs,
                    source: transaction.source,
                });
                tracing::error!(
                    session_id = ?session_id,
                    transaction_id = transaction.transaction_id,
                    disposition = ?transaction.disposition,
                    replay_span_origin_nsecs,
                    replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                    replay_span_limit_ms =
                        DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS as f64 / 1_000_000.0,
                    discarded_staging_frames,
                    first_frame_nsecs = frame.timeline_nsecs,
                    gap_ms = gap_nsecs as f64 / 1_000_000.0,
                    "failed bounded HEVC recovery at the replay-span limit"
                );
                self.note_output_housekeeping_change();
            }
            return true;
        }

        transaction
            .first_staged_frame_nsecs
            .get_or_insert(frame.timeline_nsecs);
        transaction.last_staged_frame_nsecs = Some(frame.timeline_nsecs);
        transaction.staging_queue.push_queued(frame);
        let staging_forward_nsecs = transaction
            .staging_queue
            .strict_forward_nsecs_from(transaction.resume_nsecs)
            .unwrap_or_default();
        let wall_time_exhausted =
            now.saturating_duration_since(transaction.started_at) >= DECODE_RECOVERY_MAX_WALL_TIME;
        let replay_span_exhausted = replay_span_nsecs >= DECODE_RECOVERY_MAX_REPLAY_SPAN_NSECS;
        let force_bounded_reanchor = staging_forward_nsecs < DECODE_RECOVERY_STAGING_NSECS
            && (wall_time_exhausted || replay_span_exhausted)
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor;
        let entered_buffered = transaction.phase != DecodeRecoveryPhase::Buffered
            && (staging_forward_nsecs >= DECODE_RECOVERY_STAGING_NSECS || force_bounded_reanchor);
        if entered_buffered {
            transaction.phase = DecodeRecoveryPhase::Buffered;
            tracing::debug!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                recovery_source = transaction.source.as_str(),
                gap_provenance = ?transaction.gap_provenance,
                target_nsecs = transaction.target_nsecs,
                resume_nsecs = transaction.resume_nsecs,
                disposition = ?transaction.disposition,
                staging_frames = transaction.staging_queue.len(),
                staging_forward_ms = staging_forward_nsecs as f64 / 1_000_000.0,
                replay_span_ms = replay_span_nsecs as f64 / 1_000_000.0,
                wall_elapsed_ms = now
                    .saturating_duration_since(transaction.started_at)
                    .as_secs_f64()
                    * 1_000.0,
                bounded_reanchor = force_bounded_reanchor,
                bridged_gap_count = transaction.bridged_gap_count,
                largest_confirmed_gap_ms =
                    transaction.largest_confirmed_gap_nsecs as f64 / 1_000_000.0,
                "buffered gap-aware frames for atomic decode recovery commit"
            );
        }
        if transaction.staging_queue.len() >= staging_frame_budget
            && !transaction.staging_budget_reached
        {
            transaction.staging_budget_reached = true;
            tracing::warn!(
                session_id = ?session_id,
                transaction_id = transaction.transaction_id,
                phase = ?transaction.phase,
                staging_frames = transaction.staging_queue.len(),
                staging_frame_budget,
                recovery_staged_high_water_nsecs = ?transaction
                    .staging_queue
                    .range_nsecs()
                    .map(|(_, end)| end),
                "HEVC decode recovery staging reached its hard Vulkan-frame budget"
            );
        }
        if entered_buffered {
            // mpv wakes its core whenever decoder/output state changes. Do the
            // same here because cache backpressure may otherwise prevent the
            // coordinator from revisiting the output gate.
            self.note_output_housekeeping_change();
        }
        true
    }

    pub(in crate::player::backend::ffmpeg) fn check_decode_recovery_deadline(
        &mut self,
        now: Instant,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<(), String> {
        let drained_decoder_error_boundary = self.decode_recovery_drained_boundary_ready();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return Ok(());
        };
        if transaction.phase.terminal()
            || now.saturating_duration_since(transaction.started_at) < DECODE_RECOVERY_MAX_WALL_TIME
        {
            return Ok(());
        }
        if drained_decoder_error_boundary {
            // The transaction is already commit-ready. Let the output service
            // enter its barrier and commit below instead of turning a single
            // scheduling race at the wall-time boundary into a fatal stall.
            return Ok(());
        }

        transaction.phase = DecodeRecoveryPhase::Failed;
        let transaction_id = transaction.transaction_id;
        let target_nsecs = transaction.target_nsecs;
        let resume_nsecs = transaction.resume_nsecs;
        let disposition = transaction.disposition;
        let source = transaction.source;
        let gap_provenance = transaction.gap_provenance;
        let elapsed = now.saturating_duration_since(transaction.started_at);
        let staging_frames = transaction.staging_queue.len();
        let first_staged_frame_nsecs = transaction.first_staged_frame_nsecs;
        let last_staged_frame_nsecs = transaction.last_staged_frame_nsecs;
        let rejected_frame_count = transaction.rejected_frame_count;
        let first_rejected_frame_nsecs = transaction.first_rejected_frame_nsecs;
        let last_rejected_frame_nsecs = transaction.last_rejected_frame_nsecs;
        transaction.staging_queue.clear();
        control.set_output_rebuffer_paused(false);
        tracing::error!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            resume_nsecs,
            disposition = ?disposition,
            recovery_source = source.as_str(),
            gap_provenance = ?gap_provenance,
            elapsed_ms = elapsed.as_secs_f64() * 1_000.0,
            staging_frames,
            first_staged_frame_nsecs,
            last_staged_frame_nsecs,
            rejected_frame_count,
            first_rejected_frame_nsecs,
            last_rejected_frame_nsecs,
            "HEVC decode recovery exceeded bounded wall-time deadline"
        );
        Err(format!(
            "HEVC 解码恢复事务 {transaction_id} 在 {:.0}ms 内未形成可提交输出（目标 {:.3}s，策略 {:?}）",
            elapsed.as_secs_f64() * 1_000.0,
            target_nsecs as f64 / 1_000_000_000.0,
            disposition,
        ))
    }

    pub(in crate::player::backend::ffmpeg) fn commit_decode_recovery_if_buffered(
        &mut self,
        control: &FfmpegControl,
        scheduler: &mut PlaybackScheduler,
        audio_output: Option<&AudioOutput>,
        current_start_position_nsecs: &mut u64,
        audio_ready: bool,
        session_id: PlaybackSessionId,
    ) -> bool {
        let Some(transaction) = self.decode_recovery_transaction.as_ref() else {
            return false;
        };
        if transaction.phase != DecodeRecoveryPhase::Buffered {
            return false;
        }
        if transaction.source == DecodeRecoverySource::DecoderError
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor
            && transaction.barrier_started_at.is_none()
        {
            // Preserve already-decoded good frames until playback reaches the
            // exact broken-chain boundary. Reanchoring earlier would trade the
            // visible gap for an even larger, premature content skip.
            return false;
        }
        if transaction.barrier_started_at.is_some()
            && !audio_ready
            && transaction.disposition != DecodeRecoveryDisposition::Reanchor
        {
            return false;
        }

        let transaction_id = transaction.transaction_id;
        let target_nsecs = transaction.target_nsecs;
        let resume_nsecs = transaction.resume_nsecs;
        let disposition = transaction.disposition;
        let source = transaction.source;
        let gap_provenance = transaction.gap_provenance;
        let barrier_elapsed = transaction
            .barrier_started_at
            .map(|started_at| started_at.elapsed());
        let first_staged_frame_nsecs = transaction.first_staged_frame_nsecs;
        let last_staged_frame_nsecs = transaction.last_staged_frame_nsecs;
        let bridged_gap_count = transaction.bridged_gap_count;
        let largest_confirmed_gap_nsecs = transaction.largest_confirmed_gap_nsecs;
        let rejected_frame_count = transaction.rejected_frame_count;
        let first_rejected_frame_nsecs = transaction.first_rejected_frame_nsecs;
        let last_rejected_frame_nsecs = transaction.last_rejected_frame_nsecs;
        let recovery_elapsed = transaction.started_at.elapsed();

        let mut extended_visible_frame = false;
        let mut discarded_retained_video_frames = 0usize;
        if disposition == DecodeRecoveryDisposition::HoldGap {
            extended_visible_frame = first_staged_frame_nsecs.is_some_and(|first_frame_nsecs| {
                self.scheduled_video_queue
                    .extend_back_duration_to(first_frame_nsecs)
                    .is_some()
            });
        } else if disposition == DecodeRecoveryDisposition::Reanchor {
            discarded_retained_video_frames = self.scheduled_video_queue.len();
            self.scheduled_video_queue.clear();
        }

        let appended_frames = {
            let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
                return false;
            };
            self.scheduled_video_queue
                .append_from(&mut transaction.staging_queue)
        };

        let terminal_phase = match disposition {
            DecodeRecoveryDisposition::Exact => DecodeRecoveryPhase::Committed,
            DecodeRecoveryDisposition::HoldGap => DecodeRecoveryPhase::CommittedGap,
            DecodeRecoveryDisposition::Reanchor => DecodeRecoveryPhase::Reanchored,
            DecodeRecoveryDisposition::DropForFallback => DecodeRecoveryPhase::DroppedForFallback,
        };
        if let Some(transaction) = self.decode_recovery_transaction.as_mut() {
            transaction.phase = terminal_phase;
        }

        let mut dropped_pending_audio_frames = 0usize;
        if disposition == DecodeRecoveryDisposition::Reanchor {
            dropped_pending_audio_frames = self.pending_start_audio.len();
            self.pending_start_audio.clear();
            self.set_audio_sync_drop_before_timeline_nsecs(
                resume_nsecs,
                session_id,
                "decode_recovery_large_discontinuity",
            );
            if let Some(audio_output) = audio_output {
                audio_output.reset_clock(resume_nsecs);
            }
            scheduler.reset(resume_nsecs);
            self.mark_video_clock_anchor_valid();
            *current_start_position_nsecs = resume_nsecs;
        } else if let Some(elapsed) = barrier_elapsed {
            scheduler.delay_by(elapsed);
            self.mark_video_clock_anchor_valid();
        }

        if barrier_elapsed.is_some()
            || disposition == DecodeRecoveryDisposition::Reanchor
            || control.is_output_rebuffer_paused()
        {
            control.set_output_rebuffer_paused(false);
            self.video_output_rebuffer_anchor = None;
            self.video_output_underrun_started_at = None;
            self.rebuffer_started_at = None;
            self.video_decode_underfill = false;
            self.clear_video_bootstrap_after_seek("decode_recovery_commit");
            self.set_state(PlaybackOutputState::Playing);
        }
        tracing::warn!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            resume_nsecs,
            disposition = ?disposition,
            recovery_source = source.as_str(),
            gap_provenance = ?gap_provenance,
            terminal_phase = ?terminal_phase,
            appended_frames,
            barrier_elapsed_ms = ?barrier_elapsed.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            recovery_elapsed_ms = recovery_elapsed.as_secs_f64() * 1_000.0,
            first_staged_frame_nsecs,
            last_staged_frame_nsecs,
            bridged_gap_count,
            largest_confirmed_gap_ms = largest_confirmed_gap_nsecs as f64 / 1_000_000.0,
            extended_visible_frame,
            discarded_retained_video_frames,
            dropped_pending_audio_frames,
            rejected_frame_count,
            first_rejected_frame_nsecs,
            last_rejected_frame_nsecs,
            committed_video_range = ?self.scheduled_video_queue.range_nsecs(),
            "atomically committed frames for HEVC decode recovery"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg) fn maybe_enter_decode_recovery_barrier(
        &mut self,
        now: Instant,
        media_clock_nsecs: u64,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
    ) -> bool {
        let retained_video_queue_empty = self.scheduled_video_queue.is_empty();
        let Some(transaction) = self.decode_recovery_transaction.as_mut() else {
            return false;
        };
        let buffered_decoder_reanchor = transaction.phase == DecodeRecoveryPhase::Buffered
            && transaction.source == DecodeRecoverySource::DecoderError
            && transaction.disposition == DecodeRecoveryDisposition::Reanchor;
        let retained_boundary_drained = buffered_decoder_reanchor && retained_video_queue_empty;
        if (transaction.phase == DecodeRecoveryPhase::Buffered && !buffered_decoder_reanchor)
            || transaction.phase.terminal()
            || transaction.barrier_started_at.is_some()
            || (media_clock_nsecs < transaction.target_nsecs && !retained_boundary_drained)
        {
            return false;
        }
        transaction.barrier_started_at = Some(now);
        let target_nsecs = transaction.target_nsecs;
        let transaction_id = transaction.transaction_id;
        self.set_state(PlaybackOutputState::Rebuffering);
        self.rebuffer_started_at = Some(now);
        self.video_output_rebuffer_anchor = Some(RebufferResumeAnchor {
            timeline_nsecs: target_nsecs,
            reset_to_video_when_decoded_queue_misses_anchor: false,
        });
        control.set_output_rebuffer_paused(true);
        tracing::warn!(
            session_id = ?session_id,
            transaction_id,
            target_nsecs,
            media_clock_nsecs,
            retained_video_queue_empty,
            boundary_reason = if retained_boundary_drained {
                "retained_video_queue_drained"
            } else {
                "media_clock"
            },
            "entered dedicated decode recovery barrier at gap boundary"
        );
        true
    }
}
