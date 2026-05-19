use super::fullscreen::{PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX, PLAYBACK_PROGRESS_BAR_HEIGHT_PX};
use super::*;

impl PlaybackPage {
    fn toggle_playback_pause(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.toggle_playback_pause_command(cx);
    }

    pub(super) fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
        if !self.can_toggle_playback() {
            return;
        }

        let paused = !self.playback_paused;
        let Some(backend) = self.video.owner_mut() else {
            return;
        };
        let command = if paused {
            BackendCommand::Pause
        } else {
            BackendCommand::Resume
        };
        if let Err(error) = backend.command(command) {
            self.playback_paused = true;
            self.playback_buffering = false;
            self.error_message = Some(format!("控制播放失败：{error}").into());
        } else {
            self.playback_paused = paused;
            if paused {
                self.playback_buffering = false;
            }
        }
        cx.notify();
    }

    pub(super) fn toggle_audio_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.audio_tracks.is_empty() && self.selected_audio_stream_index.is_none() {
            return;
        }
        self.open_track_select = if self.open_track_select == Some(PlaybackTrackKind::Audio) {
            None
        } else {
            Some(PlaybackTrackKind::Audio)
        };
        cx.notify();
    }

    pub(super) fn toggle_subtitle_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.subtitle_tracks.is_empty() && self.selected_subtitle_stream_index.is_none() {
            return;
        }
        self.open_track_select = if self.open_track_select == Some(PlaybackTrackKind::Subtitle) {
            None
        } else {
            Some(PlaybackTrackKind::Subtitle)
        };
        cx.notify();
    }

    pub(super) fn select_audio_track(
        &mut self,
        track_index: Option<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let previous_audio = self.selected_audio_stream_index;
        self.selected_audio_stream_index = track_index;
        self.open_track_select = None;
        self.playback_buffering = self.current_file_loaded;
        self.status_message = "正在切换轨道…".into();

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            })
        {
            self.selected_audio_stream_index = previous_audio;
            self.playback_buffering = false;
            self.error_message = Some(format!("切换轨道失败：{error}").into());
        }
        cx.notify();
    }

    pub(super) fn select_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let previous_audio = self.selected_audio_stream_index;
        let previous_subtitle = self.selected_subtitle_stream_index;
        let mut previous_active_subtitle = self.active_subtitle.take();
        self.selected_subtitle_stream_index = track.as_ref().map(|track| track.stream_index);
        self.open_track_select = None;
        self.playback_buffering = self.current_file_loaded;
        self.status_message = "正在切换轨道…".into();

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            })
        {
            self.selected_audio_stream_index = previous_audio;
            self.selected_subtitle_stream_index = previous_subtitle;
            self.active_subtitle = previous_active_subtitle.take();
            self.playback_buffering = false;
            self.error_message = Some(format!("切换轨道失败：{error}").into());
        }
        defer_drop_subtitle(previous_active_subtitle, window);
        cx.notify();
    }

    pub(super) fn begin_progress_drag(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_drag(event.position.x, cx);
        cx.stop_propagation();
    }

    pub(super) fn drag_progress(
        &mut self,
        event: &DragMoveEvent<ProgressBarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_drag(event.event.position.x, cx);
        cx.stop_propagation();
    }

    pub(super) fn finish_progress_drag(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_progress_drag(window, cx);
        cx.stop_propagation();
    }

    pub(super) fn update_progress_drag(&mut self, cursor_x: Pixels, cx: &mut Context<Self>) {
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

    pub(super) fn commit_progress_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.progress_drag_position else {
            return;
        };
        self.seek_to_position(position, window, cx);
    }

    pub(super) fn seek_relative(
        &mut self,
        delta_seconds: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_seek_playback() {
            return;
        }

        let position = self
            .progress_drag_position
            .or(self.pending_seek_position)
            .or(self.playback_position)
            .unwrap_or(0.0)
            + delta_seconds;
        self.seek_to_position(position, window, cx);
    }

    pub(super) fn seek_to_position(
        &mut self,
        position: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self
            .playback_duration
            .map(|duration| clamp_playback_position(position, duration))
            .unwrap_or(position);
        self.progress_drag_position = None;
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
            && let Err(error) = backend.command(BackendCommand::Seek {
                position_seconds: position,
            })
        {
            self.pending_seek_position = None;
            self.pending_seek_keeps_frame = false;
            self.playback_buffering = false;
            self.error_message = Some(format!("跳转播放位置失败：{error}").into());
        }
        cx.notify();
    }

    pub(super) fn position_for_progress_cursor(&self, cursor_x: Pixels) -> Option<f64> {
        let duration = self.playback_duration?;
        let bounds = self.progress_track_bounds?;
        let fraction = progress_fraction_for_cursor(cursor_x, bounds)?;
        Some(clamp_playback_position(
            duration * fraction as f64,
            duration,
        ))
    }

    pub(super) fn playback_control_button(
        id: &'static str,
        icon_path: &'static str,
        button_size: Pixels,
        icon_size: Pixels,
        enabled: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = theme::get(cx);
        let color = if enabled {
            theme.foreground.opacity(0.92)
        } else {
            theme.foreground.opacity(0.52)
        };

        div()
            .id(id)
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

    pub(super) fn render_playback_info_overlay(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(info) = self.playback_info.as_ref() else {
            return div().id("playback-info-overlay-empty").into_any_element();
        };

        let theme = theme::get(cx);
        playback_info_segments(info)
            .into_iter()
            .fold(
                div()
                    .id("playback-info-overlay")
                    .absolute()
                    .right_4()
                    .top_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.42))
                    .bg(rgba(0x000000a8))
                    .px_3()
                    .py_2()
                    .shadow_lg()
                    .occlude()
                    .text_xs()
                    .text_color(theme.foreground.opacity(0.9)),
                |this, segment| {
                    this.child(
                        div()
                            .h(px(18.0))
                            .flex()
                            .items_center()
                            .rounded(px(4.0))
                            .bg(theme.foreground.opacity(0.08))
                            .px_2()
                            .child(segment),
                    )
                },
            )
            .into_any_element()
    }

    pub(super) fn render_track_select_menu(
        &self,
        kind: PlaybackTrackKind,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let (id, tracks, selected) = match kind {
            PlaybackTrackKind::Audio => (
                "playback-audio-menu",
                &self.audio_tracks,
                self.selected_audio_stream_index,
            ),
            PlaybackTrackKind::Subtitle => (
                "playback-caption-menu",
                &self.subtitle_tracks,
                self.selected_subtitle_stream_index,
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
                    .bottom(px(38.0))
                    .flex()
                    .flex_col()
                    .min_w(px(190.0))
                    .max_w(px(280.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.62))
                    .bg(rgba(0x000000dd))
                    .py_1()
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

    pub(super) fn render_progress_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(duration) = self.playback_duration else {
            return div().id("playback-progress-empty").into_any_element();
        };

        let theme = theme::get(cx);
        let position = self
            .progress_drag_position
            .or(self.playback_position)
            .unwrap_or(0.0);
        let played_fraction = progress_fraction(position, duration);
        let playback_buffered_fraction =
            buffered_progress_fraction(self.playback_buffered_until, position, duration);
        let http_stream_buffered_range = http_stream_buffered_range_fractions(
            self.http_stream_buffered_range,
            playback_buffered_fraction,
        );
        let current_time = format_playback_time(position);
        let duration_time = format_playback_time(duration);
        let view = cx.entity().downgrade();
        let track_observer = canvas(|bounds, _, _| bounds, {
            let view = view.clone();
            move |_bounds, observed_bounds, window, _app| {
                let view = view.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_progress_track_bounds(observed_bounds, cx);
                    });
                });
            }
        })
        .absolute()
        .size_full();
        let can_toggle_playback = self.can_toggle_playback();
        let play_pause_icon = if self.playback_paused {
            "icons/play.svg"
        } else {
            "icons/pause.svg"
        };
        let can_select_audio =
            !self.audio_tracks.is_empty() || self.selected_audio_stream_index.is_some();
        let can_select_subtitle =
            !self.subtitle_tracks.is_empty() || self.selected_subtitle_stream_index.is_some();
        let audio_select_open = self.open_track_select == Some(PlaybackTrackKind::Audio);
        let subtitle_select_open = self.open_track_select == Some(PlaybackTrackKind::Subtitle);

        div()
            .id("playback-progress")
            .absolute()
            .left(relative(0.3))
            .bottom(px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX))
            .flex()
            .flex_col()
            .w(relative(0.4))
            .h(px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX))
            .justify_center()
            .gap_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.input_border.opacity(0.42))
            .bg(rgba(0x00000099))
            .px_4()
            .shadow_lg()
            .occlude()
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .text_xs()
            .text_color(theme.foreground.opacity(0.86))
            .child(
                div()
                    .id("playback-controls")
                    .relative()
                    .w_full()
                    .h(px(34.0))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .child(Self::playback_control_button(
                                "playback-previous-button",
                                "icons/previous.svg",
                                px(30.0),
                                px(16.0),
                                false,
                                cx,
                            ))
                            .child(
                                Self::playback_control_button(
                                    "playback-play-pause-button",
                                    play_pause_icon,
                                    px(34.0),
                                    px(18.0),
                                    can_toggle_playback,
                                    cx,
                                )
                                .when(
                                    can_toggle_playback,
                                    |this| {
                                        this.on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(Self::toggle_playback_pause),
                                        )
                                    },
                                ),
                            )
                            .child(Self::playback_control_button(
                                "playback-next-button",
                                "icons/next.svg",
                                px(30.0),
                                px(16.0),
                                false,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        Self::playback_control_button(
                                            "playback-audio-button",
                                            "icons/audio.svg",
                                            px(30.0),
                                            px(16.0),
                                            can_select_audio,
                                            cx,
                                        )
                                        .when(
                                            can_select_audio,
                                            |this| {
                                                this.on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(Self::toggle_audio_track_select),
                                                )
                                            },
                                        ),
                                    )
                                    .when(audio_select_open, |this| {
                                        this.child(
                                            deferred(self.render_track_select_menu(
                                                PlaybackTrackKind::Audio,
                                                cx,
                                            ))
                                            .with_priority(1),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .relative()
                                    .child(
                                        Self::playback_control_button(
                                            "playback-caption-button",
                                            "icons/caption.svg",
                                            px(30.0),
                                            px(16.0),
                                            can_select_subtitle,
                                            cx,
                                        )
                                        .when(
                                            can_select_subtitle,
                                            |this| {
                                                this.on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(Self::toggle_subtitle_track_select),
                                                )
                                            },
                                        ),
                                    )
                                    .when(subtitle_select_open, |this| {
                                        this.child(
                                            deferred(self.render_track_select_menu(
                                                PlaybackTrackKind::Subtitle,
                                                cx,
                                            ))
                                            .with_priority(1),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.0))
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
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::begin_progress_drag),
                            )
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(Self::finish_progress_drag),
                            )
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
                                    .w(relative(playback_buffered_fraction))
                                    .rounded_full()
                                    .bg(theme.muted_foreground.opacity(0.54)),
                            )
                            .when_some(
                                http_stream_buffered_range,
                                |this, (start_fraction, end_fraction)| {
                                    this.child(
                                        div()
                                            .absolute()
                                            .left(relative(start_fraction))
                                            .top(px(11.0))
                                            .h(px(6.0))
                                            .w(relative(end_fraction - start_fraction))
                                            .rounded_full()
                                            .bg(theme.muted_foreground.opacity(0.54)),
                                    )
                                },
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
                            .w(px(48.0))
                            .text_align(gpui::TextAlign::Right)
                            .child(duration_time),
                    ),
            )
            .into_any_element()
    }
}

