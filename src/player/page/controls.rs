use super::fullscreen::{PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX, PLAYBACK_PROGRESS_BAR_HEIGHT_PX};
use super::*;

const TRACK_SELECT_MENU_MAX_HEIGHT_PX: f32 = 260.0;
const VOLUME_INDICATOR_HIDE_DELAY: Duration = Duration::from_millis(1200);
const VOLUME_INDICATOR_BAR_HEIGHT_PX: f32 = 192.0;

#[derive(Clone, Copy)]
struct PlaybackControlsRenderState {
    can_toggle_playback: bool,
    play_pause_icon: &'static str,
    can_select_audio: bool,
    can_select_subtitle: bool,
    audio_select_open: bool,
    subtitle_select_open: bool,
}

impl PlaybackPage {
    fn toggle_playback_pause(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.close_track_select(cx);
        self.toggle_playback_pause_command(cx);
    }

    pub(super) fn close_track_select(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self.tracks.open.take().is_some();
        if closed {
            cx.notify();
        }
        closed
    }

    fn close_track_select_on_mouse_down(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_track_select(cx);
        cx.stop_propagation();
    }

    pub(super) fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
        if !self.can_toggle_playback() {
            return;
        }

        let paused = !self.timeline.paused;
        let Some(backend) = self.video.owner_mut() else {
            return;
        };
        let command = if paused {
            BackendCommand::Pause
        } else {
            BackendCommand::Resume
        };
        if let Err(error) = backend.command(command) {
            self.timeline.paused = true;
            self.timeline.buffering = false;
            self.error_message = Some(format!("控制播放失败：{error}").into());
        } else {
            self.timeline.paused = paused;
            if paused {
                self.timeline.buffering = false;
            }
        }
        cx.notify();
    }

    pub(super) fn adjust_playback_volume(&mut self, delta: f32, cx: &mut Context<Self>) {
        let volume = clamp_playback_volume(self.volume.level + delta);
        if (self.volume.level - volume).abs() < f32::EPSILON {
            self.show_volume_indicator(cx);
            return;
        }

        let previous_volume = self.volume.level;
        self.volume.level = volume;
        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetVolume { volume })
        {
            self.volume.level = previous_volume;
            self.error_message = Some(format!("调整音量失败：{error}").into());
        }
        self.show_volume_indicator(cx);
    }

    fn show_volume_indicator(&mut self, cx: &mut Context<Self>) {
        self.volume.indicator_visible = true;
        self.volume.hide_generation = self.volume.hide_generation.wrapping_add(1);
        let generation = self.volume.hide_generation;
        cx.spawn(async move |page, cx| {
            Timer::after(VOLUME_INDICATOR_HIDE_DELAY).await;
            page.update(cx, |page, cx| {
                if page.volume.hide_generation != generation {
                    return;
                }
                page.volume.indicator_visible = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn toggle_audio_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.tracks.audio.is_empty() && self.tracks.selected_audio_stream_index.is_none() {
            return;
        }
        self.tracks.open = if self.tracks.open == Some(PlaybackTrackKind::Audio) {
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
        if self.tracks.subtitles.is_empty() && self.tracks.selected_subtitle_stream_index.is_none()
        {
            return;
        }
        self.tracks.open = if self.tracks.open == Some(PlaybackTrackKind::Subtitle) {
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
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let previous_audio = self.tracks.selected_audio_stream_index;
        self.tracks.selected_audio_stream_index = track_index;
        self.tracks.open = None;
        self.timeline.buffering = self.timeline.loaded;
        self.status_message = "正在切换轨道…".into();

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            })
        {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.timeline.buffering = false;
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
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let previous_audio = self.tracks.selected_audio_stream_index;
        let previous_subtitle = self.tracks.selected_subtitle_stream_index;
        let mut previous_active_subtitle = self.subtitle.active.take();
        self.tracks.selected_subtitle_stream_index = track.as_ref().map(|track| track.stream_index);
        self.tracks.open = None;
        self.timeline.buffering = self.timeline.loaded;
        self.status_message = "正在切换轨道…".into();

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            })
        {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.tracks.selected_subtitle_stream_index = previous_subtitle;
            self.subtitle.active = previous_active_subtitle.take();
            self.timeline.buffering = false;
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
        if self.close_track_select(cx) {
            cx.stop_propagation();
            return;
        }
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
            .timeline
            .progress_drag_position
            .is_none_or(|current| (current - position).abs() >= 0.02)
        {
            self.timeline.progress_drag_position = Some(position);
            cx.notify();
        }
    }

    pub(super) fn commit_progress_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.timeline.progress_drag_position else {
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
            .timeline
            .progress_drag_position
            .or(self.timeline.pending_seek_position)
            .or(self.timeline.position)
            .unwrap_or(0.0)
            + delta_seconds;
        self.seek_to_position(position, window, cx);
    }

    pub(super) fn seek_to_position(
        &mut self,
        position: f64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self
            .timeline
            .duration
            .map(|duration| clamp_playback_position(position, duration))
            .unwrap_or(position);
        self.timeline.progress_drag_position = None;
        self.timeline.ended = false;
        self.timeline.position = Some(position);
        self.timeline.buffered_until =
            buffered_until_after_seek(self.timeline.buffered_until, position);
        self.timeline.pending_seek_position = Some(position);
        self.timeline.pending_seek_keeps_frame = true;
        self.timeline.buffering = false;
        self.status_message = "".into();
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.discard_pending_frames();
        }

        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::Seek {
                position_seconds: position,
            })
        {
            self.timeline.pending_seek_position = None;
            self.timeline.pending_seek_keeps_frame = false;
            self.timeline.buffering = false;
            self.error_message = Some(format!("跳转播放位置失败：{error}").into());
        }
        cx.notify();
    }

    pub(super) fn position_for_progress_cursor(&self, cursor_x: Pixels) -> Option<f64> {
        let duration = self.timeline.duration?;
        let bounds = self.timeline.progress_track_bounds?;
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
                    .overflow_y_scroll()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.input_border.opacity(0.62))
                    .bg(rgba(0x000000dd))
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

    pub(super) fn render_back_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .id("playback-back-button")
            .absolute()
            .left_4()
            .top_4()
            .flex()
            .size(px(32.0))
            .items_center()
            .justify_center()
            .rounded_md()
            .hover(move |style| style.bg(theme.secondary_hover))
            .occlude()
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

    pub(super) fn render_volume_indicator(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
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
                state.can_toggle_playback,
                state.play_pause_icon,
                cx,
            ))
            .child(self.render_track_control_buttons(
                state.can_select_audio,
                state.can_select_subtitle,
                state.audio_select_open,
                state.subtitle_select_open,
                cx,
            ))
    }

    fn render_primary_transport_controls(
        &self,
        can_toggle_playback: bool,
        play_pause_icon: &'static str,
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
                .when(can_toggle_playback, |this| {
                    this.on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_playback_pause))
                }),
            )
            .child(Self::playback_control_button(
                "playback-next-button",
                "icons/next.svg",
                px(30.0),
                px(16.0),
                false,
                cx,
            ))
    }

    fn render_track_control_buttons(
        &self,
        can_select_audio: bool,
        can_select_subtitle: bool,
        audio_select_open: bool,
        subtitle_select_open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .absolute()
            .right_0()
            .top_0()
            .flex()
            .items_center()
            .gap_2()
            .child(self.render_track_control_button(
                PlaybackTrackKind::Audio,
                "playback-audio-button",
                "icons/audio.svg",
                can_select_audio,
                audio_select_open,
                cx,
            ))
            .child(self.render_track_control_button(
                PlaybackTrackKind::Subtitle,
                "playback-caption-button",
                "icons/caption.svg",
                can_select_subtitle,
                subtitle_select_open,
                cx,
            ))
    }

    fn render_track_control_button(
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
        current_time: String,
        duration_time: String,
        played_fraction: f32,
        buffered_fraction: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);

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
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_progress_drag))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::finish_progress_drag))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::finish_progress_drag))
                    .on_drag(ProgressBarDrag, |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| ProgressBarDrag)
                    })
                    .on_drag_move(cx.listener(Self::drag_progress))
                    .child(progress_track_fill(theme.input_border.opacity(0.48), 1.0))
                    .child(progress_track_fill(
                        theme.muted_foreground.opacity(0.54),
                        buffered_fraction,
                    ))
                    .child(progress_track_fill(
                        theme.input_border_focused,
                        played_fraction,
                    ))
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
                    .child(progress_track_observer(cx)),
            )
            .child(
                div()
                    .w(px(48.0))
                    .text_align(gpui::TextAlign::Right)
                    .child(duration_time),
            )
    }

    pub(super) fn render_progress_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(duration) = self.timeline.duration else {
            return div().id("playback-progress-empty").into_any_element();
        };

        let theme = theme::get(cx);
        let position = self
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let played_fraction = progress_fraction(position, duration);
        let buffered_fraction =
            buffered_progress_fraction(self.timeline.buffered_until, position, duration);
        let current_time = format_playback_time(position);
        let duration_time = format_playback_time(duration);
        let can_toggle_playback = self.can_toggle_playback();
        let play_pause_icon = if self.timeline.paused {
            "icons/play.svg"
        } else {
            "icons/pause.svg"
        };
        let can_select_audio =
            !self.tracks.audio.is_empty() || self.tracks.selected_audio_stream_index.is_some();
        let can_select_subtitle = !self.tracks.subtitles.is_empty()
            || self.tracks.selected_subtitle_stream_index.is_some();
        let controls = PlaybackControlsRenderState {
            can_toggle_playback,
            play_pause_icon,
            can_select_audio,
            can_select_subtitle,
            audio_select_open: self.tracks.open == Some(PlaybackTrackKind::Audio),
            subtitle_select_open: self.tracks.open == Some(PlaybackTrackKind::Subtitle),
        };

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
                current_time,
                duration_time,
                played_fraction,
                buffered_fraction,
                cx,
            ))
            .into_any_element()
    }
}

