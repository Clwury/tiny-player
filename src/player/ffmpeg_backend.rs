use std::{
    ffi::{CStr, CString},
    os::raw::{c_int, c_void},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::{
    backend::{BackendError, BackendEvent, Result},
    render_host::{
        DecodedFrame, FramePixels, FramePts, FrameSlot, RenderSize, packed_bgra_from_stride,
    },
};

const AUDIO_OUTPUT_CHANNELS: c_int = 2;
const POSITION_QUERY_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_VIDEO_FRAME_DURATION_NSECS: u64 = 1_000_000_000 / 24;

static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

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
        gst::init().map_err(|error| BackendError::Ffmpeg(error.to_string()))?;
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

    pub fn load_url(&mut self, url: &str) -> Result<()> {
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

        self.worker = Some(FfmpegWorker::spawn(
            FfmpegPlaybackInput {
                url: url.to_string(),
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
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
struct FfmpegControl {
    shutdown: AtomicBool,
    seek_pending: AtomicBool,
}

impl FfmpegControl {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            seek_pending: AtomicBool::new(false),
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    fn request_seek(&self) {
        self.seek_pending.store(true, Ordering::Relaxed);
    }

    fn finish_seek(&self) {
        self.seek_pending.store(false, Ordering::Relaxed);
    }

    fn has_pending_seek(&self) -> bool {
        self.seek_pending.load(Ordering::Relaxed)
    }
}

struct FfmpegPlaybackInput {
    url: String,
    start_position_seconds: f64,
}

enum FfmpegCommand {
    Seek(f64),
}

impl FfmpegWorker {
    fn spawn(
        input: FfmpegPlaybackInput,
        frame_slot: FrameSlot,
        event_tx: Sender<BackendEvent>,
    ) -> Result<Self> {
        let control = Arc::new(FfmpegControl::new());
        let (command_tx, command_rx) = mpsc::channel();
        let pipeline = Arc::new(Mutex::new(None));
        let frame_presented = Arc::new(AtomicBool::new(false));
        let worker_control = Arc::clone(&control);
        let worker_pipeline = Arc::clone(&pipeline);
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
                    worker_pipeline,
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
            pipeline,
            handle,
        })
    }

    fn seek(&self, position_seconds: f64) -> Result<()> {
        self.control.request_seek();
        self.command_tx
            .send(FfmpegCommand::Seek(position_seconds))
            .map_err(|_| {
                self.control.finish_seek();
                BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
            })?;
        stop_ffmpeg_pipeline_async(Arc::clone(&self.pipeline));
        Ok(())
    }

    fn stop(self) {
        let Self {
            control,
            command_tx: _,
            pipeline,
            handle,
        } = self;
        stop_ffmpeg_worker(control, pipeline);
        let _ = handle.join();
    }

    fn stop_async(self) {
        let Self {
            control,
            command_tx: _,
            pipeline,
            handle,
        } = self;
        control.shutdown();
        let _ = thread::Builder::new()
            .name("tiny-ffmpeg-stop".to_string())
            .spawn(move || {
                stop_ffmpeg_pipeline(pipeline);
                let _ = handle.join();
            });
    }
}

fn stop_ffmpeg_worker(control: Arc<FfmpegControl>, pipeline: Arc<Mutex<Option<gst::Pipeline>>>) {
    control.shutdown();
    stop_ffmpeg_pipeline(pipeline);
}

fn stop_ffmpeg_pipeline(pipeline: Arc<Mutex<Option<gst::Pipeline>>>) {
    if let Some(pipeline) = pipeline
        .lock()
        .expect("FFmpeg pipeline handle poisoned")
        .as_ref()
        .cloned()
    {
        let _ = pipeline.set_state(gst::State::Null);
    }
}

fn stop_ffmpeg_pipeline_async(pipeline: Arc<Mutex<Option<gst::Pipeline>>>) {
    let pipeline = pipeline
        .lock()
        .expect("FFmpeg pipeline handle poisoned")
        .as_ref()
        .cloned();
    if let Some(pipeline) = pipeline {
        stop_pipeline_instance_async(pipeline);
    }
}

fn stop_pipeline_instance_async(pipeline: gst::Pipeline) {
    let _ = thread::Builder::new()
        .name("tiny-ffmpeg-pipeline-stop".to_string())
        .spawn(move || {
            let _ = pipeline.set_state(gst::State::Null);
        });
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

fn run_ffmpeg_playback(
    source: FfmpegPlaybackInput,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    control: Arc<FfmpegControl>,
    command_rx: Receiver<FfmpegCommand>,
    pipeline_handle: Arc<Mutex<Option<gst::Pipeline>>>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let mut input = FormatContext::open(&source.url, Arc::clone(&control))?;
    input.find_stream_info()?;

    let video_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)?
        .ok_or_else(|| "FFmpeg 未找到可解码视频流".to_string())?;
    if source.start_position_seconds > 0.0 {
        input.seek_stream(video_stream, source.start_position_seconds)?;
    }
    let video_decoder = Decoder::open(video_stream)?;
    let mut video_frame = AvFrame::new()?;
    let mut video_scaler = VideoScaler::new(&video_decoder)?;
    let mut current_start_position_nsecs = seconds_to_nsecs(source.start_position_seconds);
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let mut video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );

    let audio_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_AUDIO)
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "FFmpeg audio stream selection failed");
            None
        });
    let mut audio_decoder = match audio_stream {
        Some(stream) => match Decoder::open(stream) {
            Ok(decoder) => Some(decoder),
            Err(error) => {
                tracing::warn!(%error, "FFmpeg audio decoder initialization failed");
                None
            }
        },
        None => None,
    };
    let mut audio_frame = if audio_decoder.is_some() {
        Some(AvFrame::new()?)
    } else {
        None
    };
    let mut audio_resampler = match audio_decoder.as_ref() {
        Some(decoder) => match AudioResampler::new(decoder) {
            Ok(resampler) => Some(resampler),
            Err(error) => {
                tracing::warn!(%error, "FFmpeg audio resampler initialization failed");
                audio_decoder = None;
                audio_frame = None;
                None
            }
        },
        None => None,
    };
    let mut audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );

    let mut sinks = FfmpegPlaybackSinks::new(
        video_decoder.size()?,
        audio_resampler.as_ref().map(AudioResampler::output_rate),
        frame_slot.clone(),
        event_tx.clone(),
        Arc::clone(&frame_presented),
        Arc::clone(&pipeline_handle),
    )?;

    if let Some(duration) = input.duration_seconds() {
        let _ = event_tx.send(BackendEvent::DurationChanged(duration));
    }
    let duration_seconds = input.duration_seconds();

    let mut packet = AvPacket::new()?;
    let mut buffered_reporter = BufferedReporter::new(audio_resampler.is_some());
    let mut seek_preview_pending = source.start_position_seconds > 0.0;
    buffered_reporter.reset_to(source.start_position_seconds.max(0.0), &event_tx);

    while !control.should_stop() {
        if let Some(position_seconds) = drain_seek_command(&command_rx) {
            let seek_result: std::result::Result<(), String> = (|| {
                let position_seconds = position_seconds.max(0.0);
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
                buffered_reporter = BufferedReporter::new(audio_resampler.is_some());
                seek_preview_pending = true;
                sinks = FfmpegPlaybackSinks::new(
                    video_decoder.size()?,
                    audio_resampler.as_ref().map(AudioResampler::output_rate),
                    frame_slot.clone(),
                    event_tx.clone(),
                    Arc::clone(&frame_presented),
                    Arc::clone(&pipeline_handle),
                )?;
                buffered_reporter.reset_to(position_seconds, &event_tx);
                let _ = event_tx.send(BackendEvent::PositionChanged(position_seconds));
                Ok(())
            })();
            control.finish_seek();
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
            return Err(format!("FFmpeg 读取媒体包失败：{}", ffmpeg_error(read)));
        }

        let process_result = match packet.stream_index() {
            index if index == video_decoder.stream_index => {
                video_decoder.decode_packet(packet.as_ptr(), &mut video_frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base);
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        if seek_preview_pending {
                            let pixels = video_scaler.convert(frame)?;
                            frame_slot.push(DecodedFrame {
                                size: video_scaler.size,
                                pts: Some(FramePts {
                                    nsecs: current_start_position_nsecs,
                                }),
                                pixels: FramePixels::Bgra8(pixels),
                            });
                            frame_presented.store(true, Ordering::Relaxed);
                            seek_preview_pending = false;
                        }
                        return Ok(());
                    }
                    let pixels = video_scaler.convert(frame)?;
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    sinks.push_video(pixels, timestamp.sink_nsecs, timestamp.timeline_nsecs)?;
                    buffered_reporter.report_video_timeline_nsecs(
                        timestamp
                            .timeline_nsecs
                            .saturating_add(video_frame_duration_nsecs),
                        &event_tx,
                    );
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
                        sinks.push_audio(audio, timestamp.sink_nsecs)?;
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
        let pixels = video_scaler.convert(frame)?;
        sinks.push_video(pixels, timestamp.sink_nsecs, timestamp.timeline_nsecs)?;
        buffered_reporter.report_video_timeline_nsecs(
            timestamp
                .timeline_nsecs
                .saturating_add(video_frame_duration_nsecs),
            &event_tx,
        );
        Ok(())
    })?;

    if let (Some(decoder), Some(frame), Some(resampler)) = (
        audio_decoder.as_ref(),
        audio_frame.as_mut(),
        audio_resampler.as_mut(),
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
                sinks.push_audio(audio, timestamp.sink_nsecs)?;
                buffered_reporter.report_audio_timeline_nsecs(buffered_until_nsecs, &event_tx);
            }
            Ok(())
        })?;
    }

    buffered_reporter.report_value(duration_seconds, &event_tx);
    sinks.end_of_stream();
    Ok(())
}

