use std::{collections::HashMap, path::PathBuf, time::Duration};

use gpui::{AppContext, Context, Timer};

use crate::{
    emby::{
        EmbyImageRequest, ImageQuality, ResumeItemImageSource, ResumeItems, SortOrder, UserItem,
        UserItems, UserViews,
    },
    images::{
        cache::{self as image_cache},
        loader::ImageLoadJob,
    },
};

use super::{HomeContent, cache as home_cache};

const RESUME_CARD_IMAGE_MAX_WIDTH: u32 = 800;
const HOME_ITEM_CARD_IMAGE_MAX_WIDTH: u32 = 400;
const HOME_ITEM_PAGE_LIMIT: u32 = 16;
const HOME_SNAPSHOT_SAVE_DEBOUNCE: Duration = Duration::from_millis(450);
const HOME_CACHED_IMAGE_ENSURE_DELAY: Duration = Duration::from_millis(16);

impl HomeContent {
    pub(super) fn load_home_snapshot_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.home_snapshot.can_start() {
            return;
        }

        self.home_effects.home_snapshot = super::LoadState::Loading;
        let server = self.current_server.clone();
        let task = cx.background_spawn(async move { home_cache::load_snapshot(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_home_snapshot(result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_home_snapshot(
        &mut self,
        result: anyhow::Result<Option<home_cache::HomeSnapshot>>,
        cx: &mut Context<Self>,
    ) {
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
            self.user_views = Some(section.data);
            self.user_views_failed = None;
        }

        if let Some(section) = snapshot.resume_items
            && self.resume_items.is_none()
        {
            self.resume_items = Some(section.data);
            self.resume_items_failed = None;
        }

        for (view_id, section) in snapshot.user_view_items {
            let row = self.user_view_items_rows.entry(view_id).or_default();
            if row.items.is_none() {
                row.items = Some(section.data);
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
            self.ensure_user_items_images(items, cx);
        }
    }

    pub(super) fn load_user_views_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.user_views.can_start() {
            return;
        }

        self.home_effects.user_views = super::LoadState::Loading;
        cx.notify();
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move { client.user_views(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_user_views(result, cx))
                .ok();
        })
        .detach();
    }

    pub(super) fn load_resume_items_if_needed(&mut self, cx: &mut Context<Self>) {
        if !self.home_effects.resume_items.can_start() {
            return;
        }

        self.home_effects.resume_items = super::LoadState::Loading;
        cx.notify();
        let server = self.current_server.clone();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move { client.resume_items(&server) });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_resume_items(result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_user_views(&mut self, result: anyhow::Result<UserViews>, cx: &mut Context<Self>) {
        match result {
            Ok(views) => {
                self.home_effects.user_views = super::LoadState::Loaded;
                self.user_views_failed = None;
                self.ensure_user_view_images(&views, cx);
                self.load_user_view_items_for_views(&views, cx);
                self.user_views = Some(views);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                self.home_effects.user_views = super::LoadState::Failed;
                self.user_views_failed = Some(format!("加载首页失败：{error}").into());
            }
        }

        cx.notify();
    }

    fn finish_resume_items(&mut self, result: anyhow::Result<ResumeItems>, cx: &mut Context<Self>) {
        match result {
            Ok(items) => {
                self.home_effects.resume_items = super::LoadState::Loaded;
                self.resume_items_failed = None;
                self.ensure_resume_item_images(&items, cx);
                self.resume_items = Some(items);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                self.home_effects.resume_items = super::LoadState::Failed;
                self.resume_items_failed = Some(format!("加载继续观看失败：{error}").into());
            }
        }

        cx.notify();
    }

    pub(super) fn load_user_view_items_for_views(
        &mut self,
        views: &UserViews,
        cx: &mut Context<Self>,
    ) {
        for view in &views.items {
            let row = self
                .user_view_items_rows
                .entry(view.id.clone())
                .or_default();
            if row.loading {
                continue;
            }

            row.loading = true;
            row.failed = None;
            let server = self.current_server.clone();
            let client = self.emby_client.clone();
            let view_id = view.id.clone();
            let task_view_id = view_id.clone();
            let task = cx.background_spawn(async move {
                client.user_items(
                    &server,
                    &task_view_id,
                    0,
                    HOME_ITEM_PAGE_LIMIT,
                    SortOrder::Descending,
                )
            });

            cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| {
                    page.finish_user_view_items(view_id, result, cx)
                })
                .ok();
            })
            .detach();
        }
    }

    fn finish_user_view_items(
        &mut self,
        view_id: String,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(items) => {
                self.ensure_user_items_images(&items, cx);
                let row = self.user_view_items_rows.entry(view_id).or_default();
                row.loading = false;
                row.failed = None;
                row.items = Some(items);
                self.schedule_home_snapshot_save(cx);
            }
            Err(error) => {
                let row = self.user_view_items_rows.entry(view_id).or_default();
                row.loading = false;
                row.failed = Some(format!("加载媒体库内容失败：{error}").into());
            }
        }

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

    fn ensure_user_items_images(&mut self, items: &UserItems, cx: &mut Context<Self>) {
        for item in &items.items {
            self.ensure_user_item_image(
                item.id.clone(),
                item.primary_image_tag().map(ToString::to_string),
                cx,
            );
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

    fn ensure_user_item_image(
        &mut self,
        item_id: String,
        primary_tag: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let request = EmbyImageRequest::primary(item_id, primary_tag)
            .with_max_width(HOME_ITEM_CARD_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT);
        self.ensure_image(request, cx);
    }

    fn ensure_resume_image(&mut self, source: ResumeItemImageSource<'_>, cx: &mut Context<Self>) {
        let request = resume_image_request(source);
        self.ensure_image(request, cx);
    }

    fn ensure_image(&mut self, request: EmbyImageRequest, cx: &mut Context<Self>) {
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
            page.update(cx, |page, cx| page.finish_item_image(key, result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_item_image(
        &mut self,
        key: crate::images::cache::CachedImageKey,
        result: anyhow::Result<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.image_loader.finish_job(key, result);
        self.start_queued_image_loads(cx);
        cx.notify();
    }

    fn schedule_home_snapshot_save(&mut self, cx: &mut Context<Self>) {
        self.snapshot_save_generation = self.snapshot_save_generation.wrapping_add(1);
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
        let user_view_items = self
            .user_view_items_rows
            .iter()
            .filter_map(|(view_id, row)| {
                row.items
                    .as_ref()
                    .cloned()
                    .map(|items| (view_id.clone(), items))
            })
            .collect::<HashMap<_, _>>();

        home_cache::HomeSnapshot::new(
            &self.current_server,
            self.user_views.clone(),
            self.resume_items.clone(),
            user_view_items,
        )
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
        let request = EmbyImageRequest::primary(
            item.id.clone(),
            item.primary_image_tag().map(ToString::to_string),
        )
        .with_max_width(HOME_ITEM_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT);
        self.image_path_for_request(&request)
    }

    fn image_path_for_request(&self, request: &EmbyImageRequest) -> Option<std::path::PathBuf> {
        self.image_loader
            .path_for_request(&self.current_server, request)
    }
}

fn resume_image_request(source: ResumeItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(Some(source.tag.to_string()))
        .with_max_width(RESUME_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}
