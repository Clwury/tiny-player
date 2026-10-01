//! GPUI intent binding and cancellable delivery for this feature.
use super::{controller, effect};
#[cfg(test)]
use crate::emby::VideoItemType;
use crate::emby::{SortOrder, UserItems, UserItemsSort, UserView};
use crate::home::{
    HomeContent, HomeContentEvent,
    navigation::HomeRoute,
    notification::{
        NotificationScope, library_initial_notification_key, library_load_more_notification_key,
        library_refresh_notification_key,
    },
};
#[cfg(test)]
use controller::LibraryController;
use controller::{LibraryFailure, LibraryIntent, LibraryRequest, LibraryTransition};
use gpui::{AppContext as _, Context};

impl HomeContent {
    pub(in crate::home) fn open_library_for_view(
        &mut self,
        view: &UserView,
        cx: &mut Context<Self>,
    ) {
        let Some((change, transition)) = self.controller.open_library(view) else {
            return;
        };
        self.item_context_menu = None;
        self.library_resources.entry(view.id.clone()).or_default();
        crate::home::detail::binding::apply_navigation_change(change, &mut self.detail_resources);
        self.clear_library_notifications(&view.id);
        self.apply_library_transition(&view.id, transition, cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in crate::home) fn open_library_by_id(&mut self, view_id: &str, cx: &mut Context<Self>) {
        let view = self.controller.user_view(view_id).cloned();
        if let Some(view) = view {
            self.open_library_for_view(&view, cx);
        }
    }

    pub(in crate::home) fn toggle_current_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.controller.route() else {
            return;
        };
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };

        state.presentation.sort_menu_open = !state.presentation.sort_menu_open;
        cx.notify();
    }

    pub(in crate::home) fn close_current_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.controller.route() else {
            return;
        };
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };
        if state.presentation.sort_menu_open {
            state.presentation.sort_menu_open = false;
            cx.notify();
        }
    }

    pub(in crate::home) fn select_library_sort_by(
        &mut self,
        view_id: String,
        sort_by: UserItemsSort,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_library(&view_id, LibraryIntent::SortBy(sort_by), cx);
    }
    pub(in crate::home) fn select_library_sort_order(
        &mut self,
        view_id: String,
        sort_order: SortOrder,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_library(&view_id, LibraryIntent::SortOrder(sort_order), cx);
    }
    pub(in crate::home) fn auto_load_more_library(
        &mut self,
        view_id: &str,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.controller.route(), HomeRoute::Library {view_id: current, ..} if current == view_id)
        {
            return;
        }
        self.dispatch_library(view_id, LibraryIntent::LoadMore { automatic: true }, cx);
    }
    fn dispatch_library(&mut self, view_id: &str, intent: LibraryIntent, cx: &mut Context<Self>) {
        let Some(transition) = self.controller.dispatch_library(view_id, intent) else {
            return;
        };
        self.apply_library_transition(view_id, transition, cx);
    }
    fn apply_library_transition(
        &mut self,
        view_id: &str,
        transition: LibraryTransition,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };
        if transition.close_menu {
            state.presentation.sort_menu_open = false;
        }
        if transition.cancel {
            state.effect.cancel();
        }
        if let Some(request) = transition.request {
            if request.initial {
                self.clear_library_notifications(view_id);
            } else {
                self.clear_notification(
                    NotificationScope::Library,
                    &library_load_more_notification_key(view_id),
                );
            }
            let gateway = self.ports.browsing.clone();
            let task_request = request.clone();
            let task =
                cx.background_spawn(
                    async move { effect::run_library(gateway.as_ref(), &task_request) },
                );
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| page.finish_library(request, result, cx))
                    .ok();
            });
            if let Some(state) = self.library_resources.get_mut(view_id) {
                state.effect.replace(handle);
            }
        }
        if transition.notify {
            cx.notify();
        }
    }
    fn finish_library(
        &mut self,
        request: LibraryRequest,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let identity = self.request_identity();
        let Some(update) = self
            .controller
            .complete_library(&request, result, &identity)
        else {
            return;
        };
        if let Some(items) = update.images {
            self.layout.content_changed();
            self.ensure_user_items_images(&items, cx);
        }
        if let Some(failure) = update.failure {
            let (key, message) = match failure {
                LibraryFailure::Initial(error) => (
                    library_initial_notification_key(&request.view_id),
                    format!("加载媒体库失败：{error}"),
                ),
                LibraryFailure::Refresh(error) => {
                    (library_refresh_notification_key(&request.view_id), error)
                }
                LibraryFailure::More(error) => (
                    library_load_more_notification_key(&request.view_id),
                    format!("加载更多媒体库内容失败：{error}"),
                ),
            };
            self.push_error_notification(NotificationScope::Library, key, message, cx);
        } else if request.initial {
            self.clear_library_notifications(&request.view_id);
        } else {
            self.clear_notification(
                NotificationScope::Library,
                &library_load_more_notification_key(&request.view_id),
            );
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