fn drain_seek_command(command_rx: &Receiver<FfmpegCommand>) -> Option<f64> {
    let mut pending_seek = None;
    while let Ok(command) = command_rx.try_recv() {
        match command {
            FfmpegCommand::Seek(position_seconds) => {
                pending_seek = Some(position_seconds.max(0.0));
            }
        }
    }
    pending_seek
}

struct FfmpegPlaybackSinks {
    pipeline: gst::Pipeline,
    video_src: gst_app::AppSrc,
    audio_src: Option<gst_app::AppSrc>,
}

impl FfmpegPlaybackSinks {
    fn new(
        video_size: RenderSize,
        audio_rate: Option<c_int>,
        frame_slot: FrameSlot,
        event_tx: Sender<BackendEvent>,
        frame_presented: Arc<AtomicBool>,
        pipeline_handle: Arc<Mutex<Option<gst::Pipeline>>>,
    ) -> std::result::Result<Self, String> {
        let pipeline = gst::Pipeline::new();
        let video_caps = gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field(
                "width",
                i32::try_from(video_size.width).map_err(|_| "视频宽度过大")?,
            )
            .field(
                "height",
                i32::try_from(video_size.height).map_err(|_| "视频高度过大")?,
            )
            .field("framerate", gst::Fraction::new(0, 1))
            .build();
        let video_src = gst_app::AppSrc::builder()
            .caps(&video_caps)
            .format(gst::Format::Time)
            .is_live(false)
            .block(false)
            .max_bytes(video_frame_len(video_size)?.saturating_mul(3) as u64)
            .build();
        let video_queue = make_element("queue")?;
        set_queue_limits(&video_queue, 3);
        let video_sink = build_video_appsink(
            video_caps,
            frame_slot,
            event_tx,
            Arc::clone(&frame_presented),
        )
        .upcast::<gst::Element>();

        pipeline
            .add_many([video_src.upcast_ref(), &video_queue, &video_sink])
            .map_err(|error| error.to_string())?;
        video_src
            .link(&video_queue)
            .map_err(|error| error.to_string())?;
        video_queue
            .link(&video_sink)
            .map_err(|error| error.to_string())?;

        let audio_src = if let Some(rate) = audio_rate.filter(|rate| *rate > 0) {
            let audio_caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", rate)
                .field("channels", AUDIO_OUTPUT_CHANNELS)
                .build();
            let audio_src = gst_app::AppSrc::builder()
                .caps(&audio_caps)
                .format(gst::Format::Time)
                .is_live(false)
                .block(false)
                .max_bytes((rate as u64).saturating_mul(AUDIO_OUTPUT_CHANNELS as u64 * 4))
                .build();
            let audio_queue = make_element("queue")?;
            set_queue_limits(&audio_queue, 16);
            let convert = make_element("audioconvert")?;
            let resample = make_element("audioresample")?;
            let sink = make_element("autoaudiosink")?;
            set_bool_property_if_exists(&sink, "async", false);

            pipeline
                .add_many([
                    audio_src.upcast_ref(),
                    &audio_queue,
                    &convert,
                    &resample,
                    &sink,
                ])
                .map_err(|error| error.to_string())?;
            audio_src
                .link(&audio_queue)
                .map_err(|error| error.to_string())?;
            gst::Element::link_many([&audio_queue, &convert, &resample, &sink])
                .map_err(|error| error.to_string())?;
            Some(audio_src)
        } else {
            None
        };

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        *pipeline_handle
            .lock()
            .expect("FFmpeg pipeline handle poisoned") = Some(pipeline.clone());

        Ok(Self {
            pipeline,
            video_src,
            audio_src,
        })
    }

