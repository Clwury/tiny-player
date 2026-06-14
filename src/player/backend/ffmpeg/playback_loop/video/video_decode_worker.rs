use super::*;

const VIDEO_DECODE_COMMAND_QUEUE_CAPACITY: usize = 4;
const SOFTWARE_DECODED_VIDEO_QUEUE_CAPACITY: usize = 24;

pub(super) struct VideoDecodeWorker {
    command_tx: mpsc::SyncSender<VideoDecodeCommand>,
    result_rx: Receiver<VideoDecodeResult>,
    handle: Option<JoinHandle<()>>,
    info: VideoDecodeWorkerInfo,
    decoded_frames: VecDeque<QueuedVideoDecodedFrame>,
    completed_packets: VecDeque<VideoDecodePacketStatus>,
    decoded_frame_queue_capacity: usize,
    in_flight_packets: usize,
    flush_generation: Option<u64>,
    flush_command_sent: bool,
    drain_generation: Option<u64>,
    drain_command_sent: bool,
    pending_skip_nonref: Option<bool>,
    drain_frames: Vec<VideoDecodedFrame>,
    completed_drains: VecDeque<QueuedVideoDecodeDrainResult>,
    draining: bool,
    recovering: bool,
    eof: bool,
}

#[derive(Clone)]
pub(super) struct VideoDecodeWorkerInfo {
    pub(super) stream_index: c_int,
    pub(super) time_base: ffi::AVRational,
    pub(super) size: Option<RenderSize>,
    pub(super) decoder_name: String,
    pub(super) hardware_accelerated: bool,
    pub(super) vulkan_device: Option<Arc<VulkanDecodeDevice>>,
    pub(super) convert_context: VideoFrameConvertContext,
}