pub(super) fn playback_info_segments(info: &PlaybackVideoInfo) -> Vec<String> {
    let mut segments = vec![
        info.decoder.clone(),
        format!("{}x{}", info.size.width, info.size.height),
    ];
    if let Some(frame_rate) = info.frame_rate.and_then(valid_frame_rate) {
        segments.push(format!("{frame_rate:.2} FPS"));
    }
    segments.push(if info.hardware_accelerated {
        "HW".to_string()
    } else {
        "SW".to_string()
    });
    segments
}

pub(super) fn track_select_option(
    label: impl Into<SharedString>,
    selected: bool,
    cx: &Context<PlaybackPage>,
) -> gpui::Div {
    let theme = theme::get(cx);
    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .justify_between()
        .gap_3()
        .px_3()
        .text_xs()
        .text_color(theme.foreground.opacity(if selected { 1.0 } else { 0.82 }))
        .bg(if selected {
            theme.foreground.opacity(0.14)
        } else {
            theme.background.opacity(0.0)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(theme.foreground.opacity(0.12)))
        .child(div().min_w_0().truncate().child(label.into()))
}

pub(super) fn valid_frame_rate(frame_rate: f64) -> Option<f64> {
    frame_rate
        .is_finite()
        .then_some(frame_rate)
        .filter(|rate| *rate > 0.0)
}
