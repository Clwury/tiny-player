use super::*;

impl PlaybackPage {
    pub(in super::super) fn playback_control_button(
        id: &'static str,
        icon_path: &'static str,
        button_size: Pixels,
        icon_size: Pixels,
        enabled: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme::media_overlay(cx);
        let color = if enabled {
            theme.foreground.opacity(0.92)
        } else {
            theme.foreground.opacity(0.52)
        };

        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex()
            .size(button_size)
            .items_center()
            .justify_center()
            .rounded_full()
            .text_color(color)
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(theme.foreground.opacity(0.14)))
            })
            .when(!enabled, |this| this.cursor_default().opacity(0.62))
            .child(svg().path(icon_path).size(icon_size).text_color(color))
    }

    pub(in super::super) fn render_playback_details_overlay(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let presenter_snapshot = self.video.dependent().map(VideoPresenter::snapshot);
        let viewport_size = window.viewport_size();
        let display_size = RenderSize {
            width: f32::from(viewport_size.width).round().max(0.0) as u32,
            height: f32::from(viewport_size.height).round().max(0.0) as u32,
        };
        let output_size = self
            .frame
            .viewport_bounds
            .zip(self.frame.source_size)
            .and_then(|(viewport, source)| render_output_size(viewport, source));
        let sections = vec![
            playback_file_detail_section(
                self.title.as_ref(),
                self.content_length,
                self.source_protocol.as_deref(),
                self.playback_file_info.as_ref(),
                self.timeline.duration,
                self.timeline.cache_state.as_ref(),
            ),
            playback_display_detail_section(display_size, output_size, presenter_snapshot),
            playback_video_detail_section(self.playback_info.as_ref(), self.timeline.loaded),
            playback_audio_detail_section(
                self.playback_audio_info.as_ref(),
                self.timeline.loaded,
                self.volume.level,
            ),
        ];

        sections.into_iter().fold(
            div()
                .id("playback-details-overlay")
                .absolute()
                .left_4()
                .top(px(if window.is_fullscreen() {
                    16.0
                } else {
                    PLAYBACK_DETAILS_TOP_PX
                }))
                .flex()
                .flex_col()
                .w(px(PLAYBACK_DETAILS_WIDTH_PX))
                .max_w(relative(0.82))
                .max_h(relative(if window.is_fullscreen() { 0.92 } else { 0.82 }))
                .gap_3()
                .overflow_y_scroll()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.input_border.opacity(0.42))
                .bg(rgba(0x000000c8))
                .px_3()
                .py_3()
                .shadow_lg()
                .occlude()
                .text_xs()
                .text_color(theme.foreground.opacity(0.92))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_down(MouseButton::Right, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_scroll_wheel(|_, _, cx| {
                    cx.stop_propagation();
                }),
            |this, section| this.child(playback_detail_section_element(section, cx)),
        )
    }

    pub(in super::super) fn render_track_select_menu(
        &self,
        kind: PlaybackTrackKind,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let (id, tracks, selected) = match kind {
            PlaybackTrackKind::Audio => (
                "playback-audio-menu",
                &self.tracks.audio,
                self.tracks.selected_audio_stream_index,
            ),
            PlaybackTrackKind::Subtitle => (
                "playback-caption-menu",
                &self.tracks.subtitles,
                self.tracks.selected_subtitle_stream_index,
            ),
        };
        let off_selected = selected.is_none();
        let off_mouse_down = cx.listener(move |page: &mut PlaybackPage, _, window, cx| {
            cx.stop_propagation();
            match kind {
                PlaybackTrackKind::Audio => page.select_audio_track(None, window, cx),
                PlaybackTrackKind::Subtitle => page.select_subtitle_track(None, window, cx),
            }
        });

        tracks
            .iter()
            .enumerate()
            .fold(
                div()
                    .id(id)
                    .absolute()
                    .right_0()
                    .bottom(px(32.0))
                    .flex()
                    .flex_col()
                    .min_w(px(190.0))
                    .max_w(px(280.0))
                    .max_h(px(TRACK_SELECT_MENU_MAX_HEIGHT_PX))
                    .gap_1()
                    .overflow_y_scroll()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.72))
                    .bg(rgba(0x000000e6))
                    .p(px(4.0))
                    .shadow_lg()
                    .occlude()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        track_select_option("Off", off_selected, cx)
                            .id("playback-track-off-option")
                            .on_mouse_down(MouseButton::Left, off_mouse_down),
                    ),
                |this, (index, track)| {
                    let track = track.clone();
                    let stream_index = track.stream_index;
                    let selected = selected == Some(stream_index);
                    let label = if track.is_external {
                        format!("{} 外挂", track.label)
                    } else {
                        track.label.to_string()
                    };
                    let on_mouse_down =
                        cx.listener(move |page: &mut PlaybackPage, _, window, cx| {
                            cx.stop_propagation();
                            match kind {
                                PlaybackTrackKind::Audio => {
                                    page.select_audio_track(Some(stream_index), window, cx)
                                }
                                PlaybackTrackKind::Subtitle => {
                                    page.select_subtitle_track(Some(track.clone()), window, cx)
                                }
                            }
                        });
                    this.child(
                        track_select_option(label, selected, cx)
                            .id((
                                gpui::ElementId::from("playback-track-option"),
                                index.to_string(),
                            ))
                            .on_mouse_down(MouseButton::Left, on_mouse_down),
                    )
                },
            )
            .into_any_element()
    }

    pub(in super::super) fn render_back_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::media_overlay(cx);

        div()
            .id("playback-back-button")
            .absolute()
            .left(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .top(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .flex()
            .size(px(PLAYBACK_BACK_BUTTON_SIZE_PX))
            .items_center()
            .justify_center()
            .rounded_md()
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

    pub(in super::super) fn render_volume_indicator(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let volume = clamp_playback_volume(self.volume.level);
        let fill_height = VOLUME_INDICATOR_BAR_HEIGHT_PX * volume;
        let percent = playback_volume_percent(volume);

        div()
            .id("playback-volume-indicator")
            .absolute()
            .right(px(24.0))
            .top(relative(0.5))
            .mt(-px(106.0))
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .child(
                div()
                    .relative()
                    .w(px(8.0))
                    .h(px(VOLUME_INDICATOR_BAR_HEIGHT_PX))
                    .overflow_hidden()
                    .rounded_full()
                    .bg(theme.foreground.opacity(0.24))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(fill_height))
                            .rounded_full()
                            .bg(theme.input_border_focused),
                    ),
            )
            .child(
                div()
                    .w(px(42.0))
                    .text_align(gpui::TextAlign::Center)
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(format!("{percent}%")),
            )
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
                Self::playback_control_button(
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
                Self::playback_control_button(
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
                Self::playback_control_button(
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
        let button = Self::playback_control_button(
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
            this.child(deferred(self.render_cache_status_popover(cx)).with_priority(1))
        })
    }

    pub(in super::super) fn render_cache_status_popover(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let segments = cache_status_segments(self.timeline.cache_state.as_ref());
        segments.into_iter().fold(
            div()
                .id("playback-cache-status-popover")
                .absolute()
                .right_0()
                .bottom(px(32.0))
                .flex()
                .flex_col()
                .min_w(px(176.0))
                .gap_1()
                .rounded(px(8.0))
                .border_1()
                .border_color(theme.input_border.opacity(0.62))
                .bg(rgba(0x000000dd))
                .p_2()
                .shadow_lg()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                }),
            |this, segment| {
                this.child(
                    div()
                        .h(px(22.0))
                        .min_h(px(22.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .rounded(px(4.0))
                        .bg(theme.foreground.opacity(0.08))
                        .px_2()
                        .text_xs()
                        .text_color(theme.foreground.opacity(0.9))
                        .child(segment),
                )
            },
        )
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
        let button = Self::playback_control_button(id, icon_path, px(30.0), px(16.0), enabled, cx);
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
        state: ProgressTimelineRenderState,
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
            .cursor_default()
            .on_mouse_move(cx.listener(|page, event: &MouseMoveEvent, _, cx| {
                page.update_progress_hover(Some(event.position), cx);
            }))
            .on_hover(cx.listener(|page, hovered: &bool, window, cx| {
                page.update_progress_hover(hovered.then(|| window.mouse_position()), cx);
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_progress_drag))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_progress_drag))
            .on_drag(ProgressBarDrag, |_, _, _, cx| {
                cx.stop_propagation();
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
                progress::render_progress_hover(&self.timeline, cx),
                |track, preview| track.child(preview),
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
        let Some(duration) = self.timeline.duration else {
            return div().id("playback-progress-empty").into_any_element();
        };

        let theme = theme::media_overlay(cx);
        let bounds =
            fullscreen::playback_progress_bar_bounds(fullscreen::window_viewport_bounds(window));
        let position = self
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let played_fraction = progress_fraction(position, duration);
        let cached_seek_preview = self.timeline.progress_drag_position.map(|target| {
            cached_seek_target(
                self.timeline.cache_state.as_ref(),
                self.timeline.buffered_until,
                self.timeline.position,
                target,
            )
        });
        let cache_ranges = cache_range_fractions(self.timeline.cache_state.as_ref(), duration);
        let current_time = format_playback_time(position);
        let duration_time = format_playback_time(duration);
        let can_toggle_playback = self.can_toggle_playback();
        let play_pause_icon = play_pause_icon_for_user_pause(self.timeline.user_paused);
        let cache_status_enabled = self.timeline.cache_state.is_some();
        let can_select_audio =
            !self.tracks.audio.is_empty() || self.tracks.selected_audio_stream_index.is_some();
        let can_select_subtitle = !self.tracks.subtitles.is_empty()
            || self.tracks.selected_subtitle_stream_index.is_some();
        let controls = PlaybackControlsRenderState {
            can_switch_previous: self.can_switch_to_previous_episode(),
            can_toggle_playback,
            can_switch_next: self.can_switch_to_next_episode(),
            play_pause_icon,
            cache_status_enabled,
            cache_status_open: self.timeline.cache_status_open,
            can_select_audio,
            can_select_subtitle,
            audio_select_open: self.tracks.open == Some(PlaybackTrackKind::Audio),
            subtitle_select_open: self.tracks.open == Some(PlaybackTrackKind::Subtitle),
        };

        div()
            .id("playback-progress")
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
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.input_border.opacity(0.42))
            .bg(rgba(0x00000099))
            .px_4()
            // Reserve space inside the panel for the time below the progress track.
            .pb(px(8.0))
            .shadow_lg()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::close_track_select_on_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .text_xs()
            .text_color(theme.foreground.opacity(0.86))
            .child(self.render_playback_controls_row(controls, cx))
            .child(self.render_progress_timeline(
                ProgressTimelineRenderState {
                    current_time,
                    duration_time,
                    played_fraction,
                    cached_seek_preview,
                    forward_cache_fraction: forward_cache_fraction(&self.timeline),
                    cache_ranges,
                },
                cx,
            ))
            .into_any_element()
    }
}
