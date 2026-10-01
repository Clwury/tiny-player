//! Image scheduling and display paths for the current workspace.
use crate::emby::{
    EmbyImageRequest, EmbyImageType, ImageQuality, ResumeItemImageSource, ResumeItems, UserItem,
    UserItemImageSource, UserItems, UserViews,
};
use crate::home::HomeContent;
use crate::images::controller::{ImageUpdate, ItemImageCommand};
use gpui::{AppContext as _, Context, Task};
use std::{path::Path, sync::Arc, time::Instant};

const RESUME_CARD_IMAGE_MAX_WIDTH: u32 = 800;
const HOME_ITEM_CARD_IMAGE_MAX_WIDTH: u32 = 400;
pub(in crate::home) const EPISODE_CARD_IMAGE_MAX_WIDTH: u32 = 640;

impl HomeContent {
    pub(in crate::home) fn ensure_cached_home_images(&mut self, cx: &mut Context<Self>) {
        if let Some(views) = self.controller.feed_view().views.cloned() {
            self.ensure_user_view_images(&views, cx);
        }

        if let Some(items) = self.controller.feed_view().resume.cloned() {
            self.ensure_resume_item_images(&items, cx);
        }

        let user_view_items = self.controller.latest_items().cloned().collect::<Vec<_>>();
        for items in &user_view_items {
            self.ensure_feed_user_items_images(items, cx);
        }
    }
    pub(in crate::home) fn ensure_user_view_images(
        &mut self,
        views: &UserViews,
        cx: &mut Context<Self>,
    ) {
        for view in &views.items {
            let tag = view
                .image_tags
                .as_ref()
                .and_then(|tags| tags.primary.clone());
            self.ensure_primary_image(view.id.clone(), tag, cx);
        }
    }
    pub(in crate::home) fn ensure_resume_item_images(
        &mut self,
        items: &ResumeItems,
        cx: &mut Context<Self>,
    ) {
        for item in &items.items {
            if let Some(source) = item.image_source() {
                self.ensure_resume_image(source, cx);
            }
        }
    }
    pub(in crate::home) fn ensure_user_items_images(
        &mut self,
        items: &UserItems,
        cx: &mut Context<Self>,
    ) {
        for item in &items.items {
            self.ensure_user_item_image(item.image_source(), cx);
        }
    }
    pub(in crate::home) fn ensure_favorite_items_images(
        &mut self,
        items: &UserItems,
        cx: &mut Context<Self>,
    ) {
        for item in &items.items {
            if item.item_type.as_deref() == Some("Episode") {
                self.ensure_image(favorite_episode_image_request(item), cx);
            } else {
                self.ensure_user_item_image(item.image_source(), cx);
            }
        }
    }
    pub(in crate::home) fn ensure_feed_user_items_images(
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
    pub(in crate::home) fn ensure_primary_image(
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
    pub(in crate::home) fn ensure_user_item_image(
        &mut self,
        source: UserItemImageSource<'_>,
        cx: &mut Context<Self>,
    ) {
        let request = user_item_image_request(source);
        self.ensure_image(request, cx);
    }
    pub(in crate::home) fn ensure_episode_user_item_image(
        &mut self,
        source: UserItemImageSource<'_>,
        cx: &mut Context<Self>,
    ) {
        let request = episode_user_item_image_request(source);
        self.ensure_image(request, cx);
    }
    pub(in crate::home) fn ensure_resume_image(
        &mut self,
        source: ResumeItemImageSource<'_>,
        cx: &mut Context<Self>,
    ) {
        let request = resume_image_request(source);
        self.ensure_image(request, cx);
    }
    pub(in crate::home) fn ensure_image(
        &mut self,
        request: EmbyImageRequest,
        cx: &mut Context<Self>,
    ) {
        self.images.ensure_image(request, Instant::now());
        self.start_queued_image_loads(cx);
    }
    pub(in crate::home) fn start_queued_image_loads(&mut self, cx: &mut Context<Self>) {
        for job in self.images.start_queued_jobs() {
            self.load_item_image(job, cx);
        }
    }
    pub(in crate::home) fn load_item_image(
        &mut self,
        command: ItemImageCommand,
        cx: &mut Context<Self>,
    ) {
        let repository = self.image_repository.clone();
        let task_image = command.image.clone();
        let task = cx.background_spawn(async move { repository.load(&task_image) });
        self.await_item_image(command, task, cx);
    }
    pub(in crate::home) fn await_item_image(
        &mut self,
        command: ItemImageCommand,
        task: Task<anyhow::Result<std::path::PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        let key = command.image.key.clone();
        let handle = cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_item_image(command, result, cx))
                .ok();
        });
        self.image_effects.entry(key).or_default().replace(handle);
    }
    pub(in crate::home) fn finish_item_image(
        &mut self,
        command: ItemImageCommand,
        result: anyhow::Result<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let update =
            self.images
                .finish_job(&command, result, &self.request_identity(), Instant::now());
        if update == ImageUpdate::Ignored {
            return;
        }
        self.image_effects.remove(&command.image.key);
        self.start_queued_image_loads(cx);
        if update == ImageUpdate::Ready {
            self.layout.content_changed();
            cx.notify();
        }
    }
    pub(in crate::home) fn image_path_for_primary_image(
        &self,
        item_id: &str,
        primary_tag: Option<&str>,
    ) -> Option<Arc<Path>> {
        self.images.path_for_source(
            item_id,
            EmbyImageType::Primary,
            primary_tag,
            Some(640),
            ImageQuality::DEFAULT,
        )
    }
    pub(in crate::home) fn image_path_for_resume_image(
        &self,
        source: ResumeItemImageSource<'_>,
    ) -> Option<Arc<Path>> {
        self.images.path_for_source(
            source.item_id,
            source.image_type,
            Some(source.tag),
            Some(RESUME_CARD_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        )
    }
    pub(in crate::home) fn image_path_for_user_item(&self, item: &UserItem) -> Option<Arc<Path>> {
        let source = item.image_source();
        self.images.path_for_source(
            source.item_id,
            source.image_type,
            source.tag,
            Some(HOME_ITEM_CARD_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        )
    }
    pub(in crate::home) fn image_path_for_episode_user_item(
        &self,
        item: &UserItem,
    ) -> Option<Arc<Path>> {
        let source = item.episode_image_source();
        self.images.path_for_source(
            source.item_id,
            source.image_type,
            source.tag,
            Some(RESUME_CARD_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        )
    }
    pub(in crate::home) fn image_path_for_favorite_episode(
        &self,
        item: &UserItem,
    ) -> Option<Arc<Path>> {
        self.image_path_for_request(&favorite_episode_image_request(item))
    }
    pub(in crate::home) fn image_path_for_request(
        &self,
        request: &EmbyImageRequest,
    ) -> Option<Arc<Path>> {
        self.images.path_for_request(request)
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

fn favorite_episode_image_request(item: &UserItem) -> EmbyImageRequest {
    EmbyImageRequest::primary(
        item.id.clone(),
        item.primary_image_tag().map(str::to_string),
    )
    .with_max_width(EPISODE_CARD_IMAGE_MAX_WIDTH)
    .with_quality(ImageQuality::DEFAULT)
}

fn resume_image_request(source: ResumeItemImageSource<'_>) -> EmbyImageRequest {
    EmbyImageRequest::new(source.item_id, source.image_type)
        .with_tag(Some(source.tag.to_string()))
        .with_max_width(RESUME_CARD_IMAGE_MAX_WIDTH)
        .with_quality(ImageQuality::DEFAULT)
}

#[cfg(test)]
mod tests;
