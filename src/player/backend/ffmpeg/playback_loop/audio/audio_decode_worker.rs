use std::{
    collections::VecDeque,
    os::raw::c_int,
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use super::{
    AUDIO_DECODE_QUEUE_LIMIT_DURATION, AudioResampler, AvFrame, AvPacket, DecodedAudio, Decoder,
    duration_nsecs, frame_best_effort_timestamp,
};

const AUDIO_DECODE_COMMAND_QUEUE_CAPACITY: usize = 4;
const AUDIO_DECODE_RESULT_QUEUE_CAPACITY: usize = 128;

pub(super) struct AudioDecodeWorker {
    command_tx: mpsc::SyncSender<AudioDecodeCommand>,
    result_rx: Receiver<AudioDecodeResult>,
    handle: Option<JoinHandle<()>>,
    info: AudioDecodeWorkerInfo,
    decoded_frames: VecDeque<QueuedAudioDecodedFrame>,
    completed_packets: VecDeque<AudioDecodePacketStatus>,
    decoded_duration_nsecs: u64,
    decoded_duration_limit_nsecs: u64,
    in_flight_packets: usize,
    flush_generation: Option<u64>,
    flush_command_sent: bool,
    drain_generation: Option<u64>,
    drain_command_sent: bool,
    drain_frames: Vec<AudioDecodedFrame>,
    completed_drains: VecDeque<QueuedAudioDecodeDrainResult>,
    draining: bool,
    recovering: bool,
    recovery_started_at: Option<Instant>,
    last_result_progress_at: Option<Instant>,
    stale_results_discarded: u64,
    eof: bool,
}

#[derive(Clone, Copy)]
pub(super) struct AudioDecodeWorkerInfo {
    pub(super) stream_index: c_int,
    pub(super) time_base: ffi::AVRational,
    pub(super) output_rate: c_int,
    pub(super) output_channels: c_int,
}

pub(super) struct AudioDecodePacketResult {
    pub(super) frames: Vec<AudioDecodedFrame>,
    pub(super) result: std::result::Result<(), String>,
    pub(super) decoded_frames: u64,
    pub(super) elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioDecodeWorkerState {
    NeedPacket,
    Decoding,
    HaveFrame,
    OutputFull,
    Draining,
    Recovering,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioDecodeWorkerSnapshot {
    pub(super) state: AudioDecodeWorkerState,
    pub(super) queued_frames: usize,
    pub(super) queued_duration_nsecs: u64,
    pub(super) duration_limit_nsecs: u64,
    pub(super) pending_input_packets: usize,
    pub(super) pending_input_capacity: usize,
    pub(super) in_flight_packets: usize,
    pub(super) command_queue_capacity: usize,
    pub(super) completed_packets: usize,
    pub(super) recovery_generation: Option<u64>,
    pub(super) recovery_elapsed: Option<Duration>,
    pub(super) flush_command_sent: bool,
    pub(super) stale_results_discarded: u64,
    pub(super) last_result_progress_elapsed: Option<Duration>,
}

impl AudioDecodeWorkerSnapshot {
    pub(super) fn pending_input_full(self) -> bool {
        self.pending_input_capacity > 0 && self.pending_input_packets >= self.pending_input_capacity
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioDecodeEnqueueResult {
    Queued,
    InputFull,
    OutputFull,
}

#[derive(Debug)]
pub(super) struct AudioDecodePacketStatus {
    pub(super) generation: u64,
    pub(super) result: std::result::Result<(), String>,
    pub(super) decoded_frames: u64,
    pub(super) elapsed: Duration,
    pub(super) drained: bool,
}

pub(super) struct AudioDecodedFrame {
    pub(super) audio: DecodedAudio,
    pub(super) raw_timestamp: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioDecodedFrameTiming {
    pub(super) raw_timestamp: i64,
    pub(super) duration_nsecs: u64,
}

struct QueuedAudioDecodedFrame {
    generation: u64,
    frame: AudioDecodedFrame,
}

struct QueuedAudioDecodeDrainResult {
    generation: u64,
    result: AudioDecodePacketResult,
}

enum AudioDecodeCommand {
    Decode { generation: u64, packet: AvPacket },
    FlushBuffers { generation: u64 },
    Drain { generation: u64 },
    Shutdown,
}

enum AudioDecodeResult {
    Frame {
        generation: u64,
        frame: AudioDecodedFrame,
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
        decoded_frames: u64,
        elapsed: Duration,
    },
}

impl AudioDecodeWorker {
    pub(super) fn spawn(
        decoder: Decoder,
        output_rate: c_int,
        output_channels: c_int,
    ) -> std::result::Result<Self, String> {
        let info = AudioDecodeWorkerInfo {
            stream_index: decoder.stream_index,
            time_base: decoder.time_base,
            output_rate,
            output_channels,
        };
        let (command_tx, command_rx) = mpsc::sync_channel(AUDIO_DECODE_COMMAND_QUEUE_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel(AUDIO_DECODE_RESULT_QUEUE_CAPACITY);
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-audio-decode".to_string())
            .spawn(move || {
                run_audio_decode_worker(
                    decoder,
                    output_rate,
                    output_channels,
                    command_rx,
                    result_tx,
                )
            })
            .map_err(|error| format!("创建 FFmpeg audio decode worker 失败：{error}"))?;

        Ok(Self {
            command_tx,
            result_rx,
            handle: Some(handle),
            info,
            decoded_frames: VecDeque::new(),
            completed_packets: VecDeque::new(),
            decoded_duration_nsecs: 0,
            decoded_duration_limit_nsecs: duration_nsecs(AUDIO_DECODE_QUEUE_LIMIT_DURATION),
            in_flight_packets: 0,
            flush_generation: None,
            flush_command_sent: false,
            drain_generation: None,
            drain_command_sent: false,
            drain_frames: Vec::new(),
            completed_drains: VecDeque::new(),
            draining: false,
            recovering: false,
            recovery_started_at: None,
            last_result_progress_at: None,
            stale_results_discarded: 0,
            eof: false,
        })
    }

    pub(super) fn info(&self) -> &AudioDecodeWorkerInfo {
        &self.info
    }

    pub(super) fn snapshot(&self) -> AudioDecodeWorkerSnapshot {
        let now = Instant::now();
        AudioDecodeWorkerSnapshot {
            state: self.state(),
            queued_frames: self.decoded_frames.len(),
            queued_duration_nsecs: self.decoded_duration_nsecs,
            duration_limit_nsecs: self.decoded_duration_limit_nsecs,
            pending_input_packets: 0,
            pending_input_capacity: 0,
            in_flight_packets: self.in_flight_packets,
            command_queue_capacity: AUDIO_DECODE_COMMAND_QUEUE_CAPACITY,
            completed_packets: self.completed_packets.len(),
            recovery_generation: self.recovering.then_some(self.flush_generation).flatten(),
            recovery_elapsed: self
                .recovering
                .then(|| {
                    self.recovery_started_at
                        .map(|started_at| now.saturating_duration_since(started_at))
                })
                .flatten(),
            flush_command_sent: self.recovering && self.flush_command_sent,
            stale_results_discarded: self.stale_results_discarded,
            last_result_progress_elapsed: self
                .recovering
                .then(|| {
                    self.last_result_progress_at
                        .map(|progress_at| now.saturating_duration_since(progress_at))
                })
                .flatten(),
        }
    }

    pub(super) fn service(&mut self) -> std::result::Result<(), String> {
        self.pump_available_results()
    }

    pub(super) fn try_enqueue_packet(
        &mut self,
        packet: &AvPacket,
        generation: u64,
    ) -> std::result::Result<AudioDecodeEnqueueResult, String> {
        self.pump_available_results()?;
        if self.recovering {
            return Ok(AudioDecodeEnqueueResult::InputFull);
        }
        if self.output_full() {
            return Ok(AudioDecodeEnqueueResult::OutputFull);
        }

        let packet = AvPacket::ref_from(packet)?;
        match self
            .command_tx
            .try_send(AudioDecodeCommand::Decode { generation, packet })
        {
            Ok(()) => {
                self.in_flight_packets = self.in_flight_packets.saturating_add(1);
                self.eof = false;
                Ok(AudioDecodeEnqueueResult::Queued)
            }
            Err(mpsc::TrySendError::Full(_)) => Ok(AudioDecodeEnqueueResult::InputFull),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err("FFmpeg audio decode worker 已停止".to_string())
            }
        }
    }

    pub(super) fn poll_frame(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<AudioDecodedFrame>, String> {
        self.pump_available_results()?;
        let Some(index) = self
            .decoded_frames
            .iter()
            .position(|frame| frame.generation == generation)
        else {
            return Ok(None);
        };
        let Some(queued) = self.decoded_frames.remove(index) else {
            return Ok(None);
        };
        self.decoded_duration_nsecs = self
            .decoded_duration_nsecs
            .saturating_sub(queued.frame.audio.duration_nsecs);
        Ok(Some(queued.frame))
    }

    pub(super) fn decoded_frame_timings(
        &mut self,
    ) -> std::result::Result<Vec<AudioDecodedFrameTiming>, String> {
        self.pump_available_results()?;
        Ok(self
            .decoded_frames
            .iter()
            .map(|queued| AudioDecodedFrameTiming {
                raw_timestamp: queued.frame.raw_timestamp,
                duration_nsecs: queued.frame.audio.duration_nsecs,
            })
            .collect())
    }

    pub(super) fn poll_packet_status(
        &mut self,
        generation: u64,
    ) -> std::result::Result<Option<AudioDecodePacketStatus>, String> {
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
        let now = Instant::now();
        self.decoded_frames.clear();
        self.completed_packets.clear();
        self.drain_frames.clear();
        self.completed_drains.clear();
        self.decoded_duration_nsecs = 0;
        self.recovering = true;
        self.recovery_started_at = Some(now);
        self.last_result_progress_at = Some(now);
        self.stale_results_discarded = 0;
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
    ) -> std::result::Result<Option<AudioDecodePacketResult>, String> {
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
        while self.recovering || self.draining || !self.output_full() {
            match self.result_rx.try_recv() {
                Ok(result) => self.record_result(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("FFmpeg audio decode worker 已停止".to_string());
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
                .try_send(AudioDecodeCommand::FlushBuffers { generation })
            {
                Ok(()) => self.flush_command_sent = true,
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg audio decode worker 已停止".to_string());
                }
            }
        }

        if !self.recovering
            && self.draining
            && !self.drain_command_sent
            && let Some(generation) = self.drain_generation
        {
            match self
                .command_tx
                .try_send(AudioDecodeCommand::Drain { generation })
            {
                Ok(()) => {
                    self.drain_command_sent = true;
                    self.in_flight_packets = self.in_flight_packets.saturating_add(1);
                }
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err("FFmpeg audio decode worker 已停止".to_string());
                }
            }
        }

        Ok(())
    }

    fn record_result(&mut self, result: AudioDecodeResult) {
        if self.recovering {
            self.last_result_progress_at = Some(Instant::now());
            match result {
                AudioDecodeResult::Flushed { generation }
                    if self.flush_generation == Some(generation) =>
                {
                    self.in_flight_packets = 0;
                    self.recovering = false;
                    self.recovery_started_at = None;
                    self.last_result_progress_at = None;
                    self.draining = false;
                    self.flush_generation = None;
                    self.flush_command_sent = false;
                    self.drain_generation = None;
                    self.drain_command_sent = false;
                    self.decoded_frames.clear();
                    self.completed_packets.clear();
                    self.drain_frames.clear();
                    self.completed_drains.clear();
                    self.decoded_duration_nsecs = 0;
                }
                AudioDecodeResult::PacketDone { .. } | AudioDecodeResult::Drained { .. } => {
                    self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                    self.stale_results_discarded = self.stale_results_discarded.saturating_add(1);
                }
                AudioDecodeResult::Frame { .. } | AudioDecodeResult::Flushed { .. } => {
                    self.stale_results_discarded = self.stale_results_discarded.saturating_add(1);
                }
            }
            return;
        }

        match result {
            AudioDecodeResult::Frame { generation, frame } => {
                if self.drain_generation == Some(generation) {
                    self.drain_frames.push(frame);
                } else {
                    self.queue_decoded_frame(generation, frame);
                }
            }
            AudioDecodeResult::PacketDone {
                generation,
                result,
                decoded_frames,
                elapsed,
            } => {
                self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                self.completed_packets.push_back(AudioDecodePacketStatus {
                    generation,
                    result,
                    decoded_frames,
                    elapsed,
                    drained: false,
                });
            }
            AudioDecodeResult::Drained {
                generation,
                result,
                decoded_frames,
                elapsed,
            } => {
                self.in_flight_packets = self.in_flight_packets.saturating_sub(1);
                self.draining = false;
                self.drain_generation = None;
                self.drain_command_sent = false;
                if result.is_ok() {
                    self.eof = true;
                }
                let frames = std::mem::take(&mut self.drain_frames);
                self.completed_drains
                    .push_back(QueuedAudioDecodeDrainResult {
                        generation,
                        result: AudioDecodePacketResult {
                            frames,
                            result,
                            decoded_frames,
                            elapsed,
                        },
                    });
            }
            AudioDecodeResult::Flushed { .. } => {
                self.in_flight_packets = 0;
                self.recovering = false;
                self.recovery_started_at = None;
                self.last_result_progress_at = None;
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
                self.decoded_duration_nsecs = 0;
            }
        }
    }

    fn queue_decoded_frame(&mut self, generation: u64, frame: AudioDecodedFrame) {
        if self.output_full() {
            return;
        }
        self.decoded_duration_nsecs = self
            .decoded_duration_nsecs
            .saturating_add(frame.audio.duration_nsecs);
        self.decoded_frames
            .push_back(QueuedAudioDecodedFrame { generation, frame });
    }

    fn output_full(&self) -> bool {
        self.decoded_duration_limit_nsecs > 0
            && self.decoded_duration_nsecs >= self.decoded_duration_limit_nsecs
    }

    fn state(&self) -> AudioDecodeWorkerState {
        if self.recovering {
            AudioDecodeWorkerState::Recovering
        } else if self.draining {
            AudioDecodeWorkerState::Draining
        } else if self.eof && self.decoded_frames.is_empty() && self.in_flight_packets == 0 {
            AudioDecodeWorkerState::Eof
        } else if self.output_full() {
            AudioDecodeWorkerState::OutputFull
        } else if !self.decoded_frames.is_empty() {
            AudioDecodeWorkerState::HaveFrame
        } else if self.in_flight_packets > 0 {
            AudioDecodeWorkerState::Decoding
        } else {
            AudioDecodeWorkerState::NeedPacket
        }
    }
}

impl Drop for AudioDecodeWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(AudioDecodeCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_audio_decode_worker(
    decoder: Decoder,
    output_rate: c_int,
    output_channels: c_int,
    command_rx: mpsc::Receiver<AudioDecodeCommand>,
    result_tx: mpsc::SyncSender<AudioDecodeResult>,
) {
    let mut frame = match AvFrame::new() {
        Ok(frame) => frame,
        Err(error) => {
            tracing::error!(%error, "failed to initialize FFmpeg audio decode worker frame");
            return;
        }
    };
    let mut resampler = match AudioResampler::new(output_rate, output_channels) {
        Ok(resampler) => resampler,
        Err(error) => {
            tracing::error!(%error, "failed to initialize FFmpeg audio decode worker resampler");
            return;
        }
    };

    while let Ok(command) = command_rx.recv() {
        match command {
            AudioDecodeCommand::Decode { generation, packet } => {
                let started = Instant::now();
                let mut decoded_frames = 0u64;
                let result = decoder.decode_packet(packet.as_ptr(), &mut frame, |frame| {
                    let raw_timestamp = frame_best_effort_timestamp(frame);
                    if let Some(audio) = resampler.convert(frame)? {
                        decoded_frames = decoded_frames.saturating_add(1);
                        result_tx
                            .send(AudioDecodeResult::Frame {
                                generation,
                                frame: AudioDecodedFrame {
                                    audio,
                                    raw_timestamp,
                                },
                            })
                            .map_err(|_| {
                                "FFmpeg audio decode result receiver stopped".to_string()
                            })?;
                    }
                    Ok(())
                });
                if result_tx
                    .send(AudioDecodeResult::PacketDone {
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
            AudioDecodeCommand::FlushBuffers { generation } => {
                decoder.flush_buffers();
                frame.unref();
                if result_tx
                    .send(AudioDecodeResult::Flushed { generation })
                    .is_err()
                {
                    break;
                }
            }
            AudioDecodeCommand::Drain { generation } => {
                let started = Instant::now();
                let mut decoded_frames = 0u64;
                let result = decoder.flush(&mut frame, |frame| {
                    let raw_timestamp = frame_best_effort_timestamp(frame);
                    if let Some(audio) = resampler.convert(frame)? {
                        decoded_frames = decoded_frames.saturating_add(1);
                        result_tx
                            .send(AudioDecodeResult::Frame {
                                generation,
                                frame: AudioDecodedFrame {
                                    audio,
                                    raw_timestamp,
                                },
                            })
                            .map_err(|_| {
                                "FFmpeg audio decode result receiver stopped".to_string()
                            })?;
                    }
                    Ok(())
                });
                if result_tx
                    .send(AudioDecodeResult::Drained {
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
            AudioDecodeCommand::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::mpsc, time::Duration};

    use ffmpeg_sys_next as ffi;

    use super::{
        AUDIO_DECODE_QUEUE_LIMIT_DURATION, AudioDecodeCommand, AudioDecodeEnqueueResult,
        AudioDecodeResult, AudioDecodeWorker, AudioDecodeWorkerInfo, AudioDecodeWorkerState,
        AvPacket, duration_nsecs,
    };

    fn worker_for_test() -> AudioDecodeWorker {
        worker_with_channels(1, 1).0
    }

    fn worker_with_channels(
        command_capacity: usize,
        result_capacity: usize,
    ) -> (
        AudioDecodeWorker,
        mpsc::SyncSender<AudioDecodeResult>,
        mpsc::Receiver<AudioDecodeCommand>,
    ) {
        let (command_tx, command_rx) = mpsc::sync_channel(command_capacity);
        let (result_tx, result_rx) = mpsc::sync_channel(result_capacity);
        (
            AudioDecodeWorker {
                command_tx,
                result_rx,
                handle: None,
                info: AudioDecodeWorkerInfo {
                    stream_index: 1,
                    time_base: ffi::AVRational { num: 1, den: 1000 },
                    output_rate: 48_000,
                    output_channels: 2,
                },
                decoded_frames: VecDeque::new(),
                completed_packets: VecDeque::new(),
                decoded_duration_nsecs: 0,
                decoded_duration_limit_nsecs: duration_nsecs(AUDIO_DECODE_QUEUE_LIMIT_DURATION),
                in_flight_packets: 0,
                flush_generation: None,
                flush_command_sent: false,
                drain_generation: None,
                drain_command_sent: false,
                drain_frames: Vec::new(),
                completed_drains: VecDeque::new(),
                draining: false,
                recovering: false,
                recovery_started_at: None,
                last_result_progress_at: None,
                stale_results_discarded: 0,
                eof: false,
            },
            result_tx,
            command_rx,
        )
    }

    #[test]
    fn service_pumps_full_queue_recovery_after_tracked_packets_are_cleared() {
        let (mut worker, result_tx, command_rx) = worker_with_channels(4, 8);
        let recovery_generation = 9;
        for generation in 1..=4 {
            worker
                .command_tx
                .try_send(AudioDecodeCommand::FlushBuffers { generation })
                .unwrap();
        }
        worker.in_flight_packets = 4;

        worker.flush_buffers(recovery_generation).unwrap();
        let pending_snapshot = worker.snapshot();
        assert_eq!(pending_snapshot.state, AudioDecodeWorkerState::Recovering);
        assert_eq!(
            pending_snapshot.recovery_generation,
            Some(recovery_generation)
        );
        assert!(!pending_snapshot.flush_command_sent);

        let _ = command_rx.try_recv().expect("one command slot is released");
        worker.service().unwrap();
        assert!(worker.snapshot().flush_command_sent);

        for generation in 1..=4 {
            result_tx
                .send(AudioDecodeResult::PacketDone {
                    generation,
                    result: Ok(()),
                    decoded_frames: 0,
                    elapsed: Duration::from_millis(1),
                })
                .unwrap();
        }
        result_tx
            .send(AudioDecodeResult::Flushed {
                generation: recovery_generation,
            })
            .unwrap();
        worker.service().unwrap();

        let recovered_snapshot = worker.snapshot();
        assert_eq!(recovered_snapshot.state, AudioDecodeWorkerState::NeedPacket);
        assert_eq!(recovered_snapshot.in_flight_packets, 0);
        assert_eq!(recovered_snapshot.stale_results_discarded, 4);

        while command_rx.try_recv().is_ok() {}
        let packet = AvPacket::new().expect("packet allocates");
        assert_eq!(
            worker
                .try_enqueue_packet(&packet, recovery_generation + 1)
                .unwrap(),
            AudioDecodeEnqueueResult::Queued
        );
    }

    #[test]
    fn stale_flush_ack_does_not_complete_current_audio_recovery() {
        let (mut worker, result_tx, _command_rx) = worker_with_channels(4, 4);
        let recovery_generation = 9;
        worker.flush_buffers(recovery_generation).unwrap();

        result_tx
            .send(AudioDecodeResult::Flushed {
                generation: recovery_generation - 1,
            })
            .unwrap();
        worker.service().unwrap();
        let stale_snapshot = worker.snapshot();
        assert_eq!(stale_snapshot.state, AudioDecodeWorkerState::Recovering);
        assert_eq!(
            stale_snapshot.recovery_generation,
            Some(recovery_generation)
        );
        assert_eq!(stale_snapshot.stale_results_discarded, 1);

        result_tx
            .send(AudioDecodeResult::Flushed {
                generation: recovery_generation,
            })
            .unwrap();
        worker.service().unwrap();
        assert_eq!(worker.snapshot().state, AudioDecodeWorkerState::NeedPacket);
    }

    #[test]
    fn decoded_audio_queue_limit_has_rebuffer_headroom() {
        let worker = worker_for_test();

        assert_eq!(
            worker.snapshot().duration_limit_nsecs,
            duration_nsecs(AUDIO_DECODE_QUEUE_LIMIT_DURATION)
        );
    }

    #[test]
    fn decoded_audio_queue_reports_full_at_limit() {
        let mut worker = worker_for_test();
        worker.decoded_duration_nsecs = duration_nsecs(AUDIO_DECODE_QUEUE_LIMIT_DURATION);

        assert!(worker.output_full());
        assert_eq!(worker.state(), AudioDecodeWorkerState::OutputFull);
    }
}
