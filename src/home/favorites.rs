use super::adapter::EmbyHomeGateway;
use crate::home::detail::state::detail_binding;
pub(crate) mod actions;
pub(super) mod controller;
mod effect;
#[cfg(test)]
mod integration_tests;
mod presentation;
pub(super) mod render;

#[cfg(test)]
use super::LoadState;
use super::controller::FavoriteIntent;
use super::model::notification::ActionNotification;
#[cfg(test)]
use crate::emby::{SortOrder, UserItemsSort};
use actions::FavoriteCommand;
#[cfg(test)]
use controller::favorite_query;
pub(crate) use controller::{
    FAVORITE_ITEM_TYPES, FAVORITES_PAGE_LIMIT, FavoritesController, favorite_section_title,
};
use controller::{FavoritesIntent, FavoritesRequest};
pub(crate) use presentation::FavoritesPresentation;

use gpui::{AppContext as _, ClickEvent, Context, Window};

#[cfg(test)]
use crate::emby::UserItem;
use crate::emby::{UserItemData, UserItems, VideoItemType};

use super::{
    HomeContent, HomeContentEvent,
    navigation::{HomeRoot, HomeRoute},
    notification::NotificationScope,
};

impl HomeContent {
    pub(super) fn enter_favorites_if_needed(&mut self, cx: &mut Context<Self>) {
        let item_types = match self.controller.route() {
            HomeRoute::FavoriteItems { item_type } => vec![*item_type],
            _ => FAVORITE_ITEM_TYPES.to_vec(),
        };
        for item_type in item_types {
            self.dispatch_favorites(item_type, FavoritesIntent::Enter, cx);
        }
    }

