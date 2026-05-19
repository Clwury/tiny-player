use std::{fmt, sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, CursorStyle, DragMoveEvent, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, RenderImage, SharedString,
    StatefulInteractiveElement, Styled, Timer, Window, canvas, deferred, div, prelude::*, px,
    relative, rgb, rgba, svg,
};

use crate::theme;

use super::{
    backend::{
        BackendCommand, BackendControl, BackendEventKind, BackendLoadRequest,
        BackendSubtitleBitmap, BackendSubtitleCue, FfmpegBackend, HttpStreamBufferProgress,
        PlaybackVideoInfo,
    },
    render_host::RenderSize,
    tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection},
    video_presenter::VideoPresenter,
};

mod progress;
mod render;
mod runtime;
mod video_element;

use progress::{
    ProgressBarDrag, buffered_progress_fraction, clamp_playback_position, format_playback_time,
    http_stream_buffered_range_fractions, is_seek_position_buffered, playback_status_message,
    progress_fraction, progress_fraction_for_cursor, should_apply_backend_position,
    valid_http_stream_buffer_progress, valid_playback_duration, valid_playback_time,
};
#[cfg(test)]
use progress::{combined_buffered_until, http_stream_buffered_until};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, defer_drop_frame, normalize_video_viewport,
    render_output_size, should_render_frame, should_request_animation_frame, viewport_changed,
};
use runtime::{PlaybackBackend, ShutdownOrder};
use video_element::VideoFrameElement;

const KEYBOARD_SEEK_STEP_SECONDS: f64 = 5.0;
const SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION: f32 = 0.01;
const PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX: f32 = 24.0;
const PLAYBACK_PROGRESS_BAR_HEIGHT_PX: f32 = 94.0;
const FULLSCREEN_CONTROLS_HIDE_DELAY: Duration = Duration::from_secs(1);
const FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION: f32 = 0.5;

#[derive(Clone)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub audio_tracks: Vec<PlaybackTrack>,
    pub subtitle_tracks: Vec<PlaybackTrack>,
    pub selected_tracks: PlaybackTrackSelection,
}

