use std::{fmt, sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, DragMoveEvent, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, deferred, div, prelude::*, px, relative,
    rgb, rgba, svg,
};

use crate::{app::window_has_rounded_corners, theme};

use super::{
    backend::{
        BackendCommand, BackendControl, BackendEventKind, BackendLoadRequest,
        BackendSubtitleBitmap, BackendSubtitleCue, FfmpegBackend, PlaybackAudioInfo,
        PlaybackCacheState, PlaybackFileInfo, PlaybackSeekMode, PlaybackVideoInfo, StreamCacheKind,
    },
    render_host::RenderSize,
    tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection},
    video_presenter::{VideoPresenter, VideoPresenterSnapshot},
    volume::{PlaybackVolumeSettings, clamp_playback_volume},
};

mod backend_events;
mod controls;
mod diagnostics;
mod episodes;
mod fullscreen;
mod progress;
mod queue;
mod rate;
mod render;
mod request;
mod runtime;
mod session;
mod shortcuts;
mod state;
mod subtitles;
mod video_element;

#[cfg(test)]
mod mouse_tests;

pub use request::{
    EmbyPlaybackContext, PlaybackQueue, PlaybackQueueItem, PlaybackRequest,
    playback_initial_position_seconds,
};
pub(crate) use request::{
    playback_audio_tracks_for_source, playback_subtitle_tracks_for_source,
    preferred_playback_track_selection,
};
pub use session::{PlaybackStateUpdate, PlaybackStopCompletion, PlaybackStopResult};

use progress::{
    ProgressBarDrag, buffered_until_after_seek, cache_range_fractions, cached_seek_target,
    clamp_playback_position, format_playback_time, forward_cache_fraction, progress_fraction,
    progress_fraction_for_cursor, should_apply_backend_position, valid_playback_duration,
    valid_playback_time,
};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, defer_drop_frame, normalize_video_viewport,
    playback_status, render_output_size, render_playback_status, should_render_frame,
    should_request_animation_frame, viewport_changed,
};
use runtime::{PlaybackBackend, ShutdownOrder};
use state::{
    FullscreenControlsState, PlaybackFrameState, PlaybackTimelineState, PlaybackVolumeState,
    SubtitleOverlayState, TrackSelectState, WindowDragState,
};
use subtitles::defer_drop_subtitle;
use video_element::VideoFrameElement;

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    VolumeChanged {
        settings: PlaybackVolumeSettings,
    },
    Update {
        update: PlaybackStateUpdate,
    },
    Back {
        update: PlaybackStateUpdate,
    },
    Replace {
        request: Box<PlaybackRequest>,
        update: PlaybackStateUpdate,
    },
}

pub struct PlaybackPage {
    focus_handle: FocusHandle,
    title: SharedString,
    video: ShutdownOrder<PlaybackBackend, VideoPresenter>,
    frame: PlaybackFrameState,
    timeline: PlaybackTimelineState,
    download_speed: controls::DownloadSpeedDisplay,
    playback_details_visible: bool,
    fullscreen: FullscreenControlsState,
    window_drag: WindowDragState,
    source_protocol: Option<String>,
    source_url: String,
    content_length: Option<u64>,
    playback_file_info: Option<PlaybackFileInfo>,
    playback_info: Option<PlaybackVideoInfo>,
    playback_audio_info: Option<PlaybackAudioInfo>,
    queue: PlaybackQueue,
    episode_list: episodes::PlaybackEpisodeListState,
    emby: EmbyPlaybackContext,
    reporting: session::PlaybackReportingState,
    queue_switch: queue::PlaybackQueueSwitchState,
    tracks: TrackSelectState,
    track_preference_key: super::PlaybackTrackPreferenceKey,
    remember_subtitle_on_start: bool,
    subtitle: SubtitleOverlayState,
    volume: PlaybackVolumeState,
    rate: rate::PlaybackRateState,
    error_message: Option<SharedString>,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub(crate) fn apply_playback_config(
        &mut self,
        config: super::backend::PlaybackCacheConfig,
    ) -> super::backend::Result<()> {
        if let Some(backend) = self.video.owner_mut() {
            backend.command(BackendCommand::SetCacheConfig(config))?;
        }
        Ok(())
    }

    pub fn new(request: PlaybackRequest, cx: &mut Context<Self>) -> Self {
        Self::new_with_cache_config(request, Default::default(), cx)
    }

    pub fn new_with_cache_config(
        request: PlaybackRequest,
        cache_config: super::backend::PlaybackCacheConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_settings(request, cache_config, PlaybackVolumeSettings::default(), cx)
    }