    fn push_video(
        &mut self,
        pixels: Vec<u8>,
        sink_pts_nsecs: u64,
        timeline_pts_nsecs: u64,
    ) -> std::result::Result<(), String> {
        let mut buffer = gst::Buffer::from_mut_slice(pixels);
        let buffer_ref = buffer
            .get_mut()
            .ok_or_else(|| "FFmpeg 视频 buffer 不可写".to_string())?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(sink_pts_nsecs));
        buffer_ref.set_offset(timeline_pts_nsecs);
        self.video_src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|error| format!("推送 FFmpeg 视频帧失败：{error:?}"))
    }

    fn push_audio(
        &mut self,
        audio: DecodedAudio,
        pts_nsecs: u64,
    ) -> std::result::Result<(), String> {
        let Some(audio_src) = &self.audio_src else {
            return Ok(());
        };

        let mut buffer = gst::Buffer::from_mut_slice(audio.samples);
        let buffer_ref = buffer
            .get_mut()
            .ok_or_else(|| "FFmpeg 音频 buffer 不可写".to_string())?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(pts_nsecs));
        buffer_ref.set_duration(gst::ClockTime::from_nseconds(audio.duration_nsecs));
        audio_src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|error| format!("推送 FFmpeg 音频帧失败：{error:?}"))
    }

    fn end_of_stream(&self) {
        let _ = self.video_src.end_of_stream();
        if let Some(audio_src) = &self.audio_src {
            let _ = audio_src.end_of_stream();
        }
    }
}