impl fmt::Debug for PlaybackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackRequest")
            .field("title", &self.title)
            .field("url", &"<redacted>")
            .field("http_headers", &self.http_headers.len())
            .field("content_length", &self.content_length)
            .field("audio_tracks", &self.audio_tracks.len())
            .field("subtitle_tracks", &self.subtitle_tracks.len())
            .field("selected_tracks", &self.selected_tracks)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PlaybackEvent {
    Back,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackShortcut {
    TogglePlayback,
    ToggleFullscreen,
    ExitFullscreen,
    SeekBackward,
    SeekForward,
    ToggleInfoOverlay,
    RaiseSubtitle,
    LowerSubtitle,
}

pub struct PlaybackPage {
    focus_handle: FocusHandle,
    title: SharedString,
    video: ShutdownOrder<PlaybackBackend, VideoPresenter>,
    video_viewport_bounds: Option<Bounds<Pixels>>,
    video_source_size: Option<RenderSize>,
    current_frame: Option<Arc<RenderImage>>,
    playback_paused: bool,
    playback_buffering: bool,
    playback_position: Option<f64>,
    playback_duration: Option<f64>,
    playback_buffered_until: Option<f64>,
    http_stream_buffered_range: Option<HttpStreamBufferProgress>,
    pending_seek_position: Option<f64>,
    pending_seek_keeps_frame: bool,
    progress_track_bounds: Option<Bounds<Pixels>>,
    progress_drag_position: Option<f64>,
    current_file_loaded: bool,
    playback_info_overlay_visible: bool,
    fullscreen_cursor_visible: bool,
    fullscreen_controls_visible: bool,
    fullscreen_mouse_in_controls: bool,
    fullscreen_controls_hide_generation: u64,
    playback_info: Option<PlaybackVideoInfo>,
    audio_tracks: Vec<PlaybackTrack>,
    subtitle_tracks: Vec<PlaybackTrack>,
    selected_audio_stream_index: Option<usize>,
    selected_subtitle_stream_index: Option<usize>,
    open_track_select: Option<PlaybackTrackKind>,
    active_subtitle: Option<BackendSubtitleCue>,
    subtitle_vertical_offset_fraction: Option<f32>,
    error_message: Option<SharedString>,
    status_message: SharedString,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

fn playback_shortcut_for_key(key: &str) -> Option<PlaybackShortcut> {
    if key == " " || key.eq_ignore_ascii_case("space") {
        return Some(PlaybackShortcut::TogglePlayback);
    }

    if key.eq_ignore_ascii_case("p") {
        Some(PlaybackShortcut::TogglePlayback)
    } else if key.eq_ignore_ascii_case("f") {
        Some(PlaybackShortcut::ToggleFullscreen)
    } else if key.eq_ignore_ascii_case("escape") {
        Some(PlaybackShortcut::ExitFullscreen)
    } else if key.eq_ignore_ascii_case("left") {
        Some(PlaybackShortcut::SeekBackward)
    } else if key.eq_ignore_ascii_case("right") {
        Some(PlaybackShortcut::SeekForward)
    } else if key.eq_ignore_ascii_case("i") {
        Some(PlaybackShortcut::ToggleInfoOverlay)
    } else if key.eq_ignore_ascii_case("r") {
        Some(PlaybackShortcut::RaiseSubtitle)
    } else if key.eq_ignore_ascii_case("t") {
        Some(PlaybackShortcut::LowerSubtitle)
    } else {
        None
    }
}

impl PlaybackPage {
    pub fn new(request: PlaybackRequest, cx: &mut Context<Self>) -> Self {
        let mut error_message = None;
        let status_message = "正在加载视频…".into();

        let (backend, video_presenter) = match FfmpegBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(BackendControl::frame_slot(&backend)) {
                Ok(video_presenter) => {
                    let load_request = BackendLoadRequest {
                        url: request.url.clone(),
                        http_headers: request.http_headers.clone(),
                        content_length: request.content_length,
                        selected_tracks: request.selected_tracks.clone(),
                    };
                    if let Err(error) = backend.command(BackendCommand::Load(load_request)) {
                        error_message = Some(format!("加载视频失败：{error}").into());
                    }
                    (
                        Some(PlaybackBackend::Ffmpeg(backend)),
                        Some(video_presenter),
                    )
                }
                Err(error) => {
                    error_message = Some(format!("创建视频渲染器失败：{error}").into());
                    (Some(PlaybackBackend::Ffmpeg(backend)), None)
                }
            },
            Err(error) => {
                error_message = Some(format!("创建 FFmpeg 播放后端失败：{error}").into());
                (None, None)
            }
        };

        Self {
            focus_handle: cx.focus_handle(),
            title: request.title,
            video: ShutdownOrder::new(backend, video_presenter),
            video_viewport_bounds: None,
            video_source_size: None,
            current_frame: None,
            playback_paused: true,
            playback_buffering: false,
            playback_position: None,
            playback_duration: None,
            playback_buffered_until: None,
            http_stream_buffered_range: None,
            pending_seek_position: None,
            pending_seek_keeps_frame: false,
            progress_track_bounds: None,
            progress_drag_position: None,
            current_file_loaded: false,
            playback_info_overlay_visible: false,
            fullscreen_cursor_visible: false,
            fullscreen_controls_visible: false,
            fullscreen_mouse_in_controls: false,
            fullscreen_controls_hide_generation: 0,
            playback_info: None,
            audio_tracks: request.audio_tracks,
            subtitle_tracks: request.subtitle_tracks,
            selected_audio_stream_index: request.selected_tracks.audio_stream_index,
            selected_subtitle_stream_index: request.selected_tracks.subtitle_stream_index,
            open_track_select: None,
            active_subtitle: None,
            subtitle_vertical_offset_fraction: None,
            error_message,
            status_message,
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn can_toggle_playback(&self) -> bool {
        self.current_file_loaded && self.error_message.is_none()
    }

    fn can_seek_playback(&self) -> bool {
        self.current_file_loaded && self.error_message.is_none() && self.playback_duration.is_some()
    }

    fn progress_bar_visible(&self, is_fullscreen: bool) -> bool {
        self.playback_duration.is_some()
            && playback_progress_bar_visible(is_fullscreen, self.fullscreen_controls_visible)
    }

    fn reset_fullscreen_controls(&mut self) {
        self.fullscreen_cursor_visible = false;
        self.fullscreen_controls_visible = false;
        self.fullscreen_mouse_in_controls = false;
        self.fullscreen_controls_hide_generation =
            self.fullscreen_controls_hide_generation.wrapping_add(1);
        self.open_track_select = None;
    }

    fn schedule_fullscreen_controls_hide(&mut self, cx: &mut Context<Self>) {
        self.fullscreen_controls_hide_generation =
            self.fullscreen_controls_hide_generation.wrapping_add(1);
        let generation = self.fullscreen_controls_hide_generation;

        cx.spawn(async move |page, cx| {
            Timer::after(FULLSCREEN_CONTROLS_HIDE_DELAY).await;
            page.update(cx, |page, cx| {
                page.hide_idle_fullscreen_controls(generation, cx);
            })
            .ok();
        })
        .detach();
    }

    fn hide_idle_fullscreen_controls(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.fullscreen_controls_hide_generation != generation
            || self.fullscreen_mouse_in_controls
            || self.progress_drag_position.is_some()
        {
            return;
        }

        let changed = self.fullscreen_cursor_visible
            || self.fullscreen_controls_visible
            || self.open_track_select.is_some();
        self.fullscreen_cursor_visible = false;
        self.fullscreen_controls_visible = false;
        self.open_track_select = None;
        if changed {
            cx.notify();
        }
    }

    fn back_to_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_visible_frame(window, cx);
        cx.emit(PlaybackEvent::Back);
    }

    fn press_back_button(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.back_to_detail(window, cx);
    }

    fn toggle_playback_pause(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.toggle_playback_pause_command(cx);
    }

    fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
        if !self.can_toggle_playback() {
            return;
        }

        let paused = !self.playback_paused;
        let Some(backend) = self.video.owner_mut() else {
            return;
        };
        let command = if paused {
            BackendCommand::Pause
        } else {
            BackendCommand::Resume
        };
        if let Err(error) = backend.command(command) {
            self.playback_paused = true;
            self.playback_buffering = false;
            self.error_message = Some(format!("控制播放失败：{error}").into());
        } else {
            self.playback_paused = paused;
            if paused {
                self.playback_buffering = false;
            }
        }
        cx.notify();
    }

    fn toggle_audio_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.audio_tracks.is_empty() && self.selected_audio_stream_index.is_none() {
            return;
        }
        self.open_track_select = if self.open_track_select == Some(PlaybackTrackKind::Audio) {
            None
        } else {
            Some(PlaybackTrackKind::Audio)
        };
        cx.notify();
    }

    fn toggle_subtitle_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.subtitle_tracks.is_empty() && self.selected_subtitle_stream_index.is_none() {
            return;
        }
        self.open_track_select = if self.open_track_select == Some(PlaybackTrackKind::Subtitle) {
            None
        } else {
            Some(PlaybackTrackKind::Subtitle)
        };
        cx.notify();
    }

    fn select_audio_track(
        &mut self,
        track_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_track(PlaybackTrackKind::Audio, track_index, window, cx);
    }

    fn select_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_subtitle_track_for_backend(track, window, cx);
    }

    fn select_track(
        &mut self,
        kind: PlaybackTrackKind,
        track_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let previous_audio = self.selected_audio_stream_index;
        let previous_subtitle = self.selected_subtitle_stream_index;
        let mut previous_active_subtitle = None;
        match kind {
            PlaybackTrackKind::Audio => self.selected_audio_stream_index = track_index,
            PlaybackTrackKind::Subtitle => {
                self.selected_subtitle_stream_index = track_index;
                previous_active_subtitle = self.active_subtitle.take();
            }
        }
        self.open_track_select = None;
        self.playback_buffering = self.current_file_loaded;
        self.status_message = "正在切换轨道…".into();

        let command = match kind {
            PlaybackTrackKind::Audio => BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            },
            PlaybackTrackKind::Subtitle => BackendCommand::SetSubtitleTrack {
                track: track_index.and_then(|stream_index| {
                    self.subtitle_tracks
                        .iter()
                        .find(|track| track.stream_index == stream_index)
                        .cloned()
                }),
                position_seconds,
            },
        };
        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(command)
        {
            self.selected_audio_stream_index = previous_audio;
            self.selected_subtitle_stream_index = previous_subtitle;
            self.active_subtitle = previous_active_subtitle.take();
            self.playback_buffering = false;
            self.error_message = Some(format!("切换轨道失败：{error}").into());
        }
        defer_drop_subtitle(previous_active_subtitle, window);
        cx.notify();
    }

    fn select_subtitle_track_for_backend(
        &mut self,
        track: Option<PlaybackTrack>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let previous_audio = self.selected_audio_stream_index;
        let previous_subtitle = self.selected_subtitle_stream_index;
        let mut previous_active_subtitle = self.active_subtitle.take();
        self.selected_subtitle_stream_index = track.as_ref().map(|track| track.stream_index);
        self.open_track_select = None;
        self.playback_buffering = self.current_file_loaded;
        self.status_message = "正在切换轨道…".into();

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            })
        {
            self.selected_audio_stream_index = previous_audio;
            self.selected_subtitle_stream_index = previous_subtitle;
            self.active_subtitle = previous_active_subtitle.take();
            self.playback_buffering = false;
            self.error_message = Some(format!("切换轨道失败：{error}").into());
        }
        defer_drop_subtitle(previous_active_subtitle, window);
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.is_held || event.keystroke.modifiers.modified() {
            return;
        }

        let Some(shortcut) = playback_shortcut_for_key(&event.keystroke.key) else {
            return;
        };
        if shortcut == PlaybackShortcut::ExitFullscreen && !window.is_fullscreen() {
            return;
        }

        cx.stop_propagation();
        match shortcut {
            PlaybackShortcut::TogglePlayback => self.toggle_playback_pause_command(cx),
            PlaybackShortcut::ToggleFullscreen => {
                self.reset_fullscreen_controls();
                window.toggle_fullscreen();
                cx.notify();
            }
            PlaybackShortcut::ExitFullscreen => {
                self.reset_fullscreen_controls();
                window.toggle_fullscreen();
                cx.notify();
            }
            PlaybackShortcut::SeekBackward => {
                self.seek_relative(-KEYBOARD_SEEK_STEP_SECONDS, window, cx);
            }
            PlaybackShortcut::SeekForward => {
                self.seek_relative(KEYBOARD_SEEK_STEP_SECONDS, window, cx);
            }
            PlaybackShortcut::ToggleInfoOverlay => {
                self.playback_info_overlay_visible = !self.playback_info_overlay_visible;
                cx.notify();
            }
            PlaybackShortcut::RaiseSubtitle => {
                self.adjust_subtitle_vertical_offset_fraction(
                    subtitle_vertical_adjust_step(),
                    window,
                    cx,
                );
            }
            PlaybackShortcut::LowerSubtitle => {
                self.adjust_subtitle_vertical_offset_fraction(
                    -subtitle_vertical_adjust_step(),
                    window,
                    cx,
                );
            }
        }
    }

    fn adjust_subtitle_vertical_offset_fraction(
        &mut self,
        delta: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let current_offset_fraction = self.current_subtitle_vertical_offset_fraction(window);
        self.subtitle_vertical_offset_fraction = Some(subtitle_vertical_offset_after_adjustment(
            current_offset_fraction,
            delta,
        ));
        cx.notify();
    }

    fn current_subtitle_vertical_offset_fraction(&self, window: &Window) -> f32 {
        self.subtitle_vertical_offset_fraction
            .or_else(|| self.default_subtitle_vertical_offset_fraction(window))
            .unwrap_or(0.0)
    }

    fn default_subtitle_vertical_offset_fraction(&self, window: &Window) -> Option<f32> {
        let (video_bounds, video_fitted_bounds) = self.current_video_layout_bounds()?;
        let default_bottom = subtitle_overlay_bottom(
            video_fitted_bounds,
            video_bounds,
            self.progress_bar_visible(window.is_fullscreen()),
        );

        Some(subtitle_vertical_offset_fraction(
            video_fitted_bounds,
            subtitle_video_bottom(video_fitted_bounds) - default_bottom,
        ))
    }

    fn current_video_layout_bounds(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let video_bounds = local_video_viewport_bounds(self.video_viewport_bounds?);
        let video_fitted_bounds = aspect_fit_bounds(video_bounds, self.video_source_size?)?;
        Some((video_bounds, video_fitted_bounds))
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !window.is_fullscreen() {
            return;
        }

        let bounds = window_viewport_bounds(window);
        let in_controls = fullscreen_progress_controls_contains(event.position, bounds);
        let in_hot_zone = fullscreen_controls_hot_zone_contains(event.position, bounds);

        let controls_visible = self.fullscreen_controls_visible || in_controls || in_hot_zone;
        let changed = !self.fullscreen_cursor_visible
            || self.fullscreen_controls_visible != controls_visible
            || self.fullscreen_mouse_in_controls != in_controls;

        self.fullscreen_cursor_visible = true;
        self.fullscreen_controls_visible = controls_visible;
        self.fullscreen_mouse_in_controls = in_controls;
        self.schedule_fullscreen_controls_hide(cx);

        if changed {
            cx.notify();
        }
    }

    fn replace_visible_frame(
        &mut self,
        frame: Arc<RenderImage>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self
            .current_frame
            .as_ref()
            .is_some_and(|current| current.id == frame.id)
        {
            self.current_frame = Some(frame);
            return;
        }

        let previous = self.current_frame.replace(frame);
        if let Some(previous) = previous {
            defer_drop_frame(previous, window);
        }
    }

    fn clear_visible_frame(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(frame) = self.current_frame.take() {
            defer_drop_frame(frame, window);
        }
    }

    fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.video_viewport_bounds, bounds) {
            return;
        }

        self.video_viewport_bounds = Some(bounds);
        cx.notify();
    }

    fn update_progress_track_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.progress_track_bounds, bounds) {
            return;
        }

        self.progress_track_bounds = Some(bounds);
        cx.notify();
    }

    fn begin_progress_drag(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_drag(event.position.x, cx);
        cx.stop_propagation();
    }

    fn drag_progress(
        &mut self,
        event: &DragMoveEvent<ProgressBarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_drag(event.event.position.x, cx);
        cx.stop_propagation();
    }

    fn finish_progress_drag(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_progress_drag(window, cx);
        cx.stop_propagation();
    }

    fn update_progress_drag(&mut self, cursor_x: Pixels, cx: &mut Context<Self>) {
        let Some(position) = self.position_for_progress_cursor(cursor_x) else {
            return;
        };
        if self
            .progress_drag_position
            .is_none_or(|current| (current - position).abs() >= 0.02)
        {
            self.progress_drag_position = Some(position);
            cx.notify();
        }
    }

    fn commit_progress_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.progress_drag_position else {
            return;
        };
        self.seek_to_position(position, window, cx);
    }

    fn seek_relative(&mut self, delta_seconds: f64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_seek_playback() {
            return;
        }

        let position = self
            .progress_drag_position
            .or(self.pending_seek_position)
            .or(self.playback_position)
            .unwrap_or(0.0)
            + delta_seconds;
        self.seek_to_position(position, window, cx);
    }

    fn seek_to_position(&mut self, position: f64, window: &mut Window, cx: &mut Context<Self>) {
        let position = self
            .playback_duration
            .map(|duration| clamp_playback_position(position, duration))
            .unwrap_or(position);
        self.progress_drag_position = None;
        let uses_playable_cache_progress = self.video.owner().is_some();
        let keep_current_frame = self.current_frame.is_some()
            && is_seek_position_buffered(
                position,
                self.playback_position,
                self.playback_buffered_until,
                self.http_stream_buffered_range,
                self.playback_duration,
            );
        self.playback_position = Some(position);
        self.playback_buffered_until = if uses_playable_cache_progress {
            Some(position)
        } else {
            self.playback_buffered_until
                .map(|buffered_until| buffered_until.max(position))
        };
        if !keep_current_frame {
            self.http_stream_buffered_range = None;
        }
        self.pending_seek_position = Some(position);
        self.pending_seek_keeps_frame = keep_current_frame;
        self.playback_buffering = !keep_current_frame;
        self.status_message = if keep_current_frame {
            "".into()
        } else {
            "正在跳转…".into()
        };
        if !keep_current_frame {
            self.clear_visible_frame(window, cx);
        }
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.discard_pending_frames();
        }

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::Seek {
                position_seconds: position,
            })
        {
            self.pending_seek_position = None;
            self.pending_seek_keeps_frame = false;
            self.playback_buffering = false;
            self.error_message = Some(format!("跳转播放位置失败：{error}").into());
        }
        cx.notify();
    }

    fn position_for_progress_cursor(&self, cursor_x: Pixels) -> Option<f64> {
        let duration = self.playback_duration?;
        let bounds = self.progress_track_bounds?;
        let fraction = progress_fraction_for_cursor(cursor_x, bounds)?;
        Some(clamp_playback_position(
            duration * fraction as f64,
            duration,
        ))
    }

    fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self
            .video
            .owner_mut()
            .map(|backend| backend.poll_events())
            .unwrap_or_default();
        for event in events {
            match event.kind {
                BackendEventKind::PlaybackRestart => {
                    self.current_file_loaded = true;
                    self.playback_paused = false;
                    self.playback_buffering = false;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.status_message = "".into();
                    self.error_message = None;
                }
                BackendEventKind::LoadFailed(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_info = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.playback_buffered_until = None;
                    self.http_stream_buffered_range = None;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.progress_drag_position = None;
                    defer_drop_subtitle(self.active_subtitle.take(), window);
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("加载视频失败：{message}").into());
                }
                BackendEventKind::Fatal(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_info = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.playback_buffered_until = None;
                    self.http_stream_buffered_range = None;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.progress_drag_position = None;
                    defer_drop_subtitle(self.active_subtitle.take(), window);
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("播放后端错误：{message}").into());
                }
                BackendEventKind::Pause(paused) => {
                    self.playback_paused = paused;
                }
                BackendEventKind::Buffering(buffering) => {
                    let hidden_by_soft_seek =
                        buffering && self.pending_seek_keeps_frame && self.current_frame.is_some();
                    self.playback_buffering = buffering && !hidden_by_soft_seek;
                    if !hidden_by_soft_seek {
                        self.status_message =
                            playback_status_message(buffering, self.current_frame.is_some());
                    }
                }
                BackendEventKind::PlaybackInfoChanged(info) => {
                    self.playback_info = info;
                }
                BackendEventKind::SubtitleChanged(cue) => {
                    if self.active_subtitle != cue {
                        defer_drop_subtitle(self.active_subtitle.take(), window);
                    }
                    self.active_subtitle = cue;
                }
                BackendEventKind::VideoSizeChanged(size) => {
                    if self.video_source_size != size {
                        self.video_source_size = size;
                        self.clear_visible_frame(window, cx);
                    }
                    if let (Some(info), Some(size)) = (self.playback_info.as_mut(), size) {
                        info.size = size;
                    }
                }
                BackendEventKind::PositionChanged(position) => {
                    if should_apply_backend_position(
                        self.progress_drag_position,
                        self.pending_seek_position,
                    ) {
                        self.playback_position = valid_playback_time(position);
                    }
                }
                BackendEventKind::DurationChanged(duration) => {
                    self.playback_duration = valid_playback_duration(duration);
                    if let (Some(drag_position), Some(duration)) =
                        (self.progress_drag_position, self.playback_duration)
                    {
                        self.progress_drag_position =
                            Some(clamp_playback_position(drag_position, duration));
                    }
                }
                BackendEventKind::BufferedChanged(buffered_until) => {
                    self.playback_buffered_until = buffered_until.and_then(valid_playback_time);
                }
                BackendEventKind::HttpStreamBufferedChanged(progress) => {
                    self.http_stream_buffered_range =
                        progress.and_then(valid_http_stream_buffer_progress);
                }
            }
        }
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.prewarm_if_needed();
        }

        let render_size = self
            .video_viewport_bounds
            .zip(self.video_source_size)
            .and_then(|(viewport_bounds, source_size)| {
                render_output_size(viewport_bounds, source_size)
            });
        if should_render_frame(
            self.video.dependent().is_some(),
            self.current_file_loaded,
            self.error_message.is_some(),
            self.video_source_size.is_some(),
            render_size.is_some(),
        ) {
            let size = render_size.expect("render size checked above");
            let render_result = self
                .video
                .dependent_mut()
                .expect("video presenter checked above")
                .render_if_needed(size);

            match render_result {
                Ok(Some(frame)) => {
                    self.replace_visible_frame(frame, window, cx);
                    if !self.playback_buffering {
                        self.status_message = "".into();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.playback_paused = true;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("渲染视频失败：{error}").into());
                }
            }
        } else {
            self.clear_visible_frame(window, cx);
        }
    }

    fn message_text(&self) -> SharedString {
        self.error_message
            .clone()
            .unwrap_or_else(|| self.status_message.clone())
    }

    fn playback_control_button(
        id: &'static str,
        icon_path: &'static str,
        button_size: Pixels,
        icon_size: Pixels,
        enabled: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme::get(cx);
        let color = if enabled {
            theme.foreground.opacity(0.92)
        } else {
            theme.foreground.opacity(0.52)
        };

        div()
            .id(id)
            .flex()
            .size(button_size)
            .items_center()
            .justify_center()
            .rounded_full()
            .text_color(color)
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(theme.foreground.opacity(0.14)))
            })
            .when(!enabled, |this| this.cursor_default().opacity(0.62))
            .child(svg().path(icon_path).size(icon_size).text_color(color))
    }

    fn render_playback_info_overlay(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(info) = self.playback_info.as_ref() else {
            return div().id("playback-info-overlay-empty").into_any_element();
        };

        let theme = theme::get(cx);
        playback_info_segments(info)
            .into_iter()
            .fold(
                div()
                    .id("playback-info-overlay")
                    .absolute()
                    .right_4()
                    .top_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.42))
                    .bg(rgba(0x000000a8))
                    .px_3()
                    .py_2()
                    .shadow_lg()
                    .occlude()
                    .text_xs()
                    .text_color(theme.foreground.opacity(0.9)),
                |this, segment| {
                    this.child(
                        div()
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .bg(theme.foreground.opacity(0.08))
                            .px_2()
                            .child(segment),
                    )
                },
            )
            .into_any_element()
    }

    fn render_subtitle_overlay(
        &self,
        progress_bar_visible: bool,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        let Some(cue) = self.active_subtitle.as_ref() else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(observed_video_bounds) = self.video_viewport_bounds else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        // Canvas observations are window-relative, while absolute children below
        // are laid out relative to the playback view.
        let video_bounds = local_video_viewport_bounds(observed_video_bounds);
        let Some(source_size) = self.video_source_size else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(video_fitted_bounds) = aspect_fit_bounds(video_bounds, source_size) else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };

        if !cue.has_content() {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        }

        let bitmap_canvas_size = subtitle_bitmap_canvas_size(cue).unwrap_or(source_size);
        let bitmap_bounds =
            aspect_fit_bounds(video_bounds, bitmap_canvas_size).unwrap_or(video_fitted_bounds);
        let scale_x = bitmap_bounds.size.width / px(bitmap_canvas_size.width as f32);
        let scale_y = bitmap_bounds.size.height / px(bitmap_canvas_size.height as f32);
        let subtitle_bottom =
            subtitle_overlay_bottom(video_fitted_bounds, video_bounds, progress_bar_visible);
        let subtitle_render_bottom = subtitle_render_bottom(
            video_fitted_bounds,
            subtitle_bottom,
            self.subtitle_vertical_offset_fraction,
        );
        let bitmap_top = if self.subtitle_vertical_offset_fraction.is_some() {
            subtitle_bitmap_overlay_top_for_bottom(
                cue,
                bitmap_bounds,
                scale_y,
                subtitle_render_bottom,
            )
        } else {
            let bitmap_bottom_offset =
                subtitle_bitmap_bottom_offset(cue, bitmap_bounds, scale_y, subtitle_bottom);
            subtitle_bitmap_overlay_top(bitmap_bounds, bitmap_bottom_offset)
        };
        let bitmap_overlay = cue.bitmaps.iter().fold(
            div()
                .id("playback-subtitle-bitmap-overlay")
                .absolute()
                .left(bitmap_bounds.origin.x)
                .top(bitmap_top)
                .w(bitmap_bounds.size.width)
                .h(bitmap_bounds.size.height),
            |this, bitmap| this.child(render_subtitle_bitmap(bitmap, scale_x, scale_y)),
        );
        let overlay = div()
            .id("playback-subtitle-overlay")
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h_full()
            .child(bitmap_overlay);

        if cue.text.trim().is_empty() {
            return overlay.into_any_element();
        }

        let text_overlay_bounds = subtitle_text_overlay_bounds(
            video_fitted_bounds,
            subtitle_render_bottom,
            self.subtitle_vertical_offset_fraction,
        );
        overlay
            .child(
                div()
                    .id("playback-subtitle-text-overlay")
                    .absolute()
                    .left(text_overlay_bounds.origin.x)
                    .top(text_overlay_bounds.origin.y)
                    .w(text_overlay_bounds.size.width)
                    .h(text_overlay_bounds.size.height)
                    .flex()
                    .justify_center()
                    .items_end()
                    .px_6()
                    .child(
                        div()
                            .max_w(relative(0.86))
                            .px_3()
                            .text_center()
                            .text_3xl()
                            .line_height(px(36.0))
                            .text_color(rgb(0xffffff))
                            .child(cue.text.clone()),
                    ),
            )
            .into_any_element()
    }

    fn render_track_select_menu(
        &self,
        kind: PlaybackTrackKind,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let (id, tracks, selected) = match kind {
            PlaybackTrackKind::Audio => (
                "playback-audio-menu",
                &self.audio_tracks,
                self.selected_audio_stream_index,
            ),
            PlaybackTrackKind::Subtitle => (
                "playback-caption-menu",
                &self.subtitle_tracks,
                self.selected_subtitle_stream_index,
            ),
        };
        let off_selected = selected.is_none();
        let off_mouse_down = cx.listener(move |page: &mut PlaybackPage, _, window, cx| {
            cx.stop_propagation();
            match kind {
                PlaybackTrackKind::Audio => page.select_audio_track(None, window, cx),
                PlaybackTrackKind::Subtitle => page.select_subtitle_track(None, window, cx),
            }
        });

        tracks
            .iter()
            .enumerate()
            .fold(
                div()
                    .id(id)
                    .absolute()
                    .right_0()
                    .bottom(px(38.0))
                    .flex()
                    .flex_col()
                    .min_w(px(190.0))
                    .max_w(px(280.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.62))
                    .bg(rgba(0x000000dd))
                    .py_1()
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        track_select_option("Off", off_selected, cx)
                            .id("playback-track-off-option")
                            .on_mouse_down(MouseButton::Left, off_mouse_down),
                    ),
                |this, (index, track)| {
                    let track = track.clone();
                    let stream_index = track.stream_index;
                    let selected = selected == Some(stream_index);
                    let label = if track.is_external {
                        format!("{} 外挂", track.label)
                    } else {
                        track.label.to_string()
                    };
                    let on_mouse_down =
                        cx.listener(move |page: &mut PlaybackPage, _, window, cx| {
                            cx.stop_propagation();
                            match kind {
                                PlaybackTrackKind::Audio => {
                                    page.select_audio_track(Some(stream_index), window, cx)
                                }
                                PlaybackTrackKind::Subtitle => {
                                    page.select_subtitle_track(Some(track.clone()), window, cx)
                                }
                            }
                        });
                    this.child(
                        track_select_option(label, selected, cx)
                            .id((
                                gpui::ElementId::from("playback-track-option"),
                                index.to_string(),
                            ))
                            .on_mouse_down(MouseButton::Left, on_mouse_down),
                    )
                },
            )
            .into_any_element()
    }

    fn render_progress_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(duration) = self.playback_duration else {
            return div().id("playback-progress-empty").into_any_element();
        };

        let theme = theme::get(cx);
        let position = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let played_fraction = progress_fraction(position, duration);
        let playback_buffered_fraction =
            buffered_progress_fraction(self.playback_buffered_until, position, duration);
        let http_stream_buffered_range = http_stream_buffered_range_fractions(
            self.http_stream_buffered_range,
            playback_buffered_fraction,
        );
        let current_time = format_playback_time(position);
        let duration_time = format_playback_time(duration);
        let view = cx.entity().downgrade();
        let track_observer = canvas(|bounds, _, _| bounds, {
            let view = view.clone();
            move |_bounds, observed_bounds, window, _app| {
                let view = view.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_progress_track_bounds(observed_bounds, cx);
                    });
                });
            }
        })
        .absolute()
        .size_full();
        let can_toggle_playback = self.can_toggle_playback();
        let play_pause_icon = if self.playback_paused {
            "icons/play.svg"
        } else {
            "icons/pause.svg"
        };
        let can_select_audio =
            !self.audio_tracks.is_empty() || self.selected_audio_stream_index.is_some();
        let can_select_subtitle =
            !self.subtitle_tracks.is_empty() || self.selected_subtitle_stream_index.is_some();
        let audio_select_open = self.open_track_select == Some(PlaybackTrackKind::Audio);
        let subtitle_select_open = self.open_track_select == Some(PlaybackTrackKind::Subtitle);

        div()
            .id("playback-progress")
            .absolute()
            .left(relative(0.3))
            .bottom(px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX))
            .flex()
            .flex_col()
            .w(relative(0.4))
            .h(px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX))
            .justify_center()
            .gap_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.input_border.opacity(0.42))
            .bg(rgba(0x00000099))
            .px_4()
            .shadow_lg()
            .occlude()
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .text_xs()
            .text_color(theme.foreground.opacity(0.86))
            .child(
                div()
                    .id("playback-controls")
                    .relative()
                    .w_full()
                    .h(px(34.0))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(Self::playback_control_button(
                                "playback-previous-button",
                                "icons/previous.svg",
                                px(30.0),
                                px(16.0),
                                false,
                                cx,
                            ))
                            .child(
                                Self::playback_control_button(
                                    "playback-play-pause-button",
                                    play_pause_icon,
                                    px(34.0),
                                    px(18.0),
                                    can_toggle_playback,
                                    cx,
                                )
                                .when(
                                    can_toggle_playback,
                                    |this| {
                                        this.on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(Self::toggle_playback_pause),
                                        )
                                    },
                                ),
                            )
                            .child(Self::playback_control_button(
                                "playback-next-button",
                                "icons/next.svg",
                                px(30.0),
                                px(16.0),
                                false,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        Self::playback_control_button(
                                            "playback-audio-button",
                                            "icons/audio.svg",
                                            px(30.0),
                                            px(16.0),
                                            can_select_audio,
                                            cx,
                                        )
                                        .when(
                                            can_select_audio,
                                            |this| {
                                                this.on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(Self::toggle_audio_track_select),
                                                )
                                            },
                                        ),
                                    )
                                    .when(audio_select_open, |this| {
                                        this.child(
                                            deferred(self.render_track_select_menu(
                                                PlaybackTrackKind::Audio,
                                                cx,
                                            ))
                                            .with_priority(1),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        Self::playback_control_button(
                                            "playback-caption-button",
                                            "icons/caption.svg",
                                            px(30.0),
                                            px(16.0),
                                            can_select_subtitle,
                                            cx,
                                        )
                                        .when(
                                            can_select_subtitle,
                                            |this| {
                                                this.on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(Self::toggle_subtitle_track_select),
                                                )
                                            },
                                        ),
                                    )
                                    .when(subtitle_select_open, |this| {
                                        this.child(
                                            deferred(self.render_track_select_menu(
                                                PlaybackTrackKind::Subtitle,
                                                cx,
                                            ))
                                            .with_priority(1),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.0))
                            .text_align(gpui::TextAlign::Left)
                            .child(current_time),
                    )
                    .child(
                        div()
                            .id("playback-progress-track")
                            .relative()
                            .flex_1()
                            .h(px(28.0))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::begin_progress_drag),
                            )
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(Self::finish_progress_drag),
                            )
                            .on_drag(ProgressBarDrag, |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| ProgressBarDrag)
                            })
                            .on_drag_move(cx.listener(Self::drag_progress))
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .top(px(11.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(theme.input_border.opacity(0.48)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px(11.0))
                                    .h(px(6.0))
                                    .w(relative(playback_buffered_fraction))
                                    .rounded_full()
                                    .bg(theme.muted_foreground.opacity(0.54)),
                            )
                            .when_some(
                                http_stream_buffered_range,
                                |this, (start_fraction, end_fraction)| {
                                    this.child(
                                        div()
                                            .absolute()
                                            .left(relative(start_fraction))
                                            .top(px(11.0))
                                            .h(px(6.0))
                                            .w(relative(end_fraction - start_fraction))
                                            .rounded_full()
                                            .bg(theme.muted_foreground.opacity(0.54)),
                                    )
                                },
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px(11.0))
                                    .h(px(6.0))
                                    .w(relative(played_fraction))
                                    .rounded_full()
                                    .bg(theme.input_border_focused),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top(px(7.0))
                                    .left(relative(played_fraction))
                                    .ml(-px(6.0))
                                    .size(px(14.0))
                                    .rounded_full()
                                    .bg(theme.foreground)
                                    .border_1()
                                    .border_color(theme.input_border_focused.opacity(0.7)),
                            )
                            .child(track_observer),
                    )
                    .child(
                        div()
                            .w(px(48.0))
                            .text_align(gpui::TextAlign::Right)
                            .child(duration_time),
                    ),
            )
            .into_any_element()
    }
}

