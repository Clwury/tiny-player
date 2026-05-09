use std::{
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, Receiver},
    },
    thread,
};

use anyhow::{Context, Result, anyhow};
use gpui::RenderImage;

use super::{
    libplacebo::LibplaceboToneMapper,
    render_host::{
        DecodedFrame, FramePixels, FrameSlot, RenderSize, render_image_from_bgra,
        sdr_8bit_yuv_to_bgra,
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

        if ready_frame.is_none() {
            ready_frame = self
                .render_worker
                .take_ready_frame(self.latest_generation)?;
        }
        Ok(ready_frame)
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
                while let Some(request) = worker_state.take_request() {
                    let generation = request.generation;
                    let result = render_video_frame(&mut tone_mapper, request)
                        .map_err(|error| error.to_string());
                    worker_state.finish_request();
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

    fn finish_request(&self) {
        let mut slot = self.slot.lock().expect("video render worker poisoned");
        slot.rendering = false;
    }
}

fn render_video_frame(
    tone_mapper: &mut Option<LibplaceboToneMapper>,
    request: VideoRenderRequest,
) -> Result<Arc<RenderImage>> {
    let frame_pts = request.frame.pts;

    match request.frame.pixels {
        FramePixels::Bgra8(pixels) => {
            render_image_from_bgra(pixels, request.frame.size.width, request.frame.size.height)
        }
        FramePixels::RawVideo(raw) => {
            let source_size = request.frame.size;
            if request.output_size == source_size
                && let Some(pixels) = sdr_8bit_yuv_to_bgra(&raw, source_size)?
            {
                return render_image_from_bgra(pixels, source_size.width, source_size.height);
            }

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
    }
}
