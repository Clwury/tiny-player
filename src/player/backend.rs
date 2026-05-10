use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::dovi::{DoviFrameMetadata, DoviRpuExtractor, HevcStreamFormat};
use super::render_host::{
    DecodedFrame, FrameColor, FrameDynamicMetadata, FramePixels, FramePts, FrameSlot,
    RawVideoBufferPlane, RawVideoChromaSite, RawVideoDmaBufPlane, RawVideoFormat, RawVideoFrame,
    RawVideoPlanes, RawVideoRange, RenderSize, packed_bgra_from_stride,
    validate_raw_plane_from_stride,
};

const POSITION_QUERY_INTERVAL: Duration = Duration::from_millis(250);
const RPU_MATCH_TOLERANCE: Duration = Duration::from_millis(60);
const RPU_SLOT_CAPACITY: usize = 2048;
const DMABUF_MEMORY_FEATURE: &str = "memory:DMABuf";
const FALLBACK_VIDEO_FRAME_DURATION_NSECS: u64 = 1_000_000_000 / 24;
static NEGOTIATED_CAPS_LOGGED: AtomicBool = AtomicBool::new(false);
static NEGOTIATED_DMABUF_LOGGED: AtomicBool = AtomicBool::new(false);
static RAW_PREROLL_COUNT: AtomicU64 = AtomicU64::new(0);
static RAW_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn gst_is_dmabuf_memory(mem: *mut gst::ffi::GstMemory) -> gst::glib::ffi::gboolean;
    fn gst_dmabuf_memory_get_fd(mem: *mut gst::ffi::GstMemory) -> std::os::raw::c_int;
}

#[derive(Debug)]
pub enum BackendEvent {
    Pause(bool),
    PlaybackRestart,
    VideoSizeChanged(Option<RenderSize>),
    Buffering(bool),
    PositionChanged(f64),
    DurationChanged(f64),
    BufferedChanged(Option<f64>),
    LoadFailed(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum BackendError {
    EmptyUrl,
    Ffmpeg(String),
    GStreamer(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "播放地址为空"),
            Self::Ffmpeg(error) => error.fmt(f),
            Self::GStreamer(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BackendError {}

#[derive(Debug)]
struct RpuSlot {
    entries: VecDeque<RpuEntry>,
    max_entries: usize,
}

#[derive(Debug)]
struct RpuEntry {
    pts: FramePts,
    metadata: DoviFrameMetadata,
}

impl RpuSlot {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries,
        }
    }

    fn push(&mut self, pts: FramePts, metadata: DoviFrameMetadata) {
        if self.max_entries == 0 {
            return;
        }

        self.entries.push_back(RpuEntry { pts, metadata });
        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }

    #[cfg(test)]
    fn take_nearest(&mut self, pts: FramePts, tolerance: Duration) -> Option<DoviFrameMetadata> {
        self.take_nearest_or_closest(pts, tolerance, false)
    }

    fn take_nearest_or_closest(
        &mut self,
        pts: FramePts,
        tolerance: Duration,
        allow_closest: bool,
    ) -> Option<DoviFrameMetadata> {
        let tolerance = duration_nsecs(tolerance);
        let (index, distance) = self.nearest_index_and_distance(pts)?;
        if distance > tolerance && !allow_closest {
            self.prune_before(pts, tolerance);
            return None;
        }

        self.entries.remove(index).map(|entry| entry.metadata)
    }

    fn nearest_index_and_distance(&self, pts: FramePts) -> Option<(usize, u64)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, pts_distance(entry.pts, pts)))
            .min_by_key(|(_, distance)| *distance)
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.entries.clear();
    }

    fn prune_before(&mut self, pts: FramePts, tolerance: u64) {
        while self
            .entries
            .front()
            .is_some_and(|entry| entry.pts.nsecs.saturating_add(tolerance) < pts.nsecs)
        {
            self.entries.pop_front();
        }
    }
}

impl Default for RpuSlot {
    fn default() -> Self {
        Self::new(RPU_SLOT_CAPACITY)
    }
}

#[derive(Clone, Default)]
struct DoviState {
    rpu_slot: Arc<Mutex<RpuSlot>>,
    last_matched_rpu: Arc<Mutex<Option<DoviFrameMetadata>>>,
    latest_profile5_rpu: Arc<Mutex<Option<DoviFrameMetadata>>>,
    profile5_seen: Arc<AtomicBool>,
}

impl DoviState {
    fn push_rpu(&self, pts: FramePts, metadata: DoviFrameMetadata) {
        if metadata.profile == 5 {
            self.profile5_seen.store(true, Ordering::Relaxed);
            self.remember_latest_profile5_rpu(metadata.clone());
        }
        self.rpu_slot
            .lock()
            .expect("Dolby Vision RPU slot poisoned")
            .push(pts, metadata);
    }

    fn take_rpu_for_frame(&self, pts: Option<FramePts>) -> Option<DoviFrameMetadata> {
        match pts {
            Some(pts) => self.take_rpu(pts),
            None => self.fallback_rpu(),
        }
    }

    fn take_rpu(&self, pts: FramePts) -> Option<DoviFrameMetadata> {
        let last_matched = self.last_matched_rpu();
        let latest_profile5 = self.latest_profile5_rpu();
        let mut slot = self
            .rpu_slot
            .lock()
            .expect("Dolby Vision RPU slot poisoned");
        let metadata = slot.take_nearest_or_closest(pts, RPU_MATCH_TOLERANCE, false);
        drop(slot);

        if let Some(metadata) = metadata {
            self.remember_matched_rpu(metadata.clone());
            return Some(metadata);
        }

        last_matched.or(latest_profile5)
    }

    fn fallback_rpu(&self) -> Option<DoviFrameMetadata> {
        self.last_matched_rpu()
            .or_else(|| self.latest_profile5_rpu())
    }

    fn remember_matched_rpu(&self, metadata: DoviFrameMetadata) {
        *self
            .last_matched_rpu
            .lock()
            .expect("Dolby Vision last RPU cache poisoned") = Some(metadata);
    }

    fn remember_latest_profile5_rpu(&self, metadata: DoviFrameMetadata) {
        *self
            .latest_profile5_rpu
            .lock()
            .expect("Dolby Vision latest Profile 5 RPU cache poisoned") = Some(metadata);
    }

    fn last_matched_rpu(&self) -> Option<DoviFrameMetadata> {
        self.last_matched_rpu
            .lock()
            .expect("Dolby Vision last RPU cache poisoned")
            .clone()
    }

    fn latest_profile5_rpu(&self) -> Option<DoviFrameMetadata> {
        self.latest_profile5_rpu
            .lock()
            .expect("Dolby Vision latest Profile 5 RPU cache poisoned")
            .clone()
    }

    fn profile5_seen(&self) -> bool {
        self.profile5_seen.load(Ordering::Relaxed)
    }
}

fn pts_distance(first: FramePts, second: FramePts) -> u64 {
    first.nsecs.abs_diff(second.nsecs)
}

