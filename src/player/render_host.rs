use std::{
    collections::VecDeque,
    ffi::c_void,
    slice,
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

const PIXEL_PACK_BUFFER_COUNT: usize = 3;
const MAX_PENDING_PIXEL_PACK_READBACKS: usize = PIXEL_PACK_BUFFER_COUNT - 1;

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

fn frame_byte_len(size: RenderSize) -> Result<usize> {
    let pixels = size
        .width
        .checked_mul(size.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("video frame buffer is too large"))?;
    usize::try_from(pixels).map_err(|_| anyhow!("video frame buffer is too large"))
}

fn render_size_to_i32(size: RenderSize) -> Result<(i32, i32)> {
    Ok((
        i32::try_from(size.width).map_err(|_| anyhow!("video frame width is too large"))?,
        i32::try_from(size.height).map_err(|_| anyhow!("video frame height is too large"))?,
    ))
}

fn render_image_from_bgra(bgra: Vec<u8>, width: u32, height: u32) -> Result<Arc<RenderImage>> {
    if bgra.len() != width as usize * height as usize * 4 {
        return Err(anyhow!("invalid video frame buffer size"));
    }

    let image = ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, bgra)
        .ok_or_else(|| anyhow!("invalid video frame buffer dimensions"))?;
    Ok(Arc::new(RenderImage::new([Frame::new(RgbaImage::from(
        image,
    ))])))
}

fn next_pixel_pack_buffer_index(index: usize) -> usize {
    (index + 1) % PIXEL_PACK_BUFFER_COUNT
}

fn can_enqueue_readback(pending_count: usize) -> bool {
    pending_count < MAX_PENDING_PIXEL_PACK_READBACKS
}

struct PendingPixelPackReadback {
    index: usize,
    fence: glow::NativeFence,
}

struct PixelPackReadback {
    buffers: [glow::NativeBuffer; PIXEL_PACK_BUFFER_COUNT],
    size: RenderSize,
    byte_len: usize,
    write_index: usize,
    pending: VecDeque<PendingPixelPackReadback>,
}

impl PixelPackReadback {
    unsafe fn new(gl: &glow::Context, size: RenderSize) -> Result<Self> {
        let byte_len = frame_byte_len(size)?;
        let byte_len_i32 =
            i32::try_from(byte_len).map_err(|_| anyhow!("video frame buffer is too large"))?;
        let buffers = [
            unsafe { gl.create_buffer().map_err(|error| anyhow!(error))? },
            unsafe { gl.create_buffer().map_err(|error| anyhow!(error))? },
            unsafe { gl.create_buffer().map_err(|error| anyhow!(error))? },
        ];

        for &buffer in &buffers {
            unsafe {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(buffer));
                gl.buffer_data_size(glow::PIXEL_PACK_BUFFER, byte_len_i32, glow::STREAM_READ);
            }
        }
        unsafe { gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None) };

        Ok(Self {
            buffers,
            size,
            byte_len,
            write_index: 0,
            pending: VecDeque::new(),
        })
    }

    fn is_saturated(&self) -> bool {
        !can_enqueue_readback(self.pending.len())
    }

    unsafe fn read_pending_frame(&mut self, gl: &glow::Context) -> Result<Option<Vec<u8>>> {
        let Some(pending) = self.pending.front() else {
            return Ok(None);
        };

        let wait_result = unsafe { gl.client_wait_sync(pending.fence, 0, 0) };
        match wait_result {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {}
            glow::TIMEOUT_EXPIRED => return Ok(None),
            glow::WAIT_FAILED => return Err(anyhow!("failed to wait for video frame readback")),
            _ => return Err(anyhow!("unexpected video frame readback state")),
        }

        let pending = self
            .pending
            .pop_front()
            .expect("pending readback checked above");
        let byte_len_i32 =
            i32::try_from(self.byte_len).map_err(|_| anyhow!("video frame buffer is too large"))?;

        unsafe {
            gl.delete_sync(pending.fence);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.buffers[pending.index]));
            let ptr =
                gl.map_buffer_range(glow::PIXEL_PACK_BUFFER, 0, byte_len_i32, glow::MAP_READ_BIT);
            if ptr.is_null() {
                gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
                return Err(anyhow!("failed to map video frame buffer"));
            }

            let pixels = slice::from_raw_parts(ptr, self.byte_len).to_vec();
            gl.unmap_buffer(glow::PIXEL_PACK_BUFFER);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            Ok(Some(pixels))
        }
    }

    unsafe fn enqueue_frame_read(&mut self, gl: &glow::Context) -> Result<()> {
        if self.is_saturated() {
            return Err(anyhow!("video frame readback queue is full"));
        }

        let (width, height) = render_size_to_i32(self.size)?;
        let index = self.write_index;

        unsafe {
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, Some(self.buffers[index]));
            gl.read_pixels(
                0,
                0,
                width,
                height,
                glow::BGRA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::BufferOffset(0),
            );
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            let fence = gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(|error| anyhow!(error))?;
            gl.flush();
            self.pending
                .push_back(PendingPixelPackReadback { index, fence });
        }

        self.write_index = next_pixel_pack_buffer_index(index);
        Ok(())
    }

    unsafe fn delete(&mut self, gl: &glow::Context) {
        unsafe { gl.finish() };
        while let Some(pending) = self.pending.pop_front() {
            unsafe { gl.delete_sync(pending.fence) };
        }
        for &buffer in &self.buffers {
            unsafe { gl.delete_buffer(buffer) };
        }
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
    readback: Option<PixelPackReadback>,
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
            readback: None,
        })
    }

    pub fn render_if_needed(&mut self, size: RenderSize) -> Result<Option<Arc<RenderImage>>> {
        let size = normalize_render_size(size);
        let size_changed = render_size_changed(self.last_size, size);
        if size_changed {
            self.sync_size(size)?;
        }

        let frame = if size_changed {
            None
        } else {
            self.read_pending_frame()?
        };
        if self
            .readback
            .as_ref()
            .is_some_and(PixelPackReadback::is_saturated)
        {
            return Ok(frame);
        }

        let pending_render = self.pending_render.swap(false, Ordering::SeqCst);
        let update = if pending_render {
            self.render_context
                .update()
                .map_err(|error| anyhow!("failed to query mpv render update: {error}"))?
        } else {
            0
        };
        if should_render_frame_if_needed(pending_render, size_changed, update) {
            self.render_frame(size)?;
        }

        Ok(frame)
    }

    fn sync_size(&mut self, size: RenderSize) -> Result<()> {
        self.window
            .set_size(size.width, size.height)
            .map_err(|error| anyhow!(error))?;
        self.reset_readback()?;
        self.last_size = Some(size);
        Ok(())
    }

    fn reset_readback(&mut self) -> Result<()> {
        self.window
            .gl_make_current(&self.gl_context)
            .map_err(|error| anyhow!(error))?;
        if let Some(mut readback) = self.readback.take() {
            unsafe { readback.delete(&self.gl) };
        }
        Ok(())
    }

    fn ensure_readback(&mut self, size: RenderSize) -> Result<()> {
        if self.readback.is_none() {
            self.readback = Some(unsafe { PixelPackReadback::new(&self.gl, size)? });
        }
        Ok(())
    }

    fn read_pending_frame(&mut self) -> Result<Option<Arc<RenderImage>>> {
        self.window
            .gl_make_current(&self.gl_context)
            .map_err(|error| anyhow!(error))?;
        let Some(readback) = self.readback.as_mut() else {
            return Ok(None);
        };
        let size = readback.size;
        let Some(pixels) = (unsafe { readback.read_pending_frame(&self.gl)? }) else {
            return Ok(None);
        };

        render_image_from_bgra(pixels, size.width, size.height).map(Some)
    }

    fn render_frame(&mut self, size: RenderSize) -> Result<()> {
        self.window
            .gl_make_current(&self.gl_context)
            .map_err(|error| anyhow!(error))?;
        self.ensure_readback(size)?;
        let (width, height) = render_size_to_i32(size)?;

        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, width, height);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::CULL_FACE);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }

        self.render_context
            .render::<sdl2::VideoSubsystem>(0, width, height, false)
            .map_err(|error| anyhow!("failed to render mpv frame: {error}"))?;

        unsafe {
            self.readback
                .as_mut()
                .expect("readback checked above")
                .enqueue_frame_read(&self.gl)?;
        }
        Ok(())
    }
}

