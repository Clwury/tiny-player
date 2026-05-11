use std::{fmt, sync::Arc};

use gpui::{
    Bounds, Context, DragMoveEvent, EventEmitter, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Pixels, Render, RenderImage, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, div, prelude::*, px, relative, rgb, rgba,
    svg,
};

use crate::theme;

use super::{
    backend::{BackendEvent, FfmpegBackend},
    render_host::RenderSize,
    video_presenter::VideoPresenter,
};

#[derive(Clone)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
    pub http_headers: Vec<(String, String)>,
}

impl fmt::Debug for PlaybackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackRequest")
            .field("title", &self.title)
            .field("url", &"<redacted>")
            .field("http_headers", &self.http_headers.len())
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
                    if let Err(error) = backend.load_url(&request.url, request.http_headers.clone())
                    {
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
        self.playback_position = Some(position);
        self.playback_buffered_until = if uses_playable_cache_progress {
            Some(position)
        } else {
            self.playback_buffered_until
                .map(|buffered_until| buffered_until.max(position))
        };
        self.playback_buffering = true;
        self.status_message = "正在跳转…".into();
        self.clear_visible_frame(window, cx);

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.seek_to(position)
        {
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
                    self.status_message = "".into();
                    self.error_message = None;
                }
                BackendEvent::LoadFailed(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.playback_buffered_until = None;
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
                    self.progress_drag_position = None;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("播放后端错误：{message}").into());
                }
                BackendEvent::Pause(paused) => {
                    self.playback_paused = paused;
                }
                BackendEvent::Buffering(buffering) => {
                    self.playback_buffering = buffering;
                    self.status_message =
                        playback_status_message(buffering, self.current_frame.is_some());
                }
                BackendEvent::VideoSizeChanged(size) => {
                    if self.video_source_size != size {
                        self.video_source_size = size;
                        self.clear_visible_frame(window, cx);
                    }
                }
                BackendEvent::PositionChanged(position) => {
                    if self.progress_drag_position.is_none() {
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
        let buffered_fraction =
            buffered_progress_fraction(self.playback_buffered_until, position, duration);
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

#[derive(Clone, Copy)]
struct ProgressBarDrag;

impl Render for ProgressBarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().hidden()
    }
}

struct VideoFrameElement {
    frame: Arc<RenderImage>,
    source_size: RenderSize,
}

impl gpui::Element for VideoFrameElement {
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
        let Some(fitted_bounds) = aspect_fit_bounds(bounds, self.source_size) else {
            return;
        };

        _ = window.paint_image(
            fitted_bounds,
            gpui::Corners::default(),
            self.frame.clone(),
            0,
            false,
        );
    }
}

impl IntoElement for VideoFrameElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

enum PlaybackBackend {
    Ffmpeg(FfmpegBackend),
}

impl PlaybackBackend {
    fn poll_events(&mut self) -> Vec<BackendEvent> {
        match self {
            Self::Ffmpeg(backend) => backend.poll_events(),
        }
    }

    fn seek_to(&mut self, position_seconds: f64) -> super::backend::Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.seek_to(position_seconds),
        }
    }
}

struct ShutdownOrder<Owner, Dependent> {
    owner: Option<Owner>,
    dependent: Option<Dependent>,
}

impl<Owner, Dependent> ShutdownOrder<Owner, Dependent> {
    fn new(owner: Option<Owner>, dependent: Option<Dependent>) -> Self {
        Self { owner, dependent }
    }

    fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    fn owner_mut(&mut self) -> Option<&mut Owner> {
        self.owner.as_mut()
    }

    fn dependent(&self) -> Option<&Dependent> {
        self.dependent.as_ref()
    }

    fn dependent_mut(&mut self) -> Option<&mut Dependent> {
        self.dependent.as_mut()
    }
}

impl<Owner, Dependent> Drop for ShutdownOrder<Owner, Dependent> {
    fn drop(&mut self) {
        drop(self.dependent.take());
        drop(self.owner.take());
    }
}

fn normalize_video_viewport(bounds: Bounds<Pixels>) -> Option<(u32, u32)> {
    let width = f32::from(bounds.size.width).floor().max(0.0) as u32;
    let height = f32::from(bounds.size.height).floor().max(0.0) as u32;

    (width > 0 && height > 0).then_some((width, height))
}

fn aspect_fit_bounds(bounds: Bounds<Pixels>, source: RenderSize) -> Option<Bounds<Pixels>> {
    if source.width == 0 || source.height == 0 {
        return None;
    }

    let container_width = f32::from(bounds.size.width).max(0.0);
    let container_height = f32::from(bounds.size.height).max(0.0);
    if container_width == 0.0 || container_height == 0.0 {
        return None;
    }

    let source_width = source.width as f32;
    let source_height = source.height as f32;
    let scale = (container_width / source_width).min(container_height / source_height);
    let fitted_width = source_width * scale;
    let fitted_height = source_height * scale;
    let inset_x = (container_width - fitted_width) / 2.0;
    let inset_y = (container_height - fitted_height) / 2.0;

    Some(Bounds::new(
        gpui::point(bounds.origin.x + px(inset_x), bounds.origin.y + px(inset_y)),
        gpui::size(px(fitted_width), px(fitted_height)),
    ))
}

