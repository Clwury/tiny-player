use super::*;

impl VideoDecodePipeline {
    pub(in super::super) fn spawn(decoder: Decoder) -> std::result::Result<Self, String> {
        let requested_hardware_mode = decoder.hardware_decode_mode();
        let frame_drop = VideoDecodeFrameDrop::default();
        Ok(Self {
            worker: VideoDecodeWorker::spawn(decoder, frame_drop.epoch_handle())?,
            frame_drop,
            requested_hardware_mode,
            decoder_epoch: 1,
            admitted_video_sequence: 0,
            last_admitted_decoder_epoch: None,
            packets: VideoDecodePacketQueues::default(),
            hevc_hw_replay: VecDeque::new(),
            hevc_decode_chain_watchdog: HevcDecodeChainWatchdog::default(),
            hevc_decode_packet_diagnostics: HevcDecodePacketDiagnosticWindow::default(),
            hevc_hw_replay_journal: HevcHwReplayJournal::default(),
            hevc_same_hardware_recovery: None,
            last_hevc_decode_error: None,
            last_hevc_decode_chain_fallback: None,
            hevc_low_level_seek_observation: None,
            last_hevc_cra_low_level_landing: None,
        })
    }

    pub(in super::super) fn info(&self) -> &VideoDecodeWorkerInfo {
        self.worker.info()
    }

    pub(in super::super) fn decoder_epoch(&self) -> u64 {
        self.decoder_epoch
    }

    pub(in super::super) fn admitted_video_sequence(&self) -> u64 {
        self.admitted_video_sequence
    }

    pub(in super::super) fn last_admitted_decoder_epoch(&self) -> Option<u64> {
        self.last_admitted_decoder_epoch
    }

    pub(in super::super) fn snapshot(&self) -> VideoDecodeWorkerSnapshot {
        let mut snapshot = self.worker.snapshot();
        snapshot.decoder_framedrop_enabled = self.frame_drop.enabled();
        snapshot.decoder_drop_budget = self.frame_drop.remaining_budget();
        snapshot.decoder_drop_stats = self.frame_drop.stats();
        let (pending_input_packets, pending_input_capacity) = video_decode_pending_input_snapshot(
            self.packets.pending_input_count(),
            self.hevc_hw_replay.len(),
        );
        snapshot.pending_input_packets = pending_input_packets;
        snapshot.pending_input_capacity = pending_input_capacity;
        snapshot.oldest_submitted_packet_nsecs = self.packets.front_packet().and_then(|packet| {
            packet
                .read_diagnostic()
                .and_then(|diagnostic| diagnostic.packet_start_nsecs)
                .or_else(|| {
                    packet
                        .best_timestamp()
                        .and_then(|timestamp| timestamp_to_nsecs(timestamp, self.info().time_base))
                })
        });
        snapshot
    }

    pub(in super::super) fn block_reason_for(
        snapshot: VideoDecodeWorkerSnapshot,
        info: &VideoDecodeWorkerInfo,
    ) -> Option<PlaybackBlockReason> {
        match snapshot.state {
            VideoDecodeWorkerState::OutputFull if info.hardware_accelerated => {
                Some(PlaybackBlockReason::HwSurfacePool)
            }
            VideoDecodeWorkerState::OutputFull => Some(PlaybackBlockReason::DecodedQueueFull),
            _ if snapshot.pending_input_full() => Some(PlaybackBlockReason::PacketQueueFull),
            _ if snapshot.completed_packets > 0
                && snapshot.submitted_not_consumed_packets >= snapshot.command_queue_capacity =>
            {
                Some(PlaybackBlockReason::DecoderOutputPending)
            }
            _ if snapshot.submitted_not_consumed_packets >= snapshot.command_queue_capacity => {
                Some(PlaybackBlockReason::DecoderInFlight)
            }
            VideoDecodeWorkerState::NeedPacket if snapshot.pending_input_packets == 0 => {
                Some(PlaybackBlockReason::DecoderInputEmpty)
            }
            _ => None,
        }
    }

    pub(in super::super) fn set_skip_nonref_frames(
        &mut self,
        enabled: bool,
    ) -> std::result::Result<(), String> {
        self.worker.set_skip_nonref_frames(enabled)
    }

