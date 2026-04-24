# GPUI Player Wayland Video Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce wasted frame work in the current Wayland-compatible embedded video path by making readback rendering event-driven, releasing stale GPUI images explicitly, and introducing a small presentation boundary that keeps `PlayerApp` ready for future native backends.

**Architecture:** Keep the existing SDL2/glow + `libmpv` render API backend, but change it from unconditional timer-driven readback to mpv-update-driven presentation. Add a narrow `video_presenter` module that owns the current frame and render host, so `PlayerApp` only tracks UI state and measured video viewport bounds. The readback host will reuse one staging buffer for `glReadPixels`, and each changed frame will still become an owned `RenderImage` until a future native backend removes that final copy.

**Tech Stack:** Rust 2024, GPUI 0.2.2, libmpv2 5.0.3 render API, SDL2/glow, image, cargo test/check, manual verification in a Wayland session

---

## Planned File Structure

- `src/render_host.rs`
  Add mpv render-update callback wiring, conditional `render_frame_if_needed`, render-size change detection, and a reusable readback staging buffer.
- `src/video_presenter.rs`
  New small presentation boundary that owns `RenderHost`, the current `RenderImage`, and the stale-frame drop logic.
- `src/player_app.rs`
  Swap direct `RenderHost` ownership for `VideoPresenter`, track the measured video viewport bounds, render only when viewport + mpv updates say work is needed, and stop notifying GPUI every timer tick.
- `src/lib.rs`
  Export the new `video_presenter` module.

## Task 1: Make `RenderHost` Update-Driven

**Files:**
- Modify: `src/render_host.rs`

- [ ] **Step 1: Add failing `RenderHost` helper tests**

Replace the `use super::...` line inside `src/render_host.rs`'s existing `#[cfg(test)] mod tests` block, then append these tests.

```rust
use super::{
    normalize_render_size, readback_len, render_image_from_rgba, render_size_changed,
    should_render_update,
};

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
```

- [ ] **Step 2: Run the focused render-host test target and verify it fails**

Run: `cargo test render_host::tests --lib`

Expected: FAIL with missing items such as `readback_len`, `render_size_changed`, `should_render_update`, or missing `render_frame_if_needed` support in `RenderHost`.

- [ ] **Step 3: Replace `src/render_host.rs` with the event-driven version**

```rust
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

        if !self.pending_render.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }

        let update = self
            .render_context
            .update()
            .map_err(|error| anyhow!("failed to query mpv render update: {error}"))?;
        if !should_render_update(update) {
            return Ok(None);
        }

        if render_size_changed(self.last_render_size, (width, height)) {
            self.window
                .set_size(width, height)
                .map_err(|error| anyhow!(error))?;
            self.last_render_size = Some((width, height));
        }

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
        Ok(Some(Arc::new(render_image_from_rgba(
            width,
            height,
            self.readback_buffer.clone(),
        ))))
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
        should_render_update,
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
```

- [ ] **Step 4: Run the render-host tests again**

Run: `cargo test render_host::tests --lib`

Expected: PASS with the helper tests and the existing image-conversion tests green.

- [ ] **Step 5: Commit the render-host slice**

```bash
git add src/render_host.rs
git commit -m "refactor: make mpv readback rendering update-driven"
```

## Task 2: Add A Small `VideoPresenter` Boundary

**Files:**
- Create: `src/video_presenter.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Create a failing presenter test module and export it**

Create `src/video_presenter.rs` with only this content first.

```rust
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
```

Update `src/lib.rs` to export the new module.

```rust
pub mod media;
pub mod mpv_backend;
pub mod player_app;
pub mod render_host;
pub mod state;
pub mod video_presenter;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sample")
}