impl Drop for FfmpegPlaybackSinks {
    fn drop(&mut self) {
        stop_pipeline_instance_async(self.pipeline.clone());
    }
}

fn build_video_appsink(
    caps: gst::Caps,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    frame_presented: Arc<AtomicBool>,
) -> gst_app::AppSink {
    let position_reporter = Arc::new(Mutex::new(PositionReporter::default()));

    gst_app::AppSink::builder()
        .caps(&caps)
        .sync(true)
        .max_buffers(1)
        .drop(true)
        .wait_on_eos(false)
        .enable_last_sample(false)
        .callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    match decoded_bgra_frame_from_sample(&sample) {
                        Ok(frame) => {
                            if let Some(pts) = frame.pts {
                                position_reporter
                                    .lock()
                                    .expect("FFmpeg position reporter poisoned")
                                    .report(pts.nsecs, &event_tx);
                            }
                            let count = FFMPEG_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                            if count == 1 || count.is_multiple_of(60) {
                                tracing::debug!(
                                    frame_count = count,
                                    pts = ?frame.pts.map(|pts| pts.nsecs),
                                    width = frame.size.width,
                                    height = frame.size.height,
                                    "decoded FFmpeg video frame"
                                );
                            }
                            frame_presented.store(true, Ordering::Relaxed);
                            frame_slot.push(frame);
                        }
                        Err(error) => {
                            tracing::debug!(%error, "failed to copy FFmpeg BGRA video frame");
                            let _ = event_tx.send(BackendEvent::Fatal(format!(
                                "复制 FFmpeg 视频帧失败：{error}"
                            )));
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        )
        .build()
}

fn decoded_bgra_frame_from_sample(
    sample: &gst::Sample,
) -> std::result::Result<DecodedFrame, String> {
    let caps = sample.caps().ok_or_else(|| "视频帧缺少 caps".to_string())?;
    let info = gst_video::VideoInfo::from_caps(caps).map_err(|error| error.to_string())?;
    if info.name() != "BGRA" {
        return Err(format!(
            "FFmpeg 视频 sink 收到不支持的格式：{}",
            info.name()
        ));
    }

    let buffer = sample
        .buffer()
        .ok_or_else(|| "视频帧缺少 buffer".to_string())?;
    let map = buffer.map_readable().map_err(|error| error.to_string())?;
    let size = RenderSize {
        width: info.width(),
        height: info.height(),
    };
    let stride = info
        .stride()
        .first()
        .copied()
        .ok_or_else(|| "视频帧缺少 stride".to_string())?;
    let stride = usize::try_from(stride).map_err(|_| "视频帧 stride 无效".to_string())?;
    let pts = if buffer.offset() != gst::format::Buffers::OFFSET_NONE {
        Some(FramePts {
            nsecs: buffer.offset(),
        })
    } else {
        buffer.pts().map(|pts| FramePts {
            nsecs: pts.nseconds(),
        })
    };
    let pixels =
        packed_bgra_from_stride(map.as_slice(), size, stride).map_err(|error| error.to_string())?;

    Ok(DecodedFrame {
        size,
        pts,
        pixels: FramePixels::Bgra8(pixels),
    })
}

fn make_element(factory: &str) -> std::result::Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|error| error.to_string())
}