pub(super) struct VideoDecodeDrainResult {
    pub(super) frames: Vec<VideoDecodedFrame>,
    pub(super) result: std::result::Result<(), String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoDecodeWorkerState {
    NeedPacket,
    Decoding,
    HaveFrame,
    OutputFull,
    Draining,
    Recovering,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VideoDecodeWorkerSnapshot {
    pub(super) state: VideoDecodeWorkerState,
    pub(super) queued_frames: usize,
    pub(super) queue_capacity: usize,
    pub(super) pending_input_packets: usize,
    pub(super) pending_input_capacity: usize,
    pub(super) in_flight_packets: usize,
    pub(super) command_queue_capacity: usize,
    pub(super) completed_packets: usize,
}

impl VideoDecodeWorkerSnapshot {
    pub(super) fn pending_input_full(self) -> bool {
        self.pending_input_capacity > 0 && self.pending_input_packets >= self.pending_input_capacity
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VideoDecodeEnqueueResult {
    Queued,
    InputFull,
    OutputFull,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(super) struct VideoDecodePacketStatus {
    pub(super) generation: u64,
    pub(super) result: std::result::Result<(), String>,
    pub(super) decoded_frames: u64,
    pub(super) elapsed: Duration,
    pub(super) drained: bool,
}

pub(super) struct VideoDecodedFrame {
    frame: FfmpegFrameRef,
}

impl VideoDecodedFrame {
    pub(super) fn as_mut_ptr(&self) -> *mut ffi::AVFrame {
        self.frame.as_mut_ptr()
    }

    #[cfg(test)]
    pub(super) fn new_for_test(frame: FfmpegFrameRef) -> Self {
        Self { frame }
    }
}

struct QueuedVideoDecodedFrame {
    generation: u64,
    frame: VideoDecodedFrame,
}

struct QueuedVideoDecodeDrainResult {
    generation: u64,
    result: VideoDecodeDrainResult,
}

enum VideoDecodeCommand {
    Decode { generation: u64, packet: AvPacket },
    FlushBuffers { generation: u64 },
    Drain { generation: u64 },
    SetSkipNonref(bool),
    Shutdown,
}

enum VideoDecodeResult {
    Frame {
        generation: u64,
        frame: VideoDecodedFrame,
    },
    PacketDone {
        generation: u64,
        result: std::result::Result<(), String>,
        decoded_frames: u64,
        elapsed: Duration,
    },
    Flushed {
        generation: u64,
    },
    Drained {
        generation: u64,
        result: std::result::Result<(), String>,
    },
}

impl VideoDecodeWorker {
    pub(super) fn spawn(decoder: Decoder) -> std::result::Result<Self, String> {
        let info = VideoDecodeWorkerInfo {
            stream_index: decoder.stream_index,
            time_base: decoder.time_base,
            size: decoder.size().ok(),
            decoder_name: decoder.decoder_name(),
            hardware_accelerated: decoder.is_hardware_accelerated(),
            vulkan_device: decoder.vulkan_device(),
            convert_context: VideoFrameConvertContext::from_decoder(&decoder),
        };
        let (command_tx, command_rx) = mpsc::sync_channel(VIDEO_DECODE_COMMAND_QUEUE_CAPACITY);
        let decoded_frame_queue_capacity = if info.hardware_accelerated {
            VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES
        } else {
            SOFTWARE_DECODED_VIDEO_QUEUE_CAPACITY
        };
        let result_queue_capacity =
            decoded_frame_queue_capacity.saturating_add(VIDEO_DECODE_COMMAND_QUEUE_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(result_queue_capacity);
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-video-decode".to_string())
            .spawn(move || run_video_decode_worker(decoder, command_rx, result_tx))
            .map_err(|error| format!("创建 FFmpeg video decode worker 失败：{error}"))?;

        Ok(Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            info,
            decoded_frames: VecDeque::new(),
            completed_packets: VecDeque::new(),
            decoded_frame_queue_capacity,
            in_flight_packets: 0,
            flush_generation: None,
            flush_command_sent: false,
            drain_generation: None,
            drain_command_sent: false,
            pending_skip_nonref: None,
            drain_frames: Vec::new(),
            completed_drains: VecDeque::new(),
            draining: false,
            recovering: false,
            eof: false,
        })
    }

    pub(super) fn info(&self) -> &VideoDecodeWorkerInfo {
        &self.info
    }

    pub(super) fn set_skip_nonref_frames(
        &mut self,
        enabled: bool,
    ) -> std::result::Result<(), String> {
        self.pending_skip_nonref = Some(enabled);
        self.try_send_pending_control_commands()
    }

    pub(super) fn snapshot(&self) -> VideoDecodeWorkerSnapshot {
        VideoDecodeWorkerSnapshot {
            state: self.state(),
            queued_frames: self.decoded_frames.len(),
            queue_capacity: self.decoded_frame_queue_capacity,
            pending_input_packets: 0,
            pending_input_capacity: 0,
            in_flight_packets: self.in_flight_packets,
            command_queue_capacity: VIDEO_DECODE_COMMAND_QUEUE_CAPACITY,
            completed_packets: self.completed_packets.len(),
        }
    }

    pub(super) fn service(&mut self) -> std::result::Result<(), String> {
        self.pump_available_results()
    }

    #[allow(dead_code)]
    pub(super) fn try_enqueue_packet(
        &mut self,
        packet: &AvPacket,
        generation: u64,
    ) -> std::result::Result<VideoDecodeEnqueueResult, String> {
        self.pump_available_results()?;
        if self.recovering {
            return Ok(VideoDecodeEnqueueResult::InputFull);
        }
        if self.decoded_frames.len() >= self.decoded_frame_queue_capacity {
            return Ok(VideoDecodeEnqueueResult::OutputFull);
        }

        let packet = AvPacket::ref_from(packet)?;
        match self
            .command_tx
            .try_send(VideoDecodeCommand::Decode { generation, packet })
        {
            Ok(()) => {
                self.in_flight_packets = self.in_flight_packets.saturating_add(1);
                self.eof = false;
                Ok(VideoDecodeEnqueueResult::Queued)
            }
            Err(mpsc::TrySendError::Full(_)) => Ok(VideoDecodeEnqueueResult::InputFull),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err("FFmpeg video decode worker 已停止".to_string())
            }
        }
    }

    #[allow(dead_code)]
    pub(super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodedFrame>, String> {
        self.pump_available_results()?;
        let Some(index) = self
            .decoded_frames
            .iter()
            .position(|frame| frame.generation == generation)
        else {
            return Ok(None);
        };
        Ok(self.decoded_frames.remove(index).map(|queued| queued.frame))
    }

    #[allow(dead_code)]
    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodePacketStatus>, String> {
        self.pump_available_results()?;
        if self
            .decoded_frames
            .iter()
            .any(|frame| frame.generation == generation)
        {
            return Ok(None);
        }
        let Some(index) = self
            .completed_packets
            .iter()
            .position(|status| status.generation == generation)
        else {
            return Ok(None);
        };
        Ok(self.completed_packets.remove(index))
    }

    pub(super) fn flush_buffers(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.decoded_frames.clear();
        self.completed_packets.clear();
        self.drain_frames.clear();
        self.completed_drains.clear();
        self.recovering = true;
        self.draining = false;
        self.eof = false;
        self.flush_generation = Some(generation);
        self.flush_command_sent = false;
        self.drain_generation = None;
        self.drain_command_sent = false;
        self.try_send_pending_control_commands()
    }

    pub(super) fn request_drain(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.pump_available_results()?;
        if self.draining && self.drain_generation == Some(generation) {
            return Ok(());
        }
        self.draining = true;
        self.drain_generation = Some(generation);
        self.drain_command_sent = false;
        self.drain_frames.clear();
        self.try_send_pending_control_commands()
    }

    pub(super) fn poll_drain_result(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<VideoDecodeDrainResult>, String> {
        self.pump_available_results()?;
        let Some(index) = self
            .completed_drains
            .iter()
            .position(|result| result.generation == generation)
        else {
            return Ok(None);
        };
        Ok(self
            .completed_drains
            .remove(index)
            .map(|queued| queued.result))
    }

    fn pump_available_results(&mut self) -> std::result::Result<(), String> {
        self.try_send_pending_control_commands()?;
        while self.recovering
            || self.draining
            || self.decoded_frames.len() < self.decoded_frame_queue_capacity
        {
            match self.result_rx.try_recv() {
                Ok(result) => self.record_result(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("FFmpeg video decode worker 已停止".to_string());
                }
            }
        }
        self.try_send_pending_control_commands()?;
        Ok(())
    }

    fn try_send_pending_control_commands(&mut self) -> std::result::Result<(), String> {
        if self.recovering
            && !self.flush_command_sent
            && let Some(generation) = self.flush_generation
        {
            match self
                .command_tx
                .try_send(VideoDecodeCommand::FlushBuffers { generation })
            {
                Ok(()) => self.flush_command_sent = true,
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg video decode worker 已停止".to_string());
                }
            }
            if !self.flush_command_sent {
                return Ok(());
            }
        }

        if !self.recovering
            && self.draining
            && !self.drain_command_sent
            && let Some(generation) = self.drain_generation
        {
            match self
                .command_tx
                .try_send(VideoDecodeCommand::Drain { generation })
            {
                Ok(()) => {
                    self.drain_command_sent = true;
                    self.in_flight_packets = self.in_flight_packets.saturating_add(1);
                }
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg video decode worker 已停止".to_string());
                }
            }
        }

        if let Some(enabled) = self.pending_skip_nonref {
            match self
                .command_tx
                .try_send(VideoDecodeCommand::SetSkipNonref(enabled))
            {
                Ok(()) => self.pending_skip_nonref = None,
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg video decode worker 已停止".to_string());
                }
            }
        }

        Ok(())
    }

    fn record_result(&mut self, result: VideoDecodeResult) {
        if self.recovering {
            match result {
                VideoDecodeResult::Flushed { generation }
                    if self.flush_generation == Some(generation) =>
                {
                    self.in_flight_packets = 0;
                    self.recovering = false;
                    self.draining = false;
                    self.flush_generation = None;
                    self.flush_command_sent = false;
                    self.drain_generation = None;
                    self.drain_command_sent = false;
                    self.decoded_frames.clear();
                    self.completed_packets.clear();
                    self.drain_frames.clear();
                    self.completed_drains.clear();
                }
                VideoDecodeResult::PacketDone { .. } | VideoDecodeResult::Drained { .. } => {
                    self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                }
                VideoDecodeResult::Frame { .. } | VideoDecodeResult::Flushed { .. } => {}
            }
            return;
        }

        match result {
            VideoDecodeResult::Frame { generation, frame } => {
                if self.drain_generation == Some(generation) {
                    self.drain_frames.push(frame);
                } else {
                    self.queue_decoded_frame(generation, frame);
                }
            }
            VideoDecodeResult::PacketDone {
                generation,
                result,
                decoded_frames,
                elapsed,
            } => {
                self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                self.completed_packets.push_back(VideoDecodePacketStatus {
                    generation,
                    result,
                    decoded_frames,
                    elapsed,
                    drained: false,
                });
            }
            VideoDecodeResult::Drained { generation, result } => {
                self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                self.draining = false;
                self.drain_generation = None;
                self.drain_command_sent = false;
                if result.is_ok() {
                    self.eof = true;
                }
                let frames = std::mem::take(&mut self.drain_frames);
                self.completed_drains
                    .push_back(QueuedVideoDecodeDrainResult {
                        generation,
                        result: VideoDecodeDrainResult { frames, result },
                    });
            }
            VideoDecodeResult::Flushed { .. } => {
                self.in_flight_packets = 0;
                self.recovering = false;
                self.draining = false;
                self.eof = false;
                self.flush_generation = None;
                self.flush_command_sent = false;
                self.drain_generation = None;
                self.drain_command_sent = false;
                self.decoded_frames.clear();
                self.completed_packets.clear();
                self.drain_frames.clear();
                self.completed_drains.clear();
            }
        }
    }

    fn queue_decoded_frame(&mut self, generation: u64, frame: VideoDecodedFrame) {
        if self.decoded_frames.len() >= self.decoded_frame_queue_capacity {
            return;
        }
        self.decoded_frames
            .push_back(QueuedVideoDecodedFrame { generation, frame });
    }

    fn state(&self) -> VideoDecodeWorkerState {
        if self.recovering {
            VideoDecodeWorkerState::Recovering
        } else if self.draining {
            VideoDecodeWorkerState::Draining
        } else if self.eof && self.decoded_frames.is_empty() && self.in_flight_packets == 0 {
            VideoDecodeWorkerState::Eof
        } else if self.decoded_frames.len() >= self.decoded_frame_queue_capacity {
            VideoDecodeWorkerState::OutputFull
        } else if !self.decoded_frames.is_empty() {
            VideoDecodeWorkerState::HaveFrame
        } else if self.in_flight_packets > 0 {
            VideoDecodeWorkerState::Decoding
        } else {
            VideoDecodeWorkerState::NeedPacket
        }
    }
}

impl Drop for VideoDecodeWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(VideoDecodeCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_video_decode_worker(
    decoder: Decoder,
    command_rx: mpsc::Receiver<VideoDecodeCommand>,
    result_tx: mpsc::SyncSender<VideoDecodeResult>,
) {
    let mut frame = match AvFrame::new() {
        Ok(frame) => frame,
        Err(error) => {
            tracing::error!(%error, "failed to initialize FFmpeg video decode worker frame");
            return;
        }
    };
    loop {
        let recv_started_at = Instant::now();
        let command = match command_rx.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        let recv_wait = recv_started_at.elapsed();
        let command_kind = video_decode_command_kind(&command);
        let command_generation = video_decode_command_generation(&command);
        log_video_decode_worker_recv_wait(command_kind, command_generation, recv_wait);
        match command {
            VideoDecodeCommand::Decode { generation, packet } => {
                let started = Instant::now();
                let mut decoded_frames = 0u64;
                let mut frame_send_elapsed = Duration::ZERO;
                let packet_pts = packet.best_timestamp();
                let packet_bytes = packet.byte_len();
                let result = decoder.decode_packet(packet.as_ptr(), &mut frame, |frame| {
                    decoded_frames = decoded_frames.saturating_add(1);
                    let frame =
                        FfmpegFrameRef::new_ref(frame).map_err(|error| error.to_string())?;
                    let send_started_at = Instant::now();
                    let send_result = result_tx.send(VideoDecodeResult::Frame {
                        generation,
                        frame: VideoDecodedFrame { frame },
                    });
                    frame_send_elapsed += send_started_at.elapsed();
                    send_result
                        .map_err(|_| "FFmpeg video decode result receiver stopped".to_string())
                });
                let total_elapsed = started.elapsed();
                let decode_elapsed = total_elapsed.saturating_sub(frame_send_elapsed);
                let result_ok = result.is_ok();
                let packet_done_send_started_at = Instant::now();
                let packet_done_send_result = result_tx.send(VideoDecodeResult::PacketDone {
                    generation,
                    result,
                    decoded_frames,
                    elapsed: total_elapsed,
                });
                let packet_done_send_elapsed = packet_done_send_started_at.elapsed();
                log_video_decode_worker_decode_timing(VideoDecodeWorkerDecodeTiming {
                    generation,
                    packet_pts,
                    packet_bytes,
                    recv_wait,
                    total_elapsed,
                    decode_elapsed,
                    frame_send_elapsed,
                    packet_done_send_elapsed,
                    decoded_frames,
                    result_ok,
                });
                if packet_done_send_result.is_err() {
                    break;
                }
            }
            VideoDecodeCommand::FlushBuffers { generation } => {
                let started = Instant::now();
                decoder.flush_buffers();
                let flush_elapsed = started.elapsed();
                frame.unref();
                let send_started_at = Instant::now();
                let send_result = result_tx.send(VideoDecodeResult::Flushed { generation });
                let send_elapsed = send_started_at.elapsed();
                log_video_decode_worker_control_timing(
                    "flush_buffers",
                    Some(generation),
                    recv_wait,
                    flush_elapsed,
                    send_elapsed,
                );
                if send_result.is_err() {
                    break;
                }
            }
            VideoDecodeCommand::Drain { generation } => {
                let started = Instant::now();
                let mut frame_send_elapsed = Duration::ZERO;
                let result = decoder.flush(&mut frame, |frame| {
                    let frame =
                        FfmpegFrameRef::new_ref(frame).map_err(|error| error.to_string())?;
                    let send_started_at = Instant::now();
                    let send_result = result_tx.send(VideoDecodeResult::Frame {
                        generation,
                        frame: VideoDecodedFrame { frame },
                    });
                    frame_send_elapsed += send_started_at.elapsed();
                    send_result
                        .map_err(|_| "FFmpeg video decode result receiver stopped".to_string())
                });
                let total_elapsed = started.elapsed();
                let flush_elapsed = total_elapsed.saturating_sub(frame_send_elapsed);
                let send_started_at = Instant::now();
                let send_result = result_tx.send(VideoDecodeResult::Drained { generation, result });
                let send_elapsed = send_started_at.elapsed();
                log_video_decode_worker_drain_timing(
                    generation,
                    recv_wait,
                    total_elapsed,
                    flush_elapsed,
                    frame_send_elapsed,
                    send_elapsed,
                );
                if send_result.is_err() {
                    break;
                }
            }
            VideoDecodeCommand::SetSkipNonref(enabled) => {
                let started = Instant::now();
                decoder.set_skip_nonref_frames(enabled);
                log_video_decode_worker_control_timing(
                    "set_skip_nonref",
                    None,
                    recv_wait,
                    started.elapsed(),
                    Duration::ZERO,
                );
            }
            VideoDecodeCommand::Shutdown => break,
        }
    }
}

struct VideoDecodeWorkerDecodeTiming {
    generation: u64,
    packet_pts: Option<i64>,
    packet_bytes: usize,
    recv_wait: Duration,
    total_elapsed: Duration,
    decode_elapsed: Duration,
    frame_send_elapsed: Duration,
    packet_done_send_elapsed: Duration,
    decoded_frames: u64,
    result_ok: bool,
}

fn video_decode_command_kind(command: &VideoDecodeCommand) -> &'static str {
    match command {
        VideoDecodeCommand::Decode { .. } => "decode",
        VideoDecodeCommand::FlushBuffers { .. } => "flush_buffers",
        VideoDecodeCommand::Drain { .. } => "drain",
        VideoDecodeCommand::SetSkipNonref(_) => "set_skip_nonref",
        VideoDecodeCommand::Shutdown => "shutdown",
    }
}

fn video_decode_command_generation(command: &VideoDecodeCommand) -> Option<u64> {
    match command {
        VideoDecodeCommand::Decode { generation, .. }
        | VideoDecodeCommand::FlushBuffers { generation }
        | VideoDecodeCommand::Drain { generation } => Some(*generation),
        VideoDecodeCommand::SetSkipNonref(_) | VideoDecodeCommand::Shutdown => None,
    }
}

fn log_video_decode_worker_recv_wait(
    command_kind: &'static str,
    generation: Option<u64>,
    recv_wait: Duration,
) {
    tracing::trace!(
        command = command_kind,
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker command recv timing"
    );
    if recv_wait < WORKER_CHANNEL_RECV_WAIT_LOG_AFTER {
        return;
    }
    tracing::debug!(
        command = command_kind,
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker waited for command"
    );
}

fn log_video_decode_worker_decode_timing(timing: VideoDecodeWorkerDecodeTiming) {
    tracing::trace!(
        generation = timing.generation,
        packet_pts = ?timing.packet_pts,
        packet_bytes = timing.packet_bytes,
        recv_wait_ms = timing.recv_wait.as_secs_f64() * 1000.0,
        total_ms = timing.total_elapsed.as_secs_f64() * 1000.0,
        decode_ms = timing.decode_elapsed.as_secs_f64() * 1000.0,
        frame_send_block_ms = timing.frame_send_elapsed.as_secs_f64() * 1000.0,
        packet_done_send_block_ms = timing.packet_done_send_elapsed.as_secs_f64() * 1000.0,
        decoded_frames = timing.decoded_frames,
        result_ok = timing.result_ok,
        "FFmpeg video decode worker packet timing"
    );
    if timing.total_elapsed < DECODE_PACKET_SLOW_LOG_AFTER
        && timing.frame_send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
        && timing.packet_done_send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        generation = timing.generation,
        packet_pts = ?timing.packet_pts,
        packet_bytes = timing.packet_bytes,
        recv_wait_ms = timing.recv_wait.as_secs_f64() * 1000.0,
        total_ms = timing.total_elapsed.as_secs_f64() * 1000.0,
        decode_ms = timing.decode_elapsed.as_secs_f64() * 1000.0,
        frame_send_block_ms = timing.frame_send_elapsed.as_secs_f64() * 1000.0,
        packet_done_send_block_ms = timing.packet_done_send_elapsed.as_secs_f64() * 1000.0,
        decoded_frames = timing.decoded_frames,
        result_ok = timing.result_ok,
        "FFmpeg video decode worker packet completed slowly"
    );
}

fn log_video_decode_worker_control_timing(
    command: &'static str,
    generation: Option<u64>,
    recv_wait: Duration,
    work_elapsed: Duration,
    send_elapsed: Duration,
) {
    tracing::trace!(
        command,
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        work_ms = work_elapsed.as_secs_f64() * 1000.0,
        send_block_ms = send_elapsed.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker control command timing"
    );
    if work_elapsed < DECODE_PACKET_SLOW_LOG_AFTER
        && send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        command,
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        work_ms = work_elapsed.as_secs_f64() * 1000.0,
        send_block_ms = send_elapsed.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker control command completed slowly"
    );
}

fn log_video_decode_worker_drain_timing(
    generation: u64,
    recv_wait: Duration,
    total_elapsed: Duration,
    flush_elapsed: Duration,
    frame_send_elapsed: Duration,
    drained_send_elapsed: Duration,
) {
    tracing::trace!(
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        total_ms = total_elapsed.as_secs_f64() * 1000.0,
        flush_ms = flush_elapsed.as_secs_f64() * 1000.0,
        frame_send_block_ms = frame_send_elapsed.as_secs_f64() * 1000.0,
        drained_send_block_ms = drained_send_elapsed.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker drain timing"
    );
    if total_elapsed < DECODE_PACKET_SLOW_LOG_AFTER
        && frame_send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
        && drained_send_elapsed < WORKER_CHANNEL_SEND_WAIT_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        generation,
        recv_wait_ms = recv_wait.as_secs_f64() * 1000.0,
        total_ms = total_elapsed.as_secs_f64() * 1000.0,
        flush_ms = flush_elapsed.as_secs_f64() * 1000.0,
        frame_send_block_ms = frame_send_elapsed.as_secs_f64() * 1000.0,
        drained_send_block_ms = drained_send_elapsed.as_secs_f64() * 1000.0,
        "FFmpeg video decode worker drain completed slowly"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_worker() -> (VideoDecodeWorker, mpsc::SyncSender<VideoDecodeResult>) {
        let (worker, result_tx, _command_rx) = test_worker_with_command_rx();
        (worker, result_tx)
    }

    fn test_worker_with_command_rx() -> (
        VideoDecodeWorker,
        mpsc::SyncSender<VideoDecodeResult>,
        mpsc::Receiver<VideoDecodeCommand>,
    ) {
        let (command_tx, command_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let size = RenderSize {
            width: 1,
            height: 1,
        };

        (
            VideoDecodeWorker {
                command_tx,
                result_rx,
                handle: None,
                info: VideoDecodeWorkerInfo {
                    stream_index: 0,
                    time_base: ffi::AVRational { num: 1, den: 1 },
                    size: Some(size),
                    decoder_name: "test".to_string(),
                    hardware_accelerated: false,
                    vulkan_device: None,
                    convert_context: VideoFrameConvertContext::new_for_test(size),
                },
                decoded_frames: VecDeque::new(),
                completed_packets: VecDeque::new(),
                decoded_frame_queue_capacity: SOFTWARE_DECODED_VIDEO_QUEUE_CAPACITY,
                in_flight_packets: 0,
                flush_generation: None,
                flush_command_sent: false,
                drain_generation: None,
                drain_command_sent: false,
                pending_skip_nonref: None,
                drain_frames: Vec::new(),
                completed_drains: VecDeque::new(),
                draining: false,
                recovering: false,
                eof: false,
            },
            result_tx,
            command_rx,
        )
    }

    fn test_decoded_frame() -> VideoDecodedFrame {
        let mut frame = AvFrame::new().expect("FFmpeg frame allocates");
        unsafe {
            (*frame.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
            (*frame.as_mut_ptr()).width = 1;
            (*frame.as_mut_ptr()).height = 1;
        }
        let result = unsafe { ffi::av_frame_get_buffer(frame.as_mut_ptr(), 1) };
        assert!(result >= 0, "FFmpeg frame buffer allocates");
        VideoDecodedFrame::new_for_test(
            FfmpegFrameRef::new_ref(frame.as_mut_ptr()).expect("FFmpeg frame refs"),
        )
    }

    #[test]
    fn packet_status_waits_until_decoded_frames_for_generation_are_drained() {
        let (mut worker, _result_tx) = test_worker();
        let generation = 7;
        worker.record_result(VideoDecodeResult::Frame {
            generation,
            frame: test_decoded_frame(),
        });
        worker.record_result(VideoDecodeResult::PacketDone {
            generation,
            result: Ok(()),
            decoded_frames: 1,
            elapsed: Duration::from_millis(1),
        });

        assert!(worker.poll_packet_status(generation).unwrap().is_none());
        assert!(worker.poll_frame(generation).unwrap().is_some());

        let status = worker
            .poll_packet_status(generation)
            .unwrap()
            .expect("packet status is available after frames drain");
        assert_eq!(status.generation, generation);
    }

    #[test]
    fn service_pumps_recovery_after_tracked_packets_are_cleared() {
        let (mut worker, result_tx, _command_rx) = test_worker_with_command_rx();
        let generation = 9;
        worker.in_flight_packets = 1;
        worker.flush_buffers(generation).unwrap();

        assert_eq!(worker.snapshot().state, VideoDecodeWorkerState::Recovering);

        result_tx
            .send(VideoDecodeResult::PacketDone {
                generation: generation - 1,
                result: Ok(()),
                decoded_frames: 0,
                elapsed: Duration::from_millis(1),
            })
            .unwrap();
        worker.service().unwrap();

        let snapshot = worker.snapshot();
        assert_eq!(snapshot.state, VideoDecodeWorkerState::Recovering);
        assert_eq!(snapshot.in_flight_packets, 0);

        result_tx
            .send(VideoDecodeResult::Flushed { generation })
            .unwrap();
        worker.service().unwrap();

        let snapshot = worker.snapshot();
        assert_eq!(snapshot.state, VideoDecodeWorkerState::NeedPacket);
        assert_eq!(snapshot.in_flight_packets, 0);
    }
}
