use std::{
    collections::{BTreeMap, VecDeque},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::player::{
    dovi::DoviFrameMetadata,
    render_host::{DecodedFrame, FrameBufferPool, FramePts, PlaybackSessionId},
};

use super::video_decode_worker::VideoDecodedFrame;
use super::{
    DECODE_PACKET_SLOW_LOG_AFTER, PlaybackBlockReason, PlaybackOutputSnapshot, PlaybackOutputState,
    VideoFrameConvertContext, VideoFrameConverter, WORKER_CHANNEL_RECV_WAIT_LOG_AFTER,
    WORKER_CHANNEL_SEND_WAIT_LOG_AFTER,
};

const VIDEO_FRAME_PREPARE_COMMAND_QUEUE_CAPACITY: usize = 3;
const VIDEO_FRAME_PREPARE_PENDING_INPUT_QUEUE_CAPACITY: usize = 3;
const VIDEO_FRAME_PREPARE_RESULT_QUEUE_CAPACITY: usize = 3;

pub(super) struct VideoFramePrepareWorker {
    command_tx: mpsc::SyncSender<VideoFramePrepareCommand>,
    result_rx: Receiver<VideoFramePrepareResult>,
    handle: Option<JoinHandle<()>>,
    pending_inputs: VecDeque<VideoFramePrepareInput>,
    completed: VecDeque<VideoFramePrepareResult>,
    in_flight_by_generation: BTreeMap<u64, usize>,
    generation_floor: u64,
}

pub(super) struct VideoFramePrepareInput {
    pub(super) generation: u64,
    pub(super) diagnostic: VideoFramePrepareDiagnosticContext,
    pub(super) frame_diagnostic: DecodedVideoFrameDiagnostic,
    pub(super) frame: VideoDecodedFrame,
    pub(super) frame_pts: FramePts,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
    pub(super) convert_context: VideoFrameConvertContext,
    pub(super) dovi_metadata: Option<DoviFrameMetadata>,
}

pub(super) struct PreparedVideoFrame {
    pub(super) generation: u64,
    pub(super) frame: DecodedFrame,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
    pub(super) source_frame_diagnostic: DecodedVideoFrameDiagnostic,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DecodedVideoFrameDiagnostic {
    pub(super) best_effort_timestamp: i64,
    pub(super) pts: i64,
    pub(super) packet_dts: i64,
    pub(super) duration: i64,
    pub(super) flags: i32,
    pub(super) key_frame: bool,
    pub(super) corrupt: bool,
    pub(super) picture_type: i32,
    pub(super) decode_error_flags: i32,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) pixel_format: i32,
}

impl DecodedVideoFrameDiagnostic {
    pub(super) fn from_frame(frame: *mut ffmpeg_sys_next::AVFrame) -> Self {
        if frame.is_null() {
            return Self::default();
        }
        unsafe {
            Self {
                best_effort_timestamp: (*frame).best_effort_timestamp,
                pts: (*frame).pts,
                packet_dts: (*frame).pkt_dts,
                duration: (*frame).duration,
                flags: (*frame).flags,
                key_frame: (*frame).flags & ffmpeg_sys_next::AV_FRAME_FLAG_KEY != 0,
                corrupt: (*frame).flags & ffmpeg_sys_next::AV_FRAME_FLAG_CORRUPT != 0
                    || (*frame).decode_error_flags != 0,
                picture_type: (*frame).pict_type as i32,
                decode_error_flags: (*frame).decode_error_flags,
                width: (*frame).width,
                height: (*frame).height,
                pixel_format: (*frame).format,
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct VideoFramePrepareDiagnosticContext {
    pub(super) session_id: PlaybackSessionId,
    pub(super) decoded_video_frame_count: u64,
    pub(super) force_completion_log: bool,
    pub(super) force_completion_reason: &'static str,
    pub(super) output_state: PlaybackOutputState,
    pub(super) output_first_video_frame_pending: bool,
    pub(super) output_rebuffering: bool,
    pub(super) queued_video_frames: usize,
    pub(super) queued_video_range_nsecs: Option<(u64, u64)>,
    pub(super) pending_start_audio_nsecs: u64,
}

impl VideoFramePrepareDiagnosticContext {
    pub(super) fn from_output_snapshot(
        session_id: PlaybackSessionId,
        decoded_video_frame_count: u64,
        force_completion_log: bool,
        force_completion_reason: &'static str,
        output_snapshot: PlaybackOutputSnapshot,
    ) -> Self {
        Self {
            session_id,
            decoded_video_frame_count,
            force_completion_log,
            force_completion_reason,
            output_state: output_snapshot.state,
            output_first_video_frame_pending: output_snapshot.first_video_frame_pending,
            output_rebuffering: output_snapshot.rebuffering,
            queued_video_frames: output_snapshot.queued_video_frames,
            queued_video_range_nsecs: output_snapshot.queued_video_range_nsecs,
            pending_start_audio_nsecs: output_snapshot.pending_start_audio_nsecs,
        }
    }
}

pub(super) struct VideoFramePrepareResult {
    pub(super) generation: u64,
    pub(super) result: std::result::Result<PreparedVideoFrame, String>,
    pub(super) elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoFramePrepareEnqueueResult {
    Queued,
    InputFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoFramePrepareWorkerState {
    NeedFrame,
    PendingInput,
    Preparing,
    HaveFrame,
    InputFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VideoFramePrepareWorkerSnapshot {
    pub(super) state: VideoFramePrepareWorkerState,
    pub(super) pending_input_frames: usize,
    pub(super) pending_input_capacity: usize,
    pub(super) in_flight_frames: usize,
    pub(super) completed_frames: usize,
    pub(super) command_queue_capacity: usize,
}

impl VideoFramePrepareWorkerSnapshot {
    pub(super) fn pending_input_full(self) -> bool {
        self.pending_input_capacity > 0 && self.pending_input_frames >= self.pending_input_capacity
    }

    pub(super) fn block_reason(self) -> Option<PlaybackBlockReason> {
        self.pending_input_full()
            .then_some(PlaybackBlockReason::FramePrepareWorker)
    }
}

enum VideoFramePrepareCommand {
    Prepare(VideoFramePrepareInput),
    Shutdown,
}

impl VideoFramePrepareWorker {
    pub(super) fn spawn(buffer_pool: FrameBufferPool) -> std::result::Result<Self, String> {
        let (command_tx, command_rx) =
            mpsc::sync_channel(VIDEO_FRAME_PREPARE_COMMAND_QUEUE_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(VIDEO_FRAME_PREPARE_RESULT_QUEUE_CAPACITY);
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-video-frame-prepare".to_string())
            .spawn(move || run_video_frame_prepare_worker(buffer_pool, command_rx, result_tx))
            .map_err(|error| format!("创建 FFmpeg video frame prepare worker 失败：{error}"))?;

        Ok(Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            pending_inputs: VecDeque::new(),
            completed: VecDeque::new(),
            in_flight_by_generation: BTreeMap::new(),
            generation_floor: 0,
        })
    }

    pub(super) fn try_enqueue(
        &mut self,
        input: VideoFramePrepareInput,
    ) -> std::result::Result<VideoFramePrepareEnqueueResult, String> {
        self.pump_available_results()?;
        if input.generation < self.generation_floor {
            tracing::debug!(
                session_id = ?input.diagnostic.session_id,
                generation = input.generation,
                generation_floor = self.generation_floor,
                timeline_nsecs = input.timeline_nsecs,
                duration_nsecs = input.duration_nsecs,
                frame_pts_nsecs = input.frame_pts.nsecs,
                decoded_video_frame_count = input.diagnostic.decoded_video_frame_count,
                output_state = ?input.diagnostic.output_state,
                output_first_video_frame_pending =
                    input.diagnostic.output_first_video_frame_pending,
                output_rebuffering = input.diagnostic.output_rebuffering,
                queued_video_frames = input.diagnostic.queued_video_frames,
                queued_video_range = ?input.diagnostic.queued_video_range_nsecs,
                force_completion_log = input.diagnostic.force_completion_log,
                force_completion_reason = input.diagnostic.force_completion_reason,
                "dropped stale FFmpeg video frame prepare input below generation floor"
            );
            return Ok(VideoFramePrepareEnqueueResult::Queued);
        }
        if !self.pending_inputs.is_empty() {
            return Ok(self.buffer_pending_input(input));
        }
        self.try_send_or_buffer(input)
    }

    pub(super) fn retry_pending_input(
        &mut self,
    ) -> std::result::Result<VideoFramePrepareEnqueueResult, String> {
        self.pump_available_results()?;
        while let Some(input) = self.pending_inputs.pop_front() {
            match self.try_send_direct(input)? {
                VideoFramePrepareDirectEnqueueResult::Queued => {}
                VideoFramePrepareDirectEnqueueResult::InputFull(input) => {
                    self.pending_inputs.push_front(input);
                    return Ok(VideoFramePrepareEnqueueResult::InputFull);
                }
            }
        }
        Ok(VideoFramePrepareEnqueueResult::Queued)
    }

    pub(super) fn has_pending_input(&self) -> bool {
        !self.pending_inputs.is_empty()
    }

    pub(super) fn pending_input_full(&self) -> bool {
        self.pending_inputs.len() >= VIDEO_FRAME_PREPARE_PENDING_INPUT_QUEUE_CAPACITY
    }

    pub(super) fn snapshot(&self) -> VideoFramePrepareWorkerSnapshot {
        VideoFramePrepareWorkerSnapshot {
            state: self.state(),
            pending_input_frames: self.pending_inputs.len(),
            pending_input_capacity: VIDEO_FRAME_PREPARE_PENDING_INPUT_QUEUE_CAPACITY,
            in_flight_frames: self.in_flight_frames(),
            completed_frames: self.completed.len(),
            command_queue_capacity: VIDEO_FRAME_PREPARE_COMMAND_QUEUE_CAPACITY,
        }
    }

    pub(super) fn poll_result(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoFramePrepareResult>, String> {
        self.pump_available_results()?;
        let Some(index) = self
            .completed
            .iter()
            .position(|result| result.generation == generation)
        else {
            return Ok(None);
        };
        Ok(self.completed.remove(index))
    }

    pub(super) fn has_pending_for_generation(
        &mut self,
        generation: u64,
    ) -> std::result::Result<bool, String> {
        self.pump_available_results()?;
        Ok(self
            .pending_inputs
            .iter()
            .any(|input| input.generation == generation)
            || self
                .in_flight_by_generation
                .get(&generation)
                .copied()
                .unwrap_or_default()
                > 0
            || self
                .completed
                .iter()
                .any(|result| result.generation == generation))
    }

    pub(super) fn flush_generation(&mut self, generation: u64) {
        let pending_input_frames = self.pending_inputs.len();
        let completed_frames = self.completed.len();
        let in_flight_frames = self.in_flight_frames();
        tracing::debug!(
            generation,
            previous_generation_floor = self.generation_floor,
            pending_input_frames,
            completed_frames,
            in_flight_frames,
            "flushing FFmpeg video frame prepare worker generation"
        );
        self.generation_floor = self.generation_floor.max(generation);
        self.pending_inputs.clear();
        self.completed.clear();
        self.in_flight_by_generation.clear();
        while let Ok(_result) = self.result_rx.try_recv() {}
    }

    fn try_send_or_buffer(
        &mut self,
        input: VideoFramePrepareInput,
    ) -> std::result::Result<VideoFramePrepareEnqueueResult, String> {
        match self.try_send_direct(input)? {
            VideoFramePrepareDirectEnqueueResult::Queued => {
                Ok(VideoFramePrepareEnqueueResult::Queued)
            }
            VideoFramePrepareDirectEnqueueResult::InputFull(input) => {
                Ok(self.buffer_pending_input(input))
            }
        }
    }

    fn try_send_direct(
        &mut self,
        input: VideoFramePrepareInput,
    ) -> std::result::Result<VideoFramePrepareDirectEnqueueResult, String> {
        let generation = input.generation;
        match self
            .command_tx
            .try_send(VideoFramePrepareCommand::Prepare(input))
        {
            Ok(()) => {
                *self.in_flight_by_generation.entry(generation).or_insert(0) += 1;
                Ok(VideoFramePrepareDirectEnqueueResult::Queued)
            }
            Err(mpsc::TrySendError::Full(VideoFramePrepareCommand::Prepare(input))) => {
                Ok(VideoFramePrepareDirectEnqueueResult::InputFull(input))
            }
            Err(mpsc::TrySendError::Full(VideoFramePrepareCommand::Shutdown)) => {
                unreachable!("shutdown command is not sent through prepare enqueue")
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err("FFmpeg video frame prepare worker 已停止".to_string())
            }
        }
    }

    fn buffer_pending_input(
        &mut self,
        input: VideoFramePrepareInput,
    ) -> VideoFramePrepareEnqueueResult {
        let was_full = self.pending_input_full();
        self.pending_inputs.push_back(input);
        if was_full || self.pending_input_full() {
            VideoFramePrepareEnqueueResult::InputFull
        } else {
            VideoFramePrepareEnqueueResult::Queued
        }
    }

    fn pump_available_results(&mut self) -> std::result::Result<(), String> {
        while let Ok(result) = self.result_rx.try_recv() {
            if result.generation < self.generation_floor {
                continue;
            }
            self.record_completed_result(result);
        }
        Ok(())
    }

    fn record_completed_result(&mut self, result: VideoFramePrepareResult) {
        if let Some(count) = self.in_flight_by_generation.get_mut(&result.generation) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.in_flight_by_generation.remove(&result.generation);
            }
        }
        self.completed.push_back(result);
    }

    fn in_flight_frames(&self) -> usize {
        self.in_flight_by_generation.values().copied().sum()
    }

    fn state(&self) -> VideoFramePrepareWorkerState {
        if self.pending_input_full() {
            VideoFramePrepareWorkerState::InputFull
        } else if !self.completed.is_empty() {
            VideoFramePrepareWorkerState::HaveFrame
        } else if self.in_flight_frames() > 0 {
            VideoFramePrepareWorkerState::Preparing
        } else if !self.pending_inputs.is_empty() {
            VideoFramePrepareWorkerState::PendingInput
        } else {
            VideoFramePrepareWorkerState::NeedFrame
        }
    }
}

enum VideoFramePrepareDirectEnqueueResult {
    Queued,
    InputFull(VideoFramePrepareInput),
}

impl Drop for VideoFramePrepareWorker {
    fn drop(&mut self) {
        while let Ok(_result) = self.result_rx.try_recv() {}
        let _ = self.command_tx.send(VideoFramePrepareCommand::Shutdown);
        while let Ok(_result) = self.result_rx.try_recv() {}
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_video_frame_prepare_worker(
    buffer_pool: FrameBufferPool,
    command_rx: mpsc::Receiver<VideoFramePrepareCommand>,
    result_tx: mpsc::SyncSender<VideoFramePrepareResult>,
) {
    let mut video_converter = VideoFrameConverter::new(buffer_pool);
    loop {
        let recv_started_at = Instant::now();
        let command = match command_rx.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        let recv_wait = recv_started_at.elapsed();
        log_video_frame_prepare_worker_recv_wait(&command, recv_wait);
        match command {
            VideoFramePrepareCommand::Prepare(input) => {
                let generation = input.generation;
                let diagnostic = input.diagnostic;
                let timeline_nsecs = input.timeline_nsecs;
                let duration_nsecs = input.duration_nsecs;
                let started = Instant::now();
                let result = prepare_video_frame(&mut video_converter, input);
                let prepare_elapsed = started.elapsed();
                let result_ok = result.is_ok();
                let send_started_at = Instant::now();
                let send_result = result_tx.send(VideoFramePrepareResult {
                    generation,
                    result,
                    elapsed: prepare_elapsed,
                });
                let send_elapsed = send_started_at.elapsed();
                log_video_frame_prepare_worker_timing(VideoFramePrepareWorkerTiming {
                    generation,
                    diagnostic,
                    timeline_nsecs,
                    duration_nsecs,
                    recv_wait,
                    prepare_elapsed,
                    send_elapsed,
                    result_ok,
                    result_send_ok: send_result.is_ok(),
                });
                if send_result.is_err() {
                    break;
                }
            }
            VideoFramePrepareCommand::Shutdown => break,
        }
    }
}

fn prepare_video_frame(
    video_converter: &mut VideoFrameConverter,
    input: VideoFramePrepareInput,
) -> std::result::Result<PreparedVideoFrame, String> {
    let generation = input.generation;
    let source_frame_diagnostic = input.frame_diagnostic;
    let mut frame = video_converter.convert_with_context(
        &input.convert_context,
        input.frame.as_mut_ptr(),
        input.dovi_metadata,
    )?;
    frame.pts = Some(input.frame_pts);
    Ok(PreparedVideoFrame {
        generation,
        frame,
        timeline_nsecs: input.timeline_nsecs,
        duration_nsecs: input.duration_nsecs,
        source_frame_diagnostic,
    })
}

struct VideoFramePrepareWorkerTiming {
    generation: u64,
    diagnostic: VideoFramePrepareDiagnosticContext,
    timeline_nsecs: u64,
    duration_nsecs: u64,
    recv_wait: Duration,
    prepare_elapsed: Duration,
    send_elapsed: Duration,
    result_ok: bool,
    result_send_ok: bool,
}

fn log_video_frame_prepare_worker_recv_wait(
    command: &VideoFramePrepareCommand,
    recv_wait: Duration,
) {
    tracing::trace!(
        command = video_frame_prepare_command_kind(command),
        generation = ?video_frame_prepare_command_generation(command),
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        "FFmpeg video frame prepare worker command recv timing"
    );
    if recv_wait < WORKER_CHANNEL_RECV_WAIT_LOG_AFTER {
        return;
    }
    tracing::debug!(
        command = video_frame_prepare_command_kind(command),
        generation = ?video_frame_prepare_command_generation(command),
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        "FFmpeg video frame prepare worker waited for command"
    );
}

fn video_frame_prepare_command_kind(command: &VideoFramePrepareCommand) -> &'static str {
    match command {
        VideoFramePrepareCommand::Prepare(_) => "prepare",
        VideoFramePrepareCommand::Shutdown => "shutdown",
    }
}

fn video_frame_prepare_command_generation(command: &VideoFramePrepareCommand) -> Option<u64> {
    match command {
        VideoFramePrepareCommand::Prepare(input) => Some(input.generation),
        VideoFramePrepareCommand::Shutdown => None,
    }
}

fn log_video_frame_prepare_worker_timing(timing: VideoFramePrepareWorkerTiming) {
    let diagnostic = timing.diagnostic;
    tracing::trace!(
        session_id = ?diagnostic.session_id,
        generation = timing.generation,
        decoded_video_frame_count = diagnostic.decoded_video_frame_count,
        timeline_nsecs = timing.timeline_nsecs,
        duration_nsecs = timing.duration_nsecs,
        recv_wait_ms = timing.recv_wait.as_secs_f64() * 1000.0,
        prepare_ms = timing.prepare_elapsed.as_secs_f64() * 1000.0,
        result_send_block_ms = timing.send_elapsed.as_secs_f64() * 1000.0,
        result_ok = timing.result_ok,
        result_send_ok = timing.result_send_ok,
        output_state = ?diagnostic.output_state,
        output_first_video_frame_pending = diagnostic.output_first_video_frame_pending,
        output_rebuffering = diagnostic.output_rebuffering,
        queued_video_frames = diagnostic.queued_video_frames,
        queued_video_range = ?diagnostic.queued_video_range_nsecs,
        pending_start_audio_ms = diagnostic.pending_start_audio_nsecs as f64 / 1_000_000.0,
        "FFmpeg video frame prepare worker timing"
    );
    if diagnostic.force_completion_log {
        tracing::debug!(
            session_id = ?diagnostic.session_id,
            generation = timing.generation,
            decoded_video_frame_count = diagnostic.decoded_video_frame_count,
            force_completion_reason = diagnostic.force_completion_reason,
            timeline_nsecs = timing.timeline_nsecs,
            duration_nsecs = timing.duration_nsecs,
            recv_wait_ms = timing.recv_wait.as_secs_f64() * 1000.0,
            prepare_ms = timing.prepare_elapsed.as_secs_f64() * 1000.0,
            result_send_block_ms = timing.send_elapsed.as_secs_f64() * 1000.0,
            result_ok = timing.result_ok,
            result_send_ok = timing.result_send_ok,
            output_state = ?diagnostic.output_state,
            output_first_video_frame_pending = diagnostic.output_first_video_frame_pending,
            output_rebuffering = diagnostic.output_rebuffering,
            queued_video_frames = diagnostic.queued_video_frames,
            queued_video_range = ?diagnostic.queued_video_range_nsecs,
            pending_start_audio_ms = diagnostic.pending_start_audio_nsecs as f64 / 1_000_000.0,
            "FFmpeg video frame prepare worker completed diagnostic frame"
        );
        return;
    }
    if timing.prepare_elapsed < DECODE_PACKET_SLOW_LOG_AFTER
        && timing.send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        session_id = ?diagnostic.session_id,
        generation = timing.generation,
        decoded_video_frame_count = diagnostic.decoded_video_frame_count,
        timeline_nsecs = timing.timeline_nsecs,
        duration_nsecs = timing.duration_nsecs,
        recv_wait_ms = timing.recv_wait.as_secs_f64() * 1000.0,
        prepare_ms = timing.prepare_elapsed.as_secs_f64() * 1000.0,
        result_send_block_ms = timing.send_elapsed.as_secs_f64() * 1000.0,
        result_ok = timing.result_ok,
        result_send_ok = timing.result_send_ok,
        output_state = ?diagnostic.output_state,
        output_first_video_frame_pending = diagnostic.output_first_video_frame_pending,
        output_rebuffering = diagnostic.output_rebuffering,
        queued_video_frames = diagnostic.queued_video_frames,
        queued_video_range = ?diagnostic.queued_video_range_nsecs,
        pending_start_audio_ms = diagnostic.pending_start_audio_nsecs as f64 / 1_000_000.0,
        "FFmpeg video frame prepare worker completed slowly"
    );
}

#[cfg(test)]
mod tests {
    use super::super::video_decode_worker::VideoDecodedFrame;
    use std::{
        collections::{BTreeMap, VecDeque},
        os::raw::c_int,
        sync::mpsc,
    };

    use ffmpeg_sys_next as ffi;

    use crate::player::render_host::{FfmpegFrameRef, FramePts, PlaybackSessionId, RenderSize};

    use super::super::{
        AvFrame, DEFAULT_VIDEO_FRAME_DURATION_NSECS, PlaybackOutputState, VideoFrameConvertContext,
    };
    use super::{VideoFramePrepareEnqueueResult, VideoFramePrepareInput, VideoFramePrepareWorker};

    fn test_worker() -> VideoFramePrepareWorker {
        let (command_tx, _command_rx) = mpsc::sync_channel(1);
        let (_result_tx, result_rx) = mpsc::sync_channel(1);
        VideoFramePrepareWorker {
            command_tx,
            result_rx,
            handle: None,
            pending_inputs: VecDeque::new(),
            completed: VecDeque::new(),
            in_flight_by_generation: BTreeMap::new(),
            generation_floor: 0,
        }
    }

    fn test_input(generation: u64) -> VideoFramePrepareInput {
        let size = RenderSize {
            width: 1,
            height: 1,
        };
        let mut av_frame = AvFrame::new().expect("FFmpeg frame allocates");
        unsafe {
            (*av_frame.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
            (*av_frame.as_mut_ptr()).width = 1;
            (*av_frame.as_mut_ptr()).height = 1;
        }
        let buffer_result = unsafe { ffi::av_frame_get_buffer(av_frame.as_mut_ptr(), 1) };
        assert!(buffer_result >= 0, "FFmpeg frame buffer allocates");
        let frame_diagnostic =
            super::DecodedVideoFrameDiagnostic::from_frame(av_frame.as_mut_ptr());
        let frame = FfmpegFrameRef::new_ref(av_frame.as_mut_ptr()).expect("FFmpeg frame refs");

        VideoFramePrepareInput {
            generation,
            diagnostic: super::VideoFramePrepareDiagnosticContext {
                session_id: PlaybackSessionId::default(),
                decoded_video_frame_count: generation,
                force_completion_log: false,
                force_completion_reason: "test",
                output_state: PlaybackOutputState::Syncing,
                output_first_video_frame_pending: true,
                output_rebuffering: false,
                queued_video_frames: 0,
                queued_video_range_nsecs: None,
                pending_start_audio_nsecs: 0,
            },
            frame_diagnostic,
            frame: VideoDecodedFrame::new_for_test(frame),
            frame_pts: FramePts { nsecs: generation },
            timeline_nsecs: generation,
            duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
            convert_context: VideoFrameConvertContext::new_for_test(size),
            dovi_metadata: None,
        }
    }

    #[test]
    fn decoded_video_frame_diagnostic_captures_decoder_frame_properties() {
        let mut frame = AvFrame::new().expect("FFmpeg frame allocates");
        unsafe {
            (*frame.as_mut_ptr()).best_effort_timestamp = 208_000;
            (*frame.as_mut_ptr()).pts = 208_001;
            (*frame.as_mut_ptr()).pkt_dts = 207_960;
            (*frame.as_mut_ptr()).duration = 40;
            (*frame.as_mut_ptr()).flags = ffi::AV_FRAME_FLAG_KEY;
            (*frame.as_mut_ptr()).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            (*frame.as_mut_ptr()).decode_error_flags = ffi::FF_DECODE_ERROR_MISSING_REFERENCE;
            (*frame.as_mut_ptr()).width = 3840;
            (*frame.as_mut_ptr()).height = 2160;
            (*frame.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as c_int;
        }

        let diagnostic = super::DecodedVideoFrameDiagnostic::from_frame(frame.as_mut_ptr());

        assert_eq!(diagnostic.best_effort_timestamp, 208_000);
        assert_eq!(diagnostic.pts, 208_001);
        assert_eq!(diagnostic.packet_dts, 207_960);
        assert_eq!(diagnostic.duration, 40);
        assert!(diagnostic.key_frame);
        assert!(diagnostic.corrupt);
        assert_eq!(
            diagnostic.picture_type,
            ffi::AVPictureType::AV_PICTURE_TYPE_I as i32
        );
        assert_eq!(
            diagnostic.decode_error_flags,
            ffi::FF_DECODE_ERROR_MISSING_REFERENCE
        );
        assert_eq!(diagnostic.width, 3840);
        assert_eq!(diagnostic.height, 2160);
        assert_eq!(
            diagnostic.pixel_format,
            ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as c_int
        );
    }

    #[test]
    fn video_frame_prepare_pending_inputs_preserve_fifo_until_capacity() {
        let mut worker = test_worker();

        assert_eq!(
            worker.buffer_pending_input(test_input(1)),
            VideoFramePrepareEnqueueResult::Queued
        );
        assert_eq!(
            worker.buffer_pending_input(test_input(2)),
            VideoFramePrepareEnqueueResult::Queued
        );
        assert_eq!(
            worker.buffer_pending_input(test_input(3)),
            VideoFramePrepareEnqueueResult::InputFull
        );

        assert!(worker.pending_input_full());
        assert_eq!(
            worker
                .pending_inputs
                .iter()
                .map(|input| input.generation)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn video_frame_prepare_flush_generation_clears_pending_inputs() {
        let mut worker = test_worker();
        worker.buffer_pending_input(test_input(1));
        worker.buffer_pending_input(test_input(2));

        worker.flush_generation(5);

        assert!(!worker.has_pending_input());
        assert!(!worker.pending_input_full());
        assert_eq!(worker.generation_floor, 5);
    }
}