fn playback_info_segments(info: &PlaybackVideoInfo) -> Vec<String> {
    let mut segments = vec![
        info.decoder.clone(),
        format!("{}x{}", info.size.width, info.size.height),
    ];
    if let Some(frame_rate) = info.frame_rate.and_then(valid_frame_rate) {
        segments.push(format!("{frame_rate:.2} FPS"));
    }
    segments.push(if info.hardware_accelerated {
        "HW".to_string()
    } else {
        "SW".to_string()
    });
    segments
}

fn track_select_option(
    label: impl Into<SharedString>,
    selected: bool,
    cx: &Context<PlaybackPage>,
) -> gpui::Div {
    let theme = theme::get(cx);
    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .justify_between()
        .gap_3()
        .px_3()
        .text_xs()
        .text_color(theme.foreground.opacity(if selected { 1.0 } else { 0.82 }))
        .bg(if selected {
            theme.foreground.opacity(0.14)
        } else {
            theme.background.opacity(0.0)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(theme.foreground.opacity(0.12)))
        .child(div().min_w_0().truncate().child(label.into()))
}

fn render_subtitle_bitmap(
    bitmap: &BackendSubtitleBitmap,
    scale_x: f32,
    scale_y: f32,
) -> impl IntoElement {
    div()
        .absolute()
        .left(px(bitmap.x as f32) * scale_x)
        .top(px(bitmap.y as f32) * scale_y)
        .w(px(bitmap.width as f32) * scale_x)
        .h(px(bitmap.height as f32) * scale_y)
        .child(SubtitleBitmapElement {
            image: bitmap.image.clone(),
        })
}

fn local_video_viewport_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), bounds.size)
}

