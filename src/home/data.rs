use std::path::PathBuf;

use gpui::{AppContext, Context};

use crate::{
    emby::{
        EmbyImageRequest, ImageQuality, ResumeItemImageSource, ResumeItems, SortOrder, UserItem,
        UserItems, UserViews,
    },
    image_cache::{self, CachedImageKey},
};

use super::HomeContent;

const RESUME_CARD_IMAGE_MAX_WIDTH: u32 = 800;
const HOME_ITEM_CARD_IMAGE_MAX_WIDTH: u32 = 400;
const HOME_ITEM_PAGE_LIMIT: u32 = 30;

impl HomeContent {
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
            }
            Err(error) => {
                self.home_effects.resume_items = super::LoadState::Failed;
                self.resume_items_failed = Some(format!("加载继续观看失败：{error}").into());
            }
        }

        cx.notify();
    }

    fn load_user_view_items_for_views(&mut self, views: &UserViews, cx: &mut Context<Self>) {
        for view in &views.items {
            let row = self
                .user_view_items_rows
                .entry(view.id.clone())
                .or_default();
            if row.items.is_some() || row.loading || row.failed.is_some() {
                continue;
            }

            row.loading = true;
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
        let Some(key) = CachedImageKey::from_request(&self.current_server, &request) else {
            return;
        };

        if self.image_paths.contains_key(&key)
            || self.images_loading.contains(&key)
            || self.images_failed.contains(&key)
        {
            return;
        }

        match image_cache::cached_image_exists(&key) {
            Ok(Some(path)) => {
                self.image_paths.insert(key, path);
            }
            Ok(None) => {
                self.load_item_image(request, key, cx);
            }
            Err(_) => {
                self.images_failed.insert(key);
            }
        }
    }

    fn load_item_image(
        &mut self,
        request: EmbyImageRequest,
        key: CachedImageKey,
        cx: &mut Context<Self>,
    ) {
        self.images_loading.insert(key.clone());
        let server = self.current_server.clone();
        let task_key = key.clone();
        let client = self.emby_client.clone();
        let task = cx.background_spawn(async move {
            let bytes = client.item_image(&server, &request)?;
            image_cache::write_cached_image(&task_key, &bytes)
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
        key: CachedImageKey,
        result: anyhow::Result<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.images_loading.remove(&key);

        match result {
            Ok(path) => {
                self.images_failed.remove(&key);
                self.image_paths.insert(key, path);
            }
            Err(_) => {
                self.images_failed.insert(key);
            }
        }

        cx.notify();
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

    fn image_path_for_request(&self, request: &EmbyImageRequest) -> Option<PathBuf> {
        let key = CachedImageKey::from_request(&self.current_server, request)?;
        self.image_paths.get(&key).cloned()
    }
}

fn resume_image_request(source: ResumeItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(Some(source.tag.to_string()))
        .with_max_width(RESUME_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}
