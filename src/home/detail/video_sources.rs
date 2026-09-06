use super::*;

const RESUME_VIDEO_NOTIFICATION_KEY: &str = "detail:resume-video";

impl HomeContent {
    pub(super) fn load_resume_video_sources_if_needed(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let Some(item_id) = detail.resume_media_item_id().map(str::to_string) else {
            return;
        };
        if !detail.effects.resume_sources.can_start() {
            return;
        }
        detail.effects.resume_sources = LoadState::Loading;
        let identity = self.request_identity();
        let generation = self.detail_generation;
        let client = self.emby_client.clone();
        let server = self.current_server.clone();
        let requested_id = item_id.clone();
        // Tsukimi set_intro: query the original playable item without filtering
        // MediaSourceId, and build the version dropdown from PlaybackInfo.
        let task = cx
            .background_spawn(async move { client.playback_media_sources(&server, &requested_id) });
        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_resume_video_sources(identity, generation, item_id, result, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_resume_video_sources(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        item_id: String,
        result: anyhow::Result<Vec<crate::emby::MediaSource>>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || !self.matches_request_identity(&identity) {
            return;
        }
        let Some(detail) = self
            .series_detail
            .as_mut()
            .filter(|detail| detail.resume_media_item_id() == Some(item_id.as_str()))
        else {
            return;
        };
        match result {
            Ok(sources) => {
                let previous_source_id = detail
                    .selected_media_source()
                    .and_then(|source| source.id.clone());
                detail.effects.resume_sources = LoadState::Loaded;
                detail.resume_media_sources = Some(sources);
                if detail
                    .selected_media_source()
                    .and_then(|source| source.id.as_ref())
                    != previous_source_id.as_ref()
                {
                    detail.selected_subtitle_index = None;
                    detail.reset_playback_request();
                }
                detail.sync_media_source_selection();
                self.clear_notification(NotificationScope::Detail, RESUME_VIDEO_NOTIFICATION_KEY);
            }
            Err(error) => {
                detail.effects.resume_sources = LoadState::Failed;
                detail.sync_media_source_selection();
                self.push_error_notification(
                    NotificationScope::Detail,
                    RESUME_VIDEO_NOTIFICATION_KEY,
                    format!("加载播放版本失败：{error}"),
                    cx,
                );
            }
        }
        cx.notify();
    }
}