fn render_output_size(bounds: Bounds<Pixels>, source: RenderSize) -> Option<RenderSize> {
    let (width, height) = normalize_video_viewport(aspect_fit_bounds(bounds, source)?)?;
    Some(RenderSize {
        width: width.min(source.width),
        height: height.min(source.height),
    })
}

fn defer_drop_frame(frame: Arc<RenderImage>, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            cx.drop_image(frame, Some(window));
        });
        window.refresh();
    });
    window.refresh();
}

fn viewport_changed(previous: Option<Bounds<Pixels>>, next: Bounds<Pixels>) -> bool {
    previous != Some(next)
}

fn playback_status_message(buffering: bool, has_visible_frame: bool) -> SharedString {
    if buffering {
        "正在缓冲视频…".into()
    } else if has_visible_frame {
        "".into()
    } else {
        "正在加载视频…".into()
    }
}

fn valid_playback_time(time: f64) -> Option<f64> {
    (time.is_finite() && time >= 0.0).then_some(time)
}

fn valid_playback_duration(duration: f64) -> Option<f64> {
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

fn clamp_playback_position(position: f64, duration: f64) -> f64 {
    if !position.is_finite() {
        return 0.0;
    }
    position.clamp(0.0, duration.max(0.0))
}

fn progress_fraction(position: f64, duration: f64) -> f32 {
    let Some(duration) = valid_playback_duration(duration) else {
        return 0.0;
    };
    (clamp_playback_position(position, duration) / duration) as f32
}

fn buffered_progress_fraction(buffered_until: Option<f64>, position: f64, duration: f64) -> f32 {
    let buffered_until = buffered_until.unwrap_or(position).max(position);
    progress_fraction(buffered_until, duration)
}

fn progress_fraction_for_cursor(cursor_x: Pixels, bounds: Bounds<Pixels>) -> Option<f32> {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return None;
    }

    Some(((f32::from(cursor_x) - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0))
}

fn format_playback_time(seconds: f64) -> String {
    let seconds = valid_playback_time(seconds).unwrap_or(0.0).round() as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn should_render_frame(
    has_video_presenter: bool,
    has_loaded_file: bool,
    has_error: bool,
    has_video_size: bool,
    has_viewport: bool,
) -> bool {
    has_video_presenter && has_loaded_file && !has_error && has_video_size && has_viewport
}

#[derive(Clone, Copy)]
struct AnimationFrameRequestState {
    has_backend: bool,
    has_video_presenter: bool,
    has_loaded_file: bool,
    has_error: bool,
    has_viewport: bool,
    has_visible_frame: bool,
    playback_paused: bool,
    playback_buffering: bool,
}

fn should_request_animation_frame(state: AnimationFrameRequestState) -> bool {
    state.has_backend
        && state.has_video_presenter
        && !state.has_error
        && (!state.has_loaded_file
            || state.playback_buffering
            || (state.has_viewport && (!state.playback_paused || !state.has_visible_frame)))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Bounds, point, px, size};

    use super::{
        AnimationFrameRequestState, RenderSize, ShutdownOrder, aspect_fit_bounds,
        buffered_progress_fraction, clamp_playback_position, format_playback_time,
        normalize_video_viewport, playback_status_message, progress_fraction,
        progress_fraction_for_cursor, render_output_size, should_render_frame,
        should_request_animation_frame, valid_playback_duration, valid_playback_time,
        viewport_changed,
    };

    struct DropRecorder {
        name: &'static str,
        drops: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.name);
        }
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
    fn aspect_fit_bounds_letterboxes_wide_video_in_tall_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert_eq!(fitted.origin, point(px(0.0), px(75.0)));
        assert_eq!(fitted.size, size(px(800.0), px(450.0)));
    }

    #[test]
    fn aspect_fit_bounds_pillarboxes_tall_video_in_wide_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(720.0)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 640,
                height: 480,
            },
        )
        .unwrap();

        assert_eq!(fitted.origin, point(px(160.0), px(0.0)));
        assert_eq!(fitted.size, size(px(960.0), px(720.0)));
    }

    #[test]
    fn aspect_fit_bounds_rejects_zero_source_or_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let zero_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(600.0)));

        assert_eq!(
            aspect_fit_bounds(
                bounds,
                RenderSize {
                    width: 0,
                    height: 1080,
                },
            ),
            None
        );
        assert_eq!(
            aspect_fit_bounds(
                zero_bounds,
                RenderSize {
                    width: 1920,
                    height: 1080,
                },
            ),
            None
        );
    }

    #[test]
    fn aspect_fit_bounds_handles_fractional_viewport_sizes() {
        let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 3840,
                height: 2160,
            },
        )
        .unwrap();

        assert_eq!(normalize_video_viewport(fitted), Some((639, 359)));
    }

    #[test]
    fn render_output_size_uses_aspect_fitted_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));

        assert_eq!(
            render_output_size(
                bounds,
                RenderSize {
                    width: 3840,
                    height: 1600,
                },
            ),
            Some(RenderSize {
                width: 1920,
                height: 800,
            })
        );
    }

    #[test]
    fn render_output_size_does_not_upscale_past_source() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(3840.0), px(2160.0)));

        assert_eq!(
            render_output_size(
                bounds,
                RenderSize {
                    width: 1280,
                    height: 720,
                },
            ),
            Some(RenderSize {
                width: 1280,
                height: 720,
            })
        );
    }

    #[test]
    fn playback_status_stays_visible_until_first_frame() {
        assert_eq!(playback_status_message(true, false), "正在缓冲视频…");
        assert_eq!(playback_status_message(false, false), "正在加载视频…");
        assert_eq!(playback_status_message(false, true), "");
    }

    #[test]
    fn playback_time_helpers_reject_invalid_values() {
        assert_eq!(valid_playback_time(12.0), Some(12.0));
        assert_eq!(valid_playback_time(-1.0), None);
        assert_eq!(valid_playback_time(f64::NAN), None);
        assert_eq!(valid_playback_duration(12.0), Some(12.0));
        assert_eq!(valid_playback_duration(0.0), None);
    }

    #[test]
    fn progress_fraction_clamps_position_to_duration() {
        assert_eq!(clamp_playback_position(-5.0, 100.0), 0.0);
        assert_eq!(clamp_playback_position(25.0, 100.0), 25.0);
        assert_eq!(clamp_playback_position(125.0, 100.0), 100.0);
        assert_eq!(progress_fraction(25.0, 100.0), 0.25);
        assert_eq!(progress_fraction(125.0, 100.0), 1.0);
        assert_eq!(progress_fraction(25.0, 0.0), 0.0);
    }

    #[test]
    fn buffered_progress_never_falls_behind_played_progress() {
        assert_eq!(buffered_progress_fraction(Some(20.0), 40.0, 100.0), 0.4);
        assert_eq!(buffered_progress_fraction(Some(80.0), 40.0, 100.0), 0.8);
        assert_eq!(buffered_progress_fraction(None, 40.0, 100.0), 0.4);
    }

    #[test]
    fn progress_cursor_fraction_uses_track_bounds_and_clamps_edges() {
        let bounds = Bounds::new(point(px(100.0), px(0.0)), size(px(400.0), px(28.0)));

        assert_eq!(progress_fraction_for_cursor(px(100.0), bounds), Some(0.0));
        assert_eq!(progress_fraction_for_cursor(px(300.0), bounds), Some(0.5));
        assert_eq!(progress_fraction_for_cursor(px(700.0), bounds), Some(1.0));
        assert_eq!(
            progress_fraction_for_cursor(
                px(100.0),
                Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(28.0))),
            ),
            None
        );
    }

    #[test]
    fn format_playback_time_switches_to_hours_when_needed() {
        assert_eq!(format_playback_time(65.0), "1:05");
        assert_eq!(format_playback_time(3661.0), "1:01:01");
        assert_eq!(format_playback_time(f64::NAN), "0:00");
    }

    #[test]
    fn should_render_frame_requires_loaded_file_video_size_and_valid_viewport() {
        assert!(should_render_frame(true, true, false, true, true));
        assert!(!should_render_frame(false, true, false, true, true));
        assert!(!should_render_frame(true, false, false, true, true));
        assert!(!should_render_frame(true, true, true, true, true));
        assert!(!should_render_frame(true, true, false, false, true));
        assert!(!should_render_frame(true, true, false, true, false));
    }

    #[test]
    fn should_request_animation_frame_drives_initial_load() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_loaded_file: false,
            has_viewport: false,
            playback_paused: true,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_requires_backend_and_presenter() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_backend: false,
                has_loaded_file: false,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_video_presenter: false,
                has_loaded_file: false,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_stops_on_error() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_loaded_file: false,
                has_error: true,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_requires_unpaused_loaded_video_with_viewport() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            playback_paused: false,
            ..animation_frame_request_state()
        }));
        assert!(!should_request_animation_frame(
            animation_frame_request_state()
        ));
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_viewport: false,
                playback_paused: false,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_continues_until_first_visible_frame() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_visible_frame: false,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_continues_while_buffering() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_viewport: false,
            playback_buffering: true,
            ..animation_frame_request_state()
        }));
    }

    fn animation_frame_request_state() -> AnimationFrameRequestState {
        AnimationFrameRequestState {
            has_backend: true,
            has_video_presenter: true,
            has_loaded_file: true,
            has_error: false,
            has_viewport: true,
            has_visible_frame: true,
            playback_paused: true,
            playback_buffering: false,
        }
    }

    #[test]
    fn shutdown_order_drops_dependent_before_owner() {
        let drops = Rc::new(RefCell::new(Vec::new()));
        let recorded_drops = Rc::clone(&drops);

        let presenter = DropRecorder {
            name: "presenter",
            drops: Rc::clone(&drops),
        };
        let backend = DropRecorder {
            name: "backend",
            drops,
        };

        drop(ShutdownOrder::new(Some(backend), Some(presenter)));

        assert_eq!(&*recorded_drops.borrow(), &["presenter", "backend"]);
    }
}
