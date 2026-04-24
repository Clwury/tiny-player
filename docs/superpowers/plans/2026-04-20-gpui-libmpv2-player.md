# GPUI + libmpv2 Player Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Linux-first desktop video player that lists videos from `sample/`, preloads the first item without autoplay, switches files from a playlist sidebar, and supports play/pause.

**Architecture:** Keep the project split into small, focused Rust modules. `media.rs` discovers the playlist, `state.rs` owns UI-facing selection and playback state, `mpv_backend.rs` wraps `libmpv2` commands and event polling, `render_host.rs` owns an offscreen OpenGL render context for `libmpv2`, and `player_app.rs` renders the GPUI window and wires user actions to the backend. The offscreen render host exists because GPUI's public API does not expose a stable external GPU interop surface; it will convert `libmpv2` frames into `gpui::RenderImage` values that the main window can display.

**Tech Stack:** Rust 2024, `gpui` 0.2.2, `libmpv2` 5.0.3, `sdl2` (bundled OpenGL helper), `glow`, `image`, `anyhow`, `tempfile`

---

## Prerequisites

- Linux with `libmpv` development files installed. On Debian or Ubuntu-like systems that is typically `sudo apt install libmpv-dev`.
- A working C/C++ toolchain so `sdl2` with the `bundled` feature can build if SDL2 is not already installed.
- Keep `sample/` in the repository root, because the first version intentionally reads only that directory.

## Planned File Structure

- `Cargo.toml`
  Adds the desktop, playback, image, and test dependencies.
- `src/lib.rs`
  Re-exports the focused modules and provides the shared `sample_dir()` helper used by the app and tests.
- `src/main.rs`
  Tiny binary entrypoint that delegates to `tiny::run()`.
- `src/media.rs`
  Playlist discovery and `PlaylistEntry`.
- `src/state.rs`
  `AppState` and pure state transitions for selection, playback flags, and user-visible errors.
- `src/mpv_backend.rs`
  Thin wrapper over `libmpv2` for initialization, file loading, pause toggling, and event polling.
- `src/render_host.rs`
  Hidden SDL2 OpenGL context that lets `libmpv2` render offscreen, reads the pixel buffer back, and converts it into `gpui::RenderImage`.
- `src/player_app.rs`
  GPUI window bootstrap, layout, click handlers, polling loop, and frame presentation.

## Task 1: Add Playlist Discovery And Project Skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/media.rs`
- Test: `src/media.rs`

- [ ] **Step 1: Write the failing playlist tests in `src/media.rs`**

```rust
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_playlist_filters_and_sorts() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("b.webm"), b"").unwrap();
        fs::write(dir.path().join("a.mp4"), b"").unwrap();
        fs::write(dir.path().join("ignore.txt"), b"").unwrap();

        let playlist = discover_playlist(dir.path()).unwrap();
        let names: Vec<_> = playlist
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect();

        assert_eq!(names, vec!["a.mp4", "b.webm"]);
    }

    #[test]
    fn discover_playlist_returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let playlist = discover_playlist(dir.path()).unwrap();

        assert!(playlist.is_empty());
    }
}
```

- [ ] **Step 2: Run the focused tests to verify they fail**

Run: `cargo test discover_playlist_filters_and_sorts -- --exact`

Expected: FAIL with errors such as `cannot find function 'discover_playlist' in this scope`.

- [ ] **Step 3: Add dependencies and implement the minimal discovery code**

`Cargo.toml`

```toml
[package]
name = "tiny"
version = "0.1.0"
edition = "2024"

[dependencies]
anyhow = "1.0"
glow = "0.16"
gpui = "0.2.2"
image = "0.25"
libmpv2 = { version = "5.0.3", features = ["render"] }
sdl2 = { version = "0.37", features = ["bundled"] }

[dev-dependencies]
tempfile = "3.13"
```

`src/lib.rs`

```rust
pub mod media;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current working directory")
        .join("sample")
}
```

`src/media.rs`

