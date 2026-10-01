use super::{
    HomeContent,
    notification::{HOME_RESUME_ACTION_NOTIFICATION_KEY, NotificationScope},
};
use gpui::{AppContext as _, Context};
pub(crate) mod controller;
mod effect;
pub(super) use controller::ResumeItemAction;
use controller::{ResumeCommand, ResumeItemActionResponse, ResumeUpdate};

impl HomeContent {
    pub(super) fn start_resume_item_action(
        &mut self,
        item_id: String,
        action: ResumeItemAction,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self.controller.dispatch_resume_action(item_id, action) else {
            return;
        };
        self.item_context_menu = None;
        self.clear_notification(NotificationScope::Home, HOME_RESUME_ACTION_NOTIFICATION_KEY);
        cx.notify();
        let gateway = self.ports.browsing.clone();
        let task_command = command.clone();
        let task = cx.background_spawn(async move {
            effect::run_resume_action(gateway.as_ref(), &task_command)
        });
        let item_id = command.item_id.clone();
        let handle = cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| {
                page.finish_resume_item_action(command, result, cx)
            })
            .ok();
        });
        self.resume_effects
            .entry(item_id)
            .or_default()
            .replace(handle);
    }

    fn finish_resume_item_action(
        &mut self,
        command: ResumeCommand,
        result: anyhow::Result<ResumeItemActionResponse>,
        cx: &mut Context<Self>,
    ) {
        let Some(update) =
            self.controller
                .complete_resume_action(&command, result, &self.request_identity())
        else {
            return;
        };
        self.resume_effects.remove(&command.item_id);
        match update {
            ResumeUpdate::Changed => {
                self.layout.content_changed();
                self.clear_notification(
                    NotificationScope::Home,
                    HOME_RESUME_ACTION_NOTIFICATION_KEY,
                );
                self.schedule_home_snapshot_save(cx);
            }
            ResumeUpdate::Failed(message) => self.push_error_notification(
                NotificationScope::Home,
                HOME_RESUME_ACTION_NOTIFICATION_KEY,
                message,
                cx,
            ),
        }
        cx.notify();
    }
}
