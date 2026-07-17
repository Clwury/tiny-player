use super::fullscreen::{
    PLAYBACK_BACK_BUTTON_OFFSET_PX, PLAYBACK_BACK_BUTTON_SIZE_PX,
    PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX, PLAYBACK_PROGRESS_BAR_HEIGHT_PX,
};
use super::state::effective_playback_paused;
use super::*;

const TRACK_SELECT_MENU_MAX_HEIGHT_PX: f32 = 260.0;
const VOLUME_INDICATOR_HIDE_DELAY: Duration = Duration::from_millis(1200);
const VOLUME_INDICATOR_BAR_HEIGHT_PX: f32 = 192.0;
const PLAYBACK_DETAILS_WIDTH_PX: f32 = 500.0;
const PLAYBACK_DETAILS_TOP_PX: f32 = 56.0;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackDetailRow {
    label: String,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackDetailSection {
    title: &'static str,
    summary: String,
    rows: Vec<PlaybackDetailRow>,
}

#[derive(Clone, Copy)]
struct PlaybackControlsRenderState {
    can_switch_previous: bool,
    can_toggle_playback: bool,
    can_switch_next: bool,
    play_pause_icon: &'static str,
    cache_status_enabled: bool,
    cache_status_open: bool,
    can_select_audio: bool,
    can_select_subtitle: bool,
    audio_select_open: bool,
    subtitle_select_open: bool,
}

struct ProgressTimelineRenderState {
    current_time: String,
    duration_time: String,
    played_fraction: f32,
    cached_seek_preview: Option<bool>,
    cache_ranges: Vec<(f32, f32)>,
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
        let closed = self.tracks.open.take().is_some() || self.timeline.cache_status_open;
        self.timeline.cache_status_open = false;
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

    pub(super) fn toggle_cache_status(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.timeline.cache_state.is_none() {
            return;
        }
        self.tracks.open = None;
        self.timeline.cache_status_open = !self.timeline.cache_status_open;
        cx.notify();
    }

    pub(super) fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
        if !self.can_toggle_playback() {
            return;
        }

        let previous_user_paused = self.timeline.user_paused;
        let user_paused = !previous_user_paused;
        let Some(backend) = self.video.owner_mut() else {
            return;
        };
        let command = if user_paused {
            BackendCommand::Pause
        } else {
            BackendCommand::Resume
        };
        if let Err(error) = backend.command(command) {
            self.timeline.user_paused = previous_user_paused;
            self.timeline.paused =
                effective_playback_paused(previous_user_paused, self.timeline.paused_for_cache);
            if self.timeline.paused {
                self.timeline.buffering = false;
            }
            self.error_message = Some(format!("控制播放失败：{error}").into());
        } else {
            self.timeline.user_paused = user_paused;
            self.timeline.paused =
                effective_playback_paused(user_paused, self.timeline.paused_for_cache);
            if self.timeline.paused {
                self.timeline.buffering = false;
            }
            self.report_playback_progress(true);
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
        let was_muted = previous_volume <= f32::EPSILON;
        self.volume.level = volume;
        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetVolume { volume })
        {
            self.volume.level = previous_volume;
            self.error_message = Some(format!("调整音量失败：{error}").into());
        }
        if was_muted != (self.volume.level <= f32::EPSILON) {
            self.report_playback_progress(true);
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
        self.timeline.cache_status_open = false;
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
        self.timeline.cache_status_open = false;
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

        let command_succeeded = if let Some(backend) = self.video.owner_mut() {
            match backend.command(BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            }) {
                Ok(()) => true,
                Err(error) => {
                    self.tracks.selected_audio_stream_index = previous_audio;
                    self.timeline.buffering = false;
                    self.error_message = Some(format!("切换轨道失败：{error}").into());
                    false
                }
            }
        } else {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.timeline.buffering = false;
            false
        };
        if command_succeeded {
            self.report_playback_progress(true);
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

        let command_succeeded = if let Some(backend) = self.video.owner_mut() {
            match backend.command(BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            }) {
                Ok(()) => true,
                Err(error) => {
                    self.tracks.selected_audio_stream_index = previous_audio;
                    self.tracks.selected_subtitle_stream_index = previous_subtitle;
                    self.subtitle.active = previous_active_subtitle.take();
                    self.timeline.buffering = false;
                    self.error_message = Some(format!("切换轨道失败：{error}").into());
                    false
                }
            }
        } else {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.tracks.selected_subtitle_stream_index = previous_subtitle;
            self.subtitle.active = previous_active_subtitle.take();
            self.timeline.buffering = false;
            false
        };
        if command_succeeded {
            self.report_playback_progress(true);
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
        self.seek_to_position_with_mode(position, PlaybackSeekMode::Fast, window, cx);
    }

    pub(super) fn seek_to_position(
        &mut self,
        position: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.seek_to_position_with_mode(position, PlaybackSeekMode::Precise, window, cx);
    }

    fn seek_to_position_with_mode(
        &mut self,
        position: f64,
        seek_mode: PlaybackSeekMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = self
            .timeline
            .duration
            .map(|duration| clamp_playback_position(position, duration))
            .unwrap_or(position);
        let previous_position = self.timeline.position;
        let previous_buffered_until = self.timeline.buffered_until;
        let previous_ended = self.timeline.ended;
        let cached_seek_expected = cached_seek_target(
            self.timeline.cache_state.as_ref(),
            self.timeline.buffered_until,
            previous_position,
            position,
        );
        self.timeline.progress_drag_position = None;
        self.timeline.ended = false;
        self.timeline.position = Some(position);
        self.timeline.buffered_until = if cached_seek_expected {
            buffered_until_after_seek(self.timeline.buffered_until, position)
        } else {
            valid_playback_time(position)
        };
        self.timeline.pending_seek_position = Some(position);
        self.timeline.pending_seek_keeps_frame = cached_seek_expected;
        self.timeline.buffering = self.timeline.loaded && !cached_seek_expected;
        self.status_message =
            playback_status_message(self.timeline.buffering, self.frame.current.is_some());
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.discard_pending_frames();
        }

        let command_succeeded = if let Some(backend) = self.video.owner_mut() {
            match backend.command(BackendCommand::Seek {
                position_seconds: position,
                mode: seek_mode,
            }) {
                Ok(()) => true,
                Err(error) => {
                    self.timeline.pending_seek_position = None;
                    self.timeline.pending_seek_keeps_frame = false;
                    self.timeline.buffering = false;
                    self.error_message = Some(format!("跳转播放位置失败：{error}").into());
                    false
                }
            }
        } else {
            false
        };
        if command_succeeded {
            self.report_playback_progress(true);
        } else {
            self.timeline.position = previous_position;
            self.timeline.buffered_until = previous_buffered_until;
            self.timeline.ended = previous_ended;
            self.timeline.pending_seek_position = None;
            self.timeline.pending_seek_keeps_frame = false;
            self.timeline.buffering = false;
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

    pub(super) fn render_playback_details_overlay(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
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

    pub(super) fn render_back_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);

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
                state.can_switch_previous,
                state.can_toggle_playback,
                state.play_pause_icon,
                state.can_switch_next,
                cx,
            ))
            .child(self.render_track_control_buttons(state, cx))
    }

    fn render_primary_transport_controls(
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
            .child(self.render_cache_status_button(
                state.cache_status_enabled,
                state.cache_status_open,
                cx,
            ))
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

    fn render_cache_status_button(
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

    fn render_cache_status_popover(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
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
        state: ProgressTimelineRenderState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let played_color = if state.cached_seek_preview == Some(false) {
            theme.warning
        } else {
            theme.input_border_focused
        };

        let track = div()
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
            .children(
                state
                    .cache_ranges
                    .into_iter()
                    .map(|(start_fraction, end_fraction)| {
                        progress_track_seekable_range_fill(
                            theme.muted_foreground.opacity(0.52),
                            start_fraction,
                            end_fraction,
                        )
                    }),
            );
        let track = track
            .child(progress_track_played_fill(
                played_color,
                state.played_fraction,
            ))
            .child(progress_track_observer(cx));

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
                ProgressTimelineRenderState {
                    current_time,
                    duration_time,
                    played_fraction,
                    cached_seek_preview,
                    cache_ranges,
                },
                cx,
            ))
            .into_any_element()
    }
}