fn window_viewport_bounds(window: &Window) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), window.viewport_size())
}

fn playback_progress_bar_visible(is_fullscreen: bool, fullscreen_controls_visible: bool) -> bool {
    !is_fullscreen || fullscreen_controls_visible
}

fn fullscreen_controls_hot_zone_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
) -> bool {
    position.y
        >= viewport_bounds.origin.y
            + viewport_bounds.size.height * FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION
}

fn fullscreen_progress_controls_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
) -> bool {
    playback_progress_bar_bounds(viewport_bounds).contains(&position)
}

fn playback_progress_bar_bounds(viewport_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        gpui::point(
            viewport_bounds.origin.x + viewport_bounds.size.width * 0.3,
            viewport_bounds.origin.y + viewport_bounds.size.height
                - px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX + PLAYBACK_PROGRESS_BAR_HEIGHT_PX),
        ),
        gpui::size(
            viewport_bounds.size.width * 0.4,
            px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX),
        ),
    )
}

fn subtitle_overlay_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    video_bounds: Bounds<Pixels>,
    progress_bar_visible: bool,
) -> Pixels {
    let video_bottom = subtitle_video_bottom(video_fitted_bounds);
    if progress_bar_visible {
        let controls_top = playback_progress_bar_bounds(video_bounds).origin.y;
        video_bottom.min(controls_top)
    } else {
        video_bottom
    }
}