fn duration_nsecs(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

enum PipelineKind {
    Playbin(gst::Element),
    Custom {
        pipeline: gst::Pipeline,
        source: gst::Element,
    },
}

impl PipelineKind {
    fn root(&self) -> &gst::Element {
        match self {
            Self::Playbin(playbin) => playbin,
            Self::Custom { pipeline, .. } => pipeline.upcast_ref(),
        }
    }

    fn source(&self) -> Option<&gst::Element> {
        match self {
            Self::Playbin(_) => None,
            Self::Custom { source, .. } => Some(source),
        }
    }
}

#[derive(Clone, Copy)]
enum CacheStream {
    Video,
    Audio,
}

#[derive(Debug, Default)]
struct PlayableCacheProgress {
    base_position: AtomicU64,
    video_buffered_until: AtomicU64,
    audio_buffered_until: AtomicU64,
    video_seen: AtomicBool,
    audio_seen: AtomicBool,
    needs_audio: AtomicBool,
}

impl PlayableCacheProgress {
    fn reset_to_seconds(&self, position_seconds: f64) {
        let position_nsecs = seconds_to_timeline_nsecs(position_seconds);
        let needs_audio = self.needs_audio.load(Ordering::Relaxed);

        self.base_position.store(position_nsecs, Ordering::Relaxed);
        self.video_buffered_until
            .store(position_nsecs, Ordering::Relaxed);
        self.audio_buffered_until
            .store(position_nsecs, Ordering::Relaxed);
        self.video_seen.store(true, Ordering::Relaxed);
        self.audio_seen.store(needs_audio, Ordering::Relaxed);
    }

    fn mark_audio_present(&self) {
        self.needs_audio.store(true, Ordering::Relaxed);
        self.audio_buffered_until.fetch_max(
            self.base_position.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.audio_seen.store(true, Ordering::Relaxed);
    }

    fn observe_buffer(&self, stream: CacheStream, buffer: &gst::BufferRef) {
        let fallback_duration = match stream {
            CacheStream::Video => FALLBACK_VIDEO_FRAME_DURATION_NSECS,
            CacheStream::Audio => 0,
        };
        let Some(buffered_until) = buffer_timeline_end_nsecs(buffer, fallback_duration) else {
            return;
        };

        match stream {
            CacheStream::Video => self.observe_video_until_nsecs(buffered_until),
            CacheStream::Audio => self.observe_audio_until_nsecs(buffered_until),
        }
    }

    fn observe_video_until_nsecs(&self, buffered_until: u64) {
        self.video_buffered_until
            .fetch_max(buffered_until, Ordering::Relaxed);
        self.video_seen.store(true, Ordering::Relaxed);
    }

    fn observe_audio_until_nsecs(&self, buffered_until: u64) {
        self.audio_buffered_until
            .fetch_max(buffered_until, Ordering::Relaxed);
        self.audio_seen.store(true, Ordering::Relaxed);
    }

    fn buffered_until_seconds(&self) -> Option<f64> {
        combined_playable_cache_nsecs(
            self.video_seen.load(Ordering::Relaxed),
            self.video_buffered_until.load(Ordering::Relaxed),
            self.needs_audio.load(Ordering::Relaxed),
            self.audio_seen.load(Ordering::Relaxed),
            self.audio_buffered_until.load(Ordering::Relaxed),
        )
        .map(timeline_nsecs_to_seconds)
    }
}

pub struct GstBackend {
    pipeline: PipelineKind,
    bus: gst::Bus,
    frame_slot: FrameSlot,
    loaded: bool,
    paused: bool,
    buffering: bool,
    ended: bool,
    last_position: Option<f64>,
    last_duration: Option<f64>,
    last_buffered_until: Option<f64>,
    last_position_query: Instant,
    cache_progress: Arc<PlayableCacheProgress>,
}

impl GstBackend {
    pub fn new() -> Result<Self> {
        gst::init().map_err(gstreamer_error)?;

        let frame_slot = FrameSlot::default();
        let (pipeline, bus) = build_playbin_pipeline(None, frame_slot.clone())?;
        let cache_progress = Arc::new(PlayableCacheProgress::default());

        Ok(Self {
            pipeline,
            bus,
            frame_slot,
            loaded: false,
            paused: true,
            buffering: false,
            ended: false,
            last_position: None,
            last_duration: None,
            last_buffered_until: None,
            last_position_query: Instant::now(),
            cache_progress,
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

        self.pipeline
            .root()
            .set_state(gst::State::Null)
            .map_err(gstreamer_error)?;
        self.frame_slot.clear();
        self.loaded = false;
        self.paused = true;
        self.buffering = false;
        self.ended = false;
        self.last_position = None;
        self.last_duration = None;
        self.last_buffered_until = None;
        self.last_position_query = Instant::now();
        self.cache_progress = Arc::new(PlayableCacheProgress::default());
        self.cache_progress.reset_to_seconds(0.0);
        NEGOTIATED_CAPS_LOGGED.store(false, Ordering::Relaxed);
        NEGOTIATED_DMABUF_LOGGED.store(false, Ordering::Relaxed);
        RAW_PREROLL_COUNT.store(0, Ordering::Relaxed);
        RAW_SAMPLE_COUNT.store(0, Ordering::Relaxed);

        let cache_progress = Arc::clone(&self.cache_progress);
        let (pipeline, bus) =
            match build_custom_pipeline(url, self.frame_slot.clone(), cache_progress) {
                Ok(pipeline) => Ok(pipeline),
                Err(error) => {
                    tracing::warn!(%error, "falling back to GStreamer playbin pipeline");
                    build_playbin_pipeline(Some(url), self.frame_slot.clone())
                }
            }?;
        self.pipeline = pipeline;
        self.bus = bus;
        self.pipeline
            .root()
            .set_state(gst::State::Playing)
            .map_err(gstreamer_error)?;
        Ok(())
    }

    pub fn seek_to(&mut self, position_seconds: f64) -> Result<()> {
        let position_seconds = position_seconds.max(0.0);
        let seek_position =
            gst::ClockTime::try_from_seconds_f64(position_seconds).map_err(gstreamer_error)?;

        self.pipeline
            .root()
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                seek_position,
            )
            .map_err(gstreamer_error)?;
        self.ended = false;
        self.paused = false;
        self.last_position = Some(position_seconds);
        self.last_buffered_until = None;
        self.cache_progress.reset_to_seconds(position_seconds);
        self.pipeline
            .root()
            .set_state(gst::State::Playing)
            .map_err(gstreamer_error)?;
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();

        let messages = self.bus.iter().collect::<Vec<_>>();
        for message in messages {
            match message.view() {
                gst::MessageView::Error(error) => {
                    trace_gstreamer_error(error);
                    let message = gstreamer_message_error(error);
                    if self.loaded {
                        events.push(BackendEvent::Fatal(message));
                    } else {
                        events.push(BackendEvent::LoadFailed(message));
                    }
                }
                gst::MessageView::Warning(warning) => {
                    trace_gstreamer_warning(warning);
                }
                gst::MessageView::DurationChanged(_) => {
                    self.push_duration_event(&mut events);
                }
                gst::MessageView::Eos(_) => {
                    self.ended = true;
                    self.paused = true;
                    events.push(BackendEvent::Pause(true));
                }
                gst::MessageView::Buffering(buffering) => {
                    self.update_buffering_state(buffering.percent(), &mut events);
                }
                _ => {}
            }
        }

        self.push_frame_size_events(&mut events);
        self.push_playing_state_event(&mut events);
        self.push_position_events(&mut events);
        events
    }

    fn push_frame_size_events(&mut self, events: &mut Vec<BackendEvent>) {
        let Some(size) = self.frame_slot.take_size_change() else {
            return;
        };

        if !self.loaded {
            self.loaded = true;
            events.push(BackendEvent::PlaybackRestart);
            if self.buffering && self.paused {
                let _ = self.pipeline.root().set_state(gst::State::Playing);
                self.paused = false;
            }
        }
        events.push(BackendEvent::VideoSizeChanged(Some(size)));
    }

    fn push_playing_state_event(&mut self, events: &mut Vec<BackendEvent>) {
        let root = self.pipeline.root();
        let current = root.current_state();
        let pending = root.pending_state();
        let paused = self.ended
            || (!self.loaded && current != gst::State::Playing && pending != gst::State::Playing);
        if self.paused != paused {
            self.paused = paused;
            events.push(BackendEvent::Pause(paused));
        }
    }

    fn update_buffering_state(&mut self, percent: i32, events: &mut Vec<BackendEvent>) {
        let buffering = percent < 100;
        if self.buffering != buffering {
            tracing::debug!(percent, buffering, "GStreamer buffering state changed");
            self.buffering = buffering;
            events.push(BackendEvent::Buffering(buffering));
        }

        if self.paused && !self.ended {
            let _ = self.pipeline.root().set_state(gst::State::Playing);
            self.paused = false;
        }

        self.push_buffered_event(events);
    }

    fn push_position_events(&mut self, events: &mut Vec<BackendEvent>) {
        if self.last_position_query.elapsed() < POSITION_QUERY_INTERVAL {
            return;
        }
        self.last_position_query = Instant::now();

        if let Some(position) = self.query_position_seconds()
            && value_changed(self.last_position, position)
        {
            self.last_position = Some(position);
            events.push(BackendEvent::PositionChanged(position));
        }
        self.push_duration_event(events);
        self.push_buffered_event(events);
    }

    fn push_duration_event(&mut self, events: &mut Vec<BackendEvent>) {
        if let Some(duration) = self.query_duration_seconds()
            && value_changed(self.last_duration, duration)
        {
            self.last_duration = Some(duration);
            events.push(BackendEvent::DurationChanged(duration));
        }
    }

    fn push_buffered_event(&mut self, events: &mut Vec<BackendEvent>) {
        let buffered_until = self.query_buffered_until_seconds();
        if optional_value_changed(self.last_buffered_until, buffered_until) {
            self.last_buffered_until = buffered_until;
            events.push(BackendEvent::BufferedChanged(buffered_until));
        }
    }

    fn query_position_seconds(&self) -> Option<f64> {
        self.pipeline
            .root()
            .query_position::<gst::ClockTime>()
            .map(gst::ClockTime::seconds_f64)
    }

    fn query_duration_seconds(&self) -> Option<f64> {
        self.pipeline
            .root()
            .query_duration::<gst::ClockTime>()
            .map(gst::ClockTime::seconds_f64)
    }

    fn query_buffered_until_seconds(&self) -> Option<f64> {
        let position = self.last_position.or_else(|| self.query_position_seconds());
        max_optional_playback_time(
            query_gstreamer_buffered_until_seconds(self.pipeline.root(), position),
            max_optional_playback_time(
                self.pipeline
                    .source()
                    .and_then(|source| source_statistics_buffered_until_seconds(source, position)),
                self.cache_progress.buffered_until_seconds(),
            ),
        )
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        let root = self.pipeline.root().clone();
        let _ = std::thread::Builder::new()
            .name("tiny-gst-stop".to_string())
            .spawn(move || {
                let _ = root.set_state(gst::State::Null);
            });
        self.frame_slot.clear();
    }
}

fn build_playbin_pipeline(
    uri: Option<&str>,
    frame_slot: FrameSlot,
) -> Result<(PipelineKind, gst::Bus)> {
    let video_sink = build_video_sink(frame_slot)?;
    let audio_sink = build_audio_sink()?;
    let playbin = make_element("playbin")?;
    playbin.set_property("video-sink", &video_sink);
    playbin.set_property("audio-sink", &audio_sink);
    if let Some(uri) = uri {
        playbin.set_property("uri", uri);
    }
    let bus = playbin
        .bus()
        .ok_or_else(|| BackendError::GStreamer("GStreamer playbin 缺少消息总线".to_string()))?;

    Ok((PipelineKind::Playbin(playbin), bus))
}

fn make_element(factory: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(gstreamer_error)
}

fn build_custom_pipeline(
    uri: &str,
    frame_slot: FrameSlot,
    cache_progress: Arc<PlayableCacheProgress>,
) -> Result<(PipelineKind, gst::Bus)> {
    let pipeline = gst::Pipeline::new();
    let source = make_element("urisourcebin")?;
    let parsebin = make_element("parsebin")?;
    source.set_property("uri", uri);
    set_bool_property_if_exists(&source, "use-buffering", true);

    pipeline
        .add_many([&source, &parsebin])
        .map_err(gstreamer_error)?;
    connect_source_to_parsebin(&source, &parsebin)?;
    connect_parsebin_pads(
        &parsebin,
        &pipeline,
        frame_slot,
        DoviState::default(),
        cache_progress,
    );

    let bus = pipeline.bus().ok_or_else(|| {
        BackendError::GStreamer("GStreamer 自定义 pipeline 缺少消息总线".to_string())
    })?;
    Ok((PipelineKind::Custom { pipeline, source }, bus))
}

fn connect_source_to_parsebin(source: &gst::Element, parsebin: &gst::Element) -> Result<()> {
    let parsebin_sink = parsebin
        .static_pad("sink")
        .ok_or_else(|| BackendError::GStreamer("GStreamer parsebin 缺少入口 pad".to_string()))?;
    source.connect_pad_added(move |_, src_pad| {
        if parsebin_sink.is_linked() {
            return;
        }
        if let Err(error) = src_pad.link(&parsebin_sink) {
            tracing::warn!(%error, "failed to link GStreamer source to parsebin");
        }
    });
    Ok(())
}

fn connect_parsebin_pads(
    parsebin: &gst::Element,
    pipeline: &gst::Pipeline,
    frame_slot: FrameSlot,
    dovi_state: DoviState,
    cache_progress: Arc<PlayableCacheProgress>,
) {
    let pipeline = pipeline.clone();
    parsebin.connect_pad_added(move |_, src_pad| {
        if let Err(error) = link_parsebin_pad(
            &pipeline,
            src_pad,
            frame_slot.clone(),
            dovi_state.clone(),
            Arc::clone(&cache_progress),
        ) {
            tracing::warn!(%error, "failed to link GStreamer parsed stream");
        }
    });
}

fn link_parsebin_pad(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    frame_slot: FrameSlot,
    dovi_state: DoviState,
    cache_progress: Arc<PlayableCacheProgress>,
) -> Result<()> {
    if src_pad.is_linked() {
        return Ok(());
    }

    let Some(caps_name) = pad_caps_name(src_pad) else {
        return Ok(());
    };
    if caps_name == "video/x-h265" {
        link_h265_branch(pipeline, src_pad, frame_slot, dovi_state, cache_progress)
    } else if caps_name.starts_with("video/") {
        link_video_decode_branch(pipeline, src_pad, frame_slot, cache_progress)
    } else if caps_name.starts_with("audio/") {
        link_audio_decode_branch(pipeline, src_pad, cache_progress)
    } else {
        tracing::debug!(caps = %caps_name, "ignoring unsupported parsed GStreamer stream");
        Ok(())
    }
}

fn link_h265_branch(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    frame_slot: FrameSlot,
    dovi_state: DoviState,
    cache_progress: Arc<PlayableCacheProgress>,
) -> Result<()> {
    let input_queue = make_element("queue")?;
    let parser = make_element("h265parse")?;
    parser.set_property("config-interval", -1i32);
    let capsfilter = make_element("capsfilter")?;
    capsfilter.set_property("caps", hevc_au_caps());
    let decodebin = make_element("decodebin")?;
    let raw_appsink =
        build_hevc_raw_appsink(frame_slot, Some(dovi_state.clone())).upcast::<gst::Element>();

    pipeline
        .add_many([&input_queue, &parser, &capsfilter, &decodebin, &raw_appsink])
        .map_err(gstreamer_error)?;
    gst::Element::link_many([&input_queue, &parser, &capsfilter, &decodebin])
        .map_err(gstreamer_error)?;
    install_rpu_probe(&capsfilter, dovi_state)?;
    install_playable_cache_probe(src_pad, CacheStream::Video, cache_progress);
    connect_decodebin_to_sink(&decodebin, &raw_appsink, "video/")?;
    link_pad_to_element(src_pad, &input_queue)?;
    sync_elements([&input_queue, &parser, &capsfilter, &decodebin, &raw_appsink])?;
    Ok(())
}

fn link_video_decode_branch(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    frame_slot: FrameSlot,
    cache_progress: Arc<PlayableCacheProgress>,
) -> Result<()> {
    let input_queue = make_element("queue")?;
    let decodebin = make_element("decodebin")?;
    let convert = make_element("videoconvert")?;
    let scale = make_element("videoscale")?;
    let appsink = build_raw_appsink(frame_slot, None).upcast::<gst::Element>();

    pipeline
        .add_many([&input_queue, &decodebin, &convert, &scale, &appsink])
        .map_err(gstreamer_error)?;
    gst::Element::link_many([&input_queue, &decodebin]).map_err(gstreamer_error)?;
    gst::Element::link_many([&convert, &scale, &appsink]).map_err(gstreamer_error)?;
    install_playable_cache_probe(src_pad, CacheStream::Video, cache_progress);
    connect_decodebin_to_sink(&decodebin, &convert, "video/")?;
    link_pad_to_element(src_pad, &input_queue)?;
    sync_elements([&input_queue, &decodebin, &convert, &scale, &appsink])?;
    Ok(())
}

fn link_audio_decode_branch(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    cache_progress: Arc<PlayableCacheProgress>,
) -> Result<()> {
    let input_queue = make_element("queue")?;
    let decodebin = make_element("decodebin")?;
    let convert = make_element("audioconvert")?;
    let resample = make_element("audioresample")?;
    let sink = build_audio_sink()?;

    pipeline
        .add_many([&input_queue, &decodebin, &convert, &resample, &sink])
        .map_err(gstreamer_error)?;
    gst::Element::link_many([&input_queue, &decodebin]).map_err(gstreamer_error)?;
    gst::Element::link_many([&convert, &resample, &sink]).map_err(gstreamer_error)?;
    cache_progress.mark_audio_present();
    install_playable_cache_probe(src_pad, CacheStream::Audio, cache_progress);
    connect_decodebin_to_sink(&decodebin, &convert, "audio/")?;
    link_pad_to_element(src_pad, &input_queue)?;
    sync_elements([&input_queue, &decodebin, &convert, &resample, &sink])?;
    Ok(())
}

fn install_playable_cache_probe(
    src_pad: &gst::Pad,
    stream: CacheStream,
    cache_progress: Arc<PlayableCacheProgress>,
) {
    src_pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            if let Some(buffer) = info.buffer() {
                cache_progress.observe_buffer(stream, buffer);
            }
            if let Some(buffer_list) = info.buffer_list() {
                for buffer in buffer_list.iter() {
                    cache_progress.observe_buffer(stream, buffer);
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
}

fn install_rpu_probe(element: &gst::Element, dovi_state: DoviState) -> Result<()> {
    let src_pad = element.static_pad("src").ok_or_else(|| {
        BackendError::GStreamer("GStreamer HEVC RPU probe 缺少源 pad".to_string())
    })?;
    src_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(buffer) = info.buffer() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(pts) = frame_pts_from_buffer(buffer) else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(map) = buffer.map_readable() else {
            return gst::PadProbeReturn::Ok;
        };
        let mut extractor = DoviRpuExtractor;
        match extractor.extract_from_hevc_access_unit(map.as_slice(), HevcStreamFormat::ByteStream)
        {
            Ok(Some(metadata)) => {
                tracing::debug!(
                    profile = metadata.profile,
                    pts = pts.nsecs,
                    "extracted Dolby Vision RPU before HEVC decode"
                );
                dovi_state.push_rpu(pts, metadata);
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(%error, "failed to extract Dolby Vision RPU"),
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

fn connect_decodebin_to_sink(
    decodebin: &gst::Element,
    sink: &gst::Element,
    caps_prefix: &'static str,
) -> Result<()> {
    let sink_pad = sink.static_pad("sink").ok_or_else(|| {
        BackendError::GStreamer("GStreamer decodebin 下游缺少入口 pad".to_string())
    })?;
    decodebin.connect_pad_added(move |_, src_pad| {
        if sink_pad.is_linked() {
            return;
        }
        let Some(caps_name) = pad_caps_name(src_pad) else {
            return;
        };
        if !caps_name.starts_with(caps_prefix) {
            return;
        }
        if let Err(error) = src_pad.link(&sink_pad) {
            tracing::warn!(%error, "failed to link GStreamer decodebin output");
        }
    });
    Ok(())
}

fn link_pad_to_element(src_pad: &gst::Pad, element: &gst::Element) -> Result<()> {
    let sink_pad = element
        .static_pad("sink")
        .ok_or_else(|| BackendError::GStreamer("GStreamer 元素缺少入口 pad".to_string()))?;
    src_pad.link(&sink_pad).map_err(gstreamer_error)?;
    Ok(())
}

fn sync_elements<'a>(elements: impl IntoIterator<Item = &'a gst::Element>) -> Result<()> {
    for element in elements {
        element.sync_state_with_parent().map_err(gstreamer_error)?;
    }
    Ok(())
}

fn pad_caps_name(src_pad: &gst::Pad) -> Option<String> {
    let caps = src_pad
        .current_caps()
        .unwrap_or_else(|| src_pad.query_caps(None));
    caps.structure(0)
        .map(|structure| structure.name().to_string())
}

fn hevc_au_caps() -> gst::Caps {
    gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build()
}

fn build_audio_sink() -> Result<gst::Element> {
    let sink = gst::ElementFactory::make("autoaudiosink")
        .build()
        .map_err(gstreamer_error)?;
    set_bool_property_if_exists(&sink, "async", false);
    Ok(sink)
}

fn set_bool_property_if_exists(element: &gst::Element, property: &str, value: bool) {
    if element.find_property(property).is_some() {
        element.set_property(property, value);
    }
}

fn build_video_sink(frame_slot: FrameSlot) -> Result<gst::Element> {
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(gstreamer_error)?;
    let scale = gst::ElementFactory::make("videoscale")
        .build()
        .map_err(gstreamer_error)?;
    let appsink = build_appsink(frame_slot).upcast::<gst::Element>();
    let sink_bin = gst::Bin::new();

    sink_bin
        .add_many([&convert, &scale, &appsink])
        .map_err(gstreamer_error)?;
    gst::Element::link_many([&convert, &scale, &appsink]).map_err(gstreamer_error)?;

    let sink_pad = convert
        .static_pad("sink")
        .ok_or_else(|| BackendError::GStreamer("GStreamer 视频 sink 缺少入口 pad".to_string()))?;
    let ghost_pad = gst::GhostPad::with_target(&sink_pad).map_err(gstreamer_error)?;
    ghost_pad.set_active(true).map_err(gstreamer_error)?;
    sink_bin.add_pad(&ghost_pad).map_err(gstreamer_error)?;

    Ok(sink_bin.upcast())
}

fn build_appsink(frame_slot: FrameSlot) -> gst_app::AppSink {
    build_raw_appsink(frame_slot, None)
}

fn build_hevc_raw_appsink(
    frame_slot: FrameSlot,
    dovi_state: Option<DoviState>,
) -> gst_app::AppSink {
    build_raw_appsink_with_caps(frame_slot, dovi_state, hevc_raw_caps())
}

fn build_raw_appsink(frame_slot: FrameSlot, dovi_state: Option<DoviState>) -> gst_app::AppSink {
    build_raw_appsink_with_formats(
        frame_slot,
        dovi_state,
        ["P010_10LE", "I420_10LE", "BGRA"],
        false,
    )
}

fn build_raw_appsink_with_formats<const N: usize>(
    frame_slot: FrameSlot,
    dovi_state: Option<DoviState>,
    formats: [&'static str; N],
    prefer_dmabuf: bool,
) -> gst_app::AppSink {
    build_raw_appsink_with_caps(
        frame_slot,
        dovi_state,
        raw_caps_with_formats(formats, prefer_dmabuf),
    )
}

fn build_raw_appsink_with_caps(
    frame_slot: FrameSlot,
    dovi_state: Option<DoviState>,
    caps: gst::Caps,
) -> gst_app::AppSink {
    let sample_frame_slot = frame_slot.clone();
    let sample_dovi_state = dovi_state.clone();
    let preroll_frame_slot = frame_slot;
    let preroll_dovi_state = dovi_state;
    let appsink = gst_app::AppSink::builder()
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
                    push_decoded_sample(
                        sample,
                        &sample_frame_slot,
                        sample_dovi_state.as_ref(),
                        "sample",
                        &RAW_SAMPLE_COUNT,
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |appsink| {
                    let sample = appsink.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                    push_decoded_sample(
                        sample,
                        &preroll_frame_slot,
                        preroll_dovi_state.as_ref(),
                        "preroll",
                        &RAW_PREROLL_COUNT,
                    );
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        )
        .build();
    appsink.set_property("async", false);
    appsink
}

fn hevc_raw_caps() -> gst::Caps {
    raw_caps_with_formats(["P010_10LE", "I420_10LE", "NV12", "I420"], true)
}

fn raw_caps_with_formats<const N: usize>(
    formats: [&'static str; N],
    prefer_dmabuf: bool,
) -> gst::Caps {
    let raw_structure = gst::Structure::builder("video/x-raw")
        .field("format", gst::List::new(formats))
        .build();

    if !prefer_dmabuf {
        return gst::Caps::builder_full().structure(raw_structure).build();
    }

    let dmabuf_structure = gst::Structure::builder("video/x-raw")
        .field("format", "DMA_DRM")
        .build();

    gst::Caps::builder_full()
        .structure_with_features(
            dmabuf_structure,
            gst::CapsFeatures::new([DMABUF_MEMORY_FEATURE]),
        )
        .structure(raw_structure)
        .build()
}

fn push_decoded_sample(
    sample: gst::Sample,
    frame_slot: &FrameSlot,
    dovi_state: Option<&DoviState>,
    source: &'static str,
    counter: &'static AtomicU64,
) {
    match decoded_frame_from_sample(&sample, dovi_state) {
        Ok(frame) => {
            let frame_count = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if frame_count == 1 || frame_count.is_multiple_of(60) {
                tracing::debug!(
                    source,
                    frame_count,
                    pts = ?frame.pts.map(|pts| pts.nsecs),
                    width = frame.size.width,
                    height = frame.size.height,
                    "decoded GStreamer raw video frame"
                );
            }
            frame_slot.push(frame);
        }
        Err(error) => tracing::debug!(source, %error, "failed to copy GStreamer video frame"),
    }
}

fn decoded_frame_from_sample(
    sample: &gst::Sample,
    dovi_state: Option<&DoviState>,
) -> std::result::Result<DecodedFrame, String> {
    let caps = sample.caps().ok_or_else(|| "视频帧缺少 caps".to_string())?;
    if gst_video::is_dma_drm_caps(caps) {
        return decoded_dmabuf_frame_from_sample(sample, caps, dovi_state);
    }

    let info = gst_video::VideoInfo::from_caps(caps).map_err(|error| error.to_string())?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| "视频帧缺少 buffer".to_string())?;
    let pts = frame_pts_from_buffer(buffer);
    let size = RenderSize {
        width: info.width(),
        height: info.height(),
    };
    let raw_format = RawVideoFormat::from_gstreamer_name(info.name());
    let dovi_metadata = dovi_state.and_then(|state| state.take_rpu_for_frame(pts));
    let color = decoded_frame_color(&info, dovi_state, pts, dovi_metadata.as_ref());
    trace_negotiated_caps(&info, color);

    let pixels = if let Some(format) = raw_format {
        let metadata = dovi_metadata.map(|dolby_vision| FrameDynamicMetadata {
            dolby_vision: Some(dolby_vision),
        });
        FramePixels::RawVideo(raw_video_frame_from_sample(
            &info,
            size,
            format,
            color,
            metadata,
            sample
                .buffer_owned()
                .ok_or_else(|| "视频帧缺少 buffer".to_string())?,
        )?)
    } else {
        match info.name() {
            "BGRA" => {
                if matches!(
                    color,
                    FrameColor::Hdr10Bt2020 | FrameColor::DolbyVisionProfile5
                ) {
                    return Err(format!(
                        "{color:?} 视频协商到 8-bit BGRA，已拒绝 HDR/Dolby Vision 降级"
                    ));
                }
                let map = buffer.map_readable().map_err(|error| error.to_string())?;
                let stride = info
                    .stride()
                    .first()
                    .copied()
                    .ok_or_else(|| "视频帧缺少 stride".to_string())?;
                let stride =
                    usize::try_from(stride).map_err(|_| "视频帧 stride 无效".to_string())?;
                FramePixels::Bgra8(
                    packed_bgra_from_stride(map.as_slice(), size, stride)
                        .map_err(|error| error.to_string())?,
                )
            }
            _ => return Err(format!("不支持的视频帧格式：{}", info.name())),
        }
    };

    Ok(DecodedFrame { size, pts, pixels })
}

fn decoded_dmabuf_frame_from_sample(
    sample: &gst::Sample,
    caps: &gst::CapsRef,
    dovi_state: Option<&DoviState>,
) -> std::result::Result<DecodedFrame, String> {
    let drm_info =
        gst_video::VideoInfoDmaDrm::from_caps(caps).map_err(|error| error.to_string())?;
    let buffer = sample
        .buffer()
        .ok_or_else(|| "视频帧缺少 buffer".to_string())?;
    let pts = frame_pts_from_buffer(buffer);
    let size = RenderSize {
        width: drm_info.width(),
        height: drm_info.height(),
    };
    let raw_format = raw_video_format_from_dma_drm(&drm_info).ok_or_else(|| {
        format!(
            "不支持的 DMA_DRM 视频帧格式：fourcc=0x{:08x}, modifier=0x{:016x}",
            drm_info.fourcc(),
            drm_info.modifier()
        )
    })?;
    let dovi_metadata = dovi_state.and_then(|state| state.take_rpu_for_frame(pts));
    let color = decoded_frame_color(&drm_info, dovi_state, pts, dovi_metadata.as_ref());
    trace_negotiated_caps(&drm_info, color);

    let metadata = dovi_metadata.map(|dolby_vision| FrameDynamicMetadata {
        dolby_vision: Some(dolby_vision),
    });
    let raw = dmabuf_raw_video_frame_from_sample(
        &drm_info,
        size,
        raw_format,
        color,
        metadata,
        sample
            .buffer_owned()
            .ok_or_else(|| "视频帧缺少 buffer".to_string())?,
    )?;

    Ok(DecodedFrame {
        size,
        pts,
        pixels: FramePixels::RawVideo(raw),
    })
}

fn decoded_frame_color(
    info: &gst_video::VideoInfo,
    dovi_state: Option<&DoviState>,
    pts: Option<FramePts>,
    dovi_metadata: Option<&DoviFrameMetadata>,
) -> FrameColor {
    match dovi_metadata.map(|metadata| metadata.profile) {
        Some(5) => FrameColor::DolbyVisionProfile5,
        Some(_) => frame_color(info),
        None if dovi_state.is_some_and(DoviState::profile5_seen) => {
            tracing::debug!(
                pts = ?pts.map(|pts| pts.nsecs),
                "Dolby Vision Profile 5 frame is missing RPU metadata; using negotiated color"
            );
            frame_color(info)
        }
        None => frame_color(info),
    }
}

fn frame_pts_from_buffer(buffer: &gst::BufferRef) -> Option<FramePts> {
    buffer.pts().map(|pts| FramePts {
        nsecs: pts.nseconds(),
    })
}

fn raw_video_frame_from_sample(
    info: &gst_video::VideoInfo,
    size: RenderSize,
    format: RawVideoFormat,
    color: FrameColor,
    metadata: Option<FrameDynamicMetadata>,
    buffer: gst::Buffer,
) -> std::result::Result<RawVideoFrame, String> {
    let buffer_size = buffer.size();
    let mut planes = Vec::with_capacity(format.plane_count());
    for plane_index in 0..format.plane_count() {
        let layout = format
            .plane_layout(size, plane_index)
            .map_err(|error| error.to_string())?;
        let offset = *info
            .offset()
            .get(plane_index)
            .ok_or_else(|| "视频帧缺少 plane offset".to_string())?;
        let stride = info
            .stride()
            .get(plane_index)
            .copied()
            .ok_or_else(|| "视频帧缺少 stride".to_string())?;
        let stride = usize::try_from(stride).map_err(|_| "视频帧 stride 无效".to_string())?;
        validate_raw_plane_from_stride(buffer_size, offset, stride, layout.row_len, layout.height)
            .map_err(|error| error.to_string())?;
        planes.push(RawVideoBufferPlane { offset, stride });
    }

    Ok(RawVideoFrame {
        format,
        color,
        range: raw_video_range(info.colorimetry().range()),
        chroma_site: raw_video_chroma_site(info.chroma_site()),
        metadata,
        planes: RawVideoPlanes::GStreamer { buffer, planes },
    })
}

fn dmabuf_raw_video_frame_from_sample(
    info: &gst_video::VideoInfoDmaDrm,
    size: RenderSize,
    format: RawVideoFormat,
    color: FrameColor,
    metadata: Option<FrameDynamicMetadata>,
    buffer: gst::Buffer,
) -> std::result::Result<RawVideoFrame, String> {
    let mut planes = Vec::with_capacity(format.plane_count());
    {
        let meta = buffer
            .meta::<gst_video::VideoMeta>()
            .ok_or_else(|| "DMA_DRM 视频帧缺少 VideoMeta".to_string())?;
        if meta.n_planes() as usize != format.plane_count() {
            return Err("DMA_DRM 视频帧 plane 数量不匹配".to_string());
        }

        let plane_sizes = meta.plane_size().ok();
        let plane_heights = meta.plane_height().ok();
        for plane_index in 0..format.plane_count() {
            let layout = format
                .plane_layout(size, plane_index)
                .map_err(|error| error.to_string())?;
            let buffer_offset = *meta
                .offset()
                .get(plane_index)
                .ok_or_else(|| "DMA_DRM 视频帧缺少 plane offset".to_string())?;
            let stride = meta
                .stride()
                .get(plane_index)
                .copied()
                .ok_or_else(|| "DMA_DRM 视频帧缺少 stride".to_string())?;
            let stride =
                usize::try_from(stride).map_err(|_| "DMA_DRM 视频帧 stride 无效".to_string())?;
            if stride < layout.row_len {
                return Err("DMA_DRM 视频帧 stride 无效".to_string());
            }

            let meta_height = plane_heights
                .as_ref()
                .and_then(|heights| heights.get(plane_index).copied())
                .filter(|height| *height > 0)
                .unwrap_or(layout.height);
            let computed_span = plane_span_from_stride(stride, layout.row_len, meta_height)?;
            let plane_span = plane_sizes
                .as_ref()
                .and_then(|sizes| sizes.get(plane_index).copied())
                .filter(|size| *size > 0)
                .unwrap_or(computed_span)
                .max(computed_span);
            let (memory_index, memory, memory_skip) =
                dmabuf_plane_memory(&buffer, plane_index, buffer_offset, plane_span)?;
            let fd = dmabuf_memory_fd(memory)
                .ok_or_else(|| "DMA_DRM 视频帧 memory 不是 DMABuf".to_string())?;
            let (memory_size, base_offset, max_size) = memory.sizes();
            let memory_offset = base_offset
                .checked_add(memory_skip)
                .ok_or_else(|| "DMA_DRM 视频帧 memory offset 太大".to_string())?;
            let memory_size = max_size.max(memory_size);
            let buffer_offset = buffer_memory_prefix(&buffer, memory_index)?
                .checked_add(memory_skip)
                .ok_or_else(|| "DMA_DRM 视频帧 buffer offset 太大".to_string())?;
            let drm_format = format
                .drm_plane_fourcc(plane_index)
                .ok_or_else(|| "DMA_DRM 视频帧格式不支持 import".to_string())?;

            planes.push(RawVideoDmaBufPlane {
                fd,
                buffer_offset,
                memory_offset,
                memory_size,
                stride,
                width: layout.width,
                height: layout.height,
                drm_format,
                drm_modifier: info.modifier(),
            });
        }
    }

    trace_negotiated_dmabuf_caps(info, format, &planes, color);

    Ok(RawVideoFrame {
        format,
        color,
        range: raw_video_range(info.colorimetry().range()),
        chroma_site: raw_video_chroma_site(info.chroma_site()),
        metadata,
        planes: RawVideoPlanes::DmaBuf { buffer, planes },
    })
}

fn raw_video_format_from_dma_drm(info: &gst_video::VideoInfoDmaDrm) -> Option<RawVideoFormat> {
    let format = gst_video::dma_drm_fourcc_to_format(info.fourcc()).ok()?;
    RawVideoFormat::from_gstreamer_name(format.to_str().as_str())
}

fn dmabuf_plane_memory(
    buffer: &gst::Buffer,
    plane_index: usize,
    buffer_offset: usize,
    plane_span: usize,
) -> std::result::Result<(usize, &gst::MemoryRef, usize), String> {
    let memory_end = buffer_offset
        .checked_add(plane_span.max(1))
        .ok_or_else(|| "DMA_DRM 视频帧 plane 太大".to_string())?;
    let found_memory =
        buffer
            .find_memory(buffer_offset..memory_end)
            .and_then(|(memory_range, memory_skip)| {
                (memory_range.len() == 1).then_some((memory_range.start, memory_skip))
            });

    if buffer.n_memory()
        == buffer
            .meta::<gst_video::VideoMeta>()
            .map(|meta| meta.n_planes() as usize)
            .unwrap_or(0)
        && plane_index < buffer.n_memory()
    {
        let plane_memory = buffer.peek_memory(plane_index);
        if dmabuf_memory_fd(plane_memory).is_some() {
            let memory_skip = match found_memory {
                Some((memory_index, memory_skip)) if memory_index == plane_index => memory_skip,
                _ => buffer_offset,
            };
            return Ok((plane_index, plane_memory, memory_skip));
        }
    }

    let (memory_index, memory_skip) =
        found_memory.ok_or_else(|| "DMA_DRM 视频帧 plane 缺少独立 memory".to_string())?;
    Ok((memory_index, buffer.peek_memory(memory_index), memory_skip))
}

fn buffer_memory_prefix(
    buffer: &gst::Buffer,
    memory_index: usize,
) -> std::result::Result<usize, String> {
    let mut offset = 0usize;
    for index in 0..memory_index {
        offset = offset
            .checked_add(buffer.peek_memory(index).size())
            .ok_or_else(|| "DMA_DRM 视频帧 buffer offset 太大".to_string())?;
    }
    Ok(offset)
}

fn dmabuf_memory_fd(memory: &gst::MemoryRef) -> Option<i32> {
    if !memory.is_type("dmabuf")
        && unsafe { gst_is_dmabuf_memory(memory.as_mut_ptr()) == gst::glib::ffi::GFALSE }
    {
        return None;
    }

    let fd = unsafe { gst_dmabuf_memory_get_fd(memory.as_mut_ptr()) };
    (fd >= 0).then_some(fd)
}

fn plane_span_from_stride(
    stride: usize,
    row_len: usize,
    height: u32,
) -> std::result::Result<usize, String> {
    if row_len == 0 || height == 0 {
        return Err("DMA_DRM 视频帧尺寸无效".to_string());
    }
    let height = usize::try_from(height).map_err(|_| "DMA_DRM 视频帧太高".to_string())?;
    stride
        .checked_mul(height.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_len))
        .ok_or_else(|| "DMA_DRM 视频帧 plane 太大".to_string())
}

fn trace_negotiated_caps(info: &gst_video::VideoInfo, color: FrameColor) {
    if !matches!(
        color,
        FrameColor::Hdr10Bt2020 | FrameColor::DolbyVisionProfile5
    ) || NEGOTIATED_CAPS_LOGGED.swap(true, Ordering::Relaxed)
    {
        return;
    }

    tracing::debug!(
        format = %info.name(),
        color = ?color,
        colorimetry = %info.colorimetry(),
        range = ?info.colorimetry().range(),
        chroma_site = %info.chroma_site(),
        width = info.width(),
        height = info.height(),
        strides = ?info.stride(),
        offsets = ?info.offset(),
        "negotiated GStreamer raw video caps"
    );
}

fn trace_negotiated_dmabuf_caps(
    info: &gst_video::VideoInfoDmaDrm,
    format: RawVideoFormat,
    planes: &[RawVideoDmaBufPlane],
    color: FrameColor,
) {
    if NEGOTIATED_DMABUF_LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }

    tracing::debug!(
        format = ?format,
        color = ?color,
        colorimetry = %info.colorimetry(),
        range = ?info.colorimetry().range(),
        chroma_site = %info.chroma_site(),
        width = info.width(),
        height = info.height(),
        drm_fourcc = format!("0x{:08x}", info.fourcc()),
        drm_modifier = format!("0x{:016x}", info.modifier()),
        planes = ?planes,
        "negotiated GStreamer DMA_DRM video caps"
    );
}

fn raw_video_range(range: gst_video::VideoColorRange) -> RawVideoRange {
    match range {
        gst_video::VideoColorRange::Range0_255 => RawVideoRange::Full,
        gst_video::VideoColorRange::Range16_235 => RawVideoRange::Limited,
        _ => RawVideoRange::Unknown,
    }
}

fn raw_video_chroma_site(site: gst_video::VideoChromaSite) -> RawVideoChromaSite {
    if site.contains(gst_video::VideoChromaSite::H_COSITED)
        && site.contains(gst_video::VideoChromaSite::V_COSITED)
    {
        RawVideoChromaSite::TopLeft
    } else if site.contains(gst_video::VideoChromaSite::H_COSITED) {
        RawVideoChromaSite::Left
    } else if site.contains(gst_video::VideoChromaSite::V_COSITED) {
        RawVideoChromaSite::TopCenter
    } else if site == gst_video::VideoChromaSite::JPEG {
        RawVideoChromaSite::Center
    } else {
        RawVideoChromaSite::Unknown
    }
}

fn frame_color(info: &gst_video::VideoInfo) -> FrameColor {
    frame_color_from_metadata(&info.colorimetry())
}

fn frame_color_from_metadata(colorimetry: &gst_video::VideoColorimetry) -> FrameColor {
    let colorimetry = colorimetry.to_string().to_ascii_lowercase();
    if colorimetry == "bt2100-pq" || colorimetry.contains("2084") {
        return FrameColor::Hdr10Bt2020;
    }

    FrameColor::Sdr
}

#[cfg(test)]
fn frame_color_from_colorimetry(colorimetry: &str) -> FrameColor {
    let colorimetry = colorimetry.to_ascii_lowercase();
    if colorimetry == "bt2100-pq" || colorimetry.contains("2084") {
        FrameColor::Hdr10Bt2020
    } else {
        FrameColor::Sdr
    }
}

fn gstreamer_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::GStreamer(error.to_string())
}

pub fn is_gstreamer_matroska_large_block_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("reading large block")
        && (message.contains("matroska") || message.contains("matroskademux"))
}

fn gstreamer_message_error(error: &gst::message::Error) -> String {
    match error.debug() {
        Some(debug) => format!("{}（{debug}）", error.error()),
        None => error.error().to_string(),
    }
}

fn trace_gstreamer_error(error: &gst::message::Error) {
    tracing::error!(
        source = %gstreamer_error_source_path(error),
        error = %error.error(),
        debug = error.debug().as_deref().unwrap_or(""),
        "GStreamer bus error"
    );
}

fn trace_gstreamer_warning(warning: &gst::message::Warning) {
    tracing::warn!(
        source = %gstreamer_warning_source_path(warning),
        warning = %warning.error(),
        debug = warning.debug().as_deref().unwrap_or(""),
        "GStreamer bus warning"
    );
}

fn gstreamer_error_source_path(error: &gst::message::Error) -> String {
    error
        .src()
        .map(|source| source.path_string().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn gstreamer_warning_source_path(warning: &gst::message::Warning) -> String {
    warning
        .src()
        .map(|source| source.path_string().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn value_changed(previous: Option<f64>, next: f64) -> bool {
    previous.is_none_or(|previous| (previous - next).abs() >= 0.05)
}

fn optional_value_changed(previous: Option<f64>, next: Option<f64>) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(previous), Some(next)) => value_changed(Some(previous), next),
        _ => true,
    }
}

fn query_gstreamer_buffered_until_seconds(
    root: &gst::Element,
    position_seconds: Option<f64>,
) -> Option<f64> {
    let mut query = gst::query::Buffering::new(gst::Format::Time);
    if !root.query(query.query_mut()) {
        return None;
    }

    buffering_query_until_nsecs(&query, position_seconds.map(seconds_to_timeline_nsecs))
        .map(timeline_nsecs_to_seconds)
}

fn source_statistics_buffered_until_seconds(
    source: &gst::Element,
    position_seconds: Option<f64>,
) -> Option<f64> {
    source.find_property("statistics")?;

    let statistics = source.property::<Option<gst::Structure>>("statistics")?;
    let time_level = statistics
        .get::<u64>("maximum-time-level")
        .or_else(|_| statistics.get::<u64>("average-time-level"))
        .ok()?;
    buffered_until_from_queue_level(position_seconds, time_level)
}

fn buffering_query_until_nsecs(
    query: &gst::query::Buffering,
    position_nsecs: Option<u64>,
) -> Option<u64> {
    let mut containing_range_stop = None;
    let mut max_range_stop = None;

    for (start, stop) in query.ranges() {
        let Some((start, stop)) = buffering_range_nsecs(start, stop) else {
            continue;
        };
        max_range_stop = Some(max_optional_u64(max_range_stop, stop));

        if position_nsecs.is_none_or(|position| start <= position && position <= stop) {
            containing_range_stop = Some(max_optional_u64(containing_range_stop, stop));
        }
    }

    containing_range_stop.or(max_range_stop).or_else(|| {
        let (start, stop, _) = query.range();
        let (start, stop) = buffering_range_nsecs(start, stop)?;
        position_nsecs
            .is_none_or(|position| start <= position && position <= stop)
            .then_some(stop)
    })
}

fn buffering_range_nsecs(
    start: gst::GenericFormattedValue,
    stop: gst::GenericFormattedValue,
) -> Option<(u64, u64)> {
    match (start, stop) {
        (
            gst::GenericFormattedValue::Time(Some(start)),
            gst::GenericFormattedValue::Time(Some(stop)),
        ) => Some((start.nseconds(), stop.nseconds())),
        _ => None,
    }
}

fn buffered_until_from_queue_level(
    position_seconds: Option<f64>,
    time_level_nsecs: u64,
) -> Option<f64> {
    if time_level_nsecs == 0 {
        return None;
    }

    Some(position_seconds.unwrap_or(0.0) + timeline_nsecs_to_seconds(time_level_nsecs))
}

fn max_optional_playback_time(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn max_optional_u64(current: Option<u64>, next: u64) -> u64 {
    current.map(|current| current.max(next)).unwrap_or(next)
}

fn buffer_timeline_end_nsecs(buffer: &gst::BufferRef, fallback_duration_nsecs: u64) -> Option<u64> {
    let timestamp = buffer.pts().or_else(|| buffer.dts())?.nseconds();
    let duration = buffer
        .duration()
        .map(|duration| duration.nseconds())
        .unwrap_or(fallback_duration_nsecs);
    Some(timestamp.saturating_add(duration))
}

fn combined_playable_cache_nsecs(
    video_seen: bool,
    video_buffered_until: u64,
    needs_audio: bool,
    audio_seen: bool,
    audio_buffered_until: u64,
) -> Option<u64> {
    if !video_seen {
        return None;
    }
    if needs_audio {
        if !audio_seen {
            return None;
        }
        return Some(video_buffered_until.min(audio_buffered_until));
    }

    Some(video_buffered_until)
}

fn seconds_to_timeline_nsecs(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }

    let nsecs = seconds * 1_000_000_000.0;
    if nsecs >= u64::MAX as f64 {
        u64::MAX
    } else {
        nsecs.round() as u64
    }
}

fn timeline_nsecs_to_seconds(nsecs: u64) -> f64 {
    nsecs as f64 / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_url_error_has_user_facing_message() {
        assert_eq!(BackendError::EmptyUrl.to_string(), "播放地址为空");
    }

    #[test]
    fn matroska_large_block_error_matches_demux_debug_message() {
        let message = "Could not demultiplex stream.（../gst/matroska/matroska-demux.c: reading large block of size 47548764 not supported）";

        assert!(is_gstreamer_matroska_large_block_error(message));
    }

    #[test]
    fn matroska_large_block_error_ignores_unrelated_stream_errors() {
        let message = "Internal data stream error: streaming stopped, reason error (-5)";

        assert!(!is_gstreamer_matroska_large_block_error(message));
    }

    #[test]
    fn value_changed_tracks_initial_and_meaningful_changes() {
        assert!(value_changed(None, 1.0));
        assert!(!value_changed(Some(1.0), 1.01));
        assert!(value_changed(Some(1.0), 1.1));
    }

    #[test]
    fn optional_value_changed_tracks_presence_and_meaningful_changes() {
        assert!(!optional_value_changed(None, None));
        assert!(optional_value_changed(None, Some(1.0)));
        assert!(optional_value_changed(Some(1.0), None));
        assert!(!optional_value_changed(Some(1.0), Some(1.01)));
        assert!(optional_value_changed(Some(1.0), Some(1.1)));
    }

    #[test]
    fn playable_cache_progress_reports_video_only_until_audio_is_present() {
        let progress = PlayableCacheProgress::default();

        progress.reset_to_seconds(10.0);
        progress.observe_video_until_nsecs(12_000_000_000);

        assert_eq!(progress.buffered_until_seconds(), Some(12.0));
    }

    #[test]
    fn playable_cache_progress_uses_audio_video_minimum_when_audio_is_present() {
        let progress = PlayableCacheProgress::default();

        progress.reset_to_seconds(10.0);
        progress.observe_video_until_nsecs(16_000_000_000);
        progress.mark_audio_present();
        progress.observe_audio_until_nsecs(14_000_000_000);

        assert_eq!(progress.buffered_until_seconds(), Some(14.0));
    }

    #[test]
    fn playable_cache_progress_resets_to_seek_position() {
        let progress = PlayableCacheProgress::default();

        progress.reset_to_seconds(10.0);
        progress.mark_audio_present();
        progress.observe_video_until_nsecs(30_000_000_000);
        progress.observe_audio_until_nsecs(28_000_000_000);
        progress.reset_to_seconds(20.0);

        assert_eq!(progress.buffered_until_seconds(), Some(20.0));
    }

    #[test]
    fn combined_playable_cache_waits_for_audio_when_required() {
        assert_eq!(
            combined_playable_cache_nsecs(true, 12, true, false, 0),
            None
        );
        assert_eq!(
            combined_playable_cache_nsecs(true, 12, true, true, 10),
            Some(10)
        );
        assert_eq!(
            combined_playable_cache_nsecs(true, 12, false, false, 0),
            Some(12)
        );
    }

    #[test]
    fn buffering_query_uses_range_covering_position() {
        gst::init().unwrap();
        let mut query = gst::query::Buffering::new(gst::Format::Time);
        query.set_range(
            gst::ClockTime::from_seconds(0),
            gst::ClockTime::from_seconds(30),
            -1,
        );
        query.add_buffering_ranges([
            (
                gst::ClockTime::from_seconds(0),
                gst::ClockTime::from_seconds(10),
            ),
            (
                gst::ClockTime::from_seconds(20),
                gst::ClockTime::from_seconds(30),
            ),
        ]);

        assert_eq!(
            buffering_query_until_nsecs(&query, Some(5_000_000_000)),
            Some(10_000_000_000)
        );
        assert_eq!(
            buffering_query_until_nsecs(&query, Some(25_000_000_000)),
            Some(30_000_000_000)
        );
    }

    #[test]
    fn buffering_query_falls_back_to_max_range_without_position() {
        gst::init().unwrap();
        let mut query = gst::query::Buffering::new(gst::Format::Time);
        query.add_buffering_ranges([
            (
                gst::ClockTime::from_seconds(0),
                gst::ClockTime::from_seconds(10),
            ),
            (
                gst::ClockTime::from_seconds(20),
                gst::ClockTime::from_seconds(30),
            ),
        ]);

        assert_eq!(
            buffering_query_until_nsecs(&query, None),
            Some(30_000_000_000)
        );
    }

    #[test]
    fn queue_time_level_extends_current_position() {
        assert_eq!(
            buffered_until_from_queue_level(Some(12.0), 3_500_000_000),
            Some(15.5)
        );
        assert_eq!(buffered_until_from_queue_level(Some(12.0), 0), None);
    }

    #[test]
    fn hevc_raw_caps_prefers_dmabuf_and_keeps_cpu_fallback() {
        gst::init().unwrap();

        let caps = hevc_raw_caps();

        assert_eq!(caps.size(), 2);
        assert!(
            caps.features(0)
                .expect("dmabuf caps have features")
                .contains(DMABUF_MEMORY_FEATURE)
        );
        assert_eq!(
            caps.structure(0)
                .expect("dmabuf caps have a structure")
                .get::<&str>("format")
                .unwrap(),
            "DMA_DRM"
        );
        assert!(caps.to_string().contains("P010_10LE"));
    }

    #[test]
    fn rpu_slot_takes_exact_pts_match() {
        let mut slot = RpuSlot::new(4);
        slot.push(frame_pts(1_000), dovi_metadata(5));

        let metadata = slot
            .take_nearest(frame_pts(1_000), RPU_MATCH_TOLERANCE)
            .unwrap();

        assert_eq!(metadata.profile, 5);
        assert!(slot.entries.is_empty());
    }

    #[test]
    fn rpu_slot_takes_nearest_pts_within_tolerance() {
        let mut slot = RpuSlot::new(4);
        slot.push(frame_pts(900), dovi_metadata(5));
        slot.push(frame_pts(1_020), dovi_metadata(8));

        let metadata = slot
            .take_nearest(frame_pts(1_000), Duration::from_nanos(30))
            .unwrap();

        assert_eq!(metadata.profile, 8);
        assert_eq!(slot.entries.len(), 1);
    }

    #[test]
    fn rpu_slot_tolerates_one_frame_pts_offset() {
        let mut slot = RpuSlot::new(4);
        slot.push(frame_pts(1_041_000_000), dovi_metadata(5));

        let metadata = slot
            .take_nearest(frame_pts(1_000_000_000), RPU_MATCH_TOLERANCE)
            .unwrap();

        assert_eq!(metadata.profile, 5);
    }

    #[test]
    fn rpu_slot_rejects_pts_outside_tolerance_and_prunes_old_entries() {
        let mut slot = RpuSlot::new(4);
        slot.push(frame_pts(100), dovi_metadata(5));
        slot.push(frame_pts(200), dovi_metadata(5));

        assert!(
            slot.take_nearest(frame_pts(1_000), Duration::from_nanos(25))
                .is_none()
        );
        assert!(slot.entries.is_empty());
    }

    #[test]
    fn rpu_slot_keeps_bounded_entries_and_can_clear() {
        let mut slot = RpuSlot::new(2);
        slot.push(frame_pts(1), dovi_metadata(5));
        slot.push(frame_pts(2), dovi_metadata(5));
        slot.push(frame_pts(3), dovi_metadata(5));

        assert_eq!(slot.entries.len(), 2);
        assert_eq!(slot.entries[0].pts, frame_pts(2));
        slot.clear();
        assert!(slot.entries.is_empty());
    }

    #[test]
    fn dovi_state_reuses_last_matched_rpu_when_pts_match_is_missing() {
        let state = DoviState::default();
        state.push_rpu(frame_pts(1_000), dovi_metadata(5));
        assert_eq!(state.take_rpu(frame_pts(1_000)).unwrap().profile, 5);

        let reused = state.take_rpu(frame_pts(2_000)).unwrap();

        assert_eq!(reused.profile, 5);
    }

    #[test]
    fn dovi_state_uses_latest_profile5_rpu_without_frame_pts() {
        let state = DoviState::default();
        state.push_rpu(frame_pts(1_000), dovi_metadata(5));

        let metadata = state.take_rpu_for_frame(None).unwrap();

        assert_eq!(metadata.profile, 5);
    }

    #[test]
    fn dovi_state_uses_latest_profile5_rpu_when_pts_match_is_missing() {
        let state = DoviState::default();
        state.push_rpu(frame_pts(1_000), dovi_metadata(5));
        state
            .rpu_slot
            .lock()
            .expect("Dolby Vision RPU slot poisoned")
            .clear();

        let metadata = state.take_rpu(frame_pts(2_000)).unwrap();

        assert_eq!(metadata.profile, 5);
    }

    #[test]
    fn dovi_state_uses_fallback_without_consuming_out_of_tolerance_rpu() {
        let state = DoviState::default();
        state.push_rpu(frame_pts(1_000), dovi_metadata(5));

        let metadata = state.take_rpu(frame_pts(10_000)).unwrap();

        assert_eq!(metadata.profile, 5);
        assert_eq!(state.take_rpu(frame_pts(1_000)).unwrap().profile, 5);
    }

    #[test]
    fn frame_color_detects_hdr10_colorimetry() {
        assert_eq!(
            frame_color_from_colorimetry("bt2100-pq"),
            FrameColor::Hdr10Bt2020
        );
        assert_eq!(
            frame_color_from_colorimetry("bt2020/SMPTE2084"),
            FrameColor::Hdr10Bt2020
        );
        assert_eq!(frame_color_from_colorimetry("bt709"), FrameColor::Sdr);
    }

    #[test]
    fn frame_color_keeps_bt709_hevc_10_bit_raw_as_sdr() {
        let colorimetry = gst_video::VideoColorimetry::new(
            gst_video::VideoColorRange::Range16_235,
            gst_video::VideoColorMatrix::Bt709,
            gst_video::VideoTransferFunction::Bt709,
            gst_video::VideoColorPrimaries::Bt709,
        );

        assert_eq!(frame_color_from_metadata(&colorimetry), FrameColor::Sdr);
    }

    #[test]
    fn raw_video_format_detects_supported_gstreamer_formats() {
        assert_eq!(
            RawVideoFormat::from_gstreamer_name("P010_10LE"),
            Some(RawVideoFormat::P010Le)
        );
        assert_eq!(
            RawVideoFormat::from_gstreamer_name("I420_10LE"),
            Some(RawVideoFormat::I42010Le)
        );
        assert_eq!(
            RawVideoFormat::from_gstreamer_name("NV12"),
            Some(RawVideoFormat::Nv12)
        );
        assert_eq!(
            RawVideoFormat::from_gstreamer_name("I420"),
            Some(RawVideoFormat::I420)
        );
        assert_eq!(RawVideoFormat::from_gstreamer_name("BGRA"), None);
    }

    #[test]
    fn raw_video_range_maps_gstreamer_color_range() {
        assert_eq!(
            raw_video_range(gst_video::VideoColorRange::Range0_255),
            RawVideoRange::Full
        );
        assert_eq!(
            raw_video_range(gst_video::VideoColorRange::Range16_235),
            RawVideoRange::Limited
        );
        assert_eq!(
            raw_video_range(gst_video::VideoColorRange::Unknown),
            RawVideoRange::Unknown
        );
    }

    #[test]
    fn raw_video_chroma_site_maps_gstreamer_chroma_site() {
        assert_eq!(
            raw_video_chroma_site(gst_video::VideoChromaSite::MPEG2),
            RawVideoChromaSite::Left
        );
        assert_eq!(
            raw_video_chroma_site(gst_video::VideoChromaSite::COSITED),
            RawVideoChromaSite::TopLeft
        );
        assert_eq!(
            raw_video_chroma_site(gst_video::VideoChromaSite::JPEG),
            RawVideoChromaSite::Center
        );
        assert_eq!(
            raw_video_chroma_site(gst_video::VideoChromaSite::empty()),
            RawVideoChromaSite::Unknown
        );
    }

    fn frame_pts(nsecs: u64) -> FramePts {
        FramePts { nsecs }
    }

    fn dovi_metadata(profile: u8) -> DoviFrameMetadata {
        DoviFrameMetadata {
            profile,
            rpu_nalu: vec![0x7c, 0x01, profile],
            rpu_payload: vec![profile],
        }
    }
}
