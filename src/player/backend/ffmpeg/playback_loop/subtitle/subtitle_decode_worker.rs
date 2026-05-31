use super::*;

const SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY: usize = 4;
const SUBTITLE_DECODE_RESULT_QUEUE_CAPACITY: usize = 32;

pub(super) struct SubtitleDecodeWorker {
    command_tx: mpsc::SyncSender<SubtitleDecodeCommand>,
    result_rx: Receiver<SubtitleDecodeResult>,
    handle: Option<JoinHandle<()>>,
    info: SubtitleDecodeWorkerInfo,
    completed_packets: VecDeque<SubtitleDecodePacketStatus>,
    in_flight_packets: usize,
    flush_generation: Option<u64>,
    flush_command_sent: bool,
    recovering: bool,
}

#[derive(Clone, Copy)]
pub(super) struct SubtitleDecodeWorkerInfo {
    pub(super) stream_index: c_int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubtitleDecodeWorkerState {
    NeedPacket,
    Decoding,
    HaveResult,
    InputFull,
    Recovering,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SubtitleDecodeWorkerSnapshot {
    pub(super) state: SubtitleDecodeWorkerState,
    pub(super) pending_input_packets: usize,
    pub(super) pending_input_capacity: usize,
    pub(super) in_flight_packets: usize,
    pub(super) command_queue_capacity: usize,
    pub(super) completed_packets: usize,
}

impl SubtitleDecodeWorkerSnapshot {
    pub(super) fn pending_input_full(self) -> bool {
        self.pending_input_capacity > 0 && self.pending_input_packets >= self.pending_input_capacity
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubtitleDecodeEnqueueResult {
    Queued,
    InputFull,
}

#[derive(Clone, Copy)]
pub(super) struct SubtitleDecodePacketContext {
    pub(super) current_start_position_nsecs: u64,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
}

#[derive(Debug)]
pub(super) struct SubtitleDecodePacketStatus {
    pub(super) generation: u64,
    pub(super) result: std::result::Result<(), String>,
    pub(super) decoded_cues: u64,
    pub(super) elapsed: Duration,
    pub(super) updates: Vec<SubtitleCueUpdate>,
}

#[derive(Debug)]
pub(super) enum SubtitleCueUpdate {
    Push(BackendSubtitleCue),
    TrimOverlapsAt(u64),
}

enum SubtitleDecodeCommand {
    Decode {
        generation: u64,
        packet: AvPacket,
        context: SubtitleDecodePacketContext,
    },
    FlushBuffers {
        generation: u64,
    },
    Shutdown,
}

enum SubtitleDecodeResult {
    PacketDone {
        generation: u64,
        result: std::result::Result<(), String>,
        updates: Vec<SubtitleCueUpdate>,
        elapsed: Duration,
    },
    Flushed {
        generation: u64,
    },
}

impl SubtitleDecodeWorker {
    pub(super) fn spawn(decoder: Decoder, stream: StreamInfo) -> std::result::Result<Self, String> {
        let info = SubtitleDecodeWorkerInfo {
            stream_index: decoder.stream_index,
        };
        let (command_tx, command_rx) = mpsc::sync_channel(SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(SUBTITLE_DECODE_RESULT_QUEUE_CAPACITY);
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-subtitle-decode".to_string())
            .spawn(move || run_subtitle_decode_worker(decoder, stream, command_rx, result_tx))
            .map_err(|error| format!("创建 FFmpeg subtitle decode worker 失败：{error}"))?;

        Ok(Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            info,
            completed_packets: VecDeque::new(),
            in_flight_packets: 0,
            flush_generation: None,
            flush_command_sent: false,
            recovering: false,
        })
    }

    pub(super) fn info(&self) -> &SubtitleDecodeWorkerInfo {
        &self.info
    }

    pub(super) fn snapshot(&self) -> SubtitleDecodeWorkerSnapshot {
        SubtitleDecodeWorkerSnapshot {
            state: self.state(),
            pending_input_packets: 0,
            pending_input_capacity: 0,
            in_flight_packets: self.in_flight_packets,
            command_queue_capacity: SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY,
            completed_packets: self.completed_packets.len(),
        }
    }

    pub(super) fn try_enqueue_packet(
        &mut self,
        packet: &AvPacket,
        generation: u64,
        context: SubtitleDecodePacketContext,
    ) -> std::result::Result<SubtitleDecodeEnqueueResult, String> {
        self.pump_available_results()?;
        if self.recovering {
            return Ok(SubtitleDecodeEnqueueResult::InputFull);
        }

        let packet = AvPacket::ref_from(packet)?;
        match self.command_tx.try_send(SubtitleDecodeCommand::Decode {
            generation,
            packet,
            context,
        }) {
            Ok(()) => {
                self.in_flight_packets = self.in_flight_packets.saturating_add(1);
                Ok(SubtitleDecodeEnqueueResult::Queued)
            }
            Err(mpsc::TrySendError::Full(_)) => Ok(SubtitleDecodeEnqueueResult::InputFull),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err("FFmpeg subtitle decode worker 已停止".to_string())
            }
        }
    }

    pub(super) fn flush_buffers(&mut self, generation: u64) -> std::result::Result<(), String> {
        self.completed_packets.clear();
        self.recovering = true;
        self.flush_generation = Some(generation);
        self.flush_command_sent = false;
        self.try_send_pending_control_commands()
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<SubtitleDecodePacketStatus>, String> {
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

    fn pump_available_results(&mut self) -> std::result::Result<(), String> {
        self.try_send_pending_control_commands()?;
        while self.recovering
            || self.completed_packets.len() < SUBTITLE_DECODE_RESULT_QUEUE_CAPACITY
        {
            match self.result_rx.try_recv() {
                Ok(result) => self.record_result(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("FFmpeg subtitle decode worker 已停止".to_string());
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
                .try_send(SubtitleDecodeCommand::FlushBuffers { generation })
            {
                Ok(()) => self.flush_command_sent = true,
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg subtitle decode worker 已停止".to_string());
                }
            }
        }
        Ok(())
    }

    fn record_result(&mut self, result: SubtitleDecodeResult) {
        if self.recovering {
            match result {
                SubtitleDecodeResult::Flushed { generation }
                    if self.flush_generation == Some(generation) =>
                {
                    self.in_flight_packets = 0;
                    self.recovering = false;
                    self.flush_generation = None;
                    self.flush_command_sent = false;
                    self.completed_packets.clear();
                }
                SubtitleDecodeResult::PacketDone { .. } => {
                    self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                }
                SubtitleDecodeResult::Flushed { .. } => {}
            }
            return;
        }

        match result {
            SubtitleDecodeResult::PacketDone {
                generation,
                result,
                updates,
                elapsed,
            } => {
                self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                self.completed_packets
                    .push_back(SubtitleDecodePacketStatus {
                        generation,
                        result,
                        decoded_cues: updates.len() as u64,
                        elapsed,
                        updates,
                    });
            }
            SubtitleDecodeResult::Flushed { .. } => {
                self.in_flight_packets = 0;
                self.recovering = false;
                self.flush_generation = None;
                self.flush_command_sent = false;
                self.completed_packets.clear();
            }
        }
    }

    fn state(&self) -> SubtitleDecodeWorkerState {
        if self.recovering {
            return SubtitleDecodeWorkerState::Recovering;
        }
        if self.in_flight_packets >= SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY {
            return SubtitleDecodeWorkerState::InputFull;
        }
        if !self.completed_packets.is_empty() {
            return SubtitleDecodeWorkerState::HaveResult;
        }
        if self.in_flight_packets > 0 {
            return SubtitleDecodeWorkerState::Decoding;
        }
        SubtitleDecodeWorkerState::NeedPacket
    }
}

impl Drop for SubtitleDecodeWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(SubtitleDecodeCommand::Shutdown);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::debug!("FFmpeg subtitle decode worker panicked during shutdown");
        }
    }
}

fn run_subtitle_decode_worker(
    decoder: Decoder,
    stream: StreamInfo,
    command_rx: Receiver<SubtitleDecodeCommand>,
    result_tx: mpsc::SyncSender<SubtitleDecodeResult>,
) {
    let mut filter = match PgsFrameMergeBitstreamFilter::new(stream) {
        Ok(filter) => filter,
        Err(error) => {
            tracing::error!(%error, "failed to initialize FFmpeg subtitle bitstream filter");
            None
        }
    };
    let mut filtered_packet = if filter.is_some() {
        match AvPacket::new() {
            Ok(packet) => Some(packet),
            Err(error) => {
                tracing::error!(%error, "failed to initialize FFmpeg subtitle filter packet");
                None
            }
        }
    } else {
        None
    };

    while let Ok(command) = command_rx.recv() {
        match command {
            SubtitleDecodeCommand::Decode {
                generation,
                mut packet,
                context,
            } => {
                let started = Instant::now();
                let mut updates = Vec::new();
                let result = decode_subtitle_worker_packet(
                    &decoder,
                    stream,
                    &mut packet,
                    context,
                    filter.as_mut(),
                    filtered_packet.as_mut(),
                    &mut updates,
                );
                if result_tx
                    .send(SubtitleDecodeResult::PacketDone {
                        generation,
                        result,
                        updates,
                        elapsed: started.elapsed(),
                    })
                    .is_err()
                {
                    break;
                }
            }
            SubtitleDecodeCommand::FlushBuffers { generation } => {
                decoder.flush_buffers();
                if let Some(filter) = filter.as_mut() {
                    filter.flush();
                }
                if let Some(packet) = filtered_packet.as_mut() {
                    packet.unref();
                }
                if result_tx
                    .send(SubtitleDecodeResult::Flushed { generation })
                    .is_err()
                {
                    break;
                }
            }
            SubtitleDecodeCommand::Shutdown => break,
        }
    }
}

fn decode_subtitle_worker_packet(
    decoder: &Decoder,
    stream: StreamInfo,
    packet: &mut AvPacket,
    context: SubtitleDecodePacketContext,
    filter: Option<&mut PgsFrameMergeBitstreamFilter>,
    filtered_packet: Option<&mut AvPacket>,
    updates: &mut Vec<SubtitleCueUpdate>,
) -> std::result::Result<(), String> {
    if let Some(filter) = filter {
        filter.send_packet(packet.as_mut_ptr())?;
        let filtered_packet =
            filtered_packet.ok_or_else(|| "FFmpeg subtitle filter packet missing".to_string())?;
        loop {
            if !filter.receive_packet(filtered_packet)? {
                break;
            }
            decode_subtitle_packet_updates(decoder, stream, filtered_packet, context, updates)?;
            filtered_packet.unref();
        }
        Ok(())
    } else {
        decode_subtitle_packet_updates(decoder, stream, packet, context, updates)
    }
}

fn decode_subtitle_packet_updates(
    decoder: &Decoder,
    stream: StreamInfo,
    packet: &AvPacket,
    context: SubtitleDecodePacketContext,
    updates: &mut Vec<SubtitleCueUpdate>,
) -> std::result::Result<(), String> {
    let packet_timestamp = packet.best_timestamp();
    if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_SUBRIP
        && let Some(cue) = packet.data().and_then(|data| {
            decoded_subrip_packet_cue(
                data,
                packet
                    .duration()
                    .and_then(|duration| timestamp_to_nsecs(duration, stream.time_base)),
            )
        })
    {
        if let Some(update) = subtitle_cue_update(
            cue,
            packet_timestamp,
            stream,
            context.current_start_position_nsecs,
            context.playback_timeline_origin_nsecs,
        ) {
            updates.push(update);
        }
        return Ok(());
    }

    decoder.decode_subtitle_packet(packet.as_ptr(), |cue| {
        if let Some(update) = subtitle_cue_update(
            cue,
            packet_timestamp,
            stream,
            context.current_start_position_nsecs,
            context.playback_timeline_origin_nsecs,
        ) {
            updates.push(update);
        }
        Ok(())
    })
}

fn subtitle_cue_update(
    cue: DecodedSubtitleCue,
    packet_timestamp: Option<i64>,
    stream: StreamInfo,
    current_start_position_nsecs: u64,
    playback_timeline_origin_nsecs: Option<u64>,
) -> Option<SubtitleCueUpdate> {
    let Some(base_timeline_nsecs) = subtitle_cue_timeline_nsecs(
        cue.pts_nsecs,
        packet_timestamp,
        stream,
        playback_timeline_origin_nsecs,
    ) else {
        tracing::debug!(
            stream_index = stream.index,
            ?packet_timestamp,
            cue_pts_nsecs = ?cue.pts_nsecs,
            "dropping decoded subtitle cue without timestamp"
        );
        return None;
    };
    let cue_has_content = cue.has_content();
    let subtitle_cue = BackendSubtitleCue {
        text: cue.text,
        bitmaps: cue.bitmaps,
        start_nsecs: base_timeline_nsecs.saturating_add(cue.start_offset_nsecs),
        end_nsecs: base_timeline_nsecs.saturating_add(cue.end_offset_nsecs),
    };
    if !cue_has_content {
        return (stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
            .then_some(SubtitleCueUpdate::TrimOverlapsAt(subtitle_cue.start_nsecs));
    }
    if subtitle_cue.end_nsecs >= current_start_position_nsecs {
        return Some(SubtitleCueUpdate::Push(subtitle_cue));
    }
    None
}