fn progress_track_fill(color: gpui::Hsla, width_fraction: f32) -> gpui::Div {
    div()
        .absolute()
        .left_0()
        .top(px(11.0))
        .h(px(6.0))
        .w(relative(width_fraction))
        .rounded(px(3.0))
        .bg(color)
}

fn progress_track_played_fill(color: gpui::Hsla, width_fraction: f32) -> impl IntoElement {
    let width_fraction = width_fraction.clamp(0.0, 1.0);
    progress_track_fill(color, width_fraction)
        .when(width_fraction > 0.0, |fill| fill.min_w(px(6.0)))
}

fn progress_track_seekable_range_fill(
    color: gpui::Hsla,
    start_fraction: f32,
    end_fraction: f32,
) -> impl IntoElement {
    let start_fraction = start_fraction.clamp(0.0, 1.0);
    let end_fraction = end_fraction.clamp(start_fraction, 1.0);
    div()
        .absolute()
        .left(relative(start_fraction))
        .top(px(19.0))
        .h(px(3.0))
        .w(relative(end_fraction - start_fraction))
        .min_w(px(2.0))
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

impl PlaybackDetailRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

fn playback_detail_section_element<T>(
    section: PlaybackDetailSection,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(format!("{}:", section.title)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(section.summary),
                ),
        )
        .children(section.rows.into_iter().map(|row| {
            div()
                .flex()
                .items_start()
                .gap_2()
                .pl_2()
                .child(
                    div()
                        .flex_none()
                        .w(px(112.0))
                        .text_color(theme.muted_foreground)
                        .child(format!("{}:", row.label)),
                )
                .child(div().min_w_0().flex_1().child(row.value))
        }))
}

