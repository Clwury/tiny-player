use std::{
    collections::VecDeque,
    env,
    ffi::{CStr, CString},
    io::Read,
    os::raw::{c_int, c_void},
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    FromSample, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ffmpeg_sys_next as ffi;

use super::{BackendError, BackendEvent, HttpStreamBufferProgress, Result};
use crate::player::{
    dovi::{DoviFrameMetadata, DoviRpuExtractor, HevcStreamFormat},
    render_host::{
        DecodedFrame, FrameColor, FrameDynamicMetadata, FramePixels, FramePts, FrameSlot,
        RawVideoChromaSite, RawVideoFormat, RawVideoFrame, RawVideoPlane, RawVideoPlanes,
        RawVideoRange, RenderSize,
    },
};

mod avio;
mod codec;
mod format;

use codec::{AudioResampler, AvFrame, AvPacket, Decoder, VideoScaler};
use format::{FormatContext, StreamInfo};

#[cfg(test)]
use avio::{
    HttpRingCacheState, content_len_from_content_range, ffmpeg_http_headers,
    http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
    http_cache_request_headers_for_log, http_cache_response_headers_for_log, reqwest_header_pairs,
    should_cache_http_url,
};

const FALLBACK_AUDIO_OUTPUT_CHANNELS: c_int = 2;
const POSITION_QUERY_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_VIDEO_FRAME_DURATION_NSECS: u64 = 1_000_000_000 / 24;
const SCHEDULER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const RPU_MATCH_TOLERANCE: Duration = Duration::from_millis(60);
const RPU_QUEUE_CAPACITY: usize = 2048;
const AUDIO_BUFFER_SECONDS: usize = 4;
const AUDIO_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(300);
const AUDIO_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(120);
const LATE_VIDEO_DROP_TOLERANCE: Duration = Duration::from_millis(75);
const HTTP_RING_CACHE_CAPACITY: usize = 500 * 1024 * 1024;
const HTTP_CACHE_CHUNK_SIZE: usize = 256 * 1024;
const HTTP_CACHE_RANGE_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const HTTP_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(50);
const HTTP_CACHE_CONTENT_LEN_WAIT: Duration = Duration::from_secs(1);
const HTTP_CACHE_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES: u64 = 2 * 1024 * 1024;
const HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const HTTP_CACHE_PROGRESS_REPORT_THRESHOLD: f64 = 0.001;
const FFMPEG_AVIO_BUFFER_SIZE: c_int = 256 * 1024;
const FFMPEG_FAST_PROBE_SIZE: usize = 1024 * 1024;
const FFMPEG_FAST_ANALYZE_DURATION_US: u64 = 1_000_000;

static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputProbeProfile {
    Fast,
    Full,
}

pub struct FfmpegBackend {
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    event_rx: Receiver<BackendEvent>,
    worker: Option<FfmpegWorker>,
    current_url: Option<String>,
    loaded: bool,
    paused: bool,
}

impl FfmpegBackend {
    pub fn new() -> Result<Self> {
        init_ffmpeg_network()?;

        let frame_slot = FrameSlot::default();
        let (event_tx, event_rx) = mpsc::channel();

        Ok(Self {
            frame_slot,
            event_tx,
            event_rx,
            worker: None,
            current_url: None,
            loaded: false,
            paused: true,
        })
    }

    pub fn frame_slot(&self) -> FrameSlot {
        self.frame_slot.clone()
    }

    pub fn load_url(
        &mut self,
        url: &str,
        http_headers: Vec<(String, String)>,
        content_length: Option<u64>,
    ) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::EmptyUrl);
        }

        self.stop_worker();
        self.frame_slot.clear();
        self.current_url = Some(url.to_string());
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        let _ = self
            .event_tx
            .send(BackendEvent::HttpStreamBufferedChanged(None));

        self.worker = Some(FfmpegWorker::spawn(
            FfmpegPlaybackInput {
                url: url.to_string(),
                http_headers,
                content_length,
                start_position_seconds: 0.0,
            },
            self.frame_slot.clone(),
            self.event_tx.clone(),
        )?);
        Ok(())
    }

    pub fn seek_to(&mut self, position_seconds: f64) -> Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可跳转的媒体".to_string(),
            ));
        };
        let position_seconds = position_seconds.max(0.0);

        self.frame_slot.clear();
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        worker.seek(position_seconds)?;
        let _ = self
            .event_tx
            .send(BackendEvent::PositionChanged(position_seconds));
        let _ = self
            .event_tx
            .send(BackendEvent::BufferedChanged(Some(position_seconds)));
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if matches!(event, BackendEvent::Pause(true)) {
                self.paused = true;
            }
            events.push(event);
        }

        if let Some(size) = self.frame_slot.take_size_change() {
            if !self.loaded {
                self.loaded = true;
                self.paused = false;
                events.push(BackendEvent::PlaybackRestart);
                events.push(BackendEvent::Pause(false));
                events.push(BackendEvent::Buffering(false));
            }
            events.push(BackendEvent::VideoSizeChanged(Some(size)));
        }

        events
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
    }
}

impl Drop for FfmpegBackend {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop_async();
        }
        self.frame_slot.clear();
    }
}

struct FfmpegWorker {
    control: Arc<FfmpegControl>,
    command_tx: Sender<FfmpegCommand>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
struct FfmpegControl {
    shutdown: AtomicBool,
    seek_generation: AtomicU64,
    handled_seek_generation: AtomicU64,
}

impl FfmpegControl {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            seek_generation: AtomicU64::new(0),
            handled_seek_generation: AtomicU64::new(0),
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn should_interrupt(&self) -> bool {
        self.should_stop() || self.has_pending_seek()
    }

