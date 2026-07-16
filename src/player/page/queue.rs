use anyhow::{Result, anyhow};

use crate::emby::playback::resolve_direct_stream_url;

use super::request::{
    default_playback_track_selection, playback_audio_tracks_for_source,
    playback_subtitle_tracks_for_source, preferred_playback_media_source,
};
use super::state::effective_playback_paused;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackQueueDirection {
    Previous,
    Next,
}

impl PlaybackQueueDirection {
    fn loading_message(self, automatic: bool) -> &'static str {
        match (self, automatic) {
            (Self::Previous, _) => "正在切换到上一集…",
            (Self::Next, true) => "正在播放下一集…",
            (Self::Next, false) => "正在切换到下一集…",
        }
    }

    fn failure_prefix(self) -> &'static str {
        match self {
            Self::Previous => "切换上一集失败",
            Self::Next => "切换下一集失败",
        }
    }
}

#[derive(Default)]
pub(super) struct PlaybackQueueSwitchState {
    generation: u64,
    loading: bool,
    resume_on_failure: bool,
    publish_terminal_update_on_failure: bool,
    error: Option<SharedString>,
    message: Option<SharedString>,
}

impl PlaybackPage {
    pub(super) fn can_switch_to_previous_episode(&self) -> bool {
        !self.queue_switch.loading && self.queue.previous_index().is_some()
    }

    pub(super) fn can_switch_to_next_episode(&self) -> bool {
        !self.queue_switch.loading && self.queue.next_index().is_some()
    }

    pub(super) fn switch_to_previous_episode(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.begin_queue_switch(PlaybackQueueDirection::Previous, false, window, cx);
    }

    pub(super) fn switch_to_next_episode(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.begin_queue_switch(PlaybackQueueDirection::Next, false, window, cx);
    }

    pub(super) fn switch_to_next_episode_after_end(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_queue_switch(PlaybackQueueDirection::Next, true, window, cx);
    }

    pub(super) fn cancel_queue_switch(&mut self) {
        self.queue_switch.generation = self.queue_switch.generation.wrapping_add(1);
        self.queue_switch.loading = false;
        self.queue_switch.resume_on_failure = false;
        self.queue_switch.publish_terminal_update_on_failure = false;
        self.queue_switch.error = None;
        self.queue_switch.message = None;
    }

