use crate::render_host::RenderHost;
use anyhow::Result;
use gpui::{RenderImage, Window};
use libmpv2::Mpv;
use std::sync::Arc;

fn replace_frame(
    current_frame: &mut Option<Arc<RenderImage>>,
    next_frame: Arc<RenderImage>,
) -> Option<Arc<RenderImage>> {
    current_frame.replace(next_frame)
}

pub struct VideoPresenter {
    render_host: RenderHost,
    current_frame: Option<Arc<RenderImage>>,
}

impl VideoPresenter {
    pub fn new(mpv: &mut Mpv) -> Result<Self> {
        Ok(Self {
            render_host: RenderHost::new(mpv)?,
            current_frame: None,
        })
    }

    pub fn current_frame(&self) -> Option<Arc<RenderImage>> {
        self.current_frame.clone()
    }

    pub fn clear_frame(&mut self, window: &mut Window) {
        if let Some(frame) = self.current_frame.take() {
            _ = window.drop_image(frame);
        }
    }

    pub fn render_if_needed(
        &mut self,
        width: u32,
        height: u32,
        window: &mut Window,
    ) -> Result<bool> {
        let Some(next_frame) = self.render_host.render_frame_if_needed(width, height)? else {
            return Ok(false);
        };

        if let Some(previous_frame) = replace_frame(&mut self.current_frame, next_frame) {
            _ = window.drop_image(previous_frame);
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::replace_frame;
    use crate::render_host::render_image_from_rgba;
    use std::sync::Arc;

    fn frame(seed: u8) -> Arc<gpui::RenderImage> {
        Arc::new(render_image_from_rgba(
            1,
            1,
            vec![seed, seed.wrapping_add(1), seed.wrapping_add(2), 255],
        ))
    }

    #[test]
    fn replace_frame_returns_the_previous_frame() {
        let first = frame(7);
        let second = frame(21);
        let mut current = Some(first.clone());

        let previous = replace_frame(&mut current, second.clone());

        assert_eq!(previous, Some(first));
        assert_eq!(current, Some(second));
    }

    #[test]
    fn replace_frame_initializes_an_empty_slot() {
        let first = frame(9);
        let mut current = None;

        let previous = replace_frame(&mut current, first.clone());

        assert_eq!(previous, None);
        assert_eq!(current, Some(first));
    }
}
