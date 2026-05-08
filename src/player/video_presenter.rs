use std::sync::Arc;

use anyhow::{Context, Result};
use gpui::RenderImage;

use super::{
    libplacebo::LibplaceboToneMapper,
    render_host::{FramePixels, FrameSlot, RenderSize, render_image_from_bgra},
};

pub struct VideoPresenter {
    frame_slot: FrameSlot,
    tone_mapper: Option<LibplaceboToneMapper>,
}

impl VideoPresenter {
    pub fn new(frame_slot: FrameSlot) -> Result<Self> {
        Ok(Self {
            frame_slot,
            tone_mapper: None,
        })
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        let Some(frame) = self.frame_slot.take_frame() else {
            return Ok(None);
        };

        let frame_pts = frame.pts;

        match frame.pixels {
            FramePixels::Bgra8(pixels) => {
                render_image_from_bgra(pixels, frame.size.width, frame.size.height).map(Some)
            }
            FramePixels::RawVideo(raw) => {
                let source_size = frame.size;
                let pixels = self
                    .tone_mapper()?
                    .tone_map_to_bgra8(&raw, source_size, size)
                    .with_context(|| match frame_pts {
                        Some(pts) => format!("渲染视频帧失败（PTS {}ns）", pts.nsecs),
                        None => "渲染视频帧失败".to_string(),
                    })?;
                render_image_from_bgra(pixels, size.width, size.height).map(Some)
            }
        }
    }

    fn tone_mapper(&mut self) -> Result<&mut LibplaceboToneMapper> {
        if self.tone_mapper.is_none() {
            self.tone_mapper = Some(LibplaceboToneMapper::new()?);
        }
        Ok(self.tone_mapper.as_mut().expect("tone mapper initialized"))
    }
}
