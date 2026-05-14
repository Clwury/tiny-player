use std::{
    collections::HashSet,
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
        DecodedFrame, FramePixels, FramePts, FrameSlot, PooledBytes, RawVideoFormat, RawVideoFrame,
        RawVideoPlane, RawVideoPlanes, RenderBackpressure, RenderSize, VulkanVideoFrame,
        render_image_from_bgra,
    },
};

pub struct VideoPresenter {
    frame_slot: FrameSlot,
    render_worker: VideoRenderWorker,
    next_generation: u64,
    latest_generation: u64,
}

impl VideoPresenter {
    pub fn new(frame_slot: FrameSlot) -> Result<Self> {
        Ok(Self {
            frame_slot,
            render_worker: VideoRenderWorker::spawn(),
            next_generation: 0,
            latest_generation: 0,
        })
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        let mut ready_frame = self
            .render_worker
            .take_ready_frame(self.latest_generation)?;

        if let Some(frame) = self.frame_slot.take_frame() {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.latest_generation = self.next_generation;
            self.render_worker.render_latest(VideoRenderRequest {
                generation: self.latest_generation,
                frame,
                output_size: size,
            });
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
        self.render_worker.discard_pending_request();
        let _ = self.render_worker.take_ready_frame(self.latest_generation);
        self.update_render_backpressure();
    }

    fn update_render_backpressure(&self) {
        self.frame_slot
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
    request: Option<VideoRenderRequest>,
    rendering: bool,
    shutdown: bool,
    last_render_duration: Duration,
    average_render_duration: Duration,
    completed_frames: u64,
}

struct VideoRenderRequest {
    generation: u64,
    frame: DecodedFrame,
    output_size: RenderSize,
}

struct VideoRenderResult {
    generation: u64,
    frame: std::result::Result<Arc<RenderImage>, String>,
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
                while let Some(request) = worker_state.take_request() {
                    let generation = request.generation;
                    let started = Instant::now();
                    let result =
                        render_video_frame(&mut tone_mapper, &mut vulkan_direct_fallback, request)
                            .map_err(|error| error.to_string());
                    worker_state.finish_request(started.elapsed());
                    if result_tx
                        .send(VideoRenderResult {
                            generation,
                            frame: result,
                        })
                        .is_err()
                    {
                        break;
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
            if result.generation != latest_generation {
                continue;
            }
            match result.frame {
                Ok(frame) => ready_frame = Some(frame),
                Err(error) => return Err(anyhow!(error)),
            }
        }
        Ok(ready_frame)
    }

    fn render_latest(&self, request: VideoRenderRequest) {
        let mut slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        if slot.shutdown {
            return;
        }
        let should_notify = !slot.rendering && slot.request.is_none();
        slot.request = Some(request);
        if should_notify {
            self.state.ready.notify_one();
        }
    }

    fn discard_pending_request(&self) {
        let mut slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        slot.request = None;
    }

    fn backpressure(&self) -> RenderBackpressure {
        let slot = self
            .state
            .slot
            .lock()
            .expect("video render worker poisoned");
        RenderBackpressure {
            rendering: slot.rendering,
            pending_requests: usize::from(slot.request.is_some()),
            last_render_nsecs: duration_to_nsecs(slot.last_render_duration),
            average_render_nsecs: duration_to_nsecs(slot.average_render_duration),
        }
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
        slot.request = None;
        self.state.ready.notify_one();
    }
}

impl VideoRenderState {
    fn take_request(&self) -> Option<VideoRenderRequest> {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        loop {
            if slot.shutdown {
                return None;
            }
            if let Some(request) = slot.request.take() {
                slot.rendering = true;
                return Some(request);
            }
            slot.rendering = false;
            slot = self.ready.wait(slot).expect("video render worker poisoned");
        }
    }

    fn finish_request(&self, render_duration: Duration) {
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
                render_ms = render_duration.as_secs_f64() * 1000.0,
                average_render_ms = slot.average_render_duration.as_secs_f64() * 1000.0,
                "rendered video frame"
            );
        }
    }
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