fn playback_file_detail_section(
    title: &str,
    content_length: Option<u64>,
    protocol: Option<&str>,
    file_info: Option<&PlaybackFileInfo>,
    duration: Option<f64>,
    cache_state: Option<&PlaybackCacheState>,
) -> PlaybackDetailSection {
    let mut rows = Vec::new();
    let content_length = content_length.or_else(|| {
        cache_state.and_then(|state| state.byte.as_ref().and_then(|cache| cache.content_length))
    });
    if let Some(size) = content_length.filter(|size| *size > 0) {
        rows.push(PlaybackDetailRow::new("Size", format_cache_bytes(size)));
    }
    if let Some(format_protocol) = playback_format_protocol(file_info, protocol) {
        rows.push(PlaybackDetailRow::new("Format/Protocol", format_protocol));
    }
    if let Some(duration) = duration.filter(|duration| duration.is_finite() && *duration > 0.0) {
        rows.push(PlaybackDetailRow::new(
            "Duration",
            format_playback_time(duration),
        ));
    }
    if let Some(bitrate) = file_info.and_then(|info| info.bitrate) {
        rows.push(PlaybackDetailRow::new(
            "Overall Bitrate",
            format_bitrate(bitrate),
        ));
    }
    if let Some(total_cache) = playback_total_cache(cache_state) {
        rows.push(PlaybackDetailRow::new("Total Cache", total_cache));
    }

    PlaybackDetailSection {
        title: "File",
        summary: title.to_string(),
        rows,
    }
}

