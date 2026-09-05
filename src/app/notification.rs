use gpui::{ClickEvent, Context, IntoElement, ParentElement, SharedString};

use crate::ui::notification::{
    NOTIFICATION_AUTOHIDE, NotificationQueue, error_notification, notification_layer,
};

use super::TinyApp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AppNotificationKey {
    AppError,
    ServerError,
}

pub(super) type AppNotificationQueue = NotificationQueue<AppNotificationKey>;

impl TinyApp {
    pub(super) fn push_app_error_notification(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .app_notifications
            .push(AppNotificationKey::AppError, message.into());
        cx.notify();

        cx.spawn(async move |app, cx| {
            cx.background_executor().timer(NOTIFICATION_AUTOHIDE).await;
            app.update(cx, |app, cx| {
                if app.app_notifications.remove(id) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn clear_app_notifications(&mut self) {
        self.app_notifications
            .retain(|entry| entry.key != AppNotificationKey::AppError);
    }

    pub(super) fn has_app_notifications(&self) -> bool {
        self.app_notifications
            .iter()
            .any(|entry| entry.key == AppNotificationKey::AppError)
    }

    pub(super) fn render_app_notification_layer(&self, cx: &Context<Self>) -> impl IntoElement {
        notification_layer().children(
            self.app_notifications
                .iter()
                .filter(|entry| entry.key == AppNotificationKey::AppError)
                .map(|entry| {
                    let id = entry.notification.id;
                    let dismiss = cx.listener(move |app: &mut TinyApp, _: &ClickEvent, _, cx| {
                        app.dismiss_app_notification(id, cx);
                    });
                    error_notification(id, entry.notification.message.clone(), dismiss, cx)
                }),
        )
    }

    fn dismiss_app_notification(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.app_notifications.remove(id) {
            cx.notify();
        }
    }

    pub(super) fn push_server_error_notification(
        &mut self,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .app_notifications
            .push(AppNotificationKey::ServerError, message.into());
        cx.notify();

        cx.spawn(async move |app, cx| {
            cx.background_executor().timer(NOTIFICATION_AUTOHIDE).await;
            app.update(cx, |app, cx| {
                if app.app_notifications.remove(id) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn clear_server_notifications(&mut self) {
        self.app_notifications
            .retain(|entry| entry.key != AppNotificationKey::ServerError);
    }

    pub(super) fn has_server_page_notifications(&self) -> bool {
        !self.app_notifications.is_empty()
    }

    pub(super) fn render_server_page_notification_layer(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        notification_layer().children(self.app_notifications.iter().map(|entry| {
            let id = entry.notification.id;
            let dismiss = cx.listener(move |app: &mut TinyApp, _: &ClickEvent, _, cx| {
                app.dismiss_app_notification(id, cx);
            });
            error_notification(id, entry.notification.message.clone(), dismiss, cx)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_and_server_errors_use_distinct_queue_keys() {
        let mut notifications = AppNotificationQueue::default();
        notifications.push(AppNotificationKey::AppError, "app".into());
        notifications.push(AppNotificationKey::ServerError, "server".into());

        assert_eq!(notifications.iter().count(), 2);

        notifications.retain(|entry| entry.key != AppNotificationKey::ServerError);
        let entries = notifications.iter().collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, AppNotificationKey::AppError);
        assert_eq!(entries[0].notification.message, "app");
    }
}
