use std::sync::Arc;

use gpui::{
    Bounds, ClickEvent, Context, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, RenderImage, SharedString, StatefulInteractiveElement, Styled, Window, canvas,
    div, img, prelude::*, px, rgb, svg,
};

use crate::theme;

use super::{
    backend::{BackendEvent, MpvBackend},
    render_host::RenderSize,
    video_presenter::VideoPresenter,
};

const MAX_VIDEO_RENDER_WIDTH: u32 = 1920;
const MAX_VIDEO_RENDER_HEIGHT: u32 = 1080;

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
    video: ShutdownOrder<MpvBackend, VideoPresenter>,
    video_viewport_bounds: Option<Bounds<Pixels>>,
    current_frame: Option<Arc<RenderImage>>,
    playback_paused: bool,
    current_file_loaded: bool,
    error_message: Option<SharedString>,
    status_message: SharedString,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub fn new(request: PlaybackRequest, _cx: &mut Context<Self>) -> Self {
        let mut error_message = None;
        let status_message = "正在加载视频…".into();

        let (backend, video_presenter) = match MpvBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(backend.mpv_mut()) {
                Ok(video_presenter) => {
                    if let Err(error) = backend.load_url(&request.url) {
                        error_message = Some(format!("加载视频失败：{error}").into());
                    }
                    (Some(backend), Some(video_presenter))
                }
                Err(error) => {
                    error_message = Some(format!("创建视频渲染器失败：{error}").into());
                    (Some(backend), None)
                }
            },
            Err(error) => {
                error_message = Some(format!("创建 mpv 播放后端失败：{error}").into());
                (None, None)
            }
        };

        Self {
            title: request.title,
            video: ShutdownOrder::new(backend, video_presenter),
            video_viewport_bounds: None,
            current_frame: None,
            playback_paused: true,
            current_file_loaded: false,
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
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.current_frame.replace(frame) {
            cx.drop_image(previous, Some(window));
        }
    }

    fn clear_visible_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(frame) = self.current_frame.take() {
            cx.drop_image(frame, Some(window));
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
                    self.current_file_loaded = false;
                    self.playback_paused = true;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("加载视频失败：{message}").into());
                }
                BackendEvent::Fatal(message) => {
                    self.current_file_loaded = false;
                    self.playback_paused = true;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("播放后端错误：{message}").into());
                }
                BackendEvent::Pause(paused) => {
                    self.playback_paused = paused;
                }
                BackendEvent::FileTitle(title) => {
                    let _ = title;
                }
                BackendEvent::PositionChanged(position) => {
                    let _ = position;
                }
                BackendEvent::DurationChanged(duration) => {
                    let _ = duration;
                }
            }
        }

        let render_size = self.video_viewport_bounds.and_then(|viewport_bounds| {
            render_size_for_viewport(viewport_bounds, window.scale_factor())
        });
        if should_render_frame(
            self.video.dependent().is_some(),
            self.current_file_loaded,
            self.error_message.is_some(),
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
                    self.status_message = "".into();
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
}

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_backend(window, cx);

        let current_frame = self.current_frame.clone();
        let show_message = self.error_message.is_some() || current_frame.is_none();
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
            .and_then(|viewport_bounds| {
                render_size_for_viewport(viewport_bounds, window.scale_factor())
            })
            .is_some();
        if should_request_animation_frame(
            self.video.owner().is_some(),
            self.video.dependent().is_some(),
            self.current_file_loaded,
            self.error_message.is_some(),
            has_viewport,
            self.playback_paused,
        ) {
            window.request_animation_frame();
        }

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .text_color(rgb(0xe6edf3))
            .when_some(current_frame, |this, frame| {
                this.child(img(frame).absolute().size_full())
            })
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

fn render_size_for_viewport(bounds: Bounds<Pixels>, scale_factor: f32) -> Option<RenderSize> {
    let (width, height) = normalize_video_viewport(bounds)?;
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return None;
    }

    fit_render_size(RenderSize {
        width: round_to_u32(width as f32 * scale_factor)?,
        height: round_to_u32(height as f32 * scale_factor)?,
    })
}