    pub(in super::super) fn set_decoder_framedrop(&mut self, enabled: bool) {
        if self.frame_drop.enabled() != enabled {
            tracing::debug!(
                decoder_framedrop_enabled = enabled,
                "updated ordinary-playback decoder frame-drop policy"
            );
        }
        self.frame_drop.set_enabled(enabled);
    }

    pub(in super::super) fn decoder_framedrop_enabled(&self) -> bool {
        self.frame_drop.enabled()
    }

    pub(in super::super) fn try_enqueue_packet(
        &mut self,
        packet: &AvPacket,
        generation: u64,
        drop_policy: VideoDecodeDropPolicy,
    ) -> std::result::Result<VideoDecodeEnqueueResult, String> {
        self.worker
            .try_enqueue_packet(packet, generation, drop_policy)
    }

    pub(in super::super) fn try_enqueue_pending_packet(
        &mut self,
        mut pending_packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if self.packets.has_pending_input() || !self.hevc_hw_replay.is_empty() {
            return Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id));
        }
        pending_packet.drop_policy = self
            .frame_drop
            .submission_policy(pending_packet.drop_policy);
        let enqueue_result = self.try_enqueue_packet(
            &pending_packet.packet,
            pending_packet.generation,
            pending_packet.drop_policy,
        )?;
        match enqueue_result {
            VideoDecodeEnqueueResult::Queued => {
                self.frame_drop.submitted(pending_packet.drop_policy);
                self.push_in_flight(pending_packet, session_id);
                Ok(DecodePacketAdmissionStatus::Queued)
            }
            VideoDecodeEnqueueResult::InputFull | VideoDecodeEnqueueResult::OutputFull => {
                Ok(self.buffer_pending_input_or_backpressure(pending_packet, session_id))
            }
        }
    }

    pub(in super::super) fn retry_pending_input(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let Some(mut pending_packet) = self.take_pending_input() else {
            return Ok(DecodeInputRetryStatus::Idle);
        };
        pending_packet.drop_policy = self
            .frame_drop
            .submission_policy(pending_packet.drop_policy);
        let enqueue_result = self.try_enqueue_packet(
            &pending_packet.packet,
            pending_packet.generation,
            pending_packet.drop_policy,
        )?;
        match enqueue_result {
            VideoDecodeEnqueueResult::Queued => {
                self.frame_drop.submitted(pending_packet.drop_policy);
                self.push_in_flight(pending_packet, session_id);
                Ok(DecodeInputRetryStatus::Queued)
            }
            VideoDecodeEnqueueResult::InputFull | VideoDecodeEnqueueResult::OutputFull => {
                requeue_backpressured_video_decode_input(
                    &mut self.packets,
                    &mut self.hevc_hw_replay,
                    pending_packet,
                );
                self.log_pending_input_backpressured(session_id, enqueue_result);
                Ok(DecodeInputRetryStatus::Backpressured)
            }
        }
    }

    pub(in super::super) fn requeue_hevc_hw_replay_journal(
        &mut self,
        playback_generation: &mut PlaybackGeneration,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<usize, String> {
        let required_high_water_nsecs = self
            .hevc_same_hardware_recovery
            .as_ref()
            .and_then(|transaction| transaction.replay_required_high_water_nsecs)
            .unwrap_or(target_nsecs)
            .max(target_nsecs);
        let journal_anchor_nsecs = self.hevc_hw_replay_journal.anchor_nsecs;
        let journal_high_water_nsecs = self.hevc_hw_replay_journal.high_water_nsecs;
        let journal_anchor_after_target_nsecs = journal_anchor_nsecs
            .filter(|anchor_nsecs| *anchor_nsecs > target_nsecs)
            .map(|anchor_nsecs| anchor_nsecs.saturating_sub(target_nsecs));
        let journal_packets = self.hevc_hw_replay_journal.len();
        let journal_bytes = self.hevc_hw_replay_journal.total_bytes;
        let Some(packets) = self
            .hevc_hw_replay_journal
            .clone_replayable(target_nsecs, required_high_water_nsecs)?
        else {
            tracing::warn!(
                session_id = ?session_id,
                target_nsecs,
                required_high_water_nsecs,
                journal_anchor_nsecs = ?journal_anchor_nsecs,
                journal_anchor_after_target_ms = ?journal_anchor_after_target_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                recoverable_forward_anchor_limit_ms =
                    HEVC_RECOVERABLE_DECODE_GAP_MAX_NSECS as f64 / 1_000_000.0,
                journal_high_water_nsecs = ?journal_high_water_nsecs,
                journal_packets,
                journal_bytes,
                journal_packet_limit = HEVC_HW_REPLAY_JOURNAL_MAX_PACKETS,
                journal_byte_limit = HEVC_HW_REPLAY_JOURNAL_MAX_BYTES,
                journal_duration_limit_ms =
                    HEVC_HW_REPLAY_JOURNAL_MAX_DURATION_NSECS as f64 / 1_000_000.0,
                journal_contiguous = self.hevc_hw_replay_journal.coverage_contiguous,
                journal_exhausted = self.hevc_hw_replay_journal.coverage_exhausted,
                "safe HEVC replay journal did not cover the complete recovery cutoff"
            );
            return Ok(0);
        };
        let replay = hevc_hw_replay_packets(packets, playback_generation);
        let requeued = replay.len();
        let trimmed_packets = journal_packets.saturating_sub(requeued);
        self.hevc_hw_replay.extend(replay);
        if requeued > 0 {
            tracing::debug!(
                session_id = ?session_id,
                target_nsecs,
                required_high_water_nsecs,
                journal_anchor_nsecs = ?journal_anchor_nsecs,
                journal_anchor_after_target_ms = ?journal_anchor_after_target_nsecs
                    .map(|duration| duration as f64 / 1_000_000.0),
                journal_high_water_nsecs = ?journal_high_water_nsecs,
                requeued,
                trimmed_packets,
                reorder_tail_packets = HEVC_HW_REPLAY_REORDER_TAIL_PACKETS,
                replay_pending = self.hevc_hw_replay.len(),
                "requeued safe HEVC hardware replay journal after hardware decode fallback"
            );
        }
        Ok(requeued)
    }

    pub(super) fn buffer_pending_input_or_backpressure(
        &mut self,
        pending_packet: PendingVideoDecodePacket,
        session_id: PlaybackSessionId,
    ) -> DecodePacketAdmissionStatus {
        match self.packets.push_pending_input(pending_packet) {
            Ok(()) => {
                let snapshot = self.snapshot();
                tracing::trace!(
                    session_id = ?session_id,
                    video_decode_pending_input_packets = snapshot.pending_input_packets,
                    video_decode_pending_input_capacity =
                        snapshot.pending_input_capacity,
                    video_decode_pending_input_full = snapshot.pending_input_full(),
                    video_decode_submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
                    video_decode_state = ?snapshot.state,
                    "buffered FFmpeg video packet in decoder wrapper input queue"
                );
                DecodePacketAdmissionStatus::Queued
            }
            Err(pending_packet) => {
                self.packets.push_pending_input_back(pending_packet);
                self.log_pending_input_backpressured(
                    session_id,
                    VideoDecodeEnqueueResult::InputFull,
                );
                DecodePacketAdmissionStatus::Backpressured
            }
        }
    }

    pub(super) fn log_pending_input_backpressured(
        &self,
        session_id: PlaybackSessionId,
        enqueue_result: VideoDecodeEnqueueResult,
    ) {
        let snapshot = self.snapshot();
        let blocked_on =
            Self::block_reason_for(snapshot, self.info()).unwrap_or(match enqueue_result {
                VideoDecodeEnqueueResult::InputFull => PlaybackBlockReason::PacketQueueFull,
                VideoDecodeEnqueueResult::OutputFull if self.info().hardware_accelerated => {
                    PlaybackBlockReason::HwSurfacePool
                }
                VideoDecodeEnqueueResult::OutputFull => PlaybackBlockReason::DecodedQueueFull,
                VideoDecodeEnqueueResult::Queued => PlaybackBlockReason::OutputGate,
            });
        tracing::debug!(
            session_id = ?session_id,
            blocked_on = blocked_on.as_str(),
            video_decode_state = ?snapshot.state,
            video_decode_queued_frames = snapshot.queued_frames,
            video_decode_queue_capacity = snapshot.queue_capacity,
            video_decode_pending_input_packets = snapshot.pending_input_packets,
            video_decode_pending_input_capacity = snapshot.pending_input_capacity,
            video_decode_pending_input_full = snapshot.pending_input_full(),
            video_decode_submitted_not_consumed_packets = snapshot.submitted_not_consumed_packets,
            video_decode_completed_packets = snapshot.completed_packets,
            "FFmpeg video decoder wrapper input queue backpressured"
        );
    }
}
