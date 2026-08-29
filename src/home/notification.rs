use gpui::{ClickEvent, Context, IntoElement, ParentElement, SharedString, Timer};

use crate::ui::notification::{
    NOTIFICATION_AUTOHIDE, NotificationQueue, error_notification, notification_layer,
};

use super::{
    HomeContent,
    navigation::{HomeRoot, HomeRoute},
};

pub(super) const HOME_USER_VIEWS_NOTIFICATION_KEY: &str = "home:user-views";
pub(super) const HOME_RESUME_ITEMS_NOTIFICATION_KEY: &str = "home:resume-items";
pub(super) const HOME_RESUME_DETAIL_NOTIFICATION_KEY: &str = "home:resume-detail";
pub(super) const HOME_RESUME_ACTION_NOTIFICATION_KEY: &str = "home:resume-action";
pub(super) const FAVORITES_INITIAL_NOTIFICATION_KEY: &str = "favorites:initial";
pub(super) const FAVORITES_REFRESH_NOTIFICATION_KEY: &str = "favorites:refresh";
pub(super) const FAVORITES_LOAD_MORE_NOTIFICATION_KEY: &str = "favorites:load-more";
pub(super) const SEARCH_INITIAL_NOTIFICATION_KEY: &str = "search:initial";
pub(super) const SEARCH_LOAD_MORE_NOTIFICATION_KEY: &str = "search:load-more";

pub(super) fn library_initial_notification_key(view_id: &str) -> String {
    format!("library:{view_id}:initial")
}

pub(super) fn library_refresh_notification_key(view_id: &str) -> String {
    format!("library:{view_id}:refresh")
}

pub(super) fn library_load_more_notification_key(view_id: &str) -> String {
    format!("library:{view_id}:load-more")
}

pub(super) fn latest_items_notification_key(view_id: &str) -> String {
    format!("home:latest:{view_id}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotificationScope {
    Home,
    Favorites,
    Search,
    Library,
    Detail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HomeNotificationKey {
    scope: NotificationScope,
    name: SharedString,
}

impl HomeNotificationKey {
    fn new(scope: NotificationScope, name: impl Into<SharedString>) -> Self {
        Self {
            scope,
            name: name.into(),
        }
    }
}

impl HomeContent {
    pub(super) fn push_error_notification(
        &mut self,
        scope: NotificationScope,
        key: impl Into<SharedString>,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .notifications
            .push(HomeNotificationKey::new(scope, key), message.into());
        cx.notify();

        cx.spawn(async move |page, cx| {
            Timer::after(NOTIFICATION_AUTOHIDE).await;
            page.update(cx, |page, cx| {
                if page.notifications.remove(id) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn clear_notification(&mut self, scope: NotificationScope, key: &str) {
        self.notifications
            .retain(|entry| entry.key.scope != scope || entry.key.name.as_ref() != key);
    }

    pub(super) fn clear_notifications_for_scope(&mut self, scope: NotificationScope) {
        self.notifications.retain(|entry| entry.key.scope != scope);
    }

    pub(super) fn clear_library_notifications(&mut self, view_id: &str) {
        for key in [
            library_initial_notification_key(view_id),
            library_refresh_notification_key(view_id),
            library_load_more_notification_key(view_id),
        ] {
            self.clear_notification(NotificationScope::Library, &key);
        }
    }

    pub(super) fn clear_all_notifications(&mut self) {
        self.notifications.clear();
    }

    pub(super) fn current_notification_scope(&self) -> Option<NotificationScope> {
        notification_scope_for_route(self.navigation.current())
    }

    fn dismiss_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.notifications.remove(id) {
            cx.notify();
        }
    }

    pub(crate) fn has_visible_notifications(&self) -> bool {
        if self.notifications.is_empty() {
            return false;
        }
        self.notifications
            .iter()
            .any(|entry| notification_matches_route(&entry.key, self.navigation.current()))
    }

    pub(crate) fn render_notification_layer(&self, cx: &Context<Self>) -> impl IntoElement {
        let route = self.navigation.current().clone();

        notification_layer().children(
            self.notifications
                .iter()
                .filter(move |entry| notification_matches_route(&entry.key, &route))
                .map(|entry| {
                    let id = entry.notification.id;
                    let dismiss =
                        cx.listener(move |page: &mut HomeContent, _: &ClickEvent, _, cx| {
                            page.dismiss_notification(id, cx);
                        });
                    error_notification(id, entry.notification.message.clone(), dismiss, cx)
                }),
        )
    }
}

fn notification_matches_route(key: &HomeNotificationKey, route: &HomeRoute) -> bool {
    let Some(scope) = notification_scope_for_route(route) else {
        return false;
    };
    if key.scope != scope {
        return false;
    }

    match route {
        HomeRoute::Library { view_id, .. } => key
            .name
            .as_ref()
            .starts_with(&format!("library:{view_id}:")),
        HomeRoute::Root(_) | HomeRoute::Detail { .. } => true,
    }
}

fn notification_scope_for_route(route: &HomeRoute) -> Option<NotificationScope> {
    match route {
        HomeRoute::Root(HomeRoot::Home) => Some(NotificationScope::Home),
        HomeRoute::Root(HomeRoot::Favorites) => Some(NotificationScope::Favorites),
        HomeRoute::Root(HomeRoot::Search) => Some(NotificationScope::Search),
        HomeRoute::Library { .. } => Some(NotificationScope::Library),
        HomeRoute::Detail { .. } => Some(NotificationScope::Detail),
    }
}

pub(super) type HomeNotificationQueue = NotificationQueue<HomeNotificationKey>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_scope_follows_the_visible_workspace() {
        assert_eq!(
            notification_scope_for_route(&HomeRoute::Root(HomeRoot::Home)),
            Some(NotificationScope::Home)
        );
        assert_eq!(
            notification_scope_for_route(&HomeRoute::Root(HomeRoot::Favorites)),
            Some(NotificationScope::Favorites)
        );
        assert_eq!(
            notification_scope_for_route(&HomeRoute::Root(HomeRoot::Search)),
            Some(NotificationScope::Search)
        );
        assert_eq!(
            notification_scope_for_route(&HomeRoute::Detail {
                root_item_id: "item".into(),
                episode_id: None,
            }),
            Some(NotificationScope::Detail)
        );
        assert_eq!(
            notification_scope_for_route(&HomeRoute::Library {
                view_id: "view".into(),
                title: "Library".into(),
                item_types: Vec::new(),
            }),
            Some(NotificationScope::Library)
        );
    }

    #[test]
    fn library_notifications_only_match_their_own_library() {
        let key = HomeNotificationKey::new(
            NotificationScope::Library,
            library_initial_notification_key("movies"),
        );
        let movies = HomeRoute::Library {
            view_id: "movies".into(),
            title: "Movies".into(),
            item_types: Vec::new(),
        };
        let series = HomeRoute::Library {
            view_id: "series".into(),
            title: "Series".into(),
            item_types: Vec::new(),
        };

        assert!(notification_matches_route(&key, &movies));
        assert!(!notification_matches_route(&key, &series));
        assert!(!notification_matches_route(
            &key,
            &HomeRoute::Root(HomeRoot::Home)
        ));
    }
}
