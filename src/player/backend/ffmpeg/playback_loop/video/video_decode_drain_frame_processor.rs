use std::{
    collections::VecDeque,
    sync::{atomic::AtomicBool, mpsc::Sender},
};

use crate::player::{
    backend::BackendEvent,
    render_host::{PlaybackSessionId, VideoOutputQueue},
};

use super::decoded_video_frame::log_prepared_video_frame_if_slow;
use super::video_decode_pipeline::VideoDecodePipeline;
use super::video_decode_worker::{VideoDecodeDrainResult, VideoDecodedFrame};
use super::video_frame_admission_service::{
    DrainedPreparedVideoFrameAdmissionContext, admit_drained_prepared_video_frame,
};
use super::video_frame_prepare_admission_service::{
    DecodedVideoFramePrepareStatus, DecodedVideoFrameStartStatus,
    enqueue_decoded_video_frame_prepare, service_drained_video_frame_start,
};
use super::video_frame_prepare_worker::{
    VideoFramePrepareDiagnosticContext, VideoFramePrepareEnqueueResult, VideoFramePrepareWorker,
};
use super::{
    AudioOutput, BufferedReporter, CORRUPT_VIDEO_FRAME_RECOVERY_ERROR, DoviPipeline, FfmpegControl,
    PlaybackOutputScheduler, PlaybackScheduler, PositionReporter, SubtitlePipeline,
    TimestampMapper,
};

pub(super) enum VideoDecodeDrainProcessStatus {
    Pending { made_progress: bool },
    Complete,
}

pub(super) struct VideoDecodeDrainFrameProcessor {
    generation: u64,
    frames: VecDeque<VideoDecodedFrame>,
    pending_prepares: usize,
    decode_result: Option<std::result::Result<(), String>>,
    decoded_video_frame_count: u64,
}

