use std::{sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, DragMoveEvent, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, deferred, div, prelude::*, px, relative,
    rgb, rgba, svg,
};

use crate::{
    app::{window_corner_radii, window_uses_system_decorations},
    theme,
};

use super::presentation::{
    SubtitleImages, defer_drop_frame, defer_drop_released_images, render_image,
};
use tiny_playback::{
    BackendCommand, BackendLoadRequest, BackendSubtitleBitmap, BackendSubtitleCue,
    PlaybackAudioInfo, PlaybackCacheState, PlaybackFileInfo, PlaybackSeekMode, PlaybackTrack,
    PlaybackTrackKind, PlaybackVideoInfo, PlaybackVolumeSettings, RenderSize, StreamCacheKind,
    VideoPresenterSnapshot, clamp_playback_volume,
};

mod backend_events;
mod controls;
mod diagnostics;
mod episodes;
mod fullscreen;
mod presentation;
mod progress;
mod queue;
mod rate;
mod render;
mod report_delivery;
mod session;
mod shortcuts;
mod subtitles;
mod timers;
mod video_element;
mod video_viewport;

#[cfg(test)]
mod adapter_tests;
#[cfg(test)]
mod mouse_tests;

use super::{EmbyPlaybackContext, PlaybackRequest};
#[cfg(test)]
use super::{PlaybackQueue, PlaybackQueueItem, PlaybackTrackSelection};
pub use session::{PlaybackStateUpdate, PlaybackStopCompletion, PlaybackStopResult};

use super::backend::PlaybackBackendAdapter;
use super::model::source::{PlaybackSourceState, PlaybackTrackState, playback_protocol};
use super::model::timeline::PlaybackTimelineState;
use super::session::PlaybackIntent;
use presentation::{
    PlaybackPresentationState, PlaybackTimelinePresentation, SubtitleOverlayState, WindowDragState,
};
use progress::{
    ProgressBarDrag, clamp_playback_position, format_playback_time, progress_fraction_for_cursor,
    valid_playback_duration, valid_playback_time,
};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, normalize_video_viewport, playback_status,
    render_output_size, render_playback_status, should_render_frame,
    should_request_animation_frame, viewport_changed,
};
use subtitles::defer_drop_subtitle;
use timers::{PresentationEffects, PresentationTimer};
#[cfg(test)]
use tiny_playback::BackendEventKind;
use video_element::VideoFrameElement;
use video_viewport::VideoViewport;

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

impl PlaybackEvent {
    pub(crate) fn trace(&self) {
        let operation = match self {
            Self::VolumeChanged { .. } => "playback.volume_changed",
            Self::Update { .. } => "playback.update",
            Self::Back { .. } => "playback.back",
            Self::Replace { .. } => "playback.replace",
        };
        crate::observability::TraceId::start(operation).record("received");
    }
}

pub struct PlaybackPage {
    title: SharedString,
    video: PlaybackBackendAdapter,
    presentation: PlaybackPresentationState,
    session: super::session::PlaybackSessionController,
    // Page-owned continuation; the session owns poll eligibility and token.
    backend_poll: crate::effects::EffectHandle<gpui::Task<()>>,
    emby: EmbyPlaybackContext,
    report_effects: session::ReportingEffects,
    queue_effects: queue::QueueEffects,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

impl PlaybackPage {
    pub(crate) fn apply_playback_config(
        &mut self,
        config: tiny_playback::PlaybackCacheConfig,
    ) -> tiny_playback::Result<()> {
        if let Some(result) = self.video.command(BackendCommand::SetCacheConfig(config)) {
            result?;
        }
        Ok(())
    }

    pub fn new(request: PlaybackRequest, cx: &mut Context<Self>) -> Self {
        Self::new_with_cache_config(request, Default::default(), cx)
    }

    pub fn new_with_cache_config(
        request: PlaybackRequest,
        cache_config: tiny_playback::PlaybackCacheConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_settings(request, cache_config, PlaybackVolumeSettings::default(), cx)
    }

