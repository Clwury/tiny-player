use std::{fmt, sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, CursorStyle, DragMoveEvent, EventEmitter, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement, Styled, Timer, Window, canvas, deferred, div,
    prelude::*, px, relative, rgb, rgba, svg,
};

use crate::theme;

use super::{
    backend::{
        BackendCommand, BackendControl, BackendEventKind, BackendLoadRequest,
        BackendSubtitleBitmap, BackendSubtitleCue, FfmpegBackend, PlaybackCacheState,
        PlaybackVideoInfo, StreamCacheKind,
    },
    render_host::RenderSize,
    tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection},
    video_presenter::VideoPresenter,
};

mod backend_events;
mod controls;
mod fullscreen;
mod progress;
mod render;
mod request;
mod runtime;
mod shortcuts;
mod state;
mod subtitles;
mod video_element;

pub use request::PlaybackRequest;

use progress::{
    ProgressBarDrag, buffered_progress_fraction, buffered_until_after_seek,
    byte_cache_range_fractions, cache_range_fractions, cached_seek_target, clamp_playback_position,
    format_playback_time, playback_status_message, progress_fraction, progress_fraction_for_cursor,
    should_apply_backend_position, valid_playback_duration, valid_playback_time,
};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, defer_drop_frame, normalize_video_viewport,
    render_output_size, should_render_frame, should_request_animation_frame, viewport_changed,
};
use runtime::{PlaybackBackend, ShutdownOrder};
use state::{
    FullscreenControlsState, PlaybackFrameState, PlaybackTimelineState, PlaybackVolumeState,
    SubtitleOverlayState, TrackSelectState,
};
use subtitles::defer_drop_subtitle;
use video_element::VideoFrameElement;

#[derive(Clone, Copy, Debug)]
pub enum PlaybackEvent {
    Back,
}

pub struct PlaybackPage {
    focus_handle: FocusHandle,
    title: SharedString,
    video: ShutdownOrder<PlaybackBackend, VideoPresenter>,
    frame: PlaybackFrameState,
    timeline: PlaybackTimelineState,
    playback_info_overlay_visible: bool,
    fullscreen: FullscreenControlsState,
    playback_info: Option<PlaybackVideoInfo>,
    tracks: TrackSelectState,
    subtitle: SubtitleOverlayState,
    volume: PlaybackVolumeState,
    error_message: Option<SharedString>,
    status_message: SharedString,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub fn new(request: PlaybackRequest, cx: &mut Context<Self>) -> Self {
        let mut error_message = None;
        let status_message = "正在加载视频…".into();

        let (backend, video_presenter) = match FfmpegBackend::new() {
            Ok(mut backend) => {
                match VideoPresenter::new(BackendControl::video_output_queue(&backend)) {
                    Ok(video_presenter) => {
                        let load_request = BackendLoadRequest {
                            url: request.url.clone(),
                            http_headers: request.http_headers.clone(),
                            content_length: request.content_length,
                            selected_tracks: request.selected_tracks.clone(),
                            cache_config: Default::default(),
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
                }
            }
            Err(error) => {
                error_message = Some(format!("创建 FFmpeg 播放后端失败：{error}").into());
                (None, None)
            }
        };

        let timeline = PlaybackTimelineState::default();

        Self {
            focus_handle: cx.focus_handle(),
            title: request.title,
            video: ShutdownOrder::new(backend, video_presenter),
            frame: PlaybackFrameState::default(),
            timeline,
            playback_info_overlay_visible: false,
            fullscreen: FullscreenControlsState::default(),
            playback_info: None,
            tracks: TrackSelectState::new(
                request.audio_tracks,
                request.subtitle_tracks,
                request.selected_tracks,
            ),
            subtitle: SubtitleOverlayState::default(),
            volume: PlaybackVolumeState::default(),
            error_message,
            status_message,
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn can_toggle_playback(&self) -> bool {
        self.timeline.loaded && !self.timeline.ended && self.error_message.is_none()
    }

    fn can_seek_playback(&self) -> bool {
        self.timeline.loaded
            && !self.timeline.ended
            && self.error_message.is_none()
            && self.timeline.duration.is_some()
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

    fn toggle_playback_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_fullscreen_controls();
        window.toggle_fullscreen();
        cx.notify();
    }

    fn handle_surface_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.close_track_select(cx) {
            return;
        }
        if event.click_count == 2 {
            self.toggle_playback_fullscreen(window, cx);
        }
    }

    fn handle_surface_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.close_track_select(cx) {
            return;
        }
        if event.click_count == 1 {
            self.toggle_playback_pause_command(cx);
        }
    }

    fn handle_surface_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let delta = volume_delta_from_scroll_delta(event.delta);
        if delta.abs() < f32::EPSILON {
            return;
        }
        self.adjust_playback_volume(delta, cx);
    }

    fn handle_surface_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !window.is_fullscreen() && event.dragging() {
            cx.stop_propagation();
            window.start_window_move();
            return;
        }

        self.handle_mouse_move(event, window, cx);
    }