fn playback_display_detail_section(
    display_size: RenderSize,
    output_size: Option<RenderSize>,
    presenter: Option<VideoPresenterSnapshot>,
) -> PlaybackDetailSection {
    let mut rows = vec![
        PlaybackDetailRow::new("Context", "libplacebo / Vulkan"),
        PlaybackDetailRow::new("Resolution", format_render_size(display_size)),
    ];
    if let Some(output_size) = output_size {
        rows.push(PlaybackDetailRow::new(
            "Output Resolution",
            format_render_size(output_size),
        ));
    }
    if let Some(presenter) = presenter {
        rows.push(PlaybackDetailRow::new(
            "Dropped Frames",
            presenter.dropped_frames.to_string(),
        ));
        rows.push(PlaybackDetailRow::new(
            "Frame Queue",
            format!("{} / {}", presenter.queued, presenter.queue_capacity),
        ));
        if presenter.average_render_ms.is_finite() && presenter.average_render_ms > 0.0 {
            rows.push(PlaybackDetailRow::new(
                "Render Time",
                format!("{:.2} ms", presenter.average_render_ms),
            ));
        }
    }

    PlaybackDetailSection {
        title: "Display",
        summary: "GPUI video output".to_string(),
        rows,
    }
}

fn playback_video_detail_section(
    info: Option<&PlaybackVideoInfo>,
    loaded: bool,
) -> PlaybackDetailSection {
    let Some(info) = info else {
        return PlaybackDetailSection {
            title: "Video",
            summary: if loaded { "Unavailable" } else { "Loading…" }.to_string(),
            rows: Vec::new(),
        };
    };

    let mut rows = vec![PlaybackDetailRow::new("Decoder", info.decoder.clone())];
    rows.push(PlaybackDetailRow::new(
        "Decode Mode",
        if info.hardware_accelerated {
            "Vulkan HW"
        } else {
            "Software"
        },
    ));
    if let Some(frame_rate) = info.frame_rate.and_then(valid_frame_rate) {
        rows.push(PlaybackDetailRow::new(
            "Frame Rate",
            format!("{frame_rate:.3} fps"),
        ));
    }
    let mut resolution = format_render_size(info.size);
    if let Some((numerator, denominator)) = info
        .sample_aspect_ratio
        .filter(|(numerator, denominator)| *numerator != *denominator)
    {
        resolution.push_str(&format!("  SAR {numerator}:{denominator}"));
    }
    rows.push(PlaybackDetailRow::new("Resolution", resolution));
    push_optional_detail(&mut rows, "Format", info.pixel_format.as_deref());
    push_optional_detail(&mut rows, "Levels", info.color_range.as_deref());
    push_optional_detail(&mut rows, "Chroma Loc", info.chroma_location.as_deref());
    push_optional_detail(&mut rows, "Colormatrix", info.color_space.as_deref());
    push_optional_detail(&mut rows, "Primaries", info.color_primaries.as_deref());
    push_optional_detail(&mut rows, "Transfer", info.color_transfer.as_deref());
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }

    PlaybackDetailSection {
        title: "Video",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
        ),
        rows,
    }
}

