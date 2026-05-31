use crate::player::backend::BackendEvent;

use super::state::{effective_playback_paused, user_pause_from_effective_pause_event};
use super::*;

const PAUSED_BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(250);

impl PlaybackPage {
    pub(super) fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for event in self.poll_backend_events() {
            self.apply_backend_event(event, window, cx);
        }
        self.poll_video_presenter(window, cx);
        self.schedule_paused_backend_poll(cx);
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
                let paused_for_cache = self.timeline.paused_for_cache;
                let cache_buffering_percent = self.timeline.cache_buffering_percent;
                self.timeline.loaded = true;
                self.timeline.ended = false;
                self.timeline.user_paused = false;
                self.timeline.paused =
                    effective_playback_paused(self.timeline.user_paused, paused_for_cache);
                self.timeline.buffering = false;
                self.timeline.cache_state = None;
                self.timeline.cache_status_open = false;
                self.timeline.paused_for_cache = paused_for_cache;
                self.timeline.cache_buffering_percent =
                    cache_buffering_percent.filter(|_| paused_for_cache);
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
            BackendEventKind::PlaybackEnded => {
                self.finish_playback(window, cx);
            }
            BackendEventKind::Pause(paused) => {
                self.timeline.user_paused = user_pause_from_effective_pause_event(
                    self.timeline.user_paused,
                    self.timeline.paused_for_cache,
                    paused,
                );
                self.timeline.paused = effective_playback_paused(
                    self.timeline.user_paused,
                    self.timeline.paused_for_cache,
                );
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
            BackendEventKind::CacheStateChanged(state) => {
                self.apply_cache_state(state);
            }
            BackendEventKind::PausedForCacheChanged(paused_for_cache) => {
                apply_paused_for_cache_to_timeline(&mut self.timeline, paused_for_cache);
            }
            BackendEventKind::CacheBufferingChanged(percent) => {
                apply_cache_buffering_to_timeline(&mut self.timeline, percent);
            }
        }
    }

    fn schedule_paused_backend_poll(&mut self, cx: &mut Context<Self>) {
        if self.timeline.paused_backend_poll_scheduled || !self.should_poll_backend_while_paused() {
            return;
        }

        self.timeline.paused_backend_poll_scheduled = true;
        cx.spawn(async move |page, cx| {
            Timer::after(PAUSED_BACKEND_POLL_INTERVAL).await;
            page.update(cx, |page, cx| {
                page.timeline.paused_backend_poll_scheduled = false;
                if page.should_poll_backend_while_paused() {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn should_poll_backend_while_paused(&self) -> bool {
        self.video.owner().is_some()
            && self.timeline.loaded
            && self.timeline.paused
            && !self.timeline.ended
            && self.error_message.is_none()
            && self
                .timeline
                .cache_state
                .as_ref()
                .is_some_and(cache_state_needs_poll)
    }

    fn apply_cache_state(&mut self, state: PlaybackCacheState) {
        self.timeline.buffered_until = state.demux.cache_end.and_then(valid_playback_time);
        self.timeline.paused_for_cache = state.paused_for_cache;
        self.timeline.paused =
            effective_playback_paused(self.timeline.user_paused, state.paused_for_cache);
        self.timeline.cache_buffering_percent = state.buffering_percent;
        self.timeline.cache_state = Some(state);
    }

    fn finish_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.timeline.loaded = true;
        self.timeline.ended = true;
        self.timeline.user_paused = true;
        self.timeline.paused = true;
        self.timeline.buffering = false;
        self.timeline.cache_state = None;
        self.timeline.cache_status_open = false;
        self.timeline.paused_for_cache = false;
        self.timeline.cache_buffering_percent = None;
        self.timeline.pending_seek_position = None;
        self.timeline.pending_seek_keeps_frame = false;
        self.timeline.progress_drag_position = None;
        if let Some(duration) = self.timeline.duration {
            self.timeline.position = Some(duration);
            self.timeline.buffered_until = Some(duration);
        }
        self.timeline.cache_state = None;
        self.timeline.paused_for_cache = false;
        self.timeline.cache_buffering_percent = None;
        self.tracks.open = None;
        self.frame.source_size = None;
        self.playback_info = None;
        self.timeline.user_paused = true;
        self.status_message = "".into();
        self.error_message = None;
        defer_drop_subtitle(self.subtitle.active.take(), window);
        self.clear_visible_frame(window, cx);
    }

    fn reset_after_backend_failure(
        &mut self,
        message: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.timeline.loaded = false;
        self.timeline.ended = false;
        self.frame.source_size = None;
        self.playback_info = None;
        self.timeline.user_paused = true;
        self.timeline.paused = true;
        self.timeline.buffering = false;
        self.timeline.buffered_until = None;
        self.timeline.cache_state = None;
        self.timeline.cache_status_open = false;
        self.timeline.paused_for_cache = false;
        self.timeline.cache_buffering_percent = None;
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
            let presenter = self
                .video
                .dependent_mut()
                .expect("video presenter checked above");
            let render_result = presenter.render_if_needed(size);
            let presenter_snapshot = presenter.snapshot();
            if let Some(blocked_on) = presenter_snapshot.blocked_on {
                tracing::trace!(
                    blocked_on,
                    queued = presenter_snapshot.queued,
                    queue_capacity = presenter_snapshot.queue_capacity,
                    rendering = presenter_snapshot.rendering,
                    ready = presenter_snapshot.ready,
                    pending_render_requests = presenter_snapshot.pending_render_requests,
                    last_render_ms = presenter_snapshot.last_render_ms,
                    average_render_ms = presenter_snapshot.average_render_ms,
                    dropped_frames = presenter_snapshot.dropped_frames,
                    "video presenter output snapshot"
                );
            }

            match render_result {
                Ok(Some(frame)) => {
                    self.replace_visible_frame(frame, window, cx);
                    if !self.timeline.buffering {
                        self.status_message = "".into();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.timeline.user_paused = true;
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

fn cache_state_needs_poll(state: &PlaybackCacheState) -> bool {
    if state.paused_for_cache || state.buffering_percent.is_some() {
        return true;
    }
    if state.byte.as_ref().is_some_and(|byte| !byte.idle) {
        return true;
    }
    !state.demux.idle && !state.demux.eof
}

fn apply_paused_for_cache_to_timeline(
    timeline: &mut PlaybackTimelineState,
    paused_for_cache: bool,
) {
    timeline.paused_for_cache = paused_for_cache;
    timeline.paused = effective_playback_paused(timeline.user_paused, paused_for_cache);
    if !paused_for_cache {
        timeline.cache_buffering_percent = None;
    }
    if let Some(cache_state) = timeline.cache_state.as_mut() {
        cache_state.paused_for_cache = paused_for_cache;
        if !paused_for_cache {
            cache_state.buffering_percent = None;
        }
    }
}

fn apply_cache_buffering_to_timeline(timeline: &mut PlaybackTimelineState, percent: Option<u8>) {
    timeline.cache_buffering_percent = percent;
    if let Some(cache_state) = timeline.cache_state.as_mut() {
        cache_state.buffering_percent = percent;
    }
}

#[cfg(test)]
mod tests {
    use crate::player::backend::{ByteCacheState, DemuxCacheState, PlaybackCacheState};

    use super::{
        apply_cache_buffering_to_timeline, apply_paused_for_cache_to_timeline,
        cache_state_needs_poll,
    };
    use crate::player::page::state::PlaybackTimelineState;

    fn byte_cache_state(idle: bool, download_fraction: Option<f64>) -> ByteCacheState {
        ByteCacheState {
            ranges: Vec::new(),
            reader_fraction: None,
            download_fraction,
            cached_bytes: 0,
            content_length: None,
            disk_cache_enabled: false,
            idle,
            raw_input_rate: None,
            byte_level_seeks: 0,
        }
    }

    fn idle_demux_state() -> DemuxCacheState {
        DemuxCacheState {
            idle: true,
            ..DemuxCacheState::default()
        }
    }

    #[test]
    fn cache_state_poll_continues_while_cache_pause_is_active() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(true, None)),
            paused_for_cache: true,
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_while_byte_cache_is_active() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(false, None)),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_stops_when_byte_and_demux_cache_are_idle() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(true, None)),
            ..PlaybackCacheState::default()
        };

        assert!(!cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_until_byte_cache_reports_idle_even_when_download_completed() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(false, Some(1.0))),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_while_demux_cache_is_active() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState::default(),
            byte: Some(byte_cache_state(true, None)),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_pause_event_updates_stored_cache_state_copy() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            user_paused: false,
            paused: true,
            paused_for_cache: true,
            cache_buffering_percent: Some(42),
            cache_state: Some(PlaybackCacheState {
                demux: idle_demux_state(),
                paused_for_cache: true,
                buffering_percent: Some(42),
                ..PlaybackCacheState::default()
            }),
            ..PlaybackTimelineState::default()
        };

        apply_paused_for_cache_to_timeline(&mut timeline, false);

        assert!(!timeline.paused_for_cache);
        assert!(!timeline.paused);
        assert_eq!(timeline.cache_buffering_percent, None);
        let cache_state = timeline.cache_state.expect("cache state remains available");
        assert!(!cache_state.paused_for_cache);
        assert_eq!(cache_state.buffering_percent, None);
    }

    #[test]
    fn cache_buffering_event_updates_stored_cache_state_copy() {
        let mut timeline = PlaybackTimelineState {
            cache_state: Some(PlaybackCacheState {
                demux: idle_demux_state(),
                ..PlaybackCacheState::default()
            }),
            ..PlaybackTimelineState::default()
        };

        apply_cache_buffering_to_timeline(&mut timeline, Some(37));

        assert_eq!(timeline.cache_buffering_percent, Some(37));
        assert_eq!(
            timeline
                .cache_state
                .as_ref()
                .and_then(|state| state.buffering_percent),
            Some(37)
        );
    }
}