fn set_bool_property_if_exists(element: &gst::Element, property: &str, value: bool) {
    if element.find_property(property).is_some() {
        element.set_property(property, value);
    }
}

fn set_queue_limits(queue: &gst::Element, buffers: u32) {
    queue.set_property("max-size-buffers", buffers);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
}

struct FormatContext {
    ptr: *mut ffi::AVFormatContext,
}

impl FormatContext {
    fn open(url: &str, control: Arc<FfmpegControl>) -> std::result::Result<Self, String> {
        let url = CString::new(url).map_err(|_| "播放地址包含无效字符".to_string())?;
        let mut context = unsafe { ffi::avformat_alloc_context() };
        if context.is_null() {
            return Err("FFmpeg 分配输入上下文失败".to_string());
        }

        unsafe {
            (*context).interrupt_callback.callback = Some(ffmpeg_interrupt_callback);
            (*context).interrupt_callback.opaque = Arc::as_ptr(&control) as *mut c_void;
        }

        let result = unsafe {
            ffi::avformat_open_input(&mut context, url.as_ptr(), ptr::null_mut(), ptr::null_mut())
        };
        if result < 0 {
            if !context.is_null() {
                unsafe { ffi::avformat_close_input(&mut context) };
            }
            return Err(format!("FFmpeg 打开媒体失败：{}", ffmpeg_error(result)));
        }

        Ok(Self { ptr: context })
    }

    fn find_stream_info(&mut self) -> std::result::Result<(), String> {
        let result = unsafe { ffi::avformat_find_stream_info(self.ptr, ptr::null_mut()) };
        if result < 0 {
            return Err(format!("FFmpeg 探测媒体流失败：{}", ffmpeg_error(result)));
        }
        Ok(())
    }

    fn best_stream(
        &self,
        media_type: ffi::AVMediaType,
    ) -> std::result::Result<Option<StreamInfo>, String> {
        let mut decoder: *const ffi::AVCodec = ptr::null();
        let index =
            unsafe { ffi::av_find_best_stream(self.ptr, media_type, -1, -1, &mut decoder, 0) };
        if index == ffi::AVERROR_STREAM_NOT_FOUND || index == ffi::AVERROR_DECODER_NOT_FOUND {
            return Ok(None);
        }
        if index < 0 {
            return Err(format!("FFmpeg 选择媒体流失败：{}", ffmpeg_error(index)));
        }

        let stream = self.stream(index)?;
        let time_base = unsafe { (*stream).time_base };
        let start_nsecs = unsafe { timestamp_to_nsecs((*stream).start_time, time_base) };
        let frame_duration_nsecs = unsafe { stream_frame_duration_nsecs(stream) };
        Ok(Some(StreamInfo {
            index,
            stream,
            decoder,
            time_base,
            start_nsecs,
            frame_duration_nsecs,
        }))
    }

    fn stream(&self, index: c_int) -> std::result::Result<*mut ffi::AVStream, String> {
        let index = usize::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?;
        let stream_count = unsafe { (*self.ptr).nb_streams as usize };
        if index >= stream_count {
            return Err("FFmpeg 媒体流索引越界".to_string());
        }
        let stream = unsafe { *(*self.ptr).streams.add(index) };
        if stream.is_null() {
            return Err("FFmpeg 媒体流为空".to_string());
        }
        Ok(stream)
    }

    fn duration_seconds(&self) -> Option<f64> {
        let duration = unsafe { (*self.ptr).duration };
        if duration <= 0 || duration == ffi::AV_NOPTS_VALUE {
            return None;
        }
        Some(duration as f64 / ffi::AV_TIME_BASE as f64)
    }

