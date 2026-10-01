//! GPUI delivery handles and accepted feed results for one workspace.
use crate::home::{
    HomeContent, WorkspaceIdentity, cache as home_cache,
    feed::{FeedIntent, FeedRequest, FeedRequestKind, FeedResponse, FeedUpdate},
    notification::{
        HOME_RESUME_ITEMS_NOTIFICATION_KEY, HOME_USER_VIEWS_NOTIFICATION_KEY, NotificationScope,
        latest_items_notification_key,
    },
};
use gpui::{AppContext as _, Context, Task, point, px};
use std::time::Duration;

const HOME_CACHED_IMAGE_ENSURE_DELAY: Duration = Duration::from_millis(16);

/// Home owns delivery handles. Snapshot/views/resume replacement and delayed
/// work cancel on refresh/release. Latest retains at most four retired blocking
/// jobs until completion to preserve the IO limit; tokens suppress stale notices.
#[derive(Debug)]
pub(in crate::home) struct FeedEffects {
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
    pub(in crate::home) fn new(identity: WorkspaceIdentity) -> Self {
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
    pub(in crate::home) fn cancel_refresh(&mut self) {
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
    pub(in crate::home) fn load_home_snapshot_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadSnapshot, cx);
    }
    pub(in crate::home) fn dispatch_feed(&mut self, intent: FeedIntent, cx: &mut Context<Self>) {
        let requests = self.controller.dispatch_feed(intent);
        if !requests.is_empty() {
            self.layout.content_changed();
        }
        self.run_feed_requests(requests, cx);
    }
    pub(in crate::home) fn run_feed_requests(
        &mut self,
        requests: Vec<FeedRequest>,
        cx: &mut Context<Self>,
    ) {
        let mut visible_change = false;
        for request in requests {
            if let Some(key) = feed_notification_key(&request.kind) {
                self.clear_notification(NotificationScope::Home, &key);
                visible_change |= !matches!(request.kind, FeedRequestKind::Latest { .. });
            }
            let server = self.current_server.clone();
            let gateway = self.ports.browsing.clone();
            let persistence = self.snapshot_persistence_adapter();
            let user_data_revision = self.controller.user_data_request_revision();
            let task_request = request.clone();
            let task = cx.background_spawn(async move {
                crate::home::feed::effect::run_feed(
                    gateway.as_ref(),
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
    pub(in crate::home) fn finish_feed(
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
    pub(in crate::home) fn hydrate_home_snapshot(
        &mut self,
        snapshot: home_cache::HomeSnapshot,
        cx: &mut Context<Self>,
    ) {
        self.controller.hydrate_snapshot(snapshot);
        self.layout.content_changed();
        self.restore_track_preferences(cx);
    }
    pub(in crate::home) fn schedule_cached_home_followup(
        &mut self,
        images: bool,
        cx: &mut Context<Self>,
    ) {
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
    pub(in crate::home) fn load_home_network_effects(&mut self, cx: &mut Context<Self>) {
        self.load_user_views_if_needed(cx);
        self.load_resume_items_if_needed(cx);
    }
    pub(in crate::home) fn refresh_home_content(&mut self, cx: &mut Context<Self>) {
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
    pub(in crate::home) fn load_user_views_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadViews, cx);
    }
    pub(in crate::home) fn load_resume_items_if_needed(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::LoadResume, cx);
    }
    pub(in crate::home) fn refresh_feed_resume(&mut self, cx: &mut Context<Self>) {
        self.dispatch_feed(FeedIntent::RefreshResume, cx);
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

#[cfg(test)]
mod tests;