    pub(crate) fn new_with_settings(
        request: PlaybackRequest,
        cache_config: super::backend::PlaybackCacheConfig,
        volume_settings: PlaybackVolumeSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let volume = PlaybackVolumeState::new(volume_settings);
        let mut error_message = None;
        let source_protocol = playback_protocol(&request.url);
        let content_length = request.content_length;
        let reporting = session::PlaybackReportingState::new(&request.emby);

        let (backend, video_presenter) = match FfmpegBackend::new() {
            Ok(mut backend) => {
                match VideoPresenter::new(BackendControl::video_output_queue(&backend)) {
                    Ok(video_presenter) => {
                        let load_request = BackendLoadRequest {
                            url: request.url.clone(),
                            http_headers: request.http_headers.clone(),
                            content_length: request.content_length,
                            start_position_seconds: request.initial_position_seconds,
                            selected_tracks: request.selected_tracks.clone(),
                            cache_config: cache_config.clone().normalized(),
                        };
                        // Restore volume before loading so the first audio samples use it.
                        let load_result = backend
                            .command(BackendCommand::SetVolume {
                                volume: volume.level,
                            })
                            .and_then(|()| backend.command(BackendCommand::Load(load_request)));
                        if let Err(error) = load_result {
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

        let timeline = PlaybackTimelineState {
            position: valid_playback_time(request.initial_position_seconds),
            ..PlaybackTimelineState::default()
        };

        let mut page = Self {
            focus_handle: cx.focus_handle(),
            title: request.title,
            video: ShutdownOrder::new(backend, video_presenter),
            frame: PlaybackFrameState::default(),
            timeline,
            download_speed: controls::DownloadSpeedDisplay::default(),
            playback_details_visible: false,
            fullscreen: FullscreenControlsState::default(),
            window_drag: WindowDragState::default(),
            source_protocol,
            source_url: request.url,
            content_length,
            playback_file_info: None,
            playback_info: None,
            playback_audio_info: None,
            queue: request.queue,
            episode_list: episodes::PlaybackEpisodeListState::default(),
            emby: request.emby,
            reporting,
            queue_switch: queue::PlaybackQueueSwitchState::default(),
            tracks: TrackSelectState::new(
                request.audio_tracks,
                request.subtitle_tracks,
                request.selected_tracks,
            ),
            track_preference_key: request.track_preference_key,
            remember_subtitle_on_start: request.remember_subtitle_on_start,
            subtitle: SubtitleOverlayState::default(),
            volume,
            rate: rate::PlaybackRateState::default(),
            error_message,
        };
        if page.error_message.is_some() {
            let _ = page.close_playback_reporting(true, false);
        }
        page
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
        self.cancel_queue_switch();
        self.report_playback_progress(true);
        let update = self.close_playback_reporting(false, self.timeline.ended);
        self.clear_visible_frame(window, cx);
        cx.emit(PlaybackEvent::Back { update });
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
        if self.close_track_select(cx) {
            cx.stop_propagation();
            return;
        }
        self.window_drag = if event.click_count == 1 && !window.is_fullscreen() {
            WindowDragState::Pending
        } else {
            WindowDragState::Idle
        };
        // GPUI's Windows backend starts native moves from an unhandled
        // non-client press. Its start_window_move() implementation is a no-op.
        // Keep double clicks and menu dismissal in the player instead of
        // letting Windows maximize the window or start a move.
        if !cfg!(target_os = "windows") || self.window_drag != WindowDragState::Pending {
            cx.stop_propagation();
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
        if !event.dragging() {
            self.window_drag = WindowDragState::Idle;
        }
        if !cfg!(target_os = "windows")
            && self.window_drag == WindowDragState::Pending
            && !window.is_fullscreen()
            && event.dragging()
            && self.timeline.progress_drag_position.is_none()
        {
            // The compositor may consume the release after taking the pointer.
            self.window_drag = WindowDragState::Idle;
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

    fn render_mouse_capture(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let record_press = cx.listener(|page, in_playback: &bool, _, _| {
            page.window_drag = if *in_playback {
                WindowDragState::Blocked
            } else {
                WindowDragState::Idle
            };
        });
        let reset_on_release = cx.listener(|page, event: &MouseUpEvent, _, _| {
            if event.button == MouseButton::Left {
                page.window_drag = WindowDragState::Idle;
            }
        });
        let stop_control_drag = cx.listener(|page, _: &MouseMoveEvent, _, cx| {
            if page.window_drag == WindowDragState::Blocked {
                cx.stop_propagation();
            }
        });
        let drag_observer = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                // Observe presses and releases before controls handle or occlude
                // them. Only a subsequent surface press may arm a window move.
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase.capture() {
                        record_press(
                            &(event.button == MouseButton::Left
                                && bounds.contains(&event.position)),
                            window,
                            cx,
                        );
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase.capture() {
                        reset_on_release(event, window, cx);
                    }
                });
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    // A control press must not turn into the titlebar's own
                    // window drag either. Child controls handle drags first.
                    if phase.bubble() && event.dragging() && !bounds.contains(&event.position) {
                        stop_control_drag(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full();

        div()
            .id("playback-mouse-capture")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .when(
                cfg!(target_os = "windows") && !window.is_fullscreen(),
                |this| {
                    // Use the same native hit testing as the titlebar. Controls
                    // above this surface occlude it and keep their own gestures.
                    this.window_control_area(gpui::WindowControlArea::Drag)
                        // WM_NCRBUTTONUP would otherwise open the system menu
                        // after our right-button press toggles playback pause.
                        .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                },
            )
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
            .child(drag_observer)
    }
}

fn playback_protocol(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|url| url.scheme().trim().to_ascii_lowercase())
        .filter(|protocol| !protocol.is_empty())
}

fn playback_volume_percent(volume: f32) -> u32 {
    (clamp_playback_volume(volume) * 100.0).round() as u32
}

const PLAYBACK_VOLUME_STEP: f32 = 0.02;
// GPUI's Linux Wayland and X11 backends report three lines per wheel detent.
const SCROLL_LINES_PER_VOLUME_STEP: f32 = 3.0;

fn volume_delta_from_scroll_delta(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Lines(point) => point.y / SCROLL_LINES_PER_VOLUME_STEP * PLAYBACK_VOLUME_STEP,
        ScrollDelta::Pixels(point) => f32::from(point.y) / 25.0 * PLAYBACK_VOLUME_STEP,
    }
    .clamp(-0.2, 0.2)
}

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.poll_backend(window, cx);
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }

        let current_frame = self.frame.current.clone();
        let current_video_frame = current_frame
            .clone()
            .zip(self.frame.source_size)
            .map(|(frame, source_size)| VideoFrameElement { frame, source_size });
        let status = playback_status(
            &self.timeline,
            current_frame.is_some(),
            self.queue_switch.loading,
            self.error_message.as_ref(),
        );
        let progress_bar_visible = self.progress_bar_visible();
        if progress_bar_visible {
            self.update_download_speed(cx);
        }
        let theme = theme::get(cx);
        let is_fullscreen = window.is_fullscreen();
        if is_fullscreen && !self.fullscreen.cursor_visible {
            crate::hide_cursor_until_mouse_moves(cx);
        }
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
        .top_0()
        .right_0()
        .bottom_0()
        .left_0();
        let has_viewport = self
            .frame
            .viewport_bounds
            .is_some_and(|viewport_bounds| normalize_video_viewport(viewport_bounds).is_some());
        let video_presenter_needs_frame = self
            .video
            .dependent()
            .is_some_and(|presenter| presenter.snapshot().needs_animation_frame());
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
            video_presenter_needs_frame,
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
            .when(window_has_rounded_corners(window), |this| {
                this.rounded_b(theme.radius_lg).overflow_hidden()
            })
            .when_some(current_video_frame, |this, frame| this.child(frame))
            .when_some(status, |this, status| {
                this.child(render_playback_status(status, cx))
            })
            .child(viewport_observer)
            .child(self.render_mouse_capture(window, cx))
            .child(self.render_subtitle_overlay())
            .when(self.volume.indicator_visible, |this| {
                this.child(self.render_volume_indicator(cx))
            })
            .when(self.rate.indicator_visible, |this| {
                this.child(self.render_playback_rate_indicator(cx))
            })
            .child(self.render_queue_switch_error(cx))
            .when(self.episode_list.open, |this| {
                this.child(self.render_episode_list_backdrop(cx))
            })
            .when(progress_bar_visible, |this| {
                this.child(self.render_progress_bar(window, cx))
                    .child(self.render_download_speed(cx))
            })
            .when(
                fullscreen::playback_back_button_visible(
                    is_fullscreen,
                    self.fullscreen.controls_visible,
                ),
                |this| this.child(self.render_back_button(cx)),
            )
            .when(self.episode_list.open, |this| {
                this.child(deferred(self.render_episode_list(window, cx)).with_priority(2))
            })
            .when(self.playback_details_visible, |this| {
                // Keep stats above subtitles, controls, and deferred playback menus.
                this.child(deferred(self.render_playback_details_overlay(window)).with_priority(3))
            })
    }
}
