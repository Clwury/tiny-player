use std::sync::Arc;

use gpui::{
    Bounds, ClickEvent, Context, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, Window, canvas,
    div, prelude::*, px, rgb, svg,
};

use crate::theme;

use super::{
    backend::{BackendEvent, GstBackend, is_gstreamer_matroska_large_block_error},
    ffmpeg_backend::FfmpegBackend,
    render_host::RenderSize,
    video_presenter::VideoPresenter,
};

#[derive(Clone, Debug)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
}

#[derive(Clone, Copy, Debug)]
pub enum PlaybackEvent {
    Back,
}

pub struct PlaybackPage {
    title: SharedString,
    current_url: String,
    video: ShutdownOrder<PlaybackBackend, VideoPresenter>,
    video_viewport_bounds: Option<Bounds<Pixels>>,
    video_source_size: Option<RenderSize>,
    current_frame: Option<Arc<RenderImage>>,
    playback_paused: bool,
    playback_buffering: bool,
    current_file_loaded: bool,
    ffmpeg_fallback_attempted: bool,
    error_message: Option<SharedString>,
    status_message: SharedString,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub fn new(request: PlaybackRequest, _cx: &mut Context<Self>) -> Self {
        let mut error_message = None;
        let status_message = "正在加载视频…".into();

        let (backend, video_presenter) = match GstBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(backend.frame_slot()) {
                Ok(video_presenter) => {
                    if let Err(error) = backend.load_url(&request.url) {
                        error_message = Some(format!("加载视频失败：{error}").into());
                    }
                    (
                        Some(PlaybackBackend::Gstreamer(backend)),
                        Some(video_presenter),
                    )
                }
                Err(error) => {
                    error_message = Some(format!("创建视频渲染器失败：{error}").into());
                    (Some(PlaybackBackend::Gstreamer(backend)), None)
                }
            },
            Err(error) => {
                error_message = Some(format!("创建 GStreamer 播放后端失败：{error}").into());
                (None, None)
            }
        };

        Self {
            title: request.title,
            current_url: request.url,
            video: ShutdownOrder::new(backend, video_presenter),
            video_viewport_bounds: None,
            video_source_size: None,
            current_frame: None,
            playback_paused: true,
            playback_buffering: false,
            current_file_loaded: false,
            ffmpeg_fallback_attempted: false,
            error_message,
            status_message,
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn back_to_detail(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_visible_frame(window, cx);
        cx.emit(PlaybackEvent::Back);
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
                    self.error_message = None;
                }
                BackendEvent::LoadFailed(message) => {
                    if self.should_fallback_to_ffmpeg(&message) {
                        self.start_ffmpeg_fallback(&message, window, cx);
                        continue;
                    }
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("加载视频失败：{message}").into());
                }
                BackendEvent::Fatal(message) => {
                    self.current_file_loaded = false;
                    self.video_source_size = None;
                    self.playback_paused = true;
                    self.playback_buffering = false;
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
                    let _ = position;
                }
                BackendEvent::DurationChanged(duration) => {
                    let _ = duration;
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

    fn should_fallback_to_ffmpeg(&self, message: &str) -> bool {
        !self.ffmpeg_fallback_attempted
            && self
                .video
                .owner()
                .is_some_and(PlaybackBackend::is_gstreamer)
            && is_gstreamer_matroska_large_block_error(message)
    }

    fn start_ffmpeg_fallback(&mut self, reason: &str, window: &mut Window, cx: &mut Context<Self>) {
        tracing::warn!(
            reason,
            "GStreamer Matroska demux failed on a large block; switching to FFmpeg backend"
        );
        self.ffmpeg_fallback_attempted = true;
        self.current_file_loaded = false;
        self.video_source_size = None;
        self.playback_paused = true;
        self.playback_buffering = false;
        self.error_message = None;
        self.status_message = "正在切换 FFmpeg 播放后端…".into();
        self.clear_visible_frame(window, cx);
        self.video = ShutdownOrder::new(None, None);

        let mut backend = match FfmpegBackend::new() {
            Ok(backend) => backend,
            Err(error) => {
                self.error_message = Some(format!("创建 FFmpeg 播放后端失败：{error}").into());
                return;
            }
        };
        let video_presenter = match VideoPresenter::new(backend.frame_slot()) {
            Ok(video_presenter) => video_presenter,
            Err(error) => {
                self.error_message = Some(format!("创建视频渲染器失败：{error}").into());
                return;
            }
        };
        if let Err(error) = backend.load_url(&self.current_url) {
            self.error_message = Some(format!("加载视频失败：{error}").into());
            return;
        }

        self.video = ShutdownOrder::new(
            Some(PlaybackBackend::Ffmpeg(backend)),
            Some(video_presenter),
        );
        cx.notify();
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
        let back_to_detail = cx.listener(Self::back_to_detail);
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
                    .text_color(theme.foreground)
                    .hover(move |style| style.bg(theme.secondary_hover))
                    .child(
                        svg()
                            .path("icons/chevron-left.svg")
                            .size(px(20.0))
                            .text_color(theme.foreground),
                    )
                    .on_click(back_to_detail),
            )
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
    Gstreamer(GstBackend),
    Ffmpeg(FfmpegBackend),
}

impl PlaybackBackend {
    fn poll_events(&mut self) -> Vec<BackendEvent> {
        match self {
            Self::Gstreamer(backend) => backend.poll_events(),
            Self::Ffmpeg(backend) => backend.poll_events(),
        }
    }

    fn is_gstreamer(&self) -> bool {
        matches!(self, Self::Gstreamer(_))
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
        normalize_video_viewport, playback_status_message, render_output_size, should_render_frame,
        should_request_animation_frame, viewport_changed,
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
