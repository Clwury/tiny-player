//! GPUI resources and gateway delivery for person metadata and filmography.
use gpui::{AppContext as _, Context, Task};

use super::{
    controller::{PersonRequest, PersonTransition},
    effect,
};
use crate::{
    effects::EffectHandle,
    emby::{MediaItem, MediaPerson, UserItems},
    home::{
        HomeContent, HomeContentEvent,
        detail::binding::apply_navigation_change,
        library::{
            LibraryResources,
            controller::{
                LibraryFailure, LibraryIntent, LibraryRequest, LibrarySource, LibraryTransition,
            },
        },
        model::{navigation::HomeRoute, notification::NotificationScope},
    },
};

#[derive(Debug, Default)]
pub(in crate::home) struct PersonResources {
    metadata: EffectHandle<Task<()>>,
    pub(in crate::home) items: LibraryResources,
}

impl HomeContent {
    pub(in crate::home) fn open_person_page(
        &mut self,
        person: &MediaPerson,
        cx: &mut Context<Self>,
    ) {
        let Some((change, transition)) = self.controller.open_person(person) else {
            return;
        };
        let person_id = person.id().expect("opened person has an ID");
        self.item_context_menu = None;
        self.person_resources
            .entry(person_id.to_string())
            .or_default();
        apply_navigation_change(change, &mut self.detail_resources);
        self.apply_person_transition(person_id, transition, cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(in crate::home) fn enter_current_person_if_needed(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Person { person_id, .. } = self.controller.route() else {
            return;
        };
        let person_id = person_id.clone();
        if let Some(transition) = self.controller.enter_person(&person_id) {
            self.apply_person_transition(&person_id, transition, cx);
        }
    }

    pub(in crate::home) fn toggle_person_favorite(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Person { person_id, .. } = self.controller.route() else {
            return;
        };
        let Some(person) = self.controller.person_view(person_id) else {
            return;
        };
        if !person.loaded {
            return;
        }
        let fallback = person.user_data.cloned();
        self.toggle_item_favorite(person_id.clone(), fallback, cx);
    }

    fn apply_person_transition(
        &mut self,
        person_id: &str,
        transition: PersonTransition,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = transition.metadata {
            self.clear_notification(
                NotificationScope::Person,
                &format!("person:{person_id}:metadata"),
            );
            let gateway = self.ports.browsing.clone();
            let task_request = request.clone();
            let task =
                cx.background_spawn(
                    async move { effect::run_person(gateway.as_ref(), &task_request) },
                );
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| page.finish_person(request, result, cx))
                    .ok();
            });
            if let Some(resources) = self.person_resources.get_mut(person_id) {
                resources.metadata.replace(handle);
            }
        }
        self.apply_person_items_transition(person_id, transition.items, cx);
    }

    pub(in crate::home) fn dispatch_person_items(
        &mut self,
        person_id: &str,
        intent: LibraryIntent,
        cx: &mut Context<Self>,
    ) {
        match intent {
            LibraryIntent::SortBy(sort_by) => {
                self.select_items_sort_by(
                    &crate::home::library::ItemsSortTarget::Library(LibrarySource::Person(
                        person_id.into(),
                    )),
                    sort_by,
                    cx,
                );
                return;
            }
            LibraryIntent::SortOrder(sort_order) => {
                self.select_items_sort_order(
                    &crate::home::library::ItemsSortTarget::Library(LibrarySource::Person(
                        person_id.into(),
                    )),
                    sort_order,
                    cx,
                );
                return;
            }
            _ => {}
        }
        let Some(transition) = self.controller.dispatch_person_items(person_id, intent) else {
            return;
        };
        self.apply_person_items_transition(person_id, transition, cx);
    }

    fn apply_person_items_transition(
        &mut self,
        person_id: &str,
        transition: LibraryTransition,
        cx: &mut Context<Self>,
    ) {
        let Some(resources) = self.person_resources.get_mut(person_id) else {
            return;
        };
        if transition.close_menu {
            resources.items.presentation.sort_menu_open = false;
        }
        if transition.cancel {
            resources.items.effect.cancel();
            resources
                .items
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
                NotificationScope::Person,
                &format!("person:{person_id}:{phase}"),
            );
            let gateway = self.ports.browsing.clone();
            let task_request = request.clone();
            let task = cx.background_spawn(async move {
                crate::home::library::effect::run_library(gateway.as_ref(), &task_request)
            });
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| page.finish_person_items(request, result, cx))
                    .ok();
            });
            if let Some(resources) = self.person_resources.get_mut(person_id) {
                resources.items.effect.replace(handle);
            }
        }
        if transition.notify {
            cx.notify();
        }
    }

    fn finish_person(
        &mut self,
        request: PersonRequest,
        result: anyhow::Result<MediaItem>,
        cx: &mut Context<Self>,
    ) {
        let Some(result) =
            self.controller
                .complete_person(&request, result, &self.request_identity())
        else {
            return;
        };
        let key = format!("person:{}:metadata", request.person_id);
        match result {
            Ok(()) => self.clear_notification(NotificationScope::Person, &key),
            Err(error) => self.push_error_notification(
                NotificationScope::Person,
                key,
                format!("加载人物信息失败：{error}"),
                cx,
            ),
        }
        cx.notify();
    }

    fn finish_person_items(
        &mut self,
        request: LibraryRequest,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let LibrarySource::Person(person_id) = &request.source else {
            return;
        };
        let Some(update) =
            self.controller
                .complete_person_items(&request, result, &self.request_identity())
        else {
            return;
        };
        if let Some(items) = update.images {
            self.ensure_user_items_images(&items, cx);
        }
        let phase = if request.initial {
            "initial"
        } else {
            "load-more"
        };
        let key = format!("person:{person_id}:{phase}");
        if let Some(failure) = update.failure {
            let error = match failure {
                LibraryFailure::Initial(error)
                | LibraryFailure::Refresh(error)
                | LibraryFailure::More(error) => error,
            };
            self.push_error_notification(
                NotificationScope::Person,
                key,
                format!("加载人物作品失败：{error}"),
                cx,
            );
        } else {
            self.clear_notification(NotificationScope::Person, &key);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