fn progress_track_fill(color: gpui::Hsla, width_fraction: f32) -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top(px(11.0))
        .h(px(6.0))
        .w(relative(width_fraction))
        .rounded_full()
        .bg(color)
}

fn progress_track_observer(cx: &Context<PlaybackPage>) -> impl IntoElement {
    let view = cx.entity().downgrade();
    canvas(|bounds, _, _| bounds, {
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
    .size_full()
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
        .flex_none()
        .h(px(32.0))
        .min_h(px(32.0))
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

#[cfg(test)]
mod tests {
    use crate::player::{backend::PlaybackVideoInfo, render_host::RenderSize};

    use super::*;

    #[test]
    fn playback_info_segments_include_hw_badge_and_frame_rate() {
        let info = PlaybackVideoInfo {
            decoder: "hevc".to_string(),
            size: RenderSize {
                width: 3840,
                height: 2160,
            },
            frame_rate: Some(23.976),
            hardware_accelerated: true,
        };

        assert_eq!(
            playback_info_segments(&info),
            vec![
                "hevc".to_string(),
                "3840x2160".to_string(),
                "23.98 FPS".to_string(),
                "HW".to_string()
            ]
        );
    }

    #[test]
    fn playback_info_segments_mark_software_and_skip_invalid_rate() {
        let info = PlaybackVideoInfo {
            decoder: "h264".to_string(),
            size: RenderSize {
                width: 1920,
                height: 1080,
            },
            frame_rate: Some(f64::NAN),
            hardware_accelerated: false,
        };

        assert_eq!(
            playback_info_segments(&info),
            vec![
                "h264".to_string(),
                "1920x1080".to_string(),
                "SW".to_string()
            ]
        );
        assert_eq!(valid_frame_rate(0.0), None);
        assert_eq!(valid_frame_rate(f64::INFINITY), None);
        assert_eq!(valid_frame_rate(60.0), Some(60.0));
    }

    #[test]
    fn playback_volume_helpers_clamp_and_scale_scroll() {
        assert_eq!(playback_volume_percent(-0.5), 0);
        assert_eq!(playback_volume_percent(0.525), 52);
        assert_eq!(playback_volume_percent(f32::NAN), 100);
        assert_eq!(playback_volume_percent(1.5), 100);
        assert_eq!(
            volume_delta_from_scroll_delta(ScrollDelta::Lines(gpui::Point { x: 0.0, y: 1.0 })),
            0.05
        );
        assert_eq!(
            volume_delta_from_scroll_delta(ScrollDelta::Pixels(gpui::Point {
                x: px(0.0),
                y: px(-250.0),
            })),
            -0.2
        );
    }
}