```rust
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const SUPPORTED_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    pub display_name: String,
}

pub fn discover_playlist(sample_dir: &Path) -> Result<Vec<PlaylistEntry>> {
    let mut entries = Vec::new();

    for item in fs::read_dir(sample_dir)
        .with_context(|| format!("failed to read {}", sample_dir.display()))?
    {
        let item = item?;
        let path = item.path();
        if !path.is_file() {
            continue;
        }

        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };

        if !SUPPORTED_EXTENSIONS
            .iter()
            .any(|supported| extension.eq_ignore_ascii_case(supported))
        {
            continue;
        }

        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());

        entries.push(PlaylistEntry { path, display_name });
    }

    entries.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_playlist_filters_and_sorts() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("b.webm"), b"").unwrap();
        fs::write(dir.path().join("a.mp4"), b"").unwrap();
        fs::write(dir.path().join("ignore.txt"), b"").unwrap();

        let playlist = discover_playlist(dir.path()).unwrap();
        let names: Vec<_> = playlist
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect();

        assert_eq!(names, vec!["a.mp4", "b.webm"]);
    }

    #[test]
    fn discover_playlist_returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let playlist = discover_playlist(dir.path()).unwrap();

        assert!(playlist.is_empty());
    }
}
```

- [ ] **Step 4: Run the focused tests again**

Run: `cargo test discover_playlist_filters_and_sorts -- --exact && cargo test discover_playlist_returns_empty_for_empty_dir -- --exact`

Expected: PASS for both tests.

- [ ] **Step 5: Commit the discovery slice**

```bash
git add Cargo.toml src/lib.rs src/media.rs
git commit -m "feat: add playlist discovery"
```

## Task 2: Add Pure Player State Transitions

**Files:**
- Modify: `src/lib.rs`
- Create: `src/state.rs`
- Test: `src/state.rs`

- [ ] **Step 1: Write the failing state tests in `src/state.rs`**

```rust
use crate::media::PlaylistEntry;
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> PlaylistEntry {
        PlaylistEntry {
            path: PathBuf::from(name),
            display_name: name.to_string(),
        }
    }

    #[test]
    fn from_playlist_selects_first_item_without_playing() {
        let state = AppState::from_playlist(vec![entry("a.mp4"), entry("b.mkv")]);

        assert_eq!(state.selected_index, Some(0));
        assert!(!state.is_playing);
        assert_eq!(state.current_title(), "a.mp4");
    }

    #[test]
    fn selecting_a_new_item_resets_playback() {
        let mut state = AppState::from_playlist(vec![entry("a.mp4"), entry("b.mkv")]);
        state.is_playing = true;

        state.select(1);

        assert_eq!(state.selected_index, Some(1));
        assert!(!state.is_playing);
        assert_eq!(state.current_title(), "b.mkv");
    }
}
```

- [ ] **Step 2: Run the state tests to verify they fail**

Run: `cargo test from_playlist_selects_first_item_without_playing -- --exact && cargo test selecting_a_new_item_resets_playback -- --exact`

Expected: FAIL because `AppState` does not exist yet.

- [ ] **Step 3: Implement `AppState` and export it from `src/lib.rs`**

`src/state.rs`

