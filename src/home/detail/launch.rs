//! GPUI runner for prepared playback. The controller accepts results before
//! this adapter constructs the application-facing playback event.
use super::playback::{DetailPlaybackCommand, DetailPlaybackUpdate};
use super::*;
use crate::home::detail::binding::detail_binding;
use crate::home::track_preferences::detail_track_choices;
use crate::media::gateway::ResolvedPlayback;

impl HomeContent {
    pub(in crate::home) fn play_selected_media(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.launch_selected_media(cx);
    }

    fn launch_selected_media(&mut self, cx: &mut Context<Self>) {
        self.clear_notification(NotificationScope::Detail, DETAIL_PLAYBACK_NOTIFICATION_KEY);
        let identity = self.request_identity();
        let gateway = self.ports.playback.clone();
        let Some(detail) = self.detail_view() else {
            return;
        };
        if detail.model.playback_loading || detail.model.video_sources_loading() {
            return;
        }
        let selected = effect::selected_playback(
            detail.model,
            gateway.as_ref(),
            PlaybackLanguagePreferences::get(cx),
            &detail_track_choices(detail.model, &self.current_server, cx),
        );
        let command = match self.controller.begin_detail_playback(selected, identity) {
            Ok(Some(command)) => command,
            Ok(None) => return,
            Err(message) => {
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_PLAYBACK_NOTIFICATION_KEY,
                    message,
                    cx,
                );
                cx.notify();
                return;
            }
        };
        let detail = detail_binding(self.controller.detail_view(), &mut self.detail_resources)
            .expect("started detail playback keeps its resources");
        detail.presentation.open_select = None;
        let work = cx.background_spawn(async move {
            let result = effect::run_playback(gateway.as_ref(), &command);
            (command, result)
        });
        let task = cx.spawn(async move |page, cx| {
            let (command, result) = work.await;
            page.update(cx, |page, cx| {
                page.finish_play_selected_media(command, result, cx)
            })
            .ok();
        });
        detail.playback_task.replace(task);
        cx.notify();
    }

    pub(super) fn finish_play_selected_media(
        &mut self,
        command: DetailPlaybackCommand,
        result: anyhow::Result<ResolvedPlayback>,
        cx: &mut Context<Self>,
    ) {
        let identity = self.request_identity();
        if self.detail_view().is_none() {
            return;
        }
        let Some(update) = self
            .controller
            .complete_detail_playback(command, result, &identity)
        else {
            return;
        };
        let detail = detail_binding(self.controller.detail_view(), &mut self.detail_resources)
            .expect("accepted detail playback keeps its resources");
        detail.playback_task.cancel();
        match update {
            DetailPlaybackUpdate::Open { selected, playback } => {
                let selected = *selected;
                let track_preference_key = PlaybackTrackPreferenceKey {
                    item_id: selected.item_id,
                    media_source_id: selected.media_source_id,
                };
                cx.emit(HomeContentEvent::OpenPlayback(Box::new(PlaybackRequest {
                    title: selected.title.into(),
                    url: playback.url,
                    http_headers: playback.http_headers,
                    content_length: playback.content_length,
                    audio_tracks: selected.audio_tracks,
                    subtitle_tracks: selected.subtitle_tracks,
                    selected_tracks: selected.selected_tracks,
                    track_preference_key,
                    remember_subtitle_on_start: selected.remember_subtitle_on_start,
                    initial_position_seconds: playback_initial_position_seconds(
                        selected.playback_position_ticks,
                        selected.run_time_ticks,
                    ),
                    queue: selected.queue,
                    emby: EmbyPlaybackContext {
                        client: self.emby_client.clone(),
                        server: self.current_server.clone(),
                        item_id: playback.item_id,
                        media_source_id: playback.media_source_id,
                        play_session_id: playback
                            .play_session_id
                            .filter(|id| !id.trim().is_empty()),
                        run_time_ticks: selected.run_time_ticks,
                    },
                })));
            }
            DetailPlaybackUpdate::Failed(message) => self.push_error_notification(
                NotificationScope::Detail,
                DETAIL_PLAYBACK_NOTIFICATION_KEY,
                message,
                cx,
            ),
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
