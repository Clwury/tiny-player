use super::*;

impl PlaybackPage {
    pub(in super::super) fn toggle_audio_track_select(
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

    pub(in super::super) fn toggle_subtitle_track_select(
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

    pub(in super::super) fn select_audio_track(
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

    pub(in super::super) fn select_subtitle_track(
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
}
