use super::audio_decode_worker::AudioDecodePacketResult;
use super::decode::{DecodeInputRetryStatus, DecodePacketAdmissionStatus};
use super::decoded_audio_frame::process_audio_decode_drain_result;
use super::drain_phase::{PlaybackDrainPhase, PlaybackDrainResults};
use super::playback_block::{
    video_decode_block_reason_with_output_queue, video_output_resource_pressure,
};
use super::video_decode_drain_frame_processor::{
    VideoDecodeDrainFrameProcessor, VideoDecodeDrainProcessStatus,
};
use super::video_decode_pipeline::{VideoPacketAdmissionContext, VideoPacketAdmissionPressure};
use super::video_decode_worker::{VideoDecodeDrainResult, VideoDecodeWorkerSnapshot};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputStreamState {
    pub(super) stream_index: c_int,
    pub(super) packet_input_blocked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecoderInputSnapshot {
    pub(super) demux_streams: Vec<c_int>,
    pub(super) video_stream_index: c_int,
    pub(super) audio_stream_index: Option<c_int>,
    pub(super) subtitle_stream_index: Option<c_int>,
    pub(super) video_decode_snapshot: VideoDecodeWorkerSnapshot,
    pub(super) video_decode_blocked_on: Option<PlaybackBlockReason>,
}

pub(super) struct PlaybackPipelineState {
    pub(super) video_stream: StreamInfo,
    pub(super) video_frame_duration_nsecs: u64,
    pub(super) video_decode_pipeline: VideoDecodePipeline,
    pub(super) audio_decode_pipeline: Option<AudioDecodePipeline>,
    pub(super) subtitle_pipeline: SubtitlePipeline,
    pub(super) video_decode_recovery: VideoDecodeRecovery,
    pub(super) playback_generation: PlaybackGeneration,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) decoded_video_frame_count: u64,
    pub(super) dropped_video_frames_before_start_count: u64,
    pub(super) dropped_audio_frames_before_start_count: u64,
    pub(super) video_clock: TimestampMapper,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
    pub(super) audio_clock: TimestampMapper,
    pub(super) audio_output: Option<AudioOutput>,
    pub(super) scheduler: PlaybackScheduler,
    pub(super) output_scheduler: PlaybackOutputScheduler,
    pub(super) dovi_pipeline: DoviPipeline,
    pub(super) buffered_reporter: BufferedReporter,
    pub(super) position_reporter: PositionReporter,
    pub(super) video_frame_prepare_worker: VideoFramePrepareWorker,
    pub(super) current_start_position_nsecs: u64,
    pub(super) video_packet_count: u64,
    pub(super) video_decode_skip_nonref_active: bool,
}

impl PlaybackPipelineState {
    pub(super) fn advance_playback_generation(&mut self) -> u64 {
        self.playback_generation.advance()
    }

    pub(super) fn flush_playback_generation(
        &mut self,
        generation: u64,
    ) -> std::result::Result<(), String> {
        self.video_frame_prepare_worker.flush_generation(generation);
        self.video_decode_pipeline.flush_buffers(generation)?;
        if let Some(worker) = self.audio_decode_pipeline.as_mut() {
            worker.flush_buffers(generation)?;
        }
        self.subtitle_pipeline.flush_decode_state(generation)?;
        Ok(())
    }