pub fn run() {
    player_app::run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct CurrentDirGuard(PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }

    #[test]
    fn sample_dir_uses_manifest_dir_instead_of_current_dir() {
        let _lock = CWD_LOCK.lock().unwrap();
        let _original_dir = CurrentDirGuard(std::env::current_dir().unwrap());
        let temp_dir = tempdir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        let sample_dir = sample_dir();

        assert_eq!(
            sample_dir,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sample")
        );
    }
}
```

- [ ] **Step 2: Run the presenter test target and verify it fails**

Run: `cargo test video_presenter --lib`

Expected: FAIL with missing `replace_frame` and missing `VideoPresenter` implementation.

- [ ] **Step 3: Implement the `VideoPresenter` boundary**

Replace `src/video_presenter.rs` with this file.

```rust
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
```

- [ ] **Step 4: Run the presenter tests again**

Run: `cargo test video_presenter --lib`

Expected: PASS with the frame-replacement unit tests green.

- [ ] **Step 5: Commit the presenter boundary**

```bash
git add src/lib.rs src/video_presenter.rs
git commit -m "refactor: add video presentation boundary"
```

## Task 3: Rewire `PlayerApp` Around The Presenter And Measured Viewport

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add failing player-app helper tests for viewport-aware rendering**

In `src/player_app.rs`, replace the `use super::{ ... }` list inside the existing `#[cfg(test)] mod tests` block with this import list, then append the new viewport tests below the render-related helper tests.

```rust
use super::{
    adjacent_index, bottom_control_labels, can_toggle_playback, handle_backend_failure,
    handle_file_title_update, handle_render_failure, normalize_video_viewport,
    overlay_progress_bottom_inset_px, overlay_progress_fill_fraction,
    overlay_progress_horizontal_inset_px, reset_progress, selection_changed,
    should_render_frame, viewport_changed, Direction,
};
use crate::state::AppState;
use gpui::{Bounds, point, px, size};

#[test]
fn normalize_video_viewport_rejects_zero_sized_bounds() {
    let zero_width = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));
    let zero_height = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(0.0)));

    assert_eq!(normalize_video_viewport(zero_width), None);
    assert_eq!(normalize_video_viewport(zero_height), None);
}

#[test]
fn normalize_video_viewport_floors_fractional_pixel_sizes() {
    let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));

    assert_eq!(normalize_video_viewport(bounds), Some((640, 359)));
}

#[test]
fn viewport_changed_only_reports_real_differences() {
    let first = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
    let second = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)));

    assert!(viewport_changed(None, first));
    assert!(!viewport_changed(Some(first), first));
    assert!(viewport_changed(Some(first), second));
}

#[test]
fn should_render_frame_requires_a_valid_viewport() {
    assert!(should_render_frame(true, true, false, true));
    assert!(!should_render_frame(true, true, false, false));
    assert!(!should_render_frame(true, true, true, true));
    assert!(!should_render_frame(false, true, false, true));
    assert!(!should_render_frame(true, false, false, true));
}
```

- [ ] **Step 2: Run the player-app helper tests and verify they fail**

Run: `cargo test player_app::tests --lib`

Expected: FAIL with missing `normalize_video_viewport`, missing `viewport_changed`, and a `should_render_frame` signature mismatch.

- [ ] **Step 3: Update `src/player_app.rs` to use `VideoPresenter` and a measured viewport**

Apply these exact replacements.

Replace the import block and helper section at the top of `src/player_app.rs` with:

