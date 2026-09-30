use super::*;
use crate::player::model::track_menu::{TrackMenuIntent, track_menu};
use crate::ui::radius;

impl PlaybackPage {
    pub(in super::super) fn render_playback_details_overlay(
        &self,
        window: &Window,
    ) -> impl IntoElement {
        let presenter_snapshot = self.video.presenter_snapshot();
        let viewport_size = window.viewport_size();
        let display_size = RenderSize {
            width: (f32::from(viewport_size.width) * window.scale_factor())
                .round()
                .max(0.0) as u32,
            height: (f32::from(viewport_size.height) * window.scale_factor())
                .round()
                .max(0.0) as u32,
        };
        playback_details_overlay(
            [
                Some(playback_file_detail_section(
                    &self.session.source_view().source_url,
                    self.title.as_ref(),
                    self.session.source_view().content_length,
                    self.session.source_view().playback_file_info.as_ref(),
                    self.session.timeline().cache_state.as_ref(),
                )),
                presenter_snapshot
                    .map(|presenter| playback_display_detail_section(display_size, presenter)),
                playback_video_detail_section(self.session.source_view().playback_info.as_ref()),
                playback_audio_detail_section(
                    self.session.source_view().playback_audio_info.as_ref(),
                    self.session.controls_view().volume.level,
                ),
            ],
            window,
        )
    }

    fn render_track_select_menu(
        &self,
        kind: PlaybackTrackKind,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let view = self.session.controls_view();
        let choice = match kind {
            PlaybackTrackKind::Audio => view.audio,
            PlaybackTrackKind::Subtitle => view.subtitles,
        };
        components::track_select_menu(
            track_menu(kind, choice.tracks, choice.selected),
            cx,
            cx.listener(|page, intent: &TrackMenuIntent, window, cx| match intent {
                TrackMenuIntent::Audio(index) => page.select_audio_track(*index, window, cx),
                TrackMenuIntent::Subtitle(track) => {
                    page.select_subtitle_track(track.clone(), window, cx)
                }
            }),
        )
    }

    pub(in super::super) fn render_back_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::media_overlay(cx);