fn subtitle_video_bottom(video_fitted_bounds: Bounds<Pixels>) -> Pixels {
    video_fitted_bounds.origin.y + video_fitted_bounds.size.height
}

fn subtitle_render_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    default_subtitle_bottom: Pixels,
    subtitle_vertical_offset_fraction: Option<f32>,
) -> Pixels {
    subtitle_vertical_offset_fraction.map_or(default_subtitle_bottom, |offset_fraction| {
        subtitle_video_bottom(video_fitted_bounds)
            - subtitle_vertical_offset_pixels(video_fitted_bounds, offset_fraction)
    })
}

fn subtitle_text_overlay_bounds(
    video_fitted_bounds: Bounds<Pixels>,
    subtitle_bottom: Pixels,
    subtitle_vertical_offset_fraction: Option<f32>,
) -> Bounds<Pixels> {
    let height = if subtitle_vertical_offset_fraction.is_some() {
        video_fitted_bounds.size.height
    } else {
        subtitle_text_overlay_height_for_bottom(video_fitted_bounds, subtitle_bottom)
    };

    Bounds::new(
        gpui::point(video_fitted_bounds.origin.x, subtitle_bottom - height),
        gpui::size(video_fitted_bounds.size.width, height),
    )
}

fn subtitle_bitmap_overlay_top(
    bitmap_bounds: Bounds<Pixels>,
    bitmap_bottom_offset: Pixels,
) -> Pixels {
    bitmap_bounds.origin.y - bitmap_bottom_offset
}