```rust
use crate::{
    media::discover_playlist,
    mpv_backend::{BackendEvent, MpvBackend},
    sample_dir,
    state::AppState,
    video_presenter::VideoPresenter,
};
use gpui::{
    canvas, App, Application, Bounds, Context, Pixels, Render, StatefulInteractiveElement, Timer,
    Window, WindowBounds, WindowOptions, div, img, prelude::*, px, rgb, size,
};
use std::time::Duration;

fn selection_changed(before: Option<usize>, after: Option<usize>) -> bool {
    before != after
}

fn reset_progress(state: &mut AppState) {
    state.update_progress(0.0, 0.0);
}

fn handle_backend_failure(state: &mut AppState, message: impl Into<String>) {
    reset_progress(state);
    state.set_error(message);
}

fn handle_render_failure(
    state: &mut AppState,
    status_message: &mut String,
    message: impl Into<String>,
) {
    reset_progress(state);
    *status_message = message.into();
}

fn handle_file_title_update(
    current_title: &mut String,
    state: &mut AppState,
    status_message: &mut String,
    title: String,
) {
    *current_title = title;
    state.clear_error();
    status_message.clear();
}

fn normalize_video_viewport(bounds: Bounds<Pixels>) -> Option<(u32, u32)> {
    let width = f32::from(bounds.size.width).floor().max(0.0) as u32;
    let height = f32::from(bounds.size.height).floor().max(0.0) as u32;

    (width > 0 && height > 0).then_some((width, height))
}

fn viewport_changed(previous: Option<Bounds<Pixels>>, next: Bounds<Pixels>) -> bool {
    previous != Some(next)
}

fn should_render_frame(
    has_video_presenter: bool,
    has_current_entry: bool,
    has_error: bool,
    has_viewport: bool,
) -> bool {
    has_video_presenter && has_current_entry && !has_error && has_viewport
}

fn can_toggle_playback(has_backend: bool, can_control_playback: bool, has_error: bool) -> bool {
    has_backend && can_control_playback && !has_error
}

fn overlay_progress_fill_fraction(progress_fraction: f32) -> f32 {
    progress_fraction.clamp(0.0, 1.0)
}

fn overlay_progress_bottom_inset_px() -> f32 {
    12.0
}

fn overlay_progress_horizontal_inset_px() -> f32 {
    14.0
}

fn bottom_control_labels() -> [&'static str; 3] {
    ["Previous", "Play/Pause", "Next"]
}

#[derive(Clone, Copy)]
enum Direction {
    Previous,
    Next,
}

fn adjacent_index(
    selected_index: Option<usize>,
    playlist_len: usize,
    direction: Direction,
) -> Option<usize> {
    let index = selected_index?;

    match direction {
        Direction::Previous => index.checked_sub(1),
        Direction::Next => index
            .checked_add(1)
            .filter(|&next_index| next_index < playlist_len),
    }
}
```

Replace the `PlayerApp` struct plus its `impl PlayerApp` block with:

```rust
struct PlayerApp {
    state: AppState,
    backend: Option<MpvBackend>,
    video_presenter: Option<VideoPresenter>,
    video_viewport_bounds: Option<Bounds<Pixels>>,
    current_title: String,
    status_message: String,
    polling_started: bool,
}

impl PlayerApp {
    fn new() -> Self {
        let (playlist, mut status_message) = match discover_playlist(&sample_dir()) {
            Ok(playlist) if playlist.is_empty() => {
                (playlist, "No videos found in sample/".to_string())
            }
            Ok(playlist) => (playlist, String::new()),
            Err(error) => (Vec::new(), format!("Failed to read sample/: {error}")),
        };
        let mut state = AppState::from_playlist(playlist);
        let current_title = state.current_title().to_owned();

        let (backend, video_presenter) = match MpvBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(backend.mpv_mut()) {
                Ok(video_presenter) => {
                    if let Some((path, display_name)) = state
                        .current_entry()
                        .map(|entry| (entry.path.clone(), entry.display_name.clone()))
                    {
                        if let Err(error) = backend.load_file(&path) {
                            state.set_error(error.to_string());
                            status_message = format!("Failed to preload {display_name}");
                        }
                    }
                    (Some(backend), Some(video_presenter))
                }
                Err(error) => {
                    state.set_error(error.to_string());
                    status_message = "Failed to create video presenter".to_string();
                    (Some(backend), None)
                }
            },
            Err(error) => {
                state.set_error(error.to_string());
                status_message = "Failed to create mpv backend".to_string();
                (None, None)
            }
        };

        Self {
            state,
            backend,
            video_presenter,
            video_viewport_bounds: None,
            current_title,
            status_message,
            polling_started: false,
        }
    }

    fn ensure_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polling_started {
            return;
        }

        self.polling_started = true;
        let view = cx.entity().downgrade();
        window.spawn(cx, async move |cx| {
            loop {
                Timer::after(Duration::from_millis(33)).await;
                if view
                    .update_in(&mut *cx, |this, window, cx| this.poll_backend(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn clear_visible_frame(&mut self, window: &mut Window) {
        if let Some(video_presenter) = self.video_presenter.as_mut() {
            video_presenter.clear_frame(window);
        }
    }

    fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.video_viewport_bounds.clone(), bounds.clone()) {
            return;
        }

        self.video_viewport_bounds = Some(bounds);
        cx.notify();
    }

    fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut should_notify = false;

        if let Some(backend) = self.backend.as_mut() {
            for event in backend.poll_events() {
                match event {
                    BackendEvent::Pause(paused) => {
                        self.state.sync_pause_state(paused);
                        should_notify = true;
                    }
                    BackendEvent::FileTitle(title) => {
                        handle_file_title_update(
                            &mut self.current_title,
                            &mut self.state,
                            &mut self.status_message,
                            title,
                        );
                        should_notify = true;
                    }
                    BackendEvent::PositionChanged(position) => {
                        self.state.update_position(position);
                        should_notify = true;
                    }
                    BackendEvent::DurationChanged(duration) => {
                        self.state.update_duration(duration);
                        should_notify = true;
                    }
                    BackendEvent::LoadFailed(message) => {
                        self.clear_visible_frame(window);
                        handle_backend_failure(&mut self.state, message);
                        should_notify = true;
                    }
                    BackendEvent::Fatal(message) => {
                        self.clear_visible_frame(window);
                        handle_backend_failure(&mut self.state, message);
                        should_notify = true;
                    }
                }
            }
        }

        let viewport = self
            .video_viewport_bounds
            .clone()
            .and_then(normalize_video_viewport);

        if should_render_frame(
            self.video_presenter.is_some(),
            self.state.current_entry().is_some(),
            self.state.error_message.is_some(),
            viewport.is_some(),
        ) {
            let (width, height) = viewport.expect("viewport checked above");
            let render_result = self
                .video_presenter
                .as_mut()
                .expect("video presenter checked above")
                .render_if_needed(width, height, window);

            match render_result {
                Ok(true) => {
                    self.status_message.clear();
                    should_notify = true;
                }
                Ok(false) => {}
                Err(error) => {
                    self.clear_visible_frame(window);
                    handle_render_failure(&mut self.state, &mut self.status_message, error.to_string());
                    should_notify = true;
                }
            }
        }

        if should_notify {
            cx.notify();
        }
    }

    fn select_playlist_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let previous_selection = self.state.selected_index;
        self.state.select(index);
        if !selection_changed(previous_selection, self.state.selected_index) {
            return;
        }

        self.clear_visible_frame(window);
        reset_progress(&mut self.state);
        self.current_title = self.state.current_title().to_owned();

        let Some(entry) = self.state.current_entry() else {
            return;
        };

        if let Some(backend) = self.backend.as_mut() {
            if let Err(error) = backend.load_file(&entry.path) {
                self.clear_visible_frame(window);
                handle_backend_failure(&mut self.state, error.to_string());
            }
        }

        cx.notify();
    }

    fn select_adjacent_item(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = adjacent_index(self.state.selected_index, self.state.playlist.len(), direction) else {
            return;
        };

        self.select_playlist_item(index, window, cx);
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if !can_toggle_playback(
            self.backend.is_some(),
            self.state.can_control_playback(),
            self.state.error_message.is_some(),
        ) {
            return;
        }

        let Some(backend) = self.backend.as_mut() else {
            return;
        };

        match backend.toggle_playback() {
            Ok(is_playing) => {
                self.state.is_playing = is_playing;
                self.state.clear_error();
            }
            Err(error) => self.state.set_error(error.to_string()),
        }

        cx.notify();
    }

    fn message_text(&self) -> &str {
        self.state
            .error_message
            .as_deref()
            .unwrap_or(self.status_message.as_str())
    }
}
```

