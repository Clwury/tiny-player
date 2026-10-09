//! Genre page resources and injected gateway delivery.
use gpui::{AppContext as _, Context};

use crate::{
    emby::{MediaGenre, UserItems},
    home::{
        HomeContent, HomeContentEvent,
        detail::binding::apply_navigation_change,
        library::{
            ItemsSortTarget,
            controller::{
                LibraryFailure, LibraryIntent, LibraryRequest, LibrarySource, LibraryTransition,
            },
        },
        model::notification::NotificationScope,
    },
};

impl HomeContent {
    pub(in crate::home) fn open_genre_page(&mut self, genre: &MediaGenre, cx: &mut Context<Self>) {
        let Some(genre) = genre.normalized() else {
            return;
        };
        let Some((change, transition)) = self.controller.open_genre(&genre) else {
            return;
        };
        let key = genre.key();
        self.item_context_menu = None;
        self.genre_resources.entry(key.clone()).or_default();
        apply_navigation_change(change, &mut self.detail_resources);
        self.apply_genre_items_transition(&key, transition, cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in crate::home) fn dispatch_genre_items(
        &mut self,
        genre_key: &str,
        intent: LibraryIntent,
        cx: &mut Context<Self>,
    ) {
        let target = ItemsSortTarget::Library(LibrarySource::Genre(genre_key.to_string()));
        match intent {
            LibraryIntent::SortBy(sort_by) => {
                self.select_items_sort_by(&target, sort_by, cx);
                return;
            }
            LibraryIntent::SortOrder(sort_order) => {
                self.select_items_sort_order(&target, sort_order, cx);
                return;
            }
            _ => {}
        }
        let Some(transition) = self.controller.dispatch_genre_items(genre_key, intent) else {
            return;
        };
        self.apply_genre_items_transition(genre_key, transition, cx);
    }

    fn apply_genre_items_transition(
        &mut self,
        genre_key: &str,
        transition: LibraryTransition,
        cx: &mut Context<Self>,
    ) {
        let Some(resources) = self.genre_resources.get_mut(genre_key) else {
            return;
        };
        if transition.close_menu {
            resources.presentation.sort_menu_open = false;
        }
        if transition.cancel {
            resources.effect.cancel();
            resources
                .presentation
                .grid
                .scroll_handle
                .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
        }
        if let Some(request) = transition.request {
            let phase = if request.initial {
                "initial"
            } else {
                "load-more"
            };
            self.clear_notification(
                NotificationScope::Genre,
                &format!("genre:{genre_key}:{phase}"),
            );
            let gateway = self.ports.browsing.clone();
            let task_request = request.clone();
            let task = cx.background_spawn(async move {
                crate::home::library::effect::run_library(gateway.as_ref(), &task_request)
            });
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| page.finish_genre_items(request, result, cx))
                    .ok();
            });
            if let Some(resources) = self.genre_resources.get_mut(genre_key) {
                resources.effect.replace(handle);
            }
        }
        if transition.notify {
            cx.notify();
        }
    }

    fn finish_genre_items(
        &mut self,
        request: LibraryRequest,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let LibrarySource::Genre(key) = &request.source else {
            return;
        };
        let Some(update) =
            self.controller
                .complete_genre_items(&request, result, &self.request_identity())
        else {
            return;
        };
        if let Some(items) = update.images {
            self.layout.content_changed();
            self.ensure_user_items_images(&items, cx);
        }
        let phase = if request.initial {
            "initial"
        } else {
            "load-more"
        };
        let notification_key = format!("genre:{key}:{phase}");
        if let Some(failure) = update.failure {
            let error = match failure {
                LibraryFailure::Initial(error)
                | LibraryFailure::Refresh(error)
                | LibraryFailure::More(error) => error,
            };
            self.push_error_notification(
                NotificationScope::Genre,
                notification_key,
                format!("加载类型内容失败：{error}"),
                cx,
            );
        } else {
            self.clear_notification(NotificationScope::Genre, &notification_key);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