```rust
use crate::media::PlaylistEntry;

#[derive(Clone, Debug)]
pub struct AppState {
    pub playlist: Vec<PlaylistEntry>,
    pub selected_index: Option<usize>,
    pub is_playing: bool,
    pub error_message: Option<String>,
}

impl AppState {
    pub fn from_playlist(playlist: Vec<PlaylistEntry>) -> Self {
        let selected_index = if playlist.is_empty() { None } else { Some(0) };
        Self {
            playlist,
            selected_index,
            is_playing: false,
            error_message: None,
        }
    }

    pub fn current_entry(&self) -> Option<&PlaylistEntry> {
        self.selected_index.and_then(|index| self.playlist.get(index))
    }

    pub fn current_title(&self) -> String {
        self.current_entry()
            .map(|entry| entry.display_name.clone())
            .unwrap_or_else(|| "No video selected".to_string())
    }

    pub fn select(&mut self, index: usize) -> Option<&PlaylistEntry> {
        if self.selected_index == Some(index) || index >= self.playlist.len() {
            return None;
        }

        self.selected_index = Some(index);
        self.is_playing = false;
        self.error_message = None;
        self.current_entry()
    }

    pub fn sync_pause_state(&mut self, paused: bool) {
        self.is_playing = !paused;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error_message = Some(message.into());
        self.is_playing = false;
    }

    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    pub fn can_control_playback(&self) -> bool {
        self.selected_index.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str) -> PlaylistEntry {
        PlaylistEntry {
            path: PathBuf::from(name),
            display_name: name.to_string(),
        }
    }

    #[test]
    fn from_playlist_selects_first_item_without_playing() {
        let state = AppState::from_playlist(vec![entry("a.mp4"), entry("b.mkv")]);

        assert_eq!(state.selected_index, Some(0));
        assert!(!state.is_playing);
        assert_eq!(state.current_title(), "a.mp4");
    }

    #[test]
    fn selecting_a_new_item_resets_playback() {
        let mut state = AppState::from_playlist(vec![entry("a.mp4"), entry("b.mkv")]);
        state.is_playing = true;

        state.select(1);

        assert_eq!(state.selected_index, Some(1));
        assert!(!state.is_playing);
        assert_eq!(state.current_title(), "b.mkv");
    }
}
```

`src/lib.rs`

```rust
pub mod media;
pub mod state;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current working directory")
        .join("sample")
}
```

- [ ] **Step 4: Run the state tests again**

Run: `cargo test from_playlist_selects_first_item_without_playing -- --exact && cargo test selecting_a_new_item_resets_playback -- --exact`

Expected: PASS.

- [ ] **Step 5: Commit the pure state slice**

```bash
git add src/lib.rs src/state.rs
git commit -m "feat: add player state model"
```

## Task 3: Wrap `libmpv2` Playback Commands And Events

**Files:**
- Modify: `src/lib.rs`
- Create: `src/mpv_backend.rs`
- Test: `src/mpv_backend.rs`

- [ ] **Step 1: Write the failing backend tests in `src/mpv_backend.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{media::discover_playlist, sample_dir};

    #[test]
    fn toggle_playback_flips_pause_property() {
        let backend = MpvBackend::new().unwrap();

        assert!(backend.toggle_playback().unwrap());
        assert!(!backend.toggle_playback().unwrap());
    }

    #[test]
    fn load_file_leaves_the_player_paused() {
        let mut backend = MpvBackend::new().unwrap();
        let playlist = discover_playlist(&sample_dir()).unwrap();

        backend.load_file(&playlist[0].path).unwrap();

        assert!(backend.mpv.get_property::<bool>("pause").unwrap());
    }
}
```

- [ ] **Step 2: Run the focused backend tests to verify they fail**

Run: `cargo test toggle_playback_flips_pause_property -- --exact && cargo test load_file_leaves_the_player_paused -- --exact`

Expected: FAIL because `MpvBackend` does not exist yet.

- [ ] **Step 3: Implement the minimal backend wrapper and event mapping**

`src/mpv_backend.rs`