    fn begin_queue_switch(
        &mut self,
        direction: PlaybackQueueDirection,
        automatic: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.queue_switch.loading {
            return;
        }
        let target_index = match direction {
            PlaybackQueueDirection::Previous => self.queue.previous_index(),
            PlaybackQueueDirection::Next => self.queue.next_index(),
        };
        let Some(target_index) = target_index else {
            return;
        };
        let Some(target) = self.queue.items.get(target_index).cloned() else {
            return;
        };

        self.close_track_select(cx);
        self.queue_switch.generation = self.queue_switch.generation.wrapping_add(1);
        let generation = self.queue_switch.generation;
        self.queue_switch.loading = true;
        self.queue_switch.resume_on_failure =
            !automatic && !self.timeline.user_paused && !self.timeline.ended;
        self.queue_switch.publish_terminal_update_on_failure = automatic;
        self.queue_switch.error = None;
        self.queue_switch.message = Some(direction.loading_message(automatic).into());

        if self.queue_switch.resume_on_failure {
            let pause_result = self
                .video
                .owner_mut()
                .map(|backend| backend.command(BackendCommand::Pause));
            if let Some(Err(error)) = pause_result {
                self.queue_switch.loading = false;
                self.queue_switch.resume_on_failure = false;
                self.queue_switch.publish_terminal_update_on_failure = false;
                self.queue_switch.message = None;
                self.queue_switch.error =
                    Some(format!("{}：{error}", direction.failure_prefix()).into());
                cx.notify();
                return;
            }
            self.timeline.user_paused = true;
            self.timeline.paused = true;
            self.timeline.buffering = false;
        }
        self.report_playback_progress(true);
        cx.notify();

        let client = self.emby.client.clone();
        let server = self.emby.server.clone();
        let mut queue = self.queue.clone();
        queue.current_index = target_index;
        let task = cx.background_spawn(async move {
            resolve_queue_playback_request(client, server, queue, target)
        });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_queue_switch(generation, direction, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_queue_switch(
        &mut self,
        generation: u64,
        direction: PlaybackQueueDirection,
        result: Result<PlaybackRequest>,
        cx: &mut Context<Self>,
    ) {
        if self.queue_switch.generation != generation || !self.queue_switch.loading {
            return;
        }

        self.queue_switch.loading = false;
        match result {
            Ok(mut request) => {
                self.queue_switch.resume_on_failure = false;
                self.queue_switch.publish_terminal_update_on_failure = false;
                self.queue_switch.message = None;
                let mut update = self.close_playback_reporting(false, self.timeline.ended);
                if let Some(item) = request.queue.items.get_mut(self.queue.current_index) {
                    item.playback_position_ticks = Some(if update.ended {
                        0
                    } else {
                        update.position_ticks
                    });
                }
                update.selected_item_id = Some(request.emby.item_id.clone());
                cx.emit(PlaybackEvent::Replace {
                    request: Box::new(request),
                    update,
                });
            }
            Err(error) => {
                let resume_on_failure = self.queue_switch.resume_on_failure;
                let publish_terminal_update = self.queue_switch.publish_terminal_update_on_failure;
                self.queue_switch.resume_on_failure = false;
                self.queue_switch.publish_terminal_update_on_failure = false;
                self.queue_switch.message = None;
                self.queue_switch.error =
                    Some(format!("{}：{error}", direction.failure_prefix()).into());
                if resume_on_failure && !self.timeline.ended {
                    let resume_result = self
                        .video
                        .owner_mut()
                        .map(|backend| backend.command(BackendCommand::Resume));
                    if let Some(Err(resume_error)) = resume_result {
                        self.queue_switch.error = Some(
                            format!(
                                "{}：{error}；恢复当前播放失败：{resume_error}",
                                direction.failure_prefix()
                            )
                            .into(),
                        );
                    } else {
                        self.timeline.user_paused = false;
                        self.timeline.paused =
                            effective_playback_paused(false, self.timeline.paused_for_cache);
                        self.report_playback_progress(true);
                    }
                }
                if publish_terminal_update {
                    let update = self.close_playback_reporting(false, true);
                    cx.emit(PlaybackEvent::Update { update });
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_queue_switch_error(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(error) = self.queue_switch.error.clone() else {
            return div()
                .id("playback-queue-switch-error-empty")
                .into_any_element();
        };
        let theme = theme::get(cx);
        div()
            .id("playback-queue-switch-error")
            .absolute()
            .top(px(72.0))
            .left(relative(0.5))
            .ml(-px(180.0))
            .w(px(360.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.error.opacity(0.5))
            .bg(rgba(0x000000c8))
            .px_3()
            .py_2()
            .text_sm()
            .text_color(theme.error)
            .text_align(gpui::TextAlign::Center)
            .occlude()
            .child(error)
            .into_any_element()
    }

    pub(super) fn render_queue_switch_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(message) = self.queue_switch.message.clone() else {
            return div()
                .id("playback-queue-switch-status-empty")
                .into_any_element();
        };
        let theme = theme::get(cx);
        div()
            .id("playback-queue-switch-status")
            .absolute()
            .top(relative(0.5))
            .left(relative(0.5))
            .ml(-px(120.0))
            .mt(-px(20.0))
            .w(px(240.0))
            .rounded(px(8.0))
            .bg(rgba(0x000000b8))
            .px_3()
            .py_2()
            .text_sm()
            .text_color(theme.foreground)
            .text_align(gpui::TextAlign::Center)
            .occlude()
            .child(message)
            .into_any_element()
    }
}

fn resolve_queue_playback_request(
    client: crate::emby::EmbyClient,
    server: crate::server::CachedServer,
    queue: PlaybackQueue,
    item: PlaybackQueueItem,
) -> Result<PlaybackRequest> {
    let source = preferred_playback_media_source(&item.media_sources)
        .ok_or_else(|| anyhow!("目标单集没有可用视频源"))?;
    let selected_media_source_id = source
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("目标单集视频源缺少 ID"))?
        .to_string();
    let playback_info = client.playback_info(&server, &item.item_id, &selected_media_source_id)?;
    let resolved_source = playback_info.direct_stream_source_for(&selected_media_source_id)?;
    let direct_stream_url = resolved_source.direct_stream_url()?;
    let url = resolve_direct_stream_url(&server, direct_stream_url)?;
    let http_headers = client.playback_http_headers(&server)?;
    let media_source_id = resolved_source
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(selected_media_source_id.as_str())
        .to_string();
    let audio_tracks = playback_audio_tracks_for_source(source);
    let subtitle_tracks =
        playback_subtitle_tracks_for_source(source, &server, &item.item_id, &media_source_id);
    let selected_tracks = default_playback_track_selection(source, &subtitle_tracks);
    let initial_position_seconds =
        playback_initial_position_seconds(item.playback_position_ticks, item.run_time_ticks);

    Ok(PlaybackRequest {
        title: item.title,
        url: url.to_string(),
        http_headers,
        content_length: resolved_source.size,
        audio_tracks,
        subtitle_tracks,
        selected_tracks,
        initial_position_seconds,
        queue,
        emby: EmbyPlaybackContext {
            client,
            server,
            item_id: item.item_id,
            media_source_id,
            play_session_id: playback_info
                .play_session_id
                .filter(|id| !id.trim().is_empty()),
            run_time_ticks: item.run_time_ticks,
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::emby::{MediaSource, MediaStream};

    use super::*;

    #[test]
    fn queue_buttons_follow_current_season_boundaries() {
        let queue = PlaybackQueue::new(
            vec![
                queue_item("episode-1"),
                queue_item("episode-2"),
                queue_item("episode-3"),
            ],
            0,
        );
        assert_eq!(queue.previous_index(), None);
        assert_eq!(queue.next_index(), Some(1));

        let middle = PlaybackQueue::new(queue.items.clone(), 1);
        assert_eq!(middle.previous_index(), Some(0));
        assert_eq!(middle.next_index(), Some(2));

        let last = PlaybackQueue::new(queue.items, 2);
        assert_eq!(last.previous_index(), Some(1));
        assert_eq!(last.next_index(), None);
    }

    #[test]
    fn adjacent_episode_uses_default_media_source() {
        let sources = vec![
            media_source("source-1", Some("Secondary"), false),
            media_source("source-2", Some("Default"), false),
        ];

        assert_eq!(
            preferred_playback_media_source(&sources).and_then(|source| source.id.as_deref()),
            Some("source-2")
        );
    }

    fn queue_item(item_id: &str) -> PlaybackQueueItem {
        PlaybackQueueItem {
            item_id: item_id.to_string(),
            title: item_id.to_string().into(),
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            run_time_ticks: None,
            playback_position_ticks: None,
            media_sources: Vec::new(),
        }
    }

    fn media_source(id: &str, source_type: Option<&str>, default_video: bool) -> MediaSource {
        MediaSource {
            id: Some(id.to_string()),
            name: None,
            path: None,
            source_type: source_type.map(ToString::to_string),
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(0),
                stream_type: Some("Video".to_string()),
                display_title: None,
                title: None,
                language: None,
                codec: None,
                delivery_url: None,
                delivery_method: None,
                is_external: None,
                is_default: Some(default_video),
                is_forced: None,
                is_text_subtitle_stream: None,
                supports_external_stream: None,
            }]),
            default_subtitle_stream_index: None,
        }
    }
}
