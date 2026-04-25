use anyhow::{Result, anyhow};
use glow::{HasContext as _, NativeBuffer, PixelPackData};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage};
use libmpv2::{
    Mpv,
    render::{
        MpvRenderUpdate, OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType,
        mpv_render_update,
    },
};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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

const READBACK_BUFFER_COUNT: usize = 2;

fn readback_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

fn readback_len_fits_gl_size(width: u32, height: u32) -> bool {
    u128::from(width) * u128::from(height) * 4 <= i32::MAX as u128
}

fn readback_len_i32(width: u32, height: u32) -> Result<i32> {
    if readback_len_fits_gl_size(width, height) {
        Ok(readback_len(width, height) as i32)
    } else {
        Err(anyhow!(
            "readback buffer too large for OpenGL buffer size: {width}x{height}"
        ))
    }
}

fn prepare_readback_buffer(buffer: &mut Vec<u8>, width: u32, height: u32) {
    let expected_len = readback_len(width, height);
    if buffer.len() != expected_len {
        buffer.resize(expected_len, 0);
    }
}

fn pbo_next_index(index: usize) -> usize {
    (index + 1) % READBACK_BUFFER_COUNT
}

fn pbo_previous_index(write_index: usize) -> usize {
    (write_index + READBACK_BUFFER_COUNT - 1) % READBACK_BUFFER_COUNT
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

struct PboReadback {
    buffers: [NativeBuffer; READBACK_BUFFER_COUNT],
    byte_len: usize,
    write_index: usize,
    has_previous_frame: bool,
}

impl PboReadback {
    fn new(buffers: [NativeBuffer; READBACK_BUFFER_COUNT], byte_len: usize) -> Self {
        Self {
            buffers,
            byte_len,
            write_index: 0,
            has_previous_frame: false,
        }
    }

    fn write_buffer(&self) -> NativeBuffer {
        self.buffers[self.write_index]
    }

    fn previous_buffer(&self) -> NativeBuffer {
        self.buffers[pbo_previous_index(self.write_index)]
    }

    fn read_buffer(&self) -> NativeBuffer {
        if self.has_previous_frame {
            self.previous_buffer()
        } else {
            self.write_buffer()
        }
    }

    fn mark_submitted(&mut self) {
        self.write_index = pbo_next_index(self.write_index);
        self.has_previous_frame = true;
    }

    fn reset(&mut self, byte_len: usize) {
        self.byte_len = byte_len;
        self.write_index = 0;
        self.has_previous_frame = false;
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
    readback_buffer: Vec<u8>,
    pbo_readback: Option<PboReadback>,
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
            pbo_readback: None,
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

    pub fn reset_readback_pipeline(&mut self) {
        if let Some(readback) = self.pbo_readback.as_mut() {
            readback.reset(readback.byte_len);
        }
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

        self.ensure_pbo_readback(width, height)?;

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, width as i32, height as i32);
        }

        self.render_context
            .render::<sdl2::VideoSubsystem>(0, width as i32, height as i32, false)
            .map_err(|error| anyhow!("failed to render mpv frame: {error}"))?;

        self.read_frame_pixels_with_pbo(width, height)?;

        // GPUI's RenderImage API still requires owned bytes per frame.
        Ok(Arc::new(render_image_from_rgba(
            width,
            height,
            self.readback_buffer.clone(),
        )))
    }

    fn ensure_pbo_readback(&mut self, width: u32, height: u32) -> Result<()> {
        let byte_len_i32 = readback_len_i32(width, height)?;
        let byte_len = byte_len_i32 as usize;
        prepare_readback_buffer(&mut self.readback_buffer, width, height);

        if self.pbo_readback.is_none() {
            self.pbo_readback = Some(self.create_pbo_readback(byte_len, byte_len_i32)?);
            return Ok(());
        }

        let needs_reallocate = self
            .pbo_readback
            .as_ref()
            .is_some_and(|readback| readback.byte_len != byte_len);
        if needs_reallocate {
            let buffers = self
                .pbo_readback
                .as_ref()
                .expect("PBO readback checked above")
                .buffers;
            self.allocate_pbo_buffers(buffers, byte_len_i32)?;
            self.pbo_readback
                .as_mut()
                .expect("PBO readback checked above")
                .reset(byte_len);
        }

        Ok(())
    }

    fn create_pbo_readback(&self, byte_len: usize, byte_len_i32: i32) -> Result<PboReadback> {
        unsafe {
            let first = self
                .gl
                .create_buffer()
                .map_err(|error| anyhow!("failed to create pixel pack buffer: {error}"))?;
            let second = match self.gl.create_buffer() {
                Ok(buffer) => buffer,
                Err(error) => {
                    self.gl.delete_buffer(first);
                    return Err(anyhow!("failed to create pixel pack buffer: {error}"));
                }
            };
            let buffers = [first, second];

            if let Err(error) = self.allocate_pbo_buffers(buffers, byte_len_i32) {
                for buffer in buffers {
                    self.gl.delete_buffer(buffer);
                }
                return Err(error);
            }

            Ok(PboReadback::new(buffers, byte_len))
        }
    }

    fn allocate_pbo_buffers(
        &self,
        buffers: [NativeBuffer; READBACK_BUFFER_COUNT],
        byte_len: i32,
    ) -> Result<()> {
        unsafe {
            for buffer in buffers {
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buffer));
                self.gl
                    .buffer_data_size(glow::PIXEL_PACK_BUFFER, byte_len, glow::STREAM_READ);
            }
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }

        self.check_gl_error("failed to allocate pixel pack buffers")
    }

    fn read_frame_pixels_with_pbo(&mut self, width: u32, height: u32) -> Result<()> {
        let (write_buffer, read_buffer) = {
            let readback = self
                .pbo_readback
                .as_ref()
                .expect("PBO readback must be initialized before reading pixels");
            (readback.write_buffer(), readback.read_buffer())
        };

        unsafe {
            self.gl
                .bind_buffer(glow::PIXEL_PACK_BUFFER, Some(write_buffer));
            self.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                PixelPackData::BufferOffset(0),
            );
        }
        self.check_gl_error("failed to submit PBO readback")?;

        unsafe {
            self.gl
                .bind_buffer(glow::PIXEL_PACK_BUFFER, Some(read_buffer));
            self.gl.get_buffer_sub_data(
                glow::PIXEL_PACK_BUFFER,
                0,
                self.readback_buffer.as_mut_slice(),
            );
            self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
        }
        self.check_gl_error("failed to read PBO data")?;

        self.pbo_readback
            .as_mut()
            .expect("PBO readback must be initialized before reading pixels")
            .mark_submitted();
        Ok(())
    }

    fn check_gl_error(&self, operation: &str) -> Result<()> {
        let error = unsafe { self.gl.get_error() };
        if error == glow::NO_ERROR {
            Ok(())
        } else {
            Err(anyhow!("{operation}: GL error 0x{error:04x}"))
        }
    }
}