fn playback_audio_detail_section(
    info: Option<&PlaybackAudioInfo>,
    loaded: bool,
    volume: f32,
) -> PlaybackDetailSection {
    let Some(info) = info else {
        return PlaybackDetailSection {
            title: "Audio",
            summary: if loaded { "No audio" } else { "Loading…" }.to_string(),
            rows: Vec::new(),
        };
    };

    let mut rows = vec![PlaybackDetailRow::new("Decoder", info.decoder.clone())];
    let has_audio_output = info.output_device.is_some()
        || info.output_channels.is_some()
        || info.output_sample_format.is_some()
        || info.output_sample_rate.is_some();
    if has_audio_output {
        rows.push(PlaybackDetailRow::new("AO", "cpal"));
        push_optional_detail(&mut rows, "Device", info.output_device.as_deref());
        rows.push(PlaybackDetailRow::new(
            "AO Volume",
            format!("{}%", playback_volume_percent(volume)),
        ));
    }

    let input_channels = audio_channel_description(info.channels, info.channel_layout.as_deref());
    let output_channels = info.output_channels.map(|channels| channels.to_string());
    if let Some(channels) = transition_value(input_channels, output_channels) {
        rows.push(PlaybackDetailRow::new("Channels", channels));
    }
    if let Some(format) = transition_value(
        info.sample_format.clone(),
        info.output_sample_format.clone(),
    ) {
        rows.push(PlaybackDetailRow::new("Format", format));
    }
    if let Some(sample_rate) = transition_value(
        info.sample_rate.map(|rate| rate.to_string()),
        info.output_sample_rate.map(|rate| rate.to_string()),
    ) {
        rows.push(PlaybackDetailRow::new(
            "Sample Rate",
            format!("{sample_rate} Hz"),
        ));
    }
    if let Some(bitrate) = info.bitrate {
        rows.push(PlaybackDetailRow::new("Bitrate", format_bitrate(bitrate)));
    }

    PlaybackDetailSection {
        title: "Audio",
        summary: codec_summary(
            &info.codec,
            info.codec_description.as_deref(),
            info.profile.as_deref(),
        ),
        rows,
    }
}

fn push_optional_detail(rows: &mut Vec<PlaybackDetailRow>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        rows.push(PlaybackDetailRow::new(label, value));
    }
}

fn playback_format_protocol(
    file_info: Option<&PlaybackFileInfo>,
    protocol: Option<&str>,
) -> Option<String> {
    let format = file_info.and_then(|info| {
        match (
            info.format_description.as_deref(),
            info.format_name.as_deref(),
        ) {
            (Some(description), Some(name)) if !description.eq_ignore_ascii_case(name) => {
                Some(format!("{description} ({name})"))
            }
            (Some(description), _) => Some(description.to_string()),
            (_, Some(name)) => Some(name.to_string()),
            _ => None,
        }
    });
    transition_value(format, protocol.map(ToString::to_string))
        .map(|value| value.replace(" → ", " / "))
}

fn playback_total_cache(cache_state: Option<&PlaybackCacheState>) -> Option<String> {
    let cache_state = cache_state?;
    let bytes = cache_state.demux.forward_bytes;
    let duration = cache_state
        .demux
        .cache_duration
        .filter(|duration| duration.is_finite() && *duration > 0.0);
    if bytes == 0 && duration.is_none() {
        return None;
    }
    match (bytes > 0, duration) {
        (true, Some(duration)) => {
            Some(format!("{} ({duration:.1} sec)", format_cache_bytes(bytes)))
        }
        (true, None) => Some(format_cache_bytes(bytes)),
        (false, Some(duration)) => Some(format!("{duration:.1} sec")),
        (false, None) => None,
    }
}

fn codec_summary(codec: &str, description: Option<&str>, profile: Option<&str>) -> String {
    let mut summary = description
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .unwrap_or(codec)
        .to_string();
    if let Some(profile) = profile.map(str::trim).filter(|profile| !profile.is_empty()) {
        summary.push_str(&format!(" [{profile}]"));
    }
    summary
}

fn audio_channel_description(channels: Option<u32>, layout: Option<&str>) -> Option<String> {
    match (
        channels,
        layout.map(str::trim).filter(|layout| !layout.is_empty()),
    ) {
        (Some(channels), Some(layout)) => Some(format!("{layout} ({channels})")),
        (Some(channels), None) => Some(channels.to_string()),
        (None, Some(layout)) => Some(layout.to_string()),
        (None, None) => None,
    }
}

fn transition_value(input: Option<String>, output: Option<String>) -> Option<String> {
    match (input, output) {
        (Some(input), Some(output)) if input != output => Some(format!("{input} → {output}")),
        (Some(input), _) => Some(input),
        (None, Some(output)) => Some(output),
        (None, None) => None,
    }
}

fn format_render_size(size: RenderSize) -> String {
    format!("{}x{}", size.width, size.height)
}