    fn seek_stream(
        &mut self,
        stream: StreamInfo,
        position_seconds: f64,
    ) -> std::result::Result<(), String> {
        let target_nsecs =
            seconds_to_nsecs(position_seconds).saturating_add(stream.start_nsecs.unwrap_or(0));
        let timestamp = nsecs_to_timestamp(target_nsecs, stream.time_base);
        let result = unsafe {
            ffi::av_seek_frame(self.ptr, stream.index, timestamp, ffi::AVSEEK_FLAG_BACKWARD)
        };
        if result < 0 {
            return Err(format!("FFmpeg 跳转媒体位置失败：{}", ffmpeg_error(result)));
        }
        Ok(())
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVFormatContext {
        self.ptr
    }
}

impl Drop for FormatContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::avformat_close_input(&mut self.ptr) };
        }
    }
}

#[derive(Clone, Copy)]
struct StreamInfo {
    index: c_int,
    stream: *mut ffi::AVStream,
    decoder: *const ffi::AVCodec,
    time_base: ffi::AVRational,
    start_nsecs: Option<u64>,
    frame_duration_nsecs: Option<u64>,
}

struct Decoder {
    ptr: *mut ffi::AVCodecContext,
    stream_index: c_int,
    time_base: ffi::AVRational,
}

impl Decoder {
    fn open(stream: StreamInfo) -> std::result::Result<Self, String> {
        let codecpar = unsafe { (*stream.stream).codecpar };
        if codecpar.is_null() {
            return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
        }

        let decoder = if stream.decoder.is_null() {
            unsafe { ffi::avcodec_find_decoder((*codecpar).codec_id) }
        } else {
            stream.decoder
        };
        if decoder.is_null() {
            return Err("FFmpeg 未找到可用解码器".to_string());
        }

        let context = unsafe { ffi::avcodec_alloc_context3(decoder) };
        if context.is_null() {
            return Err("FFmpeg 分配解码上下文失败".to_string());
        }

        let decoder_context = Self {
            ptr: context,
            stream_index: stream.index,
            time_base: stream.time_base,
        };
        let result = unsafe { ffi::avcodec_parameters_to_context(context, codecpar) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 复制 codec 参数失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*context).pkt_timebase = stream.time_base;
        }
        let result = unsafe { ffi::avcodec_open2(context, decoder, ptr::null_mut()) };
        if result < 0 {
            return Err(format!("FFmpeg 打开解码器失败：{}", ffmpeg_error(result)));
        }

        Ok(decoder_context)
    }

    fn size(&self) -> std::result::Result<RenderSize, String> {
        let (width, height) = unsafe { ((*self.ptr).width, (*self.ptr).height) };
        if width <= 0 || height <= 0 {
            return Err("FFmpeg 解码器未提供有效视频尺寸".to_string());
        }
        Ok(RenderSize {
            width: u32::try_from(width).map_err(|_| "视频宽度无效".to_string())?,
            height: u32::try_from(height).map_err(|_| "视频高度无效".to_string())?,
        })
    }

    fn decode_packet<F>(
        &self,
        packet: *const ffi::AVPacket,
        frame: &mut AvFrame,
        on_frame: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        let result = unsafe { ffi::avcodec_send_packet(self.ptr, packet) };
        if result < 0 && result != ffi::AVERROR(ffi::EAGAIN) {
            return Err(format!("FFmpeg 发送解码包失败：{}", ffmpeg_error(result)));
        }
        self.receive_frames(frame, on_frame)
    }

    fn flush<F>(&self, frame: &mut AvFrame, on_frame: F) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        let result = unsafe { ffi::avcodec_send_packet(self.ptr, ptr::null()) };
        if result < 0 && result != ffi::AVERROR_EOF {
            return Err(format!("FFmpeg 刷新解码器失败：{}", ffmpeg_error(result)));
        }
        self.receive_frames(frame, on_frame)
    }

    fn flush_buffers(&self) {
        unsafe { ffi::avcodec_flush_buffers(self.ptr) };
    }

    fn receive_frames<F>(
        &self,
        frame: &mut AvFrame,
        mut on_frame: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        loop {
            let result = unsafe { ffi::avcodec_receive_frame(self.ptr, frame.as_mut_ptr()) };
            if result == ffi::AVERROR(ffi::EAGAIN) || result == ffi::AVERROR_EOF {
                return Ok(());
            }
            if result < 0 {
                return Err(format!("FFmpeg 接收解码帧失败：{}", ffmpeg_error(result)));
            }

            let frame_result = on_frame(frame.as_mut_ptr());
            frame.unref();
            frame_result?;
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::avcodec_free_context(&mut self.ptr) };
        }
    }
}

