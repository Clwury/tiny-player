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
        self.close_episode_list(cx);
        self.toggle_playback_pause_command(cx);
    }

    pub(in super::super) fn close_track_select(&mut self, cx: &mut Context<Self>) -> bool {
        let closed = self.presentation.track_select_open.take().is_some()
            || self.presentation.timeline_presentation.cache_status_open;
        self.presentation.timeline_presentation.cache_status_open = false;
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
        self.close_episode_list(cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn toggle_cache_status(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !playback_diagnostics_enabled() || self.session.timeline().cache_state.is_none() {
            return;
        }
        self.presentation.track_select_open = None;
        self.close_episode_list(cx);
        self.presentation.timeline_presentation.cache_status_open =
            !self.presentation.timeline_presentation.cache_status_open;
        cx.notify();
    }

    pub(in super::super) fn toggle_playback_pause_command(&mut self, cx: &mut Context<Self>) {
        self.dispatch_control(PlaybackIntent::TogglePause, cx);
    }

    pub(in super::super) fn adjust_playback_volume(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.dispatch_control(PlaybackIntent::AdjustVolume(delta), cx);
    }

    pub(in super::super) fn toggle_playback_mute(&mut self, cx: &mut Context<Self>) {
        self.dispatch_control(PlaybackIntent::ToggleMute, cx);
    }

    pub(in super::super) fn show_volume_indicator(&mut self, cx: &mut Context<Self>) {
        self.presentation.volume_indicator_visible = true;
        self.schedule_presentation_timer(
            PresentationTimer::Volume,
            VOLUME_INDICATOR_HIDE_DELAY,
            cx,
        );
        cx.notify();
    }

    pub(in super::super) fn seek_relative(
        &mut self,
        delta_seconds: f64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_control(PlaybackIntent::SeekRelative(delta_seconds), cx);
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
        self.dispatch_control(
            PlaybackIntent::Seek {
                position,
                mode: seek_mode,
            },
            cx,
        );
    }
}