    fn request_seek(&self) -> u64 {
        self.seek_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn finish_seek(&self, generation: u64) {
        let mut current = self.handled_seek_generation.load(Ordering::Acquire);
        while generation > current {
            match self.handled_seek_generation.compare_exchange_weak(
                current,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    fn has_pending_seek(&self) -> bool {
        self.seek_generation.load(Ordering::Acquire)
            > self.handled_seek_generation.load(Ordering::Acquire)
    }
}

struct FfmpegPlaybackInput {
    url: String,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
    start_position_seconds: f64,
}

enum FfmpegCommand {
    Seek {
        position_seconds: f64,
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingSeek {
    position_seconds: f64,
    generation: u64,
}

impl FfmpegWorker {
    fn spawn(
        input: FfmpegPlaybackInput,
        frame_slot: FrameSlot,
        event_tx: Sender<BackendEvent>,
    ) -> Result<Self> {
        let control = Arc::new(FfmpegControl::new());
        let (command_tx, command_rx) = mpsc::channel();
        let frame_presented = Arc::new(AtomicBool::new(false));
        let worker_control = Arc::clone(&control);
        let worker_presented = Arc::clone(&frame_presented);

        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-backend".to_string())
            .spawn(move || {
                let result = run_ffmpeg_playback(
                    input,
                    frame_slot,
                    event_tx.clone(),
                    worker_control.clone(),
                    command_rx,
                    worker_presented.clone(),
                );

                if worker_control.should_stop() {
                    return;
                }

                match result {
                    Ok(()) => {
                        let _ = event_tx.send(BackendEvent::Pause(true));
                    }
                    Err(error) if worker_presented.load(Ordering::Relaxed) => {
                        tracing::error!(%error, "FFmpeg playback worker failed");
                        let _ = event_tx.send(BackendEvent::Fatal(error));
                    }
                    Err(error) => {
                        tracing::error!(%error, "FFmpeg playback load failed");
                        let _ = event_tx.send(BackendEvent::LoadFailed(error));
                    }
                }
            })
            .map_err(|error| BackendError::Ffmpeg(format!("创建 FFmpeg 解码线程失败：{error}")))?;

        Ok(Self {
            control,
            command_tx,
            handle,
        })
    }

    fn seek(&self, position_seconds: f64) -> Result<()> {
        let generation = self.control.request_seek();
        self.command_tx
            .send(FfmpegCommand::Seek {
                position_seconds,
                generation,
            })
            .map_err(|_| {
                self.control.finish_seek(generation);
                BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
            })?;
        Ok(())
    }

    fn stop(self) {
        let Self {
            control,
            command_tx: _,
            handle,
        } = self;
        control.shutdown();
        let _ = handle.join();
    }

    fn stop_async(self) {
        let Self {
            control,
            command_tx: _,
            handle,
        } = self;
        control.shutdown();
        let _ = thread::Builder::new()
            .name("tiny-ffmpeg-stop".to_string())
            .spawn(move || {
                let _ = handle.join();
            });
    }
}

fn init_ffmpeg_network() -> Result<()> {
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }

    let result = unsafe { ffi::avformat_network_init() };
    if result < 0 {
        return Err(BackendError::Ffmpeg(format!(
            "初始化 FFmpeg 网络层失败：{}",
            ffmpeg_error(result)
        )));
    }
    INITIALIZED.store(true, Ordering::Relaxed);
    Ok(())
}

struct OpenedPlaybackInput {
    input: FormatContext,
    video_stream: StreamInfo,
    video_decoder: Decoder,
    audio_stream: Option<StreamInfo>,
    audio_decoder: Option<Decoder>,
}

fn open_playback_input_with_fallback(
    source: &FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<OpenedPlaybackInput, String> {
    match open_playback_input(
        source,
        Arc::clone(&control),
        event_tx,
        InputProbeProfile::Fast,
        false,
    ) {
        Ok(opened) if opened.audio_stream.is_some() => Ok(opened),
        Ok(opened) => {
            tracing::debug!("FFmpeg fast probe did not find audio stream; retrying full probe");
            match open_playback_input(source, control, event_tx, InputProbeProfile::Full, true) {
                Ok(opened) => Ok(opened),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "FFmpeg full probe fallback failed; continuing with fast probe result"
                    );
                    Ok(opened)
                }
            }
        }
        Err(fast_error) => {
            tracing::debug!(%fast_error, "FFmpeg fast probe failed; retrying full probe");
            open_playback_input(source, control, event_tx, InputProbeProfile::Full, true).map_err(
                |full_error| {
                    format!("FFmpeg 快速探测失败：{fast_error}；完整探测也失败：{full_error}")
                },
            )
        }
    }
}

fn open_playback_input(
    source: &FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
    probe_profile: InputProbeProfile,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<OpenedPlaybackInput, String> {
    let mut input = FormatContext::open(
        &source.url,
        source.http_headers.as_slice(),
        source.content_length,
        probe_profile,
        Arc::clone(&control),
        event_tx.clone(),
    )?;
    input.find_stream_info()?;

    let video_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)?
        .ok_or_else(|| "FFmpeg 未找到可解码视频流".to_string())?;
    let video_decoder = Decoder::open(video_stream)
        .map_err(|error| format!("FFmpeg 打开视频解码器失败：{error}"))?;
    let (audio_stream, audio_decoder) = open_audio_decoder(&input, allow_audio_decoder_failure)?;

    Ok(OpenedPlaybackInput {
        input,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder,
    })
}

fn open_audio_decoder(
    input: &FormatContext,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<(Option<StreamInfo>, Option<Decoder>), String> {
    let audio_stream = match input.best_stream(ffi::AVMediaType::AVMEDIA_TYPE_AUDIO) {
        Ok(stream) => stream,
        Err(error) if allow_audio_decoder_failure => {
            tracing::warn!(%error, "FFmpeg audio stream selection failed");
            None
        }
        Err(error) => return Err(format!("FFmpeg 选择音频流失败：{error}")),
    };
    let Some(stream) = audio_stream else {
        return Ok((None, None));
    };

    match Decoder::open(stream) {
        Ok(decoder) => Ok((Some(stream), Some(decoder))),
        Err(error) if allow_audio_decoder_failure => {
            tracing::warn!(%error, "FFmpeg audio decoder initialization failed");
            Ok((Some(stream), None))
        }
        Err(error) => Err(format!("FFmpeg 打开音频解码器失败：{error}")),
    }
}

fn run_ffmpeg_playback(
    source: FfmpegPlaybackInput,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    control: Arc<FfmpegControl>,
    command_rx: Receiver<FfmpegCommand>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let OpenedPlaybackInput {
        mut input,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder: opened_audio_decoder,
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    if source.start_position_seconds > 0.0 {
        input.seek_stream(video_stream, source.start_position_seconds)?;
    }
    let mut video_frame = AvFrame::new()?;
    let mut video_converter = VideoFrameConverter::new();
    let mut current_start_position_nsecs = seconds_to_nsecs(source.start_position_seconds);
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let mut video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    let mut scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    let mut position_reporter = PositionReporter::default();
    let mut dovi_queue = DoviMetadataQueue::default();

    let mut audio_output = None;
    let mut audio_decoder = None;
    let mut audio_frame = None;
    let mut audio_resampler = None;
    if let Some(decoder) = opened_audio_decoder {
        match AudioOutput::new() {
            Ok(output) => match AudioResampler::new(output.sample_rate(), output.channels()) {
                Ok(resampler) => {
                    tracing::debug!(
                        sample_rate = output.sample_rate(),
                        channels = output.channels(),
                        "initialized native FFmpeg audio output"
                    );
                    audio_frame = Some(AvFrame::new()?);
                    audio_resampler = Some(resampler);
                    audio_output = Some(output);
                    audio_decoder = Some(decoder);
                }
                Err(error) => {
                    tracing::warn!(%error, "FFmpeg audio resampler initialization failed");
                }
            },
            Err(error) => {
                tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
            }
        }
    }
    let mut audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );
    if let Some(output) = &audio_output {
        output.reset_clock(current_start_position_nsecs);
    }

    if let Some(duration) = input.duration_seconds() {
        let _ = event_tx.send(BackendEvent::DurationChanged(duration));
    }
    let duration_seconds = input.duration_seconds();

    let mut packet = AvPacket::new()?;
    let mut buffered_reporter = BufferedReporter::new(audio_output.is_some());
    let mut queued_video_frames = VecDeque::new();
    let mut first_video_frame_pending = true;
    buffered_reporter.reset_to(source.start_position_seconds.max(0.0), &event_tx);
    let _ = event_tx.send(BackendEvent::Buffering(true));

    while !control.should_stop() {
        if let Some(pending_seek) = drain_seek_command(&command_rx) {
            control.finish_seek(pending_seek.generation);
            let seek_result: std::result::Result<(), String> = (|| {
                let position_seconds = pending_seek.position_seconds.max(0.0);
                current_start_position_nsecs = seconds_to_nsecs(position_seconds);
                input.seek_stream(video_stream, position_seconds)?;
                video_decoder.flush_buffers();
                if let Some(decoder) = &audio_decoder {
                    decoder.flush_buffers();
                }
                video_frame.unref();
                if let Some(frame) = audio_frame.as_mut() {
                    frame.unref();
                }
                packet.unref();
                video_clock = TimestampMapper::new(
                    video_stream.start_nsecs,
                    current_start_position_nsecs,
                    Some(video_frame_duration_nsecs),
                );
                audio_clock = TimestampMapper::new(
                    audio_stream.and_then(|stream| stream.start_nsecs),
                    current_start_position_nsecs,
                    None,
                );
                scheduler.reset(current_start_position_nsecs);
                if let Some(output) = &audio_output {
                    output.reset_clock(current_start_position_nsecs);
                }
                queued_video_frames.clear();
                first_video_frame_pending = true;
                dovi_queue.clear();
                buffered_reporter = BufferedReporter::new(audio_output.is_some());
                buffered_reporter.reset_to(position_seconds, &event_tx);
                let _ = event_tx.send(BackendEvent::PositionChanged(position_seconds));
                let _ = event_tx.send(BackendEvent::Buffering(true));
                Ok(())
            })();
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            seek_result?;
            continue;
        }

        if control.has_pending_seek() {
            thread::yield_now();
            continue;
        }

        let read = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        if read == ffi::AVERROR_EOF {
            break;
        }
        if read < 0 {
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            return Err(format!("FFmpeg 读取媒体包失败：{}", ffmpeg_error(read)));
        }

        let process_result = match packet.stream_index() {
            index if index == video_decoder.stream_index => {
                dovi_queue.observe_packet(&packet, video_stream);
                video_decoder.decode_packet(packet.as_ptr(), &mut video_frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base);
                    let frame_pts = FramePts {
                        nsecs: timestamp.timeline_nsecs,
                    };
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        let _ = dovi_queue.take_for_frame(frame_pts);
                        return Ok(());
                    }

                    if let Some(output) = audio_output.as_ref() {
                        if !first_video_frame_pending
                            && should_drop_late_video_frame(
                                timestamp.timeline_nsecs,
                                video_frame_duration_nsecs,
                                output.played_timeline_nsecs(),
                            )
                        {
                            let _ = dovi_queue.take_for_frame(frame_pts);
                            return Ok(());
                        }

                        let dovi_metadata = dovi_metadata_from_frame(frame)
                            .or_else(|| dovi_queue.take_for_frame(frame_pts));
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);

                        if first_video_frame_pending {
                            present_decoded_video_frame(
                                decoded_frame,
                                timestamp.timeline_nsecs,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
                                &event_tx,
                            );
                            buffered_reporter.report_video_timeline_nsecs(
                                timestamp
                                    .timeline_nsecs
                                    .saturating_add(video_frame_duration_nsecs),
                                &event_tx,
                            );
                            first_video_frame_pending = false;
                            return Ok(());
                        }
                        queued_video_frames.push_back(QueuedVideoFrame {
                            frame: decoded_frame,
                            timeline_nsecs: timestamp.timeline_nsecs,
                        });
                        buffered_reporter.report_video_timeline_nsecs(
                            timestamp
                                .timeline_nsecs
                                .saturating_add(video_frame_duration_nsecs),
                            &event_tx,
                        );
                        present_due_audio_clocked_video_frames(
                            &mut queued_video_frames,
                            output,
                            &frame_slot,
                            &frame_presented,
                            &mut position_reporter,
                            &event_tx,
                        );
                        if queued_video_duration(&queued_video_frames)
                            >= AUDIO_VIDEO_QUEUE_LIMIT_DURATION
                        {
                            wait_for_audio_clocked_video_queue(
                                &mut queued_video_frames,
                                output,
                                &control,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
                                &event_tx,
                            )?;
                        }
                    } else {
                        let dovi_metadata = dovi_metadata_from_frame(frame)
                            .or_else(|| dovi_queue.take_for_frame(frame_pts));
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);

                        first_video_frame_pending = false;
                        if scheduler
                            .wait_until(timestamp.timeline_nsecs, &control)
                            .interrupted()
                        {
                            return Ok(());
                        }
                        if control.has_pending_seek() {
                            return Ok(());
                        }
                        present_decoded_video_frame(
                            decoded_frame,
                            timestamp.timeline_nsecs,
                            &frame_slot,
                            &frame_presented,
                            &mut position_reporter,
                            &event_tx,
                        );
                        buffered_reporter.report_video_timeline_nsecs(
                            timestamp
                                .timeline_nsecs
                                .saturating_add(video_frame_duration_nsecs),
                            &event_tx,
                        );
                    }
                    Ok(())
                })
            }
            index
                if audio_decoder
                    .as_ref()
                    .is_some_and(|decoder| index == decoder.stream_index) =>
            {
                let decoder = audio_decoder.as_ref().expect("audio decoder checked above");
                let frame = audio_frame
                    .as_mut()
                    .expect("audio frame exists with audio decoder");
                let resampler = audio_resampler
                    .as_mut()
                    .expect("audio resampler exists with audio decoder");
                let output = audio_output
                    .as_ref()
                    .expect("audio output exists with audio decoder");
                decoder.decode_packet(packet.as_ptr(), frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp =
                        audio_clock.map(frame_best_effort_timestamp(frame), decoder.time_base);
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        return Ok(());
                    }
                    if let Some(audio) = resampler.convert(frame)? {
                        if control.has_pending_seek() {
                            return Ok(());
                        }
                        let buffered_until_nsecs = timestamp
                            .timeline_nsecs
                            .saturating_add(audio.duration_nsecs);
                        output.push(audio.samples, &control, || {
                            present_due_audio_clocked_video_frames(
                                &mut queued_video_frames,
                                output,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
                                &event_tx,
                            );
                            Ok(())
                        })?;
                        buffered_reporter
                            .report_audio_timeline_nsecs(buffered_until_nsecs, &event_tx);
                    }
                    Ok(())
                })
            }
            _ => Ok(()),
        };
        packet.unref();
        if let Err(error) = process_result {
            if control.has_pending_seek() {
                continue;
            }
            return Err(error);
        }
        if control.has_pending_seek() {
            continue;
        }
    }

    if control.should_stop() {
        return Ok(());
    }

    video_decoder.flush(&mut video_frame, |frame| {
        let timestamp =
            video_clock.map(frame_best_effort_timestamp(frame), video_decoder.time_base);
        if timestamp.timeline_nsecs < current_start_position_nsecs {
            return Ok(());
        }
        let frame_pts = FramePts {
            nsecs: timestamp.timeline_nsecs,
        };
        if let Some(output) = audio_output.as_ref() {
            if !first_video_frame_pending
                && should_drop_late_video_frame(
                    timestamp.timeline_nsecs,
                    video_frame_duration_nsecs,
                    output.played_timeline_nsecs(),
                )
            {
                let _ = dovi_queue.take_for_frame(frame_pts);
                return Ok(());
            }

            let dovi_metadata =
                dovi_metadata_from_frame(frame).or_else(|| dovi_queue.take_for_frame(frame_pts));
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);

            if first_video_frame_pending {
                present_decoded_video_frame(
                    decoded_frame,
                    timestamp.timeline_nsecs,
                    &frame_slot,
                    &frame_presented,
                    &mut position_reporter,
                    &event_tx,
                );
                buffered_reporter.report_video_timeline_nsecs(
                    timestamp
                        .timeline_nsecs
                        .saturating_add(video_frame_duration_nsecs),
                    &event_tx,
                );
                first_video_frame_pending = false;
                return Ok(());
            }
            queued_video_frames.push_back(QueuedVideoFrame {
                frame: decoded_frame,
                timeline_nsecs: timestamp.timeline_nsecs,
            });
            buffered_reporter.report_video_timeline_nsecs(
                timestamp
                    .timeline_nsecs
                    .saturating_add(video_frame_duration_nsecs),
                &event_tx,
            );
            present_due_audio_clocked_video_frames(
                &mut queued_video_frames,
                output,
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
        } else {
            let dovi_metadata =
                dovi_metadata_from_frame(frame).or_else(|| dovi_queue.take_for_frame(frame_pts));
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);

            first_video_frame_pending = false;
            if scheduler
                .wait_until(timestamp.timeline_nsecs, &control)
                .interrupted()
            {
                return Ok(());
            }
            present_decoded_video_frame(
                decoded_frame,
                timestamp.timeline_nsecs,
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
            buffered_reporter.report_video_timeline_nsecs(
                timestamp
                    .timeline_nsecs
                    .saturating_add(video_frame_duration_nsecs),
                &event_tx,
            );
        }
        Ok(())
    })?;

    if let (Some(decoder), Some(frame), Some(resampler), Some(output)) = (
        audio_decoder.as_ref(),
        audio_frame.as_mut(),
        audio_resampler.as_mut(),
        audio_output.as_ref(),
    ) {
        decoder.flush(frame, |frame| {
            let timestamp = audio_clock.map(frame_best_effort_timestamp(frame), decoder.time_base);
            if timestamp.timeline_nsecs < current_start_position_nsecs {
                return Ok(());
            }
            if let Some(audio) = resampler.convert(frame)? {
                let buffered_until_nsecs = timestamp
                    .timeline_nsecs
                    .saturating_add(audio.duration_nsecs);
                output.push(audio.samples, &control, || {
                    present_due_audio_clocked_video_frames(
                        &mut queued_video_frames,
                        output,
                        &frame_slot,
                        &frame_presented,
                        &mut position_reporter,
                        &event_tx,
                    );
                    Ok(())
                })?;
                buffered_reporter.report_audio_timeline_nsecs(buffered_until_nsecs, &event_tx);
            }
            Ok(())
        })?;
    }

    buffered_reporter.report_value(duration_seconds, &event_tx);
    if let Some(output) = &audio_output {
        drain_audio_clocked_video_queue(
            &mut queued_video_frames,
            output,
            &control,
            &frame_slot,
            &frame_presented,
            &mut position_reporter,
            &event_tx,
        )?;
        output.drain(&control)?;
    }
    Ok(())
}