```rust
use anyhow::{Context, Result};
use libmpv2::{
    events::{Event, PropertyData},
    mpv_end_file_reason, Format, Mpv,
};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    PauseChanged(bool),
    FileTitleChanged(String),
    FileLoadFailed(String),
    Fatal(String),
}

pub struct MpvBackend {
    pub(crate) mpv: Mpv,
}

impl MpvBackend {
    pub fn new() -> Result<Self> {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "libmpv")?;
            init.set_property("keep-open", "yes")?;
            init.set_property("pause", true)?;
            Ok(())
        })
        .context("failed to initialize libmpv")?;

        mpv.disable_deprecated_events()?;
        mpv.observe_property("pause", Format::Flag, 0)?;
        mpv.observe_property("media-title", Format::String, 1)?;

        Ok(Self { mpv })
    }

    pub fn mpv_mut(&mut self) -> &mut Mpv {
        &mut self.mpv
    }

    pub fn load_file(&mut self, path: &Path) -> Result<()> {
        let path = path.to_string_lossy();
        self.mpv.command("loadfile", &[path.as_ref(), "replace"])?;
        self.mpv.set_property("pause", true)?;
        Ok(())
    }

    pub fn toggle_playback(&self) -> Result<bool> {
        let paused: bool = self.mpv.get_property("pause")?;
        let next_paused = !paused;
        self.mpv.set_property("pause", next_paused)?;
        Ok(!next_paused)
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();

        loop {
            match self.mpv.wait_event(0.0) {
                Some(Ok(Event::PropertyChange {
                    name: "pause",
                    change: PropertyData::Flag(paused),
                    ..
                })) => events.push(BackendEvent::PauseChanged(paused)),
                Some(Ok(Event::PropertyChange {
                    name: "media-title",
                    change: PropertyData::Str(title),
                    ..
                })) => events.push(BackendEvent::FileTitleChanged(title.to_string())),
                Some(Ok(Event::EndFile(mpv_end_file_reason::Error))) => {
                    events.push(BackendEvent::FileLoadFailed(
                        "mpv reported an end-file error".to_string(),
                    ));
                }
                Some(Err(error)) => events.push(BackendEvent::Fatal(error.to_string())),
                None => break,
                _ => {}
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{media::discover_playlist, sample_dir};

    #[test]
    fn toggle_playback_flips_pause_property() {
        let backend = MpvBackend::new().unwrap();

        assert!(backend.toggle_playback().unwrap());
        assert!(!backend.toggle_playback().unwrap());
    }

    #[test]
    fn load_file_leaves_the_player_paused() {
        let mut backend = MpvBackend::new().unwrap();
        let playlist = discover_playlist(&sample_dir()).unwrap();

        backend.load_file(&playlist[0].path).unwrap();

        assert!(backend.mpv.get_property::<bool>("pause").unwrap());
    }
}
```

`src/lib.rs`

```rust
pub mod media;
pub mod mpv_backend;
pub mod state;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current working directory")
        .join("sample")
}
```

- [ ] **Step 4: Run the focused backend tests again**

Run: `cargo test toggle_playback_flips_pause_property -- --exact && cargo test load_file_leaves_the_player_paused -- --exact`

Expected: PASS if `libmpv` is installed and the `sample/` directory still contains at least one supported video.

- [ ] **Step 5: Commit the backend control slice**

```bash
git add src/lib.rs src/mpv_backend.rs
git commit -m "feat: add libmpv playback backend"
```

## Task 4: Add The Offscreen Render Host

**Files:**
- Modify: `src/lib.rs`
- Create: `src/render_host.rs`
- Test: `src/render_host.rs`

- [ ] **Step 1: Write the failing frame-conversion test in `src/render_host.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_image_from_rgba_preserves_size() {
        let frame = render_image_from_rgba(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]);

        assert_eq!(frame.size(0).width.0, 2);
        assert_eq!(frame.size(0).height.0, 1);
    }
}
```

- [ ] **Step 2: Run the render-host test to verify it fails**

Run: `cargo test render_image_from_rgba_preserves_size -- --exact`

Expected: FAIL because `render_image_from_rgba` does not exist yet.

- [ ] **Step 3: Implement the hidden SDL2 OpenGL renderer**

`src/render_host.rs`

