use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow};
use glow::HasContext as _;
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};
use libmpv2::{
    Mpv,
    render::{
        MpvRenderUpdate, OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType,
        mpv_render_update,
    },
};

fn get_proc_address(video: &sdl2::VideoSubsystem, name: &str) -> *mut c_void {
    video.gl_get_proc_address(name) as *mut c_void
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

fn normalize_render_size(mut size: RenderSize) -> RenderSize {
    size.width = size.width.max(1);
    size.height = size.height.max(1);
    size
}

fn render_size_changed(previous: Option<RenderSize>, next: RenderSize) -> bool {
    previous != Some(next)
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

fn render_image_from_rgba(mut rgba: Vec<u8>, width: u32, height: u32) -> Result<Arc<RenderImage>> {
    if rgba.len() != width as usize * height as usize * 4 {
        return Err(anyhow!("invalid video frame buffer size"));
    }

    rgba_to_bgra(&mut rgba);
    let image = ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .ok_or_else(|| anyhow!("invalid video frame buffer dimensions"))?;
    Ok(Arc::new(RenderImage::new([Frame::new(RgbaImage::from(
        image,
    ))])))
}

fn rgba_to_bgra(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

pub struct RenderHost {
    _sdl: sdl2::Sdl,
    _video: sdl2::VideoSubsystem,
    window: sdl2::video::Window,
    gl_context: sdl2::video::GLContext,
    gl: glow::Context,
    render_context: RenderContext,
    pending_render: Arc<AtomicBool>,
    last_size: Option<RenderSize>,
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
            .window("tiny-video-renderer", 1, 1)
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
            last_size: None,
        })
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        let size = normalize_render_size(size);
        let size_changed = render_size_changed(self.last_size, size);
        if size_changed {
            self.sync_size(size)?;
        }

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

        Ok(Some(self.render_frame(size)?))
    }

    fn sync_size(&mut self, size: RenderSize) -> Result<()> {
        self.window
            .set_size(size.width, size.height)
            .map_err(|error| anyhow!(error))?;
        self.last_size = Some(size);
        Ok(())
    }

    fn render_frame(&mut self, size: RenderSize) -> Result<Arc<RenderImage>> {
        self.window
            .gl_make_current(&self.gl_context)
            .map_err(|error| anyhow!(error))?;

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl
                .viewport(0, 0, size.width as i32, size.height as i32);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        self.render_context
            .render::<sdl2::VideoSubsystem>(0, size.width as i32, size.height as i32, false)
            .map_err(|error| anyhow!("failed to render mpv frame: {error}"))?;

        let mut pixels = vec![0; size.width as usize * size.height as usize * 4];
        unsafe {
            self.gl.read_pixels(
                0,
                0,
                size.width as i32,
                size.height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }

        render_image_from_rgba(pixels, size.width, size.height)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RenderSize, normalize_render_size, render_image_from_rgba, render_size_changed,
        rgba_to_bgra, should_render_frame_if_needed, should_render_update,
    };

    #[test]
    fn normalize_render_size_clamps_zero_dimensions() {
        assert_eq!(
            normalize_render_size(RenderSize {
                width: 0,
                height: 0,
            }),
            RenderSize {
                width: 1,
                height: 1,
            }
        );
    }

    #[test]
    fn render_size_changed_only_flags_real_changes() {
        let first = RenderSize {
            width: 320,
            height: 240,
        };
        let second = RenderSize {
            width: 640,
            height: 240,
        };

        assert!(render_size_changed(None, first));
        assert!(!render_size_changed(Some(first), first));
        assert!(render_size_changed(Some(first), second));
    }

    #[test]
    fn should_render_update_requires_frame_flag() {
        assert!(!should_render_update(0));
        assert!(should_render_update(
            libmpv2::render::mpv_render_update::Frame
        ));
    }

    #[test]
    fn should_render_frame_if_needed_renders_on_size_change_or_frame_update() {
        assert!(should_render_frame_if_needed(false, true, 0));
        assert!(should_render_frame_if_needed(
            true,
            false,
            libmpv2::render::mpv_render_update::Frame
        ));
        assert!(!should_render_frame_if_needed(true, false, 0));
        assert!(!should_render_frame_if_needed(false, false, 0));
    }

    #[test]
    fn rgba_to_bgra_swaps_red_and_blue_channels() {
        let mut pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];

        rgba_to_bgra(&mut pixels);

        assert_eq!(pixels, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn render_image_from_rgba_outputs_gpui_bgra_bytes_without_vertical_flip() {
        let image = render_image_from_rgba(vec![1, 2, 3, 4, 5, 6, 7, 8], 2, 1).unwrap();

        assert_eq!(image.as_bytes(0), Some([3, 2, 1, 4, 7, 6, 5, 8].as_slice()));
    }

    #[test]
    fn render_image_from_rgba_rejects_wrong_buffer_size() {
        assert!(render_image_from_rgba(vec![1, 2, 3], 1, 1).is_err());
    }
}
