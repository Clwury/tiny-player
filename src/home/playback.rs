use std::time::Duration;

use gpui::Context;

use crate::player::{PlaybackStateUpdate, PlaybackStopResult};

use super::HomeContent;

const PLAYBACK_REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PLAYBACK_REFRESH_POLL_LIMIT: usize = 160;

impl HomeContent {
    pub(super) fn apply_playback_update(
        &mut self,
        update: PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        if let Some((id, change)) = self.controller.apply_playback_update(&update)
            && let Some(resources) = self.detail_resources.get_mut(&id)
        {
            resources.presentation.apply_change(change.change);
            if change.playback_cancelled {
                resources.playback_task.cancel();
            }
        }

        self.layout.content_changed();
        self.schedule_home_snapshot_save(cx);
        self.schedule_playback_server_refresh(update.stop_completion.clone(), cx);
        cx.notify();
    }

    fn schedule_playback_server_refresh(
        &mut self,
        completion: Option<crate::player::PlaybackStopCompletion>,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = completion else {
            return;
        };
        let token = self.controller.begin_playback_refresh();
        self.playback_refresh_task
            .replace(cx.spawn(async move |page, cx| {
                let mut result = completion.result();
                for _ in 0..PLAYBACK_REFRESH_POLL_LIMIT {
                    if result != PlaybackStopResult::Pending {
                        break;
                    }
                    cx.background_executor()
                        .timer(PLAYBACK_REFRESH_POLL_INTERVAL)
                        .await;
                    result = completion.result();
                }
                page.update(cx, |page, cx| {
                    if !page.controller.complete_playback_refresh(
                        &token,
                        &page.request_identity(),
                        result,
                    ) {
                        return;
                    }
                    page.refresh_user_data_after_playback(cx);
                })
                .ok();
            }));
    }

    fn refresh_user_data_after_playback(&mut self, cx: &mut Context<Self>) {
        self.refresh_feed_resume(cx);
        self.load_series_media_item_if_needed(cx);
        self.load_series_episodes_if_needed(cx);
    }
}