    pub(super) fn decoder_outputs_pending_or_in_flight(&self) -> bool {
        self.video_decode_pipeline.has_pending_or_in_flight()
            || self
                .audio_decode_pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.has_pending_or_in_flight())
            || self.subtitle_pipeline.has_pending_or_in_flight()
    }

    pub(super) fn start_decoder_drain_phase(
        &mut self,
    ) -> std::result::Result<PlaybackDrainPhase, String> {
        PlaybackDrainPhase::start(
            &mut self.playback_generation,
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn poll_decoder_drain_phase(
        &mut self,
        drain_phase: &mut PlaybackDrainPhase,
    ) -> std::result::Result<Option<PlaybackDrainResults>, String> {
        drain_phase.poll(
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(super) fn video_drain_frame_processor(
        &mut self,
        video_drain_result: VideoDecodeDrainResult,
    ) -> VideoDecodeDrainFrameProcessor {
        let video_prepare_generation = self.playback_generation.advance();
        VideoDecodeDrainFrameProcessor::new(
            video_drain_result,
            video_prepare_generation,
            self.decoded_video_frame_count,
        )
    }

    pub(super) fn poll_video_drain_processor(
        &mut self,
        processor: &mut VideoDecodeDrainFrameProcessor,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<VideoDecodeDrainProcessStatus, String> {
        processor.poll(
            &self.video_decode_pipeline,
            self.video_frame_duration_nsecs,
            &mut self.video_clock,
            &mut self.playback_timeline_origin_nsecs,
            &mut self.subtitle_pipeline,
            &mut self.current_start_position_nsecs,
            &mut self.dovi_pipeline,
            self.audio_output.as_ref(),
            &mut self.output_scheduler,
            vo_queue,
            &mut self.video_frame_prepare_worker,
            control,
            session_id,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.buffered_reporter,
            &mut self.scheduler,
        )
    }

    pub(super) fn process_audio_drain_result(
        &mut self,
        audio_drain_result: AudioDecodePacketResult,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<(), String> {
        let audio_time_base = self
            .audio_decode_pipeline
            .as_ref()
            .map(|worker| worker.info().time_base);
        process_audio_decode_drain_result(
            audio_drain_result,
            audio_time_base,
            control,
            self.audio_output.as_ref(),
            &mut self.audio_clock,
            self.current_start_position_nsecs,
            &mut self.dropped_audio_frames_before_start_count,
            &mut self.output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            &mut self.position_reporter,
            event_tx,
            &mut self.subtitle_pipeline,
            &mut self.buffered_reporter,
        )
    }

    pub(super) fn retry_pending_decoder_inputs(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let video_retry_status = self.video_decode_pipeline.retry_pending_input(session_id)?;

        let audio_retry_status = if self.audio_input_suppressed_until_output_resume() {
            None
        } else {
            self.audio_decode_pipeline
                .as_mut()
                .map(|worker| worker.retry_pending_input(session_id))
                .transpose()?
        };

        let subtitle_retry_status = self.subtitle_pipeline.retry_pending_input(
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )?;

        Ok(decoder_input_retry_status_from_streams([
            Some(video_retry_status),
            audio_retry_status,
            Some(subtitle_retry_status),
        ]))
    }

    pub(super) fn video_decode_stream_index(&self) -> c_int {
        self.video_decode_pipeline.info().stream_index
    }

    pub(super) fn decoder_input_snapshot(&self) -> DecoderInputSnapshot {
        let video_decode_snapshot = self.video_decode_pipeline.snapshot();
        let scheduled_video_queue_limit_reached = self
            .output_scheduler
            .scheduled_video_queue_limit_reached(self.subtitle_pipeline.needs_prefetch());
        let video_decode_blocked_on = video_decode_block_reason_with_output_queue(
            VideoDecodePipeline::block_reason_for(
                video_decode_snapshot,
                self.video_decode_pipeline.info(),
            ),
            video_output_resource_pressure(
                self.output_scheduler.scheduled_video_queue_len(),
                video_decode_snapshot.queued_frames,
                video_decode_snapshot.in_flight_packets,
                self.video_decode_pipeline.info().hardware_accelerated,
                scheduled_video_queue_limit_reached,
                self.output_scheduler.output_fill_phase(),
            ),
        );
        let video_stream_index = self.video_decode_stream_index();
        let audio_input_suppressed = self.audio_input_suppressed_until_output_resume();
        let audio_stream = self.audio_decode_pipeline.as_ref().map(|pipeline| {
            let audio_decode_snapshot = pipeline.snapshot();
            DecoderInputStreamState {
                stream_index: pipeline.info().stream_index,
                packet_input_blocked: audio_input_suppressed
                    || decoder_block_reason_blocks_packet_input(
                        AudioDecodePipeline::block_reason_for(audio_decode_snapshot),
                    ),
            }
        });
        let subtitle_stream = self.subtitle_pipeline.stream_index().map(|stream_index| {
            let subtitle_decode_blocked_on = self
                .subtitle_pipeline
                .snapshot()
                .and_then(SubtitlePipeline::block_reason_for);
            DecoderInputStreamState {
                stream_index,
                packet_input_blocked: decoder_block_reason_blocks_packet_input(
                    subtitle_decode_blocked_on,
                ),
            }
        });

        DecoderInputSnapshot {
            demux_streams: decoder_input_streams_for_state(
                DecoderInputStreamState {
                    stream_index: video_stream_index,
                    packet_input_blocked: decoder_block_reason_blocks_packet_input(
                        video_decode_blocked_on,
                    ),
                },
                audio_stream,
                subtitle_stream,
            ),
            video_stream_index,
            audio_stream_index: audio_stream.map(|stream| stream.stream_index),
            subtitle_stream_index: subtitle_stream.map(|stream| stream.stream_index),
            video_decode_snapshot,
            video_decode_blocked_on,
        }
    }

    fn audio_input_suppressed_until_output_resume(&self) -> bool {
        audio_input_suppressed_until_output_resume_state(
            self.audio_decode_pipeline.is_some(),
            self.output_scheduler.rebuffering(),
            self.output_scheduler.first_video_frame_pending,
            self.output_scheduler
                .pending_start_audio
                .buffered_duration(),
        )
    }

    pub(super) fn video_packet_admission_pressure(
        &self,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
    ) -> VideoPacketAdmissionPressure {
        VideoPacketAdmissionPressure {
            output_snapshot: self
                .output_scheduler
                .snapshot_for_played_until(played_until_nsecs),
            skip_nonref_for_pressure: self.output_scheduler.video_decode_skip_nonref_for_pressure(
                played_until_nsecs,
                has_audio_output,
                self.video_decode_skip_nonref_active,
            ),
            played_until_nsecs,
        }
    }

    pub(super) fn admit_video_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
        pressure: VideoPacketAdmissionPressure,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.video_decode_pipeline.admit_demux_packet(
            packet,
            &mut self.video_packet_count,
            &mut self.playback_generation,
            &mut self.video_decode_recovery,
            &mut self.dovi_pipeline,
            &mut self.video_decode_skip_nonref_active,
            VideoPacketAdmissionContext {
                session_id,
                video_stream: self.video_stream,
                output_snapshot: pressure.output_snapshot,
                skip_nonref_for_pressure: pressure.skip_nonref_for_pressure,
                played_until_nsecs: pressure.played_until_nsecs,
            },
        )
    }

    pub(super) fn admit_audio_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        if let Some(pipeline) = self.audio_decode_pipeline.as_mut() {
            pipeline.admit_demux_packet(packet, &mut self.playback_generation, session_id)
        } else {
            Ok(DecodePacketAdmissionStatus::Dropped)
        }
    }

    pub(super) fn admit_subtitle_demux_packet(
        &mut self,
        packet: &AvPacket,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodePacketAdmissionStatus, String> {
        self.subtitle_pipeline.admit_demux_packet(
            packet,
            &mut self.playback_generation,
            SubtitleDecodeContext {
                current_start_position_nsecs: self.current_start_position_nsecs,
                playback_timeline_origin_nsecs: self.playback_timeline_origin_nsecs,
            },
            session_id,
        )
    }
}

fn decoder_input_retry_status_from_streams(
    statuses: impl IntoIterator<Item = Option<DecodeInputRetryStatus>>,
) -> DecodeInputRetryStatus {
    let mut made_progress = false;
    let mut backpressured = false;
    for status in statuses.into_iter().flatten() {
        made_progress |= status.made_progress();
        backpressured |= status.backpressured();
    }
    if backpressured {
        DecodeInputRetryStatus::Backpressured
    } else if made_progress {
        DecodeInputRetryStatus::Queued
    } else {
        DecodeInputRetryStatus::Idle
    }
}

fn decoder_block_reason_blocks_packet_input(blocked_on: Option<PlaybackBlockReason>) -> bool {
    matches!(
        blocked_on,
        Some(
            PlaybackBlockReason::PacketQueueFull
                | PlaybackBlockReason::DecodedVideoQueue
                | PlaybackBlockReason::DecodedQueueFull
                | PlaybackBlockReason::HwSurfacePool
        )
    )
}

fn audio_input_suppressed_until_output_resume_state(
    has_audio_decode_pipeline: bool,
    output_rebuffering: bool,
    first_video_frame_pending: bool,
    pending_start_audio_duration: Duration,
) -> bool {
    // Total pending-start audio duration is not the same as audio coverage from
    // the eventual resume timeline. During seeks/track switches, preroll can
    // leave a gap before the first queued video frame; keep feeding audio until
    // the pending-start queue reaches its real backpressure limit.
    has_audio_decode_pipeline
        && (output_rebuffering || first_video_frame_pending)
        && pending_start_audio_duration >= PENDING_START_AUDIO_BACKPRESSURE_DURATION
}

fn decoder_input_streams_for_state(
    video: DecoderInputStreamState,
    audio: Option<DecoderInputStreamState>,
    subtitle: Option<DecoderInputStreamState>,
) -> Vec<c_int> {
    let mut streams = Vec::with_capacity(3);
    push_decoder_input_stream_if_open(&mut streams, video);
    if let Some(audio) = audio {
        push_decoder_input_stream_if_open(&mut streams, audio);
    }
    if let Some(subtitle) = subtitle {
        push_decoder_input_stream_if_open(&mut streams, subtitle);
    }
    streams
}

fn push_decoder_input_stream_if_open(streams: &mut Vec<c_int>, stream: DecoderInputStreamState) {
    if !stream.packet_input_blocked && !streams.contains(&stream.stream_index) {
        streams.push(stream.stream_index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(stream_index: c_int, packet_input_blocked: bool) -> DecoderInputStreamState {
        DecoderInputStreamState {
            stream_index,
            packet_input_blocked,
        }
    }

    #[test]
    fn decoder_input_streams_skip_only_backpressured_streams() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, true),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![11, 12]
        );
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, true)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_deduplicate_shared_stream_indices() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(10, false)),
                Some(stream(12, false))
            ),
            vec![10, 12]
        );
    }

    #[test]
    fn decoder_input_streams_allow_all_streams_until_their_decoder_queue_is_full() {
        assert_eq!(
            decoder_input_streams_for_state(
                stream(10, false),
                Some(stream(11, false)),
                Some(stream(12, false))
            ),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn audio_input_suppression_waits_until_pending_start_audio_backpressure() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            false,
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            false,
            true,
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION + Duration::from_millis(1)
        ));
        assert!(audio_input_suppressed_until_output_resume_state(
            true,
            true,
            false,
            PENDING_START_AUDIO_BACKPRESSURE_DURATION
        ));
        assert!(audio_input_suppressed_until_output_resume_state(
            true,
            false,
            true,
            PENDING_START_AUDIO_BACKPRESSURE_DURATION + Duration::from_millis(1)
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            true,
            false,
            PENDING_START_AUDIO_BACKPRESSURE_DURATION - Duration::from_millis(1)
        ));
    }

    #[test]
    fn audio_input_suppression_only_applies_while_output_waits_for_video() {
        assert!(!audio_input_suppressed_until_output_resume_state(
            false,
            true,
            false,
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
        ));
        assert!(!audio_input_suppressed_until_output_resume_state(
            true,
            false,
            false,
            VIDEO_OUTPUT_REBUFFER_RESUME_DURATION
        ));
    }

    #[test]
    fn decoder_block_reason_blocks_only_packet_input_pressure() {
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::PacketQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedVideoQueue
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecodedQueueFull
        )));
        assert!(decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::HwSurfacePool
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::DecoderInputEmpty
        )));
        assert!(!decoder_block_reason_blocks_packet_input(Some(
            PlaybackBlockReason::RenderWorker
        )));
        assert!(!decoder_block_reason_blocks_packet_input(None));
    }

    #[test]
    fn decoder_input_retry_status_keeps_backpressure_after_other_stream_progress() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Backpressured),
                Some(DecodeInputRetryStatus::Queued),
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Backpressured
        );
    }

    #[test]
    fn decoder_input_retry_status_reports_progress_without_backpressure() {
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                Some(DecodeInputRetryStatus::Queued),
                None,
            ]),
            DecodeInputRetryStatus::Queued
        );
        assert_eq!(
            decoder_input_retry_status_from_streams([
                Some(DecodeInputRetryStatus::Idle),
                None,
                Some(DecodeInputRetryStatus::Idle),
            ]),
            DecodeInputRetryStatus::Idle
        );
    }
}