```rust
use anyhow::{Context, Result};
use gpui::RenderImage;
use image::{Frame, ImageBuffer, RgbaImage, imageops};
use libmpv2::{
    Mpv,
    render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType},
};
use std::{ffi::c_void, sync::Arc};

pub struct RenderHost {
    _sdl: sdl2::Sdl,
    video: sdl2::VideoSubsystem,
    window: sdl2::video::Window,
    _gl_context: sdl2::video::GLContext,
    gl: glow::Context,
    render_context: RenderContext,
}

impl RenderHost {
    pub fn new(mpv: &mut Mpv) -> Result<Self> {
        let sdl = sdl2::init().context("failed to initialize SDL2")?;
        let video = sdl.video().context("failed to initialize SDL2 video subsystem")?;

        let gl_attr = video.gl_attr();
        gl_attr.set_context_profile(sdl2::video::GLProfile::Core);
        gl_attr.set_context_version(3, 3);

        let window = video
            .window("tiny-offscreen-render", 1280, 720)
            .opengl()
            .hidden()
            .build()
            .context("failed to create hidden SDL2 window")?;

        let gl_context = window
            .gl_create_context()
            .context("failed to create hidden OpenGL context")?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| video.gl_get_proc_address(name) as *const c_void)
        };

        let render_context = RenderContext::new(
            unsafe { mpv.ctx.as_mut() },
            vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address,
                    ctx: video.clone(),
                }),
            ],
        )
        .context("failed to create mpv render context")?;

        Ok(Self {
            _sdl: sdl,
            video,
            window,
            _gl_context: gl_context,
            gl,
            render_context,
        })
    }

    pub fn render_frame(&mut self, width: u32, height: u32) -> Result<Arc<RenderImage>> {
        let width = width.max(1);
        let height = height.max(1);

        self.window
            .set_size(width, height)
            .context("failed to resize hidden SDL2 window")?;

        self.window
            .gl_make_current(&self._gl_context)
            .context("failed to activate hidden OpenGL context")?;

        let (draw_width, draw_height) = self.window.drawable_size();

        unsafe {
            self.gl.viewport(0, 0, draw_width as i32, draw_height as i32);
        }

        self.render_context
            .render::<sdl2::VideoSubsystem>(0, draw_width as i32, draw_height as i32, true)
            .context("mpv render call failed")?;

        let mut pixels = vec![0; (draw_width * draw_height * 4) as usize];
        unsafe {
            self.gl.read_pixels(
                0,
                0,
                draw_width as i32,
                draw_height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixels)),
            );
        }

        Ok(render_image_from_rgba(draw_width, draw_height, pixels))
    }
}

fn get_proc_address(video: &sdl2::VideoSubsystem, name: &str) -> *mut c_void {
    video.gl_get_proc_address(name) as *mut c_void
}

fn render_image_from_rgba(width: u32, height: u32, pixels: Vec<u8>) -> Arc<RenderImage> {
    let mut buffer: RgbaImage = ImageBuffer::from_raw(width, height, pixels)
        .expect("valid RGBA buffer from mpv frame readback");
    imageops::flip_vertical_in_place(&mut buffer);
    Arc::new(RenderImage::new(Frame::new(buffer)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_image_from_rgba_preserves_size() {
        let frame = render_image_from_rgba(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]);

        assert_eq!(frame.size(0).width.0, 2);
        assert_eq!(frame.size(0).height.0, 1);
    }
}
```

`src/lib.rs`

```rust
pub mod media;
pub mod mpv_backend;
pub mod render_host;
pub mod state;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current working directory")
        .join("sample")
}
```

- [ ] **Step 4: Run the focused render-host test**

Run: `cargo test render_image_from_rgba_preserves_size -- --exact`

Expected: PASS.

- [ ] **Step 5: Commit the render-host slice**

```bash
git add src/lib.rs src/render_host.rs
git commit -m "feat: add offscreen mpv renderer"
```

## Task 5: Build The GPUI Window And Wire User Actions

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Create: `src/player_app.rs`

- [ ] **Step 1: Replace the binary entrypoint with a call into the real app module**

`src/main.rs`

```rust
fn main() {
    tiny::run();
}
```

`src/lib.rs`

```rust
pub mod media;
pub mod mpv_backend;
pub mod player_app;
pub mod render_host;
pub mod state;

use std::path::PathBuf;

pub fn sample_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current working directory")
        .join("sample")
}

pub fn run() {
    player_app::run();
}
```

- [ ] **Step 2: Add the GPUI root view, startup preload, and polling loop**

`src/player_app.rs`