fn subtitle_bitmap_overlay_top_for_bottom(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
    subtitle_bottom: Pixels,
) -> Pixels {
    let Some(content_bottom) = subtitle_bitmap_content_bottom(cue, bitmap_bounds, scale_y) else {
        return bitmap_bounds.origin.y;
    };

    bitmap_bounds.origin.y - (content_bottom - subtitle_bottom)
}

fn subtitle_vertical_offset_after_adjustment(current_offset: f32, delta: f32) -> f32 {
    current_offset + delta
}

fn subtitle_vertical_adjust_step() -> f32 {
    SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION
}

fn subtitle_vertical_offset_pixels(
    video_fitted_bounds: Bounds<Pixels>,
    offset_fraction: f32,
) -> Pixels {
    video_fitted_bounds.size.height * offset_fraction
}

fn subtitle_vertical_offset_fraction(video_fitted_bounds: Bounds<Pixels>, offset: Pixels) -> f32 {
    let video_height = f32::from(video_fitted_bounds.size.height);
    if video_height > 0.0 {
        f32::from(offset) / video_height
    } else {
        0.0
    }
}

#[cfg(test)]
fn subtitle_text_overlay_height(
    video_fitted_bounds: Bounds<Pixels>,
    video_bounds: Bounds<Pixels>,
    progress_bar_visible: bool,
) -> Pixels {
    let bottom = subtitle_overlay_bottom(video_fitted_bounds, video_bounds, progress_bar_visible);
    subtitle_text_overlay_height_for_bottom(video_fitted_bounds, bottom)
}

