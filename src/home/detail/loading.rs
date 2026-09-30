use super::{
    controller::{DetailRequest, DetailResponse, DetailUpdate},
    *,
};
use crate::home::detail::state::detail_binding;
use crate::{effects::DetailResource, home::adapter::EmbyHomeGateway};

fn notification_key(resource: DetailResource) -> &'static str {
    match resource {
        DetailResource::Item => DETAIL_ITEM_NOTIFICATION_KEY,
        DetailResource::Similar => DETAIL_SIMILAR_NOTIFICATION_KEY,
        DetailResource::Seasons => DETAIL_SEASONS_NOTIFICATION_KEY,
        DetailResource::NextUp => DETAIL_NEXT_UP_NOTIFICATION_KEY,
        DetailResource::Episodes => DETAIL_EPISODES_NOTIFICATION_KEY,
        DetailResource::ResumeSources => "detail:resume-video",
    }
}

impl HomeContent {
    pub(super) fn load_media_detail_effects(&mut self, cx: &mut Context<Self>) {
        self.load_detail_resource(DetailResource::ResumeSources, cx);
        self.load_series_media_item_if_needed(cx);
        self.load_detail_resource(DetailResource::Similar, cx);
        if self
            .detail_view()
            .is_some_and(|detail| detail.model.is_series())
        {
            self.load_detail_resource(DetailResource::Seasons, cx);
            if self
                .detail_view()
                .is_some_and(|detail| detail.model.should_load_next_up())
            {
                self.load_detail_resource(DetailResource::NextUp, cx);
            }
            self.load_series_episodes_if_needed(cx);
        }
    }

    pub(in crate::home) fn load_series_media_item_if_needed(&mut self, cx: &mut Context<Self>) {
        self.load_detail_resource(DetailResource::Item, cx);
    }

    pub(in crate::home) fn load_series_episodes_if_needed(&mut self, cx: &mut Context<Self>) {
        self.load_detail_resource(DetailResource::Episodes, cx);
    }

    fn load_detail_resource(&mut self, resource: DetailResource, cx: &mut Context<Self>) {
        if resource != DetailResource::ResumeSources {
            self.clear_notification(NotificationScope::Detail, notification_key(resource));
        }
        let identity = self.request_identity();
        if self.detail_view().is_none() {
            return;
        }
        let Some(request) = self.controller.begin_detail(resource, identity) else {
            return;
        };
        let detail = detail_binding(self.controller.detail_view(), &mut self.detail_resources)
            .expect("started detail request keeps its resources");
        let gateway = EmbyHomeGateway {
            client: self.emby_client.clone(),
            server: self.current_server.clone(),
        };
        let command = request.clone();
        let work =
            cx.background_spawn(async move { super::effect::run_detail(&gateway, &command) });
        let task = cx.spawn(async move |page, cx| {
            let result = work.await;
            page.update(cx, |page, cx| {
                page.finish_detail_request(request, result, cx)
            })
            .ok();
        });
        detail.tasks.entry(resource).or_default().replace(task);
    }

    pub(super) fn finish_detail_request(
        &mut self,
        request: DetailRequest,
        result: anyhow::Result<DetailResponse>,
        cx: &mut Context<Self>,
    ) {
        if self.detail_view().is_none() {
            return;
        }
        let Some(update) =
            self.controller
                .complete_detail(&request, result, &self.request_identity())
        else {
            return;
        };
        let detail = detail_binding(self.controller.detail_view(), &mut self.detail_resources)
            .expect("accepted current detail keeps its presentation resources");
        detail.tasks.remove(&request.resource);
        detail.presentation.apply_change(update.change);
        if update.playback_cancelled {
            detail.playback_task.cancel();
        }
        if update.change.episodes_reset {
            detail.tasks.remove(&DetailResource::Episodes);
        }
        // Only accepted results may allocate images. Clones are response-time
        // snapshots needed to release the state borrow before touching GPUI.
        if update.images {
            self.layout.content_changed();
            match request.resource {
                DetailResource::Item => {
                    let item = detail.model.item.clone();
                    if let Some(item) = item {
                        self.ensure_series_media_item_images(&item, cx);
                    }
                }
                DetailResource::Similar => {
                    let items = detail.model.similar_items.clone();
                    if let Some(items) = items {
                        self.ensure_user_items_images(&items, cx);
                    }
                }
                DetailResource::Episodes => {
                    let episodes = detail.model.episodes.clone();
                    if let Some(episodes) = episodes {
                        self.ensure_series_episode_images(&episodes, cx);
                    }
                }
                _ => {}
            }
        }
        self.apply_detail_update(update, request.resource, cx);
    }

    fn apply_detail_update(
        &mut self,
        update: DetailUpdate,
        resource: DetailResource,
        cx: &mut Context<Self>,
    ) {
        if update.title_changed {
            cx.emit(HomeContentEvent::TitleChanged);
        }
        if update.load_episodes {
            self.load_series_episodes_if_needed(cx);
        }
        if update.retry {
            return;
        }
        if let Some(error) = update.error {
            self.push_error_notification(
                NotificationScope::Detail,
                notification_key(resource),
                error,
                cx,
            );
        } else if resource == DetailResource::ResumeSources {
            self.clear_notification(NotificationScope::Detail, notification_key(resource));
        }
        cx.notify();
    }
}
