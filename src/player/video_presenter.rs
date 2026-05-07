use std::sync::Arc;

use anyhow::Result;
use gpui::RenderImage;
use libmpv2::Mpv;

use super::render_host::{RenderHost, RenderSize};

pub struct VideoPresenter {
    render_host: RenderHost,
}

impl VideoPresenter {
    pub fn new(mpv: &mut Mpv) -> Result<Self> {
        Ok(Self {
            render_host: RenderHost::new(mpv)?,
        })
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        self.render_host.render_if_needed(size)
    }
}