```rust
use crate::{
    media::discover_playlist,
    mpv_backend::{BackendEvent, MpvBackend},
    render_host::RenderHost,
    sample_dir,
    state::AppState,
};
use gpui::{
    App, Application, Bounds, Context, Render, Timer, Window, WindowBounds, WindowOptions, div,
    img, prelude::*, px, rgb, size,
};
use std::time::Duration;

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| PlayerApp::new(window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

pub struct PlayerApp {
    state: AppState,
    backend: Option<MpvBackend>,
    render_host: Option<RenderHost>,
    startup_message: Option<String>,
    last_frame: Option<std::sync::Arc<gpui::RenderImage>>,
}

impl PlayerApp {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut startup_message = None;

        let playlist = match discover_playlist(&sample_dir()) {
            Ok(playlist) => playlist,
            Err(error) => {
                startup_message = Some(error.to_string());
                Vec::new()
            }
        };

        let mut state = AppState::from_playlist(playlist);
        if state.playlist.is_empty() && startup_message.is_none() {
            startup_message = Some("No videos found in sample/".to_string());
        }

        let mut backend = match MpvBackend::new() {
            Ok(backend) => Some(backend),
            Err(error) => {
                state.set_error(error.to_string());
                None
            }
        };

        let mut render_host = None;
        if let Some(backend_ref) = backend.as_mut() {
            match RenderHost::new(backend_ref.mpv_mut()) {
                Ok(host) => render_host = Some(host),
                Err(error) => state.set_error(error.to_string()),
            }
        }

        let mut app = Self {
            state,
            backend,
            render_host,
            startup_message,
            last_frame: None,
        };

        app.preload_selected();
        app.start_poll_loop(window, cx);
        app
    }

    fn preload_selected(&mut self) {
        let Some(path) = self.state.current_entry().map(|entry| entry.path.clone()) else {
            return;
        };
        let Some(backend) = self.backend.as_mut() else {
            return;
        };

        if let Err(error) = backend.load_file(&path) {
            self.state.set_error(error.to_string());
        }
    }

    fn start_poll_loop(&self, window: &mut Window, cx: &mut Context<Self>) {
        let window_handle = window.window_handle();
        let entity = cx.entity();

        window
            .spawn(cx, async move |mut cx| {
                loop {
                    Timer::after(Duration::from_millis(33)).await;

                    if window_handle
                        .update(&mut cx, |_, window, cx| {
                            entity.update(cx, |this, cx| this.tick(window, cx));
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(backend) = self.backend.as_mut() {
            for event in backend.poll_events() {
                match event {
                    BackendEvent::PauseChanged(paused) => self.state.sync_pause_state(paused),
                    BackendEvent::FileTitleChanged(_) => self.state.clear_error(),
                    BackendEvent::FileLoadFailed(message) | BackendEvent::Fatal(message) => {
                        self.state.set_error(message)
                    }
                }
            }
        }

        if self.state.current_entry().is_some() {
            if let Some(render_host) = self.render_host.as_mut() {
                if let Ok(frame) = render_host.render_frame(1280, 720) {
                    self.last_frame = Some(frame);
                }
            }
        }

        window.refresh();
        cx.notify();
    }

    fn on_select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.state.select(index).cloned() else {
            return;
        };

        let Some(backend) = self.backend.as_mut() else {
            return;
        };

        if let Err(error) = backend.load_file(&entry.path) {
            self.state.set_error(error.to_string());
        }

        window.refresh();
        cx.notify();
    }

    fn on_toggle_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.as_ref() else {
            return;
        };

        match backend.toggle_playback() {
            Ok(is_playing) => self.state.is_playing = is_playing,
            Err(error) => self.state.set_error(error.to_string()),
        }

        window.refresh();
        cx.notify();
    }
}
```

- [ ] **Step 3: Render the sidebar, frame area, title, and play/pause button**

Append this `Render` implementation to `src/player_app.rs`.