    pub(crate) fn new_with_settings(
        request: PlaybackRequest,
        cache_config: tiny_playback::PlaybackCacheConfig,
        volume_settings: PlaybackVolumeSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::register_image_cleanup(cx);
        let volume = volume_settings.normalized();
        let source_protocol = playback_protocol(&request.url);
        let content_length = request.content_length;
        let report_effects = session::ReportingEffects::new(&request.emby, cx);
        let queue_effects = queue::QueueEffects::new(&request.emby);

        let (video, error_message) = PlaybackBackendAdapter::start(
            BackendLoadRequest {
                url: request.url.clone(),
                http_headers: request.http_headers.clone(),
                content_length: request.content_length,
                start_position_seconds: request.initial_position_seconds,
                selected_tracks: request.selected_tracks.clone(),
                cache_config,
            },
            volume.level,
        );

        let timeline = PlaybackTimelineState {
            position: valid_playback_time(request.initial_position_seconds),
            ..PlaybackTimelineState::default()
        };

        let mut page = Self {
            title: request.title,
            video,
            presentation: PlaybackPresentationState::new(
                cx.focus_handle(),
                request.emby.server.workspace_identity(),
                episodes::PlaybackEpisodeListState::new(&request.emby),
            ),
            session: super::session::PlaybackSessionController::new(
                timeline,
                PlaybackSourceState {
                    source_protocol,
                    source_url: request.url,
                    content_length,
                    playback_file_info: None,
                    playback_info: None,
                    playback_audio_info: None,
                    tracks: PlaybackTrackState::new(
                        request.audio_tracks,
                        request.subtitle_tracks,
                        request.selected_tracks,
                    ),
                    track_preference_key: request.track_preference_key,
                    remember_subtitle_on_start: request.remember_subtitle_on_start,
                },
                request.queue,
                request.emby.server.workspace_identity(),
                volume,
                error_message,
            ),
            backend_poll: Default::default(),
            emby: request.emby,
            report_effects,
            queue_effects,
        };
        if page.session.controls_view().error.is_some() {
            let _ = page.close_playback_reporting(true, false);
        }
        page
    }

    fn register_image_cleanup(cx: &Context<Self>) {
        cx.on_release(|page, cx| {
            let images = page.presentation.release_images();
            defer_drop_released_images(images, cx);
        })
        .detach();
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn back_to_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presentation.presentation_timers.close();
        self.session.cancel_poll();
        self.backend_poll.cancel();
        self.cancel_queue_switch();
        self.report_playback_progress(true);
        let update = self.close_playback_reporting(false, self.session.timeline().ended);
        defer_drop_subtitle(&mut self.presentation.subtitle, window);
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
        self.presentation.window_drag = if event.click_count == 1
            && !window.is_fullscreen()
            && !window_uses_system_decorations(window)
        {
            WindowDragState::Pending
        } else {
            WindowDragState::Idle
        };
        // GPUI's Windows backend starts native moves from an unhandled
        // non-client press. Its start_window_move() implementation is a no-op.
        // Keep double clicks and menu dismissal in the player instead of
        // letting Windows maximize the window or start a move.
        if !cfg!(target_os = "windows") || self.presentation.window_drag != WindowDragState::Pending
        {
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
        if !event.dragging()
            || (window_uses_system_decorations(window)
                && self.presentation.window_drag == WindowDragState::Pending)
        {
            self.presentation.window_drag = WindowDragState::Idle;
        }
        if !cfg!(target_os = "windows")
            && self.presentation.window_drag == WindowDragState::Pending
            && !window.is_fullscreen()
            && event.dragging()
            && self.session.timeline().progress_drag_position.is_none()
        {
            // The compositor may consume the release after taking the pointer.
            self.presentation.window_drag = WindowDragState::Idle;
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
            .presentation
            .frame
            .current
            .as_ref()
            .is_some_and(|current| current.id == frame.id)
        {
            self.presentation.frame.current = Some(frame);
            return;
        }

        let previous = self.presentation.frame.current.replace(frame);
        if let Some(previous) = previous {
            defer_drop_frame(previous, window);
        }
    }

    fn clear_visible_frame(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(frame) = self.presentation.frame.current.take() {
            defer_drop_frame(frame, window);
        }
    }

    fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.presentation.frame.viewport_bounds, bounds) {
            return;
        }

        self.presentation.frame.viewport_bounds = Some(bounds);
        cx.notify();
    }

    fn update_progress_track_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(
            self.presentation
                .timeline_presentation
                .progress_track_bounds,
            bounds,
        ) {
            return;
        }

