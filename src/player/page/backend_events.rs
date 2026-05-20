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
                self.timeline.loaded = true;
                self.timeline.paused = false;
                self.timeline.buffering = false;
                self.timeline.pending_seek_position = None;
                self.timeline.pending_seek_keeps_frame = false;
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
                self.timeline.paused = paused;
            }
            BackendEventKind::Buffering(buffering) => {
                let hidden_by_soft_seek = buffering
                    && self.timeline.pending_seek_keeps_frame
                    && self.frame.current.is_some();
                self.timeline.buffering = buffering && !hidden_by_soft_seek;
                if !hidden_by_soft_seek {
                    self.status_message =
                        playback_status_message(buffering, self.frame.current.is_some());
                }
            }
            BackendEventKind::PlaybackInfoChanged(info) => {
                self.playback_info = info;
            }
            BackendEventKind::SubtitleChanged(cue) => {
                if self.subtitle.active != cue {
                    defer_drop_subtitle(self.subtitle.active.take(), window);
                }
                self.subtitle.active = cue;
            }
            BackendEventKind::VideoSizeChanged(size) => {
                if self.frame.source_size != size {
                    self.frame.source_size = size;
                    self.clear_visible_frame(window, cx);
                }
                if let (Some(info), Some(size)) = (self.playback_info.as_mut(), size) {
                    info.size = size;
                }
            }
            BackendEventKind::PositionChanged(position) => {
                if should_apply_backend_position(
                    self.timeline.progress_drag_position,
                    self.timeline.pending_seek_position,
                ) {
                    self.timeline.position = valid_playback_time(position);
                }
            }
            BackendEventKind::DurationChanged(duration) => {
                self.timeline.duration = valid_playback_duration(duration);
                if let (Some(drag_position), Some(duration)) =
                    (self.timeline.progress_drag_position, self.timeline.duration)
                {
                    self.timeline.progress_drag_position =
                        Some(clamp_playback_position(drag_position, duration));
                }
            }
            BackendEventKind::BufferedChanged(buffered_until) => {
                let buffered_until = buffered_until.and_then(valid_playback_time);
                self.timeline.buffered_until = if self.timeline.pending_seek_keeps_frame {
                    match (self.timeline.buffered_until, buffered_until) {
                        (Some(current), Some(next)) => Some(current.max(next)),
                        (_, next) => next,
                    }
                } else {
                    buffered_until
                };
            }
            BackendEventKind::HttpStreamBufferedChanged(progress) => {
                self.timeline.http_stream_buffered_range =
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
        self.timeline.loaded = false;
        self.frame.source_size = None;
        self.playback_info = None;
        self.timeline.paused = true;
        self.timeline.buffering = false;
        self.timeline.buffered_until = None;
        self.timeline.http_stream_buffered_range = None;
        self.timeline.pending_seek_position = None;
        self.timeline.pending_seek_keeps_frame = false;
        self.timeline.progress_drag_position = None;
        defer_drop_subtitle(self.subtitle.active.take(), window);
        self.clear_visible_frame(window, cx);
        self.error_message = Some(message);
    }

    fn poll_video_presenter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(presenter) = self.video.dependent_mut() {
            presenter.prewarm_if_needed();
        }

        let render_size = self
            .frame
            .viewport_bounds
            .zip(self.frame.source_size)
            .and_then(|(viewport_bounds, source_size)| {
                render_output_size(viewport_bounds, source_size)
            });
        if should_render_frame(
            self.video.dependent().is_some(),
            self.timeline.loaded,
            self.error_message.is_some(),
            self.frame.source_size.is_some(),
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
                    if !self.timeline.buffering {
                        self.status_message = "".into();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.timeline.paused = true;
                    self.clear_visible_frame(window, cx);
                    self.error_message = Some(format!("渲染视频失败：{error}").into());
                }
            }
        } else {
            self.clear_visible_frame(window, cx);
        }
    }
}
