use std::{fmt, sync::Arc};

use gpui::{
    Bounds, Context, DragMoveEvent, EventEmitter, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Pixels, Render, RenderImage, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::*, px, relative, rgb, rgba,
    svg,
};

use crate::theme;

use super::{
    backend::{BackendEvent, FfmpegBackend, HttpStreamBufferProgress},
    render_host::RenderSize,
    video_presenter::VideoPresenter,
};

mod progress;
mod render;
mod runtime;
mod video_element;

#[cfg(test)]
use progress::http_stream_buffered_until;
use progress::{
    ProgressBarDrag, buffered_progress_fraction, clamp_playback_position, combined_buffered_until,
    format_playback_time, is_seek_position_buffered, playback_status_message, progress_fraction,
    progress_fraction_for_cursor, should_apply_backend_position, valid_http_stream_buffer_progress,
    valid_playback_duration, valid_playback_time,
};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, defer_drop_frame, normalize_video_viewport,
    render_output_size, should_render_frame, should_request_animation_frame, viewport_changed,
};
use runtime::{PlaybackBackend, ShutdownOrder};
use video_element::VideoFrameElement;

#[derive(Clone)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
}

impl fmt::Debug for PlaybackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackRequest")
            .field("title", &self.title)
            .field("url", &"<redacted>")
            .field("http_headers", &self.http_headers.len())
            .field("content_length", &self.content_length)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PlaybackEvent {
    Back,
}

pub struct PlaybackPage {
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
    error_message: Option<SharedString>,
    status_message: SharedString,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub fn new(request: PlaybackRequest, _cx: &mut Context<Self>) -> Self {
        let mut error_message = None;
        let status_message = "正在加载视频…".into();

        let (backend, video_presenter) = match FfmpegBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(backend.frame_slot()) {
                Ok(video_presenter) => {
                    if let Err(error) = backend.load_url(
                        &request.url,
                        request.http_headers.clone(),
                        request.content_length,
                    ) {
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
            error_message,
            status_message,
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
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
        let Some(position) = self.progress_drag_position.take() else {
            return;
        };
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
            && let Err(error) = backend.seek_to(position)
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
            match event {
                BackendEvent::PlaybackRestart => {
                    self.current_file_loaded = true;
                    self.playback_paused = false;
                    self.playback_buffering = false;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.status_message = "".into();
                    self.error_message = None;
                }
                BackendEvent::LoadFailed(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.playback_buffered_until = None;
                    self.http_stream_buffered_range = None;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.progress_drag_position = None;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("加载视频失败：{message}").into());
                }
                BackendEvent::Fatal(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.playback_buffered_until = None;
                    self.http_stream_buffered_range = None;
                    self.pending_seek_position = None;
                    self.pending_seek_keeps_frame = false;
                    self.progress_drag_position = None;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("播放后端错误：{message}").into());
                }
                BackendEvent::Pause(paused) => {
                    self.playback_paused = paused;
                }
                BackendEvent::Buffering(buffering) => {
                    let hidden_by_soft_seek =
                        buffering && self.pending_seek_keeps_frame && self.current_frame.is_some();
                    self.playback_buffering = buffering && !hidden_by_soft_seek;
                    if !hidden_by_soft_seek {
                        self.status_message =
                            playback_status_message(buffering, self.current_frame.is_some());
                    }
                }
                BackendEvent::VideoSizeChanged(size) => {
                    if self.video_source_size != size {
                        self.video_source_size = size;
                        self.clear_visible_frame(window, cx);
                    }
                }
                BackendEvent::PositionChanged(position) => {
                    if should_apply_backend_position(
                        self.progress_drag_position,
                        self.pending_seek_position,
                    ) {
                        self.playback_position = valid_playback_time(position);
                    }
                }
                BackendEvent::DurationChanged(duration) => {
                    self.playback_duration = valid_playback_duration(duration);
                    if let (Some(drag_position), Some(duration)) =
                        (self.progress_drag_position, self.playback_duration)
                    {
                        self.progress_drag_position =
                            Some(clamp_playback_position(drag_position, duration));
                    }
                }
                BackendEvent::BufferedChanged(buffered_until) => {
                    self.playback_buffered_until = buffered_until.and_then(valid_playback_time);
                }
                BackendEvent::HttpStreamBufferedChanged(progress) => {
                    self.http_stream_buffered_range =
                        progress.and_then(valid_http_stream_buffer_progress);
                }
            }
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
        let buffered_until = combined_buffered_until(
            self.playback_buffered_until,
            self.http_stream_buffered_range,
            position,
            duration,
        );
        let buffered_fraction = buffered_progress_fraction(buffered_until, position, duration);
        let current_time = format_playback_time(position);
        let duration_time = format_playback_time(duration);
        let view = cx.entity().downgrade();
        let track_observer = canvas(
            |bounds, _, _| bounds,
            move |_bounds, observed_bounds, window, _app| {
                let view = view.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_progress_track_bounds(observed_bounds, cx);
                    });
                });
            },
        )
        .absolute()
        .size_full();

        div()
            .id("playback-progress")
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .h(px(64.0))
            .items_center()
            .gap_3()
            .px_6()
            .pt_5()
            .pb_4()
            .bg(rgba(0x0000006b))
            .text_xs()
            .text_color(theme.foreground.opacity(0.86))
            .child(
                div()
                    .w(px(56.0))
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
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_progress_drag))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_progress_drag))
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
                            .w(relative(buffered_fraction))
                            .rounded_full()
                            .bg(theme.muted_foreground.opacity(0.54)),
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
                    .w(px(56.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(duration_time),
            )
            .into_any_element()
    }
}

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_backend(window, cx);

        let current_frame = self.current_frame.clone();
        let current_video_frame = current_frame
            .clone()
            .zip(self.video_source_size)
            .map(|(frame, source_size)| VideoFrameElement { frame, source_size });
        let show_message =
            self.error_message.is_some() || current_frame.is_none() || self.playback_buffering;
        let message_text = self.message_text();
        let theme = theme::get(cx);
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
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .text_color(rgb(0xe6edf3))
            .when(!window.is_maximized(), |this| {
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
            .child(self.render_progress_bar(cx))
            .child(
                div()
                    .id("playback-back-button")
                    .absolute()
                    .left_4()
                    .top_4()
                    .flex()
                    .size(px(36.0))
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.input_border.opacity(0.7))
                    .bg(theme.dialog_background.opacity(0.72))
                    .shadow_lg()
                    .occlude()
                    .cursor_pointer()
                    .text_color(theme.foreground)
                    .hover(move |style| style.bg(theme.secondary_hover))
                    .child(
                        svg()
                            .path("icons/chevron-left.svg")
                            .size(px(20.0))
                            .text_color(theme.foreground),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::press_back_button))
                    .on_mouse_up(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    }),
            )
    }
}

#[cfg(test)]
mod tests;