impl Drop for RenderHost {
    fn drop(&mut self) {
        let _ = self.window.gl_make_current(&self.gl_context);
        if let Some(mut readback) = self.readback.take() {
            unsafe { readback.delete(&self.gl) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RenderSize, can_enqueue_readback, frame_byte_len, next_pixel_pack_buffer_index,
        normalize_render_size, render_image_from_bgra, render_size_changed, render_size_to_i32,
        should_render_frame_if_needed, should_render_update,
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
    fn frame_byte_len_uses_rgba_stride() {
        assert_eq!(
            frame_byte_len(RenderSize {
                width: 3840,
                height: 2160,
            })
            .unwrap(),
            3840 * 2160 * 4
        );
    }

    #[test]
    fn render_size_to_i32_preserves_source_video_size() {
        assert_eq!(
            render_size_to_i32(RenderSize {
                width: 3840,
                height: 2160,
            })
            .unwrap(),
            (3840, 2160)
        );
    }

    #[test]
    fn next_pixel_pack_buffer_index_cycles_three_buffers() {
        assert_eq!(next_pixel_pack_buffer_index(0), 1);
        assert_eq!(next_pixel_pack_buffer_index(1), 2);
        assert_eq!(next_pixel_pack_buffer_index(2), 0);
    }

    #[test]
    fn can_enqueue_readback_leaves_one_buffer_available_for_writes() {
        assert!(can_enqueue_readback(0));
        assert!(can_enqueue_readback(1));
        assert!(!can_enqueue_readback(2));
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
    fn render_image_from_bgra_outputs_gpui_bgra_bytes_without_vertical_flip() {
        let image = render_image_from_bgra(vec![3, 2, 1, 4, 7, 6, 5, 8], 2, 1).unwrap();

        assert_eq!(image.as_bytes(0), Some([3, 2, 1, 4, 7, 6, 5, 8].as_slice()));
    }

    #[test]
    fn render_image_from_bgra_rejects_wrong_buffer_size() {
        assert!(render_image_from_bgra(vec![1, 2, 3], 1, 1).is_err());
    }
}