        self.presentation
            .timeline_presentation
            .progress_track_bounds = Some(bounds);
        cx.notify();
    }

    fn render_mouse_capture(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let record_press = cx.listener(|page, in_playback: &bool, _, _| {
            page.presentation.window_drag = if *in_playback {
                WindowDragState::Blocked
            } else {
                WindowDragState::Idle
            };
        });
        let reset_on_release = cx.listener(|page, event: &MouseUpEvent, _, _| {
            if event.button == MouseButton::Left {
                page.presentation.window_drag = WindowDragState::Idle;
            }
        });
        let stop_control_drag = cx.listener(|page, _: &MouseMoveEvent, _, cx| {
            if page.presentation.window_drag == WindowDragState::Blocked {
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
        if !self.presentation.focus_handle.is_focused(window) {
            window.focus(&self.presentation.focus_handle, cx);
        }

        let current_frame = self.presentation.frame.current.clone();
        let current_video_frame = current_frame
            .clone()
            .zip(self.presentation.frame.source_size)
            .map(|(frame, source_size)| VideoFrameElement { frame, source_size });
        let status = playback_status(
            self.session.timeline(),
            current_frame.is_some(),
            self.session.queue.view_model().loading,
            self.session.controls_view().error,
        );
        let progress_bar_visible = self.progress_bar_visible();
        if progress_bar_visible {
            self.update_download_speed(cx);
        }
        let corners = window_corner_radii(window, cx);
        let is_fullscreen = window.is_fullscreen();
        if is_fullscreen && !self.presentation.fullscreen.cursor_visible {
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
            .presentation
            .frame
            .viewport_bounds
            .is_some_and(|viewport_bounds| normalize_video_viewport(viewport_bounds).is_some());
        let video_presenter_needs_frame = self
            .video
            .presenter_snapshot()
            .is_some_and(|snapshot| snapshot.needs_animation_frame());
        if should_request_animation_frame(AnimationFrameRequestState {
            has_backend: self.video.has_backend(),
            has_video_presenter: self.video.has_presenter(),
            has_loaded_file: self.session.timeline().loaded,
            playback_ended: self.session.timeline().ended,
            has_error: self.session.controls_view().error.is_some(),
            has_viewport,
            has_visible_frame: current_frame.is_some(),
            playback_paused: self.session.timeline().paused,
            playback_buffering: self.session.timeline().buffering,
            pending_seek: self.session.timeline().pending_seek_position.is_some(),
            video_presenter_needs_frame,
        }) {
            window.request_animation_frame();
        }

        div()
            .key_context("PlaybackPage")
            .track_focus(&self.presentation.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .text_color(rgb(0xe6edf3))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .on_scroll_wheel(cx.listener(Self::handle_surface_scroll_wheel))
            .child(VideoViewport::new(
                self.presentation.frame.source_size,
                corners,
                div()
                    .when_some(current_video_frame, |this, frame| this.child(frame))
                    .child(viewport_observer)
                    .child(self.render_subtitle_overlay()),
            ))
            .when_some(status, |this, status| {
                this.child(render_playback_status(status, cx))
            })
            .child(self.render_mouse_capture(window, cx))
            .when(self.presentation.volume_indicator_visible, |this| {
                this.child(controls::volume_indicator(
                    self.session.controls_view().volume.level,
                    cx,
                ))
            })
            .when(self.presentation.rate_indicator_visible, |this| {
                this.child(self.render_playback_rate_indicator(cx))
            })
            .child(self.render_queue_switch_error(cx))
            .when(self.presentation.episode_list.open, |this| {
                this.child(self.render_episode_list_backdrop(cx))
            })
            .when(progress_bar_visible, |this| {
                this.child(self.render_progress_bar(window, cx))
                    .child(self.render_download_speed(cx))
            })
            .when(
                fullscreen::playback_back_button_visible(
                    is_fullscreen,
                    self.presentation.fullscreen.controls_visible,
                ),
                |this| this.child(self.render_back_button(cx)),
            )
            .when(self.presentation.episode_list.open, |this| {
                this.child(deferred(self.render_episode_list(window, cx)).with_priority(2))
            })
            .when(self.presentation.playback_details_visible, |this| {
                // Keep stats above subtitles, controls, and deferred playback menus.
                this.child(deferred(self.render_playback_details_overlay(window)).with_priority(3))
            })
    }
}
