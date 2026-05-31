use std::collections::BTreeMap;

use super::video_decode_worker::VideoDecodedFrame;
use super::*;

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
    pub(super) frame: VideoDecodedFrame,
    pub(super) frame_pts: FramePts,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
    pub(super) convert_context: VideoFrameConvertContext,
    pub(super) dovi_metadata: Option<DoviFrameMetadata>,
}

pub(super) struct PreparedVideoFrame {
    pub(super) frame: DecodedFrame,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
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
    while let Ok(command) = command_rx.recv() {
        match command {
            VideoFramePrepareCommand::Prepare(input) => {
                let generation = input.generation;
                let started = Instant::now();
                let result = prepare_video_frame(&mut video_converter, input);
                if result_tx
                    .send(VideoFramePrepareResult {
                        generation,
                        result,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
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
    let mut frame = video_converter.convert_with_context(
        &input.convert_context,
        input.frame.as_mut_ptr(),
        input.dovi_metadata,
    )?;
    frame.pts = Some(input.frame_pts);
    Ok(PreparedVideoFrame {
        frame,
        timeline_nsecs: input.timeline_nsecs,
        duration_nsecs: input.duration_nsecs,
    })
}

#[cfg(test)]
mod tests {
    use super::super::video_decode_worker::VideoDecodedFrame;
    use super::*;

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
        let frame = FfmpegFrameRef::new_ref(av_frame.as_mut_ptr()).expect("FFmpeg frame refs");

        VideoFramePrepareInput {
            generation,
            frame: VideoDecodedFrame::new_for_test(frame),
            frame_pts: FramePts { nsecs: generation },
            timeline_nsecs: generation,
            duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
            convert_context: VideoFrameConvertContext::new_for_test(size),
            dovi_metadata: None,
        }
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
