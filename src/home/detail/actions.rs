use super::*;
use crate::home::detail::binding::detail_binding;
use crate::home::{
    controller::PlayedIntent,
    model::notification::ActionNotification,
    played::{PlayedCommand, PlayedResponse},
};

#[cfg(test)]
#[path = "actions_tests.rs"]
mod tests;

impl HomeContent {
    pub(in crate::home) fn toggle_detail_actions_menu(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
                .filter(|detail| detail.model.is_series())
        else {
            return;
        };
        if detail.presentation.open_select == Some(SeriesDetailSelectKind::Actions) {
            detail.presentation.open_select = None;
            window.blur(cx);
        } else {
            let focus = detail
                .presentation
                .action_menu_focus
                .get_or_insert_with(|| cx.focus_handle());
            focus.focus(window, cx);
            detail.presentation.open_select = Some(SeriesDetailSelectKind::Actions);
        }
        cx.notify();
    }

    pub(in crate::home) fn toggle_series_favorite(&mut self, cx: &mut Context<Self>) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
                .filter(|detail| detail.model.is_series())
        else {
            return;
        };
        detail.presentation.open_select = None;
        let id = detail.model.series_id.clone();
        let fallback = detail
            .model
            .item
            .as_ref()
            .and_then(|item| item.user_data.clone());
        self.toggle_item_favorite(id, fallback, cx);
    }

    pub(in crate::home) fn toggle_detail_played(
        &mut self,
        whole_series: bool,
        cx: &mut Context<Self>,
    ) {
        if self.detail_view().is_none() {
            return;
        }
        self.start_played_request(PlayedIntent::ToggleDetail { whole_series }, cx);
    }

    pub(in crate::home) fn mark_user_item_played(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let (scope, key) = self.item_action_notification(item_id, "played");
        self.start_played_request(
            PlayedIntent::MarkItem {
                item_id: item_id.to_owned(),
                notification: ActionNotification {
                    scope,
                    key: key.to_string(),
                },
            },
            cx,
        );
    }

    fn start_played_request(&mut self, intent: PlayedIntent, cx: &mut Context<Self>) {
        let Some(command) = self.controller.dispatch_played(intent) else {
            return;
        };
        if let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        {
            detail.presentation.open_select = None;
        }
        self.clear_notification(
            command.request.notification.scope,
            &command.request.notification.key,
        );
        cx.notify();
        let gateway = self.ports.browsing.clone();
        let task_command = command.clone();
        let task = cx.background_spawn(async move {
            crate::home::played::effect::run_played(gateway.as_ref(), &task_command)
        });
        let handle = cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_detail_played(command, result, cx)
            })
            .ok();
        });
        self.played_effect.replace(handle);
    }

    fn finish_detail_played(
        &mut self,
        command: PlayedCommand,
        result: anyhow::Result<PlayedResponse>,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) =
            self.controller
                .accept_played(command, result, &self.request_identity())
        else {
            return;
        };
        if completion.succeeded() {
            self.invalidate_pending_home_snapshot_save();
        }
        let update = self.controller.apply_played_completion(completion);
        for message in update.errors {
            self.push_error_notification(
                update.notification.scope,
                update.notification.key.clone(),
                message,
                cx,
            );
        }
        if update.changed {
            self.layout.content_changed();
            self.invalidate_favorites();
            self.schedule_home_snapshot_save(cx);
        }
        cx.notify();
    }
}
