use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui::{AppContext, Context, Timer, point, px};

use crate::{
    emby::{
        EmbyImageRequest, ImageQuality, ResumeItemImageSource, ResumeItems, UserItem,
        UserItemImageSource, UserItems, UserViews,
    },
    images::{
        cache::{self as image_cache},
        loader::ImageLoadJob,
    },
};

use super::{
    HomeContent, WorkspaceIdentity, cache as home_cache,
    library::{is_supported_view, latest_item_types},
};

const RESUME_CARD_IMAGE_MAX_WIDTH: u32 = 800;
const HOME_ITEM_CARD_IMAGE_MAX_WIDTH: u32 = 400;
const HOME_ITEM_PAGE_LIMIT: u32 = 30;
const HOME_LATEST_CONCURRENCY: usize = 4;
const HOME_SNAPSHOT_SAVE_DEBOUNCE: Duration = Duration::from_millis(450);
const HOME_CACHED_IMAGE_ENSURE_DELAY: Duration = Duration::from_millis(16);

impl HomeContent {
    pub(super) fn load_home_snapshot_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.home_snapshot.can_start() {
            return;
        }

        self.home_effects.home_snapshot = super::LoadState::Loading;
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let generation = self.home_refresh_generation;
        let task = cx.background_spawn(async move { home_cache::load_snapshot(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_home_snapshot(identity, generation, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_home_snapshot(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        result: anyhow::Result<Option<home_cache::HomeSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) || generation != self.home_refresh_generation {
            return;
        }
        match result {
            Ok(Some(snapshot)) => {
                self.home_effects.home_snapshot = super::LoadState::Loaded;
                self.hydrate_home_snapshot(snapshot);
                self.schedule_cached_home_images_ensure(cx);
                self.schedule_home_network_refresh(cx);
            }
            Ok(None) => {
                self.home_effects.home_snapshot = super::LoadState::Loaded;
                self.load_home_network_effects(cx);
            }
            Err(error) => {
                self.home_effects.home_snapshot = super::LoadState::Failed;
                tracing::debug!(%error, "failed to load Home snapshot");
                self.load_home_network_effects(cx);
            }
        }

        cx.notify();
    }

    fn hydrate_home_snapshot(&mut self, snapshot: home_cache::HomeSnapshot) {
        if let Some(section) = snapshot.user_views
            && self.user_views.is_none()
        {
            let mut views = section.data;
            views.items.retain(is_supported_view);
            views.total_record_count = views.items.len() as u32;
            self.user_views = Some(views);
            self.user_views_failed = None;
        }

        if let Some(section) = snapshot.resume_items
            && self.resume_items.is_none()
        {
            let mut items = section.data;
            items.items.retain(|item| {
                !item.id.trim().is_empty()
                    && matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
            });
            self.resume_items = Some(items);
            self.resume_items_failed = None;
        }

        for (view_id, section) in snapshot.latest_items_by_view {
            let row = self.user_view_items_rows.entry(view_id).or_default();
            if row.items.is_none() {
                let mut items = section.data;
                items.items.retain(|item| {
                    !item.id.trim().is_empty()
                        && matches!(
                            item.item_type.as_deref(),
                            Some("Movie" | "Series" | "Episode")
                        )
                        && (item.item_type.as_deref() != Some("Episode")
                            || item
                                .series_id
                                .as_deref()
                                .is_some_and(|id| !id.trim().is_empty()))
                });
                items.total_record_count = items.items.len() as u32;
                row.items = Some(items);
                row.failed = None;
            }
        }
    }

    fn schedule_cached_home_images_ensure(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |page, cx| {
            Timer::after(HOME_CACHED_IMAGE_ENSURE_DELAY).await;
            page.update(cx, |page, cx| {
                page.ensure_cached_home_images(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn schedule_home_network_refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |page, cx| {
            Timer::after(HOME_CACHED_IMAGE_ENSURE_DELAY).await;
            page.update(cx, |page, cx| {
                page.load_home_network_effects(cx);
            })
            .ok();
        })
        .detach();
    }

    fn load_home_network_effects(&mut self, cx: &mut Context<Self>) {
        self.load_user_views_if_needed(cx);
        self.load_resume_items_if_needed(cx);
    }

    pub(super) fn refresh_home_content(&mut self, cx: &mut Context<Self>) {
        if self.authentication_error.is_some() {
            return;
        }

        self.home_refresh_generation = self.home_refresh_generation.wrapping_add(1);
        self.invalidate_pending_home_snapshot_save();
        self.home_effects.home_snapshot = super::LoadState::Loaded;
        self.home_effects.user_views = super::LoadState::Idle;
        self.home_effects.resume_items = super::LoadState::Idle;
        self.user_views = None;
        self.user_views_failed = None;
        self.user_views_carousel = Default::default();
        self.resume_items = None;
        self.resume_items_failed = None;
        self.resume_detail_failed = None;
        self.resume_action_failed = None;
        self.resume_item_context_menu = None;
        self.resume_items_carousel = Default::default();
        self.user_view_items_rows.clear();
        self.latest_queue.clear();
        self.home_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.load_home_network_effects(cx);
        cx.notify();
    }

    fn ensure_cached_home_images(&mut self, cx: &mut Context<Self>) {
        if let Some(views) = self.user_views.clone() {
            self.ensure_user_view_images(&views, cx);
        }

        if let Some(items) = self.resume_items.clone() {
            self.ensure_resume_item_images(&items, cx);
        }

        let user_view_items = self
            .user_view_items_rows
            .values()
            .filter_map(|row| row.items.clone())
            .collect::<Vec<_>>();
        for items in &user_view_items {
            self.ensure_feed_user_items_images(items, cx);
        }
    }

    pub(super) fn load_user_views_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.user_views.can_start() {
            return;
        }

        self.home_effects.user_views = super::LoadState::Loading;
        self.user_views_failed = None;
        cx.notify();
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let generation = self.home_refresh_generation;
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move { client.user_views(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_user_views(identity, generation, result, cx)
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn load_resume_items_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.resume_items.can_start() {
            return;
        }

        self.home_effects.resume_items = super::LoadState::Loading;
        self.resume_items_failed = None;
        cx.notify();
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let generation = self.home_refresh_generation;
        let user_data_revision = self.user_data_request_revision();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move { client.resume_items(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_resume_items(identity, generation, user_data_revision, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_user_views(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        result: anyhow::Result<UserViews>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) || generation != self.home_refresh_generation {
            return;
        }
        match result {
            Ok(mut views) => {
                views.items.retain(is_supported_view);
                views.total_record_count = views.items.len() as u32;
                self.home_effects.user_views = super::LoadState::Loaded;
                self.user_views_failed = None;
                self.ensure_user_view_images(&views, cx);
                self.user_views = Some(views.clone());
                self.load_user_view_items_for_views(&views, cx);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                self.home_effects.user_views = super::LoadState::Failed;
                self.user_views_failed = Some(
                    if self.user_views.is_some() {
                        format!("刷新失败：{error}")
                    } else {
                        format!("加载首页失败：{error}")
                    }
                    .into(),
                );
            }
        }

        cx.notify();
    }

    fn finish_resume_items(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        user_data_revision: u64,
        result: anyhow::Result<ResumeItems>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) || generation != self.home_refresh_generation {
            return;
        }
        match result {
            Ok(mut items) => {
                items.items.retain(|item| {
                    !item.id.trim().is_empty()
                        && matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
                });
                self.absorb_resume_items_user_data(&items, user_data_revision);
                self.home_effects.resume_items = super::LoadState::Loaded;
                self.resume_items_failed = None;
                if self.resume_item_requests.is_empty() {
                    self.resume_action_failed = None;
                }
                self.ensure_resume_item_images(&items, cx);
                self.resume_items = Some(items);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                self.home_effects.resume_items = super::LoadState::Failed;
                self.resume_items_failed = Some(
                    if self.resume_items.is_some() {
                        format!("刷新失败：{error}")
                    } else {
                        format!("加载继续观看失败：{error}")
                    }
                    .into(),
                );
            }
        }

        cx.notify();
    }

    pub(super) fn load_user_view_items_for_views(
        &mut self,
        views: &UserViews,
        cx: &mut Context<Self>,
    ) {
        let generation = self.home_refresh_generation;
        for view in &views.items {
            if latest_item_types(view.collection_type.as_deref()).is_none()
                || self
                    .latest_in_flight
                    .contains(&(view.id.clone(), generation))
                || self.latest_queue.contains(&view.id)
            {
                continue;
            }
            self.latest_queue.push_back(view.id.clone());
        }
        self.pump_latest_queue(cx);
    }

    pub(super) fn retry_latest_items(&mut self, view_id: &str, cx: &mut Context<Self>) {
        if self
            .latest_in_flight
            .contains(&(view_id.to_string(), self.home_refresh_generation))
            || self.latest_queue.iter().any(|id| id == view_id)
        {
            return;
        }
        self.latest_queue.push_front(view_id.to_string());
        self.pump_latest_queue(cx);
    }

    fn pump_latest_queue(&mut self, cx: &mut Context<Self>) {
        while self.latest_in_flight.len() < HOME_LATEST_CONCURRENCY {
            let Some(view_id) = self.latest_queue.pop_front() else {
                break;
            };
            let Some(view) = self
                .user_views
                .as_ref()
                .and_then(|views| views.items.iter().find(|view| view.id == view_id))
                .cloned()
            else {
                continue;
            };
            let Some(item_types) = latest_item_types(view.collection_type.as_deref()) else {
                continue;
            };
            let generation = self.home_refresh_generation;
            if self
                .latest_in_flight
                .contains(&(view_id.clone(), generation))
            {
                continue;
            }
            let row = self
                .user_view_items_rows
                .entry(view_id.clone())
                .or_default();
            if row.loading {
                continue;
            }
            row.loading = true;
            row.failed = None;
            self.latest_in_flight.insert((view_id.clone(), generation));
            let server = self.current_server.clone();
            let identity = self.request_identity();
            let user_data_revision = self.user_data_request_revision();
            let client = self.emby_client.clone();
            let task_view_id = view_id.clone();
            let task = cx.background_spawn(async move {
                client.latest_items(&server, &task_view_id, &item_types, HOME_ITEM_PAGE_LIMIT)
            });

            cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| {
                    page.finish_user_view_items(
                        identity,
                        generation,
                        user_data_revision,
                        view_id,
                        result,
                        cx,
                    )
                })
                .ok();
            })
            .detach();
        }
    }

    fn finish_user_view_items(
        &mut self,
        identity: WorkspaceIdentity,
        generation: u64,
        user_data_revision: u64,
        view_id: String,
        result: anyhow::Result<Vec<UserItem>>,
        cx: &mut Context<Self>,
    ) {
        self.latest_in_flight.remove(&(view_id.clone(), generation));
        if !self.matches_request_identity(&identity) || generation != self.home_refresh_generation {
            self.pump_latest_queue(cx);
            return;
        }
        match result {
            Ok(mut raw_items) => {
                let allowed = self
                    .user_views
                    .as_ref()
                    .and_then(|views| views.items.iter().find(|view| view.id == view_id))
                    .and_then(|view| latest_item_types(view.collection_type.as_deref()))
                    .unwrap_or_default();
                raw_items.retain(|item| {
                    let supported = allowed
                        .iter()
                        .any(|item_type| item.item_type.as_deref() == Some(item_type.as_str()));
                    let routable_episode = item.item_type.as_deref() != Some("Episode")
                        || item
                            .series_id
                            .as_deref()
                            .is_some_and(|id| !id.trim().is_empty());
                    if supported && !routable_episode {
                        tracing::debug!(item_id = %item.id, "skipping Latest episode without SeriesId");
                    }
                    !item.id.trim().is_empty() && supported && routable_episode
                });
                let items = UserItems {
                    total_record_count: raw_items.len() as u32,
                    items: raw_items,
                };
                self.absorb_user_items_user_data(&items, user_data_revision);
                self.ensure_feed_user_items_images(&items, cx);
                let row = self.user_view_items_rows.entry(view_id).or_default();
                row.loading = false;
                row.failed = None;
                row.items = Some(items);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                let row = self.user_view_items_rows.entry(view_id).or_default();
                row.loading = false;
                row.failed = Some(
                    if row.items.is_some() {
                        format!("刷新失败：{error}")
                    } else {
                        format!("加载媒体库内容失败：{error}")
                    }
                    .into(),
                );
            }
        }

        self.pump_latest_queue(cx);
        cx.notify();
    }

    fn ensure_user_view_images(&mut self, views: &UserViews, cx: &mut Context<Self>) {
        for view in &views.items {
            let tag = view
                .image_tags
                .as_ref()
                .and_then(|tags| tags.primary.clone());
            self.ensure_primary_image(view.id.clone(), tag, cx);
        }
    }

    fn ensure_resume_item_images(&mut self, items: &ResumeItems, cx: &mut Context<Self>) {
        for item in &items.items {
            if let Some(source) = item.image_source() {
                self.ensure_resume_image(source, cx);
            }
        }
    }

    pub(super) fn ensure_user_items_images(&mut self, items: &UserItems, cx: &mut Context<Self>) {
        for item in &items.items {
            self.ensure_user_item_image(item.image_source(), cx);
        }
    }

    pub(super) fn ensure_feed_user_items_images(
        &mut self,
        items: &UserItems,
        cx: &mut Context<Self>,
    ) {
        for item in &items.items {
            if item.item_type.as_deref() == Some("Episode") {
                self.ensure_episode_user_item_image(item.episode_image_source(), cx);
            } else {
                self.ensure_user_item_image(item.image_source(), cx);
            }
        }
    }

    fn ensure_primary_image(
        &mut self,
        item_id: String,
        primary_tag: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let request = EmbyImageRequest::primary(item_id, primary_tag)
            .with_max_width(640)
            .with_quality(ImageQuality::DEFAULT);
        self.ensure_image(request, cx);
    }

    fn ensure_user_item_image(&mut self, source: UserItemImageSource<'_>, cx: &mut Context<Self>) {
        let request = user_item_image_request(source);
        self.ensure_image(request, cx);
    }

    fn ensure_episode_user_item_image(
        &mut self,
        source: UserItemImageSource<'_>,
        cx: &mut Context<Self>,
    ) {
        let request = episode_user_item_image_request(source);
        self.ensure_image(request, cx);
    }

    fn ensure_resume_image(&mut self, source: ResumeItemImageSource<'_>, cx: &mut Context<Self>) {
        let request = resume_image_request(source);
        self.ensure_image(request, cx);
    }

    pub(super) fn ensure_image(&mut self, request: EmbyImageRequest, cx: &mut Context<Self>) {
        self.image_loader
            .ensure_image(&self.current_server, request);
        self.start_queued_image_loads(cx);
    }

    fn start_queued_image_loads(&mut self, cx: &mut Context<Self>) {
        for job in self.image_loader.start_queued_jobs() {
            self.load_item_image(job, cx);
        }
    }

    fn load_item_image(&mut self, job: ImageLoadJob, cx: &mut Context<Self>) {
        let server = self.current_server.clone();
        let identity = self.request_identity();
        let client = self.emby_client.clone();
        let key = job.key.clone();
        let task_key = job.key.clone();
        let request = job.request;
        let task = cx.background_spawn(async move {
            let image = client.item_image(&server, &request)?;
            let path = image_cache::write_cached_image(
                &task_key,
                &image.bytes,
                image.content_type.as_deref(),
            )?;
            let _ = image_cache::prune_cache(image_cache::DEFAULT_MAX_CACHE_BYTES);
            Ok(path)
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_item_image(identity, key, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_item_image(
        &mut self,
        identity: WorkspaceIdentity,
        key: crate::images::cache::CachedImageKey,
        result: anyhow::Result<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if !self.matches_request_identity(&identity) {
            return;
        }
        self.image_loader.finish_job(key, result);
        self.start_queued_image_loads(cx);
        cx.notify();
    }

    pub(super) fn schedule_home_snapshot_save(&mut self, cx: &mut Context<Self>) {
        self.snapshot_save_generation = self.snapshot_save_generation.wrapping_add(1);
        if !self.favorite_requests.is_empty() {
            return;
        }
        let generation = self.snapshot_save_generation;

        cx.spawn(async move |page, cx| {
            Timer::after(HOME_SNAPSHOT_SAVE_DEBOUNCE).await;
            page.update(cx, |page, cx| {
                page.flush_home_snapshot_save(generation, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn invalidate_pending_home_snapshot_save(&mut self) {
        self.snapshot_save_generation = self.snapshot_save_generation.wrapping_add(1);
    }

    fn flush_home_snapshot_save(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.snapshot_save_generation != generation {
            return;
        }

        let server = self.current_server.clone();
        let snapshot = self.home_snapshot();
        let task =
            cx.background_spawn(async move { home_cache::save_snapshot(&server, &snapshot) });

        cx.spawn(async move |_, _| {
            let result = task.await;
            if let Err(error) = result {
                tracing::debug!(%error, "failed to save Home snapshot");
            }
        })
        .detach();
    }

    fn home_snapshot(&self) -> home_cache::HomeSnapshot {
        let latest_items_by_view = self
            .user_view_items_rows
            .iter()
            .filter_map(|(view_id, row)| {
                row.items
                    .as_ref()
                    .cloned()
                    .map(|mut items| {
                        self.apply_user_data_overrides(&mut items.items);
                        items
                    })
                    .map(|items| (view_id.clone(), items))
            })
            .collect::<HashMap<_, _>>();

        let mut resume_items = self.resume_items.clone();
        if let Some(items) = resume_items.as_mut() {
            for item in &mut items.items {
                if let Some(data) = self.user_data_overrides.get(&item.id) {
                    item.user_data = Some(data.clone());
                }
            }
        }
        home_cache::HomeSnapshot::new(
            &self.current_server,
            self.user_views.clone(),
            resume_items,
            latest_items_by_view,
        )
    }

    fn apply_user_data_overrides(&self, items: &mut [UserItem]) {
        apply_user_data_overrides(items, &self.user_data_overrides);
    }

    pub(super) fn image_path_for_primary_image(
        &self,
        item_id: &str,
        primary_tag: Option<&str>,
    ) -> Option<PathBuf> {
        let request = EmbyImageRequest::primary(item_id, primary_tag.map(ToString::to_string))
            .with_max_width(640)
            .with_quality(ImageQuality::DEFAULT);
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_resume_image(
        &self,
        source: ResumeItemImageSource<'_>,
    ) -> Option<PathBuf> {
        let request = resume_image_request(source);
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_user_item(&self, item: &UserItem) -> Option<PathBuf> {
        let request = user_item_image_request(item.image_source());
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_episode_user_item(&self, item: &UserItem) -> Option<PathBuf> {
        let request = episode_user_item_image_request(item.episode_image_source());
        self.image_path_for_request(&request)
    }

    pub(super) fn image_path_for_request(
        &self,
        request: &EmbyImageRequest,
    ) -> Option<std::path::PathBuf> {
        self.image_loader
            .path_for_request(&self.current_server, request)
    }
}

fn user_item_image_request(source: UserItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(source.tag.map(ToString::to_string))
        .with_max_width(HOME_ITEM_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}

fn episode_user_item_image_request(source: UserItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(source.tag.map(ToString::to_string))
        .with_max_width(RESUME_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}

fn resume_image_request(source: ResumeItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(Some(source.tag.to_string()))
        .with_max_width(RESUME_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}

fn apply_user_data_overrides(
    items: &mut [UserItem],
    overrides: &HashMap<String, crate::emby::UserItemData>,
) {
    for item in items {
        if let Some(data) = overrides.get(&item.id) {
            item.user_data = Some(data.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emby::UserItemData;

    #[test]
    fn snapshot_items_receive_latest_user_data_overrides() {
        let mut items = vec![
            serde_json::from_value::<UserItem>(serde_json::json!({
                "Id": "movie-1",
                "Name": "电影",
                "Type": "Movie",
                "UserData": { "IsFavorite": false }
            }))
            .unwrap(),
        ];
        let overrides = HashMap::from([(
            "movie-1".to_string(),
            UserItemData {
                is_favorite: true,
                ..UserItemData::default()
            },
        )]);

        apply_user_data_overrides(&mut items, &overrides);

        assert!(items[0].is_favorite());
    }
}
