use super::*;
use crate::home::detail::binding::detail_binding;

use crate::home::navigation::{HomeRoot, HomeRoute};
use crate::home::{controller::OpenDetailIntent, model::navigation::NavigationChange};

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;

impl HomeContent {
    pub(in super::super) fn open_media_detail_by_id(
        &mut self,
        item_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.controller.user_item_by_id(&item_id) else {
            return;
        };
        self.open_media_detail(&item, cx);
    }

    pub(in super::super) fn open_media_detail(&mut self, item: &UserItem, cx: &mut Context<Self>) {
        self.open_detail_target(OpenDetailIntent::Item(item), cx);
    }

    pub(in super::super) fn open_resume_item_detail(
        &mut self,
        item: &ResumeItem,
        cx: &mut Context<Self>,
    ) {
        self.open_detail_target(OpenDetailIntent::Resume(item), cx);
    }

    fn open_detail_target(&mut self, intent: OpenDetailIntent<'_>, cx: &mut Context<Self>) {
        let change = match self
            .controller
            .dispatch_open_detail(intent, self.request_identity())
        {
            Ok(Some(change)) => change,
            Ok(None) => return,
            Err(message) => {
                if let Some(scope) = self.current_notification_scope() {
                    self.push_error_notification(
                        scope,
                        HOME_RESUME_DETAIL_NOTIFICATION_KEY,
                        message,
                        cx,
                    );
                }
                cx.notify();
                return;
            }
        };
        self.apply_open_detail(change, cx);
    }

    pub(in super::super) fn open_resume_item_detail_by_id(
        &mut self,
        item_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.controller.resume_item_by_id(&item_id) else {
            return;
        };
        self.open_resume_item_detail(&item, cx);
    }

    #[cfg(test)]
    pub(super) fn open_detail_state(
        &mut self,
        detail: super::controller::DetailController,
        cx: &mut Context<Self>,
    ) {
        let change = self.controller.open_detail(detail);
        self.apply_open_detail(change, cx);
    }

    fn apply_open_detail(&mut self, change: NavigationChange, cx: &mut Context<Self>) {
        self.item_context_menu = None;
        self.clear_all_notifications();
        let id = self
            .controller
            .detail_view()
            .expect("opened detail is active")
            .id;
        self.detail_resources.entry(id).or_default();
        super::binding::apply_navigation_change(change, &mut self.detail_resources);
        self.load_media_detail_effects(cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in super::super) fn close_series_detail(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_movie_overview(window, cx);
        let change = self
            .controller
            .dispatch_navigation(crate::home::controller::NavigationIntent::Back);
        if !change.changed {
            return;
        }
        self.clear_notifications_for_scope(NotificationScope::Detail);
        super::binding::apply_navigation_change(change, &mut self.detail_resources);
        if self.controller.detail_view().is_some() {
            self.load_media_detail_effects(cx);
        }
        if matches!(
            self.controller.route(),
            HomeRoute::Root(HomeRoot::Favorites) | HomeRoute::FavoriteItems { .. }
        ) {
            self.enter_favorites_if_needed(cx);
        }
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in super::super) fn close_series_detail_select(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        else {
            return;
        };
        if detail.presentation.open_select.take().is_some() {
            cx.notify();
        }
    }
}