struct AvPacket {
    ptr: *mut ffi::AVPacket,
}

impl AvPacket {
    fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_packet_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 packet 失败".to_string());
        }
        Ok(Self { ptr })
    }

    fn as_ptr(&self) -> *const ffi::AVPacket {
        self.ptr
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVPacket {
        self.ptr
    }

    fn stream_index(&self) -> c_int {
        unsafe { (*self.ptr).stream_index }
    }

    fn unref(&mut self) {
        unsafe { ffi::av_packet_unref(self.ptr) };
    }
}

impl Drop for AvPacket {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_packet_free(&mut self.ptr) };
        }
    }
}

struct AvFrame {
    ptr: *mut ffi::AVFrame,
}

impl AvFrame {
    fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_frame_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 frame 失败".to_string());
        }
        Ok(Self { ptr })
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.ptr
    }

    fn unref(&mut self) {
        unsafe { ffi::av_frame_unref(self.ptr) };
    }
}

impl Drop for AvFrame {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_frame_free(&mut self.ptr) };
        }
    }
}

struct VideoScaler {
    ptr: *mut ffi::SwsContext,
    size: RenderSize,
}

impl VideoScaler {
    fn new(decoder: &Decoder) -> std::result::Result<Self, String> {
        let size = decoder.size()?;
        let src_format = unsafe { (*decoder.ptr).pix_fmt };
        let ptr = unsafe {
            ffi::sws_getContext(
                i32::try_from(size.width).map_err(|_| "视频宽度过大".to_string())?,
                i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?,
                src_format,
                i32::try_from(size.width).map_err(|_| "视频宽度过大".to_string())?,
                i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                ffi::SwsFlags::SWS_BILINEAR as c_int,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ptr.is_null() {
            return Err("FFmpeg 创建视频色彩转换器失败".to_string());
        }
        Ok(Self { ptr, size })
    }

    fn convert(&mut self, frame: *mut ffi::AVFrame) -> std::result::Result<Vec<u8>, String> {
        let size = frame_size(frame).unwrap_or(self.size);
        if size != self.size {
            return Err("FFmpeg 暂不支持播放中切换视频尺寸".to_string());
        }

        let mut pixels = vec![0; video_frame_len(size)?];
        let mut dst_data = [ptr::null_mut(); 4];
        let mut dst_linesize = [0; 4];
        dst_data[0] = pixels.as_mut_ptr();
        dst_linesize[0] = i32::try_from(size.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| "视频帧 stride 过大".to_string())?;

        let height = i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?;
        let scaled = unsafe {
            ffi::sws_scale(
                self.ptr,
                (*frame).data.as_ptr() as *const *const u8,
                (*frame).linesize.as_ptr(),
                0,
                height,
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
            )
        };
        if scaled != height {
            return Err("FFmpeg 转换视频帧失败".to_string());
        }
        Ok(pixels)
    }
}

impl Drop for VideoScaler {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::sws_freeContext(self.ptr) };
        }
    }
}

struct AudioResampler {
    ptr: *mut ffi::SwrContext,
    output_rate: c_int,
    output_layout: ffi::AVChannelLayout,
    input_rate: c_int,
}