fn subtitle_text_overlay_height_for_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    bottom: Pixels,
) -> Pixels {
    let top = video_fitted_bounds.origin.y;

    if bottom > top { bottom - top } else { px(0.0) }
}

fn subtitle_bitmap_bottom_offset(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
    subtitle_bottom: Pixels,
) -> Pixels {
    let content_bottom = subtitle_bitmap_content_bottom(cue, bitmap_bounds, scale_y);

    content_bottom
        .filter(|content_bottom| *content_bottom > subtitle_bottom)
        .map(|content_bottom| content_bottom - subtitle_bottom)
        .unwrap_or(px(0.0))
}

fn subtitle_bitmap_content_bottom(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
) -> Option<Pixels> {
    cue.bitmaps.iter().fold(None, |bottom, bitmap| {
        let bitmap_bottom =
            bitmap_bounds.origin.y + px(bitmap.y.saturating_add(bitmap.height) as f32) * scale_y;
        Some(bottom.map_or(bitmap_bottom, |bottom: Pixels| bottom.max(bitmap_bottom)))
    })
}

fn subtitle_bitmap_canvas_size(cue: &BackendSubtitleCue) -> Option<RenderSize> {
    cue.bitmaps
        .iter()
        .filter(|bitmap| bitmap.canvas_width > 0 && bitmap.canvas_height > 0)
        .fold(None, |size, bitmap| {
            Some(match size {
                Some(size) => RenderSize {
                    width: size.width.max(bitmap.canvas_width),
                    height: size.height.max(bitmap.canvas_height),
                },
                None => RenderSize {
                    width: bitmap.canvas_width,
                    height: bitmap.canvas_height,
                },
            })
        })
}

