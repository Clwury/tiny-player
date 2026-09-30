use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{App, AppContext, Context, Task, point, px};

use crate::{
    emby::{
        EmbyImageRequest, EmbyImageType, ImageQuality, ResumeItemImageSource, ResumeItems,
        UserItem, UserItemImageSource, UserItems, UserViews,
    },
    images::controller::{ImageUpdate, ItemImageCommand},
};

use super::{
    HomeContent, WorkspaceIdentity, cache as home_cache,
    feed::{FeedIntent, FeedRequest, FeedRequestKind, FeedResponse, FeedUpdate},
    notification::{
        HOME_RESUME_ITEMS_NOTIFICATION_KEY, HOME_USER_VIEWS_NOTIFICATION_KEY, NotificationScope,
        latest_items_notification_key,
    },
};

const RESUME_CARD_IMAGE_MAX_WIDTH: u32 = 800;
const HOME_ITEM_CARD_IMAGE_MAX_WIDTH: u32 = 400;
pub(super) const EPISODE_CARD_IMAGE_MAX_WIDTH: u32 = 640;
const HOME_CACHED_IMAGE_ENSURE_DELAY: Duration = Duration::from_millis(16);

/// Home owns delivery handles. Snapshot/views/resume replacement and delayed
/// work cancel on refresh/release. Latest retains at most four retired blocking
/// jobs until completion to preserve the IO limit; tokens suppress stale notices.
#[derive(Debug)]
pub(super) struct FeedEffects {
    snapshot: crate::effects::EffectHandle<Task<()>>,
    views: crate::effects::EffectHandle<Task<()>>,
    resume: crate::effects::EffectHandle<Task<()>>,
    latest: Vec<(
        crate::effects::RequestToken,
        crate::effects::EffectHandle<Task<()>>,
    )>,
    cached_images: crate::effects::RequestSlot,
    cached_images_task: crate::effects::EffectHandle<Task<()>>,
    network_refresh: crate::effects::RequestSlot,
    network_refresh_task: crate::effects::EffectHandle<Task<()>>,
}

impl FeedEffects {
    pub(super) fn new(identity: WorkspaceIdentity) -> Self {
        use crate::effects::{RequestScope, RequestSlot};
        Self {
            snapshot: Default::default(),
            views: Default::default(),
            resume: Default::default(),
            latest: Vec::new(),
            cached_images: RequestSlot::new(RequestScope::HomeCachedImages, identity.clone()),
            cached_images_task: Default::default(),
            network_refresh: RequestSlot::new(RequestScope::HomeNetworkRefresh, identity),
            network_refresh_task: Default::default(),
        }
    }
    fn cancel_refresh(&mut self) {
        self.snapshot.cancel();
        self.views.cancel();
        self.resume.cancel();
        self.cached_images.invalidate();
        self.cached_images_task.cancel();
        self.network_refresh.invalidate();
        self.network_refresh_task.cancel();
    }
}

