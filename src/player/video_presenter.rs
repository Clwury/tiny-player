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
    render_host::{DecodedFrame, FramePixels, FrameSlot, RenderSize, render_image_from_bgra},
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
        if let Some(frame) = self.frame_slot.take_frame() {
            self.next_generation = self.next_generation.wrapping_add(1);
            self.latest_generation = self.next_generation;
            self.render_worker.render_latest(VideoRenderRequest {
                generation: self.latest_generation,
                frame,
                output_size: size,
            });
        }

        self.render_worker.take_ready_frame(self.latest_generation)
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
        slot.request = Some(request);
        self.state.ready.notify_one();
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
                return Some(request);
            }
            slot = self.ready.wait(slot).expect("video render worker poisoned");
        }
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
            if tone_mapper.is_none() {
                *tone_mapper = Some(LibplaceboToneMapper::new()?);
            }
            let source_size = request.frame.size;
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