Replace the entire `impl Render for PlayerApp` block with:

```rust
impl Render for PlayerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_polling(window, cx);

        let playlist = self.state.playlist.iter().enumerate().fold(
            div().flex().flex_col().gap_1(),
            |playlist, (index, entry)| {
                let selected = self.state.selected_index == Some(index);
                playlist.child(
                    div()
                        .id(("playlist-item", index))
                        .px_3()
                        .py_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(if selected { rgb(0x2f3a4a) } else { rgb(0x20242b) })
                        .text_color(rgb(0xf5f7fa))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_playlist_item(index, window, cx);
                        }))
                        .child(entry.display_name.clone()),
                )
            },
        );

        let play_label = if self.state.is_playing { "Pause" } else { "Play" };
        let can_go_previous = self.state.has_previous();
        let can_go_next = self.state.has_next();
        let can_toggle_playback = can_toggle_playback(
            self.backend.is_some(),
            self.state.can_control_playback(),
            self.state.error_message.is_some(),
        );
        let [previous_label, _, next_label] = bottom_control_labels();
        let progress_fill_fraction = overlay_progress_fill_fraction(self.state.progress_fraction());
        let message_text = self.message_text().to_string();
        let current_frame = self
            .video_presenter
            .as_ref()
            .and_then(|video_presenter| video_presenter.current_frame());
        let view = cx.entity().downgrade();
        let viewport_observer = canvas(
            |bounds, _, _| bounds,
            move |_bounds, observed_bounds, window, app| {
                let view = view.clone();
                let observed_bounds = observed_bounds.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_video_viewport(observed_bounds, cx);
                    });
                });
            },
        )
        .absolute()
        .size_full();

        let video_area = if let Some(frame) = current_frame {
            div().flex().flex_1().bg(rgb(0x050505)).child(img(frame).size_full())
        } else {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .bg(rgb(0x050505))
                .text_color(rgb(0x9aa5b1))
                .child("Video output will appear here")
        };

        let mut playback_toggle = div()
            .id("playback-toggle")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(if can_toggle_playback {
                rgb(0x238636)
            } else {
                rgb(0x3a434d)
            })
            .child(play_label);

        playback_toggle = if can_toggle_playback {
            playback_toggle
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_playback(cx);
                }))
        } else {
            playback_toggle.cursor_default()
        };

        let button_bg = |enabled| if enabled { rgb(0x30363d) } else { rgb(0x1f242d) };
        let button_text = |enabled| if enabled { rgb(0xe6edf3) } else { rgb(0x6e7681) };

        let mut previous_button = div()
            .id("previous-button")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(button_bg(can_go_previous))
            .text_color(button_text(can_go_previous))
            .child(previous_label);

        previous_button = if can_go_previous {
            previous_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_adjacent_item(Direction::Previous, window, cx);
                }))
        } else {
            previous_button.cursor_default()
        };

        let mut next_button = div()
            .id("next-button")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(button_bg(can_go_next))
            .text_color(button_text(can_go_next))
            .child(next_label);

        next_button = if can_go_next {
            next_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_adjacent_item(Direction::Next, window, cx);
                }))
        } else {
            next_button.cursor_default()
        };

        let overlay_progress_bar = div()
            .absolute()
            .left(px(overlay_progress_horizontal_inset_px()))
            .right(px(overlay_progress_horizontal_inset_px()))
            .bottom(px(overlay_progress_bottom_inset_px()))
            .h(px(8.0))
            .rounded_sm()
            .bg(gpui::rgba(0x00000073))
            .child(
                div()
                    .h_full()
                    .w(gpui::relative(progress_fill_fraction))
                    .rounded_sm()
                    .bg(rgb(0x2f81f7)),
            );

        div()
            .flex()
            .size_full()
            .bg(rgb(0x111418))
            .text_color(rgb(0xe6edf3))
            .child(
                div()
                    .w(px(280.0))
                    .h_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(rgb(0x161b22))
                    .child(div().text_sm().child("Playlist"))
                    .child(div().flex_1().child(playlist)),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_col()
                    .overflow_hidden()
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .child(div().text_xl().child(self.current_title.clone())),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .overflow_hidden()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x30363d))
                            .p_2()
                            .child(video_area)
                            .child(viewport_observer)
                            .child(overlay_progress_bar),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .bg(rgb(0x1f242d))
                            .text_color(rgb(0x9fb3c8))
                            .child(message_text),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .bg(rgb(0x161b22))
                            .child(previous_button)
                            .child(playback_toggle)
                            .child(next_button),
                    ),
            )
    }
}
```

