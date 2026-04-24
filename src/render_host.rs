use anyhow::{anyhow, Result};
use glow::{HasContext as _, PixelPackData};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};
use libmpv2::{
    render::{
        mpv_render_update, MpvRenderUpdate, OpenGLInitParams, RenderContext, RenderParam,
        RenderParamApiType,
    },
    Mpv,
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn get_proc_address(video: &sdl2::VideoSubsystem, name: &str) -> *mut c_void {
    video.gl_get_proc_address(name) as *mut c_void
}

fn normalize_render_size(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.max(1))
}

fn render_size_changed(previous: Option<(u32, u32)>, next: (u32, u32)) -> bool {
    previous != Some(next)
}

fn readback_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

fn prepare_readback_buffer(buffer: &mut Vec<u8>, width: u32, height: u32) {
    let expected_len = readback_len(width, height);
    if buffer.len() != expected_len {
        buffer.resize(expected_len, 0);
    }
}

fn should_render_update(update: MpvRenderUpdate) -> bool {
    update & mpv_render_update::Frame != 0
}

fn should_render_frame_if_needed(
    pending_render: bool,
    size_changed: bool,
    update: MpvRenderUpdate,
) -> bool {
    size_changed || (pending_render && should_render_update(update))
}

pub struct RenderHost {
    _sdl: sdl2::Sdl,
    _video: sdl2::VideoSubsystem,
    window: sdl2::video::Window,
    gl_context: sdl2::video::GLContext,
    gl: glow::Context,
    render_context: RenderContext,
    pending_render: Arc<AtomicBool>,
    readback_buffer: Vec<u8>,
    last_render_size: Option<(u32, u32)>,
}

impl RenderHost {
    pub fn new(mpv: &mut Mpv) -> Result<Self> {
        let sdl = sdl2::init().map_err(|error| anyhow!(error))?;
        let video = sdl.video().map_err(|error| anyhow!(error))?;

        let gl_attr = video.gl_attr();
        gl_attr.set_context_profile(sdl2::video::GLProfile::Core);
        gl_attr.set_context_version(3, 3);
        gl_attr.set_context_flags().forward_compatible().set();

        let window = video
            .window("tiny-render-host", 1, 1)
            .opengl()
            .hidden()
            .build()
            .map_err(|error| anyhow!(error))?;
        let gl_context = window.gl_create_context().map_err(|error| anyhow!(error))?;
        window
            .gl_make_current(&gl_context)
            .map_err(|error| anyhow!(error))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| video.gl_get_proc_address(name) as *const _)
        };

        let mut render_context = RenderContext::new(
            unsafe { mpv.ctx.as_mut() },
            [
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: video.clone(),
                }),
            ],
        )
        .map_err(|error| anyhow!("failed to create mpv render context: {error}"))?;

        let pending_render = Arc::new(AtomicBool::new(false));
        let callback_flag = pending_render.clone();
        render_context.set_update_callback(move || {
            callback_flag.store(true, Ordering::SeqCst);
        });
        pending_render.store(true, Ordering::SeqCst);

        Ok(Self {
            _sdl: sdl,
            _video: video,
            window,
            gl_context,
            gl,
            render_context,
            pending_render,
            readback_buffer: Vec::new(),
            last_render_size: None,
        })
    }

    pub fn render_frame_if_needed(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Option<Arc<RenderImage>>> {
        let (width, height) = normalize_render_size(width, height);
        let size_changed = self.sync_render_size(width, height)?;
        let pending_render = self.pending_render.swap(false, Ordering::SeqCst);

        let update = if pending_render {
            self.render_context
                .update()
                .map_err(|error| anyhow!("failed to query mpv render update: {error}"))?
        } else {
            0
        };
        if !should_render_frame_if_needed(pending_render, size_changed, update) {
            return Ok(None);
        }

        self.render_frame_at_size(width, height).map(Some)
    }

    pub fn render_frame(&mut self, width: u32, height: u32) -> Result<Arc<RenderImage>> {
        let (width, height) = normalize_render_size(width, height);
        self.sync_render_size(width, height)?;
        self.render_frame_at_size(width, height)
    }

    fn sync_render_size(&mut self, width: u32, height: u32) -> Result<bool> {
        let size_changed = render_size_changed(self.last_render_size, (width, height));
        if size_changed {
            self.window
                .set_size(width, height)
                .map_err(|error| anyhow!(error))?;
            self.last_render_size = Some((width, height));
        }

        Ok(size_changed)
    }

    fn render_frame_at_size(&mut self, width: u32, height: u32) -> Result<Arc<RenderImage>> {
        self.window
            .gl_make_current(&self.gl_context)
            .map_err(|error| anyhow!(error))?;

        prepare_readback_buffer(&mut self.readback_buffer, width, height);

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, width as i32, height as i32);
        }

        self.render_context
            .render::<sdl2::VideoSubsystem>(0, width as i32, height as i32, false)
            .map_err(|error| anyhow!("failed to render mpv frame: {error}"))?;

        unsafe {
            self.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                PixelPackData::Slice(Some(self.readback_buffer.as_mut_slice())),
            );
        }

        // GPUI's RenderImage API still requires owned bytes per frame.
        Ok(Arc::new(render_image_from_rgba(
            width,
            height,
            self.readback_buffer.clone(),
        )))
    }
}

pub fn render_image_from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> RenderImage {
    let buffer: RgbaImage = ImageBuffer::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow!("invalid RGBA buffer for {width}x{height} image"))
        .unwrap();

    RenderImage::new([Frame::new(buffer)])
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_render_size, readback_len, render_image_from_rgba, render_size_changed,
        should_render_frame_if_needed, should_render_update,
    };

    #[test]
    fn normalize_render_size_clamps_zero_dimensions() {
        assert_eq!(normalize_render_size(0, 0), (1, 1));
        assert_eq!(normalize_render_size(0, 240), (1, 240));
        assert_eq!(normalize_render_size(320, 0), (320, 1));
    }

    #[test]
    fn normalize_render_size_preserves_non_zero_dimensions() {
        assert_eq!(normalize_render_size(320, 240), (320, 240));
    }

    #[test]
    fn render_size_changed_only_flags_real_changes() {
        assert!(render_size_changed(None, (320, 240)));
        assert!(!render_size_changed(Some((320, 240)), (320, 240)));
        assert!(render_size_changed(Some((320, 240)), (640, 240)));
    }

    #[test]
    fn readback_len_tracks_rgba_byte_count() {
        assert_eq!(readback_len(1, 1), 4);
        assert_eq!(readback_len(320, 240), 320 * 240 * 4);
    }

    #[test]
    fn should_render_update_requires_frame_flag() {
        assert!(!should_render_update(0));
        assert!(should_render_update(libmpv2::render::mpv_render_update::Frame));
    }

    #[test]
    fn render_if_needed_still_renders_when_only_size_changed() {
        assert!(should_render_frame_if_needed(false, true, 0));
    }

    #[test]
    fn render_image_from_rgba_preserves_size_without_vertical_flip() {
        let image = render_image_from_rgba(
            2,
            3,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 32, 64, 96,
                255, 12, 34, 56, 255,
            ],
        );

        assert_eq!(image.size(0).width.0, 2);
        assert_eq!(image.size(0).height.0, 3);
        assert_eq!(
            image.as_bytes(0).unwrap(),
            &[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 32, 64, 96,
                255, 12, 34, 56, 255,
            ]
        );
    }
}
