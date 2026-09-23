use super::*;

impl PlaybackPipelineState {
    pub(in super::super::super) fn start_decoder_drain_phase(
        &mut self,
    ) -> std::result::Result<PlaybackDrainPhase, String> {
        PlaybackDrainPhase::start(
            &mut self.playback_generation,
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(in super::super::super) fn poll_decoder_drain_phase(
        &mut self,
        drain_phase: &mut PlaybackDrainPhase,
    ) -> std::result::Result<Option<PlaybackDrainResults>, String> {
        drain_phase.poll(
            &mut self.video_decode_pipeline,
            self.audio_decode_pipeline.as_mut(),
        )
    }

    pub(in super::super::super) fn video_drain_frame_processor(
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

    pub(in super::super::super) fn poll_video_drain_processor(
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

    pub(in super::super::super) fn process_audio_drain_result(
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
            self.audio_decode_pipeline.as_mut(),
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

    pub(in super::super::super) fn retry_pending_decoder_inputs(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DecodeInputRetryStatus, String> {
        let video_retry_status = if self
            .output_scheduler
            .decode_recovery_video_admission_blocked()
            || self
                .video_decode_pipeline
                .hevc_resource_pressure_decoder_input_stopped()
        {
            None
        } else {
            Some(self.video_decode_pipeline.retry_pending_input(session_id)?)
        };

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
            video_retry_status,
            audio_retry_status,
            Some(subtitle_retry_status),
        ]))
    }

    pub(in super::super::super) fn video_decode_stream_index(&self) -> c_int {
        self.video_decode_pipeline.info().stream_index
    }
}