fn fit_render_size(size: RenderSize) -> Option<RenderSize> {
    let width_scale = MAX_VIDEO_RENDER_WIDTH as f32 / size.width as f32;
    let height_scale = MAX_VIDEO_RENDER_HEIGHT as f32 / size.height as f32;
    let scale = width_scale.min(height_scale).min(1.0);

    Some(RenderSize {
        width: round_to_u32(size.width as f32 * scale)?,
        height: round_to_u32(size.height as f32 * scale)?,
    })
}

fn round_to_u32(value: f32) -> Option<u32> {
    let value = value.round();
    (value.is_finite() && value > 0.0 && value <= u32::MAX as f32).then_some((value as u32).max(1))
}

fn viewport_changed(previous: Option<Bounds<Pixels>>, next: Bounds<Pixels>) -> bool {
    previous != Some(next)
}

fn should_render_frame(
    has_video_presenter: bool,
    has_loaded_file: bool,
    has_error: bool,
    has_viewport: bool,
) -> bool {
    has_video_presenter && has_loaded_file && !has_error && has_viewport
}

fn should_request_animation_frame(
    has_backend: bool,
    has_video_presenter: bool,
    has_loaded_file: bool,
    has_error: bool,
    has_viewport: bool,
    playback_paused: bool,
) -> bool {
    has_backend
        && has_video_presenter
        && !has_error
        && (!has_loaded_file || (has_viewport && !playback_paused))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Bounds, point, px, size};

    use super::{
        ShutdownOrder, fit_render_size, normalize_video_viewport, render_size_for_viewport,
        should_render_frame, should_request_animation_frame, viewport_changed,
    };
    use crate::player::render_host::RenderSize;

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
    fn render_size_for_viewport_applies_scale_factor() {
        let bounds = Bounds::new(point(px(3.0), px(5.0)), size(px(320.0), px(180.0)));

        assert_eq!(
            render_size_for_viewport(bounds, 2.0),
            Some(RenderSize {
                width: 640,
                height: 360,
            })
        );
    }

    #[test]
    fn render_size_for_viewport_rejects_invalid_scale_factor() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(180.0)));

        assert_eq!(render_size_for_viewport(bounds, 0.0), None);
        assert_eq!(render_size_for_viewport(bounds, f32::NAN), None);
    }

    #[test]
    fn render_size_for_viewport_rejects_zero_sized_bounds() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));

        assert_eq!(render_size_for_viewport(bounds, 1.0), None);
    }

    #[test]
    fn fit_render_size_preserves_small_sizes() {
        assert_eq!(
            fit_render_size(RenderSize {
                width: 1280,
                height: 720,
            }),
            Some(RenderSize {
                width: 1280,
                height: 720,
            })
        );
    }

    #[test]
    fn fit_render_size_caps_large_sizes_while_preserving_aspect_ratio() {
        assert_eq!(
            fit_render_size(RenderSize {
                width: 3840,
                height: 2160,
            }),
            Some(RenderSize {
                width: 1920,
                height: 1080,
            })
        );
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
    fn should_render_frame_requires_loaded_file_and_valid_viewport() {
        assert!(should_render_frame(true, true, false, true));
        assert!(!should_render_frame(false, true, false, true));
        assert!(!should_render_frame(true, false, false, true));
        assert!(!should_render_frame(true, true, true, true));
        assert!(!should_render_frame(true, true, false, false));
    }

    #[test]
    fn should_request_animation_frame_drives_initial_load() {
        assert!(should_request_animation_frame(
            true, true, false, false, false, true
        ));
    }

    #[test]
    fn should_request_animation_frame_requires_backend_and_presenter() {
        assert!(!should_request_animation_frame(
            false, true, false, false, false, true
        ));
        assert!(!should_request_animation_frame(
            true, false, false, false, false, true
        ));
    }

    #[test]
    fn should_request_animation_frame_stops_on_error() {
        assert!(!should_request_animation_frame(
            true, true, false, true, false, true
        ));
    }

    #[test]
    fn should_request_animation_frame_requires_unpaused_loaded_video_with_viewport() {
        assert!(should_request_animation_frame(
            true, true, true, false, true, false
        ));
        assert!(!should_request_animation_frame(
            true, true, true, false, true, true
        ));
        assert!(!should_request_animation_frame(
            true, true, true, false, false, false
        ));
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
