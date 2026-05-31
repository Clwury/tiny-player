use std::{
    collections::{HashSet, VecDeque},
    ffi::CStr,
    os::raw::c_int,
    slice,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use ffmpeg_sys_next as ffi;
use gpui::RenderImage;

use super::{
    libplacebo::LibplaceboToneMapper,
    render_host::{
        DecodedFrame, FramePixels, FramePts, PlaybackSessionId, PooledBytes, RawVideoFormat,
        RawVideoFrame, RawVideoPlane, RawVideoPlanes, RenderBackpressure, RenderSize,
        VideoOutputQueue, VideoOutputQueueSnapshot, VulkanDecodeDevice, VulkanVideoFrame,
        render_image_from_bgra,
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoPresenterSnapshot {
    pub queued: usize,
    pub queue_capacity: usize,
    pub rendering: bool,
    pub ready: bool,
    pub pending_render_requests: usize,
    pub last_render_ms: f64,
    pub average_render_ms: f64,
    pub dropped_frames: u64,
    pub blocked_on: Option<&'static str>,
}

pub struct VideoPresenter {
    vo_queue: VideoOutputQueue,
    render_worker: VideoRenderWorker,
    last_seen_session_id: PlaybackSessionId,
    next_generation: u64,
    latest_generation: u64,
}

impl VideoPresenter {
    pub fn new(vo_queue: VideoOutputQueue) -> Result<Self> {
        let last_seen_session_id = vo_queue.snapshot().active_session_id;
        Ok(Self {
            vo_queue,
            render_worker: VideoRenderWorker::spawn(),
            last_seen_session_id,
            next_generation: 0,
            latest_generation: 0,
        })
    }

    pub fn prewarm_if_needed(&mut self) {
        self.sync_vo_queue_session();
        while let Some((_session_id, device)) = self.vo_queue.take_vulkan_prewarm() {
            self.render_worker.prewarm_vulkan(device);
        }
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        self.sync_vo_queue_session();
        let mut ready_frame = self
            .render_worker
            .take_ready_frame(self.latest_generation)?;

        if self.render_worker.can_accept_render()
            && let Some(frame) = self.vo_queue.take_next_frame()
        {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.latest_generation = self.next_generation;
            let enqueue_result = self.render_worker.enqueue_render(VideoRenderRequest {
                generation: self.latest_generation,
                frame,
                output_size: size,
            });
            if enqueue_result != VideoRenderEnqueueResult::Queued {
                tracing::debug!(
                    ?enqueue_result,
                    generation = self.latest_generation,
                    "video render request queue applied backlog policy"
                );
            }
        }
        self.update_render_backpressure();

        if ready_frame.is_none() {
            ready_frame = self
                .render_worker
                .take_ready_frame(self.latest_generation)?;
        }
        self.update_render_backpressure();
        Ok(ready_frame)
    }

    pub fn discard_pending_frames(&mut self) {
        self.next_generation = self.next_generation.wrapping_add(1);
        self.latest_generation = self.next_generation;
        self.render_worker.discard_pending_requests();
        let _ = self.render_worker.take_ready_frame(self.latest_generation);
        self.update_render_backpressure();
    }

    pub fn snapshot(&self) -> VideoPresenterSnapshot {
        let vo_snapshot = self.vo_queue.snapshot();
        let render_snapshot = self.render_worker.snapshot();
        video_presenter_snapshot(vo_snapshot, render_snapshot)
    }

    fn sync_vo_queue_session(&mut self) {
        let active_session_id = self.vo_queue.snapshot().active_session_id;
        if active_session_id == self.last_seen_session_id {
            return;
        }
        self.last_seen_session_id = active_session_id;
        self.discard_pending_frames();
    }

    fn update_render_backpressure(&self) {
        self.vo_queue
            .update_render_backpressure(self.render_worker.backpressure());
    }
}

struct VideoRenderWorker {
    state: Arc<VideoRenderState>,
    results: Receiver<VideoRenderResult>,
}

struct VideoRenderState {
    slot: Mutex<VideoRenderSlot>,
    ready: Condvar,
}

#[derive(Default)]
struct VideoRenderSlot {
    requests: VecDeque<VideoRenderRequest>,
    prewarm_device: Option<Arc<VulkanDecodeDevice>>,
    rendering: bool,
    shutdown: bool,
    ready_results: usize,
    last_render_duration: Duration,
    average_render_duration: Duration,
    completed_frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoRenderEnqueueResult {
    Queued,
    WouldBlock,
}

struct VideoRenderRequest {
    generation: u64,
    frame: DecodedFrame,
    output_size: RenderSize,
}

struct VideoRenderResult {
    generation: u64,
    pts: Option<FramePts>,
    frame: std::result::Result<Arc<RenderImage>, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VideoRenderWorkerSnapshot {
    rendering: bool,
    pending_requests: usize,
    ready_results: usize,
    last_render_nsecs: u64,
    average_render_nsecs: u64,
}

enum VideoRenderWork {
    Render(VideoRenderRequest),
    Prewarm(Arc<VulkanDecodeDevice>),
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct VulkanDirectFallbackKey {
    device: usize,
    format: RawVideoFormat,
}

impl VulkanDirectFallbackKey {
    fn new(frame: &VulkanVideoFrame) -> Self {
        Self {
            device: frame.device.key(),
            format: frame.format,
        }
    }
}

impl VideoRenderWorker {
    fn spawn() -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let state = Arc::new(VideoRenderState {
            slot: Mutex::new(VideoRenderSlot::default()),
            ready: Condvar::new(),
        });
        let worker_state = state.clone();

        thread::Builder::new()
            .name("tiny-video-render".to_string())
            .spawn(move || {
                let mut tone_mapper = None;
                let mut vulkan_direct_fallback = HashSet::new();
                while let Some(work) = worker_state.take_work() {
                    match work {
                        VideoRenderWork::Render(request) => {
                            let generation = request.generation;
                            let pts = request.frame.pts;
                            let started = Instant::now();
                            let result = render_video_frame(
                                &mut tone_mapper,
                                &mut vulkan_direct_fallback,
                                request,
                            )
                            .map_err(|error| error.to_string());
                            worker_state.finish_render(started.elapsed(), pts);
                            worker_state.record_ready_result();
                            if result_tx
                                .send(VideoRenderResult {
                                    generation,
                                    pts,
                                    frame: result,
                                })
                                .is_err()
                            {
                                worker_state.consume_ready_result();
                                break;
                            }
                        }
                        VideoRenderWork::Prewarm(device) => {
                            let started = Instant::now();
                            if let Err(error) = prewarm_vulkan_tone_mapper(&mut tone_mapper, device)
                            {
                                tracing::warn!(
                                    %error,
                                    "failed to prewarm Vulkan video renderer"
                                );
                            }
                            worker_state.finish_prewarm(started.elapsed());
                        }
                    }
                }
            })
            .expect("failed to spawn video render worker");

        Self {
            state,
            results: result_rx,
        }
    }

    fn take_ready_frame(&self, latest_generation: u64) -> Result<Option<Arc<RenderImage>>> {
        let mut ready_frame = None;
        while let Ok(result) = self.results.try_recv() {
            self.state.consume_ready_result();
            if result.generation != latest_generation {
                tracing::trace!(
                    generation = result.generation,
                    latest_generation,
                    rendered_pts = ?result.pts.map(|pts| pts.nsecs),
                    "discarding stale rendered video frame"
                );
                continue;
            }
            match result.frame {
                Ok(frame) => ready_frame = Some(frame),
                Err(error) => return Err(anyhow!(error)),
            }
        }
        Ok(ready_frame)
    }

    fn enqueue_render(&self, request: VideoRenderRequest) -> VideoRenderEnqueueResult {
        self.state.enqueue_render(request)
    }

    fn can_accept_render(&self) -> bool {
        self.state.can_accept_render()
    }

    fn prewarm_vulkan(&self, device: Arc<VulkanDecodeDevice>) {
        let mut slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        if slot.shutdown || !slot.requests.is_empty() {
            return;
        }
        let should_notify = !slot.rendering && slot.prewarm_device.is_none();
        slot.prewarm_device = Some(device);
        if should_notify {
            self.state.ready.notify_one();
        }
    }

    fn discard_pending_requests(&self) {
        let mut slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        slot.requests.clear();
        slot.prewarm_device = None;
    }

    fn backpressure(&self) -> RenderBackpressure {
        let snapshot = self.snapshot();
        RenderBackpressure {
            rendering: snapshot.rendering,
            pending_requests: snapshot.pending_requests,
            last_render_nsecs: snapshot.last_render_nsecs,
            average_render_nsecs: snapshot.average_render_nsecs,
        }
    }

    fn snapshot(&self) -> VideoRenderWorkerSnapshot {
        self.state.snapshot()
    }
}

fn video_presenter_snapshot(
    vo_snapshot: VideoOutputQueueSnapshot,
    render_snapshot: VideoRenderWorkerSnapshot,
) -> VideoPresenterSnapshot {
    let vo_waiting_for_render = render_snapshot.rendering && vo_snapshot.queued_frames > 0;
    let blocked_on = if render_snapshot.pending_requests > 0 || vo_waiting_for_render {
        Some("render_worker")
    } else {
        vo_snapshot.blocked_on()
    };
    VideoPresenterSnapshot {
        queued: vo_snapshot.queued_frames,
        queue_capacity: vo_snapshot.queue_capacity,
        rendering: render_snapshot.rendering,
        ready: render_snapshot.ready_results > 0,
        pending_render_requests: render_snapshot.pending_requests,
        last_render_ms: render_snapshot.last_render_nsecs as f64 / 1_000_000.0,
        average_render_ms: render_snapshot.average_render_nsecs as f64 / 1_000_000.0,
        dropped_frames: vo_snapshot.dropped_frames,
        blocked_on,
    }
}

impl Drop for VideoRenderWorker {
    fn drop(&mut self) {
        let mut slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        slot.shutdown = true;
        slot.requests.clear();
        self.state.ready.notify_one();
    }
}

impl VideoRenderState {
    fn enqueue_render(&self, request: VideoRenderRequest) -> VideoRenderEnqueueResult {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        if slot.shutdown || slot.rendering || !slot.requests.is_empty() {
            return VideoRenderEnqueueResult::WouldBlock;
        }

        slot.requests.push_back(request);
        self.ready.notify_one();
        VideoRenderEnqueueResult::Queued
    }

    fn can_accept_render(&self) -> bool {
        let slot = self.slot.lock().expect("video render worker poisoned");
        !slot.shutdown && !slot.rendering && slot.requests.is_empty()
    }

    fn take_work(&self) -> Option<VideoRenderWork> {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        loop {
            if slot.shutdown {
                return None;
            }
            if let Some(request) = slot.requests.pop_front() {
                slot.rendering = true;
                return Some(VideoRenderWork::Render(request));
            }
            if let Some(device) = slot.prewarm_device.take() {
                slot.rendering = true;
                return Some(VideoRenderWork::Prewarm(device));
            }
            slot.rendering = false;
            slot = self.ready.wait(slot).expect("video render worker poisoned");
        }
    }

    fn finish_render(&self, render_duration: Duration, pts: Option<FramePts>) {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        slot.rendering = false;
        slot.completed_frames = slot.completed_frames.saturating_add(1);
        slot.last_render_duration = render_duration;
        slot.average_render_duration = if slot.completed_frames == 1 {
            render_duration
        } else {
            let previous = slot.average_render_duration.as_nanos();
            let next = render_duration.as_nanos();
            Duration::from_nanos(u64::try_from((previous * 7 + next) / 8).unwrap_or(u64::MAX))
        };
        if slot.completed_frames == 1 || slot.completed_frames.is_multiple_of(60) {
            tracing::debug!(
                frame_count = slot.completed_frames,
                rendered_pts = ?pts.map(|pts| pts.nsecs),
                render_ms = render_duration.as_secs_f64() * 1000.0,
                average_render_ms = slot.average_render_duration.as_secs_f64() * 1000.0,
                "rendered video frame"
            );
        }
    }

    fn finish_prewarm(&self, duration: Duration) {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        slot.rendering = false;
        tracing::debug!(
            prewarm_ms = duration.as_secs_f64() * 1000.0,
            "prewarmed Vulkan video renderer"
        );
    }

    fn record_ready_result(&self) {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        slot.ready_results = slot.ready_results.saturating_add(1);
    }

    fn consume_ready_result(&self) {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        slot.ready_results = slot.ready_results.saturating_sub(1);
    }

    fn snapshot(&self) -> VideoRenderWorkerSnapshot {
        let slot = self.slot.lock().expect("video render worker poisoned");
        VideoRenderWorkerSnapshot {
            rendering: slot.rendering,
            pending_requests: slot.requests.len(),
            ready_results: slot.ready_results,
            last_render_nsecs: duration_to_nsecs(slot.last_render_duration),
            average_render_nsecs: duration_to_nsecs(slot.average_render_duration),
        }
    }
}

fn prewarm_vulkan_tone_mapper(
    tone_mapper: &mut Option<LibplaceboToneMapper>,
    device: Arc<VulkanDecodeDevice>,
) -> Result<()> {
    if tone_mapper
        .as_ref()
        .is_none_or(|mapper| !mapper.matches_vulkan_decode_device(&device))
    {
        *tone_mapper = Some(LibplaceboToneMapper::new_for_vulkan_decode(device)?);
    }
    Ok(())
}

fn render_video_frame(
    tone_mapper: &mut Option<LibplaceboToneMapper>,
    vulkan_direct_fallback: &mut HashSet<VulkanDirectFallbackKey>,
    request: VideoRenderRequest,
) -> Result<Arc<RenderImage>> {
    let frame_pts = request.frame.pts;

    match request.frame.pixels {
        FramePixels::Bgra8(pixels) => render_image_from_bgra(
            pixels.into_vec(),
            request.frame.size.width,
            request.frame.size.height,
        ),
        FramePixels::RawVideo(raw) => {
            let source_size = request.frame.size;
            if tone_mapper.is_none() {
                *tone_mapper = Some(LibplaceboToneMapper::new()?);
            }
            let pixels = tone_mapper
                .as_mut()
                .expect("tone mapper initialized")
                .tone_map_to_bgra8(&raw, source_size, request.output_size)
                .with_context(|| match frame_pts {
                    Some(pts) => format!("渲染视频帧失败（PTS {}ns）", pts.nsecs),
                    None => "渲染视频帧失败".to_string(),
                })?;
            render_image_from_bgra(
                pixels,
                request.output_size.width,
                request.output_size.height,
            )
        }
        FramePixels::VulkanVideo(vulkan) => {
            let source_size = request.frame.size;
            render_vulkan_video_frame(
                tone_mapper,
                vulkan_direct_fallback,
                vulkan,
                source_size,
                request.output_size,
                frame_pts,
            )
        }
    }
}

fn render_vulkan_video_frame(
    tone_mapper: &mut Option<LibplaceboToneMapper>,
    vulkan_direct_fallback: &mut HashSet<VulkanDirectFallbackKey>,
    vulkan: VulkanVideoFrame,
    source_size: RenderSize,
    output_size: RenderSize,
    frame_pts: Option<FramePts>,
) -> Result<Arc<RenderImage>> {
    let fallback_key = VulkanDirectFallbackKey::new(&vulkan);
    if !vulkan_direct_fallback.contains(&fallback_key) {
        let direct_result = (|| {
            if tone_mapper
                .as_ref()
                .is_none_or(|mapper| !mapper.matches_vulkan_decode_device(&vulkan.device))
            {
                *tone_mapper = Some(LibplaceboToneMapper::new_for_vulkan_decode(
                    vulkan.device.clone(),
                )?);
            }
            tone_mapper
                .as_mut()
                .expect("tone mapper initialized")
                .tone_map_vulkan_to_bgra8(&vulkan, source_size, output_size)
        })();

        match direct_result {
            Ok(pixels) => {
                return render_image_from_bgra(pixels, output_size.width, output_size.height);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    format = ?vulkan.format,
                    device = vulkan.device.key(),
                    "Vulkan direct video render failed; using software-download fallback"
                );
                vulkan_direct_fallback.insert(fallback_key);
                *tone_mapper = None;
            }
        }
    }

    render_vulkan_download_fallback(tone_mapper, &vulkan, source_size, output_size).with_context(
        || match frame_pts {
            Some(pts) => format!("Vulkan 视频帧降级渲染失败（PTS {}ns）", pts.nsecs),
            None => "Vulkan 视频帧降级渲染失败".to_string(),
        },
    )
}

fn render_vulkan_download_fallback(
    tone_mapper: &mut Option<LibplaceboToneMapper>,
    vulkan: &VulkanVideoFrame,
    source_size: RenderSize,
    output_size: RenderSize,
) -> Result<Arc<RenderImage>> {
    let raw = download_vulkan_frame_to_raw(vulkan, source_size)?;
    if tone_mapper.is_none() {
        *tone_mapper = Some(LibplaceboToneMapper::new()?);
    }
    let pixels = tone_mapper
        .as_mut()
        .expect("tone mapper initialized")
        .tone_map_to_bgra8(&raw, source_size, output_size)?;
    render_image_from_bgra(pixels, output_size.width, output_size.height)
}

fn download_vulkan_frame_to_raw(
    vulkan: &VulkanVideoFrame,
    size: RenderSize,
) -> Result<RawVideoFrame> {
    let mut software_frame = SoftwareFrame::new()?;
    let result = unsafe {
        ffi::av_hwframe_transfer_data(software_frame.as_mut_ptr(), vulkan.frame.as_ptr(), 0)
    };
    if result < 0 {
        return Err(anyhow!(
            "FFmpeg 下载 Vulkan 视频帧失败：{}",
            ffmpeg_error(result)
        ));
    }
    unsafe {
        let _ = ffi::av_frame_copy_props(software_frame.as_mut_ptr(), vulkan.frame.as_ptr());
    }

    Ok(RawVideoFrame {
        format: vulkan.format,
        color: vulkan.color,
        range: vulkan.range,
        chroma_site: vulkan.chroma_site,
        metadata: vulkan.metadata.clone(),
        planes: RawVideoPlanes::Owned(copy_raw_video_planes(
            software_frame.as_ptr(),
            vulkan.format,
            size,
        )?),
    })
}

fn copy_raw_video_planes(
    frame: *const ffi::AVFrame,
    format: RawVideoFormat,
    size: RenderSize,
) -> Result<Vec<RawVideoPlane>> {
    let mut planes = Vec::with_capacity(format.plane_count());
    for plane_index in 0..format.plane_count() {
        let layout = format.plane_layout(size, plane_index)?;
        let data = unsafe { (*frame).data[plane_index] };
        if data.is_null() {
            return Err(anyhow!("FFmpeg raw 视频帧缺少平面数据"));
        }
        let stride = unsafe { (*frame).linesize[plane_index] };
        if stride <= 0 {
            return Err(anyhow!("FFmpeg raw 视频帧 stride 无效"));
        }
        let stride =
            usize::try_from(stride).map_err(|_| anyhow!("FFmpeg raw 视频帧 stride 无效"))?;
        if stride < layout.row_len {
            return Err(anyhow!("FFmpeg raw 视频帧 stride 小于行宽"));
        }
        let height = usize::try_from(layout.height).map_err(|_| anyhow!("视频帧过高"))?;
        let len = layout
            .row_len
            .checked_mul(height)
            .ok_or_else(|| anyhow!("视频帧平面过大"))?;
        let mut bytes = Vec::with_capacity(len);
        for row in 0..height {
            let row_start = row
                .checked_mul(stride)
                .ok_or_else(|| anyhow!("视频帧平面过大"))?;
            let row_data = unsafe { slice::from_raw_parts(data.add(row_start), layout.row_len) };
            bytes.extend_from_slice(row_data);
        }
        planes.push(RawVideoPlane {
            data: PooledBytes::from_vec(bytes),
            stride: layout.row_len,
        });
    }
    Ok(planes)
}

struct SoftwareFrame {
    ptr: *mut ffi::AVFrame,
}

impl SoftwareFrame {
    fn new() -> Result<Self> {
        let ptr = unsafe { ffi::av_frame_alloc() };
        if ptr.is_null() {
            return Err(anyhow!("FFmpeg 分配 Vulkan 降级 frame 失败"));
        }
        Ok(Self { ptr })
    }

    fn as_ptr(&self) -> *const ffi::AVFrame {
        self.ptr
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.ptr
    }
}

impl Drop for SoftwareFrame {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_frame_free(&mut self.ptr) };
        }
    }
}

fn ffmpeg_error(result: c_int) -> String {
    let mut buffer = [0; 128];
    let status = unsafe { ffi::av_strerror(result, buffer.as_mut_ptr(), buffer.len()) };
    if status < 0 {
        return format!("FFmpeg error {result}");
    }
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn duration_to_nsecs(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_render_request(generation: u64) -> VideoRenderRequest {
        VideoRenderRequest {
            generation,
            frame: DecodedFrame {
                size: RenderSize {
                    width: 2,
                    height: 1,
                },
                pts: Some(FramePts { nsecs: generation }),
                key_frame: generation == 1,
                pixels: FramePixels::Bgra8(vec![0; 8].into()),
            },
            output_size: RenderSize {
                width: 2,
                height: 1,
            },
        }
    }

    fn render_state(rendering: bool) -> VideoRenderState {
        let state = VideoRenderState {
            slot: Mutex::new(VideoRenderSlot::default()),
            ready: Condvar::new(),
        };
        state.slot.lock().expect("test slot").rendering = rendering;
        state
    }

    fn take_render_generation(state: &VideoRenderState) -> u64 {
        match state.take_work() {
            Some(VideoRenderWork::Render(request)) => request.generation,
            Some(VideoRenderWork::Prewarm(_)) => panic!("unexpected prewarm work"),
            None => panic!("expected render work"),
        }
    }

    fn presenter_with_manual_render_worker(vo_queue: VideoOutputQueue) -> VideoPresenter {
        let (_result_tx, result_rx) = mpsc::channel();
        let last_seen_session_id = vo_queue.snapshot().active_session_id;
        VideoPresenter {
            vo_queue,
            render_worker: VideoRenderWorker {
                state: Arc::new(VideoRenderState {
                    slot: Mutex::new(VideoRenderSlot::default()),
                    ready: Condvar::new(),
                }),
                results: result_rx,
            },
            last_seen_session_id,
            next_generation: 0,
            latest_generation: 0,
        }
    }

    #[test]
    fn video_render_queue_returns_would_block_when_rendering() {
        let state = render_state(true);

        assert_eq!(
            state.enqueue_render(test_render_request(1)),
            VideoRenderEnqueueResult::WouldBlock
        );
        assert_eq!(state.snapshot().pending_requests, 0);
    }

    #[test]
    fn video_render_queue_returns_would_block_when_request_pending() {
        let state = render_state(false);

        assert_eq!(
            state.enqueue_render(test_render_request(1)),
            VideoRenderEnqueueResult::Queued
        );
        assert_eq!(
            state.enqueue_render(test_render_request(2)),
            VideoRenderEnqueueResult::WouldBlock
        );
        assert_eq!(state.snapshot().pending_requests, 1);
        assert_eq!(take_render_generation(&state), 1);
    }

    #[test]
    fn video_presenter_leaves_vo_queue_intact_while_render_worker_is_busy() {
        let vo_queue = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        vo_queue.begin_session(session_id);
        assert!(vo_queue.push(session_id, test_render_request(1).frame));
        assert!(vo_queue.push(session_id, test_render_request(2).frame));
        let mut presenter = presenter_with_manual_render_worker(vo_queue.clone());
        presenter
            .render_worker
            .state
            .slot
            .lock()
            .expect("test slot")
            .rendering = true;

        let rendered = presenter
            .render_if_needed(RenderSize {
                width: 2,
                height: 1,
            })
            .expect("render_if_needed succeeds");

        assert!(rendered.is_none());
        assert_eq!(vo_queue.snapshot().queued_frames, 2);
        assert_eq!(presenter.render_worker.snapshot().pending_requests, 0);
    }

    #[test]
    fn video_presenter_discards_render_backlog_after_vo_queue_session_change() {
        let vo_queue = VideoOutputQueue::default();
        vo_queue.begin_session(PlaybackSessionId(1));
        let mut presenter = presenter_with_manual_render_worker(vo_queue.clone());

        assert_eq!(
            presenter
                .render_worker
                .enqueue_render(test_render_request(1)),
            VideoRenderEnqueueResult::Queued
        );
        assert_eq!(presenter.render_worker.snapshot().pending_requests, 1);

        vo_queue.begin_session(PlaybackSessionId(2));
        presenter.sync_vo_queue_session();

        assert_eq!(presenter.last_seen_session_id, PlaybackSessionId(2));
        assert_eq!(presenter.latest_generation, 1);
        assert_eq!(presenter.render_worker.snapshot().pending_requests, 0);
    }

    #[test]
    fn video_presenter_snapshot_reports_render_worker_block() {
        let snapshot = video_presenter_snapshot(
            VideoOutputQueueSnapshot {
                active_session_id: PlaybackSessionId(1),
                queued_frames: 1,
                queue_capacity: 3,
                dropped_frames: 0,
                render_backpressure: RenderBackpressure {
                    rendering: true,
                    pending_requests: 1,
                    last_render_nsecs: 55_000_000,
                    average_render_nsecs: 40_000_000,
                },
            },
            VideoRenderWorkerSnapshot {
                rendering: true,
                pending_requests: 1,
                ready_results: 0,
                last_render_nsecs: 55_000_000,
                average_render_nsecs: 40_000_000,
            },
        );

        assert_eq!(snapshot.queued, 1);
        assert!(snapshot.rendering);
        assert!(!snapshot.ready);
        assert_eq!(snapshot.last_render_ms, 55.0);
        assert_eq!(snapshot.blocked_on, Some("render_worker"));
    }

    #[test]
    fn video_presenter_snapshot_reports_vo_queue_block() {
        let snapshot = video_presenter_snapshot(
            VideoOutputQueueSnapshot {
                active_session_id: PlaybackSessionId(1),
                queued_frames: 3,
                queue_capacity: 3,
                dropped_frames: 2,
                render_backpressure: RenderBackpressure::default(),
            },
            VideoRenderWorkerSnapshot {
                ready_results: 1,
                ..VideoRenderWorkerSnapshot::default()
            },
        );

        assert_eq!(snapshot.queued, 3);
        assert!(snapshot.ready);
        assert_eq!(snapshot.dropped_frames, 2);
        assert_eq!(snapshot.blocked_on, Some("vo_queue"));
    }
}
