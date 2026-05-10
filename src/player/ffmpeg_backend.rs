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

static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct FfmpegBackend {
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    event_rx: Receiver<BackendEvent>,
    worker: Option<FfmpegWorker>,
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
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}

        self.worker = Some(FfmpegWorker::spawn(
            url.to_string(),
            self.frame_slot.clone(),
            self.event_tx.clone(),
        )?);
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
        self.stop_worker();
        self.frame_slot.clear();
    }
}

struct FfmpegWorker {
    shutdown: Arc<AtomicBool>,
    pipeline: Arc<Mutex<Option<gst::Pipeline>>>,
    handle: JoinHandle<()>,
}

impl FfmpegWorker {
    fn spawn(url: String, frame_slot: FrameSlot, event_tx: Sender<BackendEvent>) -> Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let pipeline = Arc::new(Mutex::new(None));
        let frame_presented = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_pipeline = Arc::clone(&pipeline);
        let worker_presented = Arc::clone(&frame_presented);

        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-backend".to_string())
            .spawn(move || {
                let result = run_ffmpeg_playback(
                    &url,
                    frame_slot,
                    event_tx.clone(),
                    worker_shutdown.clone(),
                    worker_pipeline,
                    worker_presented.clone(),
                );

                if worker_shutdown.load(Ordering::Relaxed) {
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
            shutdown,
            pipeline,
            handle,
        })
    }

    fn stop(self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(pipeline) = self
            .pipeline
            .lock()
            .expect("FFmpeg pipeline handle poisoned")
            .as_ref()
        {
            let _ = pipeline.set_state(gst::State::Null);
        }
        let _ = self.handle.join();
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

fn run_ffmpeg_playback(
    url: &str,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    shutdown: Arc<AtomicBool>,
    pipeline_handle: Arc<Mutex<Option<gst::Pipeline>>>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let mut input = FormatContext::open(url, Arc::clone(&shutdown))?;
    input.find_stream_info()?;

    let video_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)?
        .ok_or_else(|| "FFmpeg 未找到可解码视频流".to_string())?;
    let video_decoder = Decoder::open(video_stream)?;
    let mut video_frame = AvFrame::new()?;
    let mut video_scaler = VideoScaler::new(&video_decoder)?;
    let mut video_clock = TimestampMapper::new(video_stream.start_nsecs);

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
    let mut audio_clock = TimestampMapper::new(audio_stream.and_then(|stream| stream.start_nsecs));

    let mut sinks = FfmpegPlaybackSinks::new(
        video_decoder.size()?,
        audio_resampler.as_ref().map(AudioResampler::output_rate),
        frame_slot,
        event_tx.clone(),
        frame_presented,
        pipeline_handle,
    )?;

    if let Some(duration) = input.duration_seconds() {
        let _ = event_tx.send(BackendEvent::DurationChanged(duration));
    }

    let mut packet = AvPacket::new()?;
    let mut position_reporter = PositionReporter::default();

    while !shutdown.load(Ordering::Relaxed) {
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
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base)
                        .unwrap_or(0);
                    let pixels = video_scaler.convert(frame)?;
                    sinks.push_video(pixels, timestamp)?;
                    position_reporter.report(timestamp, &event_tx);
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
                    let timestamp = audio_clock
                        .map(frame_best_effort_timestamp(frame), decoder.time_base)
                        .unwrap_or(0);
                    if let Some(audio) = resampler.convert(frame)? {
                        sinks.push_audio(audio, timestamp)?;
                    }
                    Ok(())
                })
            }
            _ => Ok(()),
        };
        packet.unref();
        process_result?;
    }

    if shutdown.load(Ordering::Relaxed) {
        return Ok(());
    }

    video_decoder.flush(&mut video_frame, |frame| {
        let timestamp = video_clock
            .map(frame_best_effort_timestamp(frame), video_decoder.time_base)
            .unwrap_or(0);
        let pixels = video_scaler.convert(frame)?;
        sinks.push_video(pixels, timestamp)?;
        position_reporter.report(timestamp, &event_tx);
        Ok(())
    })?;

    if let (Some(decoder), Some(frame), Some(resampler)) = (
        audio_decoder.as_ref(),
        audio_frame.as_mut(),
        audio_resampler.as_mut(),
    ) {
        decoder.flush(frame, |frame| {
            let timestamp = audio_clock
                .map(frame_best_effort_timestamp(frame), decoder.time_base)
                .unwrap_or(0);
            if let Some(audio) = resampler.convert(frame)? {
                sinks.push_audio(audio, timestamp)?;
            }
            Ok(())
        })?;
    }

    sinks.end_of_stream();
    Ok(())
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
            .block(true)
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
                .block(true)
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

    fn push_video(&mut self, pixels: Vec<u8>, pts_nsecs: u64) -> std::result::Result<(), String> {
        let mut buffer = gst::Buffer::from_mut_slice(pixels);
        let buffer_ref = buffer
            .get_mut()
            .ok_or_else(|| "FFmpeg 视频 buffer 不可写".to_string())?;
        buffer_ref.set_pts(gst::ClockTime::from_nseconds(pts_nsecs));
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
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn build_video_appsink(
    caps: gst::Caps,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    frame_presented: Arc<AtomicBool>,
) -> gst_app::AppSink {
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
    let pts = buffer.pts().map(|pts| FramePts {
        nsecs: pts.nseconds(),
    });
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
    fn open(url: &str, shutdown: Arc<AtomicBool>) -> std::result::Result<Self, String> {
        let url = CString::new(url).map_err(|_| "播放地址包含无效字符".to_string())?;
        let mut context = unsafe { ffi::avformat_alloc_context() };
        if context.is_null() {
            return Err("FFmpeg 分配输入上下文失败".to_string());
        }

        unsafe {
            (*context).interrupt_callback.callback = Some(ffmpeg_interrupt_callback);
            (*context).interrupt_callback.opaque = Arc::as_ptr(&shutdown) as *mut c_void;
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
        Ok(Some(StreamInfo {
            index,
            stream,
            decoder,
            time_base,
            start_nsecs,
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

struct TimestampMapper {
    start_nsecs: Option<u64>,
}

impl TimestampMapper {
    fn new(start_nsecs: Option<u64>) -> Self {
        Self { start_nsecs }
    }

    fn map(&mut self, timestamp: i64, time_base: ffi::AVRational) -> Option<u64> {
        let nsecs = timestamp_to_nsecs(timestamp, time_base)?;
        let start = *self.start_nsecs.get_or_insert(nsecs);
        Some(nsecs.saturating_sub(start))
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
    let shutdown = unsafe { &*(opaque as *const AtomicBool) };
    shutdown.load(Ordering::Relaxed) as c_int
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_mapper_uses_stream_start_when_available() {
        let mut mapper = TimestampMapper::new(Some(1_000_000_000));
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(mapper.map(1_250, time_base), Some(250_000_000));
    }

    #[test]
    fn timestamp_mapper_uses_first_timestamp_without_stream_start() {
        let mut mapper = TimestampMapper::new(None);
        let time_base = ffi::AVRational { num: 1, den: 1_000 };

        assert_eq!(mapper.map(500, time_base), Some(0));
        assert_eq!(mapper.map(750, time_base), Some(250_000_000));
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