impl AudioResampler {
    fn new(decoder: &Decoder) -> std::result::Result<Self, String> {
        let input_rate = unsafe { (*decoder.ptr).sample_rate };
        if input_rate <= 0 {
            return Err("FFmpeg 音频采样率无效".to_string());
        }

        let mut output_layout = zeroed_channel_layout();
        unsafe { ffi::av_channel_layout_default(&mut output_layout, AUDIO_OUTPUT_CHANNELS) };

        let mut fallback_input_layout = zeroed_channel_layout();
        let input_layout = unsafe {
            if (*decoder.ptr).ch_layout.nb_channels > 0
                && ffi::av_channel_layout_check(&(*decoder.ptr).ch_layout) > 0
            {
                &(*decoder.ptr).ch_layout as *const ffi::AVChannelLayout
            } else {
                let channels = (*decoder.ptr)
                    .ch_layout
                    .nb_channels
                    .max(AUDIO_OUTPUT_CHANNELS);
                ffi::av_channel_layout_default(&mut fallback_input_layout, channels);
                &fallback_input_layout as *const ffi::AVChannelLayout
            }
        };

        let mut ptr = ptr::null_mut();
        let result = unsafe {
            ffi::swr_alloc_set_opts2(
                &mut ptr,
                &output_layout,
                ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                input_rate,
                input_layout,
                (*decoder.ptr).sample_fmt,
                input_rate,
                0,
                ptr::null_mut(),
            )
        };
        unsafe { ffi::av_channel_layout_uninit(&mut fallback_input_layout) };
        if result < 0 {
            unsafe { ffi::av_channel_layout_uninit(&mut output_layout) };
            return Err(format!(
                "FFmpeg 配置音频重采样失败：{}",
                ffmpeg_error(result)
            ));
        }

        let result = unsafe { ffi::swr_init(ptr) };
        if result < 0 {
            unsafe {
                ffi::swr_free(&mut ptr);
                ffi::av_channel_layout_uninit(&mut output_layout);
            }
            return Err(format!(
                "FFmpeg 初始化音频重采样失败：{}",
                ffmpeg_error(result)
            ));
        }

        Ok(Self {
            ptr,
            output_rate: input_rate,
            output_layout,
            input_rate,
        })
    }

    fn output_rate(&self) -> c_int {
        self.output_rate
    }

    fn convert(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> std::result::Result<Option<DecodedAudio>, String> {
        let input_samples = unsafe { (*frame).nb_samples };
        if input_samples <= 0 {
            return Ok(None);
        }

        let output_samples = unsafe {
            ffi::av_rescale_rnd(
                ffi::swr_get_delay(self.ptr, self.input_rate as i64) + input_samples as i64,
                self.output_rate as i64,
                self.input_rate as i64,
                ffi::AVRounding::AV_ROUND_UP,
            )
        };
        if output_samples <= 0 {
            return Ok(None);
        }
        let output_samples =
            c_int::try_from(output_samples).map_err(|_| "音频输出采样数过大".to_string())?;
        let buffer_len = audio_buffer_len(output_samples, AUDIO_OUTPUT_CHANNELS)?;
        let mut samples = vec![0u8; buffer_len];
        let output_planes = [samples.as_mut_ptr()];
        let input_planes = unsafe {
            if !(*frame).extended_data.is_null() {
                (*frame).extended_data as *const *const u8
            } else {
                (*frame).data.as_ptr() as *const *const u8
            }
        };
        let converted = unsafe {
            ffi::swr_convert(
                self.ptr,
                output_planes.as_ptr(),
                output_samples,
                input_planes,
                input_samples,
            )
        };
        if converted < 0 {
            return Err(format!(
                "FFmpeg 转换音频帧失败：{}",
                ffmpeg_error(converted)
            ));
        }
        if converted == 0 {
            return Ok(None);
        }

        samples.truncate(audio_buffer_len(converted, AUDIO_OUTPUT_CHANNELS)?);
        let duration_nsecs =
            ((converted as u64).saturating_mul(1_000_000_000)) / self.output_rate as u64;
        Ok(Some(DecodedAudio {
            samples,
            duration_nsecs,
        }))
    }
}

impl Drop for AudioResampler {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::swr_free(&mut self.ptr) };
        }
        unsafe { ffi::av_channel_layout_uninit(&mut self.output_layout) };
    }
}

struct DecodedAudio {
    samples: Vec<u8>,
    duration_nsecs: u64,
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

fn audio_buffer_len(samples: c_int, channels: c_int) -> std::result::Result<usize, String> {
    if samples < 0 || channels <= 0 {
        return Err("音频帧尺寸无效".to_string());
    }
    usize::try_from(samples)
        .ok()
        .and_then(|samples| samples.checked_mul(usize::try_from(channels).ok()?))
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| "音频帧过大".to_string())
}

fn zeroed_channel_layout() -> ffi::AVChannelLayout {
    unsafe { std::mem::zeroed() }
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
    control.should_stop() as c_int
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
    fn audio_buffer_len_rejects_invalid_sizes() {
        assert!(audio_buffer_len(-1, AUDIO_OUTPUT_CHANNELS).is_err());
        assert!(audio_buffer_len(1024, 0).is_err());
        assert_eq!(
            audio_buffer_len(1024, AUDIO_OUTPUT_CHANNELS).unwrap(),
            1024 * AUDIO_OUTPUT_CHANNELS as usize * std::mem::size_of::<f32>()
        );
    }
}