```rust
impl Render for PlayerApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_toggle = self.state.can_control_playback() && self.backend.is_some();

        let mut video_area = div()
            .flex_1()
            .bg(rgb(0x000000))
            .items_center()
            .justify_center();

        if let Some(frame) = self.last_frame.clone() {
            video_area = video_area.child(img(frame).size_full());
        } else {
            video_area = video_area.child("No video loaded");
        }

        let mut message_bar = div().w_full();
        if let Some(message) = self
            .state
            .error_message
            .clone()
            .or_else(|| self.startup_message.clone())
        {
            message_bar = message_bar.child(
                div()
                    .px_3()
                    .py_2()
                    .bg(rgb(0x5c1f1f))
                    .text_color(gpui::white())
                    .child(message),
            );
        }

        div()
            .size_full()
            .flex()
            .bg(rgb(0x111111))
            .text_color(gpui::white())
            .child(
                div()
                    .w(px(320.0))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x1a1a1a))
                    .border_r_1()
                    .border_color(rgb(0x2f2f2f))
                    .children(self.state.playlist.iter().enumerate().map(|(index, entry)| {
                        let selected = self.state.selected_index == Some(index);
                        div()
                            .id(("playlist", index))
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .bg(if selected { rgb(0x2d6cdf) } else { rgb(0x1a1a1a) })
                            .hover(|style| style.bg(rgb(0x2a2a2a)))
                            .child(entry.display_name.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.on_select(index, window, cx);
                            }))
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(message_bar)
                    .child(video_area)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_4()
                            .py_3()
                            .bg(rgb(0x171717))
                            .border_t_1()
                            .border_color(rgb(0x2f2f2f))
                            .child(self.state.current_title())
                            .child(
                                div()
                                    .id("play-toggle")
                                    .px_4()
                                    .py_2()
                                    .bg(rgb(0x2d6cdf))
                                    .cursor_pointer()
                                    .when(!can_toggle, |style| {
                                        style.opacity(0.4)
                                    })
                                    .child(if self.state.is_playing { "Pause" } else { "Play" })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if this.state.can_control_playback() && this.backend.is_some() {
                                            this.on_toggle_playback(window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }
}
```

- [ ] **Step 4: Run a compile check on the full application shell**

Run: `cargo check`

Expected: `Finished` for the `dev` profile with no Rust compile errors.

- [ ] **Step 5: Commit the UI shell slice**

```bash
git add src/lib.rs src/main.rs src/player_app.rs
git commit -m "feat: add gpui player shell"
```

## Task 6: Finish Error Handling And Manual Verification

**Files:**
- Modify: `src/player_app.rs`
- Modify: `src/state.rs`

- [ ] **Step 1: Add the final missing state test for empty playlists**

Append this test to `src/state.rs`.

```rust
#[test]
fn empty_playlist_disables_playback_controls() {
    let state = AppState::from_playlist(Vec::new());

    assert_eq!(state.selected_index, None);
    assert!(!state.can_control_playback());
    assert_eq!(state.current_title(), "No video selected");
}
```

- [ ] **Step 2: Make the empty-playlist and startup-error messages explicit in the UI**

Update the `PlayerApp::new` startup branch so `sample/` read failures and empty playlists are differentiated, and keep the control button disabled whenever `backend` is `None`.

```rust
        let playlist = match discover_playlist(&sample_dir()) {
            Ok(playlist) => playlist,
            Err(error) => {
                startup_message = Some(format!("Failed to read sample/: {error}"));
                Vec::new()
            }
        };

        let mut state = AppState::from_playlist(playlist);
        if state.playlist.is_empty() && startup_message.is_none() {
            startup_message = Some("No videos found in sample/".to_string());
        }

        let mut backend = match MpvBackend::new() {
            Ok(backend) => Some(backend),
            Err(error) => {
                state.set_error(format!("mpv backend initialization failed: {error}"));
                None
            }
        };
```

- [ ] **Step 3: Run the full automated test suite**

Run: `cargo test`

Expected: `test result: ok` for the playlist, state, render-host, and mpv backend tests.

- [ ] **Step 4: Run the app and complete the manual verification list**

Run: `cargo run`

Expected manual checks:

- The window opens and shows the two videos already present in `sample/`.
- The first file is selected when the window appears.
- The play button label starts as `Play`.
- Clicking `Play` starts motion in the video pane.
- Clicking `Pause` freezes the frame.
- Clicking the other playlist item switches the title immediately and returns the button label to `Play`.
- If you temporarily rename `sample/`, the app still opens and shows a read error instead of crashing.

- [ ] **Step 5: Commit the finished minimal player**

```bash
git add src/player_app.rs src/state.rs
git commit -m "feat: finish minimal gpui video player"
```
