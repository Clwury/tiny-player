//! GPUI adapter for queue intents, backend commands, and page replacement.
use super::*;
use crate::{
    effects::EffectHandle,
    player::{
        adapter::EmbyPlaybackGateway,
        gateway::PlaybackGateway,
        queue::{
            QueueAction, QueueSwitchCommand, QueueSwitchUpdate, ResolvedQueuePlayback, effect,
        },
    },
    ui::radius,
};

// Page-owned immutable account gateway and cancellable continuation. Back,
// backend failure, accepted completion and release cancel the handle. Blocking
// IO may finish, but only the current token can update the inline queue error.
pub(super) struct QueueEffects {
    gateway: Arc<dyn PlaybackGateway>,
    task: EffectHandle<gpui::Task<()>>,
}
impl QueueEffects {
    pub(super) fn new(context: &EmbyPlaybackContext) -> Self {
        Self {
            gateway: Arc::new(EmbyPlaybackGateway {
                client: context.client.clone(),
                server: context.server.clone(),
            }),
            task: EffectHandle::default(),
        }
    }
}

impl PlaybackPage {
    pub(super) fn switch_to_episode(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_queue_switch(QueueAction::Select(index), false, window, cx);
    }
    pub(super) fn switch_to_previous_episode(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.begin_queue_switch(QueueAction::Previous, false, window, cx);
    }
    pub(super) fn switch_to_next_episode(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.begin_queue_switch(QueueAction::Next, false, window, cx);
    }
    pub(super) fn switch_to_next_episode_after_end(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_queue_switch(QueueAction::Next, true, window, cx);
    }
    pub(super) fn cancel_queue_switch(&mut self) {
        self.session.queue.cancel();
        self.queue_effects.task.cancel();
    }
    fn begin_queue_switch(
        &mut self,
        action: QueueAction,
        automatic: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self.session.queue.begin(
            action,
            automatic,
            self.session.timeline().user_paused,
            self.session.timeline().ended,
        ) else {
            return;
        };
        self.close_track_select(cx);
        self.close_episode_list(cx);
        if !self.session.pause_for_queue(
            &command,
            &self.emby.server.workspace_identity(),
            |command| self.video.command(command),
        ) {
            cx.notify();
            return;
        }
        self.report_playback_progress(true);
        let languages = crate::player::PlaybackLanguagePreferences::get(cx);
        let saved_tracks = effect::preference_key(&command.queue)
            .map(|key| crate::player::PlaybackTrackPreferences::get(&self.emby.server, &key, cx))
            .unwrap_or_default();
        let gateway = self.queue_effects.gateway.clone();
        let work = cx.background_spawn(async move {
            let result = effect::resolve(gateway.as_ref(), &command.queue, languages, saved_tracks);
            (command, result)
        });
        self.queue_effects
            .task
            .replace(cx.spawn(async move |page, cx| {
                let (command, result) = work.await;
                page.update(cx, |page, cx| page.finish_queue_switch(command, result, cx))
                    .ok();
            }));
        cx.notify();
    }
    fn finish_queue_switch(
        &mut self,
        command: QueueSwitchCommand,
        result: anyhow::Result<ResolvedQueuePlayback>,
        cx: &mut Context<Self>,
    ) {
        let Some(update) =
            self.session
                .queue
                .complete(command, result, &self.emby.server.workspace_identity())
        else {
            return;
        };
        self.queue_effects.task.cancel();
        match update {
            QueueSwitchUpdate::Replace(mut replacement) => {
                let mut update =
                    self.close_playback_reporting(false, self.session.timeline().ended);
                replacement.apply_close(&mut update);
                let playback = replacement.playback;
                cx.emit(PlaybackEvent::Replace {
                    request: Box::new(PlaybackRequest {
                        title: playback.title.into(),
                        url: playback.source.url,
                        http_headers: playback.source.http_headers,
                        content_length: playback.source.content_length,
                        audio_tracks: playback.audio_tracks,
                        subtitle_tracks: playback.subtitle_tracks,
                        selected_tracks: playback.selected_tracks,
                        track_preference_key: playback.track_preference_key,
                        remember_subtitle_on_start: false,
                        initial_position_seconds: playback.initial_position_seconds,
                        queue: replacement.queue,
                        emby: EmbyPlaybackContext {
                            client: self.emby.client.clone(),
                            server: self.emby.server.clone(),
                            item_id: playback.source.item_id,
                            media_source_id: playback.source.media_source_id,
                            play_session_id: playback.source.play_session_id,
                            run_time_ticks: playback.run_time_ticks,
                        },
                    }),
                    update,
                });
            }
            QueueSwitchUpdate::Failed(recovery) => {
                if self
                    .session
                    .resume_after_queue_failure(recovery.resume_current, |command| {
                        self.video.command(command)
                    })
                {
                    self.report_playback_progress(true);
                }
                if recovery.publish_terminal_update {
                    let update = self.close_playback_reporting(false, true);
                    cx.emit(PlaybackEvent::Update { update });
                }
            }
        }
        cx.notify();
    }

    pub(super) fn render_queue_switch_error(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(error) = self.session.queue.view_model().error.map(str::to_owned) else {
            return div()
                .id("playback-queue-switch-error-empty")
                .into_any_element();
        };
        let theme = theme::get(cx);
        div()
            .id("playback-queue-switch-error")
            .debug_selector(|| "playback-queue-switch-error".into())
            .absolute()
            .top(px(72.0))
            .left(relative(0.5))
            .ml(-px(180.0))
            .w(px(360.0))
            .rounded(radius::SURFACE)
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
}
#[cfg(test)]
mod tests {
    use crate::emby::{MediaSource, MediaStream};
    use crate::player::preferred_playback_media_source;

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
            episode_label: item_id.to_string().into(),
            overview: None,
            primary_image_tag: None,
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            premiere_date: None,
            run_time_ticks: None,
            playback_position_ticks: None,
            media_sources: Vec::new(),
        }
    }

    fn media_source(id: &str, source_type: Option<&str>, default_video: bool) -> MediaSource {
        MediaSource {
            id: Some(id.to_string()),
            item_id: None,
            name: None,
            path: None,
            source_type: source_type.map(ToString::to_string),
            container: None,
            size: None,
            bitrate: None,
            run_time_ticks: None,
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
                width: None,
                height: None,
                video_range: None,
            }]),
            default_subtitle_stream_index: None,
        }
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod interaction_tests;
