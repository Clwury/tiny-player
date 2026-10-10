//! Compose playback presentation without owning session policy.
use super::*;

impl Render for PlaybackPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start_gamepad_input(window, cx);
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