    fn replace_visible_frame(
        &mut self,
        frame: Arc<RenderImage>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self
            .frame
            .current
            .as_ref()
            .is_some_and(|current| current.id == frame.id)
        {
            self.frame.current = Some(frame);
            return;
        }

        let previous = self.frame.current.replace(frame);
        if let Some(previous) = previous {
            defer_drop_frame(previous, window);
        }
    }

    fn clear_visible_frame(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(frame) = self.frame.current.take() {
            defer_drop_frame(frame, window);
        }
    }

    fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.frame.viewport_bounds, bounds) {
            return;
        }

        self.frame.viewport_bounds = Some(bounds);
        cx.notify();
    }

    fn update_progress_track_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.timeline.progress_track_bounds, bounds) {
            return;
        }

        self.timeline.progress_track_bounds = Some(bounds);
        cx.notify();
    }

    fn message_text(&self) -> SharedString {
        self.error_message
            .clone()
            .unwrap_or_else(|| self.status_message.clone())
    }

    fn render_mouse_capture(&self, is_fullscreen: bool, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("playback-mouse-capture")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::handle_surface_left_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::handle_surface_right_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_surface_mouse_move))
            .on_scroll_wheel(cx.listener(Self::handle_surface_scroll_wheel))
            .when(is_fullscreen && !self.fullscreen.cursor_visible, |this| {
                this.cursor(CursorStyle::None)
            })
    }
}

fn clamp_playback_volume(volume: f32) -> f32 {
    let volume = if volume.is_finite() { volume } else { 1.0 };
    volume.clamp(0.0, 1.0)
}

fn playback_volume_percent(volume: f32) -> u32 {
    (clamp_playback_volume(volume) * 100.0).round() as u32
}

fn volume_delta_from_scroll_delta(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Lines(point) => point.y * 0.05,
        ScrollDelta::Pixels(point) => f32::from(point.y) / 500.0,
    }
    .clamp(-0.2, 0.2)
}

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_backend(window, cx);
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }

        let current_frame = self.frame.current.clone();
        let current_video_frame = current_frame
            .clone()
            .zip(self.frame.source_size)
            .map(|(frame, source_size)| VideoFrameElement { frame, source_size });
        let show_message =
            self.error_message.is_some() || current_frame.is_none() || self.timeline.buffering;
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
            .frame
            .viewport_bounds
            .is_some_and(|viewport_bounds| normalize_video_viewport(viewport_bounds).is_some());
        if should_request_animation_frame(AnimationFrameRequestState {
            has_backend: self.video.owner().is_some(),
            has_video_presenter: self.video.dependent().is_some(),
            has_loaded_file: self.timeline.loaded,
            playback_ended: self.timeline.ended,
            has_error: self.error_message.is_some(),
            has_viewport,
            has_visible_frame: current_frame.is_some(),
            playback_paused: self.timeline.paused,
            playback_buffering: self.timeline.buffering,
            pending_seek: self.timeline.pending_seek_position.is_some(),
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
            .on_scroll_wheel(cx.listener(Self::handle_surface_scroll_wheel))
            .when(is_fullscreen && !self.fullscreen.cursor_visible, |this| {
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
            .child(self.render_mouse_capture(is_fullscreen, cx))
            .when(self.playback_info_overlay_visible, |this| {
                this.child(self.render_playback_info_overlay(cx))
            })
            .child(self.render_subtitle_overlay(progress_bar_visible))
            .when(self.volume.indicator_visible, |this| {
                this.child(self.render_volume_indicator(cx))
            })
            .when(progress_bar_visible, |this| {
                this.child(self.render_progress_bar(cx))
            })
            .when(!is_fullscreen, |this| {
                this.child(self.render_back_button(cx))
            })
    }
}
