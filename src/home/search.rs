use gpui::{AppContext as _, Context, Window, point, px};

use crate::ui::editor::EditorEvent;

use super::{
    HomeContent, HomeContentEvent,
    adapter::EmbyHomeGateway,
    model::search::SearchPage,
    notification::{
        NotificationScope, SEARCH_INITIAL_NOTIFICATION_KEY, SEARCH_LOAD_MORE_NOTIFICATION_KEY,
    },
};

pub(crate) mod controller;
#[cfg(test)]
use super::LoadState;
use controller::{SearchIntent, SearchRequestContext};

impl HomeContent {
    fn dispatch_search(&mut self, intent: SearchIntent, cx: &mut Context<Self>) {
        let transition = self.controller.dispatch_search(intent);
        if transition.reset_presentation {
            self.item_context_menu = None;
            self.search_effect.cancel();
            self.search_presentation
                .grid
                .scroll_handle
                .set_offset(point(px(0.0), px(0.0)));
            self.clear_notifications_for_scope(NotificationScope::Search);
        }
        if transition.history_changed {
            cx.emit(HomeContentEvent::SearchHistoryChanged(
                self.controller.search_view().history.clone(),
            ));
        }
        if let Some(request) = transition.request {
            if request.request.initial {
                self.clear_notifications_for_scope(NotificationScope::Search);
            } else {
                self.clear_notification(
                    NotificationScope::Search,
                    SEARCH_LOAD_MORE_NOTIFICATION_KEY,
                );
            }
            self.spawn_search_request(request, cx);
        }
        if transition.notify {
            cx.notify();
        }
    }

    pub(super) fn on_search_input_event(&mut self, event: &EditorEvent, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().to_string();
        let intent = match event {
            EditorEvent::Changed => SearchIntent::InputChanged(query),
            EditorEvent::Submitted => SearchIntent::Submit(query),
        };
        self.dispatch_search(intent, cx);
    }

    fn submit_search_from_input(&mut self, cx: &mut Context<Self>) {
        self.dispatch_search(
            SearchIntent::Submit(self.search_input.read(cx).value().to_string()),
            cx,
        );
    }

    pub(super) fn search_from_history(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_input
            .update(cx, |input, cx| input.set_value(query.to_owned(), cx));
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.submit_search_from_input(cx);
    }

    pub(super) fn clear_search_history(&mut self, cx: &mut Context<Self>) {
        self.dispatch_search(SearchIntent::ClearHistory, cx);
    }

    pub(super) fn auto_load_more_search(&mut self, cx: &mut Context<Self>) {
        if self.controller.route()
            != &super::navigation::HomeRoute::Root(super::navigation::HomeRoot::Search)
        {
            return;
        }
        self.load_more_search(cx);
    }

    pub(super) fn load_more_search(&mut self, cx: &mut Context<Self>) {
        self.dispatch_search(SearchIntent::LoadMore, cx);
    }

    // Identity and Search scope come from the model token. Query reset and Home
    // release drop the handle; uncancellable IO still passes the token check.
    // Genuine errors use SEARCH_INITIAL/SEARCH_LOAD_MORE_NOTIFICATION_KEY.
    fn spawn_search_request(&mut self, request: SearchRequestContext, cx: &mut Context<Self>) {
        let gateway = EmbyHomeGateway {
            server: self.current_server.clone(),
            client: self.emby_client.clone(),
        };
        let task_request = request.request.clone();
        let task = cx.background_spawn(async move { effect::run_search(&gateway, &task_request) });
        self.search_effect.replace(cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_search_request(request, result, cx);
            })
            .ok();
        }));
    }

    fn finish_search_request(
        &mut self,
        request: SearchRequestContext,
        result: anyhow::Result<SearchPage>,
        cx: &mut Context<Self>,
    ) {
        let Some(update) =
            self.controller
                .complete_search(&request, result, &self.request_identity())
        else {
            return;
        };
        if let Some(items) = update.received {
            self.layout.content_changed();
            self.ensure_feed_user_items_images(&items, cx);
        }
        let (key, prefix) = if update.initial {
            (SEARCH_INITIAL_NOTIFICATION_KEY, "搜索失败")
        } else {
            (SEARCH_LOAD_MORE_NOTIFICATION_KEY, "加载更多搜索结果失败")
        };
        let error = update.error;
        if let Some(error) = error {
            self.push_error_notification(
                NotificationScope::Search,
                key,
                format!("{prefix}：{error}"),
                cx,
            );
        } else {
            self.clear_notification(NotificationScope::Search, key);
        }
        cx.notify();
    }
}

mod effect;
#[cfg(test)]
mod interaction_tests;