Replace the entire `#[cfg(test)] mod tests` block at the bottom of `src/player_app.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::{
        adjacent_index, bottom_control_labels, can_toggle_playback, handle_backend_failure,
        handle_file_title_update, handle_render_failure, normalize_video_viewport,
        overlay_progress_bottom_inset_px, overlay_progress_fill_fraction,
        overlay_progress_horizontal_inset_px, reset_progress, selection_changed,
        should_render_frame, viewport_changed, Direction,
    };
    use crate::state::AppState;
    use gpui::{Bounds, point, px, size};

    #[test]
    fn selection_changed_is_false_for_same_selection() {
        assert!(!selection_changed(Some(2), Some(2)));
    }

    #[test]
    fn selection_changed_is_true_for_new_selection() {
        assert!(selection_changed(Some(1), Some(2)));
    }

    #[test]
    fn reset_progress_clears_stale_playback_values() {
        let mut state = AppState::from_playlist(Vec::new());
        state.update_progress(12.5, 98.0);

        reset_progress(&mut state);

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
    }

    #[test]
    fn handle_backend_failure_sets_error_and_resets_progress() {
        let mut state = AppState::from_playlist(Vec::new());
        state.update_progress(12.5, 98.0);

        handle_backend_failure(&mut state, "fatal backend error");

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
        assert_eq!(state.error_message.as_deref(), Some("fatal backend error"));
    }

    #[test]
    fn handle_render_failure_sets_status_and_resets_progress() {
        let mut state = AppState::from_playlist(Vec::new());
        let mut status_message = String::new();
        state.update_progress(12.5, 98.0);

        handle_render_failure(&mut state, &mut status_message, "transient render failure");

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
        assert_eq!(status_message, "transient render failure");
        assert_eq!(state.error_message, None);
    }

    #[test]
    fn normalize_video_viewport_rejects_zero_sized_bounds() {
        let zero_width = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));
        let zero_height = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(0.0)));

        assert_eq!(normalize_video_viewport(zero_width), None);
        assert_eq!(normalize_video_viewport(zero_height), None);
    }

    #[test]
    fn normalize_video_viewport_floors_fractional_pixel_sizes() {
        let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));

        assert_eq!(normalize_video_viewport(bounds), Some((640, 359)));
    }

    #[test]
    fn viewport_changed_only_reports_real_differences() {
        let first = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let second = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)));

        assert!(viewport_changed(None, first));
        assert!(!viewport_changed(Some(first), first));
        assert!(viewport_changed(Some(first), second));
    }

    #[test]
    fn should_render_frame_requires_a_valid_viewport() {
        assert!(should_render_frame(true, true, false, true));
        assert!(!should_render_frame(true, true, false, false));
        assert!(!should_render_frame(true, true, true, true));
        assert!(!should_render_frame(false, true, false, true));
        assert!(!should_render_frame(true, false, false, true));
    }

    #[test]
    fn can_toggle_playback_disables_controls_while_error_is_present() {
        assert!(can_toggle_playback(true, true, false));
        assert!(!can_toggle_playback(true, true, true));
        assert!(!can_toggle_playback(false, true, false));
        assert!(!can_toggle_playback(true, false, false));
    }

    #[test]
    fn overlay_progress_fill_fraction_stays_in_unit_interval() {
        assert_eq!(overlay_progress_fill_fraction(0.25), 0.25);
        assert_eq!(overlay_progress_fill_fraction(2.0), 1.0);
        assert_eq!(overlay_progress_fill_fraction(-1.0), 0.0);
    }

    #[test]
    fn overlay_progress_bottom_inset_matches_overlay_contract() {
        assert_eq!(overlay_progress_bottom_inset_px(), 12.0);
    }

    #[test]
    fn overlay_progress_horizontal_inset_matches_overlay_contract() {
        assert_eq!(overlay_progress_horizontal_inset_px(), 14.0);
    }

    #[test]
    fn bottom_control_strip_keeps_only_transport_buttons() {
        let labels = bottom_control_labels();

        assert_eq!(labels, ["Previous", "Play/Pause", "Next"]);
    }

    #[test]
    fn handle_file_title_update_clears_stale_status_message_on_recovery() {
        let mut state = AppState::from_playlist(Vec::new());
        let mut current_title = String::from("Old title");
        let mut status_message = String::from("Failed to preload clip.mp4");
        state.set_error("backend failed");

        handle_file_title_update(
            &mut current_title,
            &mut state,
            &mut status_message,
            "Recovered title".to_string(),
        );

        assert_eq!(current_title, "Recovered title");
        assert_eq!(state.error_message, None);
        assert!(status_message.is_empty());
    }

    #[test]
    fn adjacent_index_stays_in_bounds_at_playlist_edges() {
        assert_eq!(adjacent_index(Some(0), 3, Direction::Previous), None);
        assert_eq!(adjacent_index(Some(2), 3, Direction::Next), None);
    }

    #[test]
    fn adjacent_index_moves_to_previous_and_next_items() {
        assert_eq!(adjacent_index(Some(1), 3, Direction::Previous), Some(0));
        assert_eq!(adjacent_index(Some(1), 3, Direction::Next), Some(2));
    }
}
```