impl VideoDecodeDrainFrameProcessor {
    pub(super) fn new(
        video_drain_result: VideoDecodeDrainResult,
        generation: u64,
        decoded_video_frame_count: u64,
    ) -> Self {
        let VideoDecodeDrainResult { frames, result } = video_drain_result;
        Self {
            generation,
            frames: frames.into(),
            pending_prepares: 0,
            decode_result: Some(result),
            decoded_video_frame_count,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn poll(
        &mut self,
        video_decode_pipeline: &VideoDecodePipeline,
        video_frame_duration_nsecs: u64,
        video_clock: &mut TimestampMapper,
        playback_timeline_origin_nsecs: &mut Option<u64>,
        subtitle_pipeline: &mut SubtitlePipeline,
        current_start_position_nsecs: &mut u64,
        dovi_pipeline: &mut DoviPipeline,
        audio_output: Option<&AudioOutput>,
        output_scheduler: &mut PlaybackOutputScheduler,
        vo_queue: &VideoOutputQueue,
        video_frame_prepare_worker: &mut VideoFramePrepareWorker,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        buffered_reporter: &mut BufferedReporter,
        scheduler: &mut PlaybackScheduler,
    ) -> std::result::Result<VideoDecodeDrainProcessStatus, String> {
        let mut made_progress = false;
        made_progress |= self.retry_pending_prepare(video_frame_prepare_worker)?;
        made_progress |= self.drain_ready_prepared_frames(
            video_frame_prepare_worker,
            output_scheduler,
            audio_output,
            control,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
            scheduler,
            current_start_position_nsecs,
        )?;
        if video_frame_prepare_worker.pending_input_full() {
            return Ok(VideoDecodeDrainProcessStatus::Pending { made_progress });
        }
        made_progress |= self.enqueue_ready_decoded_frames(
            video_decode_pipeline,
            video_frame_duration_nsecs,
            video_clock,
            playback_timeline_origin_nsecs,
            subtitle_pipeline,
            current_start_position_nsecs,
            dovi_pipeline,
            output_scheduler,
            video_frame_prepare_worker,
            session_id,
        )?;
        made_progress |= self.drain_ready_prepared_frames(
            video_frame_prepare_worker,
            output_scheduler,
            audio_output,
            control,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
            scheduler,
            current_start_position_nsecs,
        )?;

        if self.frames.is_empty() && self.pending_prepares == 0 {
            return self
                .complete()
                .map(|_| VideoDecodeDrainProcessStatus::Complete);
        }
        Ok(VideoDecodeDrainProcessStatus::Pending { made_progress })
    }

    fn retry_pending_prepare(
        &mut self,
        video_frame_prepare_worker: &mut VideoFramePrepareWorker,
    ) -> std::result::Result<bool, String> {
        if !video_frame_prepare_worker.has_pending_input() {
            return Ok(false);
        }
        Ok(video_frame_prepare_worker.retry_pending_input()?
            == VideoFramePrepareEnqueueResult::Queued)
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_ready_decoded_frames(
        &mut self,
        video_decode_pipeline: &VideoDecodePipeline,
        video_frame_duration_nsecs: u64,
        video_clock: &mut TimestampMapper,
        playback_timeline_origin_nsecs: &mut Option<u64>,
        subtitle_pipeline: &mut SubtitlePipeline,
        current_start_position_nsecs: &mut u64,
        dovi_pipeline: &mut DoviPipeline,
        output_scheduler: &PlaybackOutputScheduler,
        video_frame_prepare_worker: &mut VideoFramePrepareWorker,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<bool, String> {
        let mut made_progress = false;
        while let Some(decoded_frame) = self.frames.pop_front() {
            match self.enqueue_decoded_frame_prepare(
                decoded_frame,
                video_decode_pipeline,
                video_frame_duration_nsecs,
                video_clock,
                playback_timeline_origin_nsecs,
                subtitle_pipeline,
                current_start_position_nsecs,
                dovi_pipeline,
                output_scheduler,
                video_frame_prepare_worker,
                session_id,
            )? {
                DrainedFramePrepareAdmission::Queued => {
                    made_progress = true;
                }
                DrainedFramePrepareAdmission::Backpressured => {
                    made_progress = true;
                    break;
                }
                DrainedFramePrepareAdmission::Dropped => {
                    made_progress = true;
                }
            }
        }
        Ok(made_progress)
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_decoded_frame_prepare(
        &mut self,
        decoded_frame: VideoDecodedFrame,
        video_decode_pipeline: &VideoDecodePipeline,
        video_frame_duration_nsecs: u64,
        video_clock: &mut TimestampMapper,
        playback_timeline_origin_nsecs: &mut Option<u64>,
        subtitle_pipeline: &mut SubtitlePipeline,
        current_start_position_nsecs: &mut u64,
        dovi_pipeline: &mut DoviPipeline,
        output_scheduler: &PlaybackOutputScheduler,
        video_frame_prepare_worker: &mut VideoFramePrepareWorker,
        session_id: PlaybackSessionId,
    ) -> std::result::Result<DrainedFramePrepareAdmission, String> {
        let frame = decoded_frame.as_mut_ptr();
        let start_frame = match service_drained_video_frame_start(
            frame,
            video_decode_pipeline.info().time_base,
            video_clock,
            playback_timeline_origin_nsecs,
            subtitle_pipeline,
            *current_start_position_nsecs,
            dovi_pipeline,
        ) {
            DecodedVideoFrameStartStatus::Ready(frame) => frame,
            DecodedVideoFrameStartStatus::DroppedBeforeStart
            | DecodedVideoFrameStartStatus::SeekPrerollBeforeStart(_)
            | DecodedVideoFrameStartStatus::DroppedCorrupt => {
                return Ok(DrainedFramePrepareAdmission::Dropped);
            }
        };
        let frame_pts = start_frame.frame_pts;
        let timeline_nsecs = start_frame.timeline_nsecs;
        let diagnostic = VideoFramePrepareDiagnosticContext::from_output_snapshot(
            session_id,
            self.decoded_video_frame_count,
            false,
            "eof_drain",
            output_scheduler.snapshot(),
        );
        match enqueue_decoded_video_frame_prepare(
            decoded_frame,
            self.generation,
            diagnostic,
            frame_pts,
            timeline_nsecs,
            video_frame_duration_nsecs,
            dovi_pipeline,
            video_frame_prepare_worker,
            &video_decode_pipeline.info().convert_context,
        )? {
            DecodedVideoFramePrepareStatus::Queued => {
                self.pending_prepares = self.pending_prepares.saturating_add(1);
                Ok(DrainedFramePrepareAdmission::Queued)
            }
            DecodedVideoFramePrepareStatus::Backpressured => {
                self.pending_prepares = self.pending_prepares.saturating_add(1);
                Ok(DrainedFramePrepareAdmission::Backpressured)
            }
            DecodedVideoFramePrepareStatus::DroppedCorrupt => {
                Err(CORRUPT_VIDEO_FRAME_RECOVERY_ERROR.to_string())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn drain_ready_prepared_frames(
        &mut self,
        video_frame_prepare_worker: &mut VideoFramePrepareWorker,
        output_scheduler: &mut PlaybackOutputScheduler,
        audio_output: Option<&AudioOutput>,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        subtitle_pipeline: &mut SubtitlePipeline,
        buffered_reporter: &mut BufferedReporter,
        scheduler: &mut PlaybackScheduler,
        current_start_position_nsecs: &mut u64,
    ) -> std::result::Result<bool, String> {
        let mut made_progress = false;
        while let Some(result) = video_frame_prepare_worker.poll_result(self.generation)? {
            log_prepared_video_frame_if_slow(&result, session_id, output_scheduler, audio_output);
            self.pending_prepares = self.pending_prepares.saturating_sub(1);
            let prepared_frame = result.result?;
            admit_drained_prepared_video_frame(DrainedPreparedVideoFrameAdmissionContext {
                prepared_frame,
                decoded_video_frame_count: self.decoded_video_frame_count,
                scheduler,
                audio_output,
                output_scheduler,
                buffered_reporter,
                control,
                session_id,
                event_tx,
                vo_queue,
                frame_presented,
                position_reporter,
                subtitle_pipeline,
                current_start_position_nsecs,
            })?;
            made_progress = true;
        }
        Ok(made_progress)
    }

    fn complete(&mut self) -> std::result::Result<(), String> {
        self.decode_result
            .take()
            .expect("video drain decode result is present until completion")
    }
}

enum DrainedFramePrepareAdmission {
    Queued,
    Backpressured,
    Dropped,
}
