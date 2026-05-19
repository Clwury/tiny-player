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

mod backend_events;
mod controls;
mod fullscreen;
mod progress;
mod render;
mod request;
mod runtime;
mod shortcuts;
mod subtitles;
mod video_element;

pub use request::PlaybackRequest;

#[cfg(test)]
use controls::{playback_info_segments, valid_frame_rate};
#[cfg(test)]
use fullscreen::{
    fullscreen_controls_hot_zone_contains, fullscreen_progress_controls_contains,
    playback_progress_bar_bounds, playback_progress_bar_visible,
};
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
#[cfg(test)]
use shortcuts::{PlaybackShortcut, playback_shortcut_for_key};
use subtitles::defer_drop_subtitle;
#[cfg(test)]
use subtitles::{
    local_video_viewport_bounds, subtitle_bitmap_bottom_offset, subtitle_bitmap_canvas_size,
    subtitle_bitmap_overlay_top_for_bottom, subtitle_render_bottom, subtitle_text_overlay_bounds,
    subtitle_text_overlay_height, subtitle_vertical_adjust_step,
    subtitle_vertical_offset_after_adjustment, subtitle_vertical_offset_fraction,
    subtitle_vertical_offset_pixels,
};
use video_element::VideoFrameElement;

#[derive(Clone, Copy, Debug)]
pub enum PlaybackEvent {
    Back,
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

    fn message_text(&self) -> SharedString {
        self.error_message
            .clone()
            .unwrap_or_else(|| self.status_message.clone())
    }
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