- [ ] **Step 4: Run the player-app helper tests again**

Run: `cargo test player_app::tests --lib`

Expected: PASS with the viewport helpers, control helpers, and transport helpers green.

- [ ] **Step 5: Commit the Wayland player integration**

```bash
git add src/player_app.rs
git commit -m "refactor: reduce wayland video frame churn"
```

## Task 4: Verify The Whole Project And Manual Wayland Behavior

**Files:**
- Test: `src/render_host.rs`
- Test: `src/video_presenter.rs`
- Test: `src/player_app.rs`
- Test: `tests/state.rs`

- [ ] **Step 1: Run the library test suite**

Run: `cargo test --lib`

Expected: PASS with the updated render-host, presenter, player-app, and existing library tests green.

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`

Expected: PASS with integration tests and mpv-backed tests still green.

- [ ] **Step 3: Run a compile-only verification pass**

Run: `cargo check`

Expected: PASS with no compile errors.

- [ ] **Step 4: Launch the player in the current Wayland session and verify the manual checklist**

Run: `cargo run`

Expected:
- the window opens normally in the current Wayland desktop session
- the first video still appears paused after startup
- play/pause, previous, and next still work
- paused playback no longer visibly churns frames or UI updates when nothing changes
- switching files clears the old visible frame instead of leaving stale content behind
- leaving the app open for a while does not show unbounded image accumulation behavior