impl Drop for RenderHost {
    fn drop(&mut self) {
        if self.window.gl_make_current(&self.gl_context).is_err() {
            return;
        }

        if let Some(readback) = self.pbo_readback.take() {
            unsafe {
                self.gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                for buffer in readback.buffers {
                    self.gl.delete_buffer(buffer);
                }
            }
        }
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
        normalize_render_size, pbo_next_index, pbo_previous_index, readback_len,
        readback_len_fits_gl_size, render_image_from_rgba, render_size_changed,
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
    fn readback_len_fits_gl_size_accepts_normal_frames() {
        assert!(readback_len_fits_gl_size(3840, 2160));
    }

    #[test]
    fn readback_len_fits_gl_size_rejects_oversized_frames() {
        assert!(!readback_len_fits_gl_size(u32::MAX, u32::MAX));
    }

    #[test]
    fn pbo_next_index_wraps_double_buffer_ring() {
        assert_eq!(pbo_next_index(0), 1);
        assert_eq!(pbo_next_index(1), 0);
    }

    #[test]
    fn pbo_previous_index_wraps_double_buffer_ring() {
        assert_eq!(pbo_previous_index(0), 1);
        assert_eq!(pbo_previous_index(1), 0);
    }

    #[test]
    fn should_render_update_requires_frame_flag() {
        assert!(!should_render_update(0));
        assert!(should_render_update(
            libmpv2::render::mpv_render_update::Frame
        ));
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
