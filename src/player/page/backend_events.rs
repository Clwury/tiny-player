use crate::player::backend::BackendEvent;

use super::*;

impl PlaybackPage {
    pub(super) fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for event in self.poll_backend_events() {
            self.apply_backend_event(event, window, cx);
        }
        self.poll_video_presenter(window, cx);
    }

    fn poll_backend_events(&mut self) -> Vec<BackendEvent> {
        self.video
            .owner_mut()
            .map(|backend| backend.poll_events())
            .unwrap_or_default()
    }

    fn apply_backend_event(
        &mut self,
        event: BackendEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.kind {
            BackendEventKind::PlaybackRestart => {
                self.current_file_loaded = true;
                self.playback_paused = false;
                self.playback_buffering = false;
                self.pending_seek_position = None;
                self.pending_seek_keeps_frame = false;
                self.status_message = "".into();
                self.error_message = None;
            }
            BackendEventKind::LoadFailed(message) => {
                self.reset_after_backend_failure(
                    format!("加载视频失败：{message}").into(),
                    window,
                    cx,
                );
            }
            BackendEventKind::Fatal(message) => {
                self.reset_after_backend_failure(
                    format!("播放后端错误：{message}").into(),
                    window,
                    cx,
                );
            }
            BackendEventKind::Pause(paused) => {
                self.playback_paused = paused;
            }
            BackendEventKind::Buffering(buffering) => {
                let hidden_by_soft_seek =
                    buffering && self.pending_seek_keeps_frame && self.current_frame.is_some();
                self.playback_buffering = buffering && !hidden_by_soft_seek;
                if !hidden_by_soft_seek {
                    self.status_message =
                        playback_status_message(buffering, self.current_frame.is_some());
                }
            }
            BackendEventKind::PlaybackInfoChanged(info) => {
                self.playback_info = info;
            }
            BackendEventKind::SubtitleChanged(cue) => {
                if self.active_subtitle != cue {
                    defer_drop_subtitle(self.active_subtitle.take(), window);
                }
                self.active_subtitle = cue;
            }
            BackendEventKind::VideoSizeChanged(size) => {
                if self.video_source_size != size {
                    self.video_source_size = size;
                    self.clear_visible_frame(window, cx);
                }
                if let (Some(info), Some(size)) = (self.playback_info.as_mut(), size) {
                    info.size = size;
                }
            }
            BackendEventKind::PositionChanged(position) => {
                if should_apply_backend_position(
                    self.progress_drag_position,
                    self.pending_seek_position,
                ) {
                    self.playback_position = valid_playback_time(position);
                }
            }
            BackendEventKind::DurationChanged(duration) => {
                self.playback_duration = valid_playback_duration(duration);
                if let (Some(drag_position), Some(duration)) =
                    (self.progress_drag_position, self.playback_duration)
                {
                    self.progress_drag_position =
                        Some(clamp_playback_position(drag_position, duration));
                }
            }
            BackendEventKind::BufferedChanged(buffered_until) => {
                self.playback_buffered_until = buffered_until.and_then(valid_playback_time);
            }
            BackendEventKind::HttpStreamBufferedChanged(progress) => {
                self.http_stream_buffered_range =
                    progress.and_then(valid_http_stream_buffer_progress);
            }
        }
    }

    fn reset_after_backend_failure(
        &mut self,
        message: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.current_file_loaded = false;
        self.video_source_size = None;
        self.playback_info = None;
        self.playback_paused = true;
        self.playback_buffering = false;
        self.playback_buffered_until = None;
        self.http_stream_buffered_range = None;
        self.pending_seek_position = None;
        self.pending_seek_keeps_frame = false;
        self.progress_drag_position = None;
        defer_drop_subtitle(self.active_subtitle.take(), window);
        self.clear_visible_frame(window, cx);
        self.error_message = Some(message);
    }

    fn poll_video_presenter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.prewarm_if_needed();
        }

        let render_size = self
            .video_viewport_bounds
            .zip(self.video_source_size)
            .and_then(|(viewport_bounds, source_size)| {
                render_output_size(viewport_bounds, source_size)
            });
        if should_render_frame(
            self.video.dependent().is_some(),
            self.current_file_loaded,
            self.error_message.is_some(),
            self.video_source_size.is_some(),
            render_size.is_some(),
        ) {
            let size = render_size.expect("render size checked above");
            let render_result = self
                .video
                .dependent_mut()
                .expect("video presenter checked above")
                .render_if_needed(size);

            match render_result {
                Ok(Some(frame)) => {
                    self.replace_visible_frame(frame, window, cx);
                    if !self.playback_buffering {
                        self.status_message = "".into();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.playback_paused = true;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("渲染视频失败：{error}").into());
                }
            }
        } else {
            self.clear_visible_frame(window, cx);
        }
    }
}