fn format_bitrate(bits_per_second: u64) -> String {
    const KILOBIT: f64 = 1_000.0;
    const MEGABIT: f64 = 1_000_000.0;
    const GIGABIT: f64 = 1_000_000_000.0;
    let bitrate = bits_per_second as f64;
    if bitrate >= GIGABIT {
        format!("{:.2} Gb/s", bitrate / GIGABIT)
    } else if bitrate >= MEGABIT {
        format!("{:.2} Mb/s", bitrate / MEGABIT)
    } else if bitrate >= KILOBIT {
        format!("{:.1} kb/s", bitrate / KILOBIT)
    } else {
        format!("{bits_per_second} b/s")
    }
}

fn play_pause_icon_for_user_pause(user_paused: bool) -> &'static str {
    if user_paused {
        "icons/play.svg"
    } else {
        "icons/pause.svg"
    }
}

pub(super) fn cache_status_segments(cache_state: Option<&PlaybackCacheState>) -> Vec<String> {
    let Some(cache_state) = cache_state else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    if let Some(rate) = cache_state.demux.raw_input_rate {
        segments.push(format!("速率 {}/s", format_cache_bytes(rate)));
    }
    if let Some(duration) = cache_state
        .demux
        .cache_duration
        .filter(|duration| duration.is_finite())
    {
        segments.push(format!("Demux {:.1}s", duration.max(0.0)));
    }
    for stream in &cache_state.demux.streams {
        let Some(duration) = stream
            .cache_duration
            .filter(|duration| duration.is_finite())
        else {
            continue;
        };
        let label = match stream.kind {
            StreamCacheKind::Video => "V",
            StreamCacheKind::Audio => "A",
            StreamCacheKind::Subtitle => "S",
            StreamCacheKind::Unknown => "?",
        };
        let status = if stream.underrun {
            " 断供"
        } else if stream.idle {
            " 空闲"
        } else {
            ""
        };
        segments.push(format!("{label} {:.1}s{status}", duration.max(0.0)));
    }
    if let Some(byte_cache) = cache_state.byte.as_ref()
        && byte_cache.cached_bytes > 0
    {
        segments.push(format!(
            "Byte {}",
            format_cache_bytes(byte_cache.cached_bytes)
        ));
    }
    if let Some(file_cache_bytes) = cache_state.demux.file_cache_bytes
        && file_cache_bytes > 0
    {
        segments.push(format!("磁盘 {}", format_cache_bytes(file_cache_bytes)));
    }
    segments.push(if cache_state.demux.idle {
        "状态 空闲".to_string()
    } else {
        "状态 读取".to_string()
    });
    if let Some(percent) = cache_state.buffering_percent {
        segments.push(format!("缓冲 {percent}%"));
    }
    segments.push(format!(
        "Seek {}/{}/{}",
        cache_state.demux.cached_seeks,
        cache_state.demux.low_level_seeks,
        cache_state.demux.byte_level_seeks
    ));
    segments
}

fn format_cache_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

pub(super) fn track_select_option(
    label: impl Into<SharedString>,
    selected: bool,
    cx: &Context<PlaybackPage>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let hover_background = if selected {
        theme.input_border_focused.opacity(0.34)
    } else {
        theme.foreground.opacity(0.12)
    };

    div()
        .flex()
        .flex_none()
        .h(px(32.0))
        .min_h(px(32.0))
        .items_center()
        .rounded(px(6.0))
        .px_1()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.foreground
        } else {
            theme.foreground.opacity(0.86)
        })
        .bg(if selected {
            theme.input_border_focused.opacity(0.24)
        } else {
            theme.foreground.opacity(0.0)
        })
        .cursor_pointer()
        .hover(move |style| style.bg(hover_background))
        .child(div().flex_1().min_w_0().truncate().child(label.into()))
}

pub(super) fn valid_frame_rate(frame_rate: f64) -> Option<f64> {
    frame_rate
        .is_finite()
        .then_some(frame_rate)
        .filter(|rate| *rate > 0.0)
}