fn drain_seek_command(command_rx: &Receiver<FfmpegCommand>) -> Option<PendingSeek> {
    let mut pending_seek = None;
    while let Ok(command) = command_rx.try_recv() {
        match command {
            FfmpegCommand::Seek {
                position_seconds,
                generation,
            } => {
                pending_seek = Some(PendingSeek {
                    position_seconds: position_seconds.max(0.0),
                    generation,
                });
            }
        }
    }
    pending_seek
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitStatus {
    Ready,
    Interrupted,
}

impl WaitStatus {
    fn interrupted(self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

struct QueuedVideoFrame {
    frame: DecodedFrame,
    timeline_nsecs: u64,
}

fn present_due_audio_clocked_video_frames(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    audio_output: &AudioOutput,
    frame_slot: &FrameSlot,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) {
    let played_until = audio_output.played_timeline_nsecs();
    let mut due_frame = None;
    while queued_video_frames
        .front()
        .is_some_and(|frame| frame.timeline_nsecs <= played_until)
    {
        due_frame = Some(
            queued_video_frames
                .pop_front()
                .expect("queued video frame checked above"),
        );
    }
    if let Some(frame) = due_frame {
        present_decoded_video_frame(
            frame.frame,
            frame.timeline_nsecs,
            frame_slot,
            frame_presented,
            position_reporter,
            event_tx,
        );
    }
}

fn queued_video_duration(queued_video_frames: &VecDeque<QueuedVideoFrame>) -> Duration {
    match (queued_video_frames.front(), queued_video_frames.back()) {
        (Some(first), Some(last)) => {
            Duration::from_nanos(last.timeline_nsecs.saturating_sub(first.timeline_nsecs))
        }
        _ => Duration::ZERO,
    }
}

fn should_drop_late_video_frame(
    frame_timeline_nsecs: u64,
    frame_duration_nsecs: u64,
    played_until_nsecs: u64,
) -> bool {
    let late_cutoff = frame_timeline_nsecs
        .saturating_add(frame_duration_nsecs)
        .saturating_add(duration_nsecs(LATE_VIDEO_DROP_TOLERANCE));
    late_cutoff <= played_until_nsecs
}

fn wait_for_audio_clocked_video_queue(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    audio_output: &AudioOutput,
    control: &FfmpegControl,
    frame_slot: &FrameSlot,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<(), String> {
    while queued_video_duration(queued_video_frames) >= AUDIO_VIDEO_QUEUE_TARGET_DURATION
        && !control.should_interrupt()
    {
        present_due_audio_clocked_video_frames(
            queued_video_frames,
            audio_output,
            frame_slot,
            frame_presented,
            position_reporter,
            event_tx,
        );
        if queued_video_duration(queued_video_frames) < AUDIO_VIDEO_QUEUE_TARGET_DURATION
            || audio_output.queued_duration()? == Duration::ZERO
        {
            break;
        }
        audio_output.wait_for_progress(control)?;
    }
    Ok(())
}

fn drain_audio_clocked_video_queue(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    audio_output: &AudioOutput,
    control: &FfmpegControl,
    frame_slot: &FrameSlot,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<(), String> {
    while !queued_video_frames.is_empty() && !control.should_interrupt() {
        present_due_audio_clocked_video_frames(
            queued_video_frames,
            audio_output,
            frame_slot,
            frame_presented,
            position_reporter,
            event_tx,
        );
        if queued_video_frames.is_empty() || audio_output.queued_duration()? == Duration::ZERO {
            break;
        }
        audio_output.wait_for_progress(control)?;
    }
    Ok(())
}

fn present_decoded_video_frame(
    frame: DecodedFrame,
    timeline_nsecs: u64,
    frame_slot: &FrameSlot,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
) {
    frame_slot.push(frame);
    let count = FFMPEG_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count.is_multiple_of(60) {
        tracing::debug!(
            frame_count = count,
            pts = timeline_nsecs,
            "presented FFmpeg video frame"
        );
    }
    frame_presented.store(true, Ordering::Relaxed);
    position_reporter.report(timeline_nsecs, event_tx);
}

struct PlaybackScheduler {
    start_instant: Instant,
    start_position_nsecs: u64,
}

impl PlaybackScheduler {
    fn new(start_position_nsecs: u64) -> Self {
        Self {
            start_instant: Instant::now(),
            start_position_nsecs,
        }
    }

    fn reset(&mut self, start_position_nsecs: u64) {
        self.start_instant = Instant::now();
        self.start_position_nsecs = start_position_nsecs;
    }

    fn wait_until(&self, timeline_nsecs: u64, control: &FfmpegControl) -> WaitStatus {
        let target_offset = timeline_nsecs.saturating_sub(self.start_position_nsecs);
        let target = self
            .start_instant
            .checked_add(Duration::from_nanos(target_offset))
            .unwrap_or(self.start_instant);
        loop {
            if control.should_interrupt() {
                return WaitStatus::Interrupted;
            }
            let now = Instant::now();
            if now >= target {
                return WaitStatus::Ready;
            }
            thread::sleep((target - now).min(SCHEDULER_POLL_INTERVAL));
        }
    }
}

struct VideoFrameConverter {
    scaler: Option<VideoScaler>,
}

impl VideoFrameConverter {
    fn new() -> Self {
        Self { scaler: None }
    }

    fn convert(
        &mut self,
        decoder: &Decoder,
        frame: *mut ffi::AVFrame,
        dovi_metadata: Option<DoviFrameMetadata>,
    ) -> std::result::Result<DecodedFrame, String> {
        let size = frame_size(frame)
            .or_else(|| self.scaler.as_ref().map(|scaler| scaler.size))
            .or_else(|| decoder.size().ok())
            .ok_or_else(|| "FFmpeg 视频帧缺少有效尺寸".to_string())?;
        if let Some(raw) = raw_video_frame_from_av_frame(frame, size, dovi_metadata)? {
            return Ok(DecodedFrame {
                size,
                pts: None,
                pixels: FramePixels::RawVideo(raw),
            });
        }

        let scaler = match self.scaler.as_mut() {
            Some(scaler) => scaler,
            None => self.scaler.insert(VideoScaler::new(decoder)?),
        };
        let pixels = scaler.convert(frame)?;
        Ok(DecodedFrame {
            size: scaler.size,
            pts: None,
            pixels: FramePixels::Bgra8(pixels),
        })
    }
}

fn dovi_metadata_from_frame(frame: *mut ffi::AVFrame) -> Option<DoviFrameMetadata> {
    let side_data = unsafe {
        ffi::av_frame_get_side_data(
            frame,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_DOVI_RPU_BUFFER,
        )
    };
    if side_data.is_null() {
        return None;
    }

    let (data, size) = unsafe { ((*side_data).data, (*side_data).size) };
    if data.is_null() || size == 0 {
        return None;
    }
    let data = unsafe { slice::from_raw_parts(data, size) };
    match DoviFrameMetadata::from_rpu_payload(data)
        .or_else(|_| DoviFrameMetadata::from_unspec62_nalu(data))
    {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            tracing::debug!(%error, "failed to parse Dolby Vision RPU from decoded frame side data");
            None
        }
    }
}

fn raw_video_frame_from_av_frame(
    frame: *mut ffi::AVFrame,
    size: RenderSize,
    dovi_metadata: Option<DoviFrameMetadata>,
) -> std::result::Result<Option<RawVideoFrame>, String> {
    let Some(format) = ffmpeg_raw_video_format(unsafe { (*frame).format }) else {
        return Ok(None);
    };
    let color = frame_color(frame, dovi_metadata.as_ref());
    let range = frame_range(frame);
    let chroma_site = frame_chroma_site(frame);
    let planes = copy_raw_video_planes(frame, format, size)?;
    let metadata = dovi_metadata.map(|dolby_vision| FrameDynamicMetadata {
        dolby_vision: Some(dolby_vision),
    });

    Ok(Some(RawVideoFrame {
        format,
        color,
        range,
        chroma_site,
        metadata,
        planes: RawVideoPlanes::Owned(planes),
    }))
}

fn ffmpeg_raw_video_format(format: c_int) -> Option<RawVideoFormat> {
    if format == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int {
        Some(RawVideoFormat::P010Le)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE as c_int {
        Some(RawVideoFormat::I42010Le)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int {
        Some(RawVideoFormat::Nv12)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int {
        Some(RawVideoFormat::I420)
    } else {
        None
    }
}

fn copy_raw_video_planes(
    frame: *mut ffi::AVFrame,
    format: RawVideoFormat,
    size: RenderSize,
) -> std::result::Result<Vec<RawVideoPlane>, String> {
    let mut planes = Vec::with_capacity(format.plane_count());
    for plane_index in 0..format.plane_count() {
        let layout = format
            .plane_layout(size, plane_index)
            .map_err(|error| error.to_string())?;
        let data = unsafe { (*frame).data[plane_index] };
        if data.is_null() {
            return Err("FFmpeg raw 视频帧缺少平面数据".to_string());
        }
        let stride = unsafe { (*frame).linesize[plane_index] };
        if stride <= 0 {
            return Err("FFmpeg raw 视频帧 stride 无效".to_string());
        }
        let stride =
            usize::try_from(stride).map_err(|_| "FFmpeg raw 视频帧 stride 无效".to_string())?;
        if stride < layout.row_len {
            return Err("FFmpeg raw 视频帧 stride 小于行宽".to_string());
        }
        let height = usize::try_from(layout.height).map_err(|_| "视频帧过高".to_string())?;
        let len = layout
            .row_len
            .checked_mul(height)
            .ok_or_else(|| "视频帧平面过大".to_string())?;
        let mut plane = Vec::with_capacity(len);
        for row in 0..height {
            let row_start = row
                .checked_mul(stride)
                .ok_or_else(|| "视频帧平面过大".to_string())?;
            let row_data = unsafe { slice::from_raw_parts(data.add(row_start), layout.row_len) };
            plane.extend_from_slice(row_data);
        }
        planes.push(RawVideoPlane {
            data: plane,
            stride: layout.row_len,
        });
    }
    Ok(planes)
}

fn frame_color(frame: *mut ffi::AVFrame, dovi_metadata: Option<&DoviFrameMetadata>) -> FrameColor {
    if dovi_metadata.is_some_and(|metadata| metadata.profile == 5) {
        return FrameColor::DolbyVisionProfile5;
    }

    let (primaries, transfer) = unsafe { ((*frame).color_primaries, (*frame).color_trc) };
    let is_bt2020 = matches!(
        primaries,
        ffi::AVColorPrimaries::AVCOL_PRI_BT2020 | ffi::AVColorPrimaries::AVCOL_PRI_SMPTE432
    );
    let is_hdr_transfer = matches!(
        transfer,
        ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
            | ffi::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67
    );
    if is_bt2020 && is_hdr_transfer {
        FrameColor::Hdr10Bt2020
    } else {
        FrameColor::Sdr
    }
}

fn frame_range(frame: *mut ffi::AVFrame) -> RawVideoRange {
    match unsafe { (*frame).color_range } {
        ffi::AVColorRange::AVCOL_RANGE_MPEG => RawVideoRange::Limited,
        ffi::AVColorRange::AVCOL_RANGE_JPEG => RawVideoRange::Full,
        _ => RawVideoRange::Unknown,
    }
}

fn frame_chroma_site(frame: *mut ffi::AVFrame) -> RawVideoChromaSite {
    match unsafe { (*frame).chroma_location } {
        ffi::AVChromaLocation::AVCHROMA_LOC_LEFT => RawVideoChromaSite::Left,
        ffi::AVChromaLocation::AVCHROMA_LOC_CENTER => RawVideoChromaSite::Center,
        ffi::AVChromaLocation::AVCHROMA_LOC_TOPLEFT => RawVideoChromaSite::TopLeft,
        ffi::AVChromaLocation::AVCHROMA_LOC_TOP => RawVideoChromaSite::TopCenter,
        _ => RawVideoChromaSite::Unknown,
    }
}

#[derive(Default)]
struct DoviMetadataQueue {
    extractor: DoviRpuExtractor,
    entries: VecDeque<DoviMetadataEntry>,
    first_packet_nsecs: Option<u64>,
}

struct DoviMetadataEntry {
    pts: Option<FramePts>,
    metadata: DoviFrameMetadata,
}

impl DoviMetadataQueue {
    fn observe_packet(&mut self, packet: &AvPacket, stream: StreamInfo) {
        if stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return;
        }
        let Some(data) = packet.data() else {
            return;
        };
        let metadata = match extract_dovi_metadata(&mut self.extractor, data) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => return,
            Err(error) => {
                tracing::trace!(%error, "ignored non-Dolby Vision RPU candidate in FFmpeg packet");
                return;
            }
        };
        let pts = packet.best_timestamp().and_then(|timestamp| {
            dovi_packet_timeline_nsecs(
                &mut self.first_packet_nsecs,
                stream.start_nsecs,
                timestamp,
                stream.time_base,
            )
            .map(|nsecs| FramePts { nsecs })
        });
        self.entries.push_back(DoviMetadataEntry { pts, metadata });
        while self.entries.len() > RPU_QUEUE_CAPACITY {
            self.entries.pop_front();
        }
    }

    fn take_for_frame(&mut self, pts: FramePts) -> Option<DoviFrameMetadata> {
        if self.entries.is_empty() {
            return None;
        }
        let tolerance = duration_nsecs(RPU_MATCH_TOLERANCE);
        let nearest = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .pts
                    .map(|entry_pts| (index, pts_distance(entry_pts, pts)))
            })
            .min_by_key(|(_, distance)| *distance);
        if let Some((index, distance)) = nearest
            && distance <= tolerance
        {
            return self.entries.remove(index).map(|entry| entry.metadata);
        }

        if self
            .entries
            .front()
            .is_some_and(|entry| entry.pts.is_none())
        {
            return self.entries.pop_front().map(|entry| entry.metadata);
        }
        None
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.first_packet_nsecs = None;
    }
}

fn dovi_packet_timeline_nsecs(
    first_packet_nsecs: &mut Option<u64>,
    stream_start_nsecs: Option<u64>,
    timestamp: i64,
    time_base: ffi::AVRational,
) -> Option<u64> {
    let nsecs = timestamp_to_nsecs(timestamp, time_base)?;
    if let Some(start_nsecs) = stream_start_nsecs {
        return Some(nsecs.saturating_sub(start_nsecs));
    }

    let first_nsecs = *first_packet_nsecs.get_or_insert(nsecs);
    Some(nsecs.saturating_sub(first_nsecs))
}

fn extract_dovi_metadata(
    extractor: &mut DoviRpuExtractor,
    data: &[u8],
) -> anyhow::Result<Option<DoviFrameMetadata>> {
    if has_annex_b_start_code(data) {
        return extractor.extract_from_hevc_access_unit(data, HevcStreamFormat::ByteStream);
    }

    let mut last_error = None;
    for length_size in [4, 2, 1] {
        match extractor
            .extract_from_hevc_access_unit(data, HevcStreamFormat::LengthPrefixed { length_size })
        {
            Ok(metadata) => return Ok(metadata),
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(None)
}

fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|window| window == [0, 0, 1])
        || data.windows(4).any(|window| window == [0, 0, 0, 1])
}

struct AudioOutput {
    shared: Arc<AudioShared>,
    _stream: cpal::Stream,
    sample_rate: c_int,
    channels: c_int,
}

struct AudioShared {
    buffer: Mutex<AudioBuffer>,
    ready: Condvar,
    played_samples: AtomicU64,
}

struct AudioBuffer {
    samples: VecDeque<f32>,
    max_samples: usize,
}

impl AudioOutput {
    fn new() -> std::result::Result<Self, String> {
        let host = cpal::default_host();
        let mut last_error = None;
        for candidate in output_device_candidates(&host)? {
            match Self::from_device(candidate.device, candidate.name.clone()) {
                Ok(output) => return Ok(output),
                Err(error) => {
                    tracing::warn!(
                        device = %candidate.name,
                        source = %candidate.source,
                        %error,
                        "native audio output device initialization failed"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "未找到系统音频输出设备".to_string()))
    }

    fn from_device(device: cpal::Device, device_name: String) -> std::result::Result<Self, String> {
        let supported_config = device
            .default_output_config()
            .map_err(|error| format!("读取系统音频输出配置失败：{error}"))?;
        let sample_rate = c_int::try_from(supported_config.sample_rate())
            .map_err(|_| "系统音频采样率过大".to_string())?;
        let channels = c_int::from(supported_config.channels());
        let max_samples = usize::try_from(sample_rate)
            .ok()
            .and_then(|rate| rate.checked_mul(usize::try_from(channels).ok()?))
            .and_then(|samples| samples.checked_mul(AUDIO_BUFFER_SECONDS))
            .ok_or_else(|| "系统音频缓冲区过大".to_string())?;
        let shared = Arc::new(AudioShared {
            buffer: Mutex::new(AudioBuffer {
                samples: VecDeque::with_capacity(max_samples),
                max_samples,
            }),
            ready: Condvar::new(),
            played_samples: AtomicU64::new(0),
        });
        let config: cpal::StreamConfig = supported_config.clone().into();
        let sample_format = supported_config.sample_format();
        tracing::debug!(
            device = %device_name,
            sample_rate,
            channels,
            ?sample_format,
            "selected native audio output config"
        );
        let stream = match sample_format {
            cpal::SampleFormat::I8 => {
                build_audio_output_stream::<i8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I16 => {
                build_audio_output_stream::<i16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I32 => {
                build_audio_output_stream::<i32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I64 => {
                build_audio_output_stream::<i64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U8 => {
                build_audio_output_stream::<u8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U16 => {
                build_audio_output_stream::<u16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U32 => {
                build_audio_output_stream::<u32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U64 => {
                build_audio_output_stream::<u64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F32 => {
                build_audio_output_stream::<f32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F64 => {
                build_audio_output_stream::<f64>(&device, &config, shared.clone())
            }
            sample_format => {
                return Err(format!("暂不支持的系统音频采样格式：{sample_format:?}"));
            }
        }
        .map_err(|error| format!("创建系统音频输出流失败：{error}"))?;
        stream
            .play()
            .map_err(|error| format!("启动系统音频输出流失败：{error}"))?;

        Ok(Self {
            shared,
            _stream: stream,
            sample_rate,
            channels,
        })
    }

    fn sample_rate(&self) -> c_int {
        self.sample_rate
    }

    fn channels(&self) -> c_int {
        self.channels
    }

    fn push<F>(
        &self,
        samples: Vec<f32>,
        control: &FfmpegControl,
        mut on_wait: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut() -> std::result::Result<(), String>,
    {
        let mut offset = 0;
        while offset < samples.len() {
            if control.should_interrupt() {
                return Ok(());
            }
            let mut guard = self
                .shared
                .buffer
                .lock()
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            while guard.samples.len() >= guard.max_samples && !control.should_interrupt() {
                let (next_guard, _) = self
                    .shared
                    .ready
                    .wait_timeout(guard, SCHEDULER_POLL_INTERVAL)
                    .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
                guard = next_guard;
                drop(guard);
                on_wait()?;
                guard = self
                    .shared
                    .buffer
                    .lock()
                    .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            }
            if control.should_interrupt() {
                return Ok(());
            }
            let capacity = guard.max_samples.saturating_sub(guard.samples.len());
            if capacity == 0 {
                continue;
            }
            let end = (offset + capacity).min(samples.len());
            guard.samples.extend(samples[offset..end].iter().copied());
            offset = end;
            self.shared.ready.notify_all();
            drop(guard);
            on_wait()?;
        }
        Ok(())
    }

    fn reset_clock(&self, timeline_nsecs: u64) {
        if let Ok(mut guard) = self.shared.buffer.lock() {
            guard.samples.clear();
            self.shared.played_samples.store(0, Ordering::Relaxed);
            self.shared.ready.notify_all();
        }
        self.shared.played_samples.store(
            samples_for_duration(timeline_nsecs, self.sample_rate, self.channels),
            Ordering::Relaxed,
        );
    }

    fn played_timeline_nsecs(&self) -> u64 {
        audio_samples_duration(
            usize::try_from(self.shared.played_samples.load(Ordering::Relaxed))
                .unwrap_or(usize::MAX),
            self.sample_rate,
            self.channels,
        )
        .as_nanos()
        .try_into()
        .unwrap_or(u64::MAX)
    }

    fn wait_for_progress(&self, control: &FfmpegControl) -> std::result::Result<(), String> {
        let previous = self.shared.played_samples.load(Ordering::Relaxed);
        let mut guard = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
        while self.shared.played_samples.load(Ordering::Relaxed) == previous
            && !guard.samples.is_empty()
            && !control.should_interrupt()
        {
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, SCHEDULER_POLL_INTERVAL)
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            guard = next_guard;
        }
        Ok(())
    }

    fn drain(&self, control: &FfmpegControl) -> std::result::Result<(), String> {
        let timeout = self
            .queued_duration()?
            .saturating_add(Duration::from_millis(250));
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Ok(());
        };

        let mut guard = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
        while !guard.samples.is_empty() && !control.should_interrupt() {
            let now = Instant::now();
            if now >= deadline {
                tracing::debug!(
                    remaining_samples = guard.samples.len(),
                    "timed out waiting for native audio output to drain"
                );
                break;
            }
            let wait_for = (deadline - now).min(SCHEDULER_POLL_INTERVAL);
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, wait_for)
                .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
            guard = next_guard;
        }
        Ok(())
    }

    fn queued_duration(&self) -> std::result::Result<Duration, String> {
        let queued_samples = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?
            .samples
            .len();
        Ok(audio_samples_duration(
            queued_samples,
            self.sample_rate,
            self.channels,
        ))
    }
}

struct AudioDeviceCandidate {
    source: &'static str,
    name: String,
    device: cpal::Device,
}

impl AudioDeviceCandidate {
    fn new(source: &'static str, name: String, device: cpal::Device) -> Self {
        Self {
            source,
            name,
            device,
        }
    }
}

fn output_device_candidates(
    host: &cpal::Host,
) -> std::result::Result<Vec<AudioDeviceCandidate>, String> {
    let mut devices = match host.output_devices() {
        Ok(devices) => devices
            .map(|device| {
                let name = device_name(&device);
                (name, device)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(%error, "failed to enumerate native audio output devices");
            Vec::new()
        }
    };
    tracing::debug!(
        available_output_devices = ?devices.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "available native audio output devices"
    );

    let mut candidates = Vec::new();
    if let Ok(requested) = env::var("TINY_AUDIO_DEVICE") {
        let requested = requested.trim();
        if !requested.is_empty() {
            let requested_lower = requested.to_lowercase();
            if let Some((name, device)) = take_output_device(&mut devices, |name| {
                name.to_lowercase().contains(&requested_lower)
            }) {
                tracing::debug!(
                    requested_device = requested,
                    selected_device = %name,
                    "selected requested native audio output device"
                );
                candidates.push(AudioDeviceCandidate::new("requested", name, device));
            } else {
                tracing::warn!(
                    requested_device = requested,
                    "requested native audio output device was not found"
                );
            }
        }
    }

    if let Some((name, device)) = take_output_device(&mut devices, preferred_audio_service_device) {
        tracing::debug!(
            selected_device = %name,
            "selected preferred native audio service device"
        );
        candidates.push(AudioDeviceCandidate::new("preferred", name, device));
    }

    if let Some(device) = host.default_output_device() {
        let name = device_name(&device);
        devices.retain(|(device_name, _)| device_name != &name);
        if !candidates.iter().any(|candidate| candidate.name == name) {
            tracing::debug!(
                default_device = %name,
                "selected default native audio output device"
            );
            candidates.push(AudioDeviceCandidate::new("default", name, device));
        }
    }

    let (mut normal_devices, null_devices): (Vec<_>, Vec<_>) = devices
        .into_iter()
        .partition(|(name, _)| !null_audio_device(name));
    candidates.extend(
        normal_devices
            .drain(..)
            .map(|(name, device)| AudioDeviceCandidate::new("enumerated", name, device)),
    );
    candidates.extend(
        null_devices
            .into_iter()
            .map(|(name, device)| AudioDeviceCandidate::new("null-fallback", name, device)),
    );

    if candidates.is_empty() {
        return Err("未找到系统音频输出设备".to_string());
    }
    Ok(candidates)
}

fn take_output_device<P>(
    devices: &mut Vec<(String, cpal::Device)>,
    predicate: P,
) -> Option<(String, cpal::Device)>
where
    P: Fn(&str) -> bool,
{
    let index = devices.iter().position(|(name, _)| predicate(name))?;
    Some(devices.remove(index))
}

fn preferred_audio_service_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("pipewire") || name.contains("pulse")
}

fn null_audio_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "null" || name.contains("discard")
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|error| format!("<读取设备名称失败：{error}>"))
}

fn build_audio_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> std::result::Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let error_callback = |error| tracing::warn!(%error, "native audio output stream error");
    device.build_output_stream(
        config,
        move |data: &mut [T], _| fill_audio_output(data, &shared),
        error_callback,
        None,
    )
}

fn fill_audio_output<T>(data: &mut [T], shared: &AudioShared)
where
    T: Sample + FromSample<f32>,
{
    let mut guard = shared.buffer.lock().expect("audio output buffer poisoned");
    let mut played = 0u64;
    for sample in data {
        let value = match guard.samples.pop_front() {
            Some(value) => {
                played = played.saturating_add(1);
                value
            }
            None => 0.0,
        }
        .clamp(-1.0, 1.0);
        *sample = T::from_sample(value);
    }
    drop(guard);

    if played > 0 {
        shared.played_samples.fetch_add(played, Ordering::Relaxed);
    }
    shared.ready.notify_all();
}

#[derive(Default)]
struct PositionReporter {
    last_report: Option<Instant>,
    last_position: Option<f64>,
}

impl PositionReporter {
    fn report(&mut self, pts_nsecs: u64, event_tx: &Sender<BackendEvent>) {
        if self
            .last_report
            .is_some_and(|last| last.elapsed() < POSITION_QUERY_INTERVAL)
        {
            return;
        }

        let position = pts_nsecs as f64 / 1_000_000_000.0;
        if self
            .last_position
            .is_some_and(|last| (last - position).abs() < 0.05)
        {
            return;
        }

        self.last_report = Some(Instant::now());
        self.last_position = Some(position);
        let _ = event_tx.send(BackendEvent::PositionChanged(position));
    }
}

struct BufferedReporter {
    last_report: Option<Instant>,
    last_buffered_until: Option<f64>,
    video_buffered_until: Option<f64>,
    audio_buffered_until: Option<f64>,
    needs_audio: bool,
}

impl BufferedReporter {
    fn new(needs_audio: bool) -> Self {
        Self {
            last_report: None,
            last_buffered_until: None,
            video_buffered_until: None,
            audio_buffered_until: None,
            needs_audio,
        }
    }

    fn reset_to(&mut self, position_seconds: f64, event_tx: &Sender<BackendEvent>) {
        let position_seconds = position_seconds.max(0.0);
        self.last_report = None;
        self.last_buffered_until = None;
        self.video_buffered_until = Some(position_seconds);
        self.audio_buffered_until = self.needs_audio.then_some(position_seconds);
        self.report_value(Some(position_seconds), event_tx);
        self.last_report = None;
    }

    fn report_video_timeline_nsecs(
        &mut self,
        timeline_nsecs: u64,
        event_tx: &Sender<BackendEvent>,
    ) {
        self.video_buffered_until = Some(max_optional_seconds(
            self.video_buffered_until,
            timeline_nsecs,
        ));
        self.report_combined(event_tx);
    }

    fn report_audio_timeline_nsecs(
        &mut self,
        timeline_nsecs: u64,
        event_tx: &Sender<BackendEvent>,
    ) {
        self.audio_buffered_until = Some(max_optional_seconds(
            self.audio_buffered_until,
            timeline_nsecs,
        ));
        self.report_combined(event_tx);
    }

    fn report_combined(&mut self, event_tx: &Sender<BackendEvent>) {
        if self
            .last_report
            .is_some_and(|last| last.elapsed() < POSITION_QUERY_INTERVAL)
        {
            return;
        }

        let Some(buffered_until) = (if self.needs_audio {
            self.video_buffered_until
                .zip(self.audio_buffered_until)
                .map(|(video, audio)| video.min(audio))
        } else {
            self.video_buffered_until
        }) else {
            return;
        };
        let buffered_until = self
            .last_buffered_until
            .map(|last| last.max(buffered_until))
            .unwrap_or(buffered_until);
        self.report_value(Some(buffered_until), event_tx);
    }

    fn report_value(&mut self, buffered_until: Option<f64>, event_tx: &Sender<BackendEvent>) {
        if !optional_buffered_value_changed(self.last_buffered_until, buffered_until) {
            return;
        }

        self.last_report = Some(Instant::now());
        self.last_buffered_until = buffered_until;
        let _ = event_tx.send(BackendEvent::BufferedChanged(buffered_until));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MappedTimestamp {
    timeline_nsecs: u64,
    sink_nsecs: u64,
}

struct TimestampMapper {
    start_nsecs: Option<u64>,
    fallback_first_nsecs: Option<u64>,
    start_position_nsecs: u64,
    fallback_step_nsecs: u64,
    last_timeline_nsecs: Option<u64>,
}

impl TimestampMapper {
    fn new(
        start_nsecs: Option<u64>,
        start_position_nsecs: u64,
        fallback_step_nsecs: Option<u64>,
    ) -> Self {
        Self {
            start_nsecs,
            fallback_first_nsecs: None,
            start_position_nsecs,
            fallback_step_nsecs: fallback_step_nsecs.unwrap_or(1),
            last_timeline_nsecs: None,
        }
    }

    fn map(&mut self, timestamp: i64, time_base: ffi::AVRational) -> MappedTimestamp {
        let mut timeline_nsecs = timestamp_to_nsecs(timestamp, time_base)
            .map(|nsecs| self.timeline_from_timestamp(nsecs))
            .unwrap_or_else(|| self.next_synthetic_timeline());

        if self.start_position_nsecs > 0 && timeline_nsecs == 0 {
            timeline_nsecs = self.next_synthetic_timeline();
        }
        if let Some(last_timeline_nsecs) = self.last_timeline_nsecs
            && timeline_nsecs <= last_timeline_nsecs
        {
            timeline_nsecs = last_timeline_nsecs.saturating_add(self.fallback_step_nsecs);
        }

        self.last_timeline_nsecs = Some(timeline_nsecs);
        MappedTimestamp {
            timeline_nsecs,
            sink_nsecs: timeline_nsecs.saturating_sub(self.start_position_nsecs),
        }
    }

    fn timeline_from_timestamp(&mut self, nsecs: u64) -> u64 {
        if let Some(start_nsecs) = self.start_nsecs {
            nsecs.saturating_sub(start_nsecs)
        } else {
            let first_nsecs = *self.fallback_first_nsecs.get_or_insert(nsecs);
            self.start_position_nsecs
                .saturating_add(nsecs.saturating_sub(first_nsecs))
        }
    }

    fn next_synthetic_timeline(&self) -> u64 {
        self.last_timeline_nsecs
            .map(|last| last.saturating_add(self.fallback_step_nsecs))
            .unwrap_or(self.start_position_nsecs)
    }
}

fn frame_best_effort_timestamp(frame: *mut ffi::AVFrame) -> i64 {
    unsafe {
        if (*frame).best_effort_timestamp != ffi::AV_NOPTS_VALUE {
            (*frame).best_effort_timestamp
        } else {
            (*frame).pts
        }
    }
}

fn frame_sample_format(
    frame: *mut ffi::AVFrame,
) -> std::result::Result<ffi::AVSampleFormat, String> {
    let format = unsafe { (*frame).format };
    match format {
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P)
        }
        _ => Err(format!("FFmpeg 音频帧采样格式无效：{format}")),
    }
}

fn frame_size(frame: *mut ffi::AVFrame) -> Option<RenderSize> {
    let (width, height) = unsafe { ((*frame).width, (*frame).height) };
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(RenderSize {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

fn timestamp_to_nsecs(timestamp: i64, time_base: ffi::AVRational) -> Option<u64> {
    if timestamp == ffi::AV_NOPTS_VALUE || time_base.den <= 0 {
        return None;
    }
    let nsecs_time_base = ffi::AVRational {
        num: 1,
        den: 1_000_000_000,
    };
    let nsecs = unsafe { ffi::av_rescale_q(timestamp, time_base, nsecs_time_base) };
    u64::try_from(nsecs).ok()
}

unsafe fn stream_frame_duration_nsecs(stream: *mut ffi::AVStream) -> Option<u64> {
    if stream.is_null() {
        return None;
    }

    unsafe {
        rational_frame_duration_nsecs((*stream).avg_frame_rate)
            .or_else(|| rational_frame_duration_nsecs((*stream).r_frame_rate))
    }
}

fn rational_frame_duration_nsecs(rate: ffi::AVRational) -> Option<u64> {
    if rate.num <= 0 || rate.den <= 0 {
        return None;
    }

    Some(((rate.den as u64).saturating_mul(1_000_000_000) / rate.num as u64).max(1))
}

fn seconds_to_nsecs(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }

    (seconds * 1_000_000_000.0).round().min(u64::MAX as f64) as u64
}

fn nsecs_to_timestamp(nsecs: u64, time_base: ffi::AVRational) -> i64 {
    let nsecs_time_base = ffi::AVRational {
        num: 1,
        den: 1_000_000_000,
    };
    let nsecs = i64::try_from(nsecs).unwrap_or(i64::MAX);
    unsafe { ffi::av_rescale_q(nsecs, nsecs_time_base, time_base) }
}

fn nsecs_to_seconds(nsecs: u64) -> f64 {
    nsecs as f64 / 1_000_000_000.0
}

fn max_optional_seconds(current: Option<f64>, timeline_nsecs: u64) -> f64 {
    let next = nsecs_to_seconds(timeline_nsecs);
    current.map(|current| current.max(next)).unwrap_or(next)
}

fn optional_buffered_value_changed(previous: Option<f64>, next: Option<f64>) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(previous), Some(next)) => (previous - next).abs() >= 0.05,
        _ => true,
    }
}

fn video_frame_len(size: RenderSize) -> std::result::Result<usize, String> {
    let pixels = size
        .width
        .checked_mul(size.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "视频帧过大".to_string())?;
    usize::try_from(pixels).map_err(|_| "视频帧过大".to_string())
}

fn audio_sample_len(samples: c_int, channels: c_int) -> std::result::Result<usize, String> {
    if samples < 0 || channels <= 0 {
        return Err("音频帧尺寸无效".to_string());
    }
    usize::try_from(samples)
        .ok()
        .and_then(|samples| samples.checked_mul(usize::try_from(channels).ok()?))
        .ok_or_else(|| "音频帧过大".to_string())
}

fn audio_samples_duration(samples: usize, sample_rate: c_int, channels: c_int) -> Duration {
    if samples == 0 || sample_rate <= 0 || channels <= 0 {
        return Duration::ZERO;
    }

    let denominator = (sample_rate as u128).saturating_mul(channels as u128);
    if denominator == 0 {
        return Duration::ZERO;
    }
    let nanos = (samples as u128).saturating_mul(1_000_000_000) / denominator;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn samples_for_duration(timeline_nsecs: u64, sample_rate: c_int, channels: c_int) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 || channels <= 0 {
        return 0;
    }

    let samples = (timeline_nsecs as u128)
        .saturating_mul(sample_rate as u128)
        .saturating_mul(channels as u128)
        / 1_000_000_000;
    u64::try_from(samples).unwrap_or(u64::MAX)
}

fn zeroed_channel_layout() -> ffi::AVChannelLayout {
    unsafe { std::mem::zeroed() }
}

fn duration_nsecs(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn pts_distance(left: FramePts, right: FramePts) -> u64 {
    left.nsecs.abs_diff(right.nsecs)
}

fn ffmpeg_error(error: c_int) -> String {
    let mut buffer = [0i8; 256];
    let result = unsafe { ffi::av_strerror(error, buffer.as_mut_ptr(), buffer.len()) };
    if result < 0 {
        return format!("FFmpeg error {error}");
    }
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn ffmpeg_interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let control = unsafe { &*(opaque as *const FfmpegControl) };
    control.should_interrupt() as c_int
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_mapper_uses_stream_start_when_available() {
        let mut mapper = TimestampMapper::new(Some(1_000_000_000), 0, None);
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(
            mapper.map(1_250, time_base),
            MappedTimestamp {
                timeline_nsecs: 250_000_000,
                sink_nsecs: 250_000_000,
            }
        );
    }

    #[test]
    fn ffmpeg_control_tracks_seek_generations() {
        let control = FfmpegControl::new();

        let first = control.request_seek();
        assert!(control.has_pending_seek());
        control.finish_seek(first);
        assert!(!control.has_pending_seek());

        let second = control.request_seek();
        assert!(control.has_pending_seek());
        control.finish_seek(first);
        assert!(control.has_pending_seek());
        control.finish_seek(second);
        assert!(!control.has_pending_seek());
    }

    #[test]
    fn timestamp_mapper_uses_first_timestamp_without_stream_start() {
        let mut mapper = TimestampMapper::new(None, 0, None);
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(
            mapper.map(500, time_base),
            MappedTimestamp {
                timeline_nsecs: 0,
                sink_nsecs: 0,
            }
        );
        assert_eq!(
            mapper.map(750, time_base),
            MappedTimestamp {
                timeline_nsecs: 250_000_000,
                sink_nsecs: 250_000_000,
            }
        );
    }

    #[test]
    fn timestamp_mapper_offsets_sink_timestamps_after_seek() {
        let mut mapper = TimestampMapper::new(Some(0), 10_000_000_000, None);
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(
            mapper.map(10_250, time_base),
            MappedTimestamp {
                timeline_nsecs: 10_250_000_000,
                sink_nsecs: 250_000_000,
            }
        );
    }

    #[test]
    fn timestamp_mapper_synthesizes_repeated_video_timestamps() {
        let mut mapper = TimestampMapper::new(Some(0), 0, Some(40_000_000));
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(
            mapper.map(0, time_base),
            MappedTimestamp {
                timeline_nsecs: 0,
                sink_nsecs: 0,
            }
        );
        assert_eq!(
            mapper.map(0, time_base),
            MappedTimestamp {
                timeline_nsecs: 40_000_000,
                sink_nsecs: 40_000_000,
            }
        );
    }

    #[test]
    fn timestamp_mapper_keeps_missing_timestamps_at_seek_target() {
        let mut mapper = TimestampMapper::new(Some(0), 10_000_000_000, Some(40_000_000));
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(
            mapper.map(ffi::AV_NOPTS_VALUE, time_base),
            MappedTimestamp {
                timeline_nsecs: 10_000_000_000,
                sink_nsecs: 0,
            }
        );
        assert_eq!(
            mapper.map(0, time_base),
            MappedTimestamp {
                timeline_nsecs: 10_040_000_000,
                sink_nsecs: 40_000_000,
            }
        );
    }

    #[test]
    fn optional_buffered_value_changed_uses_small_threshold() {
        assert!(!optional_buffered_value_changed(None, None));
        assert!(optional_buffered_value_changed(None, Some(1.0)));
        assert!(optional_buffered_value_changed(Some(1.0), None));
        assert!(!optional_buffered_value_changed(Some(1.0), Some(1.03)));
        assert!(optional_buffered_value_changed(Some(1.0), Some(1.05)));
    }

    #[test]
    fn buffered_reporter_reports_first_video_update_after_reset() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = BufferedReporter::new(false);

        reporter.reset_to(0.0, &tx);
        assert_buffered_event(&rx, Some(0.0));

        reporter.report_video_timeline_nsecs(1_000_000_000, &tx);

        assert_buffered_event(&rx, Some(1.0));
    }

    #[test]
    fn buffered_reporter_reports_first_audio_video_update_after_reset() {
        let (tx, rx) = mpsc::channel();
        let mut reporter = BufferedReporter::new(true);

        reporter.reset_to(12.0, &tx);
        assert_buffered_event(&rx, Some(12.0));

        reporter.report_video_timeline_nsecs(13_000_000_000, &tx);
        assert!(rx.try_recv().is_err());

        reporter.report_audio_timeline_nsecs(13_000_000_000, &tx);

        assert_buffered_event(&rx, Some(13.0));
    }

    fn assert_buffered_event(rx: &Receiver<BackendEvent>, expected: Option<f64>) {
        match rx.try_recv().expect("expected buffered event") {
            BackendEvent::BufferedChanged(buffered_until) => {
                assert_eq!(buffered_until, expected);
            }
            event => panic!("expected buffered event, got {event:?}"),
        }
    }

    #[test]
    fn queued_video_duration_uses_first_and_last_frame_pts() {
        let mut queue = VecDeque::new();
        assert_eq!(queued_video_duration(&queue), Duration::ZERO);

        queue.push_back(test_queued_video_frame(1_000_000_000));
        assert_eq!(queued_video_duration(&queue), Duration::ZERO);

        queue.push_back(test_queued_video_frame(1_180_000_000));
        queue.push_back(test_queued_video_frame(1_300_000_000));

        assert_eq!(queued_video_duration(&queue), Duration::from_millis(300));
    }

    #[test]
    fn late_video_drop_waits_for_grace_after_frame_end() {
        assert!(!should_drop_late_video_frame(
            1_000_000_000,
            16_000_000,
            1_090_000_000
        ));
        assert!(should_drop_late_video_frame(
            1_000_000_000,
            16_000_000,
            1_091_000_000
        ));
    }

    fn test_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255]),
            },
            timeline_nsecs,
        }
    }

    #[test]
    fn audio_sample_len_rejects_invalid_sizes() {
        assert!(audio_sample_len(-1, FALLBACK_AUDIO_OUTPUT_CHANNELS).is_err());
        assert!(audio_sample_len(1024, 0).is_err());
        assert_eq!(
            audio_sample_len(1024, FALLBACK_AUDIO_OUTPUT_CHANNELS).unwrap(),
            1024 * FALLBACK_AUDIO_OUTPUT_CHANNELS as usize
        );
    }

    #[test]
    fn audio_samples_duration_accounts_for_interleaved_channels() {
        assert_eq!(
            audio_samples_duration(96_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
            Duration::from_secs(1)
        );
        assert_eq!(
            audio_samples_duration(0, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
            Duration::ZERO
        );
        assert_eq!(audio_samples_duration(1024, 0, 2), Duration::ZERO);
        assert_eq!(audio_samples_duration(1024, 48_000, 0), Duration::ZERO);
    }

    #[test]
    fn samples_for_duration_accounts_for_interleaved_channels() {
        assert_eq!(
            samples_for_duration(1_000_000_000, 48_000, FALLBACK_AUDIO_OUTPUT_CHANNELS),
            96_000
        );
        assert_eq!(samples_for_duration(0, 48_000, 2), 0);
        assert_eq!(samples_for_duration(1_000_000_000, 0, 2), 0);
        assert_eq!(samples_for_duration(1_000_000_000, 48_000, 0), 0);
    }

    #[test]
    fn dovi_packet_timeline_uses_stream_start_when_available() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut first_packet_nsecs = None;

        assert_eq!(
            dovi_packet_timeline_nsecs(
                &mut first_packet_nsecs,
                Some(1_000_000_000),
                1_250,
                time_base,
            ),
            Some(250_000_000)
        );
        assert_eq!(first_packet_nsecs, None);
    }

    #[test]
    fn dovi_packet_timeline_uses_first_packet_when_stream_start_is_missing() {
        let time_base = ffi::AVRational { num: 1, den: 1_000 };
        let mut first_packet_nsecs = None;

        assert_eq!(
            dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_250, time_base),
            Some(0)
        );
        assert_eq!(
            dovi_packet_timeline_nsecs(&mut first_packet_nsecs, None, 1_500, time_base),
            Some(250_000_000)
        );
    }

    #[test]
    fn fill_audio_output_converts_samples_and_outputs_silence_on_underrun() {
        let shared = AudioShared {
            buffer: Mutex::new(AudioBuffer {
                samples: [-1.0, 0.0, 1.0].into_iter().collect(),
                max_samples: 8,
            }),
            ready: Condvar::new(),
            played_samples: AtomicU64::new(0),
        };
        let mut output = [0.0f64; 4];

        fill_audio_output(&mut output, &shared);

        assert_eq!(output, [-1.0, 0.0, 1.0, 0.0]);
        assert!(
            shared
                .buffer
                .lock()
                .expect("audio output buffer poisoned")
                .samples
                .is_empty()
        );
        assert_eq!(shared.played_samples.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn ffmpeg_http_headers_formats_crlf_separated_headers() {
        let headers = ffmpeg_http_headers(&[
            ("X-Emby-Token".to_string(), "token".to_string()),
            ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
        ])
        .unwrap();

        assert_eq!(
            headers,
            "X-Emby-Token: token\r\nUser-Agent: Lenna/1.0.13\r\n"
        );
    }

    #[test]
    fn ffmpeg_http_headers_rejects_header_injection() {
        assert!(ffmpeg_http_headers(&[("Bad\nName".to_string(), "value".to_string())]).is_err());
        assert!(
            ffmpeg_http_headers(&[("X-Emby-Token".to_string(), "bad\r\nvalue".to_string())])
                .is_err()
        );
    }

    #[test]
    fn detects_cacheable_http_urls() {
        assert!(should_cache_http_url("https://example.test/video.mp4"));
        assert!(should_cache_http_url("HTTP://example.test/video.mp4"));
        assert!(!should_cache_http_url("file:///tmp/video.mp4"));
        assert!(!should_cache_http_url("/tmp/video.mp4"));
    }

    #[test]
    fn http_cache_request_header_log_includes_effective_headers() {
        let headers = reqwest_header_pairs(&[
            ("X-Emby-Token".to_string(), "token".to_string()),
            ("User-Agent".to_string(), "Lenna/1.0.13".to_string()),
        ])
        .unwrap();

        assert_eq!(
            http_cache_request_headers_for_log(&headers, "bytes=128-255"),
            vec![
                "accept-encoding: identity".to_string(),
                "connection: keep-alive".to_string(),
                "range: bytes=128-255".to_string(),
                "x-emby-token: token".to_string(),
                "user-agent: Lenna/1.0.13".to_string(),
            ]
        );
    }

    #[test]
    fn http_cache_range_header_limits_request_size() {
        assert_eq!(http_cache_range_header(0, None), "bytes=0-33554431");
        assert_eq!(http_cache_range_header(128, None), "bytes=128-33554559");
        assert_eq!(
            http_cache_range_header(595_453_649, Some(596_486_439)),
            "bytes=595453649-596486438"
        );
        assert_eq!(
            http_cache_range_header(10_675_366_349, Some(10_675_368_645)),
            "bytes=10675366349-10675368644"
        );
    }

    #[test]
    fn http_cache_range_request_timeout_is_short_for_small_tail_ranges() {
        assert_eq!(
            http_cache_range_request_len(10_675_366_349, Some(10_675_368_645)),
            2_296
        );
        assert_eq!(
            http_cache_range_request_timeout(2_296),
            HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT
        );
        assert_eq!(
            http_cache_range_request_timeout(HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES + 1),
            HTTP_CACHE_RANGE_REQUEST_TIMEOUT
        );
    }

    #[test]
    fn http_cache_response_header_log_includes_response_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            reqwest::header::HeaderValue::from_static("bytes 10-19/100"),
        );
        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("10"),
        );

        assert_eq!(
            http_cache_response_headers_for_log(&headers),
            vec![
                "content-length: 10".to_string(),
                "content-range: bytes 10-19/100".to_string(),
            ]
        );
    }

    #[test]
    fn http_ring_cache_state_copies_available_bytes() {
        let mut state = HttpRingCacheState::new(10);
        state.append_at(10, b"abcdef");
        let mut output = [0; 3];

        assert_eq!(state.copy_available(12, &mut output), Some(3));
        assert_eq!(&output, b"cde");
        assert_eq!(state.copy_available(16, &mut output), None);
    }

    #[test]
    fn http_ring_cache_state_uses_content_length_hint_for_progress() {
        let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(100));

        state.append_at(0, b"abcde");

        assert_eq!(
            state.stream_buffer_progress(),
            Some(HttpStreamBufferProgress {
                start_fraction: 0.0,
                end_fraction: 0.05,
            })
        );
    }

    #[test]
    fn http_ring_cache_state_trims_oldest_bytes() {
        let mut state = HttpRingCacheState::new(100);
        state.append_at(100, b"abcdef");

        state.set_reader_offset(102);
        state.trim_to_capacity(4);

        assert_eq!(state.base_offset, 102);
        assert_eq!(state.next_offset, 106);
        let mut output = [0; 4];
        assert_eq!(state.copy_available(102, &mut output), Some(4));
        assert_eq!(&output, b"cdef");
        assert_eq!(state.copy_available(100, &mut output), None);
    }

    #[test]
    fn http_ring_cache_state_copies_wrapped_bytes() {
        let mut state = HttpRingCacheState::new_with_cache_capacity(0, 6);
        state.append_at(0, b"abcdef");

        state.set_reader_offset(4);
        state.append_at(6, b"ghij");

        assert_eq!(state.base_offset, 4);
        assert_eq!(state.next_offset, 10);
        let mut output = [0; 6];
        assert_eq!(state.copy_available(4, &mut output), Some(6));
        assert_eq!(&output, b"efghij");
    }

    #[test]
    fn http_ring_cache_state_preserves_unread_bytes_when_over_capacity() {
        let mut state = HttpRingCacheState::new(100);
        state.append_at(100, b"abcdef");

        state.trim_to_capacity(4);

        assert_eq!(state.base_offset, 100);
        assert_eq!(state.next_offset, 106);
        let mut output = [0; 6];
        assert_eq!(state.copy_available(100, &mut output), Some(6));
        assert_eq!(&output, b"abcdef");
    }

    #[test]
    fn http_ring_cache_state_refuses_append_when_capacity_is_unread() {
        let mut state = HttpRingCacheState::new_with_cache_capacity(0, 4);
        assert!(state.append_at(0, b"abcd"));

        assert!(!state.append_at(4, b"ef"));

        assert_eq!(state.base_offset, 0);
        assert_eq!(state.next_offset, 4);
        let mut output = [0; 4];
        assert_eq!(state.copy_available(0, &mut output), Some(4));
        assert_eq!(&output, b"abcd");
    }

    #[test]
    fn http_ring_cache_state_limits_prefetch_window_from_reader() {
        let mut state = HttpRingCacheState::new(100);

        assert_eq!(
            state.append_capacity_from(100 + HTTP_RING_CACHE_CAPACITY as u64),
            0
        );
        assert_eq!(
            state.append_capacity_from(99 + HTTP_RING_CACHE_CAPACITY as u64),
            1
        );

        state.set_reader_offset(200);
        assert_eq!(
            state.append_capacity_from(100 + HTTP_RING_CACHE_CAPACITY as u64),
            100
        );
    }

    #[test]
    fn http_ring_cache_state_ignores_seek_outside_cached_range_until_read() {
        let mut state = HttpRingCacheState::new(100);
        state.append_at(100, b"abcdef");

        state.note_seek_offset(10_000);
        state.trim_to_capacity(4);

        assert_eq!(state.base_offset, 100);
        assert_eq!(state.next_offset, 106);
        let mut output = [0; 6];
        assert_eq!(state.copy_available(100, &mut output), Some(6));
        assert_eq!(&output, b"abcdef");
    }

    #[test]
    fn http_ring_cache_state_restart_clears_eof_for_next_range() {
        let mut state = HttpRingCacheState::new(100);
        state.append_at(100, b"abcdef");
        state.eof = true;

        state.restart_at(0);

        assert_eq!(state.base_offset, 0);
        assert_eq!(state.next_offset, 0);
        assert!(!state.eof);
    }

    #[test]
    fn content_range_parser_reads_total_size() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            reqwest::header::HeaderValue::from_static("bytes 100-199/12345"),
        );

        assert_eq!(content_len_from_content_range(&headers), Some(12345));
    }

    #[test]
    fn playback_scheduler_reports_ready_for_past_frames() {
        let scheduler = PlaybackScheduler::new(1_000_000_000);
        let control = FfmpegControl::new();

        assert_eq!(
            scheduler.wait_until(500_000_000, &control),
            WaitStatus::Ready
        );
    }

    #[test]
    fn annex_b_probe_detects_three_and_four_byte_start_codes() {
        assert!(has_annex_b_start_code(&[9, 0, 0, 1, 1]));
        assert!(has_annex_b_start_code(&[9, 0, 0, 0, 1, 1]));
        assert!(!has_annex_b_start_code(&[0, 0, 2, 1]));
    }

    #[test]
    fn ffmpeg_raw_video_format_maps_supported_yuv_formats() {
        assert_eq!(
            ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int),
            Some(RawVideoFormat::P010Le)
        );
        assert_eq!(
            ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE as c_int),
            Some(RawVideoFormat::I42010Le)
        );
        assert_eq!(
            ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int),
            Some(RawVideoFormat::Nv12)
        );
        assert_eq!(
            ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int),
            Some(RawVideoFormat::I420)
        );
        assert_eq!(
            ffmpeg_raw_video_format(ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int),
            None
        );
    }
}
