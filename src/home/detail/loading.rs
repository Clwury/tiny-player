use super::*;

impl HomeContent {
    pub(super) fn load_media_detail_effects(&mut self, cx: &mut Context<Self>) {
        self.load_resume_video_sources_if_needed(cx);
        self.load_series_media_item_if_needed(cx);
        self.load_similar_items_if_needed(cx);
        if self
            .series_detail
            .as_ref()
            .is_some_and(SeriesDetailState::is_series)
        {
            self.load_series_seasons_if_needed(cx);
            if self
                .series_detail
                .as_ref()
                .is_some_and(SeriesDetailState::should_load_next_up)
            {
                self.load_series_next_up_if_needed(cx);
            }
            self.load_series_episodes_if_needed(cx);
        }
    }

    pub(in super::super) fn load_series_media_item_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let generation = self.detail_generation;
        self.clear_notification(NotificationScope::Detail, DETAIL_ITEM_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.item.can_start() {
            return;
        }

        detail.effects.item = LoadState::Loading;
        detail.item_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task = cx.background_spawn(async move { client.media_item(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_media_item(
                    identity,
                    user_data_revision,
                    generation,
                    series_id,
                    result,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_series_media_item(
        &mut self,
        identity: WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItem>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || self
                .series_detail
                .as_ref()
                .is_none_or(|detail| detail.series_id.as_str() != series_id.as_str())
        {
            return;
        }

        match result {
            Ok(mut item) => {
                self.ensure_series_media_item_images(&item, cx);
                self.absorb_user_data(&item.id, item.user_data.as_ref(), user_data_revision);
                if let Some(data) = self.user_data_overrides.get(&item.id) {
                    item.user_data = Some(data.clone());
                }
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str() {
                        return;
                    }

                    detail.effects.item = LoadState::Loaded;
                    detail.item_failed = None;
                    if detail.title != item.name {
                        detail.title = item.name.clone();
                        cx.emit(HomeContentEvent::TitleChanged);
                    }
                    detail.item = Some(item);
                    if detail.is_movie() {
                        detail.sync_media_source_selection();
                    }
                }
            }
            Err(error) => {
                let message: SharedString = format!("加载媒体详情失败：{error}").into();
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str() {
                        return;
                    }

                    detail.effects.item = LoadState::Failed;
                    detail.item_failed = Some(message.clone());
                }
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_ITEM_NOTIFICATION_KEY,
                    message,
                    cx,
                );
            }
        }

        cx.notify();
    }

    pub(super) fn load_similar_items_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let user_data_revision = self.user_data_request_revision();
        let generation = self.detail_generation;
        self.clear_notification(NotificationScope::Detail, DETAIL_SIMILAR_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.effects.similar.can_start() {
            return;
        }

        detail.effects.similar = LoadState::Loading;
        detail.similar_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let item_id = detail.series_id.clone();
        let task_item_id = item_id.clone();
        let task = cx.background_spawn(async move { client.similar_items(&server, &task_item_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_similar_items(
                    identity,
                    user_data_revision,
                    generation,
                    item_id,
                    result,
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_similar_items(
        &mut self,
        identity: WorkspaceIdentity,
        user_data_revision: u64,
        generation: u64,
        item_id: String,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation
            || !self.matches_request_identity(&identity)
            || self
                .series_detail
                .as_ref()
                .is_none_or(|detail| detail.series_id.as_str() != item_id.as_str())
        {
            return;
        }

        match result {
            Ok(mut items) => {
                items.items.retain(|item| {
                    !item.id.trim().is_empty()
                        && matches!(item.item_type.as_deref(), Some("Movie" | "Series"))
                });
                items.total_record_count = items.items.len() as u32;
                self.absorb_user_items_user_data(&items, user_data_revision);
                self.ensure_user_items_images(&items, cx);
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != item_id.as_str() {
                        return;
                    }

                    detail.effects.similar = LoadState::Loaded;
                    detail.similar_failed = None;
                    detail.similar_items = Some(items);
                }
            }
            Err(error) => {
                let message: SharedString = format!("加载相似作品失败：{error}").into();
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != item_id.as_str() {
                        return;
                    }

                    detail.effects.similar = LoadState::Failed;
                    detail.similar_failed = Some(message.clone());
                }
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_SIMILAR_NOTIFICATION_KEY,
                    message,
                    cx,
                );
            }
        }

        cx.notify();
    }

    pub(super) fn load_series_seasons_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let generation = self.detail_generation;
        self.clear_notification(NotificationScope::Detail, DETAIL_SEASONS_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.is_series() || !detail.effects.seasons.can_start() {
            return;
        }

        detail.effects.seasons = LoadState::Loading;
        detail.seasons_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_seasons(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_seasons(identity, generation, series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_series_seasons(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || !self.matches_request_identity(&identity) {
            return;
        }
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.series_id.as_str() != series_id.as_str() {
            return;
        }

        match result {
            Ok(seasons) => {
                detail.effects.seasons = LoadState::Loaded;
                detail.seasons_failed = None;
                detail.seasons = Some(seasons);
                detail.choose_season_if_needed();
                self.load_series_episodes_if_needed(cx);
            }
            Err(error) => {
                let message: SharedString = format!("加载剧集季数失败：{error}").into();
                detail.effects.seasons = LoadState::Failed;
                detail.seasons_failed = Some(message.clone());
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_SEASONS_NOTIFICATION_KEY,
                    message,
                    cx,
                );
            }
        }

        cx.notify();
    }

    pub(super) fn load_series_next_up_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let generation = self.detail_generation;
        self.clear_notification(NotificationScope::Detail, DETAIL_NEXT_UP_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.should_load_next_up() || !detail.effects.next_up.can_start() {
            return;
        }

        detail.effects.next_up = LoadState::Loading;
        detail.next_up_failed = None;
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task =
            cx.background_spawn(async move { client.show_next_up(&server, &task_series_id) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_next_up(identity, generation, series_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_series_next_up(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        series_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != generation || !self.matches_request_identity(&identity) {
            return;
        }
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.series_id.as_str() != series_id.as_str() {
            return;
        }

        let mut failure = None;
        match result {
            Ok(mut next_up) => {
                apply_media_item_user_data_overrides(&mut next_up.items, &self.user_data_overrides);
                detail.effects.next_up = LoadState::Loaded;
                detail.next_up_failed = None;
                detail.next_up = Some(next_up);
                detail.apply_next_up_preference();
                self.load_series_episodes_if_needed(cx);
            }
            Err(error) => {
                let message: SharedString = format!("加载下一剧集失败：{error}").into();
                detail.effects.next_up = LoadState::Failed;
                detail.next_up_failed = Some(message.clone());
                failure = Some(message);
                detail.choose_season_if_needed();
                self.load_series_episodes_if_needed(cx);
            }
        }

        if let Some(message) = failure {
            self.push_error_notification(
                NotificationScope::Detail,
                DETAIL_NEXT_UP_NOTIFICATION_KEY,
                message,
                cx,
            );
        }

        cx.notify();
    }

    pub(in super::super) fn load_series_episodes_if_needed(&mut self, cx: &mut Context<Self>) {
        let identity = self.request_identity();
        let revisions = DetailRequestRevisions {
            detail: self.detail_generation,
            user_data: self.user_data_request_revision(),
        };
        self.clear_notification(NotificationScope::Detail, DETAIL_EPISODES_NOTIFICATION_KEY);
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if !detail.is_series() {
            return;
        }
        let Some(season_id) = detail.selected_season_id.clone() else {
            return;
        };
        let already_requested =
            detail.episodes_request_season_id.as_deref() == Some(season_id.as_str());
        if already_requested && detail.effects.episodes.is_loading() {
            return;
        }
        if already_requested
            && detail.effects.episodes == LoadState::Loaded
            && detail.episodes.is_some()
        {
            return;
        }

        detail.effects.episodes = LoadState::Loading;
        detail.episodes_failed = None;
        detail.episodes_request_season_id = Some(season_id.clone());
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let series_id = detail.series_id.clone();
        let task_series_id = series_id.clone();
        let task_season_id = season_id.clone();
        let task = cx.background_spawn(async move {
            client.show_episodes(&server, &task_series_id, Some(&task_season_id))
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_series_episodes(identity, revisions, series_id, season_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn finish_series_episodes(
        &mut self,
        identity: WorkspaceIdentity,
        revisions: DetailRequestRevisions,
        series_id: String,
        season_id: String,
        result: anyhow::Result<MediaItems>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_generation != revisions.detail
            || !self.matches_request_identity(&identity)
            || self.series_detail.as_ref().is_none_or(|detail| {
                detail.series_id.as_str() != series_id.as_str()
                    || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                    || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
            })
        {
            return;
        }

        match result {
            Ok(mut episodes) => {
                for episode in &episodes.items {
                    self.absorb_user_data(
                        &episode.id,
                        episode.user_data.as_ref(),
                        revisions.user_data,
                    );
                }
                apply_media_item_user_data_overrides(
                    &mut episodes.items,
                    &self.user_data_overrides,
                );
                self.ensure_series_episode_images(&episodes, cx);
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str()
                        || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                        || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
                    {
                        return;
                    }

                    detail.effects.episodes = LoadState::Loaded;
                    detail.episodes_failed = None;
                    detail.episodes = Some(episodes);
                    detail.choose_episode_from_loaded_episodes();
                    if detail.should_reveal_selected_episode()
                        && let Some(index) = detail.selected_episode_index()
                    {
                        detail.episodes_carousel.set_scroll_offset(
                            index as f32 * DETAIL_EPISODE_CARD_STEP_PX,
                            f32::INFINITY,
                        );
                        detail.episodes_carousel.sync_previous_offset();
                    }
                }
            }
            Err(error) => {
                let message: SharedString = format!("加载剧集分集失败：{error}").into();
                if let Some(detail) = self.series_detail.as_mut() {
                    if detail.series_id.as_str() != series_id.as_str()
                        || detail.selected_season_id.as_deref() != Some(season_id.as_str())
                        || detail.episodes_request_season_id.as_deref() != Some(season_id.as_str())
                    {
                        return;
                    }

                    detail.effects.episodes = LoadState::Failed;
                    detail.episodes_failed = Some(message.clone());
                }
                self.push_error_notification(
                    NotificationScope::Detail,
                    DETAIL_EPISODES_NOTIFICATION_KEY,
                    message,
                    cx,
                );
            }
        }

        cx.notify();
    }
}