#[cfg(test)]
mod tests {
    use crate::player::{
        backend::{
            ByteCacheState, DemuxCacheState, PlaybackAudioInfo, PlaybackCacheState,
            PlaybackFileInfo, PlaybackVideoInfo, StreamCacheState,
        },
        page::state::user_pause_from_effective_pause_event,
        render_host::RenderSize,
    };

    use super::*;

    #[test]
    fn video_detail_section_contains_mpv_style_status() {
        let info = PlaybackVideoInfo {
            codec: "hevc".to_string(),
            codec_description: Some("HEVC (High Efficiency Video Coding)".to_string()),
            profile: Some("Main 10".to_string()),
            decoder: "hevc".to_string(),
            size: RenderSize {
                width: 3840,
                height: 2160,
            },
            sample_aspect_ratio: Some((1, 1)),
            frame_rate: Some(23.976),
            pixel_format: Some("yuv420p10le".to_string()),
            color_range: Some("tv".to_string()),
            chroma_location: Some("left".to_string()),
            color_space: Some("bt2020nc".to_string()),
            color_primaries: Some("bt2020".to_string()),
            color_transfer: Some("smpte2084".to_string()),
            bitrate: Some(18_500_000),
            hardware_accelerated: true,
        };

        let section = playback_video_detail_section(Some(&info), true);

        assert_eq!(section.title, "Video");
        assert_eq!(
            section.summary,
            "HEVC (High Efficiency Video Coding) [Main 10]"
        );
        assert_eq!(detail_row_value(&section, "Decoder"), Some("hevc"));
        assert_eq!(detail_row_value(&section, "Decode Mode"), Some("Vulkan HW"));
        assert_eq!(detail_row_value(&section, "Frame Rate"), Some("23.976 fps"));
        assert_eq!(detail_row_value(&section, "Resolution"), Some("3840x2160"));
        assert_eq!(detail_row_value(&section, "Format"), Some("yuv420p10le"));
        assert_eq!(detail_row_value(&section, "Colormatrix"), Some("bt2020nc"));
        assert_eq!(detail_row_value(&section, "Bitrate"), Some("18.50 Mb/s"));
    }

    #[test]
    fn audio_detail_section_reports_input_to_output_transforms() {
        let info = PlaybackAudioInfo {
            codec: "eac3".to_string(),
            codec_description: Some("ATSC A/52B (AC-3, E-AC-3)".to_string()),
            profile: None,
            decoder: "eac3".to_string(),
            channels: Some(6),
            channel_layout: Some("5.1(side)".to_string()),
            sample_format: Some("fltp".to_string()),
            sample_rate: Some(48_000),
            output_channels: Some(2),
            output_sample_format: Some("f32".to_string()),
            output_sample_rate: Some(48_000),
            output_device: Some("Built-in Audio".to_string()),
            bitrate: Some(768_000),
        };

        let section = playback_audio_detail_section(Some(&info), true, 0.75);

        assert_eq!(section.title, "Audio");
        assert_eq!(detail_row_value(&section, "AO"), Some("cpal"));
        assert_eq!(detail_row_value(&section, "AO Volume"), Some("75%"));
        assert_eq!(
            detail_row_value(&section, "Channels"),
            Some("5.1(side) (6) → 2")
        );
        assert_eq!(detail_row_value(&section, "Format"), Some("fltp → f32"));
        assert_eq!(detail_row_value(&section, "Sample Rate"), Some("48000 Hz"));
        assert_eq!(detail_row_value(&section, "Bitrate"), Some("768.0 kb/s"));
    }

