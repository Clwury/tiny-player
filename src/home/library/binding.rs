//! GPUI intent binding and cancellable delivery for this feature.
use super::{ItemsSortTarget, controller, effect};
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
use controller::{LibraryFailure, LibraryIntent, LibraryRequest, LibrarySource, LibraryTransition};
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

    pub(in crate::home) fn toggle_current_items_sort_menu(&mut self, cx: &mut Context<Self>) {
        let menu = match self.controller.route() {
            HomeRoute::Library { view_id, .. } => self
                .library_resources
                .get_mut(view_id)
                .map(|state| &mut state.presentation.sort_menu_open),
            HomeRoute::Genre { genre_key, .. } => self
                .genre_resources
                .get_mut(genre_key)
                .map(|state| &mut state.presentation.sort_menu_open),
            HomeRoute::Person { person_id, .. } => self
                .person_resources
                .get_mut(person_id)
                .map(|person| &mut person.items.presentation.sort_menu_open),
            HomeRoute::FavoriteItems { item_type } => {
                Some(&mut self.favorites_presentation[*item_type].sort_menu_open)
            }
            _ => None,
        };
        let Some(menu) = menu else {
            return;
        };

        *menu = !*menu;
        cx.notify();
    }

    pub(in crate::home) fn close_current_items_sort_menu(&mut self, cx: &mut Context<Self>) {
        let menu = match self.controller.route() {
            HomeRoute::Library { view_id, .. } => self
                .library_resources
                .get_mut(view_id)
                .map(|state| &mut state.presentation.sort_menu_open),
            HomeRoute::Genre { genre_key, .. } => self
                .genre_resources
                .get_mut(genre_key)
                .map(|state| &mut state.presentation.sort_menu_open),
            HomeRoute::Person { person_id, .. } => self
                .person_resources
                .get_mut(person_id)
                .map(|person| &mut person.items.presentation.sort_menu_open),
            HomeRoute::FavoriteItems { item_type } => {
                Some(&mut self.favorites_presentation[*item_type].sort_menu_open)
            }
            _ => None,
        };
        let Some(menu) = menu else {
            return;
        };
        if *menu {
            *menu = false;
            cx.notify();
        }
    }

    pub(in crate::home) fn select_library_sort_by(
        &mut self,
        view_id: String,
        sort_by: UserItemsSort,
        cx: &mut Context<Self>,
    ) {
        self.select_items_sort_by(
            &ItemsSortTarget::Library(LibrarySource::View(view_id)),
            sort_by,
            cx,
        );
    }
    pub(in crate::home) fn select_library_sort_order(
        &mut self,
        view_id: String,
        sort_order: SortOrder,
        cx: &mut Context<Self>,
    ) {
        self.select_items_sort_order(
            &ItemsSortTarget::Library(LibrarySource::View(view_id)),
            sort_order,
            cx,
        );
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
    pub(in crate::home) fn select_items_sort_by(
        &mut self,
        target: &ItemsSortTarget,
        sort_by: UserItemsSort,
        cx: &mut Context<Self>,
    ) {
        if !self.items_sort_target_is_current(target)
            || !self.controller.current_items_sort_is_available(sort_by)
        {
            return;
        }
        self.close_current_items_sort_menu(cx);
        let preferences = crate::media::ItemSortPreferences {
            sort_by,
            ..crate::media::ItemSortPreferences::get(cx)
        };
        self.apply_shared_items_sort(preferences, cx);
        preferences.apply(cx);
    }

    pub(in crate::home) fn select_items_sort_order(
        &mut self,
        target: &ItemsSortTarget,
        sort_order: SortOrder,
        cx: &mut Context<Self>,
    ) {
        if !self.items_sort_target_is_current(target) {
            return;
        }
        self.close_current_items_sort_menu(cx);
        let preferences = crate::media::ItemSortPreferences {
            sort_order,
            ..crate::media::ItemSortPreferences::get(cx)
        };
        self.apply_shared_items_sort(preferences, cx);
        preferences.apply(cx);
    }

    pub(in crate::home) fn dispatch_items_source(
        &mut self,
        source: &LibrarySource,
        intent: LibraryIntent,
        cx: &mut Context<Self>,
    ) {
        match source {
            LibrarySource::View(id) => match intent {
                LibraryIntent::SortBy(sort) => self.select_library_sort_by(id.clone(), sort, cx),
                LibraryIntent::SortOrder(order) => {
                    self.select_library_sort_order(id.clone(), order, cx)
                }
                LibraryIntent::LoadMore { automatic: true } => self.auto_load_more_library(id, cx),
                intent => self.dispatch_library(id, intent, cx),
            },
            LibrarySource::Person(id) => self.dispatch_person_items(id, intent, cx),
            LibrarySource::Genre(key) => self.dispatch_genre_items(key, intent, cx),
        }
    }

    pub(in crate::home) fn auto_load_more_items_source(
        &mut self,
        source: &LibrarySource,
        cx: &mut Context<Self>,
    ) {
        match (source, self.controller.route()) {
            (LibrarySource::View(id), HomeRoute::Library { view_id, .. }) if id == view_id => {}
            (LibrarySource::Person(id), HomeRoute::Person { person_id, .. }) if id == person_id => {
            }
            (LibrarySource::Genre(key), HomeRoute::Genre { genre_key, .. }) if key == genre_key => {
            }
            _ => return,
        }
        self.dispatch_items_source(source, LibraryIntent::LoadMore { automatic: true }, cx);
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
                    library_initial_notification_key(request.source.id()),
                    format!("加载媒体库失败：{error}"),
                ),
                LibraryFailure::Refresh(error) => {
                    (library_refresh_notification_key(request.source.id()), error)
                }
                LibraryFailure::More(error) => (
                    library_load_more_notification_key(request.source.id()),
                    format!("加载更多媒体库内容失败：{error}"),
                ),
            };
            self.push_error_notification(NotificationScope::Library, key, message, cx);
        } else if request.initial {
            self.clear_library_notifications(request.source.id());
        } else {
            self.clear_notification(
                NotificationScope::Library,
                &library_load_more_notification_key(request.source.id()),
            );
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
