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
    while let Ok(command) = command_rx.recv() {
        match command {
            VideoDecodeCommand::Decode { generation, packet } => {
                let started = Instant::now();
                let mut decoded_frames = 0u64;
                let result = decoder.decode_packet(packet.as_ptr(), &mut frame, |frame| {
                    decoded_frames = decoded_frames.saturating_add(1);
                    let frame =
                        FfmpegFrameRef::new_ref(frame).map_err(|error| error.to_string())?;
                    result_tx
                        .send(VideoDecodeResult::Frame {
                            generation,
                            frame: VideoDecodedFrame { frame },
                        })
                        .map_err(|_| "FFmpeg video decode result receiver stopped".to_string())
                });
                if result_tx
                    .send(VideoDecodeResult::PacketDone {
                        generation,
                        result,
                        decoded_frames,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            VideoDecodeCommand::FlushBuffers { generation } => {
                decoder.flush_buffers();
                frame.unref();
                if result_tx
                    .send(VideoDecodeResult::Flushed { generation })
                    .is_err()
                {
                    break;
                }
            }
            VideoDecodeCommand::Drain { generation } => {
                let result = decoder.flush(&mut frame, |frame| {
                    let frame =
                        FfmpegFrameRef::new_ref(frame).map_err(|error| error.to_string())?;
                    result_tx
                        .send(VideoDecodeResult::Frame {
                            generation,
                            frame: VideoDecodedFrame { frame },
                        })
                        .map_err(|_| "FFmpeg video decode result receiver stopped".to_string())
                });
                if result_tx
                    .send(VideoDecodeResult::Drained { generation, result })
                    .is_err()
                {
                    break;
                }
            }
            VideoDecodeCommand::SetSkipNonref(enabled) => {
                decoder.set_skip_nonref_frames(enabled);
            }
            VideoDecodeCommand::Shutdown => break,
        }
    }
}