    #[test]
    fn file_detail_section_combines_format_protocol_and_cache() {
        let file_info = PlaybackFileInfo {
            format_name: Some("matroska,webm".to_string()),
            format_description: Some("Matroska / WebM".to_string()),
            bitrate: Some(20_000_000),
        };
        let cache_state = PlaybackCacheState {
            demux: DemuxCacheState {
                forward_bytes: 32 * 1024 * 1024,
                cache_duration: Some(12.5),
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        let section = playback_file_detail_section(
            "示例视频",
            Some(2 * 1024 * 1024 * 1024),
            Some("https"),
            Some(&file_info),
            Some(3661.0),
            Some(&cache_state),
        );

        assert_eq!(section.title, "File");
        assert_eq!(section.summary, "示例视频");
        assert_eq!(detail_row_value(&section, "Size"), Some("2.0 GiB"));
        assert_eq!(
            detail_row_value(&section, "Format/Protocol"),
            Some("Matroska / WebM (matroska,webm) / https")
        );
        assert_eq!(detail_row_value(&section, "Duration"), Some("1:01:01"));
        assert_eq!(
            detail_row_value(&section, "Total Cache"),
            Some("32.0 MiB (12.5 sec)")
        );
    }

    #[test]
    fn playback_detail_helpers_reject_invalid_frame_rates() {
        assert_eq!(valid_frame_rate(0.0), None);
        assert_eq!(valid_frame_rate(f64::INFINITY), None);
        assert_eq!(valid_frame_rate(60.0), Some(60.0));
    }

    fn detail_row_value<'a>(section: &'a PlaybackDetailSection, label: &str) -> Option<&'a str> {
        section
            .rows
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.value.as_str())
    }

    #[test]
    fn cache_status_segments_include_compact_cache_metrics() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_duration: Some(2.25),
                idle: true,
                file_cache_bytes: Some(4 * 1024 * 1024),
                raw_input_rate: Some(1536),
                cached_seeks: 1,
                low_level_seeks: 2,
                byte_level_seeks: 3,
                streams: vec![
                    StreamCacheState {
                        kind: StreamCacheKind::Video,
                        cache_end: Some(3.0),
                        reader_pts: Some(1.0),
                        cache_duration: Some(2.0),
                        underrun: false,
                        idle: true,
                    },
                    StreamCacheState {
                        kind: StreamCacheKind::Audio,
                        cache_end: Some(2.5),
                        reader_pts: Some(1.0),
                        cache_duration: Some(1.5),
                        underrun: true,
                        idle: false,
                    },
                ],
                ..DemuxCacheState::default()
            },
            byte: Some(ByteCacheState {
                ranges: Vec::new(),
                reader_fraction: None,
                download_fraction: None,
                cached_bytes: 8 * 1024,
                content_length: Some(64 * 1024),
                disk_cache_enabled: true,
                idle: true,
                raw_input_rate: Some(1536),
                byte_level_seeks: 3,
                ..ByteCacheState::default()
            }),
            paused_for_cache: true,
            buffering_percent: Some(42),
        };

        assert_eq!(
            cache_status_segments(Some(&state)),
            vec![
                "速率 1.5 KiB/s".to_string(),
                "Demux 2.2s".to_string(),
                "V 2.0s 空闲".to_string(),
                "A 1.5s 断供".to_string(),
                "Byte 8.0 KiB".to_string(),
                "磁盘 4.0 MiB".to_string(),
                "状态 空闲".to_string(),
                "缓冲 42%".to_string(),
                "Seek 1/2/3".to_string(),
            ]
        );
        assert!(cache_status_segments(None).is_empty());
    }

    #[test]
    fn play_pause_icon_reflects_user_pause_not_cache_pause() {
        assert_eq!(play_pause_icon_for_user_pause(true), "icons/play.svg");
        assert_eq!(play_pause_icon_for_user_pause(false), "icons/pause.svg");
        assert!(effective_playback_paused(false, true));
        assert!(!effective_playback_paused(false, false));
        assert!(!user_pause_from_effective_pause_event(false, true, false));
        assert!(!user_pause_from_effective_pause_event(false, true, true));
        assert!(user_pause_from_effective_pause_event(false, false, true));
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
