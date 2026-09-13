use super::*;

impl PlaybackPage {
    pub(in super::super) fn toggle_playback_pause(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.close_track_select(cx);
        self.toggle_playback_pause_command(cx);
    }

    pub(in super::super) fn close_track_select(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self.tracks.open.take().is_some() || self.timeline.cache_status_open;
        self.timeline.cache_status_open = false;
        if closed {
            cx.notify();
        }
        closed
    }

    pub(in super::super) fn close_track_select_on_mouse_down(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_track_select(cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn toggle_cache_status(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !playback_diagnostics_enabled() || self.timeline.cache_state.is_none() {
            return;
        }
        self.tracks.open = None;
        self.timeline.cache_status_open = !self.timeline.cache_status_open;
        cx.notify();
    }

    pub(in super::super) fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
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

    pub(in super::super) fn adjust_playback_volume(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_playback_volume(self.volume.level + delta, cx);
    }

    pub(in super::super) fn toggle_playback_mute(&mut self, cx: &mut Context<Self>) {
        self.set_playback_volume(self.volume.level_after_mute_toggle(), cx);
    }

    fn set_playback_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        let volume = clamp_playback_volume(volume);
        if (self.volume.level - volume).abs() < f32::EPSILON {
            self.show_volume_indicator(cx);
            return;
        }

        let was_muted = self.volume.level <= f32::EPSILON;
        if let Some(backend) = self.video.owner_mut()
            && let Err(error) = backend.command(BackendCommand::SetVolume { volume })
        {
            self.error_message = Some(format!("调整音量失败：{error}").into());
            self.show_volume_indicator(cx);
            return;
        }
        self.volume.set_level(volume);
        cx.emit(PlaybackEvent::VolumeChanged {
            settings: self.volume.settings(),
        });
        if was_muted != (self.volume.level <= f32::EPSILON) {
            self.report_playback_progress(true);
        }
        self.show_volume_indicator(cx);
    }

    pub(in super::super) fn show_volume_indicator(&mut self, cx: &mut Context<Self>) {
        self.volume.indicator_visible = true;
        self.volume.hide_generation = self.volume.hide_generation.wrapping_add(1);
        let generation = self.volume.hide_generation;
        cx.spawn(async move |page, cx| {
            cx.background_executor()
                .timer(VOLUME_INDICATOR_HIDE_DELAY)
                .await;
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

    pub(in super::super) fn seek_relative(
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

    pub(in super::super) fn seek_to_position(
        &mut self,
        position: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.seek_to_position_with_mode(position, PlaybackSeekMode::Precise, window, cx);
    }

    pub(in super::super) fn seek_to_position_with_mode(
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
}