impl HomeContent {
    pub(super) fn load_home_snapshot_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadSnapshot, cx);
    }

    fn dispatch_feed(&mut self, intent: FeedIntent, cx: &mut Context<Self>) {
        let requests = self.controller.dispatch_feed(intent);
        if !requests.is_empty() {
            self.layout.content_changed();
        }
        self.run_feed_requests(requests, cx);
    }

    fn run_feed_requests(&mut self, requests: Vec<FeedRequest>, cx: &mut Context<Self>) {
        let mut visible_change = false;
        for request in requests {
            if let Some(key) = feed_notification_key(&request.kind) {
                self.clear_notification(NotificationScope::Home, &key);
                visible_change |= !matches!(request.kind, FeedRequestKind::Latest { .. });
            }
            let server = self.current_server.clone();
            let gateway = super::adapter::EmbyHomeGateway {
                client: self.emby_client.clone(),
                server: server.clone(),
            };
            let persistence = self.snapshot_persistence_adapter();
            let user_data_revision = self.controller.user_data_request_revision();
            let task_request = request.clone();
            let task = cx.background_spawn(async move {
                super::feed::effect::run_feed(
                    &gateway,
                    persistence.as_ref(),
                    &server,
                    &task_request,
                )
            });
            let delivered_request = request.clone();
            let delivery = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| {
                    page.finish_feed(delivered_request, user_data_revision, result, cx)
                })
                .ok();
            });
            match request.kind {
                FeedRequestKind::Snapshot => self.feed_effects.snapshot.replace(delivery),
                FeedRequestKind::Views => self.feed_effects.views.replace(delivery),
                FeedRequestKind::Resume => self.feed_effects.resume.replace(delivery),
                FeedRequestKind::Latest { .. } => {
                    let mut handle = crate::effects::EffectHandle::default();
                    handle.replace(delivery);
                    self.feed_effects.latest.push((request.token, handle));
                }
            }
        }
        if visible_change {
            cx.notify();
        }
    }

    fn finish_feed(
        &mut self,
        request: FeedRequest,
        user_data_revision: u64,
        response: FeedResponse,
        cx: &mut Context<Self>,
    ) {
        if !request.token.is_for(&self.request_identity()) {
            return;
        }
        self.feed_effects
            .latest
            .retain(|(token, _)| token != &request.token);
        let update = self.controller.complete_feed(
            &request,
            user_data_revision,
            response,
            &self.request_identity(),
        );
        let accepted = !matches!(update, FeedUpdate::Ignored);
        if accepted {
            self.layout.content_changed();
        }
        match update {
            FeedUpdate::Ignored => {}
            FeedUpdate::Snapshot(Some(snapshot)) => {
                self.hydrate_home_snapshot(*snapshot, cx);
                self.schedule_cached_home_followup(true, cx);
                self.schedule_cached_home_followup(false, cx);
            }
            FeedUpdate::Snapshot(None) => self.load_home_network_effects(cx),
            FeedUpdate::Views => {
                let views = self
                    .controller
                    .feed_view()
                    .views
                    .cloned()
                    .expect("views committed");
                self.ensure_user_view_images(&views, cx);
                self.dispatch_feed(FeedIntent::LoadLatest, cx);
                self.schedule_home_snapshot_save(cx);
            }
            FeedUpdate::Resume => {
                let items = self
                    .controller
                    .feed_view()
                    .resume
                    .cloned()
                    .expect("resume committed");
                self.ensure_resume_item_images(&items, cx);
                self.schedule_home_snapshot_save(cx);
            }
            FeedUpdate::Latest(view_id) => {
                let items = self
                    .controller
                    .latest_row(&view_id)
                    .items
                    .cloned()
                    .expect("latest committed");
                self.ensure_feed_user_items_images(&items, cx);
                self.schedule_home_snapshot_save(cx);
            }
            FeedUpdate::Failed { kind, message } => {
                if let Some(key) = feed_notification_key(&kind) {
                    self.push_error_notification(NotificationScope::Home, key, message, cx);
                } else {
                    tracing::debug!("failed to load Home snapshot");
                    self.load_home_network_effects(cx);
                }
                if matches!(request.kind, FeedRequestKind::Latest { .. }) {
                    let requests = self.controller.pump_latest();
                    self.run_feed_requests(requests, cx);
                }
                if matches!(request.kind, FeedRequestKind::Snapshot)
                    && self.sync_track_preferences(cx)
                {
                    self.schedule_home_snapshot_save(cx);
                }
                cx.notify();
                return;
            }
        }
        if accepted {
            if let Some(key) = feed_notification_key(&request.kind) {
                self.clear_notification(NotificationScope::Home, &key);
            }
            if matches!(request.kind, FeedRequestKind::Snapshot) && self.sync_track_preferences(cx)
            {
                self.schedule_home_snapshot_save(cx);
            }
            cx.notify();
        }
        if matches!(request.kind, FeedRequestKind::Latest { .. }) {
            let requests = self.controller.pump_latest();
            self.run_feed_requests(requests, cx);
        }
    }

    pub(super) fn hydrate_home_snapshot(
        &mut self,
        snapshot: home_cache::HomeSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.controller.hydrate_snapshot(snapshot);
        self.layout.content_changed();
        self.restore_track_preferences(cx);
    }

    fn schedule_cached_home_followup(&mut self, images: bool, cx: &mut Context<Self>) {
        let slot = if images {
            &mut self.feed_effects.cached_images
        } else {
            &mut self.feed_effects.network_refresh
        };
        let token = slot.issue();
        let task = cx.spawn(async move |page, cx| {
            cx.background_executor()
                .timer(HOME_CACHED_IMAGE_ENSURE_DELAY)
                .await;
            page.update(cx, |page, cx| {
                if !token.is_for(&page.request_identity()) {
                    return;
                }
                let slot = if images {
                    &mut page.feed_effects.cached_images
                } else {
                    &mut page.feed_effects.network_refresh
                };
                if !slot.commit(&token) {
                    return;
                }
                if images {
                    page.ensure_cached_home_images(cx);
                    cx.notify();
                } else {
                    page.load_home_network_effects(cx);
                }
            })
            .ok();
        });
        if images {
            self.feed_effects.cached_images_task.replace(task);
        } else {
            self.feed_effects.network_refresh_task.replace(task);
        }
    }

    fn load_home_network_effects(&mut self, cx: &mut Context<Self>) {
        self.load_user_views_if_needed(cx);
        self.load_resume_items_if_needed(cx);
    }

    pub(super) fn refresh_home_content(&mut self, cx: &mut Context<Self>) {
        if self.authentication_error.is_some() {
            return;
        }
        self.feed_effects.cancel_refresh();
        self.invalidate_pending_home_snapshot_save();
        self.clear_notifications_for_scope(NotificationScope::Home);
        self.user_views_carousel = Default::default();
        self.resume_items_carousel = Default::default();
        self.latest_carousels.clear();
        self.item_context_menu = None;
        self.home_scroll_handle.set_offset(point(px(0.0), px(0.0)));
        self.dispatch_feed(FeedIntent::Refresh, cx);
        cx.notify();
    }

    fn ensure_cached_home_images(&mut self, cx: &mut Context<Self>) {
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

    pub(super) fn load_user_views_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadViews, cx);
    }

    pub(super) fn load_resume_items_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadResume, cx);
    }

    pub(super) fn refresh_feed_resume(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::RefreshResume, cx);
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

    pub(super) fn ensure_favorite_items_images(
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
        self.images.ensure_image(request, Instant::now());
        self.start_queued_image_loads(cx);
    }

    fn start_queued_image_loads(&mut self, cx: &mut Context<Self>) {
        for job in self.images.start_queued_jobs() {
            self.load_item_image(job, cx);
        }
    }

    fn load_item_image(&mut self, command: ItemImageCommand, cx: &mut Context<Self>) {
        let repository = self.image_repository.clone();
        let task_image = command.image.clone();
        let task = cx.background_spawn(async move { repository.load(&task_image) });
        self.await_item_image(command, task, cx);
    }

    fn await_item_image(
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

    fn finish_item_image(
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

    pub(super) fn snapshot_dirty_key(&self) -> crate::persistence::DirtyKey {
        crate::persistence::DirtyKey::HomeSnapshot(self.request_identity())
    }

    fn snapshot_persistence_adapter(&self) -> Arc<dyn crate::persistence::AppPersistence> {
        Arc::new(crate::persistence::FilePersistence {
            #[cfg(test)]
            settings_path: None,
            #[cfg(test)]
            home_path: self.snapshot_save_path.clone(),
        })
    }

    pub(super) fn schedule_home_snapshot_save(&mut self, cx: &mut Context<Self>) {
        self.submit_home_snapshot(cx);
        if self.controller.favorite_pending() {
            self.invalidate_pending_home_snapshot_save();
        }
    }

    fn submit_home_snapshot(&self, cx: &mut App) {
        #[cfg(test)]
        if self.snapshot_save_path.is_none() {
            return;
        }
        self.persistence.schedule_home(
            self.current_server.clone(),
            self.home_snapshot(),
            self.snapshot_persistence_adapter(),
            cx,
        );
    }

    pub(super) fn invalidate_pending_home_snapshot_save(&mut self) {
        self.persistence.suspend(&self.snapshot_dirty_key());
    }

    pub(super) fn finish_home_snapshot_saves(&mut self, cx: &mut App) -> Task<()> {
        let key = self.snapshot_dirty_key();
        if self.sync_track_preferences(cx) || self.persistence.needs_flush_snapshot(&key) {
            #[cfg(test)]
            if self.snapshot_save_path.is_none() {
                return Task::ready(());
            }
            self.persistence.flush_home_snapshot(
                self.current_server.clone(),
                self.home_snapshot(),
                self.snapshot_persistence_adapter(),
                cx,
            );
        }
        self.persistence.flush(&key, cx);
        self.persistence.drain(cx)
    }

    pub(super) fn home_snapshot(&self) -> home_cache::HomeSnapshot {
        self.controller.snapshot(home_cache::current_unix_time())
    }

    pub(super) fn image_path_for_primary_image(
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

    pub(super) fn image_path_for_resume_image(
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

    pub(super) fn image_path_for_user_item(&self, item: &UserItem) -> Option<Arc<Path>> {
        let source = item.image_source();
        self.images.path_for_source(
            source.item_id,
            source.image_type,
            source.tag,
            Some(HOME_ITEM_CARD_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        )
    }

    pub(super) fn image_path_for_episode_user_item(&self, item: &UserItem) -> Option<Arc<Path>> {
        let source = item.episode_image_source();
        self.images.path_for_source(
            source.item_id,
            source.image_type,
            source.tag,
            Some(RESUME_CARD_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        )
    }

    pub(super) fn image_path_for_favorite_episode(&self, item: &UserItem) -> Option<Arc<Path>> {
        self.image_path_for_request(&favorite_episode_image_request(item))
    }

    pub(super) fn image_path_for_request(&self, request: &EmbyImageRequest) -> Option<Arc<Path>> {
        self.images.path_for_request(request)
    }
}

fn feed_notification_key(kind: &FeedRequestKind) -> Option<String> {
    match kind {
        FeedRequestKind::Snapshot => None,
        FeedRequestKind::Views => Some(HOME_USER_VIEWS_NOTIFICATION_KEY.into()),
        FeedRequestKind::Resume => Some(HOME_RESUME_ITEMS_NOTIFICATION_KEY.into()),
        FeedRequestKind::Latest { view_id, .. } => Some(latest_items_notification_key(view_id)),
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
mod tests {
    use super::*;
    use crate::emby::UserItemData;

    fn page(cx: &mut gpui::TestAppContext) -> gpui::Entity<HomeContent> {
        let server = serde_json::from_value(serde_json::json!({
            "id": "local", "server_id": "remote", "user_id": "user",
            "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "test", "password": "", "added_at_unix": 0
        }))
        .unwrap();
        cx.new(|cx| {
            HomeContent::new(
                server,
                crate::emby::EmbyClient::new("test".into()).unwrap(),
                cx,
            )
        })
    }

    #[gpui::test]
    fn image_runner_drains_the_queue_through_the_port_and_only_notifies_ready_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::images::{controller::ImageController, test_support::FakeItemImages};
        let page = page(cx);
        let repository = Arc::new(FakeItemImages {
            failed: ["one".into()].into(),
            ..Default::default()
        });
        let notifications = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = notifications.clone();
        cx.update(|cx| {
            cx.observe(&page, move |_, _| observed.set(observed.get() + 1))
                .detach()
        });
        cx.run_until_parked();
        page.update(cx, |page, cx| {
            page.images = ImageController::with_limits(
                page.request_identity(),
                1,
                Duration::from_secs(30),
                3,
            );
            page.image_repository = repository.clone();
            for id in ["one", "one", "two", "three"] {
                page.ensure_image(
                    EmbyImageRequest::primary(id, Some("tag".into())).with_max_width(640),
                    cx,
                );
            }
            assert_eq!(page.image_effects.len(), 1);
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 2);
        let requests = repository.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|image| image.request.item_id.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert!(
            requests.iter().all(|image| image.key.max_width == Some(640)
                && image.key.quality == ImageQuality::DEFAULT)
        );
        page.read_with(cx, |page, _| {
            assert!(page.image_effects.is_empty());
            assert!(
                page.image_path_for_primary_image("one", Some("tag"))
                    .is_none()
            );
            assert!(
                page.image_path_for_primary_image("two", Some("tag"))
                    .is_some()
            );
            assert!(
                page.image_path_for_primary_image("three", Some("tag"))
                    .is_some()
            );
            assert!(!page.has_visible_notifications());
        });
    }

    #[gpui::test]
    fn old_image_delivery_keeps_the_current_handle_and_release_cancels_pending_work(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::images::controller::ImageController;
        use std::sync::atomic::{AtomicBool, Ordering};
        let page = page(cx);
        let completed = Arc::new(AtomicBool::new(false));
        page.update(cx, |page, cx| {
            let request = EmbyImageRequest::primary("one", Some("tag".into())).with_max_width(640);
            let mut previous = ImageController::new(page.request_identity());
            previous.ensure_image(request.clone(), Instant::now());
            let old = previous.start_queued_jobs().pop().unwrap();
            page.images.ensure_image(request.clone(), Instant::now());
            let current = page.images.start_queued_jobs().pop().unwrap();
            let key = current.image.key.clone();
            let executor = cx.background_executor().clone();
            let completed = completed.clone();
            let task = cx.background_spawn(async move {
                executor.timer(Duration::from_secs(1)).await;
                completed.store(true, Ordering::SeqCst);
                Ok(std::path::PathBuf::from("/tmp/current.png"))
            });
            page.await_item_image(current, task, cx);
            page.finish_item_image(old.clone(), Ok("/tmp/old.png".into()), cx);
            page.finish_item_image(old, Err(anyhow::anyhow!("late failure")), cx);
            assert!(page.images.path_for_request(&request).is_none());
            assert!(page.images.failure_for_request(&request).is_none());
            assert!(page.image_effects.contains_key(&key));
            assert!(!page.has_visible_notifications());
        });
        cx.run_until_parked();
        cx.update(|_| drop(page));
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(!completed.load(Ordering::SeqCst));
    }

    #[gpui::test]
    fn stale_home_failures_never_notify_or_finish_the_replacement(cx: &mut gpui::TestAppContext) {
        let page = page(cx);
        page.update(cx, |page, cx| {
            let old = page
                .controller
                .dispatch_feed(FeedIntent::LoadViews)
                .pop()
                .unwrap();
            let requests = page.controller.dispatch_feed(FeedIntent::Refresh);
            let new = requests
                .into_iter()
                .find(|request| request.kind == FeedRequestKind::Views)
                .unwrap();
            page.finish_feed(
                old,
                0,
                FeedResponse::Views(Err(anyhow::anyhow!("late"))),
                cx,
            );
            assert!(
                page.controller
                    .test_state()
                    .feed
                    .state
                    .views_load
                    .is_loading()
            );
            assert!(!page.has_visible_notifications());
            page.finish_feed(
                new,
                0,
                FeedResponse::Views(Err(anyhow::anyhow!("current"))),
                cx,
            );
            assert!(
                !page
                    .controller
                    .test_state()
                    .feed
                    .state
                    .views_load
                    .is_loading()
            );
            assert!(page.has_visible_notifications());
        });
    }

    #[gpui::test]
    fn resume_result_keeps_newer_optimistic_data_and_ignores_another_workspace(
        cx: &mut gpui::TestAppContext,
    ) {
        let page = page(cx);
        page.update(cx, |page, cx| {
            let request = page.controller.dispatch_feed(FeedIntent::LoadResume).pop().unwrap();
            page.controller.test_state_mut().user_data.bump("movie");
            page.controller.test_state_mut().user_data.overrides.insert("movie".into(), UserItemData { is_favorite: true, ..Default::default() });
            let items: ResumeItems = serde_json::from_value(serde_json::json!({
                "Items": [{"Id":"movie", "Name":"Movie", "Type":"Movie", "UserData":{"IsFavorite":false}}], "TotalRecordCount":1
            })).unwrap();
            page.finish_feed(request, 0, FeedResponse::Resume(Ok(items)), cx);
            assert!(page.controller.test_state().user_data.overrides["movie"].is_favorite);
            let mut other = super::super::feed::FeedController::new(WorkspaceIdentity {
                user_id: Some("other".into()), ..page.request_identity()
            });
            let wrong = other.dispatch(FeedIntent::LoadResume).pop().unwrap();
            page.finish_feed(wrong, 1, FeedResponse::Resume(Err(anyhow::anyhow!("wrong user"))), cx);
            assert!(page.controller.test_state().feed.state.resume_items_failed.is_none());
            assert!(!page.has_visible_notifications());
            assert_eq!(page.controller.test_state().feed.state.resume_items.as_ref().unwrap().items.len(), 1);
        });
    }

    #[gpui::test]
    fn cancelling_cached_followups_prevents_delayed_network_start(cx: &mut gpui::TestAppContext) {
        let page = page(cx);
        page.update(cx, |page, cx| {
            page.schedule_cached_home_followup(true, cx);
            page.schedule_cached_home_followup(false, cx);
        });
        cx.run_until_parked();
        page.update(cx, |page, _| page.feed_effects.cancel_refresh());
        cx.executor().advance_clock(HOME_CACHED_IMAGE_ENSURE_DELAY);
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.controller.test_state().feed.state.views_load,
                super::super::LoadState::Idle
            );
            assert_eq!(
                page.controller.test_state().feed.state.resume_load,
                super::super::LoadState::Idle
            );
            assert!(!page.has_visible_notifications());
        });
    }

    #[test]
    fn favorite_episodes_request_the_same_primary_cover_size_as_detail_cards() {
        let item: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "episode-1", "Name": "Episode", "Type": "Episode",
            "ImageTags": { "Primary": "primary-tag", "Thumb": "thumb-tag" },
            "BackdropImageTags": ["backdrop-tag"]
        }))
        .unwrap();
        let request = favorite_episode_image_request(&item);
        assert_eq!(request.item_id, "episode-1");
        assert_eq!(request.image_type, EmbyImageType::Primary);
        assert_eq!(request.tag.as_deref(), Some("primary-tag"));
        assert_eq!(request.max_width, Some(640));
    }
}