fn defer_drop_subtitle(cue: Option<BackendSubtitleCue>, window: &mut Window) {
    if let Some(cue) = cue {
        for bitmap in cue.bitmaps {
            defer_drop_frame(bitmap.image, window);
        }
    }
}

struct SubtitleBitmapElement {
    image: Arc<RenderImage>,
}

impl gpui::Element for SubtitleBitmapElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = gpui::Style {
            size: gpui::Size {
                width: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
                height: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
            },
            ..Default::default()
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut gpui::App,
    ) {
        _ = window.paint_image(
            bounds,
            gpui::Corners::default(),
            self.image.clone(),
            0,
            false,
        );
    }
}

impl IntoElement for SubtitleBitmapElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn valid_frame_rate(frame_rate: f64) -> Option<f64> {
    frame_rate
        .is_finite()
        .then_some(frame_rate)
        .filter(|rate| *rate > 0.0)
}

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_backend(window, cx);
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }

        let current_frame = self.current_frame.clone();
        let current_video_frame = current_frame
            .clone()
            .zip(self.video_source_size)
            .map(|(frame, source_size)| VideoFrameElement { frame, source_size });
        let show_message =
            self.error_message.is_some() || current_frame.is_none() || self.playback_buffering;
        let message_text = self.message_text();
        let theme = theme::get(cx);
        let is_fullscreen = window.is_fullscreen();
        let progress_bar_visible = self.progress_bar_visible(is_fullscreen);
        let view = cx.entity().downgrade();
        let viewport_observer = canvas(
            |bounds, _, _| bounds,
            move |_bounds, observed_bounds, window, _app| {
                let view = view.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_video_viewport(observed_bounds, cx);
                    });
                });
            },
        )
        .absolute()
        .size_full();
        let has_viewport = self
            .video_viewport_bounds
            .is_some_and(|viewport_bounds| normalize_video_viewport(viewport_bounds).is_some());
        if should_request_animation_frame(AnimationFrameRequestState {
            has_backend: self.video.owner().is_some(),
            has_video_presenter: self.video.dependent().is_some(),
            has_loaded_file: self.current_file_loaded,
            has_error: self.error_message.is_some(),
            has_viewport,
            has_visible_frame: current_frame.is_some(),
            playback_paused: self.playback_paused,
            playback_buffering: self.playback_buffering,
            pending_seek: self.pending_seek_position.is_some(),
        }) {
            window.request_animation_frame();
        }

        div()
            .key_context("PlaybackPage")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .text_color(rgb(0xe6edf3))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .when(is_fullscreen && !self.fullscreen_cursor_visible, |this| {
                this.cursor(CursorStyle::None)
            })
            .when(!window.is_maximized() && !is_fullscreen, |this| {
                this.rounded_b(theme.radius_lg).overflow_hidden()
            })
            .when_some(current_video_frame, |this, frame| this.child(frame))
            .when(show_message, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_base()
                        .text_color(rgb(0x9aa5b1))
                        .child(message_text),
                )
            })
            .child(viewport_observer)
            .when(self.playback_info_overlay_visible, |this| {
                this.child(self.render_playback_info_overlay(cx))
            })
            .when(is_fullscreen, |this| {
                this.child(
                    div()
                        .id("playback-fullscreen-mouse-capture")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .on_mouse_move(cx.listener(Self::handle_mouse_move))
                        .when(!self.fullscreen_cursor_visible, |this| {
                            this.cursor(CursorStyle::None)
                        }),
                )
            })
            .child(self.render_subtitle_overlay(progress_bar_visible, cx))
            .when(progress_bar_visible, |this| {
                this.child(self.render_progress_bar(cx))
            })
            .when(!is_fullscreen, |this| {
                this.child(
                    div()
                        .id("playback-back-button")
                        .absolute()
                        .left_4()
                        .top_4()
                        .flex()
                        .size(px(32.0))
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .hover(move |style| style.bg(theme.secondary_hover))
                        .child(
                            svg()
                                .path("icons/chevron-left.svg")
                                .size(px(18.0))
                                .text_color(theme.foreground),
                        )
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::press_back_button))
                        .on_mouse_up(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests;