    pub(super) fn open_favorite_items(&mut self, item_type: VideoItemType, cx: &mut Context<Self>) {
        self.item_context_menu = None;
        self.favorites_presentation.sync_previous_offsets();
        let change = self
            .controller
            .dispatch_navigation(super::controller::NavigationIntent::Favorites(item_type));
        super::detail::state::apply_navigation_change(change, &mut self.detail_resources);
        self.enter_favorites_if_needed(cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn auto_load_more_favorites(
        &mut self,
        item_type: VideoItemType,
        cx: &mut Context<Self>,
    ) {
        if self.controller.route() == &(HomeRoute::FavoriteItems { item_type }) {
            self.dispatch_favorites(item_type, FavoritesIntent::LoadMore { automatic: true }, cx);
        }
    }

    pub(super) fn load_more_favorites(&mut self, item_type: VideoItemType, cx: &mut Context<Self>) {
        self.dispatch_favorites(
            item_type,
            FavoritesIntent::LoadMore { automatic: false },
            cx,
        );
    }

    pub(super) fn load_favorites_initial(
        &mut self,
        item_type: VideoItemType,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_favorites(item_type, FavoritesIntent::Refresh, cx);
    }

    pub(super) fn invalidate_favorites(&mut self) {
        self.controller.invalidate_favorites();
        self.favorites_presentation.cancel_effects();
    }

    fn clear_favorite_notifications(&mut self, item_type: VideoItemType) {
        for phase in ["initial", "refresh", "load-more"] {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(item_type, phase),
            );
        }
    }

    fn dispatch_favorites(
        &mut self,
        item_type: VideoItemType,
        intent: FavoritesIntent,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.controller.dispatch_favorites(item_type, intent) else {
            return;
        };
        if request.initial {
            self.clear_favorite_notifications(item_type);
        } else {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(item_type, "load-more"),
            );
        }
        cx.notify();
        let gateway = EmbyHomeGateway {
            client: self.emby_client.clone(),
            server: self.current_server.clone(),
        };
        let task_request = request.clone();
        let task =
            cx.background_spawn(async move { effect::run_favorites(&gateway, &task_request) });
        let handle = cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_favorites_page(request, result, cx)
            })
            .ok();
        });
        self.favorites_presentation[item_type]
            .effect
            .replace(handle);
    }

    fn finish_favorites_page(
        &mut self,
        request: FavoritesRequest,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let Some(update) =
            self.controller
                .complete_favorites(&request, result, &self.request_identity())
        else {
            return;
        };
        if let Some(items) = update.images {
            self.layout.content_changed();
            self.ensure_favorite_items_images(&items, cx);
        }
        if let Some((phase, error)) = update.failure {
            self.push_error_notification(
                NotificationScope::Favorites,
                favorite_notification_key(request.item_type, phase),
                format!(
                    "加载收藏{}失败：{error}",
                    favorite_section_title(request.item_type)
                ),
                cx,
            );
        } else if request.initial {
            self.clear_favorite_notifications(request.item_type);
        } else {
            self.clear_notification(
                NotificationScope::Favorites,
                &favorite_notification_key(request.item_type, "load-more"),
            );
        }
        cx.notify();
    }

    pub(super) fn toggle_detail_favorite(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item_id) = self
            .detail_view()
            .and_then(|detail| detail.model.selected_playback_item())
            .map(|item| item.id.clone())
        else {
            return;
        };
        let fallback = self
            .detail_view()
            .and_then(|detail| detail.model.selected_playback_item())
            .and_then(|item| item.user_data.clone());
        self.toggle_item_favorite(item_id, fallback, cx);
    }

    pub(super) fn toggle_item_favorite(
        &mut self,
        item_id: String,
        fallback: Option<UserItemData>,
        cx: &mut Context<Self>,
    ) {
        let (scope, key) = self.item_action_notification(&item_id, "favorite");
        let Some(command) = self.controller.dispatch_favorite(FavoriteIntent {
            item_id,
            fallback,
            notification: ActionNotification {
                scope,
                key: key.to_string(),
            },
        }) else {
            return;
        };
        if let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        {
            detail.presentation.open_select = None;
        }
        self.invalidate_pending_home_snapshot_save();
        self.layout.content_changed();
        self.favorites_presentation.cancel_effects();
        self.clear_notification(scope, &key);
        cx.notify();
        let gateway = EmbyHomeGateway {
            client: self.emby_client.clone(),
            server: self.current_server.clone(),
        };
        let task_command = command.clone();
        let task =
            cx.background_spawn(
                async move { effect::run_favorite_mutation(&gateway, &task_command) },
            );
        let handle = cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_toggle_favorite(command, result, cx)
            })
            .ok();
        });
        self.favorite_effect.replace(handle);
    }

    fn finish_toggle_favorite(
        &mut self,
        command: FavoriteCommand,
        result: anyhow::Result<UserItemData>,
        cx: &mut Context<Self>,
    ) {
        let Some(update) =
            self.controller
                .complete_favorite(&command, result, &self.request_identity())
        else {
            return;
        };
        if let Some((notification, error)) = update.failure {
            self.push_error_notification(
                notification.scope,
                notification.key,
                format!("更新收藏失败：{error}"),
                cx,
            );
        }
        self.schedule_home_snapshot_save(cx);
        self.layout.content_changed();
        self.favorites_presentation.cancel_effects();
        if matches!(
            self.controller.route(),
            HomeRoute::Root(HomeRoot::Favorites) | HomeRoute::FavoriteItems { .. }
        ) {
            self.enter_favorites_if_needed(cx);
        }
        cx.notify();
    }
}

fn favorite_notification_key(item_type: VideoItemType, phase: &str) -> String {
    format!("favorites:{}:{phase}", item_type.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_queries_load_each_type_independently_with_newest_first() {
        for item_type in FAVORITE_ITEM_TYPES {
            let query = favorite_query(item_type, 30);
            assert_eq!(query.include_item_types, vec![item_type]);
            assert_eq!(query.is_favorite, Some(true));
            assert!(query.recursive);
            assert_eq!(query.start_index, 30);
            assert_eq!(query.limit, 30);
            assert_eq!(query.sort_by, Some(UserItemsSort::DateCreated));
            assert_eq!(query.sort_order, SortOrder::Descending);
            let fields = query.fields.as_deref().unwrap();
            for field in [
                "BasicSyncInfo",
                "CommunityRating",
                "ProductionYear",
                "EndDate",
                "Container",
                "SeriesId",
                "SeriesName",
            ] {
                assert!(fields.split(',').any(|value| value == field));
            }
        }
    }
}