        div()
            .id("playback-back-button")
            .cursor_pointer()
            .absolute()
            .left(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .top(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .flex()
            .size(px(PLAYBACK_BACK_BUTTON_SIZE_PX))
            .items_center()
            .justify_center()
            .rounded(radius::CONTROL)
            .hover(move |style| style.bg(theme.secondary_hover))
            .occlude()
            .on_hover(cx.listener(Self::handle_back_button_hover))
            .on_mouse_move(cx.listener(Self::handle_back_button_mouse_move))
            .child(
                svg()
                    .path("icons/chevron-left.svg")
                    .size(px(18.0))
                    .text_color(theme.foreground),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::press_back_button))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            .on_mouse_up(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_mouse_up(MouseButton::Right, |_, _, cx| {
                cx.stop_propagation();
            })
    }

    fn render_playback_controls_row(
        &self,
        state: PlaybackControlsRenderState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("playback-controls")
            .relative()
            .w_full()
            .h(px(34.0))
            .child(self.render_primary_transport_controls(
                state.can_switch_previous,
                state.can_toggle_playback,
                state.play_pause_icon,
                state.can_switch_next,
                cx,
            ))
            .child(self.render_track_control_buttons(state, cx))
    }

    pub(in super::super) fn render_primary_transport_controls(
        &self,
        can_switch_previous: bool,
        can_toggle_playback: bool,
        play_pause_icon: &'static str,
        can_switch_next: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .absolute()
            .left_0()
            .right_0()
            .top_0()
            .flex()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                components::playback_control_button(
                    "playback-previous-button",
                    "icons/previous.svg",
                    px(30.0),
                    px(16.0),
                    can_switch_previous,
                    cx,
                )
                .when(can_switch_previous, |this| {
                    this.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(Self::switch_to_previous_episode),
                    )
                }),
            )
            .child(
                components::playback_control_button(
                    "playback-play-pause-button",
                    play_pause_icon,
                    px(34.0),
                    px(18.0),
                    can_toggle_playback,
                    cx,
                )
                .when(can_toggle_playback, |this| {
                    this.on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_playback_pause))
                }),
            )
            .child(
                components::playback_control_button(
                    "playback-next-button",
                    "icons/next.svg",
                    px(30.0),
                    px(16.0),
                    can_switch_next,
                    cx,
                )
                .when(can_switch_next, |this| {
                    this.on_mouse_down(MouseButton::Left, cx.listener(Self::switch_to_next_episode))
                }),
            )
    }

    fn render_track_control_buttons(
        &self,
        state: PlaybackControlsRenderState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .absolute()
            .right_0()
            .top_0()
            .flex()
            .items_center()
            .gap_2()
            .when(playback_diagnostics_enabled(), |this| {
                this.child(self.render_cache_status_button(
                    state.cache_status_enabled,
                    state.cache_status_open,
                    cx,
                ))
            })
            .child(self.render_episode_list_button(cx))
            .child(self.render_track_control_button(
                PlaybackTrackKind::Audio,
                "playback-audio-button",
                "icons/audio.svg",
                state.can_select_audio,
                state.audio_select_open,
                cx,
            ))
            .child(self.render_track_control_button(
                PlaybackTrackKind::Subtitle,
                "playback-caption-button",
                "icons/caption.svg",
                state.can_select_subtitle,
                state.subtitle_select_open,
                cx,
            ))
    }

    pub(in super::super) fn render_cache_status_button(
        &self,
        enabled: bool,
        status_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let button = components::playback_control_button(
            "playback-cache-status-button",
            "icons/activity.svg",
            px(30.0),
            px(16.0),
            enabled,
            cx,
        )
        .when(enabled, |this| {
            this.on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_cache_status))
        });

        div().relative().child(button).when(status_open, |this| {
            this.child(
                deferred(components::cache_status_popover(
                    cache_status_segments(self.session.timeline().cache_state.as_ref()),
                    cx,
                ))
                .with_priority(1),
            )
        })
    }

    pub(in super::super) fn render_track_control_button(
        &self,
        kind: PlaybackTrackKind,
        id: &'static str,
        icon_path: &'static str,
        enabled: bool,
        select_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let button =
            components::playback_control_button(id, icon_path, px(30.0), px(16.0), enabled, cx);
        let button = match kind {
            PlaybackTrackKind::Audio => button.when(enabled, |this| {
                this.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(Self::toggle_audio_track_select),
                )
            }),
            PlaybackTrackKind::Subtitle => button.when(enabled, |this| {
                this.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(Self::toggle_subtitle_track_select),
                )
            }),
        };

        div().relative().child(button).when(select_open, |this| {
            this.child(deferred(self.render_track_select_menu(kind, cx)).with_priority(1))
        })
    }

    fn render_progress_timeline(
        &self,
        state: ProgressTimelineViewModel,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let played_color = if state.cached_seek_preview == Some(false) {
            theme.warning
        } else {
            theme.input_border_focused
        };
        let forward_cache_fill = progress_track_forward_cache_fill(
            theme.input_border_focused.opacity(0.35),
            state.forward_cache_fraction,
        );

        let track = div()
            .id("playback-progress-track")
            .debug_selector(|| "playback-progress-track".to_string())
            .relative()
            .flex_1()
            .h(px(28.0))
            .cursor_pointer()
            .on_mouse_move(cx.listener(|page, event: &MouseMoveEvent, _, cx| {
                page.update_progress_hover(Some(event.position), cx);
            }))
            .on_hover(cx.listener(|page, hovered: &bool, window, cx| {
                page.update_progress_hover(hovered.then(|| window.mouse_position()), cx);
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_progress_drag))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_progress_drag))
            .on_drag(ProgressBarDrag, |_, _, window, cx| {
                cx.stop_propagation();
                // Keep the grabbing cursor even when the pointer leaves the track.
                window.defer(cx, |window, cx| {
                    cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
                });
                cx.new(|_| ProgressBarDrag)
            })
            .on_drag_move(cx.listener(Self::drag_progress))
            .child(progress_track_fill(theme.input_border.opacity(0.48), 1.0))
            .children(forward_cache_fill)
            .when(playback_diagnostics_enabled(), |this| {
                this.children(state.cache_ranges.into_iter().map(
                    |(start_fraction, end_fraction)| {
                        progress_track_seekable_range_fill(
                            theme.muted_foreground.opacity(0.52),
                            start_fraction,
                            end_fraction,
                        )
                    },
                ))
            });
        let track = track
            .child(progress_track_played_fill(
                played_color,
                state.played_fraction,
            ))
            .child(progress_track_observer(cx))
            .when_some(
                progress::progress_hover_preview(
                    self.session.timeline(),
                    &self.presentation.timeline_presentation,
                ),
                |track, preview| track.child(progress::render_progress_hover(preview, cx)),
            );

        div()
            .flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(48.0))
                    .text_align(gpui::TextAlign::Left)
                    .child(state.current_time),
            )
            .child(track)
            .child(
                div()
                    .w(px(48.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(state.duration_time),
            )
    }

    pub(in super::super) fn render_progress_bar(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let view = self.session.controls_view();
        let Some(progress) = view.progress() else {
            return div().id("playback-progress-empty").into_any_element();
        };
        let theme = theme::media_overlay(cx);
        let bounds =
            fullscreen::playback_progress_bar_bounds(fullscreen::window_viewport_bounds(window));
        let controls = PlaybackControlsRenderState {
            can_switch_previous: view.can_previous,
            can_toggle_playback: view.can_toggle,
            can_switch_next: view.can_next,
            play_pause_icon: play_pause_icon_for_user_pause(view.timeline.user_paused),
            cache_status_enabled: view.timeline.cache_state.is_some(),
            cache_status_open: self.presentation.timeline_presentation.cache_status_open,
            can_select_audio: view.audio.enabled(),
            can_select_subtitle: view.subtitles.enabled(),
            audio_select_open: self.presentation.track_select_open
                == Some(PlaybackTrackKind::Audio),
            subtitle_select_open: self.presentation.track_select_open
                == Some(PlaybackTrackKind::Subtitle),
        };

        div()
            .id("playback-progress")
            .cursor_default()
            .debug_selector(|| "playback-progress".into())
            .absolute()
            .left(bounds.left())
            .bottom(px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX))
            .flex()
            .flex_col()
            .w(bounds.size.width)
            .h(px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX))
            .justify_center()
            .gap_2()
            .rounded(radius::SURFACE)
            .border_1()
            .border_color(theme.input_border.opacity(0.42))
            .bg(rgba(0x00000099))
            .px_4()
            // Reserve space inside the panel for the time below the progress track.
            .pb(px(8.0))
            .shadow_lg()
            // Blank space and labels dismiss popups without passing input through
            // to the video. Buttons and the progress track handle their own input.
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            // Hover still keeps the controls and cursor visible.
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .text_xs()
            .text_color(theme.foreground.opacity(0.86))
            .child(self.render_playback_controls_row(controls, cx))
            .child(self.render_progress_timeline(progress, cx))
            .into_any_element()
    }
}
